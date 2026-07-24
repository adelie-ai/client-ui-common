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

use crate::view_event::{BUILTIN_KIND, BuiltinServerDto};
use desktop_assistant_client_common::mcp_host::{BuiltinServer, BuiltinStatus};
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

/// Project a running host's [`McpHost::builtin_status`] into the panel rows the
/// C side renders.
///
/// This is the authoritative source when a connection is up: the host reports
/// the tools it actually registered and the override/disable decisions it
/// actually made. Order is preserved — it is the order the built-ins were
/// passed, which is the panel's stable tiebreak for equal names.
///
/// [`McpHost::builtin_status`]: desktop_assistant_client_common::mcp_host::McpHost::builtin_status
pub fn builtin_dtos(status: Vec<BuiltinStatus>) -> Vec<BuiltinServerDto> {
    status
        .into_iter()
        .map(|s| BuiltinServerDto {
            name: s.name,
            namespace: s.namespace,
            kind: BUILTIN_KIND,
            // Saturate rather than wrap: a count that cannot fit is absurd, and a
            // wrapped one would render as a plausible lie.
            tool_count: u32::try_from(s.tool_count).unwrap_or(u32::MAX),
            overridden_by: s.overridden_by,
            disabled_by_config: s.disabled_by_config,
        })
        .collect()
}

/// Derive the same panel rows with **no running host**, from the compiled-in set
/// plus the caller's view of the client config.
///
/// Why this exists: the MCP host starts on connect, but the settings panel is
/// opened before and between connections. Without this the panel would show no
/// built-in rows while disconnected even though the servers are linked in. The
/// override + disable bookkeeping mirrors [`McpHost::start_with_disabled`]'s,
/// which owns it for the hosted case:
///
/// - `configured` — the names of the client-mcp servers **this surface hosts**
///   (i.e. `ClientMcpConfig::resolved_servers`). A same-name server shadows the
///   built-in, so it reports `overridden_by`.
/// - `disabled` — this surface's `disabled_builtins` list.
///
/// `tool_count` comes from the built-in itself, so the row shows a real number
/// rather than a placeholder zero.
///
/// [`McpHost::start_with_disabled`]: desktop_assistant_client_common::mcp_host::McpHost::start_with_disabled
pub fn compiled_builtin_dtos(configured: &[String], disabled: &[String]) -> Vec<BuiltinServerDto> {
    builtin_servers()
        .into_iter()
        .map(|builtin| {
            let overridden = configured.contains(&builtin.name);
            let disabled_by_config = disabled.contains(&builtin.name);
            BuiltinServerDto {
                kind: BUILTIN_KIND,
                tool_count: u32::try_from(builtin.service.tools().len()).unwrap_or(u32::MAX),
                overridden_by: overridden.then(|| builtin.name.clone()),
                disabled_by_config,
                name: builtin.name,
                namespace: builtin.namespace,
            }
        })
        .collect()
}

/// Re-derive each row's `disabled_by_config` from the surface's **current**
/// `disabled_builtins` list.
///
/// Why: a running host records the disabled set once, at start — built-ins are
/// fixed until the client relaunches — so a snapshot taken from
/// [`builtin_dtos`] reflects config-at-connect, not later edits. After a live
/// toggle the panel must show the *pending* state. This corrects only that flag;
/// the tool count and `overridden_by` describe the running host and are left
/// alone.
pub fn apply_disabled_overlay(dtos: &mut [BuiltinServerDto], disabled: &[String]) {
    for dto in dtos.iter_mut() {
        dto.disabled_by_config = disabled.contains(&dto.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_assistant_client_common::mcp_host::BuiltinStatus;

    /// A [`BuiltinStatus`] as a running [`McpHost`] would report it.
    ///
    /// [`McpHost`]: desktop_assistant_client_common::mcp_host::McpHost
    fn status(name: &str, overridden_by: Option<&str>, disabled_by_config: bool) -> BuiltinStatus {
        BuiltinStatus {
            name: name.to_string(),
            namespace: name.to_string(),
            tool_count: 7,
            overridden_by: overridden_by.map(str::to_string),
            disabled_by_config,
        }
    }

    // --- builtin_dtos: the live-host projection --------------------------------

    /// Every field the panel reads must survive the projection, including the
    /// two reason flags — dropping either would render a shadowed or opted-out
    /// built-in as active.
    #[test]
    fn builtin_dtos_carry_every_panel_field() {
        let dtos = builtin_dtos(vec![status("fileio", Some("fileio"), true)]);
        let dto = dtos.first().expect("one status in, one dto out");
        assert_eq!(dto.name, "fileio");
        assert_eq!(dto.namespace, "fileio");
        assert_eq!(dto.tool_count, 7);
        assert_eq!(dto.overridden_by.as_deref(), Some("fileio"));
        assert!(dto.disabled_by_config);
    }

    /// The kind is stamped by the core, not inferred by each client: a row that
    /// came through this path is a built-in by construction.
    #[test]
    fn builtin_dtos_stamp_the_built_in_kind() {
        let dtos = builtin_dtos(vec![status("web", None, false)]);
        assert_eq!(dtos[0].kind, BUILTIN_KIND);
    }

    /// Order is the panel's stable identity for equal names, so it must be the
    /// order the host reported (which is the order the built-ins were passed).
    #[test]
    fn builtin_dtos_preserve_host_order() {
        let dtos = builtin_dtos(vec![
            status("fileio", None, false),
            status("terminal", None, false),
            status("web", None, false),
        ]);
        let names: Vec<&str> = dtos.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["fileio", "terminal", "web"]);
    }

    // --- apply_disabled_overlay ------------------------------------------------

    /// A running host records the disabled set once, at start. After a live
    /// toggle the panel must show the *pending* state, so the flag is re-derived
    /// from the current config rather than trusted from the snapshot.
    #[test]
    fn overlay_flags_builtins_named_in_the_surface_disabled_set() {
        let mut dtos = builtin_dtos(vec![
            status("fileio", None, false),
            status("web", None, false),
        ]);
        apply_disabled_overlay(&mut dtos, &["web".to_string()]);
        assert!(!dtos[0].disabled_by_config, "fileio was not named");
        assert!(dtos[1].disabled_by_config, "web was named");
    }

    /// The overlay is authoritative in BOTH directions: re-enabling must clear a
    /// flag the snapshot still carries, or the row would stay dimmed forever.
    #[test]
    fn overlay_clears_a_flag_the_snapshot_still_carries() {
        let mut dtos = builtin_dtos(vec![status("web", None, true)]);
        apply_disabled_overlay(&mut dtos, &[]);
        assert!(!dtos[0].disabled_by_config);
    }

    /// The overlay corrects only the disable flag — the override and the tool
    /// count describe the *running* host and must not be invented from config.
    #[test]
    fn overlay_leaves_override_and_tool_count_alone() {
        let mut dtos = builtin_dtos(vec![status("web", Some("web"), false)]);
        apply_disabled_overlay(&mut dtos, &["web".to_string()]);
        assert!(dtos[0].disabled_by_config);
        assert_eq!(dtos[0].overridden_by.as_deref(), Some("web"));
        assert_eq!(dtos[0].tool_count, 7);
    }

    // --- compiled_builtin_dtos: the pre-connect derivation ---------------------

    /// The panel is opened before (and between) connections, when no host exists.
    /// A feature-less build must then report nothing — the same answer a running
    /// host would give — so the panel never implies built-ins that aren't linked.
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
    fn compiled_dtos_are_empty_without_a_single_mcp_feature() {
        assert!(compiled_builtin_dtos(&[], &[]).is_empty());
    }

    /// Without a host, the override bookkeeping `McpHost::start_with_disabled`
    /// would do has to be derived here — otherwise a shadowed built-in reads as
    /// active until the next connect.
    #[cfg(feature = "mcp-fileio")]
    #[test]
    fn compiled_dtos_flag_an_external_server_of_the_same_name() {
        let dtos = compiled_builtin_dtos(&["fileio".to_string()], &[]);
        let fileio = dtos
            .iter()
            .find(|d| d.name == "fileio")
            .expect("fileio is compiled in under this feature");
        assert_eq!(fileio.overridden_by.as_deref(), Some("fileio"));
    }

    /// A configured server with a *different* name shadows nothing — the match
    /// is by name, not "any external server exists".
    #[cfg(feature = "mcp-fileio")]
    #[test]
    fn compiled_dtos_ignore_an_unrelated_external_server() {
        let dtos = compiled_builtin_dtos(&["something-else".to_string()], &[]);
        let fileio = dtos
            .iter()
            .find(|d| d.name == "fileio")
            .expect("compiled in");
        assert!(fileio.overridden_by.is_none());
    }

    /// The per-surface opt-out must be visible with no host, too.
    #[cfg(feature = "mcp-fileio")]
    #[test]
    fn compiled_dtos_flag_a_config_disabled_builtin() {
        let dtos = compiled_builtin_dtos(&[], &["fileio".to_string()]);
        let fileio = dtos
            .iter()
            .find(|d| d.name == "fileio")
            .expect("compiled in");
        assert!(fileio.disabled_by_config);
        assert!(fileio.overridden_by.is_none(), "disable is not an override");
    }

    /// Tool counts come from the built-in itself, so a pre-connect panel shows a
    /// real number rather than a placeholder zero.
    #[cfg(feature = "mcp-fileio")]
    #[test]
    fn compiled_dtos_report_a_real_tool_count() {
        let dtos = compiled_builtin_dtos(&[], &[]);
        let fileio = dtos
            .iter()
            .find(|d| d.name == "fileio")
            .expect("compiled in");
        assert!(fileio.tool_count > 0, "fileio advertises tools");
        assert_eq!(fileio.namespace, "fileio");
        assert_eq!(fileio.kind, BUILTIN_KIND);
    }

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
