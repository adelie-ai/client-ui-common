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
//! through the identical path. The actor never blocks: `apply` + `emit` +
//! `tokio::spawn` are all synchronous, so the loop returns to `recv` immediately.
//!
//! The reducer is transport-free (it carries no `Connector`): the actor owns the
//! connector directly, installs it on connect, and drops it on
//! [`Effect::ClearClient`].

use std::sync::Arc;

use client_ui_common::{
    AdeleOutput, Effect, UiMessage, WindowState, interactive_default_from_purposes,
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

use crate::view_event::ViewEvent;

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
            Intent::SendCommand {
                request_id,
                command_json,
            } => self.spawn_send_command(request_id, command_json),
        }
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
                    let mcp_servers: Vec<_> = ClientMcpConfig::load(&default_client_mcp_path())
                        .resolved_servers("kde")
                        .into_iter()
                        .cloned()
                        .collect();
                    let mcp_host = if mcp_servers.is_empty() {
                        None
                    } else {
                        let host = Arc::new(McpHost::start(&mcp_servers).await);
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
        }
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
