//! MCP-servers admin view-model (client-tooling Phase 1, epic
//! desktop-assistant#531).
//!
//! A pure, view-agnostic model for the unified "MCP servers" admin panel that
//! every UI (gtk, tui, and — via the ffi cdylib — kde) renders. It merges the
//! two populations of MCP servers a client can see:
//!
//! - **Daemon-run** servers, described by the daemon's [`McpServerView`] wire
//!   type (the fleet the daemon hosts on the user's behalf).
//! - **Client-run** servers, described by the plain [`ClientServerDto`] the
//!   host client fills in from its own registry.
//!
//! Everything here takes *already-resolved plain data* and returns plain data.
//! It deliberately does NOT depend on `client-common` or `mcp-client` (nor any
//! transport), so the crate stays wasm-clean: the host client resolves its
//! server list however it likes and hands the results in.
//!
//! Honesty over inference: the transport chip reports the real transport
//! (`stdio`/`http`), never a "local"/"remote" guess, and the runner label only
//! adds a host suffix when the client's link to the daemon is genuinely remote.

use desktop_assistant_api_model::McpServerView;

/// Where an MCP server actually runs.
///
/// Why: the admin panel groups and filters by this, and the runner drives the
/// [`runner_label`] shown next to each row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    /// Runs inside the client process (a client-registered MCP server).
    Client,
    /// Runs inside (or on behalf of) the daemon.
    Daemon,
}

impl Runner {
    /// Stable tiebreak rank used when two rows share a (case-insensitive) name.
    /// Daemon-run servers sort ahead of client-run ones. Kept private: it is an
    /// ordering detail, not part of the public contract.
    fn sort_rank(self) -> u8 {
        match self {
            Runner::Daemon => 0,
            Runner::Client => 1,
        }
    }
}

/// The runner filter backing the panel's dropdown. `All` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunnerFilter {
    /// Show every server regardless of runner.
    #[default]
    All,
    /// Show only daemon-run servers.
    Daemon,
    /// Show only client-run servers.
    Client,
}

/// How an MCP server is hosted - orthogonal to its [`Runner`].
///
/// Why: a client-run server can be an external subprocess, an external remote
/// endpoint, or a server compiled into the client and hosted in-process
/// (desktop-assistant#538). The panel renders this as a chip via [`kind_label`];
/// for external servers the kind mirrors the transport, so the two agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerKind {
    /// External process speaking MCP over stdio.
    Stdio,
    /// External endpoint speaking MCP over streamable HTTP.
    Http,
    /// Compiled into the client and hosted in-process - no subprocess or socket.
    BuiltIn,
}

impl ServerKind {
    /// Kind for an *external* (subprocess/remote) server from its transport
    /// string: `"http"` -> [`ServerKind::Http`], anything else -> [`Stdio`],
    /// mirroring [`transport_chip`]'s stdio default. Built-in rows set their kind
    /// explicitly and never pass through here.
    ///
    /// [`Stdio`]: ServerKind::Stdio
    fn from_transport(transport: &str) -> ServerKind {
        match transport {
            "http" => ServerKind::Http,
            _ => ServerKind::Stdio,
        }
    }
}

/// Plain client-side input describing one client-run MCP server.
///
/// Why a bespoke DTO: it lets this module stay free of `client-common` /
/// `mcp-client`. The host client (which *does* own those crates) resolves its
/// registry into this flat shape and passes it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientServerDto {
    /// Server name as shown in the panel.
    pub name: String,
    /// Transport: `"stdio"` or `"http"`.
    pub transport: String,
    /// Display status string (e.g. `disabled` / `running` / `error`).
    pub status: String,
    /// Number of tools the server exposes.
    pub tool_count: u32,
}

/// Plain client-side input describing one MCP server compiled into the client
/// and hosted in-process (desktop-assistant#538).
///
/// Why a bespoke DTO: like [`ClientServerDto`], it keeps this module free of the
/// client's in-process MCP host types. The host client enumerates its
/// compiled-in built-ins into this flat shape and passes them in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinServerDto {
    /// Server name as shown in the panel - also the key an external client-run
    /// server of the same name overrides.
    pub name: String,
    /// The built-in's tool namespace (e.g. `"fileio"`).
    pub namespace: String,
    /// Number of tools the built-in exposes.
    pub tool_count: u32,
    /// `Some(name)` when an external client-run server of the same name shadows
    /// this built-in (the external one wins and the built-in renders disabled);
    /// `None` when the built-in is active.
    pub overridden_by: Option<String>,
}

/// One rendered row of the MCP-servers panel — exactly the fields both gtk and
/// future clients draw, tagged with the [`Runner`] that produced it. Plain data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRow {
    /// Server name.
    pub name: String,
    /// Which side runs this server.
    pub runner: Runner,
    /// Transport: `"stdio"` or `"http"` (render via [`transport_chip`]).
    pub transport: String,
    /// Display status string, carried through from the source verbatim.
    pub status: String,
    /// Number of tools the server exposes.
    pub tool_count: u32,
    /// Optional detail (e.g. a last connection error); `None` when absent.
    pub detail: Option<String>,
    /// How the server is hosted (render via [`kind_label`]). For daemon/client
    /// rows it is derived from [`transport`](Self::transport); built-in rows are
    /// [`ServerKind::BuiltIn`].
    pub kind: ServerKind,
    /// `None` when the row is active; `Some(reason)` when it should render
    /// disabled (e.g. a built-in shadowed by an external server of the same
    /// name), with `reason` the user-facing explanation.
    pub disabled_reason: Option<String>,
}

/// Human label for a row's runner.
///
/// - [`Runner::Client`] -> `"client"` (client tools always run locally in the
///   client, so `is_remote`/`host` are ignored).
/// - [`Runner::Daemon`], co-located link -> `"daemon"`.
/// - [`Runner::Daemon`], remote link with a known host -> `"daemon · <host>"`.
///
/// `is_remote` reflects whether the client's connection to the daemon is a
/// remote WebSocket (`true`) versus a co-located UDS/D-Bus link (`false`); the
/// host suffix is only added when both `is_remote` and a `host` are present.
pub fn runner_label(runner: Runner, is_remote: bool, host: Option<&str>) -> String {
    match runner {
        Runner::Client => "client".to_string(),
        Runner::Daemon => match (is_remote, host) {
            (true, Some(host)) => format!("daemon · {host}"),
            _ => "daemon".to_string(),
        },
    }
}

/// Honest transport chip text: `"stdio"` or `"http"`. Never emits the retired
/// "local"/"remote" conflation. Anything that is not `"http"` is treated as
/// `"stdio"` (the daemon only ever reports those two values).
pub fn transport_chip(transport: &str) -> &'static str {
    match transport {
        "http" => "http",
        _ => "stdio",
    }
}

/// Human chip text for a row's [`ServerKind`]: `"stdio"` / `"http"` /
/// `"built-in"`. The unified successor to [`transport_chip`] that also names
/// built-ins; for external kinds it returns the same value the transport chip
/// would, so panels can render every row's chip from `row.kind` alone.
pub fn kind_label(kind: ServerKind) -> &'static str {
    match kind {
        ServerKind::Stdio => "stdio",
        ServerKind::Http => "http",
        ServerKind::BuiltIn => "built-in",
    }
}

/// Merge daemon-run and client-run servers into one panel-ordered list.
///
/// The 2-argument form for callers that have no built-in servers to surface. It
/// is exactly [`server_rows_with_builtins`] with an empty `builtins` slice, so
/// it returns the same rows it always has - existing panel callers (gtk/tui/kde)
/// are unaffected and adopt the built-in-aware form in a later slice.
pub fn server_rows(daemon: &[McpServerView], client: &[ClientServerDto]) -> Vec<ServerRow> {
    server_rows_with_builtins(daemon, client, &[])
}

/// Merge daemon-run, external client-run, and built-in servers into one
/// panel-ordered list.
///
/// Daemon items are tagged [`Runner::Daemon`]; external client items and
/// built-ins are both [`Runner::Client`] (a built-in is hosted in-process by the
/// client) and differ only in their [`ServerKind`]. Daemon/client `kind` is
/// derived from the transport ([`http`](ServerKind::Http) else
/// [`stdio`](ServerKind::Stdio)); built-ins are [`ServerKind::BuiltIn`]. A
/// built-in whose [`overridden_by`](BuiltinServerDto::overridden_by) is set
/// renders disabled with an "overridden by the external ..." reason.
///
/// The result is sorted alphabetically by name (case-insensitive) with the
/// [`Runner`] as a stable tiebreak (daemon before client). Built-ins are chained
/// after the external client rows, so on a name tie a shadowed built-in slots
/// directly after its active external override.
pub fn server_rows_with_builtins(
    daemon: &[McpServerView],
    client: &[ClientServerDto],
    builtins: &[BuiltinServerDto],
) -> Vec<ServerRow> {
    let daemon_rows = daemon.iter().map(|d| ServerRow {
        name: d.name.clone(),
        runner: Runner::Daemon,
        transport: d.transport.clone(),
        status: d.status.clone(),
        tool_count: d.tool_count,
        detail: d.detail.clone(),
        kind: ServerKind::from_transport(&d.transport),
        disabled_reason: None,
    });
    let client_rows = client.iter().map(|c| ServerRow {
        name: c.name.clone(),
        runner: Runner::Client,
        transport: c.transport.clone(),
        status: c.status.clone(),
        tool_count: c.tool_count,
        detail: None,
        kind: ServerKind::from_transport(&c.transport),
        disabled_reason: None,
    });
    let builtin_rows = builtins.iter().map(|b| ServerRow {
        name: b.name.clone(),
        runner: Runner::Client,
        // Built-ins have no wire transport; the chip comes from `kind`.
        transport: "builtin".to_string(),
        status: if b.overridden_by.is_some() {
            "disabled"
        } else {
            "running"
        }
        .to_string(),
        tool_count: b.tool_count,
        detail: None,
        kind: ServerKind::BuiltIn,
        disabled_reason: b
            .overridden_by
            .as_ref()
            .map(|n| format!("overridden by the external \"{n}\"")),
    });

    let mut rows: Vec<ServerRow> = daemon_rows.chain(client_rows).chain(builtin_rows).collect();
    // Case-insensitive name order, with the runner as a stable tiebreak so rows
    // that share a name order deterministically. `sort_by` is stable, so among
    // same-name client-runner rows the chain order holds: an external override
    // (client) precedes the built-in it shadows.
    rows.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then(a.runner.sort_rank().cmp(&b.runner.sort_rank()))
    });
    rows
}

/// Apply a [`RunnerFilter`] to already-built rows, preserving their order.
pub fn filter_rows(rows: &[ServerRow], filter: RunnerFilter) -> Vec<ServerRow> {
    rows.iter()
        .filter(|row| match filter {
            RunnerFilter::All => true,
            RunnerFilter::Daemon => row.runner == Runner::Daemon,
            RunnerFilter::Client => row.runner == Runner::Client,
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon [`McpServerView`] with only the fields this module reads set.
    fn dv(name: &str, transport: &str, status: &str, tools: u32) -> McpServerView {
        McpServerView {
            name: name.to_string(),
            transport: transport.to_string(),
            status: status.to_string(),
            tool_count: tools,
            ..Default::default()
        }
    }

    fn cv(name: &str, transport: &str, status: &str, tools: u32) -> ClientServerDto {
        ClientServerDto {
            name: name.to_string(),
            transport: transport.to_string(),
            status: status.to_string(),
            tool_count: tools,
        }
    }

    /// A [`BuiltinServerDto`] for a compiled-in server. `overridden_by` is the
    /// external server name shadowing it, or `None` when the built-in is active.
    fn bv(name: &str, tools: u32, overridden_by: Option<&str>) -> BuiltinServerDto {
        BuiltinServerDto {
            name: name.to_string(),
            namespace: name.to_string(),
            tool_count: tools,
            overridden_by: overridden_by.map(str::to_string),
        }
    }

    #[test]
    fn runner_enum_and_serverrow_construct() {
        let row = ServerRow {
            name: "fileio".to_string(),
            runner: Runner::Daemon,
            transport: "stdio".to_string(),
            status: "running".to_string(),
            tool_count: 5,
            detail: None,
            kind: ServerKind::Stdio,
            disabled_reason: None,
        };
        assert_eq!(row.runner, Runner::Daemon);
        assert_ne!(Runner::Daemon, Runner::Client);
        assert_eq!(row.name, "fileio");
        assert_eq!(row.tool_count, 5);
        assert_eq!(row.transport, "stdio");
        assert_eq!(row.detail, None);
        assert_eq!(row.kind, ServerKind::Stdio);
        assert_eq!(row.disabled_reason, None);

        // Runner is Copy: using it after a bind does not move it.
        let r = Runner::Client;
        let r2 = r;
        assert_eq!(r, r2);

        // ServerKind is Copy too.
        let k = ServerKind::BuiltIn;
        let k2 = k;
        assert_eq!(k, k2);
        assert_ne!(ServerKind::Stdio, ServerKind::Http);
    }

    #[test]
    fn runner_label_client_daemon_and_remote_host() {
        // Client tools run locally: remote/host are irrelevant.
        assert_eq!(runner_label(Runner::Client, false, None), "client");
        assert_eq!(
            runner_label(Runner::Client, true, Some("lab-host")),
            "client"
        );

        // Co-located daemon: no host suffix, even if a host string is supplied.
        assert_eq!(runner_label(Runner::Daemon, false, None), "daemon");
        assert_eq!(
            runner_label(Runner::Daemon, false, Some("lab-host")),
            "daemon"
        );

        // Remote daemon but host unknown: stays plain.
        assert_eq!(runner_label(Runner::Daemon, true, None), "daemon");

        // Remote daemon with a known host: host suffix appears.
        assert_eq!(
            runner_label(Runner::Daemon, true, Some("lab-host")),
            "daemon · lab-host"
        );
    }

    #[test]
    fn transport_chip_is_stdio_or_http() {
        assert_eq!(transport_chip("stdio"), "stdio");
        assert_eq!(transport_chip("http"), "http");

        // Honest: only ever stdio/http, never the retired local/remote labels.
        for t in ["stdio", "http"] {
            let chip = transport_chip(t);
            assert!(chip == "stdio" || chip == "http");
            assert_ne!(chip, "local");
            assert_ne!(chip, "remote");
        }
    }

    #[test]
    fn server_rows_tags_runner_per_source() {
        let daemon = vec![McpServerView {
            name: "alpha".to_string(),
            transport: "stdio".to_string(),
            status: "error".to_string(),
            tool_count: 0,
            detail: Some("boom".to_string()),
            ..Default::default()
        }];
        let client = vec![cv("beta", "http", "running", 1)];

        let rows = server_rows(&daemon, &client);
        assert_eq!(rows.len(), 2);

        let alpha = rows
            .iter()
            .find(|r| r.name == "alpha")
            .expect("alpha row present");
        let beta = rows
            .iter()
            .find(|r| r.name == "beta")
            .expect("beta row present");

        assert_eq!(alpha.runner, Runner::Daemon);
        assert_eq!(alpha.transport, "stdio");
        assert_eq!(alpha.status, "error");
        assert_eq!(alpha.detail.as_deref(), Some("boom"));
        // Kind is derived from transport for daemon/client rows.
        assert_eq!(alpha.kind, ServerKind::Stdio);
        assert_eq!(alpha.disabled_reason, None);

        assert_eq!(beta.runner, Runner::Client);
        assert_eq!(beta.transport, "http");
        assert_eq!(beta.kind, ServerKind::Http);
        assert_eq!(beta.disabled_reason, None);
        // Client rows never carry a daemon-side detail.
        assert_eq!(beta.detail, None);
    }

    #[test]
    fn server_rows_sorted_alphabetically_case_insensitive() {
        let daemon = vec![
            dv("Zeta", "stdio", "running", 1),
            dv("alpha", "stdio", "running", 1),
            dv("github", "http", "running", 2),
        ];
        let client = vec![
            cv("Beta", "http", "running", 1),
            cv("github", "stdio", "running", 1),
        ];

        let rows = server_rows(&daemon, &client);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "Beta", "github", "github", "Zeta"]);

        // Equal (case-insensitive) names break ties by runner: daemon first.
        let github_runners: Vec<Runner> = rows
            .iter()
            .filter(|r| r.name.eq_ignore_ascii_case("github"))
            .map(|r| r.runner)
            .collect();
        assert_eq!(github_runners, vec![Runner::Daemon, Runner::Client]);
    }

    #[test]
    fn server_rows_merges_daemon_and_client_ordered() {
        let daemon = vec![
            dv("git", "http", "running", 2),
            dv("time", "stdio", "running", 1),
        ];
        let client = vec![cv("browser", "stdio", "running", 3)];

        let rows = server_rows(&daemon, &client);
        assert_eq!(rows.len(), 3);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["browser", "git", "time"]);
    }

    #[test]
    fn server_rows_handles_empty_sources() {
        assert!(server_rows(&[], &[]).is_empty());

        let only_daemon = server_rows(&[dv("alpha", "stdio", "running", 1)], &[]);
        assert_eq!(only_daemon.len(), 1);
        assert_eq!(only_daemon[0].runner, Runner::Daemon);

        let only_client = server_rows(&[], &[cv("beta", "http", "running", 1)]);
        assert_eq!(only_client.len(), 1);
        assert_eq!(only_client[0].runner, Runner::Client);

        let only_builtin = server_rows_with_builtins(&[], &[], &[bv("notes", 2, None)]);
        assert_eq!(only_builtin.len(), 1);
        assert_eq!(only_builtin[0].runner, Runner::Client);
        assert_eq!(only_builtin[0].kind, ServerKind::BuiltIn);
    }

    #[test]
    fn filter_rows_all_daemon_client() {
        let daemon = vec![dv("alpha", "stdio", "running", 1)];
        let client = vec![cv("beta", "http", "running", 1)];
        // A built-in is client-runner, so it rides the Client filter with the
        // external client rows and is excluded by the Daemon filter.
        let builtins = vec![bv("notes", 2, None)];
        let rows = server_rows_with_builtins(&daemon, &client, &builtins);

        assert_eq!(RunnerFilter::default(), RunnerFilter::All);
        assert_eq!(filter_rows(&rows, RunnerFilter::All).len(), 3);

        let daemon_only = filter_rows(&rows, RunnerFilter::Daemon);
        assert_eq!(daemon_only.len(), 1);
        assert!(daemon_only.iter().all(|r| r.runner == Runner::Daemon));

        let client_only = filter_rows(&rows, RunnerFilter::Client);
        assert_eq!(client_only.len(), 2);
        assert!(client_only.iter().all(|r| r.runner == Runner::Client));
    }

    #[test]
    fn builtin_row_has_builtin_kind_and_client_runner() {
        let rows = server_rows_with_builtins(&[], &[], &[bv("fileio", 7, None)]);
        assert_eq!(rows.len(), 1);

        let row = &rows[0];
        assert_eq!(row.name, "fileio");
        assert_eq!(row.runner, Runner::Client);
        assert_eq!(row.kind, ServerKind::BuiltIn);
        assert_eq!(row.tool_count, 7);
        // An un-shadowed built-in is active: no disabled reason.
        assert_eq!(row.disabled_reason, None);
    }

    #[test]
    fn overridden_builtin_row_is_disabled_with_reason() {
        let rows = server_rows_with_builtins(&[], &[], &[bv("fileio", 7, Some("fileio-client"))]);
        assert_eq!(rows.len(), 1);

        let row = &rows[0];
        assert_eq!(row.kind, ServerKind::BuiltIn);
        assert_eq!(row.runner, Runner::Client);
        assert_eq!(
            row.disabled_reason,
            Some("overridden by the external \"fileio-client\"".to_string())
        );
    }

    #[test]
    fn rows_sort_stable_with_builtins() {
        // A mix of daemon, external-client, and built-in servers, including a
        // built-in shadowed by an external client server of the same name.
        let daemon = vec![
            dv("Zeta", "stdio", "running", 1),
            dv("git", "http", "running", 2),
        ];
        let client = vec![cv("fileio", "stdio", "running", 4)];
        let builtins = vec![bv("fileio", 3, Some("fileio")), bv("alpha", 2, None)];

        let rows = server_rows_with_builtins(&daemon, &client, &builtins);

        // Case-insensitive name order; daemon-before-client on ties; a shadowed
        // built-in slots directly after its active external override.
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "fileio", "fileio", "git", "Zeta"]);

        // The "alpha" row is the active built-in.
        assert_eq!(rows[0].kind, ServerKind::BuiltIn);
        assert_eq!(rows[0].disabled_reason, None);

        // The two "fileio" rows: the external client override first (active),
        // then the shadowed built-in (disabled with a reason).
        assert_eq!(rows[1].runner, Runner::Client);
        assert_eq!(rows[1].kind, ServerKind::Stdio);
        assert_eq!(rows[1].disabled_reason, None);

        assert_eq!(rows[2].runner, Runner::Client);
        assert_eq!(rows[2].kind, ServerKind::BuiltIn);
        assert_eq!(
            rows[2].disabled_reason,
            Some("overridden by the external \"fileio\"".to_string())
        );

        // Daemon rows keep their transport-derived kinds.
        assert_eq!(rows[3].kind, ServerKind::Http); // git
        assert_eq!(rows[4].kind, ServerKind::Stdio); // Zeta
    }

    #[test]
    fn server_rows_2arg_equals_with_empty_builtins() {
        // The 2-arg form is a thin wrapper: for the no-builtins case it returns
        // exactly what the built-in-aware form does with an empty slice, so
        // existing callers see unchanged behavior.
        let daemon = vec![
            dv("Zeta", "stdio", "running", 1),
            dv("git", "http", "running", 2),
        ];
        let client = vec![cv("browser", "stdio", "running", 3)];

        assert_eq!(
            server_rows(&daemon, &client),
            server_rows_with_builtins(&daemon, &client, &[])
        );
    }

    #[test]
    fn kind_label_names_each_kind_and_matches_transport_chip() {
        assert_eq!(kind_label(ServerKind::Stdio), "stdio");
        assert_eq!(kind_label(ServerKind::Http), "http");
        assert_eq!(kind_label(ServerKind::BuiltIn), "built-in");

        // For external kinds, the unified chip agrees with the legacy one.
        assert_eq!(kind_label(ServerKind::Stdio), transport_chip("stdio"));
        assert_eq!(kind_label(ServerKind::Http), transport_chip("http"));
    }
}
