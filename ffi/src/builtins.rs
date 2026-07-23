//! Compiled-in ("built-in") MCP servers hosted in-process (da#538).
//!
//! The native-core analog of `adele-gtk`'s and `adele-tui`'s `builtins.rs`:
//! each enabled `mcp-*` feature compiles one MCP server lib into this cdylib and
//! hosts it in-process, so a client is useful with no `client-mcp.toml` at all.
//!
//! **Default-OFF, unlike gtk/tui.** This cdylib is linked by BOTH adele-kde and
//! adele-mac. KDE's canonical build must stay byte-identical, so `default = []`
//! in `Cargo.toml` and a plain `cargo build -p client-ui-ffi` links none of these
//! crates and returns an empty set here — the engine then hosts exactly what
//! `client-mcp.toml` configures, as it did before this module existed.
//! adele-mac opts in via `just build-with-mcp …`.
//!
//! An external `client-mcp.toml` server of the SAME NAME overrides (suppresses)
//! the built-in — external > built-in. That decision is owned centrally by
//! [`McpHost::start_with_disabled`], which skips + logs a shadowed built-in and
//! reports it via `McpHost::builtin_status`; this module just enumerates the
//! full compiled-in set.
//!
//! [`McpHost::start_with_disabled`]: desktop_assistant_client_common::mcp_host::McpHost::start_with_disabled

use desktop_assistant_client_common::mcp_host::BuiltinServer;
#[cfg(any(
    feature = "mcp-fileio",
    feature = "mcp-terminal",
    feature = "mcp-tasks",
    feature = "mcp-web",
    feature = "mcp-weather",
    feature = "mcp-internet-radio",
    feature = "mcp-openstreetmap",
    feature = "mcp-geocode",
    feature = "mcp-skills"
))]
use std::sync::Arc;

/// Build every enabled built-in server as the full compiled-in set.
///
/// Returns the COMPLETE set; the override (skipping a built-in whose name a
/// configured `client-mcp.toml` server already provides) and the per-surface
/// disable list are applied by [`McpHost::start_with_disabled`], not here.
///
/// Each `#[cfg]` block compiles in only when its feature is on, so the default
/// (feature-less) build returns an empty `Vec` and hosts nothing. The infallible
/// constructors (fileio, web, and all five broad-set servers) are always
/// registered; the fallible ones (terminal, tasks) are logged and skipped if
/// their zero-config constructor fails, so a broken environment degrades to the
/// remaining tools rather than losing the whole set.
///
/// [`McpHost::start_with_disabled`]: desktop_assistant_client_common::mcp_host::McpHost::start_with_disabled
pub fn builtin_servers() -> Vec<BuiltinServer> {
    #[allow(unused_mut)]
    let mut out: Vec<BuiltinServer> = Vec::new();

    #[cfg(feature = "mcp-fileio")]
    out.push(BuiltinServer::new(
        "fileio",
        "fileio",
        Arc::new(fileio_mcp::build_service()),
    ));
    #[cfg(feature = "mcp-terminal")]
    match terminal_mcp::build_service() {
        Ok(svc) => out.push(BuiltinServer::new("terminal", "terminal", Arc::new(svc))),
        Err(e) => tracing::warn!("built-in terminal server unavailable: {e}"),
    }
    #[cfg(feature = "mcp-tasks")]
    match tasks_mcp::build_service() {
        Ok(svc) => out.push(BuiltinServer::new("tasks", "tasks", Arc::new(svc))),
        Err(e) => tracing::warn!("built-in tasks server unavailable: {e}"),
    }
    #[cfg(feature = "mcp-web")]
    out.push(BuiltinServer::new(
        "web",
        "web",
        Arc::new(web_mcp::build_service()),
    ));

    // The opt-in "broad set" (default-off; see the `builtin-extras` feature).
    // Each is hosted under the SAME namespace its standalone fleet binary uses,
    // so a tool's fully qualified name is identical whether the server is
    // compiled-in here or run externally.
    #[cfg(feature = "mcp-weather")]
    out.push(BuiltinServer::new(
        "weather-forecast",
        "weather-forecast",
        Arc::new(weather_forecast_mcp::build_service()),
    ));
    #[cfg(feature = "mcp-internet-radio")]
    out.push(BuiltinServer::new(
        "internet-radio",
        "internet-radio",
        Arc::new(internet_radio_mcp::build_service()),
    ));
    #[cfg(feature = "mcp-openstreetmap")]
    out.push(BuiltinServer::new(
        "openstreetmap",
        "openstreetmap",
        Arc::new(openstreetmap_mcp::build_service()),
    ));
    #[cfg(feature = "mcp-geocode")]
    out.push(BuiltinServer::new(
        "geocode",
        "geocode",
        Arc::new(geocode_mcp::build_service()),
    ));
    #[cfg(feature = "mcp-skills")]
    out.push(BuiltinServer::new(
        "skills",
        "skills",
        Arc::new(skills_mcp::build_service()),
    ));

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The KDE-preserving default: with no `mcp-*` feature selected, nothing is
    /// compiled in, so the set is empty and the engine's host decision collapses
    /// to "whatever `client-mcp.toml` says" — byte-identical to pre-da#538
    /// behavior. This is the guarantee that lets adele-mac opt in without
    /// changing adele-kde.
    #[cfg(not(any(
        feature = "mcp-fileio",
        feature = "mcp-terminal",
        feature = "mcp-tasks",
        feature = "mcp-web",
        feature = "mcp-weather",
        feature = "mcp-internet-radio",
        feature = "mcp-openstreetmap",
        feature = "mcp-geocode",
        feature = "mcp-skills"
    )))]
    #[test]
    fn default_build_compiles_in_no_builtins() {
        assert!(
            builtin_servers().is_empty(),
            "a feature-less build must link no built-in MCP servers, so adele-kde's \
             default build is unchanged"
        );
    }

    /// fileio's constructor is infallible, so a `builtin-core` build
    /// deterministically contains a server named "fileio" under the "fileio"
    /// namespace. The override lives in `McpHost::start_with_disabled`, so
    /// `builtin_servers()` always returns the full set.
    #[cfg(feature = "mcp-fileio")]
    #[test]
    fn fileio_builtin_present_and_namespaced_in_full_set() {
        let servers = builtin_servers();
        let fileio = servers
            .iter()
            .find(|s| s.name == "fileio")
            .expect("fileio built-in must be present in the compiled set");
        assert_eq!(
            fileio.namespace, "fileio",
            "fileio built-in must be advertised under the 'fileio' namespace"
        );
    }

    /// Every compiled-in server must claim a UNIQUE namespace. A duplicate would
    /// make one server's tools unreachable (the host routes by namespaced name),
    /// and it is the exact mistake a copy-paste of the blocks above invites.
    #[test]
    fn builtin_namespaces_are_unique() {
        let servers = builtin_servers();
        let mut seen: Vec<&str> = Vec::new();
        for s in &servers {
            assert!(
                !seen.contains(&s.namespace.as_str()),
                "duplicate built-in namespace '{}' — its tools would shadow another server's",
                s.namespace
            );
            seen.push(&s.namespace);
        }
    }

    /// Compiled with `builtin-extras`, all five broad-set servers are hosted
    /// under the SAME namespace their standalone fleet binaries use, so a tool's
    /// fully qualified name does not change when a server moves between
    /// compiled-in and external hosting. All five have infallible
    /// `build_service()`, so each is deterministically present.
    #[cfg(all(
        feature = "mcp-weather",
        feature = "mcp-internet-radio",
        feature = "mcp-openstreetmap",
        feature = "mcp-geocode",
        feature = "mcp-skills"
    ))]
    #[test]
    fn broad_set_builtins_present_when_extras_enabled() {
        let servers = builtin_servers();
        for ns in [
            "weather-forecast",
            "internet-radio",
            "openstreetmap",
            "geocode",
            "skills",
        ] {
            let server = servers
                .iter()
                .find(|s| s.namespace == ns)
                .unwrap_or_else(|| {
                    panic!("broad-set built-in '{ns}' must be present with builtin-extras")
                });
            assert_eq!(
                server.name, ns,
                "a broad-set built-in uses its fleet name for both name and namespace"
            );
        }
    }

    /// The core-4 are exactly what `builtin-core` promises. `terminal` and
    /// `tasks` have fallible constructors and may legitimately be absent in a
    /// hostile environment, so this asserts the two infallible ones are present
    /// and that nothing outside the core set sneaks in under that feature alone.
    #[cfg(all(
        feature = "mcp-fileio",
        feature = "mcp-web",
        not(feature = "builtin-extras")
    ))]
    #[test]
    fn core_set_contains_only_core_servers() {
        let servers = builtin_servers();
        for ns in ["fileio", "web"] {
            assert!(
                servers.iter().any(|s| s.namespace == ns),
                "core built-in '{ns}' must be present with builtin-core"
            );
        }
        for s in &servers {
            assert!(
                ["fileio", "terminal", "tasks", "web"].contains(&s.namespace.as_str()),
                "'{}' is not a core-set server but was compiled in without builtin-extras",
                s.namespace
            );
        }
    }
}
