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
    let _ = (runner, is_remote, host);
    todo!()
}

/// Honest transport chip text: `"stdio"` or `"http"`. Never emits the retired
/// "local"/"remote" conflation. Anything that is not `"http"` is treated as
/// `"stdio"` (the daemon only ever reports those two values).
pub fn transport_chip(transport: &str) -> &'static str {
    let _ = transport;
    todo!()
}

/// Merge daemon-run and client-run servers into one panel-ordered list.
///
/// Daemon items are tagged [`Runner::Daemon`] and client items
/// [`Runner::Client`]. The result is sorted alphabetically by name
/// (case-insensitive), with the [`Runner`] as a stable tiebreak so equal names
/// order deterministically.
pub fn server_rows(daemon: &[McpServerView], client: &[ClientServerDto]) -> Vec<ServerRow> {
    let _ = (daemon, client);
    todo!()
}

/// Apply a [`RunnerFilter`] to already-built rows, preserving their order.
pub fn filter_rows(rows: &[ServerRow], filter: RunnerFilter) -> Vec<ServerRow> {
    let _ = (rows, filter);
    todo!()
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

    #[test]
    fn runner_enum_and_serverrow_construct() {
        let row = ServerRow {
            name: "fileio".to_string(),
            runner: Runner::Daemon,
            transport: "stdio".to_string(),
            status: "running".to_string(),
            tool_count: 5,
            detail: None,
        };
        assert_eq!(row.runner, Runner::Daemon);
        assert_ne!(Runner::Daemon, Runner::Client);
        assert_eq!(row.name, "fileio");
        assert_eq!(row.tool_count, 5);
        assert_eq!(row.transport, "stdio");
        assert_eq!(row.detail, None);

        // Runner is Copy: using it after a bind does not move it.
        let r = Runner::Client;
        let r2 = r;
        assert_eq!(r, r2);
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

        assert_eq!(beta.runner, Runner::Client);
        assert_eq!(beta.transport, "http");
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
    }

    #[test]
    fn filter_rows_all_daemon_client() {
        let daemon = vec![dv("alpha", "stdio", "running", 1)];
        let client = vec![cv("beta", "http", "running", 1)];
        let rows = server_rows(&daemon, &client);

        assert_eq!(RunnerFilter::default(), RunnerFilter::All);
        assert_eq!(filter_rows(&rows, RunnerFilter::All).len(), 2);

        let daemon_only = filter_rows(&rows, RunnerFilter::Daemon);
        assert_eq!(daemon_only.len(), 1);
        assert!(daemon_only.iter().all(|r| r.runner == Runner::Daemon));

        let client_only = filter_rows(&rows, RunnerFilter::Client);
        assert_eq!(client_only.len(), 1);
        assert!(client_only.iter().all(|r| r.runner == Runner::Client));
    }
}
