//! The UI message type (`UiMessage`), the daemon-signal->message map, and the
//! interactive-purpose default extractor — the reducer's *input* side, lifted
//! from adele-gtk's `async_bridge.rs`. Pure: no GTK and no transport calls (the
//! glib/tokio bridge that produces these messages stays client-side), and — per
//! this crate's design rule — no transport handle in the model. The wire types
//! and the signal stream come from `api-model` (wasm-clean), never from the
//! native transport crate.

use adele_voice_client_common::AdeleOutput;
use desktop_assistant_api_model as api;
use desktop_assistant_api_model::SignalEvent;

/// Messages sent from async tasks back to the GTK main thread.
pub enum UiMessage {
    ConversationsLoaded(Vec<api::client::ConversationSummary>),
    /// A *list-only* re-fetch of the conversation list, triggered by
    /// [`UiMessage::ConversationListChanged`] (a conversation was
    /// created/renamed/deleted/(un)archived elsewhere — #1). Unlike
    /// [`UiMessage::ConversationsLoaded`], the reducer repaints ONLY the sidebar
    /// (and re-syncs the selection); it deliberately does NOT reload the open
    /// conversation's chat or touch the model picker, so a sibling-client change
    /// never disturbs what the user is reading/typing.
    ConversationListRefetched(Vec<api::client::ConversationSummary>),
    ConversationLoaded(api::client::ConversationDetail),
    /// A conversation that is *already open* was re-fetched (on reconnect, or
    /// after a debug/personality refresh). The window refreshes the cached
    /// detail + chat but, unlike [`ConversationLoaded`], does NOT reset the
    /// model picker — the user's selection (sent or unsent) must survive a
    /// reconnect (issue #72).
    ConversationReloaded(api::client::ConversationDetail),
    ConversationCreated {
        id: String,
    },
    ConversationDeleted {
        id: String,
    },
    ConversationRenamed {
        id: String,
        title: String,
    },
    StreamChunk {
        request_id: String,
        chunk: String,
    },
    StreamComplete {
        request_id: String,
        full_response: String,
    },
    StreamError {
        request_id: String,
        error: String,
    },
    AssistantStatus {
        request_id: String,
        message: String,
    },
    /// A user message was committed and a turn started (desktop-assistant
    /// `UserMessageAdded`, #1). Emitted for every turn — including ones this
    /// client did NOT initiate (a voice turn, or another client on the same
    /// account). The reducer renders the user bubble live for an external turn
    /// in the open conversation; for this client's own send it dedupes on the
    /// in-flight `request_id` (the bubble was already drawn optimistically).
    UserMessageAdded {
        conversation_id: String,
        request_id: String,
        content: String,
    },
    /// Per-turn context-window fill report (desktop-assistant#341). Token
    /// COUNTS only — drives the read-only fill indicator in the status bar.
    ContextUsage {
        conversation_id: String,
        used_tokens: u64,
        budget_tokens: u64,
        compaction_active: bool,
    },
    TitleChanged {
        conversation_id: String,
        title: String,
    },
    /// The user's conversation list changed on another connection — a
    /// conversation was created, renamed, deleted, or (un)archived by another
    /// client or the voice daemon (desktop-assistant#1). Carried from
    /// [`SignalEvent::ConversationListChanged`]; carries only the affected
    /// `conversation_id`. The reducer responds with a full list re-fetch
    /// ([`Effect::RefetchConversationList`]) — simplest and correct for every
    /// change kind — rather than a surgical per-row edit. The refetch result
    /// arrives as [`UiMessage::ConversationListRefetched`], which repaints only
    /// the sidebar.
    ConversationListChanged {
        conversation_id: String,
    },
    /// A one-time advisory for a conversation emitted as a live signal
    /// (today only `DanglingModelSelection`: the stored model selection no
    /// longer resolves and was cleared server-side). Drives a passive toast
    /// in the window. Replaces the earlier lossy `StatusUpdate`-string
    /// mapping so the handler can act on the typed warning.
    ConversationWarning {
        conversation_id: String,
        warning: api::ConversationWarning,
    },
    /// The wire ack carries a `task_id` (post-#114 `SendMessageAck`) or an
    /// empty string (legacy `Ack`). It is NOT the chunk-stream
    /// `request_id` — that is server-generated and arrives embedded in
    /// the first `AssistantDelta`. See issue #31.
    PromptSent {
        // Staged for streaming-chunk correlation (#31): consumer currently
        // ignores it (`PromptSent { task_id: _ }`), but the ack value is kept
        // on the message so the streaming work can correlate without a wire
        // change. See the variant doc above and issues #114/#31.
        #[allow(dead_code)]
        task_id: String,
        /// The conversation the prompt was sent into, captured **at send
        /// time** (GTK-2, "the stream knows its conversation"). The reducer
        /// records it so chunks/completion of the in-flight stream stay tied
        /// to the originating conversation even if the user switches away
        /// mid-stream.
        conversation_id: String,
    },
    /// Available (connection, model) pairs, fetched once on connect.
    /// Empty list means the picker should hide (e.g. D-Bus transport).
    ModelsLoaded(Vec<api::ModelListing>),
    /// The resolved interactive-purpose default model, fetched via
    /// `GetPurposes` on connect (and re-fetched after Settings edits). The
    /// picker uses it as the fallback selection for conversations with no
    /// stored selection, so the button always shows a concrete model instead
    /// of a "(default)" placeholder. `None` when it can't be resolved (the
    /// command failed, the interactive purpose is unset, or it uses the
    /// "primary"/inherit sentinel) — the picker then degrades to "Model".
    DefaultModelLoaded(Option<crate::selected_models::SelectedModel>),
    Connected {
        label: String,
    },
    Disconnected {
        reason: String,
    },
    StatusUpdate(String),
    Error(String),

    // --- Background tasks (issue #19) -------------------------------------
    //
    // The connection manager forwards `Event::Task*` frames into these
    // variants so the GTK main thread can update the process-manager panel
    // without touching tokio or the WebSocket directly. `TasksLoaded` is
    // produced by the initial `ListBackgroundTasks` snapshot taken on
    // connect (and on reconnect — see `connection_manager`).
    TasksLoaded(Vec<api::TaskView>),
    // The four streaming variants below carry the daemon's
    // `Event::Task*` frames into the GTK main thread via the
    // `SignalEvent::Task*` family on `client-common` (issue #22).
    TaskStarted(api::TaskView),
    TaskProgress {
        id: String,
        progress_hint: Option<String>,
    },
    TaskLogAppended {
        id: String,
        entry: api::TaskLogEntry,
    },
    TaskCompleted {
        id: String,
    },

    // --- Conversation scratchpad (issue #60) ------------------------------
    /// The scratchpad notes for a conversation, fetched via
    /// `GetConversationScratchpad` after a load / turn-complete / change event.
    /// The window applies it to the side pane only when it matches the active
    /// conversation.
    ConversationScratchpadLoaded {
        conversation_id: String,
        notes: Vec<api::ScratchpadNoteView>,
    },
    /// A conversation's scratchpad changed (the LLM's tools or a client command
    /// mutated it). Carried from `SignalEvent::ScratchpadChanged`; the window
    /// re-fetches when it matches the active conversation.
    ScratchpadChanged {
        conversation_id: String,
    },

    // --- Voice input (`You:` dropdown, issue #80) -------------------------
    /// The user changed the per-conversation `You:` (voice input) dropdown in
    /// the input bar. Carries the conversation the setting belongs to (so a
    /// stale setting from a since-switched conversation can't bleed) and whether
    /// voice input is Enabled (`true`, push-to-talk available) or Disabled
    /// (`false`, type only). Default is Disabled. See issue #80.
    SetVoiceIn {
        conversation_id: String,
        enabled: bool,
    },

    // --- Voice output (`Adele:` dropdown, issue #80) ----------------------
    /// The user changed the per-conversation `Adele:` (voice output) dropdown in
    /// the input bar (the model drives the same state via the `request_voice` /
    /// `stop_voice` client tools, which select OnDemand / Disabled). Carries the
    /// conversation it belongs to and the new output level. Default is Disabled.
    /// The level decides reply narration (with `You`) and the send-time
    /// `system_refinement`. See issue #80.
    SetAdeleOutput {
        conversation_id: String,
        level: AdeleOutput,
    },

    // --- Client-local tool calls (issue #76) ------------------------------
    /// The daemon suspended a turn on a client-local tool call and is waiting
    /// for this client to post the outcome (#107/#231). Carried verbatim from
    /// [`SignalEvent::ClientToolCall`]; the window must ALWAYS resolve it via
    /// `submit_client_tool_result` (even when it can't honour the tool) so the
    /// suspended turn never wedges. `say_this` is handled specially: spoken
    /// when the conversation's speech toggle is on, otherwise shown inline as
    /// `(speech mode disabled) …`. See issue #76.
    ClientToolCall {
        task_id: String,
        conversation_id: String,
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
}

// Manual `Debug` (retained from when a variant carried the non-`Debug`
// `Connector`; that handle is gone, but the explicit impl keeps test panic
// messages forwarding each variant's fields verbatim).
impl std::fmt::Debug for UiMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UiMessage::ConversationsLoaded(v) => {
                f.debug_tuple("ConversationsLoaded").field(v).finish()
            }
            UiMessage::ConversationListRefetched(v) => {
                f.debug_tuple("ConversationListRefetched").field(v).finish()
            }
            UiMessage::ConversationLoaded(v) => {
                f.debug_tuple("ConversationLoaded").field(v).finish()
            }
            UiMessage::ConversationReloaded(v) => {
                f.debug_tuple("ConversationReloaded").field(v).finish()
            }
            UiMessage::ConversationCreated { id } => f
                .debug_struct("ConversationCreated")
                .field("id", id)
                .finish(),
            UiMessage::ConversationDeleted { id } => f
                .debug_struct("ConversationDeleted")
                .field("id", id)
                .finish(),
            UiMessage::ConversationRenamed { id, title } => f
                .debug_struct("ConversationRenamed")
                .field("id", id)
                .field("title", title)
                .finish(),
            UiMessage::StreamChunk { request_id, chunk } => f
                .debug_struct("StreamChunk")
                .field("request_id", request_id)
                .field("chunk", chunk)
                .finish(),
            UiMessage::StreamComplete {
                request_id,
                full_response,
            } => f
                .debug_struct("StreamComplete")
                .field("request_id", request_id)
                .field("full_response", full_response)
                .finish(),
            UiMessage::StreamError { request_id, error } => f
                .debug_struct("StreamError")
                .field("request_id", request_id)
                .field("error", error)
                .finish(),
            UiMessage::AssistantStatus {
                request_id,
                message,
            } => f
                .debug_struct("AssistantStatus")
                .field("request_id", request_id)
                .field("message", message)
                .finish(),
            UiMessage::UserMessageAdded {
                conversation_id,
                request_id,
                content,
            } => f
                .debug_struct("UserMessageAdded")
                .field("conversation_id", conversation_id)
                .field("request_id", request_id)
                .field("content", content)
                .finish(),
            UiMessage::ContextUsage {
                conversation_id,
                used_tokens,
                budget_tokens,
                compaction_active,
            } => f
                .debug_struct("ContextUsage")
                .field("conversation_id", conversation_id)
                .field("used_tokens", used_tokens)
                .field("budget_tokens", budget_tokens)
                .field("compaction_active", compaction_active)
                .finish(),
            UiMessage::TitleChanged {
                conversation_id,
                title,
            } => f
                .debug_struct("TitleChanged")
                .field("conversation_id", conversation_id)
                .field("title", title)
                .finish(),
            UiMessage::ConversationListChanged { conversation_id } => f
                .debug_struct("ConversationListChanged")
                .field("conversation_id", conversation_id)
                .finish(),
            UiMessage::ConversationWarning {
                conversation_id,
                warning,
            } => f
                .debug_struct("ConversationWarning")
                .field("conversation_id", conversation_id)
                .field("warning", warning)
                .finish(),
            UiMessage::PromptSent {
                task_id,
                conversation_id,
            } => f
                .debug_struct("PromptSent")
                .field("task_id", task_id)
                .field("conversation_id", conversation_id)
                .finish(),
            UiMessage::ModelsLoaded(v) => f.debug_tuple("ModelsLoaded").field(v).finish(),
            UiMessage::DefaultModelLoaded(v) => {
                f.debug_tuple("DefaultModelLoaded").field(v).finish()
            }
            UiMessage::Connected { label } => {
                f.debug_struct("Connected").field("label", label).finish()
            }
            UiMessage::Disconnected { reason } => f
                .debug_struct("Disconnected")
                .field("reason", reason)
                .finish(),
            UiMessage::StatusUpdate(s) => f.debug_tuple("StatusUpdate").field(s).finish(),
            UiMessage::Error(s) => f.debug_tuple("Error").field(s).finish(),
            UiMessage::TasksLoaded(v) => f.debug_tuple("TasksLoaded").field(v).finish(),
            UiMessage::TaskStarted(v) => f.debug_tuple("TaskStarted").field(v).finish(),
            UiMessage::TaskProgress { id, progress_hint } => f
                .debug_struct("TaskProgress")
                .field("id", id)
                .field("progress_hint", progress_hint)
                .finish(),
            UiMessage::TaskLogAppended { id, entry } => f
                .debug_struct("TaskLogAppended")
                .field("id", id)
                .field("entry", entry)
                .finish(),
            UiMessage::TaskCompleted { id } => {
                f.debug_struct("TaskCompleted").field("id", id).finish()
            }
            UiMessage::ConversationScratchpadLoaded {
                conversation_id,
                notes,
            } => f
                .debug_struct("ConversationScratchpadLoaded")
                .field("conversation_id", conversation_id)
                .field("notes", notes)
                .finish(),
            UiMessage::ScratchpadChanged { conversation_id } => f
                .debug_struct("ScratchpadChanged")
                .field("conversation_id", conversation_id)
                .finish(),
            UiMessage::SetVoiceIn {
                conversation_id,
                enabled,
            } => f
                .debug_struct("SetVoiceIn")
                .field("conversation_id", conversation_id)
                .field("enabled", enabled)
                .finish(),
            UiMessage::SetAdeleOutput {
                conversation_id,
                level,
            } => f
                .debug_struct("SetAdeleOutput")
                .field("conversation_id", conversation_id)
                .field("level", level)
                .finish(),
            UiMessage::ClientToolCall {
                task_id,
                conversation_id,
                tool_call_id,
                tool_name,
                arguments,
            } => f
                .debug_struct("ClientToolCall")
                .field("task_id", task_id)
                .field("conversation_id", conversation_id)
                .field("tool_call_id", tool_call_id)
                .field("tool_name", tool_name)
                .field("arguments", arguments)
                .finish(),
        }
    }
}

/// The daemon sentinel meaning "inherit from the interactive purpose"; it
/// never appears for the *interactive* purpose itself, but we guard against it
/// (and empty fields) defensively so a malformed config degrades to "no
/// default" rather than pinning a non-resolvable model.
const PRIMARY_SENTINEL: &str = "primary";

/// Extract the interactive purpose's concrete `(connection, model)` as a
/// [`SelectedModel`]. Returns `None` when the interactive purpose is unset, has
/// an empty connection/model, or uses the `"primary"` inherit sentinel — any of
/// which means there's no concrete model to pin. Pure; unit-tested below.
///
/// Shared with `window.rs`, which re-resolves the default after Settings edits.
pub fn interactive_default_from_purposes(
    purposes: &api::PurposesView,
) -> Option<crate::selected_models::SelectedModel> {
    let cfg = purposes.interactive.as_ref()?;
    let is_resolvable = |field: &str| !field.is_empty() && field != PRIMARY_SENTINEL;
    if is_resolvable(&cfg.connection) && is_resolvable(&cfg.model) {
        Some(crate::selected_models::SelectedModel {
            connection_id: cfg.connection.clone(),
            model_id: cfg.model.clone(),
        })
    } else {
        None
    }
}

/// Translate a `SignalEvent` from `client-common` into the corresponding
/// `UiMessage` the GTK main thread consumes. Pure mapping; tested below.
pub fn signal_to_ui_message(signal: SignalEvent) -> UiMessage {
    match signal {
        // The streaming events now carry `conversation_id` (#352), but the
        // reducer routes them by the in-flight `request_id` it adopted from
        // `UserMessageAdded`/its own send, so the conversation id is redundant
        // here and deliberately dropped.
        SignalEvent::UserMessageAdded {
            conversation_id,
            request_id,
            content,
        } => UiMessage::UserMessageAdded {
            conversation_id,
            request_id,
            content,
        },
        SignalEvent::Chunk {
            request_id, chunk, ..
        } => UiMessage::StreamChunk { request_id, chunk },
        SignalEvent::Complete {
            request_id,
            full_response,
            ..
        } => UiMessage::StreamComplete {
            request_id,
            full_response,
        },
        SignalEvent::Error {
            request_id, error, ..
        } => UiMessage::StreamError { request_id, error },
        SignalEvent::Status {
            request_id,
            message,
            ..
        } => UiMessage::AssistantStatus {
            request_id,
            message,
        },
        SignalEvent::ContextUsage {
            conversation_id,
            request_id: _,
            used_tokens,
            budget_tokens,
            compaction_active,
        } => UiMessage::ContextUsage {
            conversation_id,
            used_tokens,
            budget_tokens,
            compaction_active,
        },
        SignalEvent::TitleChanged {
            conversation_id,
            title,
        } => UiMessage::TitleChanged {
            conversation_id,
            title,
        },
        // The user's conversation list changed on another connection (#1).
        // Carry it through to the reducer, which responds with a full list
        // re-fetch that repaints only the sidebar.
        SignalEvent::ConversationListChanged { conversation_id } => {
            UiMessage::ConversationListChanged { conversation_id }
        }
        SignalEvent::ConversationWarning {
            conversation_id,
            warning,
        } => UiMessage::ConversationWarning {
            conversation_id,
            warning,
        },
        SignalEvent::TaskStarted { task } => UiMessage::TaskStarted(task),
        SignalEvent::TaskProgress { id, progress_hint } => {
            UiMessage::TaskProgress { id, progress_hint }
        }
        SignalEvent::TaskLogAppended { id, entry } => UiMessage::TaskLogAppended { id, entry },
        SignalEvent::TaskCompleted { id, .. } => UiMessage::TaskCompleted { id },
        SignalEvent::ScratchpadChanged { conversation_id } => {
            UiMessage::ScratchpadChanged { conversation_id }
        }
        // Client-local tool execution (#107/#231/#76). The window must ALWAYS
        // resolve this (via `submit_client_tool_result`) or the suspended turn
        // wedges — the previous status-string mapping silently dropped it.
        // `say_this` is honoured (spoken or shown inline) per the conversation's
        // speech toggle; any other tool name is resolved with an error result so
        // the turn still completes. See issue #76.
        SignalEvent::ClientToolCall {
            task_id,
            conversation_id,
            tool_call_id,
            tool_name,
            arguments,
        } => UiMessage::ClientToolCall {
            task_id,
            conversation_id,
            tool_call_id,
            tool_name,
            arguments,
        },
        SignalEvent::Disconnected { reason } => UiMessage::Disconnected { reason },
    }
}
