//! Pure window state machine: the Elm-style reducer and its effects.
//!
//! `WindowState::apply(UiMessage) -> Vec<Effect>` is a pure decision function —
//! it mutates state and returns the side-effects to perform, but performs none
//! itself (no GTK, no widget refs, no spawns). The thin executor in the parent
//! module walks the returned effects against the real widgets. Keeping the whole
//! state machine here makes it (and its ~1,800 lines of tests) unit-testable
//! without a live GTK context.

use std::collections::HashMap;

use desktop_assistant_api_model as api;
use desktop_assistant_api_model::client::{ChatMessage, ConversationDetail, ConversationSummary};

use adele_voice_client_common::AdeleOutput;

use crate::message::UiMessage;

/// In-flight streaming-reply state — present (`Some`) exactly while a turn is
/// streaming, absent (`None`) otherwise.
///
/// Collapsing the former five free-standing fields (`pending_request_id`,
/// `pending_conversation_id`, `streaming_buffer`, `say_this_spoken_this_turn`,
/// `pending_turn_external`) into one optional record makes the invalid
/// in-between states unrepresentable: a partial buffer with no request slot, or
/// half-cleared pending fields, can no longer exist. A stream either exists with
/// all of its parts or does not exist at all.
#[derive(Debug, Clone, Default)]
struct StreamState {
    /// The daemon-assigned chunk-stream id once known, or `None` during the
    /// `__pending__` window (issue #31): the turn was acked and this stream slot
    /// reserved, but the real id only arrives inside the first `AssistantDelta`
    /// (or the echoed `UserMessageAdded`). The first matching frame claims it.
    /// `None` here is what `is_some()` on the old `pending_request_id` sentinel
    /// string expressed — there is no longer a magic `"__pending__"` value.
    request_id: Option<String>,
    /// The conversation this stream belongs to, captured **at send time** from
    /// `PromptSent` (GTK-2, "the stream knows its conversation"). Chunk
    /// rendering, completion, narration, and the chat status line are keyed off
    /// this — never off whichever conversation happens to be open when an event
    /// arrives.
    conversation_id: String,
    /// The accumulated reply text. Belongs to the originating conversation and
    /// re-seeds the view if the user switches back to it mid-stream.
    buffer: String,
    /// Set when a `say_this` aside is *spoken* for this turn; suppresses the
    /// duplicate full-reply narration at `StreamComplete` so the user doesn't
    /// hear the turn twice (the spoken aside, then the whole reply read aloud).
    /// Only relevant to gtk-initiated turns (the only ones gtk narrates).
    say_this_spoken_this_turn: bool,
    /// `true` when this turn was NOT initiated by this client — adopted from a
    /// `UserMessageAdded` for a turn started elsewhere (a voice turn, or another
    /// client on the same account) so its reply streams live into the open
    /// conversation (#1). Suppresses gtk's own reply narration for it: the
    /// originator (e.g. the voice daemon) already speaks the reply, so narrating
    /// again here would double-speak.
    external: bool,
}

/// Shared mutable state for the window.
#[derive(Default)]
pub struct WindowState {
    pub conversations: Vec<ConversationSummary>,
    pub current_conversation_id: Option<String>,
    pub current_conversation: Option<ConversationDetail>,
    /// In-flight streaming reply, or `None` when no turn is streaming. See
    /// [`StreamState`]: collapsing the former five `pending_*`/`streaming_buffer`
    /// fields into one optional record makes the half-set intermediate states
    /// unrepresentable, and ties every stream event to its originating
    /// conversation (GTK-2) by construction.
    stream: Option<StreamState>,
    pub debug_enabled: bool,
    /// Per-conversation `You:` (voice input) state (issue #80), keyed by
    /// conversation id. Default (absent key) is **Disabled** (type only). When
    /// Enabled, the input bar shows a push-to-talk control and — combined with
    /// `Adele == OnDemand` — gates reply narration. Per-conversation, so
    /// enabling it in one conversation never affects another.
    conversation_voice_in: HashMap<String, bool>,
    /// Per-conversation `Adele:` (voice output) level (issue #80), keyed by
    /// conversation id. Default (absent key) is **Disabled** (never speaks).
    /// Set by the user (the dropdown) or the model (`request_voice` → OnDemand,
    /// `stop_voice` → Disabled). Decides reply narration (with `You`), the
    /// `say_this` gate, and the send-time `system_refinement`. Replaces phase-2's
    /// two toggles (read-aloud == Always, voice-mode == OnDemand).
    conversation_adele_output: HashMap<String, AdeleOutput>,
}

impl WindowState {
    /// Whether `You:` (voice input) is Enabled for `conversation` (issue #80).
    /// `false` when it was never set (default Disabled). Part of the shared
    /// public API: clients render per-conversation voice state from it.
    pub fn voice_in_for(&self, conversation: &str) -> bool {
        self.conversation_voice_in
            .get(conversation)
            .copied()
            .unwrap_or(false)
    }

    /// Whether `You:` (voice input) is Enabled for the *currently active*
    /// conversation. `false` when there is no active conversation or it was
    /// never set (default Disabled).
    pub fn voice_in_for_current(&self) -> bool {
        self.current_conversation_id
            .as_deref()
            .map(|id| self.voice_in_for(id))
            .unwrap_or(false)
    }

    /// The `Adele:` (voice output) level for `conversation` (issue #80).
    /// `Disabled` when it was never set (default). Part of the shared public
    /// API: clients render per-conversation voice state from it.
    pub fn adele_output_for(&self, conversation: &str) -> AdeleOutput {
        self.conversation_adele_output
            .get(conversation)
            .copied()
            .unwrap_or_default()
    }

    /// The `Adele:` (voice output) level for the *currently active*
    /// conversation. `Disabled` when there is no active conversation or it was
    /// never set (default Disabled).
    pub fn adele_output_for_current(&self) -> AdeleOutput {
        self.current_conversation_id
            .as_deref()
            .map(|id| self.adele_output_for(id))
            .unwrap_or_default()
    }

    /// Whether a *reply* is spoken for `conversation` (issue #80): `Adele ==
    /// Always` OR (`Adele == OnDemand` AND `You == Enabled`). The gate the
    /// reply-narration path consults — keyed by the *originating*
    /// conversation (GTK-2); `Disabled` never narrates. Delegates to the shared
    /// gate (desktop-assistant#274). Part of the shared public API.
    pub fn narrate_for(&self, conversation: &str) -> bool {
        self.adele_output_for(conversation)
            .narrates_reply(self.voice_in_for(conversation))
    }

    /// Whether a *reply* is spoken for the *currently active* conversation —
    /// `narrate_for` keyed by the open conversation. `false` with none open.
    /// Test-only convenience for the gate tests; the production narration path
    /// keys off the originating conversation (GTK-2) via `narrate_for`.
    #[cfg(test)]
    fn narrate_for_current(&self) -> bool {
        self.current_conversation_id
            .as_deref()
            .map(|id| self.narrate_for(id))
            .unwrap_or(false)
    }

    /// Whether a `say_this` aside is spoken for `conversation` (issue #80):
    /// spoken iff `Adele ∈ {OnDemand, Always}` (independent of `You`) — keyed
    /// by the *call's* conversation (GTK-4). `Disabled` downgrades the aside
    /// to inline text. Delegates to the shared gate (desktop-assistant#274).
    /// Part of the shared public API.
    pub fn say_this_spoken_for(&self, conversation: &str) -> bool {
        self.adele_output_for(conversation).speaks_aside()
    }

    /// Whether a `say_this` aside is spoken for the *currently active*
    /// conversation — `say_this_spoken_for` keyed by the open conversation.
    /// Test-only convenience; the production path keys off the call's
    /// conversation (GTK-4) via `say_this_spoken_for`.
    #[cfg(test)]
    fn say_this_spoken_for_current(&self) -> bool {
        self.current_conversation_id
            .as_deref()
            .map(|id| self.say_this_spoken_for(id))
            .unwrap_or(false)
    }

    /// Whether `conversation` is the one currently open in the chat view.
    fn is_active_conversation(&self, conversation: &str) -> bool {
        self.current_conversation_id.as_deref() == Some(conversation)
    }

    /// Whether the in-flight stream (if any) belongs to the conversation
    /// currently open in the chat view (GTK-2). With no recorded originating
    /// conversation (legacy/defensive) the stream is treated as active,
    /// preserving the pre-GTK-2 behavior.
    fn pending_stream_is_active(&self) -> bool {
        match &self.stream {
            Some(stream) => self.is_active_conversation(&stream.conversation_id),
            None => true,
        }
    }

    /// Whether `request_id` is the stream this state is rendering — the claimed
    /// id, or *any* id while still in the `__pending__` window (`request_id` not
    /// yet claimed; the first frame claims it). `false` when no stream is in
    /// flight. Mirrors the old `pending_request_id == Some(id) ||
    /// pending_request_id == Some("__pending__")`.
    fn stream_matches(&self, request_id: &str) -> bool {
        match &self.stream {
            Some(s) => s.request_id.is_none() || s.request_id.as_deref() == Some(request_id),
            None => false,
        }
    }

    /// The accumulated text of the in-flight streaming reply, or empty when no
    /// stream is buffering. Read-only accessor for view clients — the TUI
    /// renders the partial reply from it; the field stays private so only
    /// `apply` mutates it. Part of the shared public API.
    pub fn streaming_buffer(&self) -> &str {
        self.stream.as_ref().map_or("", |s| s.buffer.as_str())
    }

    /// Whether a streamed reply is currently in flight (the `request_id` slot is
    /// occupied). A view's submit path gates on this so a second prompt can't be
    /// sent while a turn streams. Part of the shared public API.
    pub fn is_streaming(&self) -> bool {
        self.stream.is_some()
    }

    /// Whether the in-flight stream (if any) belongs to the conversation
    /// currently in view — the render guard a view consults before painting the
    /// streaming buffer, so a backgrounded turn's chunks never bleed into a
    /// conversation the user switched to (GTK-2). Public wrapper over the private
    /// `pending_stream_is_active`. Part of the shared public API.
    pub fn streaming_is_active_for_view(&self) -> bool {
        self.pending_stream_is_active()
    }

    /// Drop all in-flight streaming state *without* finalizing it — the
    /// connection-teardown path (TUI-8). Unlike the [`UiMessage::Disconnected`]
    /// reducer arm (which appends a `[Connection lost]` stub to the originating
    /// conversation before clearing), this simply discards the partial: the link
    /// died, so the buffer must not linger as a frozen partial and the ack
    /// sentinel must not mis-claim the first post-reconnect stream. Part of the
    /// shared public API for view clients that own their connection lifecycle
    /// outside the reducer (the TUI drives reconnect from its run loop, not from
    /// a `Disconnected` message).
    pub fn reset_streaming_state(&mut self) {
        self.stream = None;
    }
}

/// The system refinement to attach on the next send, or `None` (issue #80),
/// chosen by the active conversation's `Adele:` level: `OnDemand` →
/// brief/conversational/speakable; `Always` → speakable-but-full (don't
/// shorten); `Disabled` → none. Pure decision the send path consults to choose
/// `send_prompt_with_system_refinement`. Free function (not a method) so the
/// send closure can call it through a snapshot without holding a `WindowState`
/// borrow across the await. Delegates to the shared per-level refinement
/// (desktop-assistant#274).
pub fn refinement_for_send(state: &WindowState) -> Option<&'static str> {
    state.adele_output_for_current().send_refinement()
}

/// The session-scoped client tools this client advertises so the model can
/// enter/leave spoken voice mode (issue #78). Both take no arguments. Registered
/// on connect; the daemon replaces the prior set on each call, so this is the
/// full list, not a delta. (Phase-1's `say_this` is handled defensively without
/// registration — the daemon forwards it regardless — so it is intentionally
/// not advertised here.)
pub fn voice_mode_client_tools() -> Vec<api::ClientToolRegistration> {
    let no_args = serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    });
    vec![
        api::ClientToolRegistration {
            name: "request_voice".to_string(),
            description: "Switch this conversation into spoken voice mode (the user asked to talk \
                 by voice); replies will be read aloud and kept conversational."
                .to_string(),
            input_schema: no_args.clone(),
        },
        api::ClientToolRegistration {
            name: "stop_voice".to_string(),
            description: "Leave voice mode; go back to text-only.".to_string(),
            input_schema: no_args,
        },
    ]
}

/// Extract the `text` argument from a `say_this` client-tool call (issue #76).
///
/// Returns `None` (rather than panicking) when `arguments` is not an object,
/// the `text` field is absent, or it isn't a string — a hostile or buggy
/// payload must resolve to an `Err` result, never crash the turn. An empty
/// string is accepted (the LLM asked to say nothing; the result still resolves).
fn say_this_text(arguments: &serde_json::Value) -> Option<String> {
    arguments
        .get("text")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// A single observable side-effect produced by [`WindowState::apply`].
///
/// `apply` is a pure decision function: it mutates `WindowState` and returns
/// the list of effects to perform, but performs none of them itself (no GTK,
/// no widget refs, no spawns). The thin executor in [`handle_ui_message`]
/// walks the returned `Vec<Effect>` in order and performs each against the
/// real widgets — mirroring the `TasksModel`/`apply` shape already used by
/// `widgets/tasks_panel.rs`. This keeps the entire state-machine decision
/// logic unit-testable without a live GTK context.
///
/// Effects are emitted in the exact order the legacy `handle_ui_message`
/// performed them, so the observable behavior is identical.
pub enum Effect {
    /// Clear the client cell (on disconnect). There is no `SetClient`
    /// counterpart: per this crate's design rule the reducer holds no transport
    /// handle, so the client installs its own connector when it connects; the
    /// reducer only signals teardown, which it drives from a `Disconnected`
    /// signal.
    ClearClient,
    /// Set the bottom status-bar text verbatim.
    SetStatusText(String),
    /// Enable/disable the send button.
    SetSendSensitive(bool),
    /// Repaint the sidebar conversation list.
    SetConversations(Vec<ConversationSummary>),
    /// Run `ensure_active_conversation` (selection sync + auto-load/-create).
    /// Kept as an effect because it needs the live client + ui_tx and spawns
    /// async RPCs; the *decision* to run it lives in `apply`.
    EnsureActiveConversation,
    /// Load an (already debug-filtered) conversation into the chat view.
    LoadConversationIntoChat(ConversationDetail),
    /// Re-fetch a conversation that is already open, to refresh the cached
    /// detail + chat after a reconnect (or a debug/personality refresh) WITHOUT
    /// resetting the model picker. The reply arrives as
    /// `UiMessage::ConversationReloaded`. Unlike a conversation *switch* (which
    /// flows through `ConversationLoaded` and re-applies the picker selection),
    /// a reload must never clobber the user's pick — see issue #72.
    ReloadConversation(String),
    /// Fetch a conversation as a *fresh switch*: the reply arrives as
    /// `UiMessage::ConversationLoaded`, which applies the model picker selection.
    /// Used when the active conversation has no cached detail yet (e.g. a
    /// just-created conversation) so a single fetch both loads it and sets the
    /// picker — replacing the old new-conversation flow's redundant second fetch
    /// (GTK-10).
    LoadConversation(String),
    /// Re-fetch the conversation list from the daemon, then deliver it as
    /// [`UiMessage::ConversationListRefetched`] — a *list-only* refresh used when
    /// the list changed on another connection (#1). Kept as an effect because it
    /// needs the live client + ui_tx and spawns an async RPC; the decision to run
    /// it lives in `apply`. The result repaints only the sidebar (it does NOT
    /// reload the open conversation or touch the model picker), distinguishing it
    /// from the connect-time `list_conversations -> ConversationsLoaded` path.
    RefetchConversationList,
    /// Clear the chat view.
    ClearChat,
    /// Set the chat's transient status line.
    SetChatStatus(String),
    /// Clear the chat's transient status line.
    ClearChatStatus,
    /// Update the read-only context-window fill indicator (#341). `None`
    /// clears it (no reading for the open conversation).
    SetContextUsage(Option<crate::context_usage::ContextUsageView>),
    /// Append a user-message bubble to the chat view. Used to render the user's
    /// prompt for a turn this client did not initiate (#1) — the local send path
    /// draws its own bubble optimistically and does not go through this effect.
    AddUserMessage(String),
    /// Append a streaming chunk to the chat view.
    ReceiveChunk(String),
    /// Finalize a streaming response in the chat view.
    CompleteStreaming(String),
    /// Run the actual send-prompt RPC for an accepted [`UiMessage::SubmitPrompt`].
    /// The reducer has already drawn the user's bubble optimistically and gated
    /// the send; the client's executor only performs the transport call (folding
    /// in the staged model override it owns) and feeds the ack back as
    /// [`UiMessage::PromptSent`] — or [`UiMessage::SendFailed`] on error.
    /// `system_refinement` is the voice-derived per-turn system-prompt shaping
    /// for the open conversation's `Adele:` level (`None` = no refinement).
    SendPrompt {
        conversation_id: String,
        prompt: String,
        system_refinement: Option<String>,
    },
    /// Apply (or clear, with `None`) the model-picker selection.
    SetModelSelection(Option<api::ConversationModelSelectionView>),
    /// Replace the model-picker's available models.
    SetModels(Vec<api::ModelListing>),
    /// Set the picker's resolved interactive-purpose default (issue #53). Used
    /// as the fallback selection for conversations with no stored selection so
    /// the button shows a concrete model instead of "(default)".
    SetDefaultModel(Option<crate::selected_models::SelectedModel>),
    /// Show/hide the model picker.
    SetModelPickerVisible(bool),
    /// Reveal a passive toast with the given message.
    ShowToast(String),
    /// Replace the entire background-task list.
    TasksReplaceAll(Vec<api::TaskView>),
    /// A task started.
    TaskStarted(api::TaskView),
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
    /// A task completed (terminal).
    TaskCompleted { id: String },

    // --- Live multi-client conversation sync (issue #1) -------------------
    /// Tell the daemon which conversations this connection is viewing so it
    /// fans their turn events (`UserMessageAdded`/`AssistantDelta`/
    /// `AssistantCompleted`/`AssistantError`/`AssistantStatus`) to us —
    /// including turns started by another client or the voice daemon. Sends
    /// `api::Command::SubscribeConversations`, which is set-replace: the WHOLE
    /// viewed set each time it changes (today just the single active
    /// conversation; a future tabs feature passes several). Emitted when the
    /// active conversation is loaded/switched, and re-sent on reconnect.
    SubscribeConversations(Vec<String>),

    // --- Conversation side pane (issue #60) -------------------------------
    /// Fetch the scratchpad for the given conversation (async RPC + ui_tx),
    /// mirroring `EnsureActiveConversation`. The reply arrives as
    /// `UiMessage::ConversationScratchpadLoaded`.
    FetchScratchpad(String),
    /// Replace the side pane's scratchpad notes (empty clears it).
    SidePaneSetScratchpad(Vec<api::ScratchpadNoteView>),
    /// Recompute the side pane's task list from the authoritative `TasksModel`,
    /// filtered to the active conversation.
    RefreshSidePaneTasks,

    // --- Speech toggle + client tools (issue #76) -------------------------
    /// Speak `text` through the embedded `Speaker`. Emitted only when the
    /// active conversation's speech toggle is ON (the executor still no-ops if
    /// there is no embedded engine, e.g. the daemon path). The master audio
    /// cut-off lives in `apply`: when speech is OFF this effect is never
    /// produced, so no path plays audio while the toggle is off.
    Speak(String),
    /// Render an inline note in the chat transcript (issue #76). Used for the
    /// `(speech mode disabled) …` downgrade when `say_this` arrives with speech
    /// OFF, so the text is shown rather than dropped.
    AddInlineNote(String),
    /// Reflect the active conversation's `Adele:` output level on the input-bar
    /// dropdown (issue #80). Emitted when the model drives the level via
    /// `request_voice` (→ OnDemand) / `stop_voice` (→ Disabled) so the dropdown
    /// tracks the model's change (the user-driven path needs no echo — the
    /// dropdown is its own write source). Suppressed inside
    /// `set_adele_output_active`, so it never loops.
    SetAdeleOutputDropdown(AdeleOutput),
    /// Resolve a suspended client-tool call back to the daemon via
    /// `submit_client_tool_result` so the parked turn resumes (issue #76). Every
    /// `ClientToolCall` yields exactly one of these — `Ok` on success, `Err`
    /// with a reason otherwise — which is what kills the silent-drop wedge.
    SubmitClientToolResult {
        task_id: String,
        tool_call_id: String,
        result: Result<String, String>,
    },
}

// Manual `Debug` (retained from when `Effect::SetClient` carried the
// non-`Debug` `Connector`; that variant is gone, but the explicit impl keeps
// test panic messages forwarding each variant's fields verbatim).
impl std::fmt::Debug for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effect::ClearClient => f.write_str("ClearClient"),
            Effect::SetStatusText(t) => f.debug_tuple("SetStatusText").field(t).finish(),
            Effect::SetSendSensitive(b) => f.debug_tuple("SetSendSensitive").field(b).finish(),
            Effect::SetConversations(c) => f.debug_tuple("SetConversations").field(c).finish(),
            Effect::EnsureActiveConversation => f.write_str("EnsureActiveConversation"),
            Effect::LoadConversationIntoChat(d) => {
                f.debug_tuple("LoadConversationIntoChat").field(d).finish()
            }
            Effect::ReloadConversation(id) => {
                f.debug_tuple("ReloadConversation").field(id).finish()
            }
            Effect::LoadConversation(id) => f.debug_tuple("LoadConversation").field(id).finish(),
            Effect::RefetchConversationList => f.write_str("RefetchConversationList"),
            Effect::ClearChat => f.write_str("ClearChat"),
            Effect::SetChatStatus(m) => f.debug_tuple("SetChatStatus").field(m).finish(),
            Effect::ClearChatStatus => f.write_str("ClearChatStatus"),
            Effect::SetContextUsage(u) => f.debug_tuple("SetContextUsage").field(u).finish(),
            Effect::AddUserMessage(c) => f.debug_tuple("AddUserMessage").field(c).finish(),
            Effect::ReceiveChunk(c) => f.debug_tuple("ReceiveChunk").field(c).finish(),
            Effect::CompleteStreaming(c) => f.debug_tuple("CompleteStreaming").field(c).finish(),
            Effect::SendPrompt {
                conversation_id,
                prompt,
                system_refinement,
            } => f
                .debug_struct("SendPrompt")
                .field("conversation_id", conversation_id)
                .field("prompt", prompt)
                .field("system_refinement", system_refinement)
                .finish(),
            Effect::SetModelSelection(s) => f.debug_tuple("SetModelSelection").field(s).finish(),
            Effect::SetModels(m) => f.debug_tuple("SetModels").field(m).finish(),
            Effect::SetDefaultModel(m) => f.debug_tuple("SetDefaultModel").field(m).finish(),
            Effect::SetModelPickerVisible(v) => {
                f.debug_tuple("SetModelPickerVisible").field(v).finish()
            }
            Effect::ShowToast(m) => f.debug_tuple("ShowToast").field(m).finish(),
            Effect::TasksReplaceAll(t) => f.debug_tuple("TasksReplaceAll").field(t).finish(),
            Effect::TaskStarted(t) => f.debug_tuple("TaskStarted").field(t).finish(),
            Effect::TaskProgress { id, progress_hint } => f
                .debug_struct("TaskProgress")
                .field("id", id)
                .field("progress_hint", progress_hint)
                .finish(),
            Effect::TaskLogAppended { id, entry } => f
                .debug_struct("TaskLogAppended")
                .field("id", id)
                .field("entry", entry)
                .finish(),
            Effect::TaskCompleted { id } => {
                f.debug_struct("TaskCompleted").field("id", id).finish()
            }
            Effect::SubscribeConversations(ids) => {
                f.debug_tuple("SubscribeConversations").field(ids).finish()
            }
            Effect::FetchScratchpad(c) => f.debug_tuple("FetchScratchpad").field(c).finish(),
            Effect::SidePaneSetScratchpad(n) => {
                f.debug_tuple("SidePaneSetScratchpad").field(n).finish()
            }
            Effect::RefreshSidePaneTasks => f.write_str("RefreshSidePaneTasks"),
            Effect::Speak(t) => f.debug_tuple("Speak").field(t).finish(),
            Effect::AddInlineNote(t) => f.debug_tuple("AddInlineNote").field(t).finish(),
            Effect::SetAdeleOutputDropdown(l) => {
                f.debug_tuple("SetAdeleOutputDropdown").field(l).finish()
            }
            Effect::SubmitClientToolResult {
                task_id,
                tool_call_id,
                result,
            } => f
                .debug_struct("SubmitClientToolResult")
                .field("task_id", task_id)
                .field("tool_call_id", tool_call_id)
                .field("result", result)
                .finish(),
        }
    }
}

impl WindowState {
    /// Apply a `UiMessage` to the window state, returning the side-effects to
    /// perform. PURE: mutates `self` and returns effects; performs no GTK
    /// work and holds no widget refs.
    ///
    /// Every `UiMessage` variant is handled here; the executor in
    /// `handle_ui_message` is a mechanical translation of the returned
    /// effects into widget calls.
    pub fn apply(&mut self, msg: UiMessage) -> Vec<Effect> {
        match msg {
            UiMessage::ConversationsLoaded(convs) => {
                self.conversations = convs.clone();
                let mut effects = vec![
                    Effect::SetConversations(convs),
                    Effect::EnsureActiveConversation,
                ];
                // The window already has an active conversation that is still
                // present (reconnect, or a just-created conversation whose
                // `ConversationCreated` set the active id). `EnsureActiveConversation`
                // only re-syncs the sidebar selection in that case — it does not
                // reload the messages — so fetch the conversation here:
                //
                // - detail already cached (a true reconnect refresh): use
                //   `ReloadConversation`, which keeps the model picker intact
                //   (issue #72).
                // - detail NOT cached (a freshly-created conversation): use a
                //   single `LoadConversation`, which arrives as
                //   `ConversationLoaded` and sets the picker. This replaces the
                //   old new-conversation flow that fetched twice — once
                //   explicitly and once via this reload (GTK-10).
                //
                // On the very first connect there is no active conversation yet,
                // so the initial load still happens through
                // `EnsureActiveConversation -> ConversationLoaded`.
                if let Some(id) = self.current_conversation_id.clone()
                    && self.conversations.iter().any(|c| c.id == id)
                {
                    // Re-establish the daemon's turn-event subscription for the
                    // open conversation on (re)connect (#1). A reconnect's
                    // refresh flows through `ReloadConversation` →
                    // `ConversationReloaded` (which keeps the model picker, #72)
                    // and so does NOT pass through `ConversationLoaded`, where
                    // the switch-time subscribe lives — so subscribe here too,
                    // covering both the cached-detail reconnect and the
                    // not-yet-cached path before its `ConversationLoaded` lands.
                    effects.push(Effect::SubscribeConversations(vec![id.clone()]));
                    let detail_cached = self
                        .current_conversation
                        .as_ref()
                        .is_some_and(|c| c.id == id);
                    if detail_cached {
                        effects.push(Effect::ReloadConversation(id));
                    } else {
                        effects.push(Effect::LoadConversation(id));
                    }
                }
                effects
            }
            UiMessage::ConversationLoaded(detail) => {
                let id = detail.id.clone();
                let filtered = filter_messages(&detail, self.debug_enabled);
                let selection = detail.model_selection.clone();
                self.current_conversation = Some(detail);
                self.current_conversation_id = Some(id.clone());
                let mut effects = vec![
                    Effect::SetModelSelection(selection),
                    Effect::LoadConversationIntoChat(filtered),
                    // Drop any stale context-fill reading from the previous
                    // conversation; the next turn re-establishes it (#341).
                    Effect::SetContextUsage(None),
                    // Subscribe the daemon to this (now-active) conversation so
                    // its turn events — including ones started by another client
                    // or the voice daemon — fan to us for live render (#1). The
                    // set is replaced wholesale, so passing just the active
                    // conversation also drops the previously-viewed one.
                    Effect::SubscribeConversations(vec![id.clone()]),
                    // Rebind the side pane to the new conversation: clear stale
                    // notes until the fetch returns, refresh the filtered task
                    // list, and fetch this conversation's scratchpad.
                    Effect::SidePaneSetScratchpad(Vec::new()),
                    Effect::RefreshSidePaneTasks,
                    Effect::FetchScratchpad(id),
                ];
                // A stream may still be in flight for another (or this)
                // conversation (GTK-2). Deliberately do NOT clear the pending
                // stream here — it keeps buffering for its originating
                // conversation — but reconcile the view:
                if self.stream.is_some() {
                    if self.pending_stream_is_active() {
                        // Switched (back) to the streaming conversation: the
                        // fresh load wiped the partial reply from the view, so
                        // re-seed the buffered prefix.
                        if !self.streaming_buffer().is_empty() {
                            effects.push(Effect::ReceiveChunk(self.streaming_buffer().to_string()));
                        }
                    } else {
                        // Switched away: the streaming turn's status line
                        // belongs to the old conversation and must not linger.
                        effects.push(Effect::ClearChatStatus);
                    }
                }
                effects
            }
            UiMessage::ConversationReloaded(detail) => {
                // A conversation already open was re-fetched (reconnect / debug /
                // personality refresh). Refresh the cached detail + chat (and
                // side pane) but deliberately do NOT emit `SetModelSelection`:
                // the model picker must keep the user's current selection across
                // a reconnect (issue #72). Drop the reply if the user switched
                // conversations while the fetch was in flight.
                if self.current_conversation_id.as_deref() != Some(detail.id.as_str()) {
                    vec![]
                } else {
                    let id = detail.id.clone();
                    let filtered = filter_messages(&detail, self.debug_enabled);
                    self.current_conversation = Some(detail);
                    vec![
                        Effect::LoadConversationIntoChat(filtered),
                        Effect::SidePaneSetScratchpad(Vec::new()),
                        Effect::RefreshSidePaneTasks,
                        Effect::FetchScratchpad(id),
                    ]
                }
            }
            UiMessage::ConversationCreated { id } => {
                self.current_conversation_id = Some(id);
                vec![]
            }
            UiMessage::ConversationDeleted { id } => {
                self.conversations.retain(|c| c.id != id);
                // Prune the deleted conversation's per-conversation voice state
                // (GTK-9): otherwise the maps grow unbounded and a later id
                // reuse could inherit a stale `You:`/`Adele:` setting.
                self.conversation_voice_in.remove(&id);
                self.conversation_adele_output.remove(&id);
                let is_active = self.current_conversation_id.as_deref() == Some(&id);
                if is_active {
                    self.current_conversation_id = None;
                    self.current_conversation = None;
                }
                let convs = self.conversations.clone();
                let mut effects = vec![Effect::SetConversations(convs)];
                if is_active {
                    effects.push(Effect::ClearChat);
                    effects.push(Effect::SidePaneSetScratchpad(Vec::new()));
                    effects.push(Effect::RefreshSidePaneTasks);
                    effects.push(Effect::EnsureActiveConversation);
                }
                effects
            }
            UiMessage::ConversationRenamed { id, title } => {
                for conv in &mut self.conversations {
                    if conv.id == id {
                        conv.title = title.clone();
                    }
                }
                vec![Effect::SetConversations(self.conversations.clone())]
            }
            UiMessage::ConversationListChanged { conversation_id: _ } => {
                // The user's list changed on another connection — a conversation
                // was created/renamed/deleted/(un)archived by another client or
                // the voice daemon (#1). The signal carries only the affected id;
                // rather than patch a single row, re-fetch the whole list (correct
                // for every change kind). The reply lands as
                // `ConversationListRefetched`, which repaints ONLY the sidebar —
                // so this never disturbs the open conversation or the model picker.
                vec![Effect::RefetchConversationList]
            }
            UiMessage::ConversationListRefetched(convs) => {
                // The list-only refresh requested by `ConversationListChanged`.
                // Store the fresh list and repaint the sidebar; re-sync the
                // selection via `EnsureActiveConversation` (a no-op beyond
                // re-selecting the active row when it is still present — see
                // `ensure_active_conversation`). Deliberately NO
                // `ReloadConversation`/`LoadConversation`: the open conversation's
                // chat and the model picker must stay exactly as the user left
                // them. If the open conversation was the one deleted elsewhere,
                // it is now absent from the list and `EnsureActiveConversation`
                // falls back to the first conversation (or creates one), which is
                // the right thing to show.
                self.conversations = convs.clone();
                vec![
                    Effect::SetConversations(convs),
                    Effect::EnsureActiveConversation,
                ]
            }
            UiMessage::SubmitPrompt { prompt } => {
                // Single send-decision point (Phase-2): gate, draw the user's
                // bubble optimistically, choose the per-turn refinement, and emit
                // the RPC effect. The connection gate + the staged model override
                // stay client-side (transport concerns the core doesn't own).
                //
                // Block a second send while a reply is still in flight (TUI-7):
                // the single streaming buffer renders one turn at a time, so
                // interleaving would cross-wire the request-id claim. The composer
                // text is preserved client-side; surface why.
                if self.is_streaming() {
                    return vec![Effect::SetStatusText(
                        "A reply is still streaming — wait for it to finish (your text is \
                         preserved)"
                            .to_string(),
                    )];
                }
                // Nothing to send, or no open conversation to send into: ignore
                // silently (the composer keeps its text; the action is gated
                // upstream too, so this is a belt-and-braces no-op).
                if prompt.is_empty() {
                    return vec![];
                }
                let Some(conversation_id) = self.current_conversation_id.clone() else {
                    return vec![];
                };
                // Optimistic local echo of our own send (#1): draw the user bubble
                // now so the turn feels instant. The daemon assigns the real id
                // when it persists the turn; the echoed-back `UserMessageAdded` is
                // de-duped by request_id, so an empty id here is correct.
                if let Some(conv) = self.current_conversation.as_mut() {
                    conv.messages.push(ChatMessage {
                        id: String::new(),
                        role: "user".to_string(),
                        content: prompt.clone(),
                    });
                }
                // Per the conversation's `Adele:` level (#80) carry a system
                // refinement so the reply is shaped for speech (OnDemand → brief;
                // Always → speakable but full; Disabled → none). Decided here so
                // the whole send decision is one tested place.
                let system_refinement = refinement_for_send(self).map(str::to_string);
                vec![Effect::SendPrompt {
                    conversation_id,
                    prompt,
                    system_refinement,
                }]
            }
            UiMessage::SendFailed {
                conversation_id,
                prompt,
            } => {
                // The send RPC failed (TUI-2): roll the optimistic user bubble
                // back out, but only when it is still the tail of the conversation
                // it was added to — the user may have switched conversations, or
                // another message (e.g. an inline note) may have landed after it.
                // The client refills its composer and surfaces the error.
                if let Some(conv) = self.current_conversation.as_mut()
                    && conv.id == conversation_id
                    && conv
                        .messages
                        .last()
                        .is_some_and(|m| m.role == "user" && m.content == prompt)
                {
                    conv.messages.pop();
                }
                vec![]
            }
            UiMessage::PromptSent {
                task_id: _,
                conversation_id,
            } => {
                // The wire ack carries either a `task_id` (post-#114
                // `SendMessageAck`) or an empty string (legacy `Ack`). Neither
                // is the chunk-stream `request_id` — that is daemon-generated and
                // arrives inside the first `AssistantDelta` (see issue #31). Open
                // the stream with `request_id: None` (the `__pending__` window);
                // the first frame claims the real id. Tie it to its conversation
                // as captured at send time (GTK-2): every later event is judged
                // against this id, not against whatever conversation is open when
                // it arrives. This client initiated the turn, so it owns reply
                // narration (`external: false`) and no aside has been spoken yet.
                self.stream = Some(StreamState {
                    request_id: None,
                    conversation_id,
                    buffer: String::new(),
                    say_this_spoken_this_turn: false,
                    external: false,
                });
                vec![]
            }
            UiMessage::UserMessageAdded {
                conversation_id,
                request_id,
                content,
            } => {
                // Case 1 — this client's own send, echoed back (#1). We drew the
                // user bubble optimistically at send time and set "__pending__";
                // claim the real request_id now (it precedes the first chunk) and
                // render nothing more. This also resolves the stream's request_id
                // earlier and more reliably than the claim-on-first-chunk fallback.
                if let Some(stream) = &mut self.stream
                    && stream.request_id.is_none()
                    && stream.conversation_id == conversation_id
                {
                    stream.request_id = Some(request_id);
                    return vec![];
                }
                // Case 2 — a turn this client did NOT initiate (a voice turn, or
                // another client on the same account) for the conversation in
                // view, with no gtk turn already occupying the single in-flight
                // slot. Adopt it into the pending slot so the existing
                // chunk/completion path streams the reply live, and draw the
                // user's bubble now. Marked external so its reply is NOT narrated
                // here — the originator (e.g. the voice daemon) already speaks it.
                // A turn for a background conversation, or one arriving while our
                // own turn is in flight, is left to the reload-on-switch path (the
                // daemon persists it).
                if self.stream.is_none() && self.is_active_conversation(&conversation_id) {
                    self.stream = Some(StreamState {
                        request_id: Some(request_id),
                        conversation_id,
                        buffer: String::new(),
                        say_this_spoken_this_turn: false,
                        external: true,
                    });
                    if let Some(ref mut conv) = self.current_conversation {
                        conv.messages.push(ChatMessage {
                            // Locally-adopted external turn: no server id yet
                            // (the event carries none). Empty is the sanctioned
                            // placeholder for a message the daemon hasn't keyed;
                            // the next reload swaps in the authoritative copy.
                            id: String::new(),
                            role: "user".to_string(),
                            content: content.clone(),
                        });
                    }
                    return vec![Effect::AddUserMessage(content)];
                }
                vec![]
            }
            UiMessage::AssistantStatus {
                request_id,
                message,
            } => {
                // Show only for the in-flight stream AND only while its
                // originating conversation is the one in view (GTK-2): a
                // background turn's status must not paint over another
                // conversation's chat.
                if self.stream_matches(&request_id) && self.pending_stream_is_active() {
                    vec![Effect::SetChatStatus(message)]
                } else {
                    vec![]
                }
            }
            UiMessage::ContextUsage {
                conversation_id,
                used_tokens,
                budget_tokens,
                compaction_active,
            } => {
                // Only paint the fill indicator for the conversation in view
                // (#341): a background turn's reading must not mislead the user
                // about the conversation they are looking at.
                if self.is_active_conversation(&conversation_id) {
                    vec![Effect::SetContextUsage(Some(
                        crate::context_usage::ContextUsageView {
                            used_tokens,
                            budget_tokens,
                            compaction_active,
                        },
                    ))]
                } else {
                    vec![]
                }
            }
            UiMessage::StreamChunk { request_id, chunk } => {
                let Some(stream) = &mut self.stream else {
                    return vec![];
                };
                // Claim the real id on the first frame of a `__pending__` stream.
                if stream.request_id.is_none() {
                    stream.request_id = Some(request_id.clone());
                }
                if stream.request_id.as_deref() != Some(&request_id) {
                    return vec![];
                }
                let first_chunk = stream.buffer.is_empty();
                // Always accumulate — the buffer belongs to the stream's
                // originating conversation (GTK-2) and is what re-seeds the view
                // if the user switches back mid-stream...
                stream.buffer.push_str(&chunk);
                let origin = stream.conversation_id.clone();
                // ...but only render into the chat when that conversation is the
                // one in view.
                if !self.is_active_conversation(&origin) {
                    return vec![];
                }
                let mut effects = Vec::new();
                if first_chunk {
                    effects.push(Effect::ClearChatStatus);
                }
                effects.push(Effect::ReceiveChunk(chunk));
                effects
            }
            UiMessage::StreamComplete {
                request_id,
                full_response,
            } => {
                let Some(stream) = &mut self.stream else {
                    return vec![];
                };
                if stream.request_id.is_none() {
                    stream.request_id = Some(request_id.clone());
                }
                if stream.request_id.as_deref() != Some(&request_id) {
                    return vec![];
                }
                // The stream belongs to its originating conversation (GTK-2),
                // recorded at send time — judge everything below against it, not
                // against whichever conversation is open right now.
                let origin = stream.conversation_id.clone();
                let said_via_tool = stream.say_this_spoken_this_turn;
                // An adopted external turn (a voice turn, or another client) is
                // narrated by its originator — gtk must not also speak it.
                let was_external = stream.external;
                self.stream = None;
                let is_active = self.is_active_conversation(&origin);

                if !is_active {
                    // The originating conversation isn't the one in view, so we
                    // don't hold its detail (`current_conversation` caches only
                    // the open conversation). Touch NOTHING in the open chat: no
                    // CompleteStreaming, no chat status, no audio. The reply is
                    // persisted daemon-side and appears when the user switches
                    // back and the conversation reloads.
                    return vec![];
                }

                // Reply narration (issue #80): narrate the finalized reply via
                // the embedded `Speaker` when the gate holds — `Adele == Always`
                // OR (`Adele == OnDemand` AND `You == Enabled`). Gated entirely
                // here so the cut-off holds: when the gate is false no `Speak`
                // effect exists, so no path plays audio. (The executor
                // additionally no-ops when there is no embedded engine, e.g. the
                // daemon path, which narrates its own replies.) Keyed by the
                // *originating* conversation (GTK-2): a backgrounded turn never
                // narrates (handled by the early return above) — only an in-view
                // streaming conversation can. Suppress the full-reply narration
                // when a `say_this` aside already spoke this turn — otherwise the
                // user hears it twice (the aside, then the whole reply read aloud).
                let narrate = !said_via_tool && !was_external && self.narrate_for(&origin);

                // The streaming conversation is the one in view: finalize it.
                if let Some(ref mut conv) = self.current_conversation {
                    conv.messages.push(ChatMessage {
                        // Locally-finalized reply: no server id in hand (empty
                        // placeholder); the next reload reconciles.
                        id: String::new(),
                        role: "assistant".to_string(),
                        content: full_response.clone(),
                    });
                }
                let mut effects = vec![Effect::ClearChatStatus];
                if narrate {
                    effects.push(Effect::Speak(full_response.clone()));
                }
                effects.push(Effect::CompleteStreaming(full_response));
                // The turn may have changed the scratchpad (Adele's todos);
                // refresh the pane. (The live `ScratchpadChanged` event also
                // covers this, but a turn-boundary refetch is a cheap backstop if
                // the event was missed.)
                if let Some(id) = self.current_conversation_id.clone() {
                    effects.push(Effect::FetchScratchpad(id));
                }
                effects
            }
            UiMessage::StreamError { request_id, error } => {
                let Some(stream) = &mut self.stream else {
                    return vec![];
                };
                if stream.request_id.is_none() {
                    stream.request_id = Some(request_id.clone());
                }
                if stream.request_id.as_deref() != Some(&request_id) {
                    return vec![];
                }
                let origin = stream.conversation_id.clone();
                self.stream = None;
                let is_active = self.is_active_conversation(&origin);
                // Only clear the chat status line if the failed stream's
                // conversation is the one in view (GTK-2); a background turn's
                // failure must not blank another conversation's chat. The
                // status-text line is the global one, so always surface the error.
                let mut effects = vec![Effect::SetStatusText(format!("Error: {error}"))];
                if is_active {
                    effects.insert(0, Effect::ClearChatStatus);
                }
                effects
            }
            UiMessage::TitleChanged {
                conversation_id,
                title,
            } => {
                for conv in &mut self.conversations {
                    if conv.id == conversation_id {
                        conv.title = title.clone();
                    }
                }
                vec![Effect::SetConversations(self.conversations.clone())]
            }
            UiMessage::ConversationWarning {
                conversation_id,
                warning,
            } => {
                // Single variant today — DanglingModelSelection. The daemon has
                // already cleared its side and fell back; if this is the
                // currently-open conversation, clear the header picker so it
                // doesn't show a stale "stuck" model, then surface a passive
                // toast explaining the fallback.
                match &warning {
                    api::ConversationWarning::DanglingModelSelection {
                        previous_selection,
                        fallback_to,
                    } => {
                        let is_current = self.current_conversation_id.as_deref()
                            == Some(conversation_id.as_str());
                        let mut effects = Vec::new();
                        if is_current {
                            effects.push(Effect::SetModelSelection(None));
                            // Also clear the cached detail's selection so a
                            // later `ModelsLoaded` doesn't re-apply the stale
                            // dangling selection, contradicting this toast.
                            if let Some(ref mut conv) = self.current_conversation {
                                conv.model_selection = None;
                            }
                        }
                        let message = format!(
                            "The model \"{}\" on connection \"{}\" is no longer available — falling back to \"{}\" on \"{}\".",
                            previous_selection.model_id,
                            previous_selection.connection_id,
                            fallback_to.model_id,
                            fallback_to.connection_id,
                        );
                        effects.push(Effect::ShowToast(message));
                        effects
                    }
                }
            }
            UiMessage::StatusUpdate(text) => vec![Effect::SetStatusText(text)],
            UiMessage::Error(text) => vec![Effect::SetStatusText(format!("Error: {text}"))],
            UiMessage::ModelsLoaded(listings) => {
                // A models refresh fires on every (re)connect (the UDS link
                // drops on idle / the daemon restarts) and when Settings is
                // opened. It must NOT re-apply the conversation's stored
                // selection: `set_models` already preserves the picker's active
                // selection, and re-applying the *cached* `model_selection`
                // (which is `None`/default for most conversations and is never
                // refreshed after a send) clobbered the user's in-memory pick
                // back to stored-or-default on each reconnect. The picker's
                // selection is owned by `ConversationLoaded` (an explicit
                // switch) and `set_default_model` (connect). See issue #72.
                let visible = !listings.is_empty();
                vec![
                    Effect::SetModels(listings),
                    Effect::SetModelPickerVisible(visible),
                ]
            }
            UiMessage::DefaultModelLoaded(default) => {
                // The picker uses this as the fallback selection for
                // conversations with no stored selection. Set it independently
                // of `set_selection`; the picker re-resolves
                // stored-or-default on every conversation load, so ordering
                // between the two only requires both to have run.
                vec![Effect::SetDefaultModel(default)]
            }
            UiMessage::Connected { label } => {
                vec![Effect::SetStatusText(label), Effect::SetSendSensitive(true)]
            }
            UiMessage::TasksLoaded(tasks) => {
                vec![Effect::TasksReplaceAll(tasks), Effect::RefreshSidePaneTasks]
            }
            UiMessage::TaskStarted(task) => {
                vec![Effect::TaskStarted(task), Effect::RefreshSidePaneTasks]
            }
            UiMessage::TaskProgress { id, progress_hint } => {
                vec![
                    Effect::TaskProgress { id, progress_hint },
                    Effect::RefreshSidePaneTasks,
                ]
            }
            UiMessage::TaskLogAppended { id, entry } => {
                // Log lines don't change the row set, so the side pane (which
                // shows no logs) needs no refresh here.
                vec![Effect::TaskLogAppended { id, entry }]
            }
            UiMessage::TaskCompleted { id } => {
                vec![Effect::TaskCompleted { id }, Effect::RefreshSidePaneTasks]
            }
            UiMessage::ConversationScratchpadLoaded {
                conversation_id,
                notes,
            } => {
                // Apply only if it's still the active conversation (a fetch may
                // race a conversation switch).
                if self.current_conversation_id.as_deref() == Some(conversation_id.as_str()) {
                    vec![Effect::SidePaneSetScratchpad(notes)]
                } else {
                    vec![]
                }
            }
            UiMessage::ScratchpadChanged { conversation_id } => {
                if self.current_conversation_id.as_deref() == Some(conversation_id.as_str()) {
                    vec![Effect::FetchScratchpad(conversation_id)]
                } else {
                    vec![]
                }
            }
            UiMessage::SetVoiceIn {
                conversation_id,
                enabled,
            } => {
                // Record the per-conversation `You:` (voice input) setting (issue
                // #80). Pure state change; the dropdown is the write source here
                // (the user changed it), so no UI reflection is needed. Keyed by
                // conversation so it never bleeds across them.
                self.conversation_voice_in.insert(conversation_id, enabled);
                vec![]
            }
            UiMessage::SetAdeleOutput {
                conversation_id,
                level,
            } => {
                // Record the per-conversation `Adele:` (voice output) level
                // (issue #80). Pure state change; the dropdown is the write
                // source here (the user changed it), so no UI reflection is
                // needed. Keyed by conversation so it never bleeds across them.
                self.conversation_adele_output
                    .insert(conversation_id, level);
                vec![]
            }
            UiMessage::ClientToolCall {
                task_id,
                conversation_id,
                tool_call_id,
                tool_name,
                arguments,
            } => {
                // ALWAYS resolve the call (issue #76) so the suspended turn
                // resumes — the previous code dropped it and wedged the turn.
                //
                // Every effect is keyed off the call's `conversation_id`
                // (GTK-4), not whichever conversation is open: a tool call for a
                // backgrounded conversation (e.g. a concurrent voice session, or
                // a turn the user switched away from) must act on its OWN
                // conversation's state — never borrow the viewed conversation's
                // gate, and never play audio for a conversation the user isn't
                // looking at. The dropdown reflects the *viewed* conversation, so
                // it is only nudged when the call targets the active one.
                let is_active = self.is_active_conversation(&conversation_id);
                match tool_name.as_str() {
                    "say_this" => match say_this_text(&arguments) {
                        // say_this gate (issue #80, GTK-4): the aside is spoken
                        // iff `Adele ∈ {OnDemand, Always}` for the *call's*
                        // conversation AND that conversation is the one in view.
                        // A backgrounded call's aside is never voiced — it
                        // downgrades to an inline note so it isn't lost.
                        Some(text) if is_active && self.say_this_spoken_for(&conversation_id) => {
                            // If this spoken aside belongs to the in-flight turn,
                            // mark it so StreamComplete doesn't ALSO read the full
                            // reply aloud — the model already chose its spoken form
                            // (avoids the double-speak: aside, then whole reply).
                            if let Some(stream) = &mut self.stream
                                && stream.conversation_id == conversation_id
                            {
                                stream.say_this_spoken_this_turn = true;
                            }
                            vec![
                                Effect::Speak(text),
                                Effect::SubmitClientToolResult {
                                    task_id,
                                    tool_call_id,
                                    result: Ok("spoken".to_string()),
                                },
                            ]
                        }
                        Some(text) => {
                            // Either the call's conversation has speech disabled,
                            // or it isn't the one in view: show, don't speak. The
                            // turn still completes; no audio on any path.
                            let note = format!("(speech mode disabled) {text}");
                            vec![
                                Effect::AddInlineNote(note),
                                Effect::SubmitClientToolResult {
                                    task_id,
                                    tool_call_id,
                                    result: Ok("speech mode disabled; shown to the user as text \
                                         instead of spoken"
                                        .to_string()),
                                },
                            ]
                        }
                        None => {
                            // Malformed arguments (missing/!string `text`):
                            // never panic, resolve an Err so the turn completes.
                            vec![Effect::SubmitClientToolResult {
                                task_id,
                                tool_call_id,
                                result: Err(
                                    "say_this requires a string `text` argument".to_string()
                                ),
                            }]
                        }
                    },
                    // The model asks to switch this conversation into spoken
                    // voice mode (issue #80, GTK-4): set `Adele = OnDemand` on the
                    // *call's* conversation; sticks until left. Only nudge the
                    // dropdown when that conversation is the one in view (the
                    // dropdown shows the viewed conversation). Always resolve a
                    // result. `request_voice` / `stop_voice` take no arguments,
                    // so a junk payload is simply ignored — never a panic.
                    "request_voice" => {
                        self.conversation_adele_output
                            .insert(conversation_id.clone(), AdeleOutput::OnDemand);
                        let mut effects = Vec::new();
                        if is_active {
                            effects.push(Effect::SetAdeleOutputDropdown(AdeleOutput::OnDemand));
                        }
                        effects.push(Effect::SubmitClientToolResult {
                            task_id,
                            tool_call_id,
                            result: Ok("voice mode on; replies will be read aloud and kept \
                                 conversational"
                                .to_string()),
                        });
                        effects
                    }
                    "stop_voice" => {
                        self.conversation_adele_output
                            .insert(conversation_id.clone(), AdeleOutput::Disabled);
                        let mut effects = Vec::new();
                        if is_active {
                            effects.push(Effect::SetAdeleOutputDropdown(AdeleOutput::Disabled));
                        }
                        effects.push(Effect::SubmitClientToolResult {
                            task_id,
                            tool_call_id,
                            result: Ok("voice mode off; back to text-only".to_string()),
                        });
                        effects
                    }
                    _ => {
                        // Any other client tool: this client has no runtime for
                        // it, but it must still be resolved or the turn wedges.
                        vec![Effect::SubmitClientToolResult {
                            task_id,
                            tool_call_id,
                            result: Err(format!("this client cannot run the tool \"{tool_name}\"")),
                        }]
                    }
                }
            }
            UiMessage::Disconnected { reason } => {
                let mut effects = vec![
                    Effect::ClearClient,
                    Effect::SetSendSensitive(false),
                    Effect::SetStatusText(format!("Disconnected: {reason}")),
                ];

                // Finalize any in-progress streaming buffer — but only into the
                // conversation it actually belongs to (GTK-2). If the streaming
                // conversation was backgrounded when the link dropped, the
                // truncated "[Connection lost]" buffer must NOT be appended to
                // whatever conversation happens to be open; it's simply dropped
                // (the partial reply was never persisted daemon-side anyway).
                if let Some(stream) = self.stream.take() {
                    let is_active = self.is_active_conversation(&stream.conversation_id);
                    if is_active && !stream.buffer.is_empty() {
                        let full = format!("{}\n\n[Connection lost]", stream.buffer);
                        if let Some(ref mut conv) = self.current_conversation {
                            conv.messages.push(ChatMessage {
                                // Local connection-lost stub: no server id
                                // (empty placeholder).
                                id: String::new(),
                                role: "assistant".to_string(),
                                content: full.clone(),
                            });
                        }
                        effects.push(Effect::CompleteStreaming(full));
                    }
                }
                effects
            }
        }
    }
}

/// Filter a conversation's messages based on debug mode.
///
/// When debug is off, only user and assistant messages are shown.
/// When debug is on, tool messages are included as well.
fn filter_messages(detail: &ConversationDetail, debug: bool) -> ConversationDetail {
    ConversationDetail {
        id: detail.id.clone(),
        title: detail.title.clone(),
        messages: detail
            .messages
            .iter()
            .filter(|m| {
                if debug {
                    return true;
                }
                match m.role.as_str() {
                    "user" => true,
                    // Hide empty assistant messages (tool_calls-only)
                    "assistant" => !m.content.trim().is_empty(),
                    _ => false,
                }
            })
            .cloned()
            .collect(),
        model_selection: detail.model_selection.clone(),
        conversation_personality: detail.conversation_personality,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only accessors mirroring the former free-standing `pending_*` /
    /// `streaming_buffer` fields, now that they live behind [`StreamState`]. A
    /// `__pending__` stream — request reserved but the daemon id not yet claimed
    /// — reads as `stream_request_id() == None` + `stream_unclaimed() == true`.
    impl WindowState {
        /// The claimed daemon request id, or `None` (no stream, or still in the
        /// `__pending__` window).
        fn stream_request_id(&self) -> Option<&str> {
            self.stream.as_ref().and_then(|s| s.request_id.as_deref())
        }
        /// The originating conversation of the in-flight stream, if any.
        fn stream_conversation_id(&self) -> Option<&str> {
            self.stream.as_ref().map(|s| s.conversation_id.as_str())
        }
        /// A stream is in flight but its real id is not yet claimed (the old
        /// `pending_request_id == Some("__pending__")`).
        fn stream_unclaimed(&self) -> bool {
            self.stream.as_ref().is_some_and(|s| s.request_id.is_none())
        }
        /// The in-flight stream is an adopted external turn.
        fn stream_external(&self) -> bool {
            self.stream.as_ref().is_some_and(|s| s.external)
        }
    }

    // --- Fixtures --------------------------------------------------------

    fn summary(id: &str, title: &str, archived: bool) -> ConversationSummary {
        ConversationSummary {
            id: id.to_string(),
            title: title.to_string(),
            message_count: 0,
            archived,
        }
    }

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            id: String::new(),
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    fn detail(id: &str, messages: Vec<ChatMessage>) -> ConversationDetail {
        ConversationDetail {
            id: id.to_string(),
            title: format!("conv {id}"),
            messages,
            model_selection: None,
            conversation_personality: None,
        }
    }

    fn selection(connection_id: &str, model_id: &str) -> api::ConversationModelSelectionView {
        api::ConversationModelSelectionView {
            connection_id: connection_id.to_string(),
            model_id: model_id.to_string(),
            effort: None,
        }
    }

    fn listing(connection_id: &str, model_id: &str) -> api::ModelListing {
        api::ModelListing {
            connection_id: connection_id.to_string(),
            connection_label: connection_id.to_string(),
            model: api::ModelInfoView {
                id: model_id.to_string(),
                display_name: model_id.to_string(),
                context_limit: None,
                capabilities: api::ModelCapabilitiesView::default(),
            },
        }
    }

    // --- __pending__ sentinel handoff (#31) ------------------------------

    #[test]
    fn prompt_sent_sets_pending_sentinel_and_clears_buffer() {
        let mut state = WindowState {
            // A prior stream left a partial buffer; PromptSent must start the new
            // turn from a clean slate.
            stream: Some(StreamState {
                buffer: "leftover".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::PromptSent {
            task_id: "ack-1".to_string(),
            conversation_id: "c1".to_string(),
        });
        assert!(effects.is_empty(), "PromptSent performs no widget effects");
        assert!(
            state.stream_unclaimed(),
            "the request id is the __pending__ sentinel until the first frame claims it"
        );
        assert!(state.streaming_buffer().is_empty());
    }

    /// GTK-2: the stream knows its conversation — `PromptSent` records the
    /// send-time conversation id so later stream events can be judged against
    /// the originating conversation, not whichever one is open.
    #[test]
    fn prompt_sent_records_originating_conversation() {
        let mut state = WindowState {
            // The user already switched to c2 by the time the ack arrived; the
            // recorded conversation must still be the send-time one.
            current_conversation_id: Some("c2".to_string()),
            ..Default::default()
        };
        state.apply(UiMessage::PromptSent {
            task_id: "ack-1".to_string(),
            conversation_id: "c1".to_string(),
        });
        assert_eq!(state.stream_conversation_id(), Some("c1"));
    }

    // --- SubmitPrompt / SendFailed: the core-owned send decision ----------

    #[test]
    fn submit_prompt_draws_the_bubble_and_emits_send_prompt() {
        let mut state = WindowState {
            current_conversation: Some(detail("c1", vec![])),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: "hello".to_string(),
        });
        // Optimistic user bubble drawn into the open transcript...
        let conv = state.current_conversation.as_ref().unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "hello");
        // ...and the RPC effect emitted for the client to run. Adele is Disabled
        // by default, so no voice refinement rides along.
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::SendPrompt { conversation_id, prompt, system_refinement }]
                    if conversation_id == "c1" && prompt == "hello" && system_refinement.is_none()
            ),
            "{effects:?}"
        );
    }

    #[test]
    fn submit_prompt_carries_the_voice_refinement_when_adele_is_on() {
        let mut state = WindowState {
            current_conversation: Some(detail("c1", vec![])),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        state.apply(UiMessage::SetAdeleOutput {
            conversation_id: "c1".to_string(),
            level: AdeleOutput::OnDemand,
        });
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: "hi".to_string(),
        });
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::SendPrompt { system_refinement: Some(r), .. }] if !r.is_empty()
            ),
            "an OnDemand conversation must carry a speech refinement: {effects:?}"
        );
    }

    #[test]
    fn submit_prompt_is_blocked_while_a_reply_is_streaming() {
        // TUI-7: a second send is refused mid-stream — no bubble, no RPC, just a
        // status line explaining why (the client keeps the composer text).
        let mut state = mid_stream_state("c1", "c1");
        let before = state.current_conversation.as_ref().unwrap().messages.len();
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: "second".to_string(),
        });
        assert!(
            matches!(effects.as_slice(), [Effect::SetStatusText(_)]),
            "{effects:?}"
        );
        assert_eq!(
            state.current_conversation.as_ref().unwrap().messages.len(),
            before,
            "a blocked send must not append a bubble"
        );
    }

    #[test]
    fn submit_prompt_empty_is_a_silent_noop() {
        let mut state = WindowState {
            current_conversation: Some(detail("c1", vec![])),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: String::new(),
        });
        assert!(effects.is_empty());
        assert!(
            state
                .current_conversation
                .as_ref()
                .unwrap()
                .messages
                .is_empty()
        );
    }

    #[test]
    fn send_failed_rolls_back_the_matching_optimistic_tail() {
        let mut state = WindowState {
            current_conversation: Some(detail("c1", vec![msg("user", "doomed")])),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::SendFailed {
            conversation_id: "c1".to_string(),
            prompt: "doomed".to_string(),
        });
        assert!(effects.is_empty());
        assert!(
            state
                .current_conversation
                .as_ref()
                .unwrap()
                .messages
                .is_empty(),
            "the optimistic user bubble must be rolled back"
        );
    }

    #[test]
    fn send_failed_leaves_another_conversations_tail_intact() {
        // The user switched conversations between submit and the failure; the now
        // open conversation's transcript must not be touched (TUI-2).
        let mut state = WindowState {
            current_conversation: Some(detail("c2", vec![msg("user", "different")])),
            current_conversation_id: Some("c2".to_string()),
            ..Default::default()
        };
        state.apply(UiMessage::SendFailed {
            conversation_id: "c1".to_string(),
            prompt: "doomed".to_string(),
        });
        assert_eq!(
            state.current_conversation.as_ref().unwrap().messages.len(),
            1,
            "the other conversation's transcript stays intact"
        );
    }

    #[test]
    fn send_failed_does_not_pop_a_non_matching_tail() {
        // Something landed after the optimistic append (e.g. an inline note): only
        // an exact matching tail is rolled back, never an unrelated last message.
        let mut state = WindowState {
            current_conversation: Some(detail(
                "c1",
                vec![msg("user", "doomed"), msg("assistant", "(an aside)")],
            )),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        state.apply(UiMessage::SendFailed {
            conversation_id: "c1".to_string(),
            prompt: "doomed".to_string(),
        });
        assert_eq!(
            state.current_conversation.as_ref().unwrap().messages.len(),
            2,
            "a non-matching tail must not be popped"
        );
    }

    // --- GTK-2: in-flight stream vs conversation switch -------------------

    /// Pin a pending stream originating in `from`, viewed from `current`.
    fn mid_stream_state(from: &str, current: &str) -> WindowState {
        WindowState {
            stream: Some(StreamState {
                request_id: Some("req-real".to_string()),
                conversation_id: from.to_string(),
                buffer: "partial ".to_string(),
                ..Default::default()
            }),
            current_conversation_id: Some(current.to_string()),
            current_conversation: Some(detail(current, vec![msg("user", "hi")])),
            ..Default::default()
        }
    }

    /// GTK-2 acceptance: a chunk arriving after the user switched away keeps
    /// buffering for the originating conversation but is NOT rendered into the
    /// newly opened conversation's chat.
    #[test]
    fn chunk_after_conversation_switch_is_buffered_not_rendered() {
        let mut state = mid_stream_state("c1", "c2");
        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "req-real".to_string(),
            chunk: "more".to_string(),
        });
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::ReceiveChunk(_))),
            "a background stream's chunk must not render into the open conversation: {effects:?}"
        );
        assert_eq!(
            state.streaming_buffer(),
            "partial more",
            "the chunk must still accumulate for the originating conversation"
        );
    }

    /// The public streaming accessors (consumed by view clients like the TUI)
    /// reflect the private pending-stream state across the key transitions.
    #[test]
    fn streaming_accessors_reflect_pending_state() {
        // Fresh: nothing streaming, empty buffer.
        let state = WindowState::default();
        assert!(!state.is_streaming());
        assert_eq!(state.streaming_buffer(), "");

        // A stream pinned to c1, viewed from c1: streaming, buffered, in view.
        let state = mid_stream_state("c1", "c1");
        assert!(state.is_streaming());
        assert_eq!(state.streaming_buffer(), "partial ");
        assert!(state.streaming_is_active_for_view());

        // The same stream after switching to c2: still streaming and buffering,
        // but NOT active for the view — the render guard must hold.
        let state = mid_stream_state("c1", "c2");
        assert!(state.is_streaming());
        assert!(!state.streaming_is_active_for_view());
    }

    /// TUI-8: `reset_streaming_state` drops the in-flight stream without
    /// finalizing it — no frozen partial, no lingering pending id, and (unlike
    /// the `Disconnected` arm) it does NOT append a `[Connection lost]` stub to
    /// the open conversation. It also clears the ack sentinel so the next
    /// post-reconnect stream can't be mis-claimed.
    #[test]
    fn reset_streaming_state_discards_the_partial_without_finalizing() {
        let mut state = mid_stream_state("c1", "c1");
        let before = state.current_conversation.as_ref().unwrap().messages.len();

        state.reset_streaming_state();

        assert!(!state.is_streaming(), "the pending stream must be cleared");
        assert_eq!(state.streaming_buffer(), "", "the partial must be dropped");
        // The emptied buffer is what makes the view's render guard inert
        // (`!buffer.is_empty() && active`): with no originating conversation
        // recorded, `streaming_is_active_for_view()` is vacuously true, so it's
        // the empty buffer — not the guard — that stops the partial painting.
        assert_eq!(
            state.current_conversation.as_ref().unwrap().messages.len(),
            before,
            "reset must NOT append a [Connection lost] stub (that's Disconnected's job)"
        );
    }

    /// After a reset clears the ack sentinel, a chunk for a brand-new stream
    /// must not be claimed by the dead turn (the TUI-8 mis-claim guard).
    #[test]
    fn reset_streaming_state_prevents_misclaim_of_the_next_stream() {
        let mut state = mid_stream_state("c1", "c1");
        state.reset_streaming_state();

        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "post-reconnect-req".to_string(),
            chunk: "someone else's chunk".to_string(),
        });

        assert!(
            effects.is_empty(),
            "a chunk with nothing pending is ignored"
        );
        assert_eq!(state.streaming_buffer(), "", "and nothing is buffered");
    }

    /// GTK-2 acceptance: `StreamComplete` after a switch finalizes the
    /// originating conversation only — the currently open conversation's cache
    /// and chat view stay untouched.
    #[test]
    fn complete_after_switch_does_not_append_to_current_conversation() {
        let mut state = mid_stream_state("c1", "c2");
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "the answer".to_string(),
        });
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::CompleteStreaming(_))),
            "a background completion must not finalize into the open chat: {effects:?}"
        );
        let current = state.current_conversation.as_ref().unwrap();
        assert!(
            current.messages.iter().all(|m| m.content != "the answer"),
            "the reply must not be appended to the wrong conversation"
        );
        assert!(!state.is_streaming(), "stream is over");
    }

    /// GTK-2: an `AssistantStatus` for a background stream must not paint the
    /// open conversation's status line.
    #[test]
    fn assistant_status_for_background_stream_is_not_shown() {
        let mut state = mid_stream_state("c1", "c2");
        let effects = state.apply(UiMessage::AssistantStatus {
            request_id: "req-real".to_string(),
            message: "Searching...".to_string(),
        });
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::SetChatStatus(_))),
            "background status must not show over another conversation: {effects:?}"
        );
    }

    /// GTK-2: switching away mid-stream clears the chat status line that
    /// belonged to the streaming conversation's turn.
    #[test]
    fn switching_away_mid_stream_clears_chat_status() {
        let mut state = mid_stream_state("c1", "c1");
        let effects = state.apply(UiMessage::ConversationLoaded(detail("c2", vec![])));
        assert!(
            effects.iter().any(|e| matches!(e, Effect::ClearChatStatus)),
            "the streaming turn's status must not linger over c2: {effects:?}"
        );
    }

    /// GTK-2: switching back to the streaming conversation mid-stream re-seeds
    /// the partial reply into the chat view (the buffered prefix would
    /// otherwise be missing until completion).
    #[test]
    fn switching_back_to_streaming_conversation_reseeds_partial_reply() {
        let mut state = mid_stream_state("c1", "c2");
        let effects = state.apply(UiMessage::ConversationLoaded(detail("c1", vec![])));
        let position_load = effects
            .iter()
            .position(|e| matches!(e, Effect::LoadConversationIntoChat(_)));
        let position_seed = effects
            .iter()
            .position(|e| matches!(e, Effect::ReceiveChunk(c) if c == "partial "));
        assert!(
            position_seed.is_some(),
            "the buffered partial reply must be re-seeded: {effects:?}"
        );
        assert!(
            position_load < position_seed,
            "the seed must render after the conversation loads: {effects:?}"
        );
    }

    /// GTK-2 unhappy path: a disconnect while the streaming conversation is
    /// backgrounded must not finalize the truncated buffer into the open
    /// conversation.
    #[test]
    fn disconnect_mid_stream_after_switch_does_not_finalize_into_current() {
        let mut state = mid_stream_state("c1", "c2");
        let effects = state.apply(UiMessage::Disconnected {
            reason: "socket closed".to_string(),
        });
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::CompleteStreaming(_))),
            "the truncated background stream must not render into c2: {effects:?}"
        );
        let current = state.current_conversation.as_ref().unwrap();
        assert!(
            current
                .messages
                .iter()
                .all(|m| !m.content.contains("[Connection lost]")),
            "the [Connection lost] marker must not land in the wrong conversation"
        );
        assert!(!state.is_streaming());
    }

    /// GTK-2/GTK-4: reply narration follows the originating conversation —
    /// a completion for a backgrounded conversation produces no audio even
    /// when that conversation's gate is wide open (`Adele == Always`).
    #[test]
    fn narration_skipped_when_originating_conversation_backgrounded() {
        let mut state = mid_stream_state("c1", "c2");
        state
            .conversation_adele_output
            .insert("c1".to_string(), AdeleOutput::Always);
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "an answer".to_string(),
        });
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "a background conversation's reply must not be narrated: {effects:?}"
        );
    }

    #[test]
    fn first_stream_chunk_claims_real_request_id_from_pending_sentinel() {
        let mut state = WindowState {
            // A __pending__ stream (id not yet claimed) for the open conversation.
            stream: Some(StreamState {
                conversation_id: "c1".to_string(),
                ..Default::default()
            }),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "req-real".to_string(),
            chunk: "hello".to_string(),
        });
        // The __pending__ slot is claimed by the daemon's real request id...
        assert_eq!(state.stream_request_id(), Some("req-real"));
        assert_eq!(state.streaming_buffer(), "hello");
        // ...and because this is the first chunk, the chat status is cleared
        // before the chunk is rendered.
        assert!(
            matches!(effects.as_slice(), [Effect::ClearChatStatus, Effect::ReceiveChunk(c)] if c == "hello"),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn subsequent_stream_chunk_appends_without_clearing_status() {
        let mut state = WindowState {
            stream: Some(StreamState {
                request_id: Some("req-real".to_string()),
                conversation_id: "c1".to_string(),
                buffer: "hello".to_string(),
                ..Default::default()
            }),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "req-real".to_string(),
            chunk: " world".to_string(),
        });
        assert_eq!(state.streaming_buffer(), "hello world");
        // Non-first chunk: only the chunk is rendered, no status clear.
        assert!(
            matches!(effects.as_slice(), [Effect::ReceiveChunk(c)] if c == " world"),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn stream_chunk_for_unrelated_request_id_is_ignored() {
        let mut state = WindowState {
            stream: Some(StreamState {
                request_id: Some("req-real".to_string()),
                conversation_id: "c1".to_string(),
                buffer: "hello".to_string(),
                ..Default::default()
            }),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "some-other-req".to_string(),
            chunk: "noise".to_string(),
        });
        assert!(effects.is_empty(), "stray chunk must not render");
        assert_eq!(
            state.streaming_buffer(),
            "hello",
            "buffer must be untouched"
        );
    }

    #[test]
    fn assistant_status_matches_pending_sentinel_before_request_id_known() {
        let mut state = WindowState {
            // __pending__ stream (id not yet claimed) for the open conversation.
            stream: Some(StreamState {
                conversation_id: "c1".to_string(),
                ..Default::default()
            }),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::AssistantStatus {
            request_id: "req-not-yet-claimed".to_string(),
            message: "Searching...".to_string(),
        });
        assert!(
            matches!(effects.as_slice(), [Effect::SetChatStatus(m)] if m == "Searching..."),
            "status during the __pending__ window must reach the chat: {effects:?}"
        );
    }

    #[test]
    fn stream_complete_claims_sentinel_appends_message_and_clears_pending() {
        let mut state = WindowState {
            stream: Some(StreamState {
                conversation_id: "c1".to_string(),
                buffer: "partial".to_string(),
                ..Default::default()
            }),
            current_conversation: Some(detail("c1", vec![msg("user", "hi")])),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "the answer".to_string(),
        });
        assert!(!state.is_streaming());
        assert!(state.streaming_buffer().is_empty());
        let conv = state.current_conversation.as_ref().unwrap();
        assert_eq!(conv.messages.last().unwrap().role, "assistant");
        assert_eq!(conv.messages.last().unwrap().content, "the answer");
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::ClearChatStatus,
                    Effect::CompleteStreaming(c),
                    Effect::FetchScratchpad(conv),
                ] if c == "the answer" && conv == "c1"
            ),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn stream_error_clears_pending_and_sets_error_status() {
        let mut state = WindowState {
            stream: Some(StreamState {
                request_id: Some("req-real".to_string()),
                conversation_id: "c1".to_string(),
                buffer: "partial".to_string(),
                ..Default::default()
            }),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamError {
            request_id: "req-real".to_string(),
            error: "boom".to_string(),
        });
        assert!(!state.is_streaming());
        assert!(state.streaming_buffer().is_empty());
        assert!(
            matches!(effects.as_slice(), [Effect::ClearChatStatus, Effect::SetStatusText(t)] if t == "Error: boom"),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn disconnect_finalizes_in_progress_stream_with_connection_lost_marker() {
        let mut state = WindowState {
            stream: Some(StreamState {
                request_id: Some("req-real".to_string()),
                conversation_id: "c1".to_string(),
                buffer: "half a thought".to_string(),
                ..Default::default()
            }),
            current_conversation: Some(detail("c1", vec![])),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::Disconnected {
            reason: "socket closed".to_string(),
        });
        assert!(!state.is_streaming());
        assert!(state.streaming_buffer().is_empty());
        // The partial response is committed to the conversation with the marker.
        let last = state
            .current_conversation
            .as_ref()
            .unwrap()
            .messages
            .last()
            .unwrap();
        assert_eq!(last.content, "half a thought\n\n[Connection lost]");
        // Effects: clear client, desensitize send, status text, then finalize.
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::ClearClient,
                    Effect::SetSendSensitive(false),
                    Effect::SetStatusText(t),
                    Effect::CompleteStreaming(c),
                ] if t == "Disconnected: socket closed" && c == "half a thought\n\n[Connection lost]"
            ),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn disconnect_without_active_stream_does_not_emit_complete_streaming() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::Disconnected {
            reason: "bye".to_string(),
        });
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::ClearClient,
                    Effect::SetSendSensitive(false),
                    Effect::SetStatusText(_)
                ]
            ),
            "no streaming buffer => no CompleteStreaming: {effects:?}"
        );
    }

    // --- GTK-10: single load for a freshly-created conversation ----------

    /// GTK-10: when `ConversationsLoaded` arrives for an active conversation
    /// whose detail is NOT yet cached (a just-created conversation), the reducer
    /// emits a single picker-setting `LoadConversation` — never a redundant
    /// `ReloadConversation` on top of a separate explicit fetch.
    #[test]
    fn conversations_loaded_for_uncached_active_emits_single_fresh_load() {
        let mut state = WindowState {
            current_conversation_id: Some("new".to_string()),
            // No cached detail for "new" — it was just created.
            current_conversation: None,
            ..Default::default()
        };
        let convs = vec![summary("new", "New Conversation", false)];
        let effects = state.apply(UiMessage::ConversationsLoaded(convs));
        let loads = effects
            .iter()
            .filter(|e| matches!(e, Effect::LoadConversation(id) if id == "new"))
            .count();
        let reloads = effects
            .iter()
            .filter(|e| matches!(e, Effect::ReloadConversation(_)))
            .count();
        assert_eq!(
            loads, 1,
            "a fresh active conversation gets one LoadConversation: {effects:?}"
        );
        assert_eq!(
            reloads, 0,
            "and no picker-preserving ReloadConversation: {effects:?}"
        );
    }

    /// GTK-10: a reconnect (`ConversationsLoaded` while the active conversation's
    /// detail IS cached) still refreshes via the picker-preserving
    /// `ReloadConversation`, never a fresh `LoadConversation` (#72 must hold).
    #[test]
    fn conversations_loaded_for_cached_active_reloads_not_fresh_load() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            current_conversation: Some(detail("c1", vec![msg("user", "hi")])),
            ..Default::default()
        };
        let convs = vec![summary("c1", "one", false)];
        let effects = state.apply(UiMessage::ConversationsLoaded(convs));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::ReloadConversation(id) if id == "c1")),
            "reconnect refresh must use ReloadConversation (preserves picker): {effects:?}"
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::LoadConversation(_))),
            "reconnect must not re-apply the picker via LoadConversation: {effects:?}"
        );
    }

    // --- Archived-list refresh -------------------------------------------

    #[test]
    fn conversations_loaded_stores_list_and_refreshes_sidebar_then_ensures_active() {
        // The "show archived" toggle re-fetches and re-delivers the list via
        // ConversationsLoaded; apply must repaint the sidebar with the new
        // (possibly archived-including) set and re-run ensure-active.
        let mut state = WindowState::default();
        let convs = vec![
            summary("c1", "Active one", false),
            summary("c2", "Archived one", true),
        ];
        let effects = state.apply(UiMessage::ConversationsLoaded(convs.clone()));
        assert_eq!(state.conversations.len(), 2);
        assert_eq!(state.conversations[1].id, "c2");
        assert!(state.conversations[1].archived);
        match effects.as_slice() {
            [
                Effect::SetConversations(got),
                Effect::EnsureActiveConversation,
            ] => {
                assert_eq!(got.len(), 2);
                assert_eq!(got[1].id, "c2");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    /// Issue #1: a `ConversationListChanged` signal (the list changed on
    /// another connection) must trigger a full list re-fetch — and nothing else.
    /// It carries only the affected id; the reducer responds with a single
    /// `RefetchConversationList` effect rather than patching a row.
    #[test]
    fn conversation_list_changed_triggers_a_list_refetch_only() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "one", false)],
            current_conversation_id: Some("c1".to_string()),
            current_conversation: Some(detail("c1", vec![])),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationListChanged {
            conversation_id: "c2".to_string(),
        });
        assert!(
            matches!(effects.as_slice(), [Effect::RefetchConversationList]),
            "ConversationListChanged must request exactly one list re-fetch: {effects:?}"
        );
        // The decision step must not yet mutate the cached list or touch the
        // open conversation — that waits for the refetch result.
        assert_eq!(state.conversations.len(), 1);
        assert_eq!(state.current_conversation_id.as_deref(), Some("c1"));
        assert!(state.current_conversation.is_some());
    }

    /// Issue #1: the refetch result repaints ONLY the sidebar (and re-syncs the
    /// selection via `EnsureActiveConversation`). It must NOT reload the open
    /// conversation's chat or re-apply the model picker — so a sibling-client
    /// change never disturbs what the user is reading/typing. Concretely, no
    /// `ReloadConversation`/`LoadConversation` is emitted even though an open
    /// conversation is present and cached.
    #[test]
    fn conversation_list_refetched_repaints_sidebar_without_disturbing_open_chat() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "one", false)],
            current_conversation_id: Some("c1".to_string()),
            current_conversation: Some(detail("c1", vec![msg("user", "hi")])),
            ..Default::default()
        };
        // A sibling client added "c2" and renamed "c1".
        let fresh = vec![
            summary("c1", "one renamed", false),
            summary("c2", "two", false),
        ];
        let effects = state.apply(UiMessage::ConversationListRefetched(fresh.clone()));

        // The fresh list is stored and the sidebar repainted + re-synced.
        assert_eq!(state.conversations.len(), 2);
        assert_eq!(state.conversations[0].title, "one renamed");
        assert_eq!(state.conversations[1].id, "c2");
        match effects.as_slice() {
            [
                Effect::SetConversations(got),
                Effect::EnsureActiveConversation,
            ] => {
                assert_eq!(got.len(), 2);
                assert_eq!(got[1].id, "c2");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
        // The open conversation must be left exactly as the user had it: no
        // chat reload, no picker re-apply, and the cached detail is untouched.
        assert!(
            !effects.iter().any(|e| matches!(
                e,
                Effect::ReloadConversation(_)
                    | Effect::LoadConversation(_)
                    | Effect::LoadConversationIntoChat(_)
                    | Effect::SetModelSelection(_)
            )),
            "a list-only refetch must not disturb the open chat or picker: {effects:?}"
        );
        assert_eq!(state.current_conversation_id.as_deref(), Some("c1"));
        assert!(
            state
                .current_conversation
                .as_ref()
                .is_some_and(|c| c.messages.len() == 1),
            "the open conversation's cached detail must be preserved verbatim"
        );
    }

    #[test]
    fn deleting_active_conversation_clears_chat_and_re_ensures_active() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "one", false), summary("c2", "two", false)],
            current_conversation_id: Some("c1".to_string()),
            current_conversation: Some(detail("c1", vec![])),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationDeleted {
            id: "c1".to_string(),
        });
        assert_eq!(state.conversations.len(), 1);
        assert_eq!(state.conversations[0].id, "c2");
        assert!(state.current_conversation_id.is_none());
        assert!(state.current_conversation.is_none());
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::SetConversations(_),
                    Effect::ClearChat,
                    Effect::SidePaneSetScratchpad(_),
                    Effect::RefreshSidePaneTasks,
                    Effect::EnsureActiveConversation
                ]
            ),
            "deleting the active conversation must clear chat + side pane + re-ensure: {effects:?}"
        );
    }

    /// GTK-9: deleting a conversation prunes its per-conversation voice maps
    /// (`You:` input + `Adele:` output level) so a recycled/UUID-reused id can't
    /// inherit a stale voice setting, and the maps don't grow unbounded.
    #[test]
    fn deleting_conversation_prunes_its_voice_maps() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "one", false), summary("c2", "two", false)],
            current_conversation_id: Some("c2".to_string()),
            current_conversation: Some(detail("c2", vec![])),
            ..Default::default()
        };
        // Both conversations carry voice settings.
        state.conversation_voice_in.insert("c1".to_string(), true);
        state
            .conversation_adele_output
            .insert("c1".to_string(), AdeleOutput::Always);
        state.conversation_voice_in.insert("c2".to_string(), true);
        state
            .conversation_adele_output
            .insert("c2".to_string(), AdeleOutput::OnDemand);

        // Delete the inactive one.
        state.apply(UiMessage::ConversationDeleted {
            id: "c1".to_string(),
        });
        assert!(
            !state.conversation_voice_in.contains_key("c1"),
            "deleted conversation's You: setting must be pruned"
        );
        assert!(
            !state.conversation_adele_output.contains_key("c1"),
            "deleted conversation's Adele: level must be pruned"
        );
        // The surviving conversation's settings are untouched.
        assert_eq!(state.conversation_voice_in.get("c2").copied(), Some(true));
        assert_eq!(
            state.conversation_adele_output.get("c2").copied(),
            Some(AdeleOutput::OnDemand)
        );

        // Deleting the active one prunes it too.
        state.apply(UiMessage::ConversationDeleted {
            id: "c2".to_string(),
        });
        assert!(state.conversation_voice_in.is_empty());
        assert!(state.conversation_adele_output.is_empty());
    }

    fn note_view(key: &str) -> api::ScratchpadNoteView {
        api::ScratchpadNoteView {
            id: format!("id-{key}"),
            key: key.to_string(),
            content: "x".to_string(),
            note_type: "note".to_string(),
            sequence: None,
            done: false,
            updated_at: "t".to_string(),
        }
    }

    #[test]
    fn scratchpad_loaded_applies_only_for_active_conversation() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        // Matching conversation → set the pane.
        let effects = state.apply(UiMessage::ConversationScratchpadLoaded {
            conversation_id: "c1".to_string(),
            notes: vec![note_view("goal")],
        });
        assert!(
            matches!(effects.as_slice(), [Effect::SidePaneSetScratchpad(n)] if n.len() == 1),
            "unexpected: {effects:?}"
        );
        // A fetch that resolves after a conversation switch is ignored.
        let effects = state.apply(UiMessage::ConversationScratchpadLoaded {
            conversation_id: "stale".to_string(),
            notes: vec![note_view("goal")],
        });
        assert!(effects.is_empty(), "stale scratchpad must be dropped");
    }

    #[test]
    fn scratchpad_changed_refetches_only_for_active_conversation() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ScratchpadChanged {
            conversation_id: "c1".to_string(),
        });
        assert!(matches!(effects.as_slice(), [Effect::FetchScratchpad(c)] if c == "c1"));
        let effects = state.apply(UiMessage::ScratchpadChanged {
            conversation_id: "other".to_string(),
        });
        assert!(
            effects.is_empty(),
            "a change to another conversation is ignored"
        );
    }

    #[test]
    fn tasks_loaded_also_refreshes_the_side_pane() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::TasksLoaded(vec![]));
        assert!(matches!(
            effects.as_slice(),
            [Effect::TasksReplaceAll(_), Effect::RefreshSidePaneTasks]
        ));
    }

    #[test]
    fn deleting_inactive_conversation_only_refreshes_sidebar() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "one", false), summary("c2", "two", false)],
            current_conversation_id: Some("c1".to_string()),
            current_conversation: Some(detail("c1", vec![])),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationDeleted {
            id: "c2".to_string(),
        });
        assert!(state.current_conversation_id.as_deref() == Some("c1"));
        assert!(
            matches!(effects.as_slice(), [Effect::SetConversations(got)] if got.len() == 1),
            "deleting an inactive conversation must not touch the chat: {effects:?}"
        );
    }

    #[test]
    fn rename_updates_matching_conversation_title_and_refreshes_sidebar() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "old", false), summary("c2", "keep", false)],
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationRenamed {
            id: "c1".to_string(),
            title: "new title".to_string(),
        });
        assert_eq!(state.conversations[0].title, "new title");
        assert_eq!(state.conversations[1].title, "keep");
        match effects.as_slice() {
            [Effect::SetConversations(got)] => assert_eq!(got[0].title, "new title"),
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn title_changed_signal_updates_matching_conversation_and_refreshes_sidebar() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "untitled", false)],
            ..Default::default()
        };
        let effects = state.apply(UiMessage::TitleChanged {
            conversation_id: "c1".to_string(),
            title: "Auto Title".to_string(),
        });
        assert_eq!(state.conversations[0].title, "Auto Title");
        assert!(matches!(effects.as_slice(), [Effect::SetConversations(_)]));
    }

    // --- Debug filter ----------------------------------------------------

    #[test]
    fn conversation_loaded_hides_tool_messages_when_debug_off() {
        let mut state = WindowState {
            debug_enabled: false,
            ..Default::default()
        };
        let d = detail(
            "c1",
            vec![
                msg("user", "hi"),
                msg("tool", "tool noise"),
                msg("assistant", "answer"),
                msg("assistant", "   "), // empty (tool-calls only) assistant
            ],
        );
        let effects = state.apply(UiMessage::ConversationLoaded(d));
        // The cached (unfiltered) conversation keeps all 4 messages...
        assert_eq!(
            state.current_conversation.as_ref().unwrap().messages.len(),
            4
        );
        // ...but the chat view receives only user + non-empty assistant.
        match effects.as_slice() {
            [
                Effect::SetModelSelection(_),
                Effect::LoadConversationIntoChat(filtered),
                Effect::SetContextUsage(None),
                Effect::SubscribeConversations(_),
                Effect::SidePaneSetScratchpad(_),
                Effect::RefreshSidePaneTasks,
                Effect::FetchScratchpad(_),
            ] => {
                let roles: Vec<&str> = filtered.messages.iter().map(|m| m.role.as_str()).collect();
                assert_eq!(roles, vec!["user", "assistant"]);
                assert_eq!(filtered.messages[1].content, "answer");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    // --- Context-usage indicator (#341) ---

    #[test]
    fn context_usage_for_open_conversation_sets_indicator() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ContextUsage {
            conversation_id: "c1".to_string(),
            used_tokens: 12_000,
            budget_tokens: 32_000,
            compaction_active: false,
        });
        match effects.as_slice() {
            [Effect::SetContextUsage(Some(u))] => {
                assert_eq!(u.used_tokens, 12_000);
                assert_eq!(u.budget_tokens, 32_000);
                assert!(!u.compaction_active);
            }
            other => panic!("expected SetContextUsage(Some), got {other:?}"),
        }
    }

    #[test]
    fn context_usage_for_background_conversation_is_ignored() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        // A reading for a conversation that is not in view must not paint.
        let effects = state.apply(UiMessage::ContextUsage {
            conversation_id: "c2".to_string(),
            used_tokens: 30_000,
            budget_tokens: 32_000,
            compaction_active: true,
        });
        assert!(
            effects.is_empty(),
            "background-conversation usage must produce no effect"
        );
    }

    #[test]
    fn switching_conversation_clears_context_usage_indicator() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        // Loading a (different) conversation must emit SetContextUsage(None)
        // so a stale fill never bleeds across conversations.
        let effects = state.apply(UiMessage::ConversationLoaded(detail(
            "c2",
            vec![msg("user", "hi")],
        )));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SetContextUsage(None))),
            "conversation switch must clear the context-fill indicator"
        );
    }

    #[test]
    fn conversation_loaded_shows_tool_messages_when_debug_on() {
        let mut state = WindowState {
            debug_enabled: true,
            ..Default::default()
        };
        let d = detail(
            "c1",
            vec![
                msg("user", "hi"),
                msg("tool", "tool noise"),
                msg("assistant", "   "),
            ],
        );
        let effects = state.apply(UiMessage::ConversationLoaded(d));
        match effects.as_slice() {
            [
                Effect::SetModelSelection(_),
                Effect::LoadConversationIntoChat(filtered),
                Effect::SetContextUsage(None),
                Effect::SubscribeConversations(_),
                Effect::SidePaneSetScratchpad(_),
                Effect::RefreshSidePaneTasks,
                Effect::FetchScratchpad(_),
            ] => {
                // Debug on: nothing is filtered out.
                assert_eq!(filtered.messages.len(), 3);
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn conversation_loaded_sets_active_id_and_applies_stored_model_selection() {
        let mut state = WindowState::default();
        let mut d = detail("c9", vec![msg("user", "hi")]);
        d.model_selection = Some(selection("work", "claude"));
        let effects = state.apply(UiMessage::ConversationLoaded(d));
        assert_eq!(state.current_conversation_id.as_deref(), Some("c9"));
        match effects.as_slice() {
            [
                Effect::SetModelSelection(Some(sel)),
                Effect::LoadConversationIntoChat(_),
                Effect::SetContextUsage(None),
                Effect::SubscribeConversations(_),
                Effect::SidePaneSetScratchpad(_),
                Effect::RefreshSidePaneTasks,
                Effect::FetchScratchpad(conv),
            ] => {
                assert_eq!(sel.connection_id, "work");
                assert_eq!(sel.model_id, "claude");
                assert_eq!(conv, "c9");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    // --- Live multi-client conversation subscription (#1) ----------------

    #[test]
    fn conversation_loaded_subscribes_to_that_conversation() {
        // Switching/loading a conversation must tell the daemon we're now
        // viewing it (set-replace, just the active one) so its turn events —
        // including ones started by another client or the voice daemon — fan to
        // this connection for live render.
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::ConversationLoaded(detail(
            "c7",
            vec![msg("user", "hi")],
        )));
        let subscribed = effects.iter().find_map(|e| match e {
            Effect::SubscribeConversations(ids) => Some(ids),
            _ => None,
        });
        assert_eq!(
            subscribed.map(Vec::as_slice),
            Some(["c7".to_string()].as_slice()),
            "ConversationLoaded must emit SubscribeConversations([active id]); got {effects:?}"
        );
    }

    // --- Model-picker re-application -------------------------------------

    #[test]
    fn models_loaded_does_not_touch_picker_selection() {
        // Regression (issue #72): a models refresh fires on every (re)connect.
        // It must NOT re-apply the conversation's stored selection — doing so
        // clobbered the user's in-memory pick back to stored-or-default on each
        // reconnect. `set_models` preserves the picker's `active`; the selection
        // is owned by ConversationLoaded (switch) and set_default_model.
        let mut conv = detail("c1", vec![]);
        conv.model_selection = Some(selection("work", "claude"));
        let mut state = WindowState {
            current_conversation: Some(conv),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ModelsLoaded(vec![listing("work", "claude")]));
        match effects.as_slice() {
            [
                Effect::SetModels(models),
                Effect::SetModelPickerVisible(true),
            ] => {
                assert_eq!(models.len(), 1);
            }
            other => panic!("ModelsLoaded must not emit SetModelSelection: {other:?}"),
        }
    }

    #[test]
    fn models_loaded_empty_list_hides_picker_and_skips_reapply_when_no_conversation() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::ModelsLoaded(Vec::new()));
        match effects.as_slice() {
            [
                Effect::SetModels(models),
                Effect::SetModelPickerVisible(false),
            ] => {
                assert!(models.is_empty());
            }
            other => panic!("unexpected effects (no conversation => no reapply): {other:?}"),
        }
    }

    // --- Reconnect: reload the active conversation without resetting picker --

    #[test]
    fn conversations_loaded_on_reconnect_reloads_active_conversation() {
        // Issue #72: on reconnect the (still-present) active conversation is
        // re-fetched via ReloadConversation — which refreshes the cache and
        // keeps the picker — instead of ConversationLoaded (which resets it).
        // A true reconnect has the conversation's detail already cached (it was
        // open before the link dropped); that cached detail is what selects the
        // picker-preserving ReloadConversation over a fresh LoadConversation
        // (GTK-10).
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            current_conversation: Some(detail("c1", vec![msg("user", "earlier")])),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationsLoaded(vec![summary(
            "c1", "first", false,
        )]));
        match effects.as_slice() {
            [
                Effect::SetConversations(_),
                Effect::EnsureActiveConversation,
                // Reconnect re-establishes the daemon's turn-event subscription
                // for the still-open conversation (#1) — the cached-detail path
                // refreshes via ReloadConversation, which never passes through
                // ConversationLoaded where the switch-time subscribe lives, so
                // the subscribe must be re-sent here too.
                Effect::SubscribeConversations(ids),
                Effect::ReloadConversation(id),
            ] => {
                assert_eq!(ids.as_slice(), ["c1".to_string()]);
                assert_eq!(id, "c1");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn conversations_loaded_on_first_connect_does_not_reload() {
        // First connect: no active conversation yet, so the initial load runs
        // through EnsureActiveConversation -> ConversationLoaded (which sets the
        // picker). No ReloadConversation.
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::ConversationsLoaded(vec![summary(
            "c1", "first", false,
        )]));
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::SetConversations(_),
                    Effect::EnsureActiveConversation
                ]
            ),
            "first connect must not reload: {effects:?}"
        );
    }

    #[test]
    fn conversations_loaded_skips_reload_when_active_conversation_gone() {
        // The active conversation was deleted while disconnected: don't try to
        // reload it (EnsureActiveConversation switches to another / creates one).
        let mut state = WindowState {
            current_conversation_id: Some("gone".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationsLoaded(vec![summary(
            "c1", "first", false,
        )]));
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::SetConversations(_),
                    Effect::EnsureActiveConversation
                ]
            ),
            "must not reload a conversation that's no longer present: {effects:?}"
        );
    }

    #[test]
    fn conversation_reloaded_refreshes_cache_and_chat_without_touching_picker() {
        // Issue #72: a reload refreshes the cached detail + chat but must NOT
        // emit SetModelSelection (the picker keeps the user's pick).
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let mut d = detail("c1", vec![msg("user", "hi")]);
        d.model_selection = Some(selection("work", "claude"));
        let effects = state.apply(UiMessage::ConversationReloaded(d));
        assert!(
            state.current_conversation.is_some(),
            "cache must be updated"
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::SetModelSelection(_))),
            "reload must not touch the picker: {effects:?}"
        );
        match effects.as_slice() {
            [
                Effect::LoadConversationIntoChat(_),
                Effect::SidePaneSetScratchpad(_),
                Effect::RefreshSidePaneTasks,
                Effect::FetchScratchpad(conv),
            ] => assert_eq!(conv, "c1"),
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn conversation_reloaded_ignored_when_user_switched_away() {
        // A reload reply that arrives after the user switched conversations must
        // be dropped — it would otherwise overwrite the now-current chat.
        let mut state = WindowState {
            current_conversation_id: Some("c2".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationReloaded(detail("c1", vec![])));
        assert!(
            effects.is_empty(),
            "stale reload for a non-active conversation must be a no-op: {effects:?}"
        );
    }

    #[test]
    fn default_model_loaded_emits_set_default_model_effect() {
        let mut state = WindowState::default();
        let default = crate::selected_models::SelectedModel {
            connection_id: "work".to_string(),
            model_id: "claude".to_string(),
        };
        let effects = state.apply(UiMessage::DefaultModelLoaded(Some(default.clone())));
        match effects.as_slice() {
            [Effect::SetDefaultModel(Some(got))] => {
                assert_eq!(got.connection_id, "work");
                assert_eq!(got.model_id, "claude");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn default_model_loaded_none_emits_set_default_model_none() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::DefaultModelLoaded(None));
        assert!(
            matches!(effects.as_slice(), [Effect::SetDefaultModel(None)]),
            "unresolved default must still emit a (None) effect: {effects:?}"
        );
    }

    #[test]
    fn dangling_model_warning_for_current_conversation_clears_picker_and_cached_selection() {
        let mut conv = detail("c1", vec![]);
        conv.model_selection = Some(selection("old", "gone"));
        let mut state = WindowState {
            current_conversation: Some(conv),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let warning = api::ConversationWarning::DanglingModelSelection {
            previous_selection: selection("old", "gone"),
            fallback_to: selection("work", "claude"),
        };
        let effects = state.apply(UiMessage::ConversationWarning {
            conversation_id: "c1".to_string(),
            warning,
        });
        // Cached selection must be cleared so a later reload/switch doesn't
        // re-apply the stale dangling selection, contradicting the toast.
        assert!(
            state
                .current_conversation
                .as_ref()
                .unwrap()
                .model_selection
                .is_none()
        );
        match effects.as_slice() {
            [Effect::SetModelSelection(None), Effect::ShowToast(message)] => {
                assert!(message.contains("gone"));
                assert!(message.contains("claude"));
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn dangling_model_warning_for_other_conversation_only_toasts() {
        let mut conv = detail("c1", vec![]);
        conv.model_selection = Some(selection("old", "gone"));
        let mut state = WindowState {
            current_conversation: Some(conv),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let warning = api::ConversationWarning::DanglingModelSelection {
            previous_selection: selection("old", "gone"),
            fallback_to: selection("work", "claude"),
        };
        let effects = state.apply(UiMessage::ConversationWarning {
            conversation_id: "c2-not-current".to_string(),
            warning,
        });
        // Not the current conversation: don't touch the picker or cached
        // selection — only surface the advisory toast.
        assert!(
            state
                .current_conversation
                .as_ref()
                .unwrap()
                .model_selection
                .is_some()
        );
        assert!(
            matches!(effects.as_slice(), [Effect::ShowToast(_)]),
            "unexpected effects: {effects:?}"
        );
    }

    // --- Simple passthrough variants -------------------------------------

    #[test]
    fn status_update_sets_status_text_verbatim() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::StatusUpdate("Connecting".to_string()));
        assert!(matches!(effects.as_slice(), [Effect::SetStatusText(t)] if t == "Connecting"));
    }

    #[test]
    fn error_message_is_prefixed_in_status_bar() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::Error("nope".to_string()));
        assert!(matches!(effects.as_slice(), [Effect::SetStatusText(t)] if t == "Error: nope"));
    }

    #[test]
    fn connected_sets_label_and_enables_send() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::Connected {
            label: "Local daemon".to_string(),
        });
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::SetStatusText(t), Effect::SetSendSensitive(true)] if t == "Local daemon"
            ),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn conversation_created_sets_active_id_without_effects() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::ConversationCreated {
            id: "new-c".to_string(),
        });
        assert_eq!(state.current_conversation_id.as_deref(), Some("new-c"));
        assert!(effects.is_empty());
    }

    // --- Voice UI: You/Adele dropdowns + client tools (issue #80) --------

    /// A `say_this` client-tool call (#76, still used in #80). Convenience
    /// constructor for the tests below.
    fn say_this_call(conversation_id: &str, text: &str) -> UiMessage {
        UiMessage::ClientToolCall {
            task_id: "task-1".to_string(),
            conversation_id: conversation_id.to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "say_this".to_string(),
            arguments: serde_json::json!({ "text": text }),
        }
    }

    /// A `request_voice` / `stop_voice` client-tool call (#80). Convenience
    /// constructor mirroring `say_this_call`.
    fn voice_tool_call(conversation_id: &str, tool_name: &str) -> UiMessage {
        UiMessage::ClientToolCall {
            task_id: "task-v".to_string(),
            conversation_id: conversation_id.to_string(),
            tool_call_id: "call-v".to_string(),
            tool_name: tool_name.to_string(),
            arguments: serde_json::json!({}),
        }
    }

    /// A `WindowState` pinned to conversation `c1` with the given `You:` and
    /// `Adele:` settings — the common test fixture for the gate tests below.
    fn state_with(voice_in: bool, adele: AdeleOutput) -> WindowState {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        state
            .conversation_voice_in
            .insert("c1".to_string(), voice_in);
        state
            .conversation_adele_output
            .insert("c1".to_string(), adele);
        state
    }

    /// A `StreamComplete` for `c1` carrying `full_response`, against a freshly
    /// pinned pending request — the reply-narration trigger.
    fn stream_complete_in(state: &mut WindowState, full_response: &str) -> Vec<Effect> {
        state.stream = Some(StreamState {
            request_id: Some("req".to_string()),
            conversation_id: "c1".to_string(),
            ..Default::default()
        });
        state.current_conversation = Some(detail("c1", vec![]));
        state.apply(UiMessage::StreamComplete {
            request_id: "req".to_string(),
            full_response: full_response.to_string(),
        })
    }

    /// Default (You=Disabled, Adele=Disabled): both controls default off for an
    /// untouched conversation, so no audio path can fire.
    #[test]
    fn defaults_are_voice_in_disabled_and_adele_disabled() {
        let state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        assert!(
            !state.voice_in_for_current(),
            "You must default Disabled for an untouched conversation"
        );
        assert_eq!(
            state.adele_output_for_current(),
            AdeleOutput::Disabled,
            "Adele must default Disabled for an untouched conversation"
        );
        assert!(!state.narrate_for_current(), "default gate must be closed");
        assert!(
            !state.say_this_spoken_for_current(),
            "default say_this must downgrade to inline"
        );
    }

    /// Default: a `say_this` produces the inline `(speech mode disabled) …`
    /// downgrade, NO `Speak`, and ALWAYS a `SubmitClientToolResult` (the turn
    /// completes, can't hang).
    #[test]
    fn default_say_this_renders_inline_and_resolves_without_audio() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(say_this_call("c1", "the aside"));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "Adele Disabled must never produce a Speak effect: {effects:?}"
        );
        let inline = effects.iter().any(
            |e| matches!(e, Effect::AddInlineNote(t) if t == "(speech mode disabled) the aside"),
        );
        assert!(inline, "expected the inline downgrade note: {effects:?}");
        let resolved = effects.iter().any(|e| {
            matches!(
                e,
                Effect::SubmitClientToolResult { task_id, tool_call_id, result: Ok(_) }
                    if task_id == "task-1" && tool_call_id == "call-1"
            )
        });
        assert!(
            resolved,
            "say_this must always resolve a result: {effects:?}"
        );
    }

    /// Adele=Always: every reply is spoken (and finalized), independent of You.
    #[test]
    fn adele_always_speaks_every_reply_regardless_of_you() {
        for voice_in in [false, true] {
            let mut state = state_with(voice_in, AdeleOutput::Always);
            assert!(
                state.narrate_for_current(),
                "Always must narrate (You={voice_in})"
            );
            let effects = stream_complete_in(&mut state, "an answer");
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::Speak(t) if t == "an answer")),
                "Always must speak the reply (You={voice_in}): {effects:?}"
            );
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::CompleteStreaming(t) if t == "an answer")),
                "the reply text must still be finalized: {effects:?}"
            );
        }
    }

    // --- Live external-turn rendering (#1) --------------------------------

    /// A `UserMessageAdded` for the open conversation, with no gtk turn in
    /// flight, is a turn this client did not initiate (voice / another client).
    /// It renders the user bubble and adopts the turn into the pending slot so
    /// the reply streams live.
    #[test]
    fn external_user_message_in_active_conversation_renders_bubble_and_adopts() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            current_conversation: Some(detail("c1", vec![])),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "voice-req".to_string(),
            content: "what's the weather?".to_string(),
        });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::AddUserMessage(t) if t == "what's the weather?")),
            "an external turn in the open conversation must render the user bubble: {effects:?}"
        );
        assert_eq!(
            state.stream_request_id(),
            Some("voice-req"),
            "the external turn must be adopted into the pending slot so its reply streams live"
        );
        assert_eq!(state.stream_conversation_id(), Some("c1"));
        assert!(
            state.stream_external(),
            "an adopted turn must be flagged external so gtk does not also narrate it"
        );
        assert_eq!(
            state.current_conversation.as_ref().unwrap().messages.len(),
            1,
            "the user message must be cached so a reload keeps it"
        );
    }

    /// This client's own send is echoed back as `UserMessageAdded`. The bubble
    /// was already drawn optimistically at send time, so the echo renders
    /// nothing — it only claims the real `request_id` onto the `__pending__`
    /// slot (so the stream correlates).
    #[test]
    fn own_send_echo_dedupes_and_claims_request_id() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        // Simulate the local send: PromptSent pins "__pending__" + the conv.
        state.apply(UiMessage::PromptSent {
            task_id: String::new(),
            conversation_id: "c1".to_string(),
        });
        let effects = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "real-req".to_string(),
            content: "typed this".to_string(),
        });
        assert!(
            effects.is_empty(),
            "our own send's echo must not double-render the bubble: {effects:?}"
        );
        assert_eq!(
            state.stream_request_id(),
            Some("real-req"),
            "the echo must claim the real request_id off the __pending__ sentinel"
        );
        assert!(
            !state.stream_external(),
            "our own turn must NOT be flagged external (gtk owns its narration)"
        );
    }

    /// An adopted external turn streams its reply into the view but is NOT
    /// narrated by gtk even when the conversation's gate is open — the
    /// originator (e.g. the voice daemon) already speaks it; narrating again
    /// would double-speak.
    #[test]
    fn adopted_external_turn_streams_reply_without_gtk_narration() {
        // Adele=Always would normally narrate every reply.
        let mut state = state_with(false, AdeleOutput::Always);
        state.current_conversation = Some(detail("c1", vec![]));
        state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "voice-req".to_string(),
            content: "a question".to_string(),
        });
        let done = state.apply(UiMessage::StreamComplete {
            request_id: "voice-req".to_string(),
            full_response: "the spoken answer".to_string(),
        });
        assert!(
            done.iter()
                .any(|e| matches!(e, Effect::CompleteStreaming(t) if t == "the spoken answer")),
            "the reply text must still be finalized in the view: {done:?}"
        );
        assert!(
            !done.iter().any(|e| matches!(e, Effect::Speak(_))),
            "an external turn must NOT be narrated by gtk (the originator speaks it): {done:?}"
        );
        assert!(
            !state.stream_external(),
            "the external flag must reset at turn completion"
        );
    }

    /// A `UserMessageAdded` for a conversation NOT in view is left to the
    /// reload-on-switch path — it must not touch the open chat or the pending
    /// slot.
    #[test]
    fn external_turn_for_background_conversation_is_ignored() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            current_conversation: Some(detail("c1", vec![])),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c2".to_string(),
            request_id: "bg-req".to_string(),
            content: "background".to_string(),
        });
        assert!(
            effects.is_empty(),
            "a background conversation's turn must not render into the open chat: {effects:?}"
        );
        assert!(
            !state.is_streaming(),
            "a background turn must not be adopted into the pending slot"
        );
    }

    /// While this client's own turn is in flight (request_id already claimed),
    /// a concurrent external turn for the same conversation is NOT adopted — the
    /// single in-flight slot stays bound to our turn (the external turn surfaces
    /// on reload).
    #[test]
    fn external_turn_ignored_while_own_turn_in_flight() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            current_conversation: Some(detail("c1", vec![])),
            ..Default::default()
        };
        state.stream = Some(StreamState {
            request_id: Some("mine".to_string()),
            conversation_id: "c1".to_string(),
            ..Default::default()
        });
        let effects = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "other".to_string(),
            content: "concurrent".to_string(),
        });
        assert!(
            effects.is_empty(),
            "must not adopt a second turn while one is in flight: {effects:?}"
        );
        assert_eq!(
            state.stream_request_id(),
            Some("mine"),
            "the in-flight turn's slot must be preserved"
        );
    }

    /// Adele=Always: a `say_this` aside is spoken (Adele ∈ {OnDemand, Always}).
    #[test]
    fn adele_always_speaks_say_this_aside() {
        let mut state = state_with(false, AdeleOutput::Always);
        let effects = state.apply(say_this_call("c1", "hello aloud"));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Speak(t) if t == "hello aloud")),
            "Always must speak a say_this aside: {effects:?}"
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::AddInlineNote(_))),
            "no inline downgrade when spoken: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::SubmitClientToolResult { result: Ok(r), .. } if r == "spoken"
            )),
            "result must be \"spoken\": {effects:?}"
        );
    }

    /// Double-speak fix: when a `say_this` aside is spoken for the in-flight
    /// turn, the `StreamComplete` reply narration is suppressed — the user hears
    /// the aside once, not the aside AND the whole reply read aloud after.
    #[test]
    fn say_this_aside_suppresses_duplicate_reply_narration() {
        // Always (and OnDemand+You) would otherwise narrate every reply.
        let mut state = state_with(true, AdeleOutput::Always);
        // Simulate an in-flight turn for the open conversation.
        state.stream = Some(StreamState {
            request_id: Some("req".to_string()),
            conversation_id: "c1".to_string(),
            ..Default::default()
        });
        state.current_conversation = Some(detail("c1", vec![]));

        // The model speaks an aside mid-turn.
        let aside = state.apply(say_this_call("c1", "the spoken answer"));
        assert!(
            aside
                .iter()
                .any(|e| matches!(e, Effect::Speak(t) if t == "the spoken answer")),
            "the say_this aside should be spoken: {aside:?}"
        );

        // The turn completes: the full reply must NOT be read aloud again.
        let done = state.apply(UiMessage::StreamComplete {
            request_id: "req".to_string(),
            full_response: "the spoken answer, in more words".to_string(),
        });
        assert!(
            !done.iter().any(|e| matches!(e, Effect::Speak(_))),
            "no second narration after a spoken aside (double-speak): {done:?}"
        );
        assert!(
            done.iter()
                .any(|e| matches!(e, Effect::CompleteStreaming(_))),
            "the reply is still finalized in the chat: {done:?}"
        );
    }

    /// Adele=OnDemand + You=Enabled: the reply is spoken (the gate's OnDemand
    /// arm) and finalized.
    #[test]
    fn adele_on_demand_with_you_enabled_speaks_reply() {
        let mut state = state_with(true, AdeleOutput::OnDemand);
        assert!(
            state.narrate_for_current(),
            "OnDemand + You=Enabled narrates"
        );
        let effects = stream_complete_in(&mut state, "an answer");
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Speak(t) if t == "an answer")),
            "OnDemand + You=Enabled must speak the reply: {effects:?}"
        );
    }

    /// Adele=OnDemand + You=Disabled: the reply is NOT spoken (text-only), but a
    /// `say_this` aside still speaks (asides ignore You).
    #[test]
    fn adele_on_demand_with_you_disabled_text_only_but_say_this_speaks() {
        // Reply NOT narrated.
        let mut state = state_with(false, AdeleOutput::OnDemand);
        assert!(
            !state.narrate_for_current(),
            "OnDemand + You=Disabled must not narrate replies"
        );
        let effects = stream_complete_in(&mut state, "an answer");
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "OnDemand + You=Disabled must not speak the reply: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::CompleteStreaming(t) if t == "an answer")),
            "the reply text must still be finalized: {effects:?}"
        );

        // say_this aside STILL speaks (independent of You).
        let mut state = state_with(false, AdeleOutput::OnDemand);
        let effects = state.apply(say_this_call("c1", "an aside"));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Speak(t) if t == "an aside")),
            "OnDemand say_this aside must speak even when You=Disabled: {effects:?}"
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::AddInlineNote(_))),
            "no inline downgrade when spoken: {effects:?}"
        );
    }

    // --- GTK-3: the AdeleOutput gate is the ONLY narration path -----------
    // The legacy #65 `voice_reply_pending` hook (which spoke every dictated
    // turn's reply regardless of the gate, and double-spoke alongside
    // `Effect::Speak`) was deleted. These pin the post-deletion contract: a
    // dictated turn narrates iff the conversation's gate holds, and never more
    // than once.

    /// GTK-3 acceptance: a dictated turn whose conversation has `Adele ==
    /// Disabled` produces ZERO `Speak` effects (the gate is the only narration
    /// path; dictation no longer force-speaks). The reply is still finalized.
    #[test]
    fn disabled_conversation_dictated_turn_emits_no_speak() {
        // `You == Enabled` models a dictated turn; `Adele == Disabled` is the
        // default output level. The old legacy hook would have spoken anyway.
        let mut state = state_with(true, AdeleOutput::Disabled);
        let effects = stream_complete_in(&mut state, "a silent reply");
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "Adele=Disabled must never narrate, even a dictated turn: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::CompleteStreaming(t) if t == "a silent reply")),
            "the reply text must still be finalized: {effects:?}"
        );
    }

    /// GTK-3 acceptance: a dictated turn whose conversation narrates (`Adele ==
    /// OnDemand` AND `You == Enabled`) produces EXACTLY ONE `Speak` effect — the
    /// reducer is the single narration source, so there is no double-speak.
    #[test]
    fn narrating_conversation_dictated_turn_emits_exactly_one_speak() {
        let mut state = state_with(true, AdeleOutput::OnDemand);
        let effects = stream_complete_in(&mut state, "spoken once");
        let speaks = effects
            .iter()
            .filter(|e| matches!(e, Effect::Speak(t) if t == "spoken once"))
            .count();
        assert_eq!(
            speaks, 1,
            "exactly one Speak — no legacy hook double-narration: {effects:?}"
        );
    }

    /// `request_voice` sets Adele=OnDemand for the active conversation, reflects
    /// the dropdown, and ALWAYS resolves a result (no audio by itself).
    #[test]
    fn request_voice_sets_on_demand_reflects_and_resolves() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(voice_tool_call("c1", "request_voice"));
        assert_eq!(
            state.adele_output_for_current(),
            AdeleOutput::OnDemand,
            "request_voice must set Adele=OnDemand for the active conversation"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SetAdeleOutputDropdown(AdeleOutput::OnDemand))),
            "request_voice must reflect OnDemand on the dropdown: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::SubmitClientToolResult { task_id, tool_call_id, result: Ok(_) }
                    if task_id == "task-v" && tool_call_id == "call-v"
            )),
            "request_voice must resolve an Ok result: {effects:?}"
        );
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "request_voice itself must not speak: {effects:?}"
        );
    }

    /// `stop_voice` sets Adele=Disabled, reflects the dropdown, and ALWAYS
    /// resolves a result.
    #[test]
    fn stop_voice_sets_disabled_reflects_and_resolves() {
        let mut state = state_with(true, AdeleOutput::Always);
        let effects = state.apply(voice_tool_call("c1", "stop_voice"));
        assert_eq!(
            state.adele_output_for_current(),
            AdeleOutput::Disabled,
            "stop_voice must set Adele=Disabled"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SetAdeleOutputDropdown(AdeleOutput::Disabled))),
            "stop_voice must reflect Disabled on the dropdown: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::SubmitClientToolResult { task_id, tool_call_id, result: Ok(_) }
                    if task_id == "task-v" && tool_call_id == "call-v"
            )),
            "stop_voice must resolve an Ok result: {effects:?}"
        );
    }

    /// Every client-tool call emits exactly one result (no wedge, no double),
    /// across say_this / request_voice / stop_voice / an unknown tool.
    #[test]
    fn every_client_tool_call_emits_exactly_one_result() {
        let calls = [
            say_this_call("c1", "x"),
            voice_tool_call("c1", "request_voice"),
            voice_tool_call("c1", "stop_voice"),
            UiMessage::ClientToolCall {
                task_id: "t".to_string(),
                conversation_id: "c1".to_string(),
                tool_call_id: "tc".to_string(),
                tool_name: "frobnicate".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        for call in calls {
            let mut state = WindowState {
                current_conversation_id: Some("c1".to_string()),
                ..Default::default()
            };
            let effects = state.apply(call);
            let results = effects
                .iter()
                .filter(|e| matches!(e, Effect::SubmitClientToolResult { .. }))
                .count();
            assert_eq!(
                results, 1,
                "exactly one result per client-tool call: {effects:?}"
            );
        }
    }

    /// An unknown client tool the GTK client can't run still ALWAYS gets an
    /// `Err` result (no audio), so the suspended turn resumes rather than
    /// wedging.
    #[test]
    fn unknown_client_tool_always_resolves_with_error_result() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ClientToolCall {
            task_id: "task-2".to_string(),
            conversation_id: "c1".to_string(),
            tool_call_id: "call-2".to_string(),
            tool_name: "frobnicate".to_string(),
            arguments: serde_json::json!({}),
        });
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "an unknown tool must not produce audio: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::SubmitClientToolResult { task_id, tool_call_id, result: Err(_) }
                    if task_id == "task-2" && tool_call_id == "call-2"
            )),
            "an unrunnable tool must resolve with an Err result: {effects:?}"
        );
    }

    /// Malformed `say_this` arguments (missing/invalid `text`) must not panic
    /// and must resolve with an `Err` result (never unwrap), even with Adele on.
    #[test]
    fn say_this_with_malformed_arguments_resolves_error_not_panic() {
        let mut state = state_with(true, AdeleOutput::Always);
        let effects = state.apply(UiMessage::ClientToolCall {
            task_id: "task-3".to_string(),
            conversation_id: "c1".to_string(),
            tool_call_id: "call-3".to_string(),
            tool_name: "say_this".to_string(),
            // `text` missing entirely.
            arguments: serde_json::json!({ "wrong": 5 }),
        });
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "malformed say_this must not speak: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SubmitClientToolResult { result: Err(_), .. })),
            "malformed say_this must resolve an Err result: {effects:?}"
        );
    }

    /// `request_voice` / `stop_voice` with malformed/non-object args still
    /// resolve exactly one result without panicking (they take no arguments).
    #[test]
    fn voice_tools_with_malformed_args_resolve_without_panic() {
        for tool in ["request_voice", "stop_voice"] {
            let mut state = WindowState {
                current_conversation_id: Some("c1".to_string()),
                ..Default::default()
            };
            let effects = state.apply(UiMessage::ClientToolCall {
                task_id: "t".to_string(),
                conversation_id: "c1".to_string(),
                tool_call_id: "tc".to_string(),
                tool_name: tool.to_string(),
                arguments: serde_json::json!("not-an-object"),
            });
            let results = effects
                .iter()
                .filter(|e| matches!(e, Effect::SubmitClientToolResult { .. }))
                .count();
            assert_eq!(
                results, 1,
                "{tool} must resolve exactly one result: {effects:?}"
            );
        }
    }

    /// Both controls are per-conversation and isolated: setting them on c1 must
    /// not leak into c2, and they stick when switching back.
    #[test]
    fn both_controls_are_per_conversation_isolated() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        state.apply(UiMessage::SetVoiceIn {
            conversation_id: "c1".to_string(),
            enabled: true,
        });
        state.apply(UiMessage::SetAdeleOutput {
            conversation_id: "c1".to_string(),
            level: AdeleOutput::Always,
        });
        assert!(state.voice_in_for_current());
        assert_eq!(state.adele_output_for_current(), AdeleOutput::Always);

        // Switch to c2: both inherit their defaults (no bleed).
        state.current_conversation_id = Some("c2".to_string());
        assert!(!state.voice_in_for_current(), "You must not leak c1 → c2");
        assert_eq!(
            state.adele_output_for_current(),
            AdeleOutput::Disabled,
            "Adele must not leak c1 → c2"
        );

        // Back to c1: both stick.
        state.current_conversation_id = Some("c1".to_string());
        assert!(state.voice_in_for_current());
        assert_eq!(state.adele_output_for_current(), AdeleOutput::Always);
    }

    // --- GTK-4: client tools keyed off the *call's* conversation ----------

    /// GTK-4 acceptance: a `say_this` for a background conversation produces
    /// no audio — even when that conversation's own gate is open — and the
    /// text is downgraded to an inline note so it isn't lost. The turn still
    /// resolves exactly once.
    #[test]
    fn say_this_for_background_conversation_no_audio_inline_note() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        // The call's conversation has speech wide open — but it isn't in view.
        state
            .conversation_adele_output
            .insert("c2".to_string(), AdeleOutput::Always);
        let effects = state.apply(say_this_call("c2", "background aside"));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "a background conversation's say_this must never play audio: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::AddInlineNote(t) if t.contains("background aside"))),
            "the aside must be shown as text instead: {effects:?}"
        );
        let results = effects
            .iter()
            .filter(|e| matches!(e, Effect::SubmitClientToolResult { result: Ok(_), .. }))
            .count();
        assert_eq!(results, 1, "exactly one Ok result: {effects:?}");
    }

    /// GTK-4: a background `say_this` must not borrow the *active*
    /// conversation's open gate either — the old code gated on the active
    /// conversation and played the foreign aside under it.
    #[test]
    fn background_say_this_does_not_borrow_active_conversations_gate() {
        let mut state = state_with(true, AdeleOutput::Always); // active c1, gate open
        let effects = state.apply(say_this_call("c2", "should not play"));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "c2's aside must not play under c1's gate: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::AddInlineNote(_))),
            "the aside downgrades to text: {effects:?}"
        );
    }

    /// GTK-4: the `say_this` gate is keyed off the call's conversation when it
    /// IS the active one — `Disabled` there downgrades to the inline note even
    /// if some other conversation has speech on.
    #[test]
    fn active_say_this_gates_on_its_own_conversations_level() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        state
            .conversation_adele_output
            .insert("c9".to_string(), AdeleOutput::Always); // unrelated
        let effects = state.apply(say_this_call("c1", "quiet aside"));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "c1 is Disabled; no audio: {effects:?}"
        );
        assert!(
            effects.iter().any(
                |e| matches!(e, Effect::AddInlineNote(t) if t == "(speech mode disabled) quiet aside")
            ),
            "expected the inline downgrade note: {effects:?}"
        );
    }

    /// GTK-4 acceptance: `request_voice` for a background conversation flips
    /// THAT conversation's level — not the viewed one's — and does not touch
    /// the dropdown (which reflects the viewed conversation). Still resolves.
    #[test]
    fn request_voice_targets_call_conversation_when_backgrounded() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(voice_tool_call("c2", "request_voice"));
        assert_eq!(
            state.conversation_adele_output.get("c2").copied(),
            Some(AdeleOutput::OnDemand),
            "request_voice must write the call's conversation"
        );
        assert_eq!(
            state.adele_output_for_current(),
            AdeleOutput::Disabled,
            "the viewed conversation must not be flipped into voice mode"
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::SetAdeleOutputDropdown(_))),
            "the dropdown shows the viewed conversation; a background change must not touch it: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SubmitClientToolResult { result: Ok(_), .. })),
            "still always resolves: {effects:?}"
        );
    }

    /// GTK-4: `stop_voice` for a background conversation clears THAT
    /// conversation's level, leaves the viewed one alone, and skips the
    /// dropdown.
    #[test]
    fn stop_voice_targets_call_conversation_when_backgrounded() {
        let mut state = state_with(true, AdeleOutput::OnDemand); // viewed c1
        state
            .conversation_adele_output
            .insert("c2".to_string(), AdeleOutput::Always);
        let effects = state.apply(voice_tool_call("c2", "stop_voice"));
        assert_eq!(
            state.conversation_adele_output.get("c2").copied(),
            Some(AdeleOutput::Disabled),
            "stop_voice must write the call's conversation"
        );
        assert_eq!(
            state.adele_output_for_current(),
            AdeleOutput::OnDemand,
            "the viewed conversation must keep its level"
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::SetAdeleOutputDropdown(_))),
            "a background change must not touch the dropdown: {effects:?}"
        );
    }

    /// `refinement_for_send` returns the right variant per (Adele level, You):
    /// Disabled → none; OnDemand → the brief/conversational refinement; Always →
    /// the speakable-but-full refinement. `You` does not change the refinement
    /// (it's chosen by the Adele level), and both refinement strings are
    /// non-empty and free of markdown markers so they can't leak formatting.
    #[test]
    fn refinement_for_send_returns_right_variant_per_level() {
        // Disabled → none (independent of You).
        for voice_in in [false, true] {
            let state = state_with(voice_in, AdeleOutput::Disabled);
            assert!(
                refinement_for_send(&state).is_none(),
                "Adele=Disabled must attach no refinement (You={voice_in})"
            );
        }
        // OnDemand → the brief refinement (independent of You).
        for voice_in in [false, true] {
            let state = state_with(voice_in, AdeleOutput::OnDemand);
            assert_eq!(
                refinement_for_send(&state),
                Some(adele_voice_client_common::ON_DEMAND_SYSTEM_REFINEMENT),
                "Adele=OnDemand must attach the brief refinement (You={voice_in})"
            );
        }
        // Always → the full refinement (independent of You).
        for voice_in in [false, true] {
            let state = state_with(voice_in, AdeleOutput::Always);
            assert_eq!(
                refinement_for_send(&state),
                Some(adele_voice_client_common::ALWAYS_SYSTEM_REFINEMENT),
                "Adele=Always must attach the full refinement (You={voice_in})"
            );
        }
        // The two refinements differ, are non-empty, and carry no markdown.
        use adele_voice_client_common::{ALWAYS_SYSTEM_REFINEMENT, ON_DEMAND_SYSTEM_REFINEMENT};
        assert_ne!(ON_DEMAND_SYSTEM_REFINEMENT, ALWAYS_SYSTEM_REFINEMENT);
        // OnDemand asks for brevity; Always explicitly does not shorten.
        assert!(ON_DEMAND_SYSTEM_REFINEMENT.to_lowercase().contains("brief"));
        assert!(
            ALWAYS_SYSTEM_REFINEMENT
                .to_lowercase()
                .contains("do not shorten")
        );
        for refinement in [ON_DEMAND_SYSTEM_REFINEMENT, ALWAYS_SYSTEM_REFINEMENT] {
            assert!(!refinement.trim().is_empty());
            for marker in ['*', '_', '`', '#'] {
                assert!(
                    !refinement.contains(marker),
                    "the refinement itself must avoid markdown markers ({marker})"
                );
            }
        }
    }

    /// A user-driven `SetVoiceIn` records the per-conversation state and emits
    /// no effects.
    #[test]
    fn set_voice_in_records_state_scoped_to_conversation() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::SetVoiceIn {
            conversation_id: "c1".to_string(),
            enabled: true,
        });
        assert!(effects.is_empty(), "setting You emits no effects");
        assert!(state.voice_in_for_current());
        state.current_conversation_id = Some("c2".to_string());
        assert!(
            !state.voice_in_for_current(),
            "You set on c1 must not leak into c2"
        );
    }

    /// A user-driven `SetAdeleOutput` records the per-conversation level and
    /// emits no effects.
    #[test]
    fn set_adele_output_records_state_scoped_to_conversation() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::SetAdeleOutput {
            conversation_id: "c1".to_string(),
            level: AdeleOutput::OnDemand,
        });
        assert!(effects.is_empty(), "setting Adele emits no effects");
        assert_eq!(state.adele_output_for_current(), AdeleOutput::OnDemand);
        state.current_conversation_id = Some("c2".to_string());
        assert_eq!(
            state.adele_output_for_current(),
            AdeleOutput::Disabled,
            "Adele set on c1 must not leak into c2"
        );
    }
}
