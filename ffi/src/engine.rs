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

/// The actor: owns the reducer state + the connector, runs effects.
struct Engine {
    state: WindowState,
    connector: Option<Arc<Connector>>,
    self_tx: mpsc::UnboundedSender<CoreMsg>,
    sink: ViewSink,
    /// Per-message model override staged by `SelectModel`, applied on the next
    /// send. `None` ⇒ inherit the conversation / interactive-purpose default.
    staged_override: Option<api::SendPromptOverride>,
}

impl Engine {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<CoreMsg>) {
        while let Some(msg) = rx.recv().await {
            match msg {
                CoreMsg::Intent(intent) => self.handle_intent(intent),
                CoreMsg::Ui(boxed) => self.dispatch(*boxed),
                CoreMsg::InstallConnector(conn) => self.connector = Some(conn),
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
        }
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

    /// Send-decision via the shared core. The reducer draws the optimistic user
    /// bubble into its own transcript but does NOT emit `AddUserMessage` for our
    /// own send (tui re-reads state; our view is event-driven) — so surface the
    /// bubble here when accepted, mirroring gtk's optimistic draw. The daemon's
    /// echoed `UserMessageAdded` is deduped by request_id, so no double-render.
    fn submit_prompt(&mut self, text: String) {
        let effects = self.state.apply(UiMessage::SubmitPrompt {
            prompt: text.clone(),
        });
        if effects
            .iter()
            .any(|e| matches!(e, Effect::SendPrompt { .. }))
        {
            self.sink.emit(&ViewEvent::AddUserMessage { content: text });
        }
        for effect in effects {
            self.run_effect(effect);
        }
    }

    /// Run one effect: view effects emit; the connector-state + RPC effects are
    /// handled by the actor.
    fn run_effect(&mut self, effect: Effect) {
        // `ClearClient` both mutates actor state and notifies the view.
        if matches!(effect, Effect::ClearClient) {
            self.connector = None;
            self.sink.emit(&ViewEvent::ClientCleared);
            return;
        }
        match ViewEvent::try_from_view_effect(effect) {
            Ok(ev) => self.sink.emit(&ev),
            Err(rpc) => self.run_rpc_effect(*rpc),
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
            } => self.spawn_send(conversation_id, prompt, system_refinement),
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
        tokio::spawn(async move {
            let mut config = ConnectionConfig {
                transport_mode: mode,
                ..Default::default()
            };
            match mode {
                TransportMode::Uds if !address.is_empty() => {
                    config.socket_path = Some(address.into());
                }
                TransportMode::Ws if !address.is_empty() => config.ws_url = address,
                _ => {}
            }
            match Connector::connect(&config).await {
                Ok(conn) => {
                    let conn = Arc::new(conn);
                    let label = conn.label().to_string();
                    // Install in the actor FIRST so later effects find it.
                    let _ = tx.send(CoreMsg::InstallConnector(Arc::clone(&conn)));
                    // Pump signals -> messages. Holds only the receiver (never the
                    // Arc<Connector>), so dropping the actor's connector tears the
                    // connection down cleanly.
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
                    // Advertise voice-mode client tools (best-effort; the daemon
                    // replaces its set per call, so send on every connect).
                    if let Err(e) = conn.register_client_tools(voice_mode_client_tools()).await {
                        tracing::debug!("voice-mode client tools not registered: {e}");
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
            // With a staged model override, send via the generic Commands channel
            // (`send_prompt_full` carries BOTH the override and the refinement);
            // otherwise use the Connector's refinement send, which also handles
            // the no-Commands D-Bus prompt-fold fallback.
            let result = if let Some(ov) = override_selection {
                if let Some(cmds) = conn.client().as_commands() {
                    cmds.send_prompt_full(
                        &conversation_id,
                        &prompt,
                        Some(ov),
                        refinement.to_string(),
                    )
                    .await
                } else {
                    conn.send_prompt_with_system_refinement(&conversation_id, &prompt, refinement)
                        .await
                }
            } else {
                conn.send_prompt_with_system_refinement(&conversation_id, &prompt, refinement)
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
            self_tx: tx.clone(),
            sink,
            staged_override: None,
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
