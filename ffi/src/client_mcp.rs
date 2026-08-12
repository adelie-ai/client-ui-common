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
//! Every write here goes through [`ClientMcpConfig::edit`], which holds an
//! exclusive lock on the config's sidecar for the whole read-mutate-write
//! transaction. That is what orders this core's writes against the *other*
//! clients on the machine; a lock only helps where every writer takes it.
//!
//! As far as `flock` reaches, which `edit` is explicit about: on a network home
//! directory (NFS or SMB) the lock can be local to one host, or refused
//! outright. `edit` names macOS especially, and `mac` is one of the surfaces
//! this crate serves, so an Adele whose `~/.config` is on a network share can
//! still lose an update between two machines. The in-process ordering below is
//! unaffected, and so is the atomicity of each individual save.
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

/// Orders every read-modify-write of `client-mcp.toml` this core makes, ahead
/// of the machine-wide lock [`ClientMcpConfig::edit`] takes.
///
/// **One core only.** Each Adele client on the machine holds its own core, and
/// they all write this one file. Ordering *between* clients is `edit`'s sidecar
/// lock; this one orders the tasks inside a single core.
///
/// Why keep it, when `edit` already serializes: `edit` waits a bounded two
/// seconds for the sidecar and then refuses, so two of this core's own tasks
/// racing each other could turn into a refusal the person has to see and retry.
/// Taken first, they queue in this process instead, and only one of them ever
/// opens and locks the sidecar.
///
/// It is private to [`edit_config`], which is the only way to reach `edit` from
/// this crate. Ordering the two locks correctly is then a property of one
/// function rather than a rule each write path has to remember.
static WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Run one [`ClientMcpConfig::edit`] transaction: the single door from this
/// crate to a `client-mcp.toml` write.
///
/// Takes [`WRITE_LOCK`] first and holds it across the whole call, then runs the
/// transaction. Both locks are released on every exit path, including a refusal
/// and a panic.
///
/// `edit` is synchronous. It sleeps while another client holds the sidecar
/// lock, for up to two seconds before it gives up, so it runs in
/// `tokio::task::spawn_blocking` rather than on a runtime worker: this core's
/// runtime has two worker threads, and parking one of them for two seconds
/// stalls half of it.
///
/// **Not re-entrant, either half.** `change` runs on the blocking thread inside
/// both locks; calling back into `edit_config` from it would deadlock on
/// [`WRITE_LOCK`]. Both of this crate's change closures are pure in-memory
/// mutations of the config they are handed.
///
/// Dropping this future does not cancel the transaction - a blocking task runs
/// to completion - so a dropped caller releases [`WRITE_LOCK`] while its own
/// edit is still in flight. Both write paths run in an un-cancelled
/// `tokio::spawn`, and the sidecar lock still rules out a lost update either
/// way; the cost would only be a later edit told to try again.
pub(crate) async fn edit_config<F>(path: &Path, change: F) -> Result<(), String>
where
    F: FnOnce(&mut ClientMcpConfig) -> Result<(), String> + Send + 'static,
{
    let _guard = WRITE_LOCK.lock().await;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || ClientMcpConfig::edit(&path, change))
        .await
        .map_err(|err| format!("the client MCP config edit did not complete: {err}"))?
}

impl ClientServerWrite {
    /// Apply this edit to the config at `path`, for `surface`.
    ///
    /// One [`ClientMcpConfig::edit`] transaction, via [`edit_config`]: the
    /// strict re-read, the change and the save all happen inside the lock `edit`
    /// holds on the config's sidecar, so a concurrent editor in **another**
    /// Adele client queues rather than losing one of the two changes.
    ///
    /// Fails without writing anything when the edit is not valid, so a refused
    /// edit leaves the file exactly as it was. Both locks are released either
    /// way.
    pub async fn apply(&self, path: &Path, surface: &str) -> Result<(), String> {
        let write = self.clone();
        let surface = surface.to_string();
        edit_config(path, move |cfg| write.change(cfg, &surface)).await
    }

    /// The change itself, run inside the [`ClientMcpConfig::edit`] transaction
    /// against the config that transaction re-read under the lock.
    ///
    /// Returning `Err` abandons the edit, and `edit` writes nothing.
    fn change(&self, cfg: &mut ClientMcpConfig, surface: &str) -> Result<(), String> {
        match self {
            Self::Upsert { server_json } => apply_upsert(cfg, surface, server_json),
            Self::Remove { name } => cfg.remove_server(name.trim()),
            Self::SetEnabled { name, enabled } => apply_enabled(cfg, surface, name, *enabled),
        }
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
///
/// Turning **on** a definition that reaches its server over HTTP is refused, as
/// the upsert path refuses to write one: the client MCP host spawns `command`,
/// which an HTTP definition leaves empty, so the result could only ever fail to
/// start. Turning one **off** stays allowed - a definition already in a
/// surface's list needs a way out.
fn apply_enabled(
    cfg: &mut ClientMcpConfig,
    surface: &str,
    name: &str,
    enabled: bool,
) -> Result<(), String> {
    let name = name.trim();
    // `None` when nothing of that name is defined; `Some(true)` when the
    // definition that is reaches its server over HTTP.
    let over_http = cfg
        .list_defined_servers()
        .iter()
        .find(|s| s.name == name)
        .map(|s| s.http.is_some());
    if enabled {
        if over_http == Some(true) {
            return Err(format!(
                "server '{name}' is configured for http; this client runs stdio servers only"
            ));
        }
        cfg.set_server_enabled(name, true)?;
    } else if over_http.is_none() {
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
/// Seeding never reads into or writes to `[surfaces.default]` itself: it is the
/// fallback every other surface reads, and one surface's edit must not move it.
/// (Deleting a *definition* is the one write that still reaches it, because
/// [`ClientMcpConfig::remove_server`] prunes the name from every surface - a
/// definition that no longer exists must not be listed anywhere.)
///
/// Only the `enabled` list is inherited: `disabled_builtins` has no fallback to
/// begin with.
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

/// Take the sidecar lock [`ClientMcpConfig::edit`] uses, the way another Adele
/// client on the machine would, and hold it until the returned file drops.
///
/// A `flock` belongs to an open file description, so a second `open` in this
/// same process contends exactly as a second process does - which is what lets
/// a test stand in for the other client. The sidecar path is derived here rather
/// than read from `client-common`, so the test pins the contract instead of
/// following the implementation.
#[cfg(test)]
pub(crate) fn hold_config_lock(config_path: &Path) -> std::fs::File {
    let mut lock_name = config_path
        .file_name()
        .expect("a config path has a file name")
        .to_os_string();
    lock_name.push(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(config_path.with_file_name(lock_name))
        .expect("open the sidecar lock");
    file.try_lock().expect("take the sidecar lock");
    file
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
    ///
    /// The directory is unique per fixture, not a fixed path derived from
    /// `name`: the edit lock is machine-wide, so two test binaries running at the
    /// same time on one shared path contend for the real sidecar lock and refuse
    /// each other's writes. `name` survives as a prefix, to name the directory a
    /// failing case leaves behind.
    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = tempfile::Builder::new()
                .prefix(&format!("adele-client-mcp-{name}-"))
                .tempdir()
                .expect("temp dir");
            Self { dir }
        }

        fn path(&self) -> PathBuf {
            self.dir.path().join("client-mcp.toml")
        }

        fn write(&self, toml: &str) {
            std::fs::write(self.path(), toml).expect("seed config");
        }

        /// The config as it is on disk, parsed strictly: a test that means to
        /// assert on what was written must not read a tolerant empty default
        /// when the write produced something unparseable.
        fn read(&self) -> ClientMcpConfig {
            ClientMcpConfig::from_toml(&self.raw()).expect("config parses")
        }

        fn raw(&self) -> String {
            std::fs::read_to_string(self.path()).unwrap_or_default()
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

    /// One definition that reaches its server over HTTP, which the client MCP
    /// host cannot run: it spawns `command`, and this definition has none.
    const HTTP_DEFINITION: &str = r#"
[[servers]]
name = "search"
enabled = true
[servers.http]
url = "https://mcp.example.com/sse"
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

    /// A definition with an HTTP endpoint carries no command, and the client MCP
    /// host spawns a command. Enabling one can only produce a server that fails
    /// to start, so the write is refused and says why.
    #[tokio::test]
    async fn enabling_an_http_definition_is_refused() {
        let fx = Fixture::new("enable-http");
        fx.write(HTTP_DEFINITION);
        let before = fx.raw();

        let err = ClientServerWrite::SetEnabled {
            name: "search".to_string(),
            enabled: true,
        }
        .apply(&fx.path(), SURFACE)
        .await
        .expect_err("an http definition cannot be hosted here");

        assert!(err.contains("http"), "{err}");
        assert_eq!(fx.raw(), before, "the refusal writes nothing");
    }

    /// Disabling must keep working: a definition already in a surface's list
    /// needs a way out, whatever transport it names.
    #[tokio::test]
    async fn disabling_an_http_definition_still_works() {
        let fx = Fixture::new("disable-http");
        fx.write(&format!(
            r#"{HTTP_DEFINITION}
[surfaces.mac]
enabled = ["search"]
"#
        ));

        ClientServerWrite::SetEnabled {
            name: "search".to_string(),
            enabled: false,
        }
        .apply(&fx.path(), SURFACE)
        .await
        .expect("disable succeeds");

        assert!(names_enabled_for(&fx.read(), SURFACE).is_empty());
    }

    // --- remove ---------------------------------------------------------------

    /// Deleting a definition needs no ability to run it, so an HTTP one is
    /// removable.
    #[tokio::test]
    async fn removing_an_http_definition_still_works() {
        let fx = Fixture::new("remove-http");
        fx.write(HTTP_DEFINITION);

        ClientServerWrite::Remove {
            name: "search".to_string(),
        }
        .apply(&fx.path(), SURFACE)
        .await
        .expect("remove succeeds");

        assert!(definition(&fx.read(), "search").is_none());
    }

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

    /// The refusal must be the strict parse, named as such. Every failure the
    /// edit can return leaves the file untouched, so asserting only on "it
    /// failed" would be satisfied by a lock failure and pass without the
    /// fail-closed read ever running.
    #[tokio::test]
    async fn a_malformed_config_is_refused_rather_than_overwritten() {
        let fx = Fixture::new("malformed");
        let broken = "this is not toml {{{";
        fx.write(broken);

        let err = upsert(r#"{"name":"notes","command":"notes-mcp"}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect_err("a config that cannot be parsed is refused");

        assert!(
            err.contains("parse error"),
            "the refusal must name the parse failure; got: {err}"
        );
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

    /// `[surfaces.default]` is the fallback every other surface reads, so adding
    /// a server for one surface must never change it. (Deleting a definition
    /// does prune it from every surface, `default` included - see
    /// `removing_drops_the_definition_and_every_surface_membership`.)
    #[tokio::test]
    async fn an_upsert_never_edits_the_default_surface() {
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

    /// How many writers the concurrency cases dispatch at once. Every run of
    /// these cases against the unserialized code lost several of them; the
    /// number is high to make a lost update easy to hit, not because any one
    /// count is guaranteed to lose one.
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

    /// A refused write must release the file lock it took, at once.
    ///
    /// This is the latency half of "a refusal does not block the next writer",
    /// and it is measured on the fixture's own sidecar, which no other test
    /// touches, so the figure means what it says. `hold_config_lock` takes the
    /// lock with `try_lock` and panics if it cannot, so a lock the refusal
    /// stranded fails here immediately rather than through a timeout.
    #[tokio::test]
    async fn a_refused_write_releases_the_file_lock_at_once() {
        let fx = Fixture::new("refused-releases-file-lock");
        fx.write("");

        upsert(r#"{"name":"notes","command":"  "}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect_err("a command is required");

        let started = std::time::Instant::now();
        let free = hold_config_lock(&fx.path());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "the refused write left the file lock held: took {:?}",
            started.elapsed()
        );
        drop(free);
    }

    /// A refused write must not strand the in-process lock either, or every
    /// later edit in this core stalls for good. What this catches is a hold that
    /// outlives a failure - hand-rolled lock and unlock calls, say, with the
    /// unlock after an early return.
    ///
    /// The budget is a hang guard, deliberately not a latency bound: a stranded
    /// `tokio::sync::Mutex` guard never releases, so the two cases this
    /// separates are "some seconds" and "forever". A latency bound cannot be
    /// asserted here, because the in-process lock is one static shared by the
    /// whole test binary and several cases park on it for the two seconds `edit`
    /// waits; the latency claim is the case above, on the fixture's own sidecar.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_write_does_not_strand_the_in_process_lock() {
        let fx = Fixture::new("refused-then-next");
        fx.write("");

        upsert(r#"{"name":"notes","command":"  "}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect_err("a command is required");

        let next = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            upsert(r#"{"name":"notes","command":"notes-mcp"}"#).apply(&fx.path(), SURFACE),
        )
        .await
        .expect("the refused write stranded the in-process lock");
        next.expect("the next write succeeds");

        assert!(definition(&fx.read(), "notes").is_some());
    }

    /// The in-process lock orders this core's own writes. It says nothing about
    /// the other Adele clients on the machine, which write the same file, so the
    /// write must also take the machine-wide lock on the config's sidecar.
    ///
    /// Driven by holding that sidecar lock: a write that takes it is refused,
    /// and one that ignores it overwrites the other client's transaction.
    #[tokio::test]
    async fn a_write_is_refused_while_another_client_holds_the_file_lock() {
        let fx = Fixture::new("write-file-lock-held");
        fx.write("");
        let held = hold_config_lock(&fx.path());

        let err = upsert(r#"{"name":"notes","command":"notes-mcp"}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect_err("an edit in flight elsewhere must refuse this write");

        assert!(err.contains("another Adele client is editing"), "{err}");
        assert!(fx.raw().is_empty(), "nothing was written");
        drop(held);
    }

    /// And the refusal is not permanent: a write that starts while the other
    /// client still holds the lock retries, and lands once that client is done.
    ///
    /// The holder is released from another thread after the write is already in
    /// flight, so the retry loop is what carries this write through. Releasing
    /// the holder before dispatching would pass with no retry loop at all, which
    /// is why the elapsed time is asserted: the write must have waited.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_retries_until_the_other_client_releases_the_file_lock() {
        const HELD_FOR: std::time::Duration = std::time::Duration::from_millis(300);

        let fx = Fixture::new("write-file-lock-released");
        fx.write("");
        let held = hold_config_lock(&fx.path());
        std::thread::spawn(move || {
            std::thread::sleep(HELD_FOR);
            drop(held);
        });

        let started = std::time::Instant::now();
        upsert(r#"{"name":"notes","command":"notes-mcp"}"#)
            .apply(&fx.path(), SURFACE)
            .await
            .expect("the retry must carry the write through once the lock frees");
        let waited = started.elapsed();

        assert!(
            waited >= HELD_FOR,
            "the write did not contend for the lock at all: finished in {waited:?}"
        );
        assert!(definition(&fx.read(), "notes").is_some());
    }

    /// A write that is waiting on the file lock must not park a runtime worker.
    ///
    /// `edit` is synchronous and sleeps in its retry loop, so calling it inline
    /// would own a worker thread for the whole two-second wait. This core's
    /// runtime has two workers, so that is half of it; the runtime here has one,
    /// which makes a parked worker the whole runtime, and a probe task that
    /// cannot be scheduled fails the case.
    ///
    /// Two details are what make it discriminate, and both were arrived at by
    /// running it against an inline `edit`:
    ///
    /// - It is a plain `#[test]` driving the runtime from outside. A
    ///   `#[tokio::test]` body runs inside `block_on`, and that thread also runs
    ///   spawned tasks, so it picks the probe up itself and the case passes with
    ///   the write inline.
    /// - It probes for as long as the write is in flight, rather than once after
    ///   a fixed pause. The in-process lock is shared by the whole test binary,
    ///   so a single early probe can land while the write is still queued on
    ///   that lock and has not reached the sidecar at all.
    #[test]
    fn a_write_waiting_on_the_file_lock_leaves_the_runtime_responsive() {
        /// How long one probe task may take to be scheduled. Far above the
        /// microseconds a free runtime needs, far below the two seconds a parked
        /// worker would cost.
        const PROBE_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

        let fx = Fixture::new("write-does-not-park-the-runtime");
        fx.write("");
        let held = hold_config_lock(&fx.path());

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build the single-worker runtime");
        let handle = runtime.handle().clone();

        let path = fx.path();
        let writer = handle.spawn(async move {
            upsert(r#"{"name":"notes","command":"notes-mcp"}"#)
                .apply(&path, SURFACE)
                .await
        });

        while !writer.is_finished() {
            let (tx, rx) = std::sync::mpsc::channel();
            handle.spawn(async move {
                let _ = tx.send(());
            });
            rx.recv_timeout(PROBE_BUDGET).expect(
                "the runtime must still schedule work while a write waits on the file lock",
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        runtime
            .block_on(writer)
            .expect("the writer task must not panic")
            .expect_err("the held lock refuses the write");
        drop(held);
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
