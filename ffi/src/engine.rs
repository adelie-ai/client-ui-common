//! The executor: a single actor that owns the reducer state + the transport.
//!
//! Everything funnels through one [`mpsc`] channel into one task that owns the
//! [`WindowState`] — so state is touched from exactly one place, with no locks
//! and no re-entrancy (the same single-threaded `apply` loop gtk/tui run). Each
//! input becomes effects via [`WindowState::apply`]; the actor splits them:
//!
//! - **view** effects → a [`ViewEvent`] → JSON → the C callback ([`ViewSink`]);
//! - **RPC** effects → an async task that runs the connector round-trip and
//!   feeds the result back as a [`UiMessage`] on the same channel.
//!
//! Daemon signals arrive on the same channel too (a pump maps each
//! [`SignalEvent`](desktop_assistant_api_model::SignalEvent) →
//! [`UiMessage`] via [`signal_to_ui_message`]), so live cross-client turns flow
//! through the identical path. A signal the reducer does not model is also
//! forwarded straight to the view by
//! [`view_event_for_signal`](crate::view_event::view_event_for_signal), since no
//! `Effect` would ever carry it. The actor never blocks: `apply` + `emit` +
//! `tokio::spawn` are all synchronous, so the loop returns to `recv` immediately.
//!
//! The reducer is transport-free (it carries no `Connector`): the actor owns the
//! connector directly, installs it on connect, and drops it on
//! [`Effect::ClearClient`].

use std::path::Path;
use std::sync::Arc;

use client_ui_common::{
    AdeleOutput, Effect, TurnOutcome, UiMessage, WindowState, interactive_default_from_purposes,
    signal_to_ui_message, voice_mode_client_tools,
};
use desktop_assistant_api_model as api;
use desktop_assistant_client_common::mcp_host::{
    ClientMcpConfig, McpHost, default_client_mcp_path, dispatch_client_tool_call,
    merge_registrations,
};
use desktop_assistant_client_common::{
    AssistantClient, ConnectionConfig, Connector, TransportMode,
};
use tokio::sync::mpsc;

use crate::client_mcp::ClientServerWrite;
use crate::view_event::{ClientServerDto, ViewEvent, view_event_for_signal};

/// The `client-mcp.toml` surface a core resolves MCP servers (and
/// `disabled_builtins`) under when the client never declares one.
///
/// This cdylib backs both adele-kde and adele-mac, and the surface is what lets
/// one machine-wide set of server *definitions* be enabled per client. It stays
/// `"kde"` here so adele-kde — which predates the setter and never calls it —
/// keeps reading `[surfaces.kde]` unchanged; every other client declares its own
/// via [`Intent::SetMcpSurface`].
pub const DEFAULT_MCP_SURFACE: &str = "kde";

/// The C function the core calls with each view-event JSON string.
///
/// `user_data` is the opaque pointer passed to `adele_core_new`; `json` is a
/// NUL-terminated UTF-8 string valid **only for the duration of the call** (copy
/// it). The callback fires on a core worker thread — the C++ side must marshal
/// to its UI thread (e.g. `QMetaObject::invokeMethod(..., Qt::QueuedConnection)`).
pub type ViewCallback =
    extern "C" fn(user_data: *mut std::ffi::c_void, json: *const std::ffi::c_char);

/// A thread-safe wrapper around the C callback + its `user_data`.
///
/// `user_data` is carried as `usize` (not a raw pointer) so the sink is `Send`;
/// the `unsafe impl`s assert the contract the C caller must uphold: the callback
/// is safe to invoke from any thread and `user_data` stays valid until
/// `adele_core_free`.
#[derive(Clone, Copy)]
pub struct ViewSink {
    callback: ViewCallback,
    user_data: usize,
}

// SAFETY: the C caller guarantees `callback` is thread-safe and `user_data`
// outlives the core; we never dereference `user_data` in Rust.
unsafe impl Send for ViewSink {}
unsafe impl Sync for ViewSink {}

impl ViewSink {
    pub fn new(callback: ViewCallback, user_data: usize) -> Self {
        Self {
            callback,
            user_data,
        }
    }

    /// Serialize `ev` and hand it to the C callback. Serialization or
    /// interior-NUL failures are logged and dropped — a malformed event must
    /// never panic across the FFI boundary.
    fn emit(&self, ev: &ViewEvent) {
        let json = match ev.to_json() {
            Ok(j) => j,
            Err(e) => {
                tracing::error!("failed to serialize view event: {e}");
                return;
            }
        };
        match std::ffi::CString::new(json) {
            Ok(c) => {
                // SAFETY: `c` lives until the end of this call; the C side copies.
                (self.callback)(self.user_data as *mut std::ffi::c_void, c.as_ptr());
            }
            Err(_) => tracing::error!("view event contained an interior NUL; dropped"),
        }
    }
}

/// A controller intent from the C side — a user action that isn't a 1:1
/// [`UiMessage`]. The actor translates each into reducer messages and/or RPCs.
pub enum Intent {
    /// Connect over `mode`. `address` is the UDS path / WS url (empty ⇒ default);
    /// ignored for D-Bus (session bus + service name from the environment).
    Connect {
        mode: TransportMode,
        address: String,
    },
    /// The user submitted `prompt` into the open conversation.
    SendPrompt(String),
    /// The user checked out queued message `index` to edit it (recall / a chip's
    /// edit affordance). It loads into the composer via a `composer_text` view
    /// event; re-submitting reinserts it in place.
    EditQueued(usize),
    /// The user removed queued message `index` (a chip's x) without sending it.
    RemoveQueued(usize),
    /// The user abandoned an in-progress queued-message edit (the checked-out
    /// message returns to the queue unchanged and the composer clears).
    CancelQueuedEdit,
    /// The user opened a conversation.
    SelectConversation(String),
    /// The user asked for a new conversation.
    NewConversation,
    /// The user deleted a conversation.
    DeleteConversation(String),
    /// The user changed the `You:` (voice input) setting for a conversation.
    SetVoiceIn {
        conversation_id: String,
        enabled: bool,
    },
    /// The user changed the `Adele:` (voice output) level for a conversation.
    SetAdeleOutput {
        conversation_id: String,
        level: AdeleOutput,
    },
    /// Stage (or clear) a per-message model override, applied to the next send.
    /// Empty `connection_id`/`model_id` clears it; `effort` is
    /// "low"/"medium"/"high" or empty. The reducer keeps model selection
    /// client-side, so the override lives in the actor, not `WindowState`.
    SelectModel {
        connection_id: String,
        model_id: String,
        effort: String,
    },
    /// Request cancellation of a background task by id.
    CancelTask(String),
    /// Fetch a background task's log page (delivered as a `TaskLogs` view event).
    FetchTaskLogs(String),
    /// Stage (or clear) an explicit WebSocket bearer token for the next connect.
    /// Empty ⇒ clear (fall back to the connector's own token minting). Lets a
    /// client with no local D-Bus token minter (e.g. macOS) supply a token it
    /// obtained out-of-band from the daemon's `/login`.
    SetWsJwt(String),
    /// Set whether basic device context (name, username, home dir, hostname,
    /// timezone, OS) is shared with the assistant on the next connect (#549).
    /// Mirrors the `ConnectionConfig::share_client_context` opt-out: the flag is
    /// staged on the actor and applied when `spawn_connect` builds the config, so
    /// a change takes effect on the next (re)connect. Default **on** — the core
    /// initializes it `true`, matching `ConnectionConfig::default()`; the KDE KCM
    /// checkbox flips it to opt out.
    SetShareClientContext(bool),
    /// Declare which `client-mcp.toml` surface this client resolves its MCP
    /// servers (and `disabled_builtins`) under, e.g. `"mac"`. Server definitions
    /// are machine-wide; the surface is the per-client enable layer, so each
    /// client must name its own or it silently adopts another's selection.
    /// Staged on the actor and read when the next connect starts the MCP host.
    /// An empty name is ignored, keeping [`DEFAULT_MCP_SURFACE`].
    SetMcpSurface(String),
    /// Ask for this client's compiled-in ("built-in") MCP servers and their
    /// status under the declared surface, delivered as a
    /// [`ViewEvent::McpBuiltins`]. Answerable with no connection: built-ins are a
    /// property of how this cdylib was built plus what `client-mcp.toml` says.
    RequestMcpBuiltins,
    /// Ask for this client's external client-run MCP servers (the
    /// `client-mcp.toml` servers the declared surface hosts on the edge) and
    /// their live status, delivered as a [`ViewEvent::McpClientServers`]. The
    /// sibling of [`RequestMcpBuiltins`]: answerable with no connection (the
    /// server list is a property of what `client-mcp.toml` says), with the live
    /// tool counts filling in once a connection has started the MCP host.
    RequestMcpClientServers,
    /// Turn one built-in on or off **for this client's surface**, by writing
    /// `[surfaces.<surface>].disabled_builtins` in the shared `client-mcp.toml`.
    ///
    /// The Rust side owns that file — every client surface on the machine reads
    /// it — so the write goes through here rather than through each client's own
    /// parser. Takes effect on the next connect (the running host is fixed at
    /// start); the refreshed [`ViewEvent::McpBuiltins`] that follows shows the
    /// pending state so the panel is honest in the meantime.
    SetMcpBuiltinDisabled { name: String, disabled: bool },
    /// Add, edit, enable or remove one **external client-run** MCP server in the
    /// shared `client-mcp.toml`, for this client's surface.
    ///
    /// The sibling of [`SetMcpBuiltinDisabled`], and for the same reason: that
    /// file is machine-wide, so the Rust side owns every write to it rather than
    /// each client parsing and rewriting it. Takes effect on the next connect
    /// (the running host is fixed at start); the refreshed
    /// [`ViewEvent::McpClientServers`] that follows shows the pending state, so
    /// the panel is honest in the meantime.
    WriteMcpClientServer(ClientServerWrite),
    /// Send an arbitrary management `api::Command` (serialized as JSON) over the
    /// connector; the `CommandResult` comes back as a `command_result` view event
    /// keyed by `request_id`. The generic channel for settings/management
    /// (connections, purposes, knowledge base) the typed effects don't cover.
    SendCommand {
        request_id: String,
        command_json: String,
    },
}

/// The actor's single input channel.
enum CoreMsg {
    Intent(Intent),
    /// A reducer message. Boxed because `UiMessage` is large and these are
    /// queued — keeps the channel slot small (clippy::large_enum_variant), the
    /// same "keep the enum small" posture the other clients take.
    Ui(Box<UiMessage>),
    /// The connect task hands the live connector to the actor to own.
    InstallConnector(Arc<Connector>),
    /// The connect task hands the freshly-started client-side MCP host to the
    /// actor to own (issue #464). Sent right after `InstallConnector` and before
    /// the signal pump starts, so any `ClientToolCall` the pump forwards finds
    /// the host already installed.
    InstallMcpHost(Arc<McpHost>),
    /// The connect task failed before producing a connector.
    ConnectFailed(String),
    /// A view event produced outside the reducer — the signal pump's direct
    /// forward (see [`view_event_for_signal`]). Routed through the actor rather
    /// than emitted from the pump so every callback into the C side still
    /// happens on the one actor task. Boxed for the same reason [`Self::Ui`] is:
    /// a `ViewEvent` is far larger than every other variant, and these are
    /// queued (clippy::large_enum_variant).
    EmitView(Box<ViewEvent>),
}

/// Wrap a reducer message as a (boxed) channel item.
fn ui(msg: UiMessage) -> CoreMsg {
    CoreMsg::Ui(Box::new(msg))
}

/// Parse a "low"/"medium"/"high" token into an [`api::EffortLevel`]; anything
/// else (including empty) ⇒ `None` (no effort hint).
fn parse_effort(s: &str) -> Option<api::EffortLevel> {
    match s {
        "low" => Some(api::EffortLevel::Low),
        "medium" => Some(api::EffortLevel::Medium),
        "high" => Some(api::EffortLevel::High),
        _ => None,
    }
}

/// Assemble the [`ConnectionConfig`] for a connect from the staged inputs.
///
/// Pulled out of [`Engine::spawn_connect`] so the config-assembly logic is
/// unit-testable across the FFI boundary without a live daemon or runtime: the
/// transport-specific address wiring, the WS-only staged bearer token, and the
/// `share_client_context` opt-out (#549) all land on the config here. Everything
/// else keeps [`ConnectionConfig::default()`].
fn build_connection_config(
    mode: TransportMode,
    address: &str,
    ws_jwt: Option<String>,
    share_client_context: bool,
) -> ConnectionConfig {
    let mut config = ConnectionConfig {
        transport_mode: mode,
        share_client_context,
        ..Default::default()
    };
    match mode {
        TransportMode::Uds if !address.is_empty() => {
            config.socket_path = Some(address.into());
        }
        TransportMode::Ws if !address.is_empty() => config.ws_url = address.to_string(),
        _ => {}
    }
    // An explicitly staged token short-circuits `resolve_ws_bearer_token` (no
    // D-Bus / `/login` round-trip) — the macOS path, where the token was fetched
    // out-of-band. Only meaningful for WS.
    if matches!(mode, TransportMode::Ws) && ws_jwt.is_some() {
        config.ws_jwt = ws_jwt;
    }
    config
}

/// Build the `mcp_builtins` view event for `surface`, reading the client MCP
/// config at `path`.
///
/// Two sources, one shape. When a host is running (`host` is `Some`) its
/// `builtin_status()` is authoritative — it reports the tools actually
/// registered and the decisions actually made. With no host the same rows are
/// derived from the compiled-in set plus the config, so the panel is answerable
/// before the first connect.
///
/// In both cases the disable flag is re-derived from the config as it is *now*:
/// the running host froze that decision at start, and a toggle made since must
/// show as pending rather than not at all.
///
/// Takes the path explicitly so the on-disk behavior is testable without
/// touching the developer's real `~/.config/adele/client-mcp.toml`.
fn mcp_builtins_event_at(path: &Path, host: Option<&McpHost>, surface: &str) -> ViewEvent {
    let cfg = ClientMcpConfig::load(path);
    let disabled = cfg.surface_disabled_builtins(surface);
    let mut servers = match host {
        Some(host) => crate::builtins::builtin_dtos(host.builtin_status()),
        None => {
            let configured: Vec<String> = cfg
                .resolved_servers(surface)
                .into_iter()
                .map(|s| s.name.clone())
                .collect();
            crate::builtins::compiled_builtin_dtos(&configured, disabled)
        }
    };
    crate::builtins::apply_disabled_overlay(&mut servers, disabled);
    ViewEvent::McpBuiltins {
        surface: surface.to_string(),
        servers,
    }
}

/// Build the `mcp_client_servers` view event for `surface`, reading the client
/// MCP config at `path`.
///
/// The sibling of [`mcp_builtins_event_at`], same "config + optional host"
/// shape. The server list is every server the machine *defines*, not only the
/// ones this surface hosts, because a panel that cannot see a switched-off
/// server can never switch it back on — and would report the machine as defining
/// nothing. Each row's transport is read straight from the definition (`http`
/// when an HTTP endpoint is configured, else `stdio`).
///
/// The status and tool count come from the surface's selection first, then the
/// host:
///
/// - **Not hosted here** — the definition is switched off, or this surface does
///   not list it: `disabled`, with a `0` tool count.
/// - **Hosted, no host running** (`host` is `None`): `enabled` — configured and
///   switched on, not yet started.
/// - **Hosted, host running**: a server whose namespace the host tallies is
///   `running` with its live tool count; one the host did NOT start — it failed
///   to launch or list its tools — is absent from the tally and reports `error`.
///
/// The order matters: a disabled server is absent from a running host's tally
/// too, so deciding it first is what keeps it from reporting as a failure.
///
/// The tool-count key is the server's namespace (`cfg.namespace`, or its name
/// when unset), matching [`McpHost::tool_counts`]'s key exactly.
///
/// Takes the path explicitly so the on-disk behavior is testable without
/// touching the developer's real `~/.config/adele/client-mcp.toml`.
///
/// [`McpHost::tool_counts`]: desktop_assistant_client_common::mcp_host::McpHost::tool_counts
fn mcp_client_servers_event_at(path: &Path, host: Option<&McpHost>, surface: &str) -> ViewEvent {
    let cfg = ClientMcpConfig::load(path);
    let counts = host.map(|h| h.tool_counts());
    let hosted: Vec<&str> = cfg
        .resolved_servers(surface)
        .into_iter()
        .map(|s| s.name.as_str())
        .collect();
    let servers = cfg
        .list_defined_servers()
        .iter()
        .map(|s| {
            let namespace_key = s.namespace.clone().unwrap_or_else(|| s.name.clone());
            let transport = if s.http.is_some() { "http" } else { "stdio" };
            // With a running host, a hosted server the host is serving reports
            // its live tool count; one the host never started is absent from the
            // tally and is surfaced as an error rather than a silent zero. A
            // server this surface does not host is absent from the tally for a
            // reason that is not failure, so it is decided before the tally.
            let (status, tool_count) = match &counts {
                _ if !hosted.contains(&s.name.as_str()) => ("disabled", 0),
                None => ("enabled", 0),
                Some(counts) => match counts.get(&namespace_key) {
                    // Saturate rather than wrap: a count that cannot fit is absurd.
                    Some(&n) => ("running", u32::try_from(n).unwrap_or(u32::MAX)),
                    None => ("error", 0),
                },
            };
            ClientServerDto {
                name: s.name.clone(),
                transport: transport.to_string(),
                status: status.to_string(),
                tool_count,
                namespace: s.namespace.clone(),
            }
        })
        .collect();
    ViewEvent::McpClientServers {
        surface: surface.to_string(),
        servers,
    }
}

/// Add or remove `name` in one surface's `disabled_builtins` list in the client
/// MCP config at `path`.
///
/// Fail-closed on a malformed file, via [`crate::client_mcp::load_strict`] —
/// which carries the reasoning, and which every edit to this shared file goes
/// through.
///
/// An empty `name` is refused: a blank entry is inert noise every other client
/// sharing the file would then carry.
fn write_builtin_disabled(
    path: &Path,
    surface: &str,
    name: &str,
    disabled: bool,
) -> Result<(), String> {
    if name.is_empty() {
        return Err("built-in server name must not be empty".to_string());
    }
    let mut cfg = crate::client_mcp::load_strict(path)?;
    cfg.set_builtin_disabled(surface, name, disabled);
    cfg.save(path)
}

/// The actor: owns the reducer state + the connector, runs effects.
struct Engine {
    state: WindowState,
    connector: Option<Arc<Connector>>,
    /// Client-side MCP host (issue #464): local MCP servers whose tools are
    /// advertised to the daemon as client-side tools and invoked here on a
    /// `ClientToolCall`. `None` until a connection starts one (only when the
    /// `kde` surface has servers configured); replaced on each connect and shut
    /// down on disconnect.
    mcp_host: Option<Arc<McpHost>>,
    self_tx: mpsc::UnboundedSender<CoreMsg>,
    sink: ViewSink,
    /// Per-message model override staged by `SelectModel`, applied on the next
    /// send. `None` ⇒ inherit the conversation / interactive-purpose default.
    staged_override: Option<api::SendPromptOverride>,
    /// Explicit WS bearer token staged by `SetWsJwt`, applied to the next WS
    /// connect. `None` ⇒ let the connector mint one (D-Bus / `/login`).
    ws_jwt: Option<String>,
    /// Whether to share basic device context with the assistant on connect
    /// (#549), staged by `SetShareClientContext` and applied in `spawn_connect`.
    /// `true` by default (matches `ConnectionConfig::default()`); the KDE KCM
    /// checkbox flips it to opt out.
    share_client_context: bool,
    /// The `client-mcp.toml` surface this core resolves under; see
    /// [`DEFAULT_MCP_SURFACE`].
    mcp_surface: String,
    /// The cancel handle last reported to the view, so
    /// [`report_turn_state`](Self::report_turn_state) can emit only on a change.
    ///
    /// It mirrors the reducer rather than owning anything: the reducer is the
    /// single source of truth, and this is one value of memory so an unchanged
    /// answer is not re-sent on every streaming chunk.
    active_task_id: Option<String>,
}

/// Build the `SubmitPrompt` message for a user send, minting a fresh per-send
/// idempotency key (#570). Pulled out of [`Engine::submit_prompt`] so the
/// key-minting contract is unit-testable without standing up a transport.
///
/// Why the host mints it: the reducer stays wasm-clean and never generates
/// UUIDs, so the native host supplies one. It stamps the optimistic bubble and
/// rides the `SendMessage` wire field so a dropped-connection retry re-attaches
/// to the live turn and the echoed `UserMessageAdded` dedupes by exact match.
/// (KDE's default D-Bus transport drops the key — idempotency is inert there
/// until a UDS/WS transport carries it; that is harmless.)
fn submit_prompt_message(text: String) -> UiMessage {
    UiMessage::SubmitPrompt {
        prompt: text,
        idempotency_key: Some(uuid::Uuid::new_v4().to_string()),
    }
}

impl Engine {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<CoreMsg>) {
        while let Some(msg) = rx.recv().await {
            match msg {
                CoreMsg::Intent(intent) => self.handle_intent(intent),
                CoreMsg::Ui(boxed) => self.dispatch(*boxed),
                CoreMsg::InstallConnector(conn) => self.connector = Some(conn),
                CoreMsg::EmitView(ev) => self.sink.emit(&ev),
                CoreMsg::InstallMcpHost(host) => {
                    // Shut down any prior host before adopting the new one.
                    self.shutdown_mcp_host();
                    self.mcp_host = Some(host);
                }
                CoreMsg::ConnectFailed(err) => {
                    self.sink.emit(&ViewEvent::ConnectError {
                        message: err.clone(),
                    });
                    self.sink.emit(&ViewEvent::Status {
                        text: format!("Connection failed: {err}"),
                    });
                    self.sink.emit(&ViewEvent::SendSensitive { value: false });
                }
            }
        }
    }

    /// Apply a reducer message and run the resulting effects.
    fn dispatch(&mut self, msg: UiMessage) {
        // Client-hosted MCP tools (issue #464). A `ClientToolCall` the local MCP
        // host serves is run off the actor loop and its result submitted via the
        // connector (the bridge's host path), skipping the reducer entirely — the
        // reducer only knows the built-in voice tools (`say_this` / `request_voice`
        // / `stop_voice`). Any tool the host does NOT serve (or when there is no
        // host / no connector) falls through to the reducer unchanged, so the
        // built-in tools still resolve. Host tools take precedence at dispatch,
        // matching `dispatch_client_tool_call` (and the tui/gtk wiring); the
        // namespaced host names never collide with the built-ins in practice.
        if let UiMessage::ClientToolCall { tool_name, .. } = &msg
            && let (Some(host), Some(conn)) = (&self.mcp_host, &self.connector)
            && host.handles(tool_name)
        {
            let host = Arc::clone(host);
            let conn = Arc::clone(conn);
            // Re-destructure by value now that we've decided to serve it; the
            // outer borrow of `msg` (via `tool_name`) has ended by its last use
            // above, so moving `msg` here is allowed.
            if let UiMessage::ClientToolCall {
                task_id,
                tool_call_id,
                tool_name,
                arguments,
                ..
            } = msg
            {
                tokio::spawn(async move {
                    // `dispatch_client_tool_call` always submits — even on error —
                    // so the daemon's parked turn resumes instead of timing out.
                    dispatch_client_tool_call(
                        &host,
                        &*conn,
                        &task_id,
                        &tool_call_id,
                        &tool_name,
                        arguments,
                    )
                    .await;
                });
            }
            return;
        }

        // Surface an explicit connection-up event in addition to the reducer's
        // own status/sensitivity effects, so the C++ side has a clean signal.
        if let UiMessage::Connected { label } = &msg {
            self.sink.emit(&ViewEvent::Connected {
                label: label.clone(),
            });
        }
        for effect in self.state.apply(msg) {
            self.run_effect(effect);
        }
        self.report_turn_state();
    }

    /// Carry the reducer's turn state across the C ABI.
    ///
    /// Two values a view needs and cannot compute: the handle that cancels the
    /// open turn, and the prompt of a turn that just failed. adele-gtk reads
    /// both off `WindowState` directly; a client on the far side of the ABI
    /// holds no `WindowState`, so the engine reports them as events.
    ///
    /// Called after the reducer applies a message, because both answers are
    /// derived from what that message did.
    fn report_turn_state(&mut self) {
        let active = self.state.active_task_id_for_view();
        // Only on a change. A view redrawing per event would otherwise be told
        // the same thing by every streamed chunk of a long reply.
        if active != self.active_task_id {
            self.active_task_id = active.clone();
            self.sink.emit(&ViewEvent::ActiveTurn { task_id: active });
        }
        // One-shot by construction: taking the offer clears it, so a stale
        // prompt cannot resurface at a later, unrelated moment.
        if let Some(text) = self.state.take_pending_retry_prompt() {
            self.sink.emit(&ViewEvent::RetryPrompt { text });
        }
    }

    fn handle_intent(&mut self, intent: Intent) {
        match intent {
            Intent::Connect { mode, address } => self.spawn_connect(mode, address),
            Intent::SendPrompt(text) => self.submit_prompt(text),
            Intent::EditQueued(index) => self.dispatch(UiMessage::EditQueued { index }),
            Intent::RemoveQueued(index) => self.dispatch(UiMessage::RemoveQueued { index }),
            Intent::CancelQueuedEdit => self.dispatch(UiMessage::CancelQueuedEdit),
            Intent::SelectConversation(id) => self.spawn_get_conversation(id, false),
            Intent::NewConversation => self.spawn_create_conversation(),
            Intent::DeleteConversation(id) => self.spawn_delete_conversation(id),
            Intent::SetVoiceIn {
                conversation_id,
                enabled,
            } => self.dispatch(UiMessage::SetVoiceIn {
                conversation_id,
                enabled,
            }),
            Intent::SetAdeleOutput {
                conversation_id,
                level,
            } => self.dispatch(UiMessage::SetAdeleOutput {
                conversation_id,
                level,
            }),
            Intent::SelectModel {
                connection_id,
                model_id,
                effort,
            } => self.set_model_override(connection_id, model_id, effort),
            Intent::CancelTask(id) => self.spawn_cancel_task(id),
            Intent::FetchTaskLogs(id) => self.spawn_fetch_task_logs(id),
            Intent::SetWsJwt(jwt) => self.ws_jwt = (!jwt.is_empty()).then_some(jwt),
            Intent::SetShareClientContext(enabled) => self.share_client_context = enabled,
            // Ignore an empty name rather than resolving `[surfaces.]`, which
            // would fall through to the `default` section and look like it worked.
            Intent::SetMcpSurface(surface) if !surface.is_empty() => self.mcp_surface = surface,
            Intent::SetMcpSurface(_) => {}
            Intent::RequestMcpBuiltins => self.spawn_emit_mcp_builtins(),
            Intent::RequestMcpClientServers => self.spawn_emit_mcp_client_servers(),
            Intent::SetMcpBuiltinDisabled { name, disabled } => {
                self.spawn_set_mcp_builtin_disabled(name, disabled)
            }
            Intent::WriteMcpClientServer(write) => self.spawn_write_mcp_client_server(write),
            Intent::SendCommand {
                request_id,
                command_json,
            } => self.spawn_send_command(request_id, command_json),
        }
    }

    /// Answer [`Intent::RequestMcpBuiltins`] with the current built-in inventory.
    ///
    /// Off the actor loop because it reads `client-mcp.toml` from disk (and, with
    /// no host, constructs the compiled-in services to count their tools).
    fn spawn_emit_mcp_builtins(&self) {
        let sink = self.sink;
        let surface = self.mcp_surface.clone();
        let host = self.mcp_host.clone();
        tokio::spawn(async move {
            sink.emit(&mcp_builtins_event_at(
                &default_client_mcp_path(),
                host.as_deref(),
                &surface,
            ));
        });
    }

    /// Answer [`Intent::RequestMcpClientServers`] with the current external
    /// client-run inventory.
    ///
    /// Off the actor loop because it reads `client-mcp.toml` from disk (and, when
    /// a host is running, tallies its live tool counts). The sibling of
    /// [`spawn_emit_mcp_builtins`](Self::spawn_emit_mcp_builtins).
    fn spawn_emit_mcp_client_servers(&self) {
        let sink = self.sink;
        let surface = self.mcp_surface.clone();
        let host = self.mcp_host.clone();
        tokio::spawn(async move {
            sink.emit(&mcp_client_servers_event_at(
                &default_client_mcp_path(),
                host.as_deref(),
                &surface,
            ));
        });
    }

    /// Answer [`Intent::SetMcpBuiltinDisabled`]: write this surface's opt-out,
    /// then re-emit the inventory so the panel resyncs.
    ///
    /// The refreshed inventory is emitted on failure too — the panel must fall
    /// back to the truth on disk rather than keep an optimistic toggle that never
    /// landed.
    fn spawn_set_mcp_builtin_disabled(&self, name: String, disabled: bool) {
        let sink = self.sink;
        let surface = self.mcp_surface.clone();
        let host = self.mcp_host.clone();
        tokio::spawn(async move {
            let path = default_client_mcp_path();
            if let Err(err) = write_builtin_disabled(&path, &surface, &name, disabled) {
                tracing::warn!("failed to update built-in '{name}' for surface '{surface}': {err}");
                sink.emit(&ViewEvent::Toast {
                    text: format!("Could not update built-in server: {err}"),
                });
            }
            sink.emit(&mcp_builtins_event_at(&path, host.as_deref(), &surface));
        });
    }

    /// Answer [`Intent::WriteMcpClientServer`]: apply one edit to the external
    /// client-run population, then re-emit the inventory so the panel resyncs.
    ///
    /// The refreshed inventory is emitted on failure too, for the same reason the
    /// built-in opt-out does it: the panel must fall back to the truth on disk
    /// rather than keep an optimistic row that never landed. A refused edit
    /// writes nothing, so the re-read is the state the file already had.
    fn spawn_write_mcp_client_server(&self, write: ClientServerWrite) {
        let sink = self.sink;
        let surface = self.mcp_surface.clone();
        let host = self.mcp_host.clone();
        tokio::spawn(async move {
            let path = default_client_mcp_path();
            if let Err(err) = write.apply(&path, &surface) {
                tracing::warn!("client MCP write failed for surface '{surface}': {err}");
                sink.emit(&ViewEvent::Toast {
                    text: format!("Could not update client MCP server: {err}"),
                });
            }
            sink.emit(&mcp_client_servers_event_at(
                &path,
                host.as_deref(),
                &surface,
            ));
        });
    }

    /// Send an arbitrary management command over the connector and emit its
    /// `CommandResult` (or an error) as a `command_result` view event keyed by
    /// `request_id`. The C side correlates the reply to its awaiting caller.
    fn spawn_send_command(&self, request_id: String, command_json: String) {
        let sink = self.sink;
        let connector = self.connector.clone();
        tokio::spawn(async move {
            let (ok, result, error) = match connector {
                None => (false, None, Some("not connected".to_string())),
                Some(conn) => match serde_json::from_str::<api::Command>(&command_json) {
                    Err(e) => (false, None, Some(format!("invalid command json: {e}"))),
                    Ok(command) => match conn.client().as_commands() {
                        None => (
                            false,
                            None,
                            Some("command channel unavailable on this transport".to_string()),
                        ),
                        Some(cmds) => match cmds.send_command(command).await {
                            Ok(res) => (true, serde_json::to_value(&res).ok(), None),
                            Err(e) => (false, None, Some(format!("{e}"))),
                        },
                    },
                },
            };
            sink.emit(&ViewEvent::CommandResult {
                request_id,
                ok,
                result,
                error,
            });
        });
    }

    /// Stage (or clear) the per-message model override applied to the next send.
    /// Empty connection/model clears it (inherit the conversation/purpose default).
    fn set_model_override(&mut self, connection_id: String, model_id: String, effort: String) {
        if connection_id.is_empty() || model_id.is_empty() {
            self.staged_override = None;
            return;
        }
        self.staged_override = Some(api::SendPromptOverride {
            connection_id,
            model_id,
            effort: parse_effort(&effort),
        });
    }

    /// Send-decision via the shared core. The optimistic user bubble is drawn
    /// where the `SendPrompt` effect is executed ([`run_rpc_effect`]), not here —
    /// because a send can now also originate from a queue *flush* (a burst of
    /// messages queued while a reply streamed, sent as one when it finishes),
    /// which arrives as a `StreamComplete`/`StreamError` reducer message, not as
    /// a `SubmitPrompt`. Drawing on the effect covers both paths with no
    /// double-render (the daemon's echoed `UserMessageAdded` is deduped by
    /// request_id, and a queued submit emits no `SendPrompt` at all).
    fn submit_prompt(&mut self, text: String) {
        self.dispatch(submit_prompt_message(text));
    }

    /// Run one effect: view effects emit; the connector-state + RPC effects are
    /// handled by the actor.
    fn run_effect(&mut self, effect: Effect) {
        // `ClearClient` both mutates actor state and notifies the view.
        if matches!(effect, Effect::ClearClient) {
            self.connector = None;
            // Tear down the client MCP host with the connection (issue #464): its
            // tools were advertised over this connection, and a later reconnect
            // (re-driven by KDE's D-Bus service watcher) starts a fresh one.
            self.shutdown_mcp_host();
            self.sink.emit(&ViewEvent::ClientCleared);
            return;
        }
        match ViewEvent::try_from_view_effect(effect) {
            Ok(ev) => self.sink.emit(&ev),
            Err(rpc) => self.run_rpc_effect(*rpc),
        }
    }

    /// Take and shut down the current client MCP host, if any (issue #464). Runs
    /// off the actor loop because the graceful shutdown kills each server child
    /// asynchronously. If an in-flight tool call still holds a handle, the sole
    /// owner isn't available for the graceful `shutdown(self)`; dropping the
    /// `Arc` there (and when that call finishes) still tears the children down —
    /// `McpClient` kills its child on drop — so the servers never leak.
    fn shutdown_mcp_host(&mut self) {
        if let Some(host) = self.mcp_host.take() {
            tokio::spawn(async move {
                match Arc::try_unwrap(host) {
                    Ok(host) => host.shutdown().await,
                    Err(_still_shared) => {
                        tracing::debug!(
                            "client MCP host still in use by an in-flight tool call; \
                             children will be reaped on drop"
                        );
                    }
                }
            });
        }
    }

    fn run_rpc_effect(&mut self, effect: Effect) {
        match effect {
            Effect::EnsureActiveConversation => self.ensure_active_conversation(),
            Effect::LoadConversation(id) => self.spawn_get_conversation(id, false),
            Effect::ReloadConversation(id) => self.spawn_get_conversation(id, true),
            Effect::RefetchConversationList => self.spawn_refetch_list(),
            Effect::SendPrompt {
                conversation_id,
                prompt,
                system_refinement,
                idempotency_key,
            } => {
                // Draw the optimistic user bubble for our own send (the reducer
                // pushed it into its transcript, but the KDE view is event-driven
                // and doesn't re-read state). Covers both a direct submit and a
                // queue flush; the daemon's echoed `UserMessageAdded` is deduped
                // by idempotency key (or request_id) so this never double-renders.
                self.sink.emit(&ViewEvent::AddUserMessage {
                    content: prompt.clone(),
                });
                self.spawn_send(conversation_id, prompt, system_refinement, idempotency_key);
            }
            Effect::SubscribeConversations(ids) => self.spawn_subscribe(ids),
            Effect::FetchScratchpad(id) => self.spawn_fetch_scratchpad(id),
            Effect::SubmitClientToolResult {
                task_id,
                tool_call_id,
                result,
            } => self.spawn_submit_tool_result(task_id, tool_call_id, result),
            // A turn ended. The reducer reports it so a host can close a
            // per-turn span; this engine keeps no spans, so it records the
            // correlation on one log line instead. That line is what an
            // operator greps to find a turn, so it is INFO, and it carries ids
            // and a flag only. The failure TEXT stays off it deliberately:
            // INFO never carries content, and that string is not guaranteed to
            // be free of it.
            Effect::TurnFinished {
                conversation_id,
                request_id,
                idempotency_key,
                outcome,
            } => {
                tracing::info!(
                    conversation_id,
                    request_id,
                    idempotency_key,
                    failed = matches!(outcome, TurnOutcome::Failed(_)),
                    "turn finished"
                );
            }
            // `try_from_view_effect` returns `Err` only for the RPC set above;
            // a brand-new effect variant would land here — assert in debug so a
            // future wiring gap is loud, and log (not panic) in release.
            other => {
                debug_assert!(false, "unhandled effect in executor: {other:?}");
                tracing::warn!("unhandled effect in executor: {other:?}");
            }
        }
    }

    /// Auto-open the most-recent conversation (or create one when the list is
    /// empty), mirroring gtk's `ensure_active_conversation`. A no-op when an
    /// active conversation is already set and still present.
    fn ensure_active_conversation(&mut self) {
        if let Some(active) = self.state.current_conversation_id.as_deref()
            && self.state.conversations.iter().any(|c| c.id == active)
        {
            return;
        }
        match self.state.conversations.first() {
            Some(conv) => {
                let id = conv.id.clone();
                self.spawn_get_conversation(id, false);
            }
            None => self.spawn_create_conversation(),
        }
    }

    // --- RPC spawns ------------------------------------------------------
    //
    // Each clones the connector Arc + the self-channel and runs off the actor
    // loop, feeding results back as `ui(..)`. A missing connector means
    // we're disconnected — the action is silently dropped (the reducer/UI gate
    // upstream), except `send`, which rolls its optimistic bubble back.

    fn spawn_connect(&self, mode: TransportMode, address: String) {
        let tx = self.self_tx.clone();
        let ws_jwt = self.ws_jwt.clone();
        let share_client_context = self.share_client_context;
        let mcp_surface = self.mcp_surface.clone();
        tokio::spawn(async move {
            let config = build_connection_config(mode, &address, ws_jwt, share_client_context);
            match Connector::connect(&config).await {
                Ok(conn) => {
                    let conn = Arc::new(conn);
                    let label = conn.label().to_string();
                    // Install in the actor FIRST so later effects find it.
                    let _ = tx.send(CoreMsg::InstallConnector(Arc::clone(&conn)));
                    // Start the client-side MCP host (issue #464) for the `kde`
                    // surface: run the locally-configured MCP servers on the edge
                    // and hold their tools to advertise (below) + route calls to.
                    // Selection comes from the shared, per-machine
                    // `~/.config/adele/client-mcp.toml`; an absent/empty config
                    // yields no host, degrading cleanly. Install it in the actor
                    // BEFORE the signal pump starts so a `ClientToolCall` always
                    // finds the host already in place (the channel is FIFO).
                    let mcp_cfg = ClientMcpConfig::load(&default_client_mcp_path());
                    let mcp_servers: Vec<_> = mcp_cfg
                        .resolved_servers(&mcp_surface)
                        .into_iter()
                        .cloned()
                        .collect();
                    // Compiled-in built-ins (da#538), hosted in-process with no
                    // subprocess. Empty unless this cdylib was built with an
                    // `mcp-*` feature — adele-kde's default build links none, so
                    // its behavior is unchanged; adele-mac opts in via
                    // `just build-with-mcp`. `start_with_disabled` centralizes
                    // both the override (a configured server of the same name
                    // shadows the built-in) and the per-surface
                    // `disabled_builtins` opt-out.
                    let mcp_builtins = crate::builtins::builtin_servers();
                    // Host if there is anything to host (configured OR built-in).
                    let mcp_host = if mcp_servers.is_empty() && mcp_builtins.is_empty() {
                        None
                    } else {
                        let host = Arc::new(
                            McpHost::start_with_disabled(
                                &mcp_servers,
                                mcp_builtins,
                                mcp_cfg.surface_disabled_builtins(&mcp_surface),
                            )
                            .await,
                        );
                        let _ = tx.send(CoreMsg::InstallMcpHost(Arc::clone(&host)));
                        Some(host)
                    };
                    // Pump signals -> messages. Holds only the receiver (never the
                    // Arc<Connector>), so dropping the actor's connector tears the
                    // connection down cleanly. A `ClientToolCall` is served by the
                    // MCP host in the actor's `dispatch` (which owns the host +
                    // connector), so the pump stays a pure signal->message map.
                    {
                        let mut rx = conn.subscribe();
                        let tx2 = tx.clone();
                        tokio::spawn(async move {
                            while let Some(sig) = rx.recv().await {
                                // A signal the reducer does not model (today only
                                // `KnowledgeChanged`) also goes straight to the
                                // view, since no `Effect` would ever carry it.
                                if let Some(ev) = view_event_for_signal(&sig)
                                    && tx2.send(CoreMsg::EmitView(Box::new(ev))).is_err()
                                {
                                    break;
                                }
                                if tx2.send(ui(signal_to_ui_message(sig))).is_err() {
                                    break;
                                }
                            }
                        });
                    }
                    // Initial loads (tui's subscribe_and_load + finish_connection_init).
                    match conn.client().list_conversations().await {
                        Ok(convs) => {
                            let _ = tx.send(ui(UiMessage::ConversationsLoaded(convs)));
                        }
                        Err(e) => {
                            let _ =
                                tx.send(ui(UiMessage::Error(format!("load conversations: {e}"))));
                        }
                    }
                    if let Some(cmds) = conn.client().as_commands() {
                        if let Ok(models) = cmds.list_available_models(None, false).await {
                            let _ = tx.send(ui(UiMessage::ModelsLoaded(models)));
                        }
                        if let Ok(api::CommandResult::Purposes(p)) =
                            cmds.send_command(api::Command::GetPurposes).await
                        {
                            let _ = tx.send(ui(UiMessage::DefaultModelLoaded(
                                interactive_default_from_purposes(&p),
                            )));
                        }
                        if let Ok(api::CommandResult::BackgroundTasks(tasks)) = cmds
                            .send_command(api::Command::ListBackgroundTasks {
                                include_finished: false,
                                limit: None,
                            })
                            .await
                        {
                            let _ = tx.send(ui(UiMessage::TasksLoaded(tasks)));
                        }
                    }
                    // Advertise this client's tools (best-effort; the daemon
                    // replaces its set per call, so send on every connect): the
                    // built-in voice-mode tools merged with the client-hosted MCP
                    // tools (issue #464), built-ins winning any name clash. This
                    // works over KDE's D-Bus transport — the daemon bridges client
                    // tools over D-Bus (desktop-assistant#320), so `as_commands()`
                    // is `Some` there and the registration is accepted.
                    let host_tools = mcp_host
                        .as_ref()
                        .map(|host| host.registrations())
                        .unwrap_or_default();
                    if let Err(e) = conn
                        .register_client_tools(merge_registrations(
                            voice_mode_client_tools(),
                            host_tools,
                        ))
                        .await
                    {
                        tracing::debug!("client tools not registered: {e}");
                    }
                    let _ = tx.send(ui(UiMessage::Connected { label }));
                }
                Err(e) => {
                    let _ = tx.send(CoreMsg::ConnectFailed(e.to_string()));
                }
            }
        });
    }

    fn spawn_get_conversation(&self, id: String, reload: bool) {
        let Some(conn) = self.connector.clone() else {
            return;
        };
        let tx = self.self_tx.clone();
        tokio::spawn(async move {
            match conn.client().get_conversation(&id).await {
                Ok(detail) => {
                    let msg = if reload {
                        UiMessage::ConversationReloaded(detail)
                    } else {
                        UiMessage::ConversationLoaded(detail)
                    };
                    let _ = tx.send(ui(msg));
                }
                Err(e) => {
                    let _ = tx.send(ui(UiMessage::Error(format!("load conversation: {e}"))));
                }
            }
        });
    }

    fn spawn_refetch_list(&self) {
        let Some(conn) = self.connector.clone() else {
            return;
        };
        let tx = self.self_tx.clone();
        tokio::spawn(async move {
            match conn.client().list_conversations().await {
                Ok(convs) => {
                    let _ = tx.send(ui(UiMessage::ConversationListRefetched(convs)));
                }
                Err(e) => tracing::warn!("refetch conversation list failed: {e}"),
            }
        });
    }

    fn spawn_send(
        &self,
        conversation_id: String,
        prompt: String,
        system_refinement: Option<String>,
        idempotency_key: Option<String>,
    ) {
        let Some(conn) = self.connector.clone() else {
            // No live connection: roll the optimistic bubble back out.
            let _ = self.self_tx.send(ui(UiMessage::SendFailed {
                conversation_id,
                prompt,
            }));
            let _ = self.self_tx.send(ui(UiMessage::Error(
                "Not connected — message not sent (your text is preserved)".to_string(),
            )));
            return;
        };
        let override_selection = self.staged_override.clone();
        let tx = self.self_tx.clone();
        // Kept for the ack: the send call consumes the key, and the reducer
        // needs it back to tie the turn to this send (#51).
        let echoed_key = idempotency_key.clone();
        tokio::spawn(async move {
            let refinement = system_refinement.as_deref().unwrap_or("");
            // Forward the client-minted idempotency key on the `SendMessage` wire
            // field (#570) via the `*_idempotent` send methods, so a retry after a
            // dropped connection re-attaches to the live turn and the echoed
            // `UserMessageAdded` dedupes by exact match. With a staged model
            // override, send via the generic Commands channel
            // (`send_prompt_idempotent` carries the override, refinement, AND
            // key); otherwise use the Connector's refinement+key send, which also
            // handles the no-Commands D-Bus prompt-fold fallback (which drops the
            // key — inert until a socket transport, as documented on `submit`).
            let result = if let Some(ov) = override_selection {
                if let Some(cmds) = conn.client().as_commands() {
                    cmds.send_prompt_idempotent(
                        &conversation_id,
                        &prompt,
                        Some(ov),
                        refinement.to_string(),
                        idempotency_key,
                    )
                    .await
                } else {
                    conn.send_prompt_with_system_refinement_idempotent(
                        &conversation_id,
                        &prompt,
                        refinement,
                        idempotency_key,
                    )
                    .await
                }
            } else {
                conn.send_prompt_with_system_refinement_idempotent(
                    &conversation_id,
                    &prompt,
                    refinement,
                    idempotency_key,
                )
                .await
            };
            match result {
                Ok(task_id) => {
                    let _ = tx.send(ui(UiMessage::PromptSent {
                        task_id,
                        conversation_id,
                        // Echo the key this send carried, so the reducer ties
                        // the turn to the send that started it rather than
                        // guessing which of several in flight it answers (#51).
                        idempotency_key: echoed_key,
                    }));
                }
                Err(e) => {
                    let _ = tx.send(ui(UiMessage::Error(format!(
                        "Send error: {e} (your text is preserved)"
                    ))));
                    let _ = tx.send(ui(UiMessage::SendFailed {
                        conversation_id,
                        prompt,
                    }));
                }
            }
        });
    }

    fn spawn_subscribe(&self, ids: Vec<String>) {
        let Some(conn) = self.connector.clone() else {
            return;
        };
        tokio::spawn(async move {
            if let Some(cmds) = conn.client().as_commands()
                && let Err(e) = cmds
                    .send_command(api::Command::SubscribeConversations {
                        conversation_ids: ids,
                    })
                    .await
            {
                tracing::warn!("SubscribeConversations failed: {e}");
            }
        });
    }

    fn spawn_cancel_task(&self, task_id: String) {
        let Some(conn) = self.connector.clone() else {
            return;
        };
        tokio::spawn(async move {
            if let Some(cmds) = conn.client().as_commands()
                && let Err(e) = cmds
                    .send_command(api::Command::CancelBackgroundTask { id: task_id })
                    .await
            {
                tracing::warn!("CancelBackgroundTask failed: {e}");
            }
        });
    }

    fn spawn_fetch_task_logs(&self, task_id: String) {
        let Some(conn) = self.connector.clone() else {
            return;
        };
        // The task-log page is a display-only fetch with no reducer state, so
        // emit it straight to the view (the sink is thread-safe) rather than
        // routing a new message through the reducer.
        let sink = self.sink;
        tokio::spawn(async move {
            let Some(cmds) = conn.client().as_commands() else {
                return;
            };
            match cmds
                .send_command(api::Command::GetBackgroundTaskLogs {
                    id: task_id.clone(),
                    after_seq: None,
                    limit: None,
                })
                .await
            {
                Ok(api::CommandResult::BackgroundTaskLogs { entries, .. }) => {
                    sink.emit(&ViewEvent::TaskLogs {
                        id: task_id,
                        entries,
                    });
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("GetBackgroundTaskLogs failed: {e}"),
            }
        });
    }

    fn spawn_fetch_scratchpad(&self, id: String) {
        let Some(conn) = self.connector.clone() else {
            return;
        };
        let tx = self.self_tx.clone();
        tokio::spawn(async move {
            if let Some(cmds) = conn.client().as_commands() {
                match cmds.get_conversation_scratchpad(&id, None).await {
                    Ok(notes) => {
                        let _ = tx.send(ui(UiMessage::ConversationScratchpadLoaded {
                            conversation_id: id,
                            notes,
                        }));
                    }
                    Err(e) => tracing::warn!("get_conversation_scratchpad failed: {e}"),
                }
            }
        });
    }

    fn spawn_submit_tool_result(
        &self,
        task_id: String,
        tool_call_id: String,
        result: Result<String, String>,
    ) {
        let Some(conn) = self.connector.clone() else {
            tracing::warn!("no connector to submit client-tool result for task {task_id}");
            return;
        };
        tokio::spawn(async move {
            if let Err(e) = conn
                .submit_client_tool_result(&task_id, &tool_call_id, result)
                .await
            {
                tracing::warn!("submit_client_tool_result failed: {e}");
            }
        });
    }

    fn spawn_create_conversation(&self) {
        let Some(conn) = self.connector.clone() else {
            return;
        };
        let tx = self.self_tx.clone();
        tokio::spawn(async move {
            match conn.create_conversation("New Conversation").await {
                Ok(id) => {
                    let _ = tx.send(ui(UiMessage::ConversationCreated { id: id.clone() }));
                    match conn.client().get_conversation(&id).await {
                        Ok(detail) => {
                            let _ = tx.send(ui(UiMessage::ConversationLoaded(detail)));
                        }
                        Err(e) => {
                            let _ = tx
                                .send(ui(UiMessage::Error(format!("load new conversation: {e}"))));
                        }
                    }
                    if let Ok(convs) = conn.client().list_conversations().await {
                        let _ = tx.send(ui(UiMessage::ConversationsLoaded(convs)));
                    }
                }
                Err(e) => {
                    let _ = tx.send(ui(UiMessage::Error(format!("create conversation: {e}"))));
                }
            }
        });
    }

    fn spawn_delete_conversation(&self, id: String) {
        let Some(conn) = self.connector.clone() else {
            return;
        };
        let tx = self.self_tx.clone();
        tokio::spawn(async move {
            match conn.client().delete_conversation(&id).await {
                Ok(()) => {
                    let _ = tx.send(ui(UiMessage::ConversationDeleted { id }));
                    if let Ok(convs) = conn.client().list_conversations().await {
                        let _ = tx.send(ui(UiMessage::ConversationsLoaded(convs)));
                    }
                }
                Err(e) => {
                    let _ = tx.send(ui(UiMessage::Error(format!("delete conversation: {e}"))));
                }
            }
        });
    }
}

/// The opaque handle the C side holds. Owns the tokio runtime (its drop shuts
/// the worker threads + the actor down) and the channel into the actor.
pub struct Core {
    // Held to keep the worker threads (and thus the actor) alive for the
    // handle's lifetime; dropped — and joined — when `adele_core_free` runs.
    _runtime: tokio::runtime::Runtime,
    tx: mpsc::UnboundedSender<CoreMsg>,
}

impl Core {
    /// Build the runtime, spawn the actor, and return the handle.
    pub fn new(sink: ViewSink) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build tokio runtime for the adele client core");
        let (tx, rx) = mpsc::unbounded_channel();
        let engine = Engine {
            state: WindowState::default(),
            connector: None,
            mcp_host: None,
            self_tx: tx.clone(),
            sink,
            staged_override: None,
            ws_jwt: None,
            // Default on, matching `ConnectionConfig::default()`; the KDE KCM
            // checkbox opts out via `SetShareClientContext(false)` (#549).
            share_client_context: true,
            mcp_surface: DEFAULT_MCP_SURFACE.to_string(),
            active_task_id: None,
        };
        runtime.spawn(engine.run(rx));
        Self {
            _runtime: runtime,
            tx,
        }
    }

    /// Queue a controller intent for the actor.
    pub fn send_intent(&self, intent: Intent) {
        let _ = self.tx.send(CoreMsg::Intent(intent));
    }
}

#[cfg(test)]
mod mcp_host_tests {
    //! Cover the engine-specific client-MCP-host wiring (issue #464): the `kde`
    //! surface selection and the exact tool set the engine advertises. The host
    //! orchestration + the `dispatch_client_tool_call` bridge are unit-tested in
    //! `client-common::mcp_host`; here we lock in the choices `spawn_connect` and
    //! `dispatch` make on top of them (surface name, voice+host merge, routing
    //! predicate) — the bits that would silently regress if reworded.
    use super::*;

    /// Minimal `/bin/sh` fake MCP server (one `echo` tool), mirroring the
    /// `mcp_host` unit tests: answers `initialize`, lists a single tool, and
    /// replies to `tools/call`. Enough for the host to namespace a tool so we can
    /// assert the advertise/route wiring.
    const FAKE_SERVER: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf %s "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"f","version":"0"}}}\n' "$id" ;;
    *'"method":"notifications/initialized"'*) : ;;
    *'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"d","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"ok"}]}}\n' "$id" ;;
  esac
done
"#;

    fn names(regs: &[api::ClientToolRegistration]) -> std::collections::HashSet<&str> {
        regs.iter().map(|r| r.name.as_str()).collect()
    }

    /// The engine resolves the `kde` surface from `client-mcp.toml`; a server
    /// enabled under `[surfaces.kde]` is hosted on the KDE client.
    #[test]
    fn kde_surface_resolves_configured_servers() {
        let cfg = ClientMcpConfig::from_toml(
            r#"
[[servers]]
name = "filesystem"
command = "fileio-mcp"
namespace = "fs"
[surfaces.kde]
enabled = ["filesystem"]
"#,
        )
        .unwrap();
        let resolved: Vec<&str> = cfg
            .resolved_servers("kde")
            .into_iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(resolved, vec!["filesystem"]);
    }

    /// A box that configures only `[surfaces.default]` still hosts it on KDE:
    /// `kde` has no entry of its own, so it inherits the default set.
    #[test]
    fn kde_surface_falls_back_to_default() {
        let cfg = ClientMcpConfig::from_toml(
            r#"
[[servers]]
name = "git"
command = "git-mcp"
namespace = "git"
[surfaces.default]
enabled = ["git"]
"#,
        )
        .unwrap();
        let resolved: Vec<&str> = cfg
            .resolved_servers("kde")
            .into_iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(resolved, vec!["git"]);
    }

    /// The full advertise chain `spawn_connect` runs: resolve the `kde` surface,
    /// start the host, and merge its tools with the built-in voice tools into the
    /// single set handed to `register_client_tools`. The routing predicate the
    /// actor's `dispatch` uses (`host.handles`) must claim the hosted tool and
    /// disown the built-ins, so each is served by the right path.
    #[tokio::test]
    async fn advertised_set_merges_voice_and_host_tools() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake.sh");
        std::fs::write(&script, FAKE_SERVER).unwrap();

        let cfg = ClientMcpConfig::from_toml(&format!(
            r#"
[[servers]]
name = "fake"
command = "/bin/sh"
args = ["{}"]
namespace = "ns"
[surfaces.kde]
enabled = ["fake"]
"#,
            script.display()
        ))
        .unwrap();
        let servers: Vec<_> = cfg.resolved_servers("kde").into_iter().cloned().collect();
        let host = McpHost::start(&servers).await;

        // Exactly the set spawn_connect advertises to the daemon.
        let advertised = merge_registrations(voice_mode_client_tools(), host.registrations());
        let advertised = names(&advertised);
        assert!(advertised.contains("request_voice"), "built-in voice tool");
        assert!(advertised.contains("stop_voice"), "built-in voice tool");
        assert!(advertised.contains("ns__echo"), "client-hosted MCP tool");

        // dispatch's routing gate: host tools go to the host, built-ins fall
        // through to the reducer.
        assert!(host.handles("ns__echo"));
        assert!(!host.handles("request_voice"));

        host.shutdown().await;
    }
}

#[cfg(test)]
mod share_context_tests {
    //! Cover the `share_client_context` opt-out wiring (#549): the intent stores
    //! the staged flag on the actor, `spawn_connect`'s config assembly carries it
    //! onto the `ConnectionConfig`, and the default is on. This is the KDE opt-out
    //! path — the KCM checkbox flips the flag through the C-ABI setter.
    use super::*;

    /// A no-op view sink: these tests never assert on emitted events, they only
    /// exercise the intent handler's field mutation.
    extern "C" fn noop_sink(_user_data: *mut std::ffi::c_void, _json: *const std::ffi::c_char) {}

    /// Build a bare engine for the field-storage assertions. No tokio runtime is
    /// needed because `SetShareClientContext` only mutates a field — it never
    /// spawns — so a plain `#[test]` suffices.
    fn test_engine() -> Engine {
        let (tx, _rx) = mpsc::unbounded_channel();
        Engine {
            state: WindowState::default(),
            connector: None,
            mcp_host: None,
            self_tx: tx,
            sink: ViewSink::new(noop_sink, 0),
            staged_override: None,
            ws_jwt: None,
            share_client_context: true,
            mcp_surface: DEFAULT_MCP_SURFACE.to_string(),
            active_task_id: None,
        }
    }

    /// A fresh engine resolves MCP config under `kde`, so adele-kde — which
    /// never calls the setter — keeps reading `[surfaces.kde]` exactly as before.
    #[test]
    fn mcp_surface_defaults_to_kde() {
        assert_eq!(
            test_engine().mcp_surface,
            DEFAULT_MCP_SURFACE,
            "the default surface must stay kde for back-compat"
        );
    }

    /// `SetMcpSurface` stages the client's own surface, so adele-mac resolves
    /// `[surfaces.mac]` instead of silently sharing KDE's server selection.
    #[test]
    fn intent_stores_the_client_surface() {
        let mut engine = test_engine();
        engine.handle_intent(Intent::SetMcpSurface("mac".into()));
        assert_eq!(engine.mcp_surface, "mac");
    }

    /// An empty surface is ignored rather than resolving `[surfaces.]` — a
    /// client that passes nothing keeps the default instead of silently
    /// falling through to the `default` section.
    #[test]
    fn empty_surface_is_ignored() {
        let mut engine = test_engine();
        engine.handle_intent(Intent::SetMcpSurface(String::new()));
        assert_eq!(engine.mcp_surface, DEFAULT_MCP_SURFACE);
    }

    /// The staged surface is what the connect path resolves servers under.
    #[test]
    fn staged_surface_selects_that_surfaces_servers() {
        let cfg = ClientMcpConfig::from_toml(
            r#"
[[servers]]
name = "mac-only"
command = "/bin/true"

[surfaces.kde]
enabled = []

[surfaces.mac]
enabled = ["mac-only"]
"#,
        )
        .expect("fixture parses");
        let mut engine = test_engine();
        engine.handle_intent(Intent::SetMcpSurface("mac".into()));
        let names: Vec<&str> = cfg
            .resolved_servers(&engine.mcp_surface)
            .into_iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["mac-only"], "must resolve the mac surface, not kde");
    }

    /// A fresh engine shares context by default, matching `ConnectionConfig::default()`.
    #[test]
    fn defaults_to_sharing_on() {
        assert!(
            test_engine().share_client_context,
            "sharing must default on"
        );
    }

    /// `SetShareClientContext(false)` stages the opt-out on the actor.
    #[test]
    fn intent_stores_opt_out() {
        let mut engine = test_engine();
        engine.handle_intent(Intent::SetShareClientContext(false));
        assert!(
            !engine.share_client_context,
            "the opt-out flag must be stored on the engine"
        );
    }

    /// ...and toggling back on restores sharing (the checkbox is re-checkable).
    #[test]
    fn intent_restores_sharing() {
        let mut engine = test_engine();
        engine.handle_intent(Intent::SetShareClientContext(false));
        engine.handle_intent(Intent::SetShareClientContext(true));
        assert!(
            engine.share_client_context,
            "sharing must be re-enablable after opting out"
        );
    }

    /// The staged flag reaches the `ConnectionConfig` that `spawn_connect` builds:
    /// off stays off, on stays on. Assembled via the same helper `spawn_connect`
    /// uses, so this locks the flag onto the actual connect path.
    #[test]
    fn flag_reaches_connection_config() {
        let off = build_connection_config(TransportMode::Ws, "", None, false);
        assert!(
            !off.share_client_context,
            "the opt-out must reach the ConnectionConfig"
        );
        let on = build_connection_config(TransportMode::Ws, "", None, true);
        assert!(
            on.share_client_context,
            "the opt-in must reach the ConnectionConfig"
        );
    }

    /// The extraction preserves the transport-specific wiring: a UDS address
    /// still lands as the socket path while the flag rides along.
    #[test]
    fn preserves_transport_wiring() {
        let cfg = build_connection_config(TransportMode::Uds, "/run/adele.sock", None, false);
        assert_eq!(
            cfg.socket_path.as_deref(),
            Some(std::path::Path::new("/run/adele.sock")),
            "UDS address must still map to the socket path"
        );
        assert!(!cfg.share_client_context);
    }
}

#[cfg(test)]
mod idempotency_key_tests {
    //! Cover the host-side per-send idempotency-key minting (#570): the native
    //! FFI host supplies a fresh v4 UUID per user send so a dropped-connection
    //! retry re-attaches to the live turn and the echoed `UserMessageAdded`
    //! dedupes by exact match. Mirrors the GTK host's
    //! `each_send_mints_a_distinct_idempotency_key` — the reducer stays
    //! wasm-clean and never mints keys.
    use super::*;

    fn key_of(msg: UiMessage) -> String {
        match msg {
            UiMessage::SubmitPrompt {
                idempotency_key, ..
            } => idempotency_key.expect("a user send must carry an idempotency key"),
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
    }

    /// A user send stamps a fresh v4 (random) UUID key.
    #[test]
    fn send_stamps_a_fresh_v4_idempotency_key() {
        let msg = submit_prompt_message("hello".to_string());
        match &msg {
            UiMessage::SubmitPrompt { prompt, .. } => assert_eq!(prompt, "hello"),
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
        let key = key_of(msg);
        let parsed = uuid::Uuid::parse_str(&key).expect("the idempotency key must be a valid UUID");
        assert_eq!(
            parsed.get_version(),
            Some(uuid::Version::Random),
            "the key must be a v4 (random) UUID"
        );
    }

    /// Two sends mint DISTINCT keys — retrying one turn never re-attaches to
    /// another. Each key also parses as a v4 UUID.
    #[test]
    fn each_send_mints_a_distinct_v4_key() {
        let first = key_of(submit_prompt_message("a".to_string()));
        let second = key_of(submit_prompt_message("b".to_string()));
        assert_ne!(first, second, "each send must get its own idempotency key");
        for key in [&first, &second] {
            let parsed =
                uuid::Uuid::parse_str(key).expect("each idempotency key must be a valid UUID");
            assert_eq!(
                parsed.get_version(),
                Some(uuid::Version::Random),
                "each key must be a v4 (random) UUID"
            );
        }
    }
}

#[cfg(test)]
mod mcp_builtin_tests {
    //! Cover the built-in MCP inventory read/write paths the panels drive
    //! (adele-mac#12): projecting the compiled-in set + the client's config into
    //! a `mcp_builtins` view event, and writing one built-in's per-surface
    //! opt-out back into `client-mcp.toml`.
    //!
    //! Both take an explicit path so the on-disk behavior is testable without
    //! touching the developer's real `~/.config/adele/client-mcp.toml`; the
    //! actor supplies `default_client_mcp_path()`.
    use super::*;
    use std::path::PathBuf;

    /// A temp dir plus the config path inside it. The dir guard must stay alive
    /// for the test, so it is returned alongside the path.
    fn temp_config(contents: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("client-mcp.toml");
        if let Some(contents) = contents {
            std::fs::write(&path, contents).expect("seed the config");
        }
        (dir, path)
    }

    fn reload(path: &std::path::Path) -> ClientMcpConfig {
        let contents = std::fs::read_to_string(path).expect("config was written");
        ClientMcpConfig::from_toml(&contents).expect("written config re-parses")
    }

    fn servers_of(ev: &ViewEvent) -> &[crate::view_event::BuiltinServerDto] {
        match ev {
            ViewEvent::McpBuiltins { servers, .. } => servers,
            other => panic!("expected an mcp_builtins event, got {other:?}"),
        }
    }

    fn surface_of(ev: &ViewEvent) -> &str {
        match ev {
            ViewEvent::McpBuiltins { surface, .. } => surface,
            other => panic!("expected an mcp_builtins event, got {other:?}"),
        }
    }

    // --- write path: the per-surface opt-out ----------------------------------

    /// The whole point of the `mac` surface: an opt-out written from the Mac's
    /// panel must land in `[surfaces.mac]` and leave every other client's
    /// section — they share this one file — untouched.
    #[test]
    fn disabling_a_builtin_writes_the_named_surface_only() {
        let (_dir, path) = temp_config(Some(
            r#"
[surfaces.kde]
enabled = []
disabled_builtins = ["terminal"]
"#,
        ));

        write_builtin_disabled(&path, "mac", "fileio", true).expect("write succeeds");

        let cfg = reload(&path);
        assert_eq!(cfg.surface_disabled_builtins("mac"), ["fileio".to_string()]);
        assert_eq!(
            cfg.surface_disabled_builtins("kde"),
            ["terminal".to_string()],
            "another surface's opt-outs must survive verbatim"
        );
    }

    /// Re-enabling is the same write in reverse: the name leaves the list, and
    /// the surface is left with an empty (not absent-by-accident) selection.
    #[test]
    fn re_enabling_a_builtin_removes_it_from_the_surface() {
        let (_dir, path) = temp_config(Some(
            r#"
[surfaces.mac]
enabled = []
disabled_builtins = ["fileio", "web"]
"#,
        ));

        write_builtin_disabled(&path, "mac", "fileio", false).expect("write succeeds");

        assert_eq!(
            reload(&path).surface_disabled_builtins("mac"),
            ["web".to_string()],
            "only the named built-in is re-enabled"
        );
    }

    /// Disabling twice must not duplicate the entry.
    #[test]
    fn disabling_a_builtin_twice_is_idempotent() {
        let (_dir, path) = temp_config(None);
        write_builtin_disabled(&path, "mac", "web", true).expect("first write");
        write_builtin_disabled(&path, "mac", "web", true).expect("second write");
        assert_eq!(
            reload(&path).surface_disabled_builtins("mac"),
            ["web".to_string()]
        );
    }

    /// `client-mcp.toml` is machine-wide and holds every surface's server
    /// definitions. A built-in toggle must rewrite it in place, never replace it
    /// — losing the `[[servers]]` block would break every other client.
    #[test]
    fn disabling_a_builtin_preserves_the_shared_server_definitions() {
        let (_dir, path) = temp_config(Some(
            r#"
[[servers]]
name = "notes"
command = "/usr/bin/notes-mcp"
namespace = "notes"

[surfaces.tui]
enabled = ["notes"]
"#,
        ));

        write_builtin_disabled(&path, "mac", "fileio", true).expect("write succeeds");

        let cfg = reload(&path);
        let defined: Vec<&str> = cfg
            .list_defined_servers()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(defined, ["notes"], "the shared definitions must survive");
        assert_eq!(cfg.surface_enabled_names("tui"), ["notes".to_string()]);
    }

    /// A surface with no section yet gets one materialized, rather than the
    /// opt-out silently falling into `[surfaces.default]`.
    #[test]
    fn disabling_a_builtin_materializes_a_missing_surface_section() {
        let (_dir, path) = temp_config(None);

        write_builtin_disabled(&path, "mac", "tasks", true).expect("write succeeds");

        let cfg = reload(&path);
        assert_eq!(cfg.surface_disabled_builtins("mac"), ["tasks".to_string()]);
        assert!(
            cfg.surface_disabled_builtins("default").is_empty(),
            "the inheritance fallback must not be edited as a side effect"
        );
    }

    /// Fail-closed on a malformed file. The tolerant loader the read path uses
    /// degrades an unparseable config to an EMPTY one; saving that back would
    /// erase every server definition on the machine, so the edit path must
    /// refuse instead — and leave the bytes exactly as they were.
    #[test]
    fn a_malformed_config_is_refused_rather_than_overwritten() {
        let original = "this is not = valid toml [[[";
        let (_dir, path) = temp_config(Some(original));

        let err = write_builtin_disabled(&path, "mac", "fileio", true)
            .expect_err("a malformed config must not be silently replaced");
        assert!(!err.is_empty(), "the failure must explain itself");
        assert_eq!(
            std::fs::read_to_string(&path).expect("file still there"),
            original,
            "the user's file must be left byte-identical"
        );
    }

    // --- read path: the mcp_builtins event ------------------------------------

    /// The event names the surface it resolved under, so a client can verify it
    /// is reading its own section rather than another client's.
    #[test]
    fn builtins_event_reports_the_surface_it_resolved() {
        let (_dir, path) = temp_config(None);
        let ev = mcp_builtins_event_at(&path, None, "mac");
        assert_eq!(surface_of(&ev), "mac");
    }

    /// A missing `client-mcp.toml` is the common case on a fresh machine: the
    /// read path must still answer (with the compiled-in set, nothing disabled)
    /// rather than error or hang the panel.
    #[test]
    fn builtins_event_survives_a_missing_config() {
        let (_dir, path) = temp_config(None);
        let ev = mcp_builtins_event_at(&path, None, "mac");
        for dto in servers_of(&ev) {
            assert!(!dto.disabled_by_config);
            assert!(dto.overridden_by.is_none());
        }
    }

    /// A core with no `mcp-*` feature — adele-kde's build — reports no built-ins
    /// at all, so its panel renders none.
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
    fn builtins_event_is_empty_on_a_default_featured_core() {
        let (_dir, path) = temp_config(None);
        assert!(servers_of(&mcp_builtins_event_at(&path, None, "mac")).is_empty());
    }

    /// The surface's own `disabled_builtins` list drives the flag — read back
    /// live, so a toggle is reflected before the next connect restarts the host.
    #[cfg(feature = "mcp-fileio")]
    #[test]
    fn builtins_event_flags_a_builtin_disabled_for_this_surface() {
        let (_dir, path) = temp_config(Some(
            r#"
[surfaces.mac]
enabled = []
disabled_builtins = ["fileio"]
"#,
        ));
        let ev = mcp_builtins_event_at(&path, None, "mac");
        let fileio = servers_of(&ev)
            .iter()
            .find(|d| d.name == "fileio")
            .expect("fileio is compiled in under this feature");
        assert!(fileio.disabled_by_config);
    }

    /// Another surface's opt-out must not dim this client's row.
    #[cfg(feature = "mcp-fileio")]
    #[test]
    fn builtins_event_ignores_another_surfaces_opt_out() {
        let (_dir, path) = temp_config(Some(
            r#"
[surfaces.kde]
enabled = []
disabled_builtins = ["fileio"]
"#,
        ));
        let ev = mcp_builtins_event_at(&path, None, "mac");
        let fileio = servers_of(&ev)
            .iter()
            .find(|d| d.name == "fileio")
            .expect("compiled in");
        assert!(!fileio.disabled_by_config);
    }

    /// External beats built-in: a configured client-mcp server of the same name
    /// this surface hosts shadows the built-in, and the row says which one.
    #[cfg(feature = "mcp-fileio")]
    #[test]
    fn builtins_event_marks_an_externally_overridden_builtin() {
        let (_dir, path) = temp_config(Some(
            r#"
[[servers]]
name = "fileio"
command = "/usr/bin/fileio-mcp"

[surfaces.mac]
enabled = ["fileio"]
"#,
        ));
        let ev = mcp_builtins_event_at(&path, None, "mac");
        let fileio = servers_of(&ev)
            .iter()
            .find(|d| d.name == "fileio")
            .expect("compiled in");
        assert_eq!(fileio.overridden_by.as_deref(), Some("fileio"));
    }

    /// A same-name server this surface does NOT enable hosts nothing, so it
    /// shadows nothing — the built-in stays active.
    #[cfg(feature = "mcp-fileio")]
    #[test]
    fn builtins_event_ignores_a_same_name_server_this_surface_does_not_host() {
        let (_dir, path) = temp_config(Some(
            r#"
[[servers]]
name = "fileio"
command = "/usr/bin/fileio-mcp"

[surfaces.kde]
enabled = ["fileio"]

[surfaces.mac]
enabled = []
"#,
        ));
        let ev = mcp_builtins_event_at(&path, None, "mac");
        let fileio = servers_of(&ev)
            .iter()
            .find(|d| d.name == "fileio")
            .expect("compiled in");
        assert!(fileio.overridden_by.is_none());
    }

    // --- intent wiring ---------------------------------------------------------

    /// An empty built-in name is refused rather than written: a blank entry in
    /// `disabled_builtins` is inert noise every other client sharing the file
    /// would then carry, and it can only come from a caller bug.
    #[test]
    fn an_empty_builtin_name_is_not_written() {
        let (_dir, path) = temp_config(None);
        write_builtin_disabled(&path, "mac", "", true).expect_err("an empty name is rejected");
        assert!(!path.exists(), "nothing should have been written");
    }
}

#[cfg(test)]
mod mcp_client_server_tests {
    //! Cover the external client-run MCP inventory read path the panel drives
    //! (adele-mac#3): projecting the surface's `client-mcp.toml` servers into a
    //! `mcp_client_servers` view event — the sibling of the built-in read path.
    //!
    //! Unlike built-ins, this population comes purely from config plus the live
    //! host, so it is feature-independent (no `mcp-*` gating): the tests configure
    //! external servers in TOML directly. The path is taken explicitly so the
    //! on-disk behavior is testable without touching the developer's real
    //! `~/.config/adele/client-mcp.toml`.
    use super::*;
    use std::path::PathBuf;

    /// A minimal `/bin/sh` fake MCP server (one `echo` tool): answers
    /// `initialize`, lists a single tool, and replies to `tools/call`. Enough for
    /// a real [`McpHost`] to start it and tally one tool.
    const FAKE_SERVER: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf %s "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"f","version":"0"}}}\n' "$id" ;;
    *'"method":"notifications/initialized"'*) : ;;
    *'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"d","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"ok"}]}}\n' "$id" ;;
  esac
done
"#;

    fn temp_config(contents: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("client-mcp.toml");
        if let Some(contents) = contents {
            std::fs::write(&path, contents).expect("seed the config");
        }
        (dir, path)
    }

    fn servers_of(ev: &ViewEvent) -> &[ClientServerDto] {
        match ev {
            ViewEvent::McpClientServers { servers, .. } => servers,
            other => panic!("expected an mcp_client_servers event, got {other:?}"),
        }
    }

    fn surface_of(ev: &ViewEvent) -> &str {
        match ev {
            ViewEvent::McpClientServers { surface, .. } => surface,
            other => panic!("expected an mcp_client_servers event, got {other:?}"),
        }
    }

    /// The event names the surface it resolved under, so a client can verify it
    /// is reading its own section rather than another client's.
    #[test]
    fn client_servers_event_reports_the_surface_it_resolved() {
        let (_dir, path) = temp_config(None);
        let ev = mcp_client_servers_event_at(&path, None, "mac");
        assert_eq!(surface_of(&ev), "mac");
    }

    /// A missing `client-mcp.toml`, or a surface that enables no external
    /// servers, answers with an empty list rather than erroring or hanging.
    #[test]
    fn client_servers_event_is_empty_with_no_config() {
        let (_dir, path) = temp_config(None);
        assert!(servers_of(&mcp_client_servers_event_at(&path, None, "mac")).is_empty());
    }

    /// The default core → empty client list (the acceptance spec's baseline): no
    /// external servers configured means no client rows, on any build.
    #[test]
    fn client_servers_event_lists_configured_servers_offline() {
        let (_dir, path) = temp_config(Some(
            r#"
[[servers]]
name = "browser"
command = "/usr/bin/browser-mcp"
namespace = "web"

[[servers]]
name = "remote"
namespace = "rem"
[servers.http]
url = "https://example.test/mcp"

[surfaces.mac]
enabled = ["browser", "remote"]
"#,
        ));
        let ev = mcp_client_servers_event_at(&path, None, "mac");
        let servers = servers_of(&ev);
        assert_eq!(servers.len(), 2);

        let browser = servers
            .iter()
            .find(|s| s.name == "browser")
            .expect("browser resolved for this surface");
        // Offline: listed as enabled, no live tools yet, stdio transport.
        assert_eq!(browser.status, "enabled");
        assert_eq!(browser.tool_count, 0);
        assert_eq!(browser.transport, "stdio");
        assert_eq!(browser.namespace.as_deref(), Some("web"));

        let remote = servers
            .iter()
            .find(|s| s.name == "remote")
            .expect("remote resolved for this surface");
        // An HTTP endpoint reports the http transport honestly, never a guess.
        assert_eq!(remote.transport, "http");
        assert_eq!(remote.status, "enabled");
        assert_eq!(remote.namespace.as_deref(), Some("rem"));
    }

    /// A server this surface does not enable is still listed, as `disabled`.
    ///
    /// The list is the *defined* set with each row's state, not the hosted set:
    /// a panel that could not see a switched-off server could never switch it
    /// back on, and would report the machine as defining nothing.
    #[test]
    fn client_servers_event_lists_a_server_this_surface_does_not_enable_as_disabled() {
        let (_dir, path) = temp_config(Some(
            r#"
[[servers]]
name = "browser"
command = "/usr/bin/browser-mcp"

[surfaces.kde]
enabled = ["browser"]

[surfaces.mac]
enabled = []
"#,
        ));
        let ev = mcp_client_servers_event_at(&path, None, "mac");
        let servers = servers_of(&ev);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "browser");
        assert_eq!(servers[0].status, "disabled");
        assert_eq!(servers[0].tool_count, 0);
    }

    /// A definition switched off at the definition level hosts nothing anywhere,
    /// so every surface sees it as `disabled` rather than as absent.
    #[test]
    fn client_servers_event_reports_a_disabled_definition_as_disabled() {
        let (_dir, path) = temp_config(Some(
            r#"
[[servers]]
name = "browser"
command = "/usr/bin/browser-mcp"
enabled = false

[surfaces.mac]
enabled = ["browser"]
"#,
        ));
        let ev = mcp_client_servers_event_at(&path, None, "mac");
        let servers = servers_of(&ev);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].status, "disabled");
    }

    /// A missing namespace travels as `None` so the client can fall back to the
    /// name itself — the same key the host tallies counts under.
    #[test]
    fn client_servers_event_carries_no_namespace_when_unset() {
        let (_dir, path) = temp_config(Some(
            r#"
[[servers]]
name = "browser"
command = "/usr/bin/browser-mcp"

[surfaces.mac]
enabled = ["browser"]
"#,
        ));
        let ev = mcp_client_servers_event_at(&path, None, "mac");
        let browser = &servers_of(&ev)[0];
        assert!(browser.namespace.is_none());
    }

    /// A running host fills in the live tool count and flips the status to
    /// `running`; a resolved server the host could not start reports `error`
    /// rather than a silent zero. Both are exercised in one host so the join is
    /// covered end to end.
    #[tokio::test]
    async fn client_servers_event_reflects_a_running_host() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake.sh");
        std::fs::write(&script, FAKE_SERVER).unwrap();
        let cfg_path = dir.path().join("client-mcp.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"
[[servers]]
name = "good"
command = "/bin/sh"
args = ["{}"]
namespace = "ns"

[[servers]]
name = "broken"
command = "/nonexistent/definitely-not-a-real-binary"
namespace = "broke"

[surfaces.mac]
enabled = ["good", "broken"]
"#,
                script.display()
            ),
        )
        .unwrap();

        let servers: Vec<_> = ClientMcpConfig::load(&cfg_path)
            .resolved_servers("mac")
            .into_iter()
            .cloned()
            .collect();
        let host = McpHost::start(&servers).await;

        let ev = mcp_client_servers_event_at(&cfg_path, Some(&host), "mac");
        let rows = servers_of(&ev);

        let good = rows.iter().find(|s| s.name == "good").expect("good listed");
        assert_eq!(good.status, "running", "a hosted server is running");
        assert_eq!(good.tool_count, 1, "its one echo tool is counted live");

        let broken = rows
            .iter()
            .find(|s| s.name == "broken")
            .expect("broken listed");
        assert_eq!(
            broken.status, "error",
            "a resolved server the host did not start is an error, not a silent zero"
        );
        assert_eq!(broken.tool_count, 0);

        host.shutdown().await;
    }

    /// A disabled server is absent from a running host's tally for a reason that
    /// has nothing to do with failure, so it must never be reported as `error`.
    /// The disabled case is decided before the tally is consulted.
    #[tokio::test]
    async fn client_servers_event_never_reports_a_disabled_server_as_error() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake.sh");
        std::fs::write(&script, FAKE_SERVER).unwrap();
        let cfg_path = dir.path().join("client-mcp.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"
[[servers]]
name = "good"
command = "/bin/sh"
args = ["{}"]
namespace = "ns"

[[servers]]
name = "parked"
command = "/usr/bin/parked-mcp"

[surfaces.mac]
enabled = ["good"]
"#,
                script.display()
            ),
        )
        .unwrap();

        let servers: Vec<_> = ClientMcpConfig::load(&cfg_path)
            .resolved_servers("mac")
            .into_iter()
            .cloned()
            .collect();
        let host = McpHost::start(&servers).await;

        let ev = mcp_client_servers_event_at(&cfg_path, Some(&host), "mac");
        let rows = servers_of(&ev);
        let parked = rows
            .iter()
            .find(|s| s.name == "parked")
            .expect("a disabled server is still listed while a host runs");
        assert_eq!(parked.status, "disabled");
        assert_eq!(parked.tool_count, 0);

        host.shutdown().await;
    }
}

#[cfg(test)]
mod turn_state_tests {
    //! Cover the turn-state view events: the cancel handle for the open turn,
    //! and the one-shot retry offer for a turn that failed (#58).
    //!
    //! The reducer computes both. adele-gtk reads them directly because it holds
    //! the `WindowState`; a client on the far side of the C ABI holds none and
    //! sees only `ViewEvent` JSON. So the engine drains both after every applied
    //! message.
    //!
    //! The assertions are over the emitted JSON, because the JSON is the
    //! contract a client codes against.
    use super::*;
    use std::ffi::CStr;
    use std::sync::{Mutex, OnceLock};

    /// Events the recording sink captured, oldest first.
    ///
    /// A `static` rather than a `user_data` pointer: the sink is an
    /// `extern "C" fn` that captures nothing, and `emitted` holds a lock for the
    /// whole of each case, so a shared buffer costs nothing in coverage and
    /// avoids threading a raw pointer through the ABI for no reason.
    fn recorded() -> &'static Mutex<Vec<String>> {
        static EVENTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
        EVENTS.get_or_init(|| Mutex::new(Vec::new()))
    }

    /// Serializes the cases, which share `recorded()`.
    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    extern "C" fn recording_sink(_user_data: *mut std::ffi::c_void, json: *const std::ffi::c_char) {
        // SAFETY: the sink is only ever called by `ViewSink::emit`, which passes
        // a NUL-terminated C string that outlives the call.
        let text = unsafe { CStr::from_ptr(json) }
            .to_string_lossy()
            .into_owned();
        recorded().lock().expect("event buffer poisoned").push(text);
    }

    /// Drive an engine through `messages` and return every event it emitted.
    fn emitted(messages: Vec<UiMessage>) -> Vec<serde_json::Value> {
        let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        recorded().lock().expect("event buffer poisoned").clear();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut engine = Engine {
            state: WindowState::default(),
            connector: None,
            mcp_host: None,
            self_tx: tx,
            sink: ViewSink::new(recording_sink, 0),
            staged_override: None,
            ws_jwt: None,
            share_client_context: true,
            mcp_surface: DEFAULT_MCP_SURFACE.to_string(),
            active_task_id: None,
        };
        for message in messages {
            engine.dispatch(message);
        }
        recorded()
            .lock()
            .expect("event buffer poisoned")
            .iter()
            .map(|json| serde_json::from_str(json).expect("every event must be valid JSON"))
            .collect()
    }

    /// Every `active_turn` event in order, read as its `task_id`.
    fn handles(events: &[serde_json::Value]) -> Vec<Option<String>> {
        events
            .iter()
            .filter(|e| e["type"] == "active_turn")
            .map(|e| e["task_id"].as_str().map(str::to_string))
            .collect()
    }

    /// Every `retry_prompt` event in order, read as its `text`.
    fn offers(events: &[serde_json::Value]) -> Vec<String> {
        events
            .iter()
            .filter(|e| e["type"] == "retry_prompt")
            .map(|e| e["text"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    fn open(id: &str) -> UiMessage {
        UiMessage::ConversationLoaded(api::client::ConversationDetail {
            id: id.to_string(),
            title: "t".to_string(),
            messages: Vec::new(),
            model_selection: None,
            conversation_personality: None,
            tool_gate_disabled: false,
        })
    }

    fn user_said(text: &str) -> UiMessage {
        UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "r1".to_string(),
            content: text.to_string(),
            idempotency_key: None,
        }
    }

    fn prompt_sent(task_id: &str) -> UiMessage {
        UiMessage::PromptSent {
            task_id: task_id.to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: None,
        }
    }

    fn stream_error() -> UiMessage {
        UiMessage::StreamError {
            request_id: "r1".to_string(),
            error: "timeout".to_string(),
        }
    }

    fn stream_complete() -> UiMessage {
        UiMessage::StreamComplete {
            request_id: "r1".to_string(),
            full_response: "done".to_string(),
        }
    }

    // --- the cancel handle ---

    /// A turn starting in the open conversation reports the id Cancel acts on,
    /// so a client can offer the control without holding reducer state.
    #[test]
    fn a_started_turn_reports_its_cancel_handle() {
        let events = emitted(vec![open("c1"), prompt_sent("task-42")]);
        assert_eq!(handles(&events), vec![Some("task-42".to_string())]);
    }

    /// A finished turn is not cancelable, so the handle is withdrawn.
    #[test]
    fn a_completed_turn_withdraws_its_cancel_handle() {
        let events = emitted(vec![open("c1"), prompt_sent("task-42"), stream_complete()]);
        assert_eq!(
            handles(&events),
            vec![Some("task-42".to_string()), None],
            "a completed turn must withdraw the handle"
        );
    }

    /// Nor is an abandoned one.
    #[test]
    fn an_errored_turn_withdraws_its_cancel_handle() {
        let events = emitted(vec![open("c1"), prompt_sent("task-42"), stream_error()]);
        assert_eq!(
            handles(&events),
            vec![Some("task-42".to_string()), None],
            "an errored turn must withdraw the handle"
        );
    }

    /// A legacy daemon acks with no task id. A stream is in flight, but nothing
    /// can cancel it, so no event says otherwise.
    #[test]
    fn a_turn_without_a_handle_reports_nothing() {
        let events = emitted(vec![open("c1"), prompt_sent("")]);
        assert!(
            handles(&events).is_empty(),
            "an id-less ack must report no handle, got {:?}",
            handles(&events)
        );
    }

    /// The event fires only on a change, so a client that redraws per event is
    /// not told the same thing by every streaming chunk.
    #[test]
    fn an_unchanged_handle_is_not_repeated() {
        let events = emitted(vec![
            open("c1"),
            prompt_sent("task-42"),
            UiMessage::StreamChunk {
                request_id: "r1".to_string(),
                chunk: "hello".to_string(),
            },
            UiMessage::StreamChunk {
                request_id: "r1".to_string(),
                chunk: " world".to_string(),
            },
        ]);
        assert_eq!(
            handles(&events),
            vec![Some("task-42".to_string())],
            "streaming chunks must not re-report an unchanged handle"
        );
    }

    // --- the retry offer ---

    /// A turn that fails offers its prompt back, so a client can put it in the
    /// composer for a one-click resend.
    #[test]
    fn a_failed_turn_offers_its_prompt_back() {
        let events = emitted(vec![
            open("c1"),
            user_said("what is the time"),
            prompt_sent("task-42"),
            stream_error(),
        ]);
        assert_eq!(offers(&events), vec!["what is the time".to_string()]);
    }

    /// The offer is one-shot: taking it clears it, so no later message can
    /// resurface a stale prompt into a composer.
    #[test]
    fn the_retry_offer_is_made_once() {
        let events = emitted(vec![
            open("c1"),
            user_said("what is the time"),
            prompt_sent("task-42"),
            stream_error(),
            UiMessage::StatusUpdate("something else".to_string()),
            UiMessage::StatusUpdate("and again".to_string()),
        ]);
        assert_eq!(
            offers(&events).len(),
            1,
            "the offer must be made once, not on every later message"
        );
    }

    /// A turn that completes normally offers nothing back.
    #[test]
    fn a_completed_turn_offers_nothing_back() {
        let events = emitted(vec![
            open("c1"),
            user_said("what is the time"),
            prompt_sent("task-42"),
            stream_complete(),
        ]);
        assert!(
            offers(&events).is_empty(),
            "a completed turn must not offer a retry"
        );
    }
}
