//! The C++-facing view-event schema — the FFI's stable JSON contract.
//!
//! The reducer ([`client_ui_common::WindowState::apply`]) returns
//! [`Effect`]s. The executor ([`crate::engine`]) splits them: *view* effects
//! become a [`ViewEvent`], are serialized to JSON, and pushed to the C callback;
//! *RPC* effects (the connector round-trips) are run by the executor and never
//! reach the C side. This module owns that split ([`ViewEvent::try_from_view_effect`])
//! and the serializable DTOs.
//!
//! Why DTOs and not the reducer's own types: the digested `api::client` views
//! (`ConversationSummary`/`ConversationDetail`/`ChatMessage`) and
//! `client-ui-common`'s `ContextUsageView`/`AdeleOutput` are intentionally NOT
//! `Serialize` (the reducer stays wasm-clean and presentation-free). Defining
//! the wire shape here keeps the C++/QML contract deliberate and decoupled from
//! the reducer's internals — the same reason the reducer is view-agnostic. The
//! `api`-model view types that *are* `Serialize` (models, tasks, scratchpad
//! notes, the model selection) are embedded directly to avoid pointless mirrors.

use client_ui_common::{AdeleOutput, ContextFillLevel, ContextUsageView, Effect, SelectedModel};
use desktop_assistant_api_model as api;
use desktop_assistant_api_model::client::{ChatMessage, ConversationDetail, ConversationSummary};
use serde::Serialize;

/// A conversation row for the sidebar.
#[derive(Debug, Serialize)]
pub struct ConversationSummaryDto {
    pub id: String,
    pub title: String,
    pub message_count: u32,
    pub archived: bool,
}

impl From<ConversationSummary> for ConversationSummaryDto {
    fn from(c: ConversationSummary) -> Self {
        Self {
            id: c.id,
            title: c.title,
            message_count: c.message_count,
            archived: c.archived,
        }
    }
}

/// A single message in the open transcript.
#[derive(Debug, Serialize)]
pub struct ChatMessageDto {
    /// Stable message id (empty only when talking to a pre-id daemon).
    pub id: String,
    pub role: String,
    pub content: String,
    /// Presentation metadata as an ABI token (`normal` / `spoken` /
    /// `speech_disabled`). Projected so a client can render a Spoken or
    /// SpeechDisabled affordance from the metadata instead of parsing
    /// `content` — `MessageKind` carries no serde derive, so it travels as a
    /// string like [`adele_output_str`] does for `AdeleOutput`.
    pub kind: &'static str,
}

impl From<ChatMessage> for ChatMessageDto {
    fn from(m: ChatMessage) -> Self {
        Self {
            id: m.id,
            role: m.role,
            content: m.content,
            kind: message_kind_str(m.kind),
        }
    }
}

/// The ABI token for a [`MessageKind`](api::client::MessageKind).
pub fn message_kind_str(kind: api::client::MessageKind) -> &'static str {
    match kind {
        api::client::MessageKind::Normal => "normal",
        api::client::MessageKind::Spoken => "spoken",
        api::client::MessageKind::SpeechDisabled => "speech_disabled",
    }
}

/// The `kind` token every built-in row carries, matching the shared panel
/// view-model's `ServerKind::BuiltIn`.
///
/// Why a constant field rather than an implied kind: a client renders one merged
/// list of daemon, external-client, and built-in rows, and the chip text comes
/// from the row's kind. Stamping it here means the kind is decided once, by the
/// core that actually hosts the server, instead of each client inferring
/// "built-in" from which array the row arrived in.
pub const BUILTIN_KIND: &str = "built_in";

/// One built-in (compiled-in, in-process) MCP server's panel status.
///
/// Mirrors `client-ui-common`'s `BuiltinServerDto` — the shape the shared
/// MCP-servers view-model already merges and sorts — and carries the two
/// *orthogonal* reasons a built-in can render disabled: shadowed by a same-name
/// external server ([`overridden_by`](Self::overridden_by)), or turned off for
/// this surface ([`disabled_by_config`](Self::disabled_by_config)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuiltinServerDto {
    /// Server name — also the key an external client-run server of the same name
    /// overrides.
    pub name: String,
    /// The built-in's tool namespace (e.g. `"fileio"`).
    pub namespace: String,
    /// Always [`BUILTIN_KIND`].
    pub kind: &'static str,
    /// When hosted, the tools actually registered; when not, the number this
    /// built-in would have advertised.
    pub tool_count: u32,
    /// `Some(name)` when a configured client-mcp server of the same name shadows
    /// this built-in (external wins); `None` when nothing shadows it.
    pub overridden_by: Option<String>,
    /// `true` when this surface's `[surfaces.<name>].disabled_builtins` names it.
    /// Orthogonal to [`overridden_by`](Self::overridden_by) — both can be set.
    pub disabled_by_config: bool,
}

/// The [`ViewEvent`] a daemon signal produces *directly*, bypassing the
/// reducer, or `None` when the reducer already covers it.
///
/// Why: nearly every signal becomes a `UiMessage` and reaches the view as an
/// `Effect`. `KnowledgeChanged` is the exception — the knowledge browser is a
/// self-contained widget rather than part of the conversation reducer, so the
/// reducer drops the message and each client wires its own refresh at the
/// window layer. This FFI *is* that layer for its clients, so it forwards the
/// signal itself. Keep this list minimal: a signal the reducer already handles
/// would otherwise reach the view twice.
pub fn view_event_for_signal(sig: &api::SignalEvent) -> Option<ViewEvent> {
    match sig {
        api::SignalEvent::KnowledgeChanged => Some(ViewEvent::KnowledgeChanged),
        _ => None,
    }
}

/// The open conversation (already debug-filtered by the reducer).
#[derive(Debug, Serialize)]
pub struct ConversationDetailDto {
    pub id: String,
    pub title: String,
    pub messages: Vec<ChatMessageDto>,
    pub model_selection: Option<api::ConversationModelSelectionView>,
}

impl From<ConversationDetail> for ConversationDetailDto {
    fn from(d: ConversationDetail) -> Self {
        Self {
            id: d.id,
            title: d.title,
            messages: d.messages.into_iter().map(ChatMessageDto::from).collect(),
            model_selection: d.model_selection,
        }
    }
}

/// Context-window fill readout (#341). All display formatting (`readout`,
/// `level`) is computed in Rust so the C++ side never reimplements it.
#[derive(Debug, Serialize)]
pub struct ContextUsageDto {
    pub used_tokens: u64,
    pub budget_tokens: u64,
    pub compaction_active: bool,
    pub fraction: f64,
    /// `"green"` / `"amber"` / `"red"`.
    pub level: &'static str,
    /// Pre-formatted glanceable string, e.g. `12k / 32k (38%)`.
    pub readout: String,
}

impl From<ContextUsageView> for ContextUsageDto {
    fn from(u: ContextUsageView) -> Self {
        Self {
            used_tokens: u.used_tokens,
            budget_tokens: u.budget_tokens,
            compaction_active: u.compaction_active,
            fraction: u.fraction(),
            level: fill_level_str(u.level()),
            readout: u.readout(),
        }
    }
}

fn fill_level_str(level: ContextFillLevel) -> &'static str {
    match level {
        ContextFillLevel::Green => "green",
        ContextFillLevel::Amber => "amber",
        ContextFillLevel::Red => "red",
    }
}

/// Serialize an [`AdeleOutput`] level as the snake_case token the C ABI uses.
pub fn adele_output_str(level: AdeleOutput) -> &'static str {
    match level {
        AdeleOutput::Disabled => "disabled",
        AdeleOutput::OnDemand => "on_demand",
        AdeleOutput::Always => "always",
    }
}

/// Parse an [`AdeleOutput`] level from the C ABI token; anything unrecognised
/// (or empty) is the safe default, `Disabled` (never speaks).
pub fn adele_output_from_str(s: &str) -> AdeleOutput {
    match s {
        "on_demand" => AdeleOutput::OnDemand,
        "always" => AdeleOutput::Always,
        _ => AdeleOutput::Disabled,
    }
}

/// One observable update for the C++/QML view, serialized as
/// `{"type": "<snake_case>", ...fields}`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ViewEvent {
    /// A connection came up (executor-emitted, not from an `Effect`).
    Connected { label: String },
    /// A connection attempt failed (executor-emitted).
    ConnectError { message: String },
    /// Result of a client-issued management command (`adele_core_send_command`),
    /// correlated by `request_id`. Executor-emitted (not from an `Effect`).
    /// `result` carries the `CommandResult` as JSON on success; `error` is set on
    /// failure. This is the generic management channel (connections, purposes,
    /// knowledge base) the reducer's typed effects don't cover.
    CommandResult {
        request_id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The connector was torn down — `Effect::ClearClient` (disconnect).
    ClientCleared,
    /// Bottom status-bar text.
    Status { text: String },
    /// Enable/disable the send control.
    SendSensitive { value: bool },
    /// Replace the composer widget's text (cursor to end) — the reducer loads a
    /// recalled queued message here, or clears the composer (`""`) when a
    /// submitted message is queued or an edit is cancelled.
    ComposerText { text: String },
    /// Render-ready snapshot of the open conversation's queued messages (in
    /// submit order) plus the index currently checked out for editing, if any.
    /// Drives the "N queued" chips / indicator.
    QueuedMessages {
        messages: Vec<String>,
        editing: Option<usize>,
    },
    /// Replace the sidebar conversation list.
    Conversations { items: Vec<ConversationSummaryDto> },
    /// Load a conversation into the chat view (replaces the transcript).
    LoadConversation { detail: ConversationDetailDto },
    /// Clear the chat view.
    ClearChat,
    /// Transient chat status line (e.g. "Thinking…").
    ChatStatus { text: String },
    /// Clear the transient chat status line.
    ClearChatStatus,
    /// Context-window fill, or `null` to hide it.
    ContextUsage { usage: Option<ContextUsageDto> },
    /// Append a user bubble (own optimistic send, or an adopted external turn).
    AddUserMessage { content: String },
    /// Append a streaming chunk to the in-progress assistant bubble.
    Chunk { text: String },
    /// Finalize the in-progress assistant bubble.
    Complete { text: String },
    /// Apply (or clear) the model-picker selection.
    ModelSelection {
        selection: Option<api::ConversationModelSelectionView>,
    },
    /// Replace the model-picker's available models.
    Models { items: Vec<api::ModelListing> },
    /// The resolved interactive-purpose default model (picker fallback).
    DefaultModel { model: Option<SelectedModel> },
    /// Show/hide the model picker.
    ModelPickerVisible { value: bool },
    /// Reveal a passive toast.
    Toast { text: String },
    /// Replace the whole background-task list.
    TasksReplaceAll { items: Vec<api::TaskView> },
    /// A task started.
    TaskStarted { task: api::TaskView },
    /// A task progress update.
    TaskProgress {
        id: String,
        progress_hint: Option<String>,
    },
    /// A task log line was appended.
    TaskLogAppended {
        id: String,
        entry: api::TaskLogEntry,
    },
    /// A task reached a terminal state.
    TaskCompleted { id: String },
    /// A fetched page of a background task's logs (response to a FetchTaskLogs
    /// intent). Emitted directly by the executor, not via an `Effect`.
    TaskLogs {
        id: String,
        entries: Vec<api::TaskLogEntry>,
    },
    /// Replace the side pane's scratchpad notes.
    Scratchpad { notes: Vec<api::ScratchpadNoteView> },
    /// Recompute the side pane's per-conversation task view (the C++ side filters
    /// its own task list — this is the hint to refresh).
    RefreshSidePaneTasks,
    /// The user's knowledge base changed — refetch it if a browser is open.
    /// Forwarded straight from the daemon signal by [`view_event_for_signal`]
    /// rather than produced by the reducer, which does not model the knowledge
    /// browser.
    KnowledgeChanged,
    /// Speak `text` (the C++ side may route this to `org.desktopAssistant.Voice`;
    /// the plasmoid has no embedded engine, so it is a no-op there).
    Speak { text: String },
    /// Render an inline transcript note — a line the client generated itself
    /// rather than received from the daemon (a `say_this` the voice tool spoke,
    /// the same line when speech is off, a local notice).
    InlineNote {
        text: String,
        /// Presentation metadata as an ABI token (`normal` / `spoken` /
        /// `speech_disabled`), the same contract [`ChatMessageDto::kind`]
        /// carries for a reloaded transcript. Why here too: this is the *live*
        /// path for the same lines, and a client that had to recover the kind
        /// by matching a marker in `text` would classify by prose — silently
        /// falling back to `normal` the moment the wording moved.
        kind: &'static str,
    },
    /// Reflect the active conversation's `Adele:` level on the dropdown after the
    /// model drove it (`request_voice` / `stop_voice`).
    AdeleOutputDropdown { level: &'static str },
    /// This client's compiled-in ("built-in") MCP servers and their status under
    /// the declared `client-mcp.toml` surface — the reply to
    /// `adele_core_request_mcp_builtins`, and re-emitted after
    /// `adele_core_set_mcp_builtin_disabled` so a toggle resyncs the panel.
    ///
    /// Executor-emitted (no `Effect` carries it): built-ins are a property of how
    /// this cdylib was built plus what is on disk, not of the conversation
    /// reducer. `servers` is empty on a core built with no `mcp-*` feature —
    /// adele-kde's build — which is the honest answer, not a missing one.
    ///
    /// `surface` echoes which section was resolved, so a client can tell it is
    /// reading its own (`mac`) rather than silently inheriting another's.
    McpBuiltins {
        surface: String,
        servers: Vec<BuiltinServerDto>,
    },
}

impl ViewEvent {
    /// Convert a *view* effect into the JSON event the C++ side renders.
    ///
    /// Returns `Err(effect)` for the effects the executor must handle itself —
    /// the connector round-trips (`SendPrompt`, `LoadConversation`,
    /// `SubscribeConversations`, …) and `ClearClient` (which also clears the
    /// executor's connector). This is the one place the view/RPC split is
    /// decided, so adding an `Effect` variant forces a decision here (the wildcard
    /// only catches the known RPC set; a brand-new variant lands in `Err` and is
    /// surfaced by the executor's debug assert).
    pub fn try_from_view_effect(effect: Effect) -> Result<ViewEvent, Box<Effect>> {
        let ev = match effect {
            Effect::SetStatusText(text) => ViewEvent::Status { text },
            Effect::SetSendSensitive(value) => ViewEvent::SendSensitive { value },
            Effect::SetComposerText(text) => ViewEvent::ComposerText { text },
            Effect::SetQueuedMessages { messages, editing } => {
                ViewEvent::QueuedMessages { messages, editing }
            }
            Effect::SetConversations(convs) => ViewEvent::Conversations {
                items: convs
                    .into_iter()
                    .map(ConversationSummaryDto::from)
                    .collect(),
            },
            Effect::LoadConversationIntoChat(detail) => ViewEvent::LoadConversation {
                detail: ConversationDetailDto::from(detail),
            },
            Effect::ClearChat => ViewEvent::ClearChat,
            Effect::SetChatStatus(text) => ViewEvent::ChatStatus { text },
            Effect::ClearChatStatus => ViewEvent::ClearChatStatus,
            Effect::SetContextUsage(u) => ViewEvent::ContextUsage {
                usage: u.map(ContextUsageDto::from),
            },
            Effect::AddUserMessage(content) => ViewEvent::AddUserMessage { content },
            Effect::ReceiveChunk(text) => ViewEvent::Chunk { text },
            Effect::CompleteStreaming(text) => ViewEvent::Complete { text },
            Effect::SetModelSelection(selection) => ViewEvent::ModelSelection { selection },
            Effect::SetModels(items) => ViewEvent::Models { items },
            Effect::SetDefaultModel(model) => ViewEvent::DefaultModel { model },
            Effect::SetModelPickerVisible(value) => ViewEvent::ModelPickerVisible { value },
            Effect::ShowToast(text) => ViewEvent::Toast { text },
            Effect::TasksReplaceAll(items) => ViewEvent::TasksReplaceAll { items },
            Effect::TaskStarted(task) => ViewEvent::TaskStarted { task },
            Effect::TaskProgress { id, progress_hint } => {
                ViewEvent::TaskProgress { id, progress_hint }
            }
            Effect::TaskLogAppended { id, entry } => ViewEvent::TaskLogAppended { id, entry },
            Effect::TaskCompleted { id } => ViewEvent::TaskCompleted { id },
            Effect::SidePaneSetScratchpad(notes) => ViewEvent::Scratchpad { notes },
            Effect::RefreshSidePaneTasks => ViewEvent::RefreshSidePaneTasks,
            Effect::Speak(text) => ViewEvent::Speak { text },
            Effect::AddLocalMessage { content, kind } => ViewEvent::InlineNote {
                text: content,
                kind: message_kind_str(kind),
            },
            Effect::SetAdeleOutputDropdown(level) => ViewEvent::AdeleOutputDropdown {
                level: adele_output_str(level),
            },
            // RPC / connector-state effects: the executor runs these. Boxed so
            // the (large) `Effect` doesn't bloat every `Result` (result_large_err).
            rpc => return Err(Box::new(rpc)),
        };
        Ok(ev)
    }

    /// Serialize to the compact JSON string passed across the C boundary.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_event_tag_is_snake_case_with_fields() {
        let ev = ViewEvent::Chunk {
            text: "hi".to_string(),
        };
        assert_eq!(ev.to_json().unwrap(), r#"{"type":"chunk","text":"hi"}"#);
    }

    #[test]
    fn unit_view_event_serializes_with_only_a_tag() {
        assert_eq!(
            ViewEvent::ClearChatStatus.to_json().unwrap(),
            r#"{"type":"clear_chat_status"}"#
        );
    }

    #[test]
    fn conversations_effect_maps_to_a_view_event() {
        let convs = vec![ConversationSummary {
            id: "c1".into(),
            title: "First".into(),
            message_count: 3,
            archived: false,
        }];
        let ev = ViewEvent::try_from_view_effect(Effect::SetConversations(convs))
            .expect("SetConversations is a view effect");
        let json = ev.to_json().unwrap();
        assert!(json.contains(r#""type":"conversations""#));
        assert!(json.contains(r#""id":"c1""#));
        assert!(json.contains(r#""message_count":3"#));
    }

    #[test]
    fn context_usage_carries_rust_computed_readout_and_level() {
        let ev = ViewEvent::try_from_view_effect(Effect::SetContextUsage(Some(ContextUsageView {
            used_tokens: 27_200,
            budget_tokens: 32_000,
            compaction_active: false,
        })))
        .expect("SetContextUsage is a view effect");
        let json = ev.to_json().unwrap();
        assert!(json.contains(r#""level":"amber""#), "0.85 ⇒ amber: {json}");
        assert!(json.contains(r#""readout":"27k / 32k (85%)""#), "{json}");
    }

    #[test]
    fn rpc_effects_are_returned_for_the_executor() {
        // A representative RPC effect must NOT be turned into a ViewEvent.
        let back = ViewEvent::try_from_view_effect(Effect::SendPrompt {
            conversation_id: "c1".into(),
            prompt: "hello".into(),
            system_refinement: None,
            idempotency_key: None,
        });
        assert!(matches!(back, Err(b) if matches!(*b, Effect::SendPrompt { .. })));

        assert!(matches!(
            ViewEvent::try_from_view_effect(Effect::ClearClient),
            Err(b) if matches!(*b, Effect::ClearClient)
        ));
        assert!(matches!(
            ViewEvent::try_from_view_effect(Effect::SubscribeConversations(vec!["c".into()])),
            Err(b) if matches!(*b, Effect::SubscribeConversations(_))
        ));
    }

    #[test]
    fn composer_text_effect_maps_to_a_view_event() {
        let ev = ViewEvent::try_from_view_effect(Effect::SetComposerText("recalled".into()))
            .expect("SetComposerText is a view effect");
        assert_eq!(
            ev.to_json().unwrap(),
            r#"{"type":"composer_text","text":"recalled"}"#
        );
    }

    #[test]
    fn queued_messages_effect_maps_to_a_view_event() {
        let ev = ViewEvent::try_from_view_effect(Effect::SetQueuedMessages {
            messages: vec!["a".into(), "b".into()],
            editing: Some(1),
        })
        .expect("SetQueuedMessages is a view effect");
        let json = ev.to_json().unwrap();
        assert!(json.contains(r#""type":"queued_messages""#), "{json}");
        assert!(json.contains(r#""messages":["a","b"]"#), "{json}");
        assert!(json.contains(r#""editing":1"#), "{json}");
    }

    #[test]
    fn chat_message_dto_projects_kind() {
        // `ChatMessage` carries explicit presentation metadata (voice#126), but
        // the DTO used to drop it — leaving an FFI client (adele-mac, adele-kde)
        // unable to render a Spoken / SpeechDisabled affordance without parsing
        // `content` back, which is exactly what the metadata exists to avoid.
        let dto = ChatMessageDto::from(ChatMessage {
            id: "m1".into(),
            role: "assistant".into(),
            content: "hello".into(),
            kind: api::client::MessageKind::Spoken,
            idempotency_key: None,
        });
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains(r#""kind":"spoken""#), "{json}");
    }

    #[test]
    fn every_message_kind_has_a_distinct_abi_token() {
        // One-way only: the C side never sets a message kind, so unlike
        // `adele_output_*` there is no parse-back counterpart. The tokens are a
        // wire contract a client matches on, so pin them literally.
        assert_eq!(message_kind_str(api::client::MessageKind::Normal), "normal");
        assert_eq!(message_kind_str(api::client::MessageKind::Spoken), "spoken");
        assert_eq!(
            message_kind_str(api::client::MessageKind::SpeechDisabled),
            "speech_disabled"
        );
    }

    #[test]
    fn inline_note_carries_structured_kind() {
        // A `say_this` line generated DURING a turn reaches the view as
        // `AddLocalMessage`. The kind used to be stringified into the note's
        // text ("Spoken: …" / "(speech mode disabled) …") and parsed back by the
        // client — the very round-trip the metadata exists to avoid. The event
        // carries the kind as an ABI token and leaves `text` unmarked; a client
        // renders whatever affordance it has (a badge, or its own marker).
        let ev = ViewEvent::try_from_view_effect(Effect::AddLocalMessage {
            content: "hello there".into(),
            kind: api::client::MessageKind::Spoken,
        })
        .expect("AddLocalMessage is a view effect");
        assert_eq!(
            ev.to_json().unwrap(),
            r#"{"type":"inline_note","text":"hello there","kind":"spoken"}"#
        );
    }

    #[test]
    fn a_speech_disabled_inline_note_is_unmarked_too() {
        // The suppressed case travelled as a parenthetical prefix, which is the
        // one most likely to be mistaken for prose the model wrote.
        let ev = ViewEvent::try_from_view_effect(Effect::AddLocalMessage {
            content: "hello there".into(),
            kind: api::client::MessageKind::SpeechDisabled,
        })
        .expect("AddLocalMessage is a view effect");
        assert_eq!(
            ev.to_json().unwrap(),
            r#"{"type":"inline_note","text":"hello there","kind":"speech_disabled"}"#
        );
    }

    #[test]
    fn an_ordinary_inline_note_carries_the_normal_token() {
        // Notes that are not `say_this` lines (reconnect notices and the like)
        // must state `normal` explicitly rather than omit the field, so a client
        // reads one contract instead of "missing ⇒ normal".
        let ev = ViewEvent::try_from_view_effect(Effect::AddLocalMessage {
            content: "Reconnected to the daemon.".into(),
            kind: api::client::MessageKind::Normal,
        })
        .expect("AddLocalMessage is a view effect");
        assert_eq!(
            ev.to_json().unwrap(),
            r#"{"type":"inline_note","text":"Reconnected to the daemon.","kind":"normal"}"#
        );
    }

    #[test]
    fn a_normal_message_still_carries_its_kind() {
        // The common case must be explicit rather than an absent field, so a
        // client reads one contract instead of "missing ⇒ normal".
        let dto = ChatMessageDto::from(ChatMessage {
            id: "m2".into(),
            role: "user".into(),
            content: "hi".into(),
            kind: api::client::MessageKind::Normal,
            idempotency_key: None,
        });
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains(r#""kind":"normal""#), "{json}");
    }

    #[test]
    fn knowledge_changed_signal_maps_to_a_view_event() {
        // The conversation reducer deliberately ignores `KnowledgeChanged` (the
        // knowledge browser is a self-contained widget), so gtk subscribes to the
        // signal at its window layer. The FFI *is* the window-layer boundary for
        // adele-mac / adele-kde, so it forwards the signal directly.
        let ev = view_event_for_signal(&api::SignalEvent::KnowledgeChanged)
            .expect("KnowledgeChanged is forwarded to the view");
        assert_eq!(ev.to_json().unwrap(), r#"{"type":"knowledge_changed"}"#);
    }

    #[test]
    fn unrelated_signals_are_not_forwarded_to_the_view() {
        // Only signals the reducer drops on the floor need a direct forward;
        // everything else must keep flowing through `signal_to_ui_message` alone,
        // or the view would see it twice.
        assert!(
            view_event_for_signal(&api::SignalEvent::Chunk {
                conversation_id: "c1".into(),
                request_id: "r1".into(),
                chunk: "hi".into(),
            })
            .is_none()
        );
    }

    #[test]
    fn adele_output_round_trips_through_the_abi_tokens() {
        for level in [
            AdeleOutput::Disabled,
            AdeleOutput::OnDemand,
            AdeleOutput::Always,
        ] {
            assert_eq!(adele_output_from_str(adele_output_str(level)), level);
        }
        // Unknown / empty ⇒ the safe default.
        assert_eq!(adele_output_from_str("garbage"), AdeleOutput::Disabled);
        assert_eq!(adele_output_from_str(""), AdeleOutput::Disabled);
    }

    // --- mcp_builtins (the built-in inventory read path) -----------------------

    fn builtin_dto(name: &str, overridden_by: Option<&str>) -> BuiltinServerDto {
        BuiltinServerDto {
            name: name.to_string(),
            namespace: name.to_string(),
            kind: BUILTIN_KIND,
            tool_count: 3,
            overridden_by: overridden_by.map(str::to_string),
            disabled_by_config: false,
        }
    }

    /// The wire shape every client decodes. Pinned literally: the field names ARE
    /// the ABI, and a rename would silently blank the panel rather than fail.
    #[test]
    fn mcp_builtins_event_carries_the_surface_and_every_row_field() {
        let ev = ViewEvent::McpBuiltins {
            surface: "mac".to_string(),
            servers: vec![builtin_dto("fileio", None)],
        };
        assert_eq!(
            ev.to_json().expect("serializes"),
            r#"{"type":"mcp_builtins","surface":"mac","servers":[{"name":"fileio","namespace":"fileio","kind":"built_in","tool_count":3,"overridden_by":null,"disabled_by_config":false}]}"#
        );
    }

    /// An override travels as the shadowing server's name, not a bare boolean, so
    /// the panel can name it in the row's reason line.
    #[test]
    fn mcp_builtins_event_names_the_overriding_server() {
        let ev = ViewEvent::McpBuiltins {
            surface: "mac".to_string(),
            servers: vec![builtin_dto("web", Some("web"))],
        };
        let json = ev.to_json().expect("serializes");
        assert!(json.contains(r#""overridden_by":"web""#), "{json}");
    }

    /// A core with no built-ins linked still answers — with an empty list — so the
    /// panel can tell "none compiled in" from "never asked".
    #[test]
    fn mcp_builtins_event_answers_empty_rather_than_silently() {
        let ev = ViewEvent::McpBuiltins {
            surface: "kde".to_string(),
            servers: Vec::new(),
        };
        assert_eq!(
            ev.to_json().expect("serializes"),
            r#"{"type":"mcp_builtins","surface":"kde","servers":[]}"#
        );
    }
}
