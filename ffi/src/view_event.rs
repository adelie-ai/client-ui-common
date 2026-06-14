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
}

impl From<ChatMessage> for ChatMessageDto {
    fn from(m: ChatMessage) -> Self {
        Self {
            id: m.id,
            role: m.role,
            content: m.content,
        }
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
    /// The connector was torn down — `Effect::ClearClient` (disconnect).
    ClientCleared,
    /// Bottom status-bar text.
    Status { text: String },
    /// Enable/disable the send control.
    SendSensitive { value: bool },
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
    /// Replace the side pane's scratchpad notes.
    Scratchpad { notes: Vec<api::ScratchpadNoteView> },
    /// Recompute the side pane's per-conversation task view (the C++ side filters
    /// its own task list — this is the hint to refresh).
    RefreshSidePaneTasks,
    /// Speak `text` (the C++ side may route this to `org.desktopAssistant.Voice`;
    /// the plasmoid has no embedded engine, so it is a no-op there).
    Speak { text: String },
    /// Render an inline transcript note (e.g. a `(speech mode disabled) …`
    /// downgrade).
    InlineNote { text: String },
    /// Reflect the active conversation's `Adele:` level on the dropdown after the
    /// model drove it (`request_voice` / `stop_voice`).
    AdeleOutputDropdown { level: &'static str },
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
            Effect::AddInlineNote(text) => ViewEvent::InlineNote { text },
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
}
