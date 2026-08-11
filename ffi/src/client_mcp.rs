//! Edits to the machine-wide client MCP config (`client-mcp.toml`), scoped to
//! one client surface.
//!
//! The read side lives in [`crate::engine`], which projects the same file into
//! the `mcp_client_servers` and `mcp_builtins` view events. This module owns the
//! write side for the **external client-run** population: add, edit, enable and
//! remove a server this client hosts itself.
//!
//! The write belongs in the core rather than in each client. `client-mcp.toml`
//! is machine-wide: every Adele client on the box reads the same file, so a
//! second independent writer is a correctness hazard for all of them. A C ABI
//! consumer therefore asks for an edit and reads the result back, and never
//! parses or writes the file itself.
//!
//! Not here: the built-in opt-out (`disabled_builtins`), which is a different
//! population with its own intent, and anything about the daemon's own MCP
//! fleet, which is administered over the daemon command channel.

use std::path::Path;

use desktop_assistant_client_common::mcp_host::{
    ClientMcpConfig, DEFAULT_SURFACE, McpServerConfig, SurfaceConfig,
};

/// One edit to this surface's external client-run MCP servers.
///
/// The panel asks for an edit; the core applies it to the shared config and
/// re-emits the inventory, so the panel renders the truth on disk rather than an
/// optimistic local state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientServerWrite {
    /// Add a definition, or edit the one of the same name. Carries the client's
    /// form as JSON (see [`ClientServerForm`]) so a new field costs no ABI
    /// change.
    Upsert { server_json: String },
    /// Delete a definition, and its membership in every surface.
    Remove { name: String },
    /// Turn one definition on or off for this surface.
    SetEnabled { name: String, enabled: bool },
}

/// The fields a client's add/edit form carries for a client-run server.
///
/// Deliberately smaller than [`McpServerConfig`]: a client form cannot express
/// `env_secrets` (there is no client-side secret store) or an HTTP endpoint, and
/// an edit preserves both rather than blanking them. Unknown fields are refused
/// so a client that asks for something this core cannot honour - an HTTP
/// transport, say - fails by name instead of silently getting a stdio server.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientServerForm {
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

/// Orders every read-modify-write of `client-mcp.toml` this core makes.
///
/// Why a lock, when [`ClientMcpConfig::save`] is already atomic: the save is
/// atomic against a *partial read*, so no reader ever sees a torn file. The
/// unprotected part is the transaction around it (load, change one thing,
/// save), which two tasks can interleave so that the second save drops the
/// first one's result. The lock spans the whole transaction.
///
/// **One core only.** Each Adele client on the machine holds its own core, and
/// they all write this one file, so this lock does not order one client's writes
/// against another's. That needs a lock on the file itself and is a separate
/// piece of work.
static WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Take the write lock for one read-modify-write of the shared config.
///
/// The built-in opt-out in [`crate::engine`] takes the same lock, so the two
/// populations' writes are ordered against each other and not only within
/// themselves.
pub(crate) async fn lock_writes() -> tokio::sync::MutexGuard<'static, ()> {
    WRITE_LOCK.lock().await
}

impl ClientServerWrite {
    /// Apply this edit to the config at `path`, for `surface`.
    ///
    /// Holds [`WRITE_LOCK`] across the load, the change and the save, so an edit
    /// dispatched while another is in flight waits rather than overwriting it.
    ///
    /// Fails without writing anything when the edit is not valid, so a refused
    /// edit leaves the file exactly as it was. The lock is released either way.
    pub async fn apply(&self, path: &Path, surface: &str) -> Result<(), String> {
        let _guard = lock_writes().await;
        self.apply_locked(path, surface)
    }

    /// The transaction itself. Private, and reachable only through
    /// [`apply`](Self::apply), so no caller can run it unserialized.
    fn apply_locked(&self, path: &Path, surface: &str) -> Result<(), String> {
        let mut cfg = load_strict(path)?;
        match self {
            Self::Upsert { server_json } => apply_upsert(&mut cfg, surface, server_json)?,
            Self::Remove { name } => cfg.remove_server(name.trim())?,
            Self::SetEnabled { name, enabled } => apply_enabled(&mut cfg, surface, name, *enabled)?,
        }
        cfg.save(path)
    }
}

/// Parse the client's form, then insert or replace the definition and set this
/// surface's membership to match its `enabled` field, so both grains agree.
///
/// An edit keeps what the form cannot carry (`env`, `env_secrets`,
/// `inherit_env`, `description`), and refuses a definition that reaches its
/// server over HTTP: this form describes a stdio server, so applying it would
/// quietly drop the endpoint and the auth with it.
fn apply_upsert(cfg: &mut ClientMcpConfig, surface: &str, server_json: &str) -> Result<(), String> {
    let form: ClientServerForm =
        serde_json::from_str(server_json).map_err(|e| format!("invalid server json: {e}"))?;
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return Err("server name must not be empty".to_string());
    }
    let command = form.command.trim().to_string();
    if command.is_empty() {
        return Err(format!("server '{name}' needs a command to run"));
    }
    let existing = cfg.list_defined_servers().iter().find(|s| s.name == name);
    if existing.is_some_and(|s| s.http.is_some()) {
        return Err(format!(
            "server '{name}' is configured for http; this client edits stdio servers only"
        ));
    }
    let server = McpServerConfig {
        name: name.clone(),
        command,
        args: form.args.iter().map(|a| a.trim().to_string()).collect(),
        namespace: form
            .namespace
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty()),
        enabled: form.enabled,
        env: existing.map(|s| s.env.clone()).unwrap_or_default(),
        env_secrets: existing.map(|s| s.env_secrets.clone()).unwrap_or_default(),
        inherit_env: existing.map(|s| s.inherit_env.clone()).unwrap_or_default(),
        http: None,
        description: existing.and_then(|s| s.description.clone()),
    };
    cfg.upsert_server(server);
    seed_surface_from_default(cfg, surface);
    cfg.set_surface_enabled(surface, &name, form.enabled);
    Ok(())
}

/// Turn one definition on or off **for this surface**, asymmetrically, so one
/// surface's choice never disturbs another sharing the same file:
///
/// - **On:** join `[surfaces.<surface>]` and set the definition's own `enabled`,
///   so enabling really results in this surface hosting the server even when the
///   shared definition had been turned off.
/// - **Off:** drop this surface's entry only, leaving the definition enabled so
///   every other surface that lists it keeps hosting it.
///
/// Fails when no definition of that name exists, in either direction, rather
/// than materializing a surface entry for a server that does not exist.
fn apply_enabled(
    cfg: &mut ClientMcpConfig,
    surface: &str,
    name: &str,
    enabled: bool,
) -> Result<(), String> {
    let name = name.trim();
    if enabled {
        cfg.set_server_enabled(name, true)?;
    } else if !cfg.list_defined_servers().iter().any(|s| s.name == name) {
        return Err(format!("no such server: {name}"));
    }
    seed_surface_from_default(cfg, surface);
    cfg.set_surface_enabled(surface, name, enabled);
    Ok(())
}

/// Give `surface` a section of its own, seeded with the servers it was
/// inheriting from `[surfaces.default]`.
///
/// Every write path materializes a surface section, and an empty one does not
/// mean the same thing as no section at all:
/// [`ClientMcpConfig::resolved_servers`] falls back to `[surfaces.default]` only
/// while the surface has no section, and reads an explicit empty list as "hosts
/// nothing". So materializing an empty section un-hosts every server the surface
/// was inheriting, none of which the person edited. Seeding first makes the new
/// section say what the surface already hosted; the edit then adds or removes
/// one name from it.
///
/// A surface that already has a section inherits nothing, so it is left alone.
/// `[surfaces.default]` is never seeded and never changed - it is the fallback
/// every other surface reads. Only the `enabled` list is inherited:
/// `disabled_builtins` has no fallback to begin with.
pub(crate) fn seed_surface_from_default(cfg: &mut ClientMcpConfig, surface: &str) {
    if surface == DEFAULT_SURFACE || cfg.surfaces.contains_key(surface) {
        return;
    }
    let inherited = cfg.surface_enabled_names(DEFAULT_SURFACE).to_vec();
    if inherited.is_empty() {
        return;
    }
    cfg.surfaces.insert(
        surface.to_string(),
        SurfaceConfig {
            enabled: inherited,
            ..Default::default()
        },
    );
}

/// Parse the config at `path` strictly, for an edit.
///
/// **Fail-closed on a malformed file.** [`ClientMcpConfig::load`] is deliberately
/// tolerant - an unparseable config degrades to an empty one so a bad file never
/// stops a client connecting - but saving that empty config back would erase
/// every server definition on the machine, for every surface. An edit therefore
/// parses strictly and refuses rather than replacing what it could not read. A
/// file that is merely *absent* is fine: that is a first write.
pub fn load_strict(path: &Path) -> Result<ClientMcpConfig, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => ClientMcpConfig::from_toml(&contents),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ClientMcpConfig::default()),
        Err(err) => Err(format!("failed to read {}: {err}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const SURFACE: &str = "mac";

    /// A config path inside a fresh temp directory, plus the directory guard.
    ///
    /// Writes go to a real file because the fail-closed and atomic-save behavior
    /// under test is on-disk behavior; the developer's own
    /// `~/.config/adele/client-mcp.toml` is never touched.
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("adele-client-mcp-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self { dir }
        }

        fn path(&self) -> PathBuf {
            self.dir.join("client-mcp.toml")
        }

        fn write(&self, toml: &str) {
            std::fs::write(self.path(), toml).expect("seed config");
        }

        fn read(&self) -> ClientMcpConfig {
            load_strict(&self.path()).expect("config parses")
        }

        fn raw(&self) -> String {
            std::fs::read_to_string(self.path()).unwrap_or_default()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn upsert(json: &str) -> ClientServerWrite {
        ClientServerWrite::Upsert {
            server_json: json.to_string(),
        }
    }

    fn names_enabled_for(cfg: &ClientMcpConfig, surface: &str) -> Vec<String> {
        cfg.surface_enabled_names(surface).to_vec()
    }

    fn definition<'a>(cfg: &'a ClientMcpConfig, name: &str) -> Option<&'a McpServerConfig> {
        cfg.list_defined_servers().iter().find(|s| s.name == name)
    }

    /// What `surface` actually hosts, after the `[surfaces.default]` fallback and
    /// the definition-level filter. Sorted, because the answer is a set.
    fn names_hosted_by(cfg: &ClientMcpConfig, surface: &str) -> Vec<String> {
        let mut names: Vec<String> = cfg
            .resolved_servers(surface)
            .iter()
            .map(|s| s.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Two servers, both hosted by every surface through `[surfaces.default]`.
    /// The `mac` surface has no section of its own, so it inherits them.
    const INHERITING: &str = r#"
[[servers]]
name = "fs"
command = "fileio-mcp"

[[servers]]
name = "git"
command = "git-mcp"

[surfaces.default]
enabled = ["fs", "git"]
"#;

    // --- upsert ---------------------------------------------------------------

    #[tokio::test]
    async fn upsert_creates_a_definition_and_joins_this_surface() {
        let fx = Fixture::new("upsert-creates");
        upsert(r#"{"name":"notes","command":"notes-mcp","args":["--stdio"]}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect("upsert succeeds");

        let cfg = fx.read();
        let server = definition(&cfg, "notes").expect("definition written");
        assert_eq!(server.command, "notes-mcp");
        assert_eq!(server.args, vec!["--stdio"]);
        assert!(server.enabled);
        assert_eq!(names_enabled_for(&cfg, SURFACE), vec!["notes"]);
    }

    #[tokio::test]
    async fn upsert_into_a_missing_file_is_a_first_write() {
        let fx = Fixture::new("upsert-first-write");
        assert!(!fx.path().exists());

        upsert(r#"{"name":"notes","command":"notes-mcp"}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect("first write succeeds");

        assert!(definition(&fx.read(), "notes").is_some());
    }

    #[tokio::test]
    async fn upsert_preserves_env_secrets_and_description() {
        let fx = Fixture::new("upsert-preserves");
        fx.write(
            r#"
[[servers]]
name = "notes"
command = "old-notes"
enabled = true
description = "keep me"
[servers.env_secrets]
TOKEN = "notes-token"
[servers.env]
NOTES_DIR = "/srv/notes"
"#,
        );

        upsert(r#"{"name":"notes","command":"notes-mcp"}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect("edit succeeds");

        let cfg = fx.read();
        let server = definition(&cfg, "notes").expect("definition survives");
        assert_eq!(server.command, "notes-mcp");
        assert_eq!(server.description.as_deref(), Some("keep me"));
        assert_eq!(
            server.env_secrets.get("TOKEN").map(String::as_str),
            Some("notes-token")
        );
        assert_eq!(
            server.env.get("NOTES_DIR").map(String::as_str),
            Some("/srv/notes")
        );
    }

    #[tokio::test]
    async fn upsert_disabled_defines_the_server_but_leaves_this_surface_off_it() {
        let fx = Fixture::new("upsert-disabled");
        upsert(r#"{"name":"notes","command":"notes-mcp","enabled":false}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect("upsert succeeds");

        let cfg = fx.read();
        assert!(!definition(&cfg, "notes").expect("defined").enabled);
        assert!(names_enabled_for(&cfg, SURFACE).is_empty());
    }

    #[tokio::test]
    async fn upsert_normalizes_a_blank_namespace_to_none() {
        let fx = Fixture::new("upsert-namespace");
        upsert(r#"{"name":"notes","command":"notes-mcp","namespace":"  "}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect("upsert succeeds");

        assert_eq!(
            definition(&fx.read(), "notes").expect("defined").namespace,
            None
        );
    }

    #[tokio::test]
    async fn upsert_refuses_an_empty_name() {
        let fx = Fixture::new("upsert-empty-name");
        let err = upsert(r#"{"name":"  ","command":"notes-mcp"}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect_err("an empty name is refused");

        assert!(err.contains("name"), "{err}");
        assert!(fx.raw().is_empty(), "nothing was written");
    }

    #[tokio::test]
    async fn upsert_refuses_a_server_with_no_command() {
        let fx = Fixture::new("upsert-no-command");
        let err = upsert(r#"{"name":"notes","command":"  "}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect_err("a command is required");

        assert!(err.contains("command"), "{err}");
        assert!(fx.raw().is_empty(), "nothing was written");
    }

    #[tokio::test]
    async fn upsert_refuses_to_rewrite_an_http_server_as_stdio() {
        let fx = Fixture::new("upsert-http");
        fx.write(
            r#"
[[servers]]
name = "remote"
enabled = true
[servers.http]
url = "https://mcp.example.com/sse"
"#,
        );

        let err = upsert(r#"{"name":"remote","command":"remote-mcp"}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect_err("an http definition is refused");

        assert!(err.contains("http"), "{err}");
        assert!(
            definition(&fx.read(), "remote")
                .expect("definition survives")
                .http
                .is_some(),
            "the endpoint is left alone"
        );
    }

    #[tokio::test]
    async fn upsert_refuses_a_field_this_core_cannot_honour() {
        let fx = Fixture::new("upsert-unknown-field");
        let err = upsert(r#"{"name":"notes","command":"notes-mcp","transport":"http"}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect_err("an unknown field is refused");

        assert!(err.contains("invalid server json"), "{err}");
        assert!(fx.raw().is_empty(), "nothing was written");
    }

    // --- enable / disable -----------------------------------------------------

    #[tokio::test]
    async fn enabling_joins_this_surface_and_switches_the_definition_on() {
        let fx = Fixture::new("enable-on");
        fx.write(
            r#"
[[servers]]
name = "notes"
command = "notes-mcp"
enabled = false
"#,
        );

        ClientServerWrite::SetEnabled {
            name: "notes".to_string(),
            enabled: true,
        }
        .apply(&fx.path(), SURFACE)
        .await
        .expect("enable succeeds");

        let cfg = fx.read();
        assert!(definition(&cfg, "notes").expect("defined").enabled);
        assert_eq!(names_enabled_for(&cfg, SURFACE), vec!["notes"]);
    }

    #[tokio::test]
    async fn disabling_drops_only_this_surface_and_leaves_others_hosting_it() {
        let fx = Fixture::new("enable-off");
        fx.write(
            r#"
[[servers]]
name = "notes"
command = "notes-mcp"
enabled = true

[surfaces.mac]
enabled = ["notes"]

[surfaces.gtk]
enabled = ["notes"]
"#,
        );

        ClientServerWrite::SetEnabled {
            name: "notes".to_string(),
            enabled: false,
        }
        .apply(&fx.path(), SURFACE)
        .await
        .expect("disable succeeds");

        let cfg = fx.read();
        assert!(names_enabled_for(&cfg, SURFACE).is_empty());
        assert_eq!(names_enabled_for(&cfg, "gtk"), vec!["notes"]);
        assert!(
            definition(&cfg, "notes").expect("defined").enabled,
            "the definition stays on for the surfaces that still list it"
        );
    }

    #[tokio::test]
    async fn enabling_an_undefined_server_fails_and_writes_nothing() {
        let fx = Fixture::new("enable-unknown");
        fx.write("");

        let err = ClientServerWrite::SetEnabled {
            name: "ghost".to_string(),
            enabled: true,
        }
        .apply(&fx.path(), SURFACE)
        .await
        .expect_err("an undefined server is refused");

        assert!(err.contains("no such server"), "{err}");
        assert!(fx.raw().is_empty(), "nothing was written");
    }

    #[tokio::test]
    async fn disabling_an_undefined_server_fails_and_writes_nothing() {
        let fx = Fixture::new("disable-unknown");
        fx.write("");

        let err = ClientServerWrite::SetEnabled {
            name: "ghost".to_string(),
            enabled: false,
        }
        .apply(&fx.path(), SURFACE)
        .await
        .expect_err("an undefined server is refused");

        assert!(err.contains("no such server"), "{err}");
        assert!(fx.raw().is_empty(), "nothing was written");
    }

    // --- remove ---------------------------------------------------------------

    #[tokio::test]
    async fn removing_drops_the_definition_and_every_surface_membership() {
        let fx = Fixture::new("remove");
        fx.write(
            r#"
[[servers]]
name = "notes"
command = "notes-mcp"
enabled = true

[surfaces.mac]
enabled = ["notes"]

[surfaces.gtk]
enabled = ["notes"]
"#,
        );

        ClientServerWrite::Remove {
            name: "notes".to_string(),
        }
        .apply(&fx.path(), SURFACE)
        .await
        .expect("remove succeeds");

        let cfg = fx.read();
        assert!(definition(&cfg, "notes").is_none());
        assert!(names_enabled_for(&cfg, SURFACE).is_empty());
        assert!(names_enabled_for(&cfg, "gtk").is_empty());
    }

    #[tokio::test]
    async fn removing_an_undefined_server_fails_and_writes_nothing() {
        let fx = Fixture::new("remove-unknown");
        fx.write("");

        let err = ClientServerWrite::Remove {
            name: "ghost".to_string(),
        }
        .apply(&fx.path(), SURFACE)
        .await
        .expect_err("an undefined server is refused");

        assert!(err.contains("no such server"), "{err}");
        assert!(fx.raw().is_empty(), "nothing was written");
    }

    // --- fail-closed ----------------------------------------------------------

    #[tokio::test]
    async fn a_malformed_config_is_refused_rather_than_overwritten() {
        let fx = Fixture::new("malformed");
        let broken = "this is not toml {{{";
        fx.write(broken);

        upsert(r#"{"name":"notes","command":"notes-mcp"}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect_err("a config that cannot be parsed is refused");

        assert_eq!(fx.raw(), broken, "the file every client reads is untouched");
    }

    // --- inheritance from [surfaces.default] ----------------------------------

    /// Adding a server on a surface that inherits `[surfaces.default]` must not
    /// un-host what it was inheriting. The write gives the surface a section of
    /// its own, so that section has to carry the inherited names as well as the
    /// new one.
    #[tokio::test]
    async fn adding_a_server_keeps_what_this_surface_inherited() {
        let fx = Fixture::new("inherit-upsert");
        fx.write(INHERITING);

        upsert(r#"{"name":"notes","command":"notes-mcp"}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect("upsert succeeds");

        assert_eq!(names_hosted_by(&fx.read(), SURFACE), ["fs", "git", "notes"]);
    }

    /// The same for the enable path: switching one server on for an inheriting
    /// surface adds to what it hosts rather than replacing it.
    #[tokio::test]
    async fn enabling_a_server_keeps_what_this_surface_inherited() {
        let fx = Fixture::new("inherit-enable");
        fx.write(&format!(
            r#"{INHERITING}
[[servers]]
name = "notes"
command = "notes-mcp"
"#
        ));

        ClientServerWrite::SetEnabled {
            name: "notes".to_string(),
            enabled: true,
        }
        .apply(&fx.path(), SURFACE)
        .await
        .expect("enable succeeds");

        assert_eq!(names_hosted_by(&fx.read(), SURFACE), ["fs", "git", "notes"]);
    }

    /// Switching one inherited server off must remove that one only. The other
    /// inherited servers were never touched by the person doing the edit.
    #[tokio::test]
    async fn disabling_one_inherited_server_keeps_the_others() {
        let fx = Fixture::new("inherit-disable");
        fx.write(INHERITING);

        ClientServerWrite::SetEnabled {
            name: "fs".to_string(),
            enabled: false,
        }
        .apply(&fx.path(), SURFACE)
        .await
        .expect("disable succeeds");

        assert_eq!(names_hosted_by(&fx.read(), SURFACE), ["git"]);
    }

    /// `[surfaces.default]` is the fallback every other surface reads, so an edit
    /// made for one surface must never change it.
    #[tokio::test]
    async fn a_write_never_edits_the_default_surface() {
        let fx = Fixture::new("inherit-default-untouched");
        fx.write(INHERITING);

        upsert(r#"{"name":"notes","command":"notes-mcp"}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect("upsert succeeds");

        let cfg = fx.read();
        assert_eq!(names_enabled_for(&cfg, "default"), ["fs", "git"]);
        assert!(cfg.surface_disabled_builtins("default").is_empty());
    }

    /// A surface that already has a section of its own inherits nothing, so the
    /// write must not import the default list into it.
    #[tokio::test]
    async fn a_surface_with_its_own_section_is_not_seeded_from_default() {
        let fx = Fixture::new("inherit-own-section");
        fx.write(&format!(
            r#"{INHERITING}
[surfaces.mac]
enabled = []
"#
        ));

        upsert(r#"{"name":"notes","command":"notes-mcp"}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect("upsert succeeds");

        assert_eq!(names_hosted_by(&fx.read(), SURFACE), ["notes"]);
    }

    // --- serialization --------------------------------------------------------

    /// How many writers the concurrency cases dispatch at once. Enough that an
    /// unserialized read-modify-write loses at least one update on every run.
    const WRITERS: usize = 16;

    /// Writes dispatched together must all land. Each edit reads the whole file,
    /// changes one thing and writes it back, so two writers that overlap can
    /// silently drop the first one's result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_all_land() {
        let fx = Fixture::new("concurrent-writes");
        fx.write("");

        let mut writers = Vec::new();
        for i in 0..WRITERS {
            let path = fx.path();
            writers.push(tokio::spawn(async move {
                upsert(&format!(r#"{{"name":"s{i:02}","command":"s{i:02}-mcp"}}"#))
                    .apply(&path, SURFACE)
                    .await
            }));
        }
        for writer in writers {
            writer
                .await
                .expect("the writer task must not panic")
                .expect("every write succeeds");
        }

        let cfg = fx.read();
        let mut landed: Vec<String> = cfg
            .list_defined_servers()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        landed.sort_unstable();
        let expected: Vec<String> = (0..WRITERS).map(|i| format!("s{i:02}")).collect();
        assert_eq!(landed, expected, "no write may be lost");
    }

    /// The same file's surface membership must survive the same race: every
    /// writer joins this surface, so a lost update shows up here too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_all_join_this_surface() {
        let fx = Fixture::new("concurrent-surface");
        fx.write("");

        let mut writers = Vec::new();
        for i in 0..WRITERS {
            let path = fx.path();
            writers.push(tokio::spawn(async move {
                upsert(&format!(r#"{{"name":"s{i:02}","command":"s{i:02}-mcp"}}"#))
                    .apply(&path, SURFACE)
                    .await
            }));
        }
        for writer in writers {
            writer
                .await
                .expect("the writer task must not panic")
                .expect("every write succeeds");
        }

        let cfg = fx.read();
        let mut listed = names_enabled_for(&cfg, SURFACE);
        listed.sort_unstable();
        let expected: Vec<String> = (0..WRITERS).map(|i| format!("s{i:02}")).collect();
        assert_eq!(listed, expected);
    }

    /// A refused write must leave the next one free to run: serialization that
    /// keeps its hold after a failure would stall every later edit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_write_does_not_block_the_next_one() {
        let fx = Fixture::new("refused-then-next");
        fx.write("");

        upsert(r#"{"name":"notes","command":"  "}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect_err("a command is required");

        let next = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            upsert(r#"{"name":"notes","command":"notes-mcp"}"#).apply(&fx.path(), SURFACE),
        )
        .await
        .expect("the next write must not wait on the refused one");
        next.expect("the next write succeeds");

        assert!(definition(&fx.read(), "notes").is_some());
    }

    #[tokio::test]
    async fn an_edit_leaves_another_surfaces_servers_alone() {
        let fx = Fixture::new("other-surface");
        fx.write(
            r#"
[[servers]]
name = "shared"
command = "shared-mcp"
enabled = true

[surfaces.gtk]
enabled = ["shared"]
disabled_builtins = ["fileio"]
"#,
        );

        upsert(r#"{"name":"notes","command":"notes-mcp"}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect("upsert succeeds");

        let cfg = fx.read();
        assert_eq!(names_enabled_for(&cfg, "gtk"), vec!["shared"]);
        assert_eq!(cfg.surface_disabled_builtins("gtk"), vec!["fileio"]);
        assert!(definition(&cfg, "shared").is_some());
    }
}
