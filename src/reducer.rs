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
use desktop_assistant_api_model::client::{
    ChatMessage, ConversationDetail, ConversationSummary, MessageKind,
};

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
    /// The accumulated reply text. Belongs to the conversation whose model holds
    /// this stream and re-seeds the view if the user switches back to it
    /// mid-stream. (The owning conversation is the [`open`](WindowState::open)
    /// map key now — Phase-2 Step-2b-ii — so the stream no longer carries a
    /// redundant `conversation_id` of its own; "the stream knows its
    /// conversation" via where it lives.)
    buffer: String,
    /// Set when a `say_this` aside is *spoken* for this turn; a defensive
    /// backstop that suppresses full-reply narration at `StreamComplete` so the
    /// user can't hear the turn twice. `say_this` speaks only in on-demand mode
    /// (voice#126), which never auto-narrates, so the two paths are already
    /// mutually exclusive — this keeps the guarantee if the modes ever change.
    /// Only relevant to gtk-initiated turns (the only ones gtk narrates).
    say_this_spoken_this_turn: bool,
    /// `true` when this turn was NOT initiated by this client — adopted from a
    /// `UserMessageAdded` for a turn started elsewhere (a voice turn, or another
    /// client on the same account) so its reply streams live into the open
    /// conversation (#1). Suppresses gtk's own reply narration for it: the
    /// originator (e.g. the voice daemon) already speaks the reply, so narrating
    /// again here would double-speak.
    external: bool,
    /// The daemon-registered background-task id for this turn — the handle
    /// `CancelBackgroundTask { id }` acts on (#138) — captured from the
    /// `PromptSent` ack at send time so a view can offer **Cancel** while the
    /// turn is in flight (surfaced via [`WindowState::active_task_id_for_view`]).
    /// Empty when unknown: a legacy daemon's id-less ack, or an
    /// [`external`](Self::external) turn adopted from elsewhere (this client
    /// never received that turn's ack, so it holds no handle to cancel it).
    task_id: String,
    /// The client-minted idempotency key of the send that started this turn
    /// (#570), carried through to [`Effect::TurnFinished`] so a host can name
    /// the submit a completion closes out (#51). Echoed onto this stream by the
    /// [`UiMessage::PromptSent`] ack that opened it, which is the only place
    /// that knows which send it answers. `None` for a keyless send and for an
    /// [`external`](Self::external) turn this client never sent.
    idempotency_key: Option<String>,
}

/// Separator used to fold several queued messages into one combined prompt on
/// flush. A blank line (an EOL plus an empty line) separates each queued
/// message, so a burst of Enters reads as distinct paragraphs rather than
/// run-together lines.
const QUEUE_JOIN: &str = "\n\n";

/// One message awaiting flush in a conversation's outbox. Carries the text plus
/// the client-minted idempotency key (#570) supplied when it was queued, so the
/// combined flush can adopt the first message's key and the echoed
/// `UserMessageAdded` still dedupes by exact match. `idempotency_key` is `None`
/// for a keyless send.
#[derive(Debug, Clone)]
struct QueuedMessage {
    text: String,
    idempotency_key: Option<String>,
}

/// A queued message the user has pulled back into the composer to edit
/// ([`ConversationModel::editing`]). Records where it came from so a re-submit
/// reinserts it in place and a cancel restores it unchanged.
#[derive(Debug, Clone)]
struct QueuedEdit {
    /// The outbox slot the message was checked out from; a re-submit reinserts
    /// the edited text here (clamped to the current queue length).
    index: usize,
    /// The message's text as it was when checked out, so `CancelQueuedEdit`
    /// restores it even if the composer was edited.
    original: String,
    /// The checked-out message's idempotency key (#570), preserved so a
    /// reinsert (finish-edit or cancel) keeps the original queued key rather
    /// than dropping it.
    key: Option<String>,
}

/// One conversation's view-model — all of its per-conversation state in one
/// place, keyed by conversation id in [`WindowState::open`] so it's found by
/// identity. Holds the loaded transcript (`detail`, optional so a model can
/// outlive its transcript), the `You:`/`Adele:` voice settings, the unsent
/// composer draft, and the in-flight streaming reply. The composer narrowing
/// (#2), the voice settings, and (Phase-2 Step-2b-ii) the `stream` used to live
/// as flat side-maps / a window-level field on `WindowState`; folding them here
/// is the per-conversation consolidation — every conversation now owns its own
/// stream, so several can stream concurrently in the background.
#[derive(Debug, Clone, Default)]
struct ConversationModel {
    /// The loaded conversation transcript + metadata, or `None` when this model
    /// exists only to retain per-conversation state for a conversation whose
    /// transcript isn't currently loaded — a backgrounded conversation whose
    /// detail was evicted on switch-away to bound memory (it re-fetches on
    /// switch-back), or one a voice/draft was set on before its detail loaded.
    detail: Option<ConversationDetail>,
    /// `You:` (voice input) Enabled for this conversation (issue #80); default
    /// (Disabled) for a fresh model.
    voice_in: bool,
    /// `Adele:` (voice output) level for this conversation (issue #80).
    adele_output: AdeleOutput,
    /// The unsent composer draft for this conversation (the composer narrowing,
    /// #2) — empty when there is no draft.
    composer: String,
    /// Messages the user submitted into this conversation *while a reply was
    /// streaming* (the send was "not allowed"). Held in submit order and
    /// flushed as ONE combined submission when the stream ends — so a burst of
    /// Enter-presses ("I hit enter as I think") becomes a single turn, not
    /// several. Empty when nothing is queued.
    outbox: Vec<QueuedMessage>,
    /// Messages from a flush that has been sent but not yet acked, parked here so
    /// a failed or abandoned flush can restore them to `outbox` for retry rather
    /// than dropping them (#25). Empty except between a flush's `SendPrompt` and
    /// its `PromptSent` (cleared) or `SendFailed` (restored). Invisible to the
    /// view — the optimistic bubble represents the in-flight send, so the queue
    /// indicator reads `outbox` only (keeps the #24 no-double-render fix).
    pending_flush: Vec<QueuedMessage>,
    /// Set when the user has pulled a queued message back into the composer to
    /// edit it (up-arrow recall / a chip's edit affordance): the outbox slot it
    /// was checked out from (so a re-submit reinserts it in place) and its
    /// original text (so a cancel restores it unchanged). `None` when composing
    /// a fresh message. The checked-out message lives in the composer, not the
    /// outbox, while this is `Some`.
    editing: Option<QueuedEdit>,
    /// This conversation's in-flight streaming reply, or `None` when no turn is
    /// streaming for it (Phase-2 Step-2b-ii). Folding `stream` per-conversation
    /// (it formerly lived as a single `WindowState::stream`) lets several
    /// conversations stream at once: a backgrounded conversation accumulates its
    /// own partial here while the open one renders live, and switching back
    /// re-seeds the buffered prefix. See [`StreamState`].
    stream: Option<StreamState>,
}

/// Shared mutable state for the window.
#[derive(Default)]
pub struct WindowState {
    pub conversations: Vec<ConversationSummary>,
    pub current_conversation_id: Option<String>,
    /// Per-conversation view-models, keyed by conversation id. Retains a model
    /// for every conversation the user has touched (its voice settings + unsent
    /// draft), not just the open one — that's what lets per-conversation state
    /// survive a switch. The *transcript* (`ConversationModel::detail`) is only
    /// kept for the open conversation; switching away evicts the outgoing
    /// transcript (its small state stays) so memory doesn't grow with every
    /// visited conversation. Private — view clients read it through
    /// [`current_conversation`](Self::current_conversation).
    open: HashMap<String, ConversationModel>,
    pub debug_enabled: bool,
    /// A one-shot "try again" offer for the in-view turn that just failed
    /// (#138 item 3): the failed prompt, set when a streaming turn errors/times
    /// out for the open conversation with nothing queued, so a client can put it
    /// back in the composer for a one-click resend. `None` otherwise. Consumed
    /// via [`take_pending_retry_prompt`](Self::take_pending_retry_prompt); not
    /// offered when follow-ups were queued (they flush instead), for a
    /// background failure, or on success. Private — the offer is read once
    /// through the taker so it can't linger and resurface later.
    pending_retry_prompt: Option<String>,
}

impl WindowState {
    /// Whether `You:` (voice input) is Enabled for `conversation` (issue #80).
    /// `false` when it was never set (default Disabled). Part of the shared
    /// public API: clients render per-conversation voice state from it.
    pub fn voice_in_for(&self, conversation: &str) -> bool {
        self.open.get(conversation).is_some_and(|c| c.voice_in)
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
        self.open
            .get(conversation)
            .map(|c| c.adele_output)
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

    /// Whether a *reply* is auto-narrated in full for `conversation` (issue
    /// #80): `Adele == Always` only. `OnDemand` speaks via `say_this` (not
    /// auto-narration) and `Disabled` never speaks. Decoupled from `You`
    /// (voice#126) — the `Adele:` level alone governs her output. The gate the
    /// reply-narration path consults, keyed by the *originating* conversation
    /// (GTK-2). Delegates to the shared gate (desktop-assistant#274). Part of
    /// the shared public API.
    pub fn narrate_for(&self, conversation: &str) -> bool {
        self.adele_output_for(conversation).narrates_reply()
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

    /// Whether a `say_this` aside is spoken aloud for `conversation` (issue
    /// #80): spoken iff `Adele == OnDemand`, where `say_this` is Adele's sole
    /// spoken channel (voice#126) — keyed by the *call's* conversation (GTK-4).
    /// `Always` already narrates every reply and `Disabled` is silent, so both
    /// downgrade the aside to shown text. Delegates to the shared gate
    /// (desktop-assistant#274). Part of the shared public API.
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

    /// The in-flight stream of the *currently open* conversation, if any. The
    /// view always renders the current conversation, so this is what the public
    /// streaming accessors read from. A backgrounded conversation's stream lives
    /// on its own model and is reached via [`route_stream`](Self::route_stream).
    fn current_stream(&self) -> Option<&StreamState> {
        let id = self.current_conversation_id.as_deref()?;
        self.open.get(id).and_then(|m| m.stream.as_ref())
    }

    /// Route a stream event carrying `request_id` to the conversation whose model
    /// owns that stream, returning its id (Phase-2 Step-2b-ii). Now that every
    /// conversation owns its stream, chunk/complete/error events — which carry
    /// only the daemon `request_id` — must find their originating conversation by
    /// it: the claimed-id match is unambiguous; a still-`__pending__` stream
    /// claims the first event it sees, but only when it is the *unique* pending
    /// stream (with several pending at once an unmatched id is ambiguous, so it is
    /// left unrouted — in practice the echoed `UserMessageAdded`, which carries
    /// the conversation id, claims each real id before the first chunk). `None`
    /// when no conversation's stream owns the id.
    fn route_stream(&self, request_id: &str) -> Option<String> {
        // 1. Unambiguous: a stream whose real id is already claimed.
        if let Some((id, _)) = self.open.iter().find(|(_, m)| {
            m.stream.as_ref().and_then(|s| s.request_id.as_deref()) == Some(request_id)
        }) {
            return Some(id.clone());
        }
        // 2. Otherwise the unique `__pending__` stream claims this id. More than
        //    one pending stream makes an unmatched id ambiguous — leave it.
        let mut pending = self
            .open
            .iter()
            .filter(|(_, m)| m.stream.as_ref().is_some_and(|s| s.request_id.is_none()));
        match (pending.next(), pending.next()) {
            (Some((id, _)), None) => Some(id.clone()),
            _ => None,
        }
    }

    /// The accumulated text of the *open* conversation's in-flight streaming
    /// reply, or empty when it has no stream buffering. Read-only accessor for
    /// view clients — the TUI renders the partial reply from it; the field stays
    /// private so only `apply` mutates it. The view always renders the current
    /// conversation, so a backgrounded conversation's partial never leaks here.
    /// Part of the shared public API.
    pub fn streaming_buffer(&self) -> &str {
        self.current_stream().map_or("", |s| s.buffer.as_str())
    }

    /// Whether *any* conversation has a streamed reply in flight (not just the
    /// open one) — a coarse "a turn is streaming somewhere" indicator. Part of
    /// the shared public API; its `-> bool` shape and any-conversation semantics
    /// are unchanged from when a single stream lived at window level, so view
    /// clients need no change. The per-conversation send gate (TUI-7) keys off
    /// the *target* conversation instead (see the `SubmitPrompt` arm), so a send
    /// to a non-streaming conversation is allowed while another streams.
    pub fn is_streaming(&self) -> bool {
        self.open.values().any(|m| m.stream.is_some())
    }

    /// Whether the *open* conversation has an in-flight stream — the render guard
    /// a view consults before painting the streaming buffer, so a backgrounded
    /// turn's chunks never bleed into the conversation the user is looking at
    /// (GTK-2). With per-conversation streams this is simply "does the current
    /// conversation own a stream": the buffer it would paint is that stream's, so
    /// a background stream (on another model) is invisible to the view by
    /// construction. Part of the shared public API.
    pub fn streaming_is_active_for_view(&self) -> bool {
        self.current_stream().is_some()
    }

    /// The background-task id of the turn streaming into the *open* conversation,
    /// or `None` when the open conversation has no in-flight turn, the turn
    /// carries no cancel handle (a legacy daemon's id-less ack), or it is an
    /// adopted [`external`](StreamState::external) turn this client never sent.
    /// This is the handle a view offers **Cancel** for
    /// (`CancelBackgroundTask { id }`, #138), so its `Some`/`None` also answers
    /// "should a Cancel affordance show for the open turn?". It clears with the
    /// stream: both `StreamComplete` and `StreamError` take the stream, so a
    /// finished or abandoned turn is no longer cancelable. Part of the shared
    /// public API.
    pub fn active_task_id_for_view(&self) -> Option<String> {
        self.current_stream()
            .map(|s| s.task_id.clone())
            .filter(|t| !t.is_empty())
    }

    /// Take the one-shot "try again" offer for a just-failed in-view turn
    /// (#138 item 3), if any — the failed prompt, for a client to drop back into
    /// the composer for a one-click resend. Consuming it clears it, so a stale
    /// offer can never resurface on a later, unrelated moment; a client should
    /// only apply it when its composer is empty, so it never overwrites text the
    /// user typed while waiting. Part of the shared public API.
    pub fn take_pending_retry_prompt(&mut self) -> Option<String> {
        self.pending_retry_prompt.take()
    }

    /// The content of the most recent `user` message in the open conversation —
    /// the optimistic bubble of the turn just sent. Used to recover a failed
    /// turn's prompt for the retry offer (#138). `None` when nothing is open or
    /// the transcript holds no user message.
    fn last_user_prompt_in_view(&self) -> Option<String> {
        self.current_conversation()?
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.clone())
    }

    /// Drop *every* conversation's in-flight streaming state *without* finalizing
    /// it — the connection-teardown path (TUI-8). Unlike the
    /// [`UiMessage::Disconnected`] reducer arm (which appends a `[Connection
    /// lost]` stub to each originating conversation before clearing), this simply
    /// discards the partials: the link died, so no buffer must linger as a frozen
    /// partial and no ack sentinel must mis-claim the first post-reconnect stream.
    /// With several conversations possibly streaming at once it walks them all.
    /// Part of the shared public API for view clients that own their connection
    /// lifecycle outside the reducer (the TUI drives reconnect from its run loop,
    /// not from a `Disconnected` message).
    ///
    /// Returns one [`Effect::TurnFinished`] per turn it ended (#51). A turn
    /// dropped here never completes, so a host that opened a per-turn span for
    /// it has nothing else to close the span with. A caller that runs no
    /// telemetry can ignore the result; the state change is the same either way.
    pub fn reset_streaming_state(&mut self) -> Vec<Effect> {
        self.end_every_turn("Streaming state reset")
    }

    /// Drop every conversation's in-flight stream and report each one as a
    /// failed turn, for a teardown that ends turns without a reply (#51).
    ///
    /// Why: `StreamComplete` and `StreamError` are not the only ways a turn
    /// ends. The connection can drop mid-turn, and a turn dropped in silence
    /// leaves a host's per-turn span open for the life of the process. `reason`
    /// becomes the outcome text, so it must name the teardown rather than any
    /// content of the turn.
    ///
    /// Reports are ordered by conversation id so the effect list is
    /// deterministic, rather than following the `open` map's hash order.
    fn end_every_turn(&mut self, reason: &str) -> Vec<Effect> {
        let mut ended: Vec<Effect> = self
            .open
            .iter_mut()
            .filter_map(|(conversation_id, model)| {
                let stream = model.stream.take()?;
                Some(Effect::TurnFinished {
                    conversation_id: conversation_id.clone(),
                    // A turn torn down inside the `__pending__` window never
                    // received a daemon id. Empty says "unknown", the same way
                    // an id-less ack leaves `task_id` empty.
                    request_id: stream.request_id.unwrap_or_default(),
                    idempotency_key: stream.idempotency_key,
                    outcome: TurnOutcome::Failed(reason.to_string()),
                })
            })
            .collect();
        ended.sort_by(|a, b| match (a, b) {
            (
                Effect::TurnFinished {
                    conversation_id: x, ..
                },
                Effect::TurnFinished {
                    conversation_id: y, ..
                },
            ) => x.cmp(y),
            _ => std::cmp::Ordering::Equal,
        });
        ended
    }

    /// The currently-open conversation's loaded detail, or `None` when nothing
    /// is open (or its detail hasn't loaded yet). Read-only accessor for view
    /// clients — replaces the former public `current_conversation` field now
    /// that the detail lives in the keyed [`open`](Self::open) map. Part of the
    /// shared public API.
    pub fn current_conversation(&self) -> Option<&ConversationDetail> {
        let id = self.current_conversation_id.as_deref()?;
        self.open.get(id).and_then(|c| c.detail.as_ref())
    }

    /// Mutable access to the open conversation's detail — e.g. to append an
    /// optimistic user bubble or roll one back. `None` when nothing is open.
    /// Part of the shared public API.
    pub fn current_conversation_mut(&mut self) -> Option<&mut ConversationDetail> {
        let id = self.current_conversation_id.clone()?;
        self.open.get_mut(&id).and_then(|c| c.detail.as_mut())
    }

    /// Mutable access to `conversation`'s in-flight stream, if it has one
    /// (Phase-2 Step-2b-ii). `None` when that conversation isn't modeled or has
    /// no stream. The reducer routes a stream event to its owning conversation
    /// (via [`route_stream`](Self::route_stream)) and reaches its stream through
    /// this.
    fn stream_of_mut(&mut self, conversation: &str) -> Option<&mut StreamState> {
        self.open
            .get_mut(conversation)
            .and_then(|m| m.stream.as_mut())
    }

    /// Switch the open conversation to `detail`'s, caching its transcript and
    /// making it current. Retention (#2): other conversations' models persist in
    /// `open` (their voice settings + draft); only the *outgoing* conversation's
    /// transcript is evicted (its small state stays), so memory doesn't grow with
    /// every visited conversation — it re-fetches on switch-back.
    fn switch_to(&mut self, detail: ConversationDetail) {
        let incoming = detail.id.clone();
        // Evict the outgoing conversation's transcript (its small state stays).
        if let Some(outgoing) = self.current_conversation_id.clone()
            && outgoing != incoming
            && let Some(model) = self.open.get_mut(&outgoing)
        {
            model.detail = None;
        }
        self.current_conversation_id = Some(incoming.clone());
        self.open.entry(incoming).or_default().detail = Some(detail);
    }

    /// Cache/refresh `detail`'s transcript *without* changing which conversation
    /// is open — for a re-fetch of the already-open conversation (reconnect /
    /// refresh). Preserves the model's per-conversation state.
    fn cache_detail(&mut self, detail: ConversationDetail) {
        let id = detail.id.clone();
        self.open.entry(id).or_default().detail = Some(detail);
    }

    /// Seed the open conversation directly — for a client whose connect-time
    /// load path doesn't route through `apply(ConversationLoaded)` (e.g. the
    /// TUI's `load_conversation`). Makes `detail`'s conversation current and
    /// caches its detail. Part of the shared public API.
    pub fn open_conversation(&mut self, detail: ConversationDetail) {
        self.switch_to(detail);
    }

    /// The saved composer draft for `conversation_id` — the unsent text the user
    /// last had in the composer for that conversation, or `""` if none is saved.
    /// View clients read this to restore the draft when switching *to* a
    /// conversation (the native editor is set from it, cursor at end). Part of
    /// the shared public API.
    pub fn composer_draft(&self, conversation_id: &str) -> &str {
        self.open
            .get(conversation_id)
            .map_or("", |c| c.composer.as_str())
    }

    /// Save (or clear) `conversation_id`'s composer draft. View clients call this
    /// to snapshot the *outgoing* conversation's unsent text when switching away
    /// from it (a client may also snapshot live as the user types). An empty
    /// `text` drops the entry, so the map only ever retains non-empty drafts.
    /// Part of the shared public API.
    pub fn set_composer_draft(&mut self, conversation_id: &str, text: String) {
        // Save the draft on the conversation's model. An empty draft on an
        // existing model just clears its composer (the model persists for its
        // other state); an empty draft for an unmodeled conversation is a no-op.
        match self.open.get_mut(conversation_id) {
            Some(model) => model.composer = text,
            None if !text.is_empty() => {
                self.open
                    .entry(conversation_id.to_string())
                    .or_default()
                    .composer = text;
            }
            None => {}
        }
    }

    /// The queued messages awaiting flush for `conversation_id`, in submit
    /// order, or an empty slice when nothing is queued. View clients render the
    /// "N queued" indicator / chips from this. Part of the shared public API.
    pub fn queued_messages(&self, conversation_id: &str) -> Vec<String> {
        self.open.get(conversation_id).map_or_else(Vec::new, |m| {
            m.outbox.iter().map(|q| q.text.clone()).collect()
        })
    }

    /// The queued messages for the *currently open* conversation. Part of the
    /// shared public API — a client that redraws from state each frame reads
    /// this rather than tracking [`Effect::SetQueuedMessages`].
    pub fn queued_messages_for_view(&self) -> Vec<String> {
        self.current_conversation_id
            .as_deref()
            .map_or_else(Vec::new, |id| self.queued_messages(id))
    }

    /// The outbox index of the queued message currently checked out into the
    /// composer for editing in the open conversation, or `None` when composing
    /// a fresh message. Lets a client (the TUI) walk the queue with up/down.
    /// Part of the shared public API.
    pub fn editing_queued_index(&self) -> Option<usize> {
        self.current_conversation_id
            .as_deref()
            .and_then(|id| self.open.get(id))
            .and_then(|m| m.editing.as_ref())
            .map(|e| e.index)
    }

    /// Build the render snapshot of the open conversation's queue for the
    /// client — the queued texts plus the index being edited. Emitted whenever
    /// the queue/edit state changes so the "N queued" indicator stays in sync.
    fn queued_snapshot_effect(&self) -> Effect {
        let (messages, editing) = self
            .current_conversation_id
            .as_deref()
            .and_then(|id| self.open.get(id))
            .map(|m| {
                (
                    m.outbox.iter().map(|q| q.text.clone()).collect(),
                    m.editing.as_ref().map(|e| e.index),
                )
            })
            .unwrap_or_default();
        Effect::SetQueuedMessages { messages, editing }
    }

    /// Commit `prompt` as a real send into the currently-open conversation:
    /// draw the optimistic user bubble, drop the saved draft, and emit the send
    /// RPC. Shared by the direct-send path ([`UiMessage::SubmitPrompt`], idle
    /// with an empty queue) and the queue flush. Caller guarantees a current
    /// conversation exists and it is idle (no in-flight stream). Does NOT touch
    /// the live composer widget — the caller decides whether to clear it (a
    /// direct send clears it; a background flush must not clobber a fresh
    /// draft).
    fn commit_send(&mut self, prompt: String, idempotency_key: Option<String>) -> Vec<Effect> {
        let conversation_id = self
            .current_conversation_id
            .clone()
            .expect("commit_send requires a current conversation (caller contract)");
        // Optimistic local echo of our own send (#1): draw the user bubble now
        // so the turn feels instant. The daemon assigns the real id when it
        // persists the turn; the echoed-back `UserMessageAdded` is de-duped by
        // its `idempotency_key` (exact match, #570) or — keyless — by
        // request_id, so an empty id here is correct. Stamp the bubble with the
        // send's key so the echo can find it regardless of content or ordering.
        if let Some(conv) = self.current_conversation_mut() {
            conv.messages.push(ChatMessage {
                id: String::new(),
                role: "user".to_string(),
                content: prompt.clone(),
                kind: MessageKind::Normal,
                idempotency_key: idempotency_key.clone(),
                // No server id (see above), so there is no UUIDv7 to recover a
                // time from: `None` rather than a fabricated local clock reading.
                created_at_ms: None,
            });
        }
        // NB: does NOT clear the saved composer draft. A *direct* send consumes
        // the composer text (its caller clears the draft), but a *flush* sends
        // the outbox — the composer may hold an unrelated fresh draft that must
        // survive (a switch-back flush would otherwise wipe it; see the flush
        // paths). The one caller that consumes the composer clears it itself.
        let system_refinement = refinement_for_send(self).map(str::to_string);
        vec![Effect::SendPrompt {
            conversation_id,
            prompt,
            system_refinement,
            idempotency_key,
        }]
    }

    /// Flush the open conversation's queued messages as ONE combined send, if it
    /// is idle and has a non-empty queue. Joins the queued texts with
    /// [`QUEUE_JOIN`] and sends them as a single turn, then clears the queue.
    /// The in-progress composer draft (a fresh, not-yet-Entered message) is left
    /// untouched — only the committed queue is flushed. Returns the send effects
    /// plus a cleared-queue snapshot, or an empty vec when there is nothing to
    /// flush (no current conversation, a reply still streaming, an empty queue,
    /// or a queued message currently checked out for editing). Only ever flushes
    /// the *current* conversation, because the combined send draws its optimistic
    /// bubble into the open transcript.
    ///
    /// Deferred while an edit is checked out: the checked-out message lives in
    /// `editing.original` (removed from `outbox`), and flushing now would either
    /// drop it or send the stale pre-edit text and orphan the user's in-progress
    /// edit. So a flush that fires mid-edit (a reply completing while the user
    /// recalls a queued message to fix it) leaves the queue intact; it flushes
    /// when the user finishes the edit (a `SubmitPrompt` reinserts it, then
    /// flushes) or on the next send into the idle conversation.
    fn flush_outbox(&mut self) -> Vec<Effect> {
        let Some(id) = self.current_conversation_id.clone() else {
            return vec![];
        };
        if self.current_stream().is_some() {
            return vec![];
        }
        let Some(model) = self.open.get_mut(&id) else {
            return vec![];
        };
        // A flush is already in flight (sent, awaiting ack): its queue lives in
        // `pending_flush` until acked or restored on failure (#25). Don't start a
        // second overlapping flush.
        if !model.pending_flush.is_empty() {
            return vec![];
        }
        if model.editing.is_some() || model.outbox.is_empty() {
            return vec![];
        }
        let queued = std::mem::take(&mut model.outbox);
        let combined = queued
            .iter()
            .map(|q| q.text.as_str())
            .collect::<Vec<_>>()
            .join(QUEUE_JOIN);
        // The combined turn adopts the FIRST queued message's key (#570), so its
        // echo still dedupes by exact match; `None` when the queue was keyless.
        let combined_key = queued.first().and_then(|q| q.idempotency_key.clone());
        // Hold the queued messages until the send is acked (#25): a failed or
        // abandoned flush restores them to `outbox` for retry instead of dropping
        // them. (Was `mem::take`-and-discard — the source of the loss.)
        model.pending_flush = queued;
        let mut effects = self.commit_send(combined, combined_key);
        effects.push(self.queued_snapshot_effect());
        effects
    }

    /// Flush any pending queued messages for the now-open conversation as one
    /// combined send — the public entry point for a client whose conversation
    /// switch seeds the detail directly via [`open_conversation`](Self::open_conversation)
    /// rather than routing through [`UiMessage::ConversationLoaded`] (which
    /// flushes on switch-back automatically). The TUI's `load_conversation` is
    /// such a path. A no-op (empty vec) when the current conversation is idle
    /// with an empty queue, still streaming, or mid-edit — so calling it after
    /// every switch is safe. Part of the shared public API.
    pub fn flush_pending_queue(&mut self) -> Vec<Effect> {
        self.flush_outbox()
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

/// How a turn's reply stream ended, carried on [`Effect::TurnFinished`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The reply finished normally.
    Completed,
    /// The turn failed, or a teardown ended it before a reply arrived. Carries
    /// the failure text: the daemon's own error, the same text
    /// [`Effect::SetStatusText`] surfaces, or the reason a disconnect gave.
    ///
    /// Treat this string as UNTRUSTED FOR TELEMETRY. It comes from the daemon
    /// or from a provider, and neither promises to keep conversation content
    /// out of it - a content-moderation refusal can quote the text it refused.
    /// So it may go to a user-facing message and to a DEBUG log, and it must
    /// not go on a span field or an INFO line, which carry ids and durations
    /// only. Match on the variant when all you need is "did this turn fail".
    Failed(String),
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
    /// Replace the client's live composer widget text (cursor to end). The
    /// reducer owns composer content for the message-queue flows: this loads a
    /// recalled queued message for editing, and clears the composer (`""`) when
    /// a submitted message is queued or an edit is cancelled. Distinct from the
    /// passive [`composer_draft`](WindowState::composer_draft) store, which the
    /// client snapshots on conversation switch.
    SetComposerText(String),
    /// Render-ready snapshot of the *current* conversation's queued-message
    /// outbox: the queued texts in submit order, plus the outbox index currently
    /// checked out for editing (loaded in the composer), if any. Emitted
    /// whenever the queue or edit state changes, and on conversation load, so
    /// the client repaints its "N queued" indicator / chips.
    SetQueuedMessages {
        messages: Vec<String>,
        editing: Option<usize>,
    },
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
        /// The client-minted idempotency key for this send (#570), or `None` for
        /// a keyless send. The executor forwards it on the `SendMessage` wire
        /// field (via the `*_idempotent` send methods) so a retry re-attaches to
        /// the live turn and the echoed `UserMessageAdded` dedupes by exact key.
        idempotency_key: Option<String>,
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
    /// Append a client-local transcript line the daemon didn't send, tagged
    /// with explicit presentation metadata so the executor renders the marker
    /// (a "Spoken" badge, a "(speech mode disabled)" note) from the `kind`
    /// rather than by parsing `content` (voice#126). Two cases:
    /// - `MessageKind::Spoken`: a `say_this` Adele voiced (on-demand mode) —
    ///   shown in the chat alongside the `Speak` effect so the spoken line is
    ///   also visible.
    /// - `MessageKind::SpeechDisabled`: a `say_this` that was NOT spoken (the
    ///   conversation's `Adele:` is Disabled/Always, or the call is for a
    ///   backgrounded conversation) — shown as a note so the text isn't lost.
    AddLocalMessage { content: String, kind: MessageKind },
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

    // --- Turn-completion correlation (#51) --------------------------------
    /// A turn ended. It completed, it failed, or a teardown ended it before a
    /// reply arrived. Emitted for EVERY turn the reducer was tracking,
    /// including one whose conversation is not the one in view, which is the
    /// whole point: a backgrounded turn used to end with zero effects, so a
    /// host executor that watches effects could not observe it at all.
    ///
    /// Four paths produce it: [`UiMessage::StreamComplete`],
    /// [`UiMessage::StreamError`], [`UiMessage::Disconnected`], and
    /// [`WindowState::reset_streaming_state`]. Together they cover every way a
    /// turn leaves the reducer, so the reducer offers a close for every span it
    /// let a host open. A host still has to run the effects it gets back from
    /// all four, `reset_streaming_state` included, or it keeps the span open
    /// on whichever path it drops.
    ///
    /// Why: a host that opens a per-turn span when the person presses send has
    /// to close it when the reply ends, and the daemon's `request_id` alone does
    /// not say which send that was. This carries the correlation the host
    /// already held at submit time. It saw the same `conversation_id` and
    /// `idempotency_key` on [`Effect::SendPrompt`].
    ///
    /// This reports the REDUCER's view of which stream ended, not the daemon's.
    /// A stray or unrouted `request_id` produces no `TurnFinished`, so a host
    /// never closes a span for a turn the reducer was not tracking.
    ///
    /// Purely informational: a host that runs no telemetry ignores it, and the
    /// reducer's own state is already settled by the time it is emitted.
    TurnFinished {
        /// The conversation whose stream ended, as the reducer routed it. That
        /// is the conversation the turn was sent into, not whichever one is
        /// open now.
        conversation_id: String,
        /// The daemon's turn id for the stream that ended. Empty when a
        /// teardown ended the turn before that id arrived, the same way an
        /// id-less ack leaves a task id empty.
        request_id: String,
        /// The client-minted idempotency key of the send behind this turn, so a
        /// host can name the exact [`UiMessage::SubmitPrompt`] it closes.
        /// `None` for a keyless send and for an adopted external turn (a voice
        /// turn, or another client) that this client never sent. A key-minting
        /// host reads `None` as "I hold no span for this".
        ///
        /// A queue flush sends several submits as ONE turn and adopts the first
        /// queued message's key, so that is the key reported here.
        idempotency_key: Option<String>,
        /// Whether the turn completed or failed.
        outcome: TurnOutcome,
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
            Effect::SetComposerText(t) => f.debug_tuple("SetComposerText").field(t).finish(),
            Effect::SetQueuedMessages { messages, editing } => f
                .debug_struct("SetQueuedMessages")
                .field("messages", messages)
                .field("editing", editing)
                .finish(),
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
                idempotency_key,
            } => f
                .debug_struct("SendPrompt")
                .field("conversation_id", conversation_id)
                .field("prompt", prompt)
                .field("system_refinement", system_refinement)
                .field("idempotency_key", idempotency_key)
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
            Effect::AddLocalMessage { content, kind } => f
                .debug_struct("AddLocalMessage")
                .field("content", content)
                .field("kind", kind)
                .finish(),
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
            Effect::TurnFinished {
                conversation_id,
                request_id,
                idempotency_key,
                outcome,
            } => f
                .debug_struct("TurnFinished")
                .field("conversation_id", conversation_id)
                .field("request_id", request_id)
                .field("idempotency_key", idempotency_key)
                .field("outcome", outcome)
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
                    let detail_cached = self.current_conversation().is_some_and(|c| c.id == id);
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
                self.switch_to(detail);
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
                // A stream may still be in flight for the now-open conversation
                // and/or one we left (GTK-2). Each lives on its own model, so
                // none is cleared here — they keep buffering for their own
                // conversations — but reconcile the view:
                if self.current_stream().is_some() {
                    // Switched (back) to a streaming conversation: the fresh load
                    // wiped its partial reply from the view, so re-seed the
                    // buffered prefix.
                    if !self.streaming_buffer().is_empty() {
                        effects.push(Effect::ReceiveChunk(self.streaming_buffer().to_string()));
                    }
                } else if self.is_streaming() {
                    // Switched away from a streaming conversation to one that
                    // isn't streaming: that turn's status line belongs to the
                    // conversation we left and must not linger over this one.
                    effects.push(Effect::ClearChatStatus);
                }
                // If this conversation had messages queued while a now-finished
                // reply streamed (queued, then switched away before it completed
                // — a backgrounded completion doesn't flush), flush them now that
                // it's back in view and idle. Then resync the "N queued"
                // indicator to this conversation's queue (usually empty), so
                // chips from the previous conversation don't linger.
                effects.extend(self.flush_outbox());
                effects.push(self.queued_snapshot_effect());
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
                    self.cache_detail(detail);
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
                // Prune the deleted conversation's model (GTK-9): its
                // per-conversation state (voice settings, draft, any cached
                // transcript) goes with it, so a later id reuse can't inherit a
                // stale `You:`/`Adele:` setting or composer draft.
                self.open.remove(&id);
                let is_active = self.current_conversation_id.as_deref() == Some(&id);
                if is_active {
                    self.current_conversation_id = None;
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
            UiMessage::SubmitPrompt {
                prompt,
                idempotency_key,
            } => {
                // Single send-decision point (Phase-2). Rather than *refuse* a
                // send while a reply streams (the old TUI-7 gate), we QUEUE it:
                // the user can keep hitting Enter as they think and the whole
                // burst flushes as ONE combined turn when the reply finishes.
                // The connection gate + staged model override stay client-side.
                let Some(conversation_id) = self.current_conversation_id.clone() else {
                    // No open conversation to send into: belt-and-braces no-op.
                    return vec![];
                };
                let has_text = !prompt.trim().is_empty();

                // (1) Finishing an edit of a checked-out queued message: reinsert
                //     the edited text at its original slot (or drop it if
                //     emptied) rather than sending now. The composer clears.
                if let Some(edit) = self
                    .open
                    .get_mut(&conversation_id)
                    .and_then(|m| m.editing.take())
                {
                    if let Some(model) = self.open.get_mut(&conversation_id) {
                        if has_text {
                            let at = edit.index.min(model.outbox.len());
                            // Reinsert keeps the recalled item's ORIGINAL queued
                            // key (#570), not a fresh one — it is the same send.
                            model.outbox.insert(
                                at,
                                QueuedMessage {
                                    text: prompt,
                                    idempotency_key: edit.key,
                                },
                            );
                        }
                        model.composer.clear();
                    }
                    let mut effects = vec![Effect::SetComposerText(String::new())];
                    // Edit-then-Enter while idle means the user is done with the
                    // batch: flush it as one. Still streaming → it stays queued.
                    if self.current_stream().is_none() {
                        effects.extend(self.flush_outbox());
                    } else {
                        effects.push(self.queued_snapshot_effect());
                    }
                    return effects;
                }

                // (2) A reply is streaming into this conversation → QUEUE instead
                //     of refusing. The Enter lands as a chip and the composer
                //     clears; the batch flushes as one when the reply completes.
                if self.current_stream().is_some() {
                    if has_text && let Some(model) = self.open.get_mut(&conversation_id) {
                        model.outbox.push(QueuedMessage {
                            text: prompt,
                            idempotency_key,
                        });
                        model.composer.clear();
                    }
                    return vec![
                        Effect::SetComposerText(String::new()),
                        self.queued_snapshot_effect(),
                    ];
                }

                // (3) Idle but with a pending queue (e.g. the reply finished
                //     between two Enters): append this message and flush the
                //     whole batch as one combined send.
                let has_queue = self
                    .open
                    .get(&conversation_id)
                    .is_some_and(|m| !m.outbox.is_empty());
                if has_queue {
                    // The user hit Enter, consuming the composer, so drop the
                    // saved draft (commit_send no longer does — a background
                    // flush must not, and this is a user-initiated one).
                    if let Some(model) = self.open.get_mut(&conversation_id) {
                        if has_text {
                            model.outbox.push(QueuedMessage {
                                text: prompt,
                                idempotency_key,
                            });
                        }
                        model.composer.clear();
                    }
                    return self.flush_outbox();
                }

                // (4) Idle with an empty queue → the original single-send path.
                //     An empty prompt here is a silent no-op (the composer keeps
                //     its text; the action is gated upstream too).
                if prompt.is_empty() {
                    return vec![];
                }
                // The composer text is the sent text: drop the saved draft so a
                // later switch-away snapshot can't resurrect it (commit_send no
                // longer clears it — the flush path must not).
                if let Some(model) = self.open.get_mut(&conversation_id) {
                    model.composer.clear();
                }
                self.commit_send(prompt, idempotency_key)
            }
            UiMessage::EditQueued { index } => {
                // Check out queued item `index` into the composer to edit it
                // (up-arrow recall / a chip's edit affordance). Any
                // already-checked-out item returns to the queue unchanged first
                // — navigating between queued items discards the in-composer
                // edits of the one you leave, like shell history.
                let Some(conversation_id) = self.current_conversation_id.clone() else {
                    return vec![];
                };
                let Some(model) = self.open.get_mut(&conversation_id) else {
                    return vec![];
                };
                if let Some(prev) = model.editing.take() {
                    let at = prev.index.min(model.outbox.len());
                    model.outbox.insert(
                        at,
                        QueuedMessage {
                            text: prev.original,
                            idempotency_key: prev.key,
                        },
                    );
                }
                if index >= model.outbox.len() {
                    // Stale/out-of-range (the queue changed under a click): after
                    // any reinsert there is nothing to check out — clear the
                    // composer's edit state and resync.
                    return vec![
                        Effect::SetComposerText(String::new()),
                        self.queued_snapshot_effect(),
                    ];
                }
                let item = model.outbox.remove(index);
                model.editing = Some(QueuedEdit {
                    index,
                    original: item.text.clone(),
                    // Preserve the checked-out send's key across the edit (#570).
                    key: item.idempotency_key,
                });
                model.composer = item.text.clone();
                vec![
                    Effect::SetComposerText(item.text),
                    self.queued_snapshot_effect(),
                ]
            }
            UiMessage::RemoveQueued { index } => {
                // Drop queued item `index` without sending it (a chip's x).
                let Some(conversation_id) = self.current_conversation_id.clone() else {
                    return vec![];
                };
                let Some(model) = self.open.get_mut(&conversation_id) else {
                    return vec![];
                };
                if index >= model.outbox.len() {
                    return vec![];
                }
                model.outbox.remove(index);
                // Keep an in-progress edit's reinsert slot consistent when an
                // earlier queued item is removed out from under it.
                if let Some(edit) = model.editing.as_mut()
                    && index < edit.index
                {
                    edit.index -= 1;
                }
                vec![self.queued_snapshot_effect()]
            }
            UiMessage::CancelQueuedEdit => {
                // Abandon an in-progress edit: return the checked-out message to
                // the queue unchanged and clear the composer. No-op otherwise.
                let Some(conversation_id) = self.current_conversation_id.clone() else {
                    return vec![];
                };
                let Some(model) = self.open.get_mut(&conversation_id) else {
                    return vec![];
                };
                let Some(edit) = model.editing.take() else {
                    return vec![];
                };
                let at = edit.index.min(model.outbox.len());
                model.outbox.insert(
                    at,
                    QueuedMessage {
                        text: edit.original,
                        idempotency_key: edit.key,
                    },
                );
                model.composer.clear();
                vec![
                    Effect::SetComposerText(String::new()),
                    self.queued_snapshot_effect(),
                ]
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
                if let Some(conv) = self.current_conversation_mut()
                    && conv.id == conversation_id
                    && conv
                        .messages
                        .last()
                        .is_some_and(|m| m.role == "user" && m.content == prompt)
                {
                    conv.messages.pop();
                }
                // Restore a failed flush's queued messages so the user can retry
                // (#25): a flush parked them in `pending_flush`; put them back at
                // the front of the outbox (they were queued before anything typed
                // since). A direct send leaves `pending_flush` empty, so this is a
                // no-op there — no phantom queue entries.
                let requeued = match self.open.get_mut(&conversation_id) {
                    Some(model) if !model.pending_flush.is_empty() => {
                        let restored = std::mem::take(&mut model.pending_flush);
                        for (i, msg) in restored.into_iter().enumerate() {
                            model.outbox.insert(i, msg);
                        }
                        true
                    }
                    _ => false,
                };
                if requeued && self.is_active_conversation(&conversation_id) {
                    vec![self.queued_snapshot_effect()]
                } else {
                    vec![]
                }
            }
            UiMessage::PromptSent {
                task_id,
                conversation_id,
                idempotency_key,
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
                // The stream lives on its OWN conversation's model now (Phase-2
                // Step-2b-ii), so it keeps streaming independently if the user
                // switches away — and another conversation may stream alongside.
                //
                // Record the `task_id` on the stream (#138): it is the
                // background-task handle Cancel acts on, so it lets a view offer
                // Cancel for this turn until the stream terminates (empty for a
                // legacy id-less ack — no Cancel then).
                let model = self.open.entry(conversation_id.clone()).or_default();
                // The daemon accepted the send (#25): the flush is now its
                // responsibility, so drop the client-side copy held for
                // restore-on-failure. No-op for a direct send (empty).
                model.pending_flush.clear();
                let replaced = model.stream.replace(StreamState {
                    request_id: None,
                    buffer: String::new(),
                    say_this_spoken_this_turn: false,
                    external: false,
                    task_id,
                    // The ack names its own send, so this turn is keyed by the
                    // send that started it and by no other (#51).
                    idempotency_key,
                });
                // A stream was already here, so a second send left before this
                // ack and the conversation cannot model both. The turn just
                // replaced is lost - that is #53, and it predates the report -
                // but its END is not: a host holds a span for it, opened at its
                // own submit. Report it under ITS key, so the host closes that
                // span rather than leaving it open or, worse, closing it later
                // under the surviving turn's key and recording a duration that
                // reads plausible and belongs to a different turn.
                match replaced {
                    Some(stream) => vec![Effect::TurnFinished {
                        conversation_id,
                        request_id: stream.request_id.unwrap_or_default(),
                        idempotency_key: stream.idempotency_key,
                        outcome: TurnOutcome::Failed("Replaced by a later send".to_string()),
                    }],
                    None => vec![],
                }
            }
            UiMessage::UserMessageAdded {
                conversation_id,
                request_id,
                content,
                idempotency_key,
            } => {
                // Case 0 — our own send recognized by EXACT idempotency-key match
                // (#570). When the echo carries a key we stamped on an optimistic
                // bubble still held for this conversation, this is unambiguously
                // our send coming back — regardless of send/echo ordering and
                // regardless of whether the daemon normalized the content. Claim
                // the real `request_id` onto a still-pending stream (so chunks
                // correlate) if one exists yet, and render nothing. This subsumes
                // Case 1 / Case 1b for keyed sends; the keyless fallbacks below
                // stay for voice turns, other clients, and pre-key clients.
                //
                // Reconnect-interleave: this scan covers ALL loaded conversation
                // messages, not just an in-memory optimistic bubble. So once the
                // daemon persists the key on the message row and surfaces it on
                // reload, a row rebuilt by a transcript RELOAD or a
                // switch-away-and-back (real id, no optimistic bubble, no stream)
                // still exact-key-matches its echo here — closing the reload hole
                // that the content compare (Case 1b) could not, since Case 1b only
                // rescues the empty-id optimistic bubble. Keyless turns (voice,
                // other clients, pre-key clients) still fall to the content
                // compare below. (Refs #570)
                if let Some(key) = idempotency_key.as_deref()
                    && self
                        .open
                        .get(&conversation_id)
                        .and_then(|m| m.detail.as_ref())
                        .is_some_and(|d| {
                            d.messages
                                .iter()
                                .any(|msg| msg.idempotency_key.as_deref() == Some(key))
                        })
                {
                    if let Some(stream) = self.stream_of_mut(&conversation_id)
                        && stream.request_id.is_none()
                    {
                        stream.request_id = Some(request_id);
                    }
                    return vec![];
                }
                // Case 1 — this client's own send, echoed back (#1). We drew the
                // user bubble optimistically at send time and set "__pending__"
                // on this conversation's own stream (Phase-2 Step-2b-ii); claim
                // the real request_id now (it precedes the first chunk) and render
                // nothing more. This also resolves the stream's request_id earlier
                // and more reliably than the claim-on-first-chunk fallback.
                if let Some(stream) = self.stream_of_mut(&conversation_id)
                    && stream.request_id.is_none()
                {
                    stream.request_id = Some(request_id);
                    return vec![];
                }
                // Case 1b — this client's own send echoed back BEFORE its
                // `PromptSent` ack opened the `__pending__` stream. A queue FLUSH
                // reliably hits this: the daemon, primed by the just-finished
                // turn, emits `UserMessageAdded` for the combined follow-up
                // before it acks the send, so Case 1 finds no pending stream yet.
                // The optimistic bubble `commit_send` drew is still the last
                // message; recognize this echo as its duplicate (same content,
                // still unkeyed) and render nothing — otherwise Case 2 would draw
                // the user bubble a second time and mark the reply external (so
                // our own turn wouldn't narrate). The pending stream is opened by
                // the ack (`PromptSent`) as usual, exactly as for a direct send.
                // (An idempotency id on the send/echo would make this an exact
                // match instead of a content compare; tracked as a follow-up.)
                if self.current_stream().is_none()
                    && self.is_active_conversation(&conversation_id)
                    && self
                        .current_conversation()
                        .and_then(|c| c.messages.last())
                        .is_some_and(|m| {
                            m.id.is_empty() && m.role == "user" && m.content == content
                        })
                {
                    return vec![];
                }
                // Case 2 — a turn this client did NOT initiate (a voice turn, or
                // another client on the same account) for the conversation in
                // view, with no turn already occupying *that conversation's*
                // in-flight slot. Adopt it onto the conversation's stream so the
                // existing chunk/completion path streams the reply live, and draw
                // the user's bubble now. Marked external so its reply is NOT
                // narrated here — the originator (e.g. the voice daemon) already
                // speaks it. A turn for a background conversation, or one arriving
                // while that conversation's own turn is in flight, is left to the
                // reload-on-switch path (the daemon persists it).
                if self.current_stream().is_none() && self.is_active_conversation(&conversation_id)
                {
                    let stream = StreamState {
                        request_id: Some(request_id),
                        buffer: String::new(),
                        say_this_spoken_this_turn: false,
                        external: true,
                        // Adopted from elsewhere: this client never received the
                        // turn's ack, so it holds no task id and cannot cancel it
                        // (#138).
                        task_id: String::new(),
                        // Nor did it send the turn, so there is no key of ours to
                        // report when it finishes (#51).
                        idempotency_key: None,
                    };
                    self.open.entry(conversation_id).or_default().stream = Some(stream);
                    if let Some(conv) = self.current_conversation_mut() {
                        conv.messages.push(ChatMessage {
                            // Locally-adopted external turn: no server id yet
                            // (the event carries none). Empty is the sanctioned
                            // placeholder for a message the daemon hasn't keyed;
                            // the next reload swaps in the authoritative copy.
                            id: String::new(),
                            role: "user".to_string(),
                            content: content.clone(),
                            kind: MessageKind::Normal,
                            // An externally-initiated turn (voice / another
                            // client): not our optimistic send, so no key (#570).
                            idempotency_key: None,
                            // No server id (see above), so there is no UUIDv7 to recover a
                            // time from: `None` rather than a fabricated local clock reading.
                            created_at_ms: None,
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
                // Route the status to its owning conversation and show it only
                // when that conversation is the one in view (GTK-2). With several
                // streams in flight (Phase-2 Step-2b-ii) routing keeps a
                // backgrounded turn's status off the open chat even when the open
                // conversation has its OWN `__pending__` stream: a claimed-id
                // match wins over the pending fallback, so the background status
                // routes to its own (background) conversation, not the viewed one.
                if self.route_stream(&request_id).as_deref()
                    == self.current_conversation_id.as_deref()
                    && self.current_stream().is_some()
                {
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
                // Route the chunk to the conversation whose stream owns this id
                // (Phase-2 Step-2b-ii): the claimed-id match, or the unique
                // `__pending__` stream that claims it now. No owner → stray chunk,
                // ignored.
                let Some(origin) = self.route_stream(&request_id) else {
                    return vec![];
                };
                let Some(stream) = self.stream_of_mut(&origin) else {
                    return vec![];
                };
                // Claim the real id on the first frame of a `__pending__` stream.
                if stream.request_id.is_none() {
                    stream.request_id = Some(request_id.clone());
                }
                let first_chunk = stream.buffer.is_empty();
                // Always accumulate — the buffer belongs to its own conversation
                // (GTK-2) and is what re-seeds the view if the user switches back
                // mid-stream...
                stream.buffer.push_str(&chunk);
                // ...but only render into the chat when that conversation is the
                // one in view. A backgrounded conversation's chunk accumulates on
                // its model and emits no render Effect (so it can stream
                // concurrently without disturbing the open conversation).
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
                // Route the completion to the conversation whose stream owns this
                // id (Phase-2 Step-2b-ii). No owner → unrelated completion,
                // ignored. Take ONLY this conversation's stream — any other
                // conversation still streaming is left untouched.
                let Some(origin) = self.route_stream(&request_id) else {
                    return vec![];
                };
                let Some(stream) = self.open.get_mut(&origin).and_then(|m| m.stream.take()) else {
                    return vec![];
                };
                let said_via_tool = stream.say_this_spoken_this_turn;
                // An adopted external turn (a voice turn, or another client) is
                // narrated by its originator — gtk must not also speak it.
                let was_external = stream.external;
                let is_active = self.is_active_conversation(&origin);
                // The turn is over: report it so a host can close a per-turn
                // span (#51). Built here, while the stream is still in hand, and
                // emitted on BOTH paths below, the backgrounded one included,
                // which is the path a host could not observe at all.
                let finished = Effect::TurnFinished {
                    conversation_id: origin.clone(),
                    request_id,
                    idempotency_key: stream.idempotency_key,
                    outcome: TurnOutcome::Completed,
                };

                if !is_active {
                    // The originating conversation isn't the one in view, so we
                    // don't hold its detail (`current_conversation` caches only
                    // the open conversation). Touch NOTHING in the open chat: no
                    // CompleteStreaming, no chat status, no audio. The reply is
                    // persisted daemon-side and appears when the user switches
                    // back and the conversation reloads. The turn report is the
                    // one exception: it renders nothing, and without it a
                    // backgrounded turn would never reach the host (#51).
                    return vec![finished];
                }

                // Reply narration (issue #80): narrate the finalized reply via
                // the embedded `Speaker` when the gate holds — `Adele == Always`
                // only now (voice#126: on-demand speaks via say_this, not
                // auto-narration; decoupled from `You`). Gated entirely here so
                // the cut-off holds: when the gate is false no `Speak` effect
                // exists, so no path plays audio. (The executor additionally
                // no-ops when there is no embedded engine, e.g. the daemon path,
                // which narrates its own replies.) Keyed by the *originating*
                // conversation (GTK-2): a backgrounded turn never narrates
                // (handled by the early return above) — only an in-view streaming
                // conversation can. `!said_via_tool` is a defensive backstop (see
                // `say_this_spoken_this_turn`); Always never fires say_this.
                let narrate = !said_via_tool && !was_external && self.narrate_for(&origin);

                // The streaming conversation is the one in view: finalize it.
                if let Some(conv) = self.current_conversation_mut() {
                    conv.messages.push(ChatMessage {
                        // Locally-finalized reply: no server id in hand (empty
                        // placeholder); the next reload reconciles.
                        id: String::new(),
                        role: "assistant".to_string(),
                        content: full_response.clone(),
                        kind: MessageKind::Normal,
                        // Assistant replies never carry a send idempotency key.
                        idempotency_key: None,
                        // No server id (see above), so there is no UUIDv7 to recover a
                        // time from: `None` rather than a fabricated local clock reading.
                        created_at_ms: None,
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
                // Report the finished turn BEFORE the flush below, which starts
                // the NEXT turn: a host must see this turn close before the next
                // one opens, or it nests them (#51).
                effects.push(finished);
                // The reply finished: flush any messages the user queued while it
                // streamed as ONE combined follow-up turn. (Only reached for the
                // in-view conversation — a backgrounded completion returned
                // early above; its queue flushes when the user switches back.)
                effects.extend(self.flush_outbox());
                effects
            }
            UiMessage::StreamError { request_id, error } => {
                // Route the error to the conversation whose stream owns this id
                // (Phase-2 Step-2b-ii). No owner → unrelated error, ignored. Clear
                // ONLY this conversation's stream — any other still streaming is
                // left untouched.
                let Some(origin) = self.route_stream(&request_id) else {
                    return vec![];
                };
                let Some(stream) = self.open.get_mut(&origin).and_then(|m| m.stream.take()) else {
                    return vec![];
                };
                let is_active = self.is_active_conversation(&origin);
                // Only clear the chat status line if the failed stream's
                // conversation is the one in view (GTK-2); a background turn's
                // failure must not blank another conversation's chat. The
                // status-text line is the global one, so always surface the error.
                let mut effects = vec![Effect::SetStatusText(format!("Error: {error}"))];
                // The turn is over: report it on BOTH paths so a host can close
                // its per-turn span even for a backgrounded failure (#51).
                let finished = Effect::TurnFinished {
                    conversation_id: origin.clone(),
                    request_id,
                    idempotency_key: stream.idempotency_key,
                    outcome: TurnOutcome::Failed(error),
                };
                effects.push(finished);
                if is_active {
                    effects.insert(0, Effect::ClearChatStatus);
                    // The turn failed, but the user's queued follow-ups are still
                    // theirs to send: flush them as one combined turn now that
                    // the conversation is idle again.
                    let flush = self.flush_outbox();
                    let had_queued_follow_ups = !flush.is_empty();
                    effects.extend(flush);
                    // Retry offer (#138 item 3): with no queued follow-up to send,
                    // the failed prompt would otherwise be lost from the composer.
                    // Offer it back for a one-click "try again" — recovered from
                    // the optimistic user bubble still in the transcript. If
                    // follow-ups were queued, the user has moved on; we flush
                    // those instead of shoving the failed prompt over the top.
                    if !had_queued_follow_ups {
                        self.pending_retry_prompt = self.last_user_prompt_in_view();
                    }
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
                            if let Some(conv) = self.current_conversation_mut() {
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
            // The knowledge browser is a self-contained widget with its own
            // fetch pump (not part of this conversation reducer), so the live
            // refresh is wired at the window layer. Nothing to do in the reducer.
            UiMessage::KnowledgeChanged => vec![],
            UiMessage::SetVoiceIn {
                conversation_id,
                enabled,
            } => {
                // Record the per-conversation `You:` (voice input) setting (issue
                // #80). Pure state change; the dropdown is the write source here
                // (the user changed it), so no UI reflection is needed. Keyed by
                // conversation so it never bleeds across them.
                self.open.entry(conversation_id).or_default().voice_in = enabled;
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
                self.open.entry(conversation_id).or_default().adele_output = level;
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
                        // iff `Adele == OnDemand` for the *call's* conversation
                        // (its sole spoken channel, voice#126) AND that
                        // conversation is the one in view. A backgrounded call's
                        // aside is never voiced — it downgrades to a shown note so
                        // it isn't lost.
                        Some(text) if is_active && self.say_this_spoken_for(&conversation_id) => {
                            // Belt-and-suspenders against double-speak: mark *that
                            // conversation's* stream so its StreamComplete won't
                            // also narrate the full reply. (On-demand never
                            // auto-narrates, so the gate below is already false —
                            // this stays correct if the modes ever change.)
                            if let Some(stream) = self.stream_of_mut(&conversation_id) {
                                stream.say_this_spoken_this_turn = true;
                            }
                            // Show the spoken line in the transcript too, tagged
                            // Spoken so the executor can badge it — the user sees
                            // what Adele said aloud (voice#126).
                            vec![
                                Effect::Speak(text.clone()),
                                Effect::AddLocalMessage {
                                    content: text,
                                    kind: MessageKind::Spoken,
                                },
                                Effect::SubmitClientToolResult {
                                    task_id,
                                    tool_call_id,
                                    result: Ok("spoken".to_string()),
                                },
                            ]
                        }
                        Some(text) => {
                            // Not spoken: the call's conversation is Disabled or
                            // Always (say_this isn't its spoken channel), or it
                            // isn't the one in view. Show the text tagged
                            // SpeechDisabled — the executor adds the "(speech mode
                            // disabled)" marker; we keep `content` clean so the
                            // metadata, not a baked-in string, drives rendering.
                            // The turn still completes; no audio on any path.
                            vec![
                                Effect::AddLocalMessage {
                                    content: text,
                                    kind: MessageKind::SpeechDisabled,
                                },
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
                        self.open
                            .entry(conversation_id.clone())
                            .or_default()
                            .adele_output = AdeleOutput::OnDemand;
                        let mut effects = Vec::new();
                        if is_active {
                            effects.push(Effect::SetAdeleOutputDropdown(AdeleOutput::OnDemand));
                        }
                        effects.push(Effect::SubmitClientToolResult {
                            task_id,
                            tool_call_id,
                            result: Ok(
                                "voice mode on (on-demand): your written reply is shown as \
                                 text and not read aloud; speak by calling say_this, kept brief \
                                 and conversational"
                                    .to_string(),
                            ),
                        });
                        effects
                    }
                    "stop_voice" => {
                        self.open
                            .entry(conversation_id.clone())
                            .or_default()
                            .adele_output = AdeleOutput::Disabled;
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
                // conversation it actually belongs to (GTK-2). Every conversation
                // owns its own stream now (Phase-2 Step-2b-ii), so several may be
                // in flight: drop them ALL (none may linger as a frozen partial or
                // later mis-claim a post-reconnect id), and append the truncated
                // "[Connection lost]" stub only to the *open* conversation's
                // stream — a backgrounded stream's partial was never persisted
                // daemon-side, so it is simply discarded.
                let active_partial = self
                    .current_conversation_id
                    .clone()
                    .and_then(|id| self.open.get(&id))
                    .and_then(|m| m.stream.as_ref())
                    .map(|s| s.buffer.clone())
                    .filter(|buffer| !buffer.is_empty());
                // Drop every stream, and report each turn it ended so a host can
                // close its per-turn span (#51). Every turn in flight is over:
                // the link that would have finished them is gone.
                let ended = self.end_every_turn(&format!("Disconnected: {reason}"));
                if let Some(buffer) = active_partial {
                    let full = format!("{buffer}\n\n[Connection lost]");
                    if let Some(conv) = self.current_conversation_mut() {
                        conv.messages.push(ChatMessage {
                            // Local connection-lost stub: no server id
                            // (empty placeholder).
                            id: String::new(),
                            role: "assistant".to_string(),
                            content: full.clone(),
                            kind: MessageKind::Normal,
                            idempotency_key: None,
                            // No server id (see above), so there is no UUIDv7 to recover a
                            // time from: `None` rather than a fabricated local clock reading.
                            created_at_ms: None,
                        });
                    }
                    effects.push(Effect::CompleteStreaming(full));
                }
                // After the render, so the partial is on screen before the turn
                // is called over.
                effects.extend(ended);
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
        tool_gate_disabled: detail.tool_gate_disabled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only accessors mirroring the former free-standing `pending_*` /
    /// `streaming_buffer` fields, now that the stream lives per-conversation on
    /// each [`ConversationModel`] (Phase-2 Step-2b-ii). The single-stream tests
    /// assert against *the* in-flight stream regardless of which conversation
    /// holds it, so these find the unique one (and panic if a test accidentally
    /// leaves two in flight — the concurrent-stream tests use the
    /// conversation-scoped [`stream_of`](Self::stream_of) accessor instead). A
    /// `__pending__` stream — request reserved but the daemon id not yet claimed
    /// — reads as `stream_request_id() == None` + `stream_unclaimed() == true`.
    impl WindowState {
        /// The unique in-flight stream across all open conversations, or `None`.
        /// Panics if more than one conversation is streaming (a single-stream
        /// test invariant; concurrent tests use [`stream_of`](Self::stream_of)).
        fn any_stream(&self) -> Option<&StreamState> {
            let mut it = self.open.values().filter_map(|m| m.stream.as_ref());
            let first = it.next();
            assert!(
                it.next().is_none(),
                "any_stream() expects at most one in-flight stream; use stream_of() for concurrent"
            );
            first
        }
        /// The stream on `conversation`'s model, if any — the conversation-scoped
        /// accessor the concurrent-stream tests use.
        fn stream_of(&self, conversation: &str) -> Option<&StreamState> {
            self.open.get(conversation).and_then(|m| m.stream.as_ref())
        }
        /// The claimed daemon request id of the unique in-flight stream, or `None`
        /// (no stream, or still in the `__pending__` window).
        fn stream_request_id(&self) -> Option<&str> {
            self.any_stream().and_then(|s| s.request_id.as_deref())
        }
        /// The originating conversation of the unique in-flight stream, if any —
        /// the `open` map key of the model that holds it (the owning conversation
        /// is now identified by where the stream lives, not a field on it).
        /// Panics if more than one conversation is streaming (single-stream test
        /// invariant), mirroring [`any_stream`](Self::any_stream).
        fn stream_conversation_id(&self) -> Option<&str> {
            let mut it = self
                .open
                .iter()
                .filter(|(_, m)| m.stream.is_some())
                .map(|(id, _)| id.as_str());
            let first = it.next();
            assert!(
                it.next().is_none(),
                "stream_conversation_id() expects one stream"
            );
            first
        }
        /// A (unique) stream is in flight but its real id is not yet claimed (the
        /// old `pending_request_id == Some("__pending__")`).
        fn stream_unclaimed(&self) -> bool {
            self.any_stream().is_some_and(|s| s.request_id.is_none())
        }
        /// The unique in-flight stream is an adopted external turn.
        fn stream_external(&self) -> bool {
            self.any_stream().is_some_and(|s| s.external)
        }

        /// Test builder: open `detail`'s conversation — make it current and
        /// insert it into the open map. Mirrors the old
        /// `current_conversation: Some(detail)` + `current_conversation_id:
        /// Some(id)` literal pair now that detail lives behind the keyed map.
        fn with_open(mut self, detail: ConversationDetail) -> Self {
            let id = detail.id.clone();
            self.current_conversation_id = Some(id.clone());
            self.open.entry(id).or_default().detail = Some(detail);
            self
        }

        /// Test builder: pin `stream` onto `conversation`'s model (creating the
        /// model if absent). Replaces the old `WindowState { stream: Some(..),
        /// .. }` struct literal now that the stream lives per-conversation.
        fn with_stream(mut self, conversation: &str, stream: StreamState) -> Self {
            self.open
                .entry(conversation.to_string())
                .or_default()
                .stream = Some(stream);
            self
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
            kind: MessageKind::Normal,
            idempotency_key: None,
            // No server id (see above), so there is no UUIDv7 to recover a
            // time from: `None` rather than a fabricated local clock reading.
            created_at_ms: None,
        }
    }

    fn detail(id: &str, messages: Vec<ChatMessage>) -> ConversationDetail {
        ConversationDetail {
            id: id.to_string(),
            title: format!("conv {id}"),
            messages,
            model_selection: None,
            conversation_personality: None,
            tool_gate_disabled: false,
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
            notices: Vec::new(),
        }
    }

    // --- Idempotency key threading + exact-match dedup (#570) ------------

    #[test]
    fn submit_prompt_threads_idempotency_key_into_send_effect() {
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: "hello".to_string(),
            idempotency_key: Some("turn-k".to_string()),
        });
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::SendPrompt { idempotency_key: Some(k), .. }] if k == "turn-k"
            ),
            "the send effect must carry the client-minted idempotency key: {effects:?}"
        );
    }

    #[test]
    fn optimistic_bubble_is_stamped_with_the_idempotency_key() {
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: "hello".to_string(),
            idempotency_key: Some("turn-k".to_string()),
        });
        let bubble = state
            .current_conversation()
            .expect("c1 is open")
            .messages
            .last()
            .expect("the optimistic user bubble");
        assert_eq!(bubble.role, "user");
        assert_eq!(
            bubble.idempotency_key.as_deref(),
            Some("turn-k"),
            "the optimistic user bubble must be stamped with the send's key"
        );
    }

    #[test]
    fn an_echo_matching_the_optimistic_key_dedups_even_with_different_content() {
        // Our own send: key `k`, content "a" — drawn optimistically.
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: "a".to_string(),
            idempotency_key: Some("k".to_string()),
        });
        // The daemon echoes `UserMessageAdded` with the SAME key but DIFFERENT
        // content. Exact-key match (#570) must dedupe regardless of content —
        // proving it is a key compare, not the content fallback.
        let echo = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "r".to_string(),
            content: "DIFFERENT".to_string(),
            idempotency_key: Some("k".to_string()),
        });
        assert!(
            !echo.iter().any(|e| matches!(e, Effect::AddUserMessage(_))),
            "an exact-key echo must not draw a second bubble even when content differs: {echo:?}"
        );
        let user_bubbles = state
            .current_conversation()
            .expect("c1 is open")
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .count();
        assert_eq!(
            user_bubbles, 1,
            "exactly one user bubble (the optimistic one), not a duplicate"
        );
    }

    #[test]
    fn a_keyless_echo_still_uses_the_content_fallback() {
        // With no idempotency key the pre-#570 content compare (Case 1b) still
        // dedupes our own echoed send: a direct send draws the optimistic
        // bubble, and the keyless echo of the same content — arriving before the
        // ack opens the pending stream — renders nothing.
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: "hi".to_string(),
            idempotency_key: None,
        });
        let echo = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "r".to_string(),
            content: "hi".to_string(),
            idempotency_key: None,
        });
        assert!(
            !echo.iter().any(|e| matches!(e, Effect::AddUserMessage(_))),
            "a keyless echo of our own send must fall back to the content compare: {echo:?}"
        );
        let user_bubbles = state
            .current_conversation()
            .expect("c1 is open")
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .count();
        assert_eq!(
            user_bubbles, 1,
            "exactly one user bubble via the keyless content fallback"
        );
    }

    #[test]
    fn reloaded_keyed_user_row_dedupes_matching_echo() {
        // Reconnect-interleave hole (#570): after a transcript reload the detail
        // is rebuilt from persisted rows — each with a REAL server id (not the
        // empty placeholder of an in-memory optimistic bubble) and, once the
        // daemon persists+surfaces the key, its idempotency key. No optimistic
        // bubble and no stream survive the reload. When our own send's echo then
        // arrives, Case 0 scans ALL loaded messages for an exact key match and
        // dedupes it — closing the hole. Without the persisted key this reloaded
        // real-id row would fail Case 1b's `id.is_empty()` guard and fall through
        // to Case 2, drawing a duplicate user bubble.
        let mut state = WindowState::default().with_open(detail(
            "c1",
            vec![ChatMessage {
                id: "m1".to_string(),
                role: "user".to_string(),
                content: "hello".to_string(),
                kind: MessageKind::Normal,
                idempotency_key: Some("K".to_string()),
                // No server id (see above), so there is no UUIDv7 to recover a
                // time from: `None` rather than a fabricated local clock reading.
                created_at_ms: None,
            }],
        ));
        let echo = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "R".to_string(),
            // Daemon-normalized content differs from the stored row — proving the
            // dedup is an exact-key match on the reloaded row, not a content
            // compare.
            content: "normalized-differently".to_string(),
            idempotency_key: Some("K".to_string()),
        });
        assert!(
            !echo.iter().any(|e| matches!(e, Effect::AddUserMessage(_))),
            "a keyed echo matching a reloaded row's key must not draw a second bubble: {echo:?}"
        );
        let user_bubbles = state
            .current_conversation()
            .expect("c1 is open")
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .count();
        assert_eq!(
            user_bubbles, 1,
            "exactly one user bubble (the reloaded row) — the reconnect-interleave hole is closed"
        );
    }

    #[test]
    fn reloaded_keyless_user_row_still_falls_back() {
        // Counterpart to the keyed reload test: with NO persisted key on the
        // reloaded row (and none on the echo), Case 0 cannot fire. The content
        // fallback (Case 1b) only rescues an optimistic bubble still carrying the
        // empty-id placeholder, so a reloaded row with a REAL id falls through to
        // Case 2 even when the content matches exactly — the daemon-persisted copy
        // is redrawn. This pins the still-open keyless reload behavior that
        // persisting the key (the keyed test above) closes.
        let mut state = WindowState::default().with_open(detail(
            "c1",
            vec![ChatMessage {
                id: "m1".to_string(),
                role: "user".to_string(),
                content: "hello".to_string(),
                kind: MessageKind::Normal,
                idempotency_key: None,
                // No server id (see above), so there is no UUIDv7 to recover a
                // time from: `None` rather than a fabricated local clock reading.
                created_at_ms: None,
            }],
        ));
        let echo = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "R".to_string(),
            content: "hello".to_string(),
            idempotency_key: None,
        });
        assert!(
            echo.iter().any(|e| matches!(e, Effect::AddUserMessage(_))),
            "without a persisted key the reloaded real-id row is not deduped — Case 2 redraws it: {echo:?}"
        );
        let user_bubbles = state
            .current_conversation()
            .expect("c1 is open")
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .count();
        assert_eq!(
            user_bubbles, 2,
            "the keyless reload hole remains: the persisted row plus a redrawn duplicate"
        );
    }

    #[test]
    fn queue_flush_uses_the_first_queued_messages_key() {
        // Two sends queued mid-stream each carry their own key; the combined
        // flush turn adopts the FIRST queued message's key (#570), so its echo
        // still dedupes by exact match.
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "a".to_string(),
            idempotency_key: Some("ka".to_string()),
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "b".to_string(),
            idempotency_key: Some("kb".to_string()),
        });
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "done".to_string(),
        });
        let sent = effects.iter().find_map(|e| match e {
            Effect::SendPrompt {
                idempotency_key,
                prompt,
                ..
            } => Some((idempotency_key.clone(), prompt.clone())),
            _ => None,
        });
        assert_eq!(
            sent,
            Some((Some("ka".to_string()), "a\n\nb".to_string())),
            "the combined flush turn adopts the FIRST queued message's key: {effects:?}"
        );
    }

    #[test]
    fn case0_claims_request_id_onto_pending_stream() {
        // Our own send drew an optimistic bubble keyed `K`, then the ack opened
        // a `__pending__` stream (real id not yet claimed). The keyed echo
        // arrives: Case 0 recognizes it by exact key, claims the daemon
        // `request_id` onto the pending stream (so later chunks correlate), and
        // renders nothing — no duplicate bubble.
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: "hello".to_string(),
            idempotency_key: Some("K".to_string()),
        });
        state.apply(UiMessage::PromptSent {
            task_id: "ack-1".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: None,
        });
        assert!(
            state.stream_unclaimed(),
            "precondition: the ack left a __pending__ stream with no claimed id"
        );
        let echo = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "R".to_string(),
            content: "normalized-differently".to_string(),
            idempotency_key: Some("K".to_string()),
        });
        assert_eq!(
            state.stream_request_id(),
            Some("R"),
            "Case 0 must claim the real request_id onto the pending stream"
        );
        assert!(
            !echo.iter().any(|e| matches!(e, Effect::AddUserMessage(_))),
            "the keyed echo must not draw a second bubble: {echo:?}"
        );
        let user_bubbles = state
            .current_conversation()
            .expect("c1 is open")
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .count();
        assert_eq!(
            user_bubbles, 1,
            "exactly one user bubble (the optimistic one)"
        );
    }

    #[test]
    fn case0_echo_before_ack_no_stream_renders_nothing() {
        // The keyed echo can beat the `PromptSent` ack that opens the pending
        // stream (a queue flush primes the daemon to echo first). With no stream
        // yet, Case 0's exact-key match still dedupes the echo: nothing to claim,
        // and no second bubble drawn.
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: "hi".to_string(),
            idempotency_key: Some("K".to_string()),
        });
        assert!(
            state.any_stream().is_none(),
            "precondition: no stream is open before the ack"
        );
        let echo = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "R".to_string(),
            content: "hi".to_string(),
            idempotency_key: Some("K".to_string()),
        });
        assert!(
            echo.is_empty(),
            "a keyed echo with no stream yet must render nothing: {echo:?}"
        );
        let user_bubbles = state
            .current_conversation()
            .expect("c1 is open")
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .count();
        assert_eq!(
            user_bubbles, 1,
            "dedup holds before the ack opens the stream: one bubble only"
        );
    }

    // --- __pending__ sentinel handoff (#31) ------------------------------

    #[test]
    fn prompt_sent_sets_pending_sentinel_and_clears_buffer() {
        // A prior stream left a partial buffer on c1; PromptSent for c1 must
        // start the new turn from a clean slate (a fresh __pending__ stream).
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        }
        .with_stream(
            "c1",
            StreamState {
                buffer: "leftover".to_string(),
                ..Default::default()
            },
        );
        let effects = state.apply(UiMessage::PromptSent {
            task_id: "ack-1".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: None,
        });
        assert!(
            !effects
                .iter()
                .any(|e| !matches!(e, Effect::TurnFinished { .. })),
            "PromptSent performs no widget effects: {effects:?}"
        );
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
            idempotency_key: None,
        });
        assert_eq!(state.stream_conversation_id(), Some("c1"));
    }

    // --- active_task_id_for_view: the Cancel handle for the open turn (#138) --

    #[test]
    fn active_task_id_for_view_returns_the_open_turns_task_id() {
        // PromptSent carries the background-task id (the handle Cancel acts on);
        // the reducer records it on the open conversation's stream so a view can
        // offer Cancel for the in-flight turn.
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        state.apply(UiMessage::PromptSent {
            task_id: "task-42".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: None,
        });
        assert_eq!(state.active_task_id_for_view().as_deref(), Some("task-42"));
    }

    #[test]
    fn active_task_id_for_view_is_none_without_a_stream() {
        let state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        assert_eq!(state.active_task_id_for_view(), None);
    }

    #[test]
    fn active_task_id_for_view_clears_when_the_stream_completes() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        state.apply(UiMessage::PromptSent {
            task_id: "task-42".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: None,
        });
        // The unique pending stream claims this completion.
        state.apply(UiMessage::StreamComplete {
            request_id: "r1".to_string(),
            full_response: "done".to_string(),
        });
        assert_eq!(
            state.active_task_id_for_view(),
            None,
            "a finished turn is no longer cancelable"
        );
    }

    #[test]
    fn active_task_id_for_view_clears_when_the_stream_errors() {
        // The abandonment/watchdog path (StreamError) tears the stream down, so
        // the Cancel affordance for that turn disappears with it.
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        state.apply(UiMessage::PromptSent {
            task_id: "task-42".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::StreamError {
            request_id: "r1".to_string(),
            error: "boom".to_string(),
        });
        assert_eq!(
            state.active_task_id_for_view(),
            None,
            "an errored/abandoned turn is no longer cancelable"
        );
    }

    #[test]
    fn active_task_id_for_view_is_none_for_a_legacy_empty_task_id() {
        // A legacy daemon acks with no task id. A stream is in flight, but with
        // no cancel handle no Cancel affordance can be offered.
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        state.apply(UiMessage::PromptSent {
            task_id: String::new(),
            conversation_id: "c1".to_string(),
            idempotency_key: None,
        });
        assert!(
            state.streaming_is_active_for_view(),
            "a stream is in flight"
        );
        assert_eq!(
            state.active_task_id_for_view(),
            None,
            "but with no task id it cannot be cancelled"
        );
    }

    #[test]
    fn active_task_id_for_view_is_none_for_an_adopted_external_turn() {
        // A turn started elsewhere (a voice turn / another client) adopted into
        // the open conversation streams live, but this client never received its
        // ack, so it holds no task id and cannot offer Cancel for it.
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "r1".to_string(),
            content: "hi from voice".to_string(),
            idempotency_key: None,
        });
        assert!(
            state.streaming_is_active_for_view(),
            "the adopted external turn streams into the view"
        );
        assert_eq!(
            state.active_task_id_for_view(),
            None,
            "an adopted external turn carries no local task id"
        );
    }

    #[test]
    fn active_task_id_for_view_tracks_only_the_open_conversation() {
        // A turn is in flight on a backgrounded conversation; the open one has
        // none. The view's Cancel handle must reflect the OPEN conversation, so
        // the background turn's id must not leak into it.
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        state.apply(UiMessage::PromptSent {
            task_id: "task-bg".to_string(),
            conversation_id: "c2".to_string(),
            idempotency_key: None,
        });
        assert_eq!(
            state.active_task_id_for_view(),
            None,
            "c1 is open with no turn; c2's background turn is not the view's"
        );
    }

    // --- retry a failed/timed-out turn (#138 item 3) --------------------------

    /// Drive a direct send into `c1`, open its stream, then fail it.
    fn send_then_fail(prompt: &str) -> WindowState {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: prompt.to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::PromptSent {
            task_id: "t-1".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::StreamError {
            request_id: "r1".to_string(),
            error: "the turn was abandoned".to_string(),
        });
        state
    }

    #[test]
    fn stream_error_offers_retry_of_the_failed_prompt() {
        // The timed-out/abandoned turn's prompt is offered back so the user can
        // resend it ("try again") — the prompt is never dropped.
        let mut state = send_then_fail("summarize the meeting notes");
        assert_eq!(
            state.take_pending_retry_prompt().as_deref(),
            Some("summarize the meeting notes")
        );
    }

    #[test]
    fn retry_offer_is_consumed_once() {
        let mut state = send_then_fail("hello");
        assert_eq!(state.take_pending_retry_prompt().as_deref(), Some("hello"));
        assert_eq!(
            state.take_pending_retry_prompt(),
            None,
            "the offer is one-shot: a second take yields nothing"
        );
    }

    #[test]
    fn retry_is_not_offered_when_follow_ups_were_queued() {
        // The user queued a follow-up while the turn streamed, then it failed.
        // Their queued message is theirs to send (it flushes); we do not also
        // shove the failed prompt back into the composer over the top of it.
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: "first".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::PromptSent {
            task_id: "t-1".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: None,
        });
        // A second send while streaming is QUEUED, not sent.
        state.apply(UiMessage::SubmitPrompt {
            prompt: "second".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::StreamError {
            request_id: "r1".to_string(),
            error: "boom".to_string(),
        });
        assert_eq!(
            state.take_pending_retry_prompt(),
            None,
            "queued follow-ups flush; the failed prompt is not re-offered on top"
        );
    }

    #[test]
    fn retry_is_not_offered_for_a_background_conversation_failure() {
        // A turn on a conversation that is NOT in view fails; the open
        // conversation's composer must not be touched.
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        // A background turn on c2 (no bubble drawn in the open view).
        state.apply(UiMessage::PromptSent {
            task_id: "t-2".to_string(),
            conversation_id: "c2".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::StreamError {
            request_id: "r2".to_string(),
            error: "boom".to_string(),
        });
        assert_eq!(state.take_pending_retry_prompt(), None);
    }

    #[test]
    fn a_successful_turn_offers_no_retry() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: "hello".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::PromptSent {
            task_id: "t-1".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::StreamComplete {
            request_id: "r1".to_string(),
            full_response: "hi there".to_string(),
        });
        assert_eq!(
            state.take_pending_retry_prompt(),
            None,
            "a turn that finished has nothing to retry"
        );
    }

    // --- SubmitPrompt / SendFailed: the core-owned send decision ----------

    #[test]
    fn submit_prompt_draws_the_bubble_and_emits_send_prompt() {
        let mut state = WindowState {
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: "hello".to_string(),
            idempotency_key: None,
        });
        // Optimistic user bubble drawn into the open transcript...
        let conv = state.current_conversation().unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "hello");
        // ...and the RPC effect emitted for the client to run. Adele is Disabled
        // by default, so no voice refinement rides along.
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::SendPrompt { conversation_id, prompt, system_refinement, .. }]
                    if conversation_id == "c1" && prompt == "hello" && system_refinement.is_none()
            ),
            "{effects:?}"
        );
    }

    #[test]
    fn submit_prompt_carries_the_voice_refinement_when_adele_is_on() {
        let mut state = WindowState {
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        state.apply(UiMessage::SetAdeleOutput {
            conversation_id: "c1".to_string(),
            level: AdeleOutput::OnDemand,
        });
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: "hi".to_string(),
            idempotency_key: None,
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
    fn submit_prompt_while_streaming_queues_instead_of_refusing() {
        // Queue-while-busy: a send mid-stream is no longer refused — it is
        // QUEUED. No bubble, no RPC yet; the composer clears and the message
        // joins this conversation's outbox for a combined flush on completion.
        let mut state = mid_stream_state("c1", "c1");
        let before = state.current_conversation().unwrap().messages.len();
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: "second".to_string(),
            idempotency_key: None,
        });
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::SetComposerText(t),
                    Effect::SetQueuedMessages { messages, editing: None }
                ] if t.is_empty() && messages == &["second".to_string()]
            ),
            "a mid-stream send clears the composer and queues the text: {effects:?}"
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::SendPrompt { .. })),
            "a queued send must not emit an RPC yet: {effects:?}"
        );
        assert_eq!(
            state.current_conversation().unwrap().messages.len(),
            before,
            "a queued send must not append a bubble yet"
        );
        assert_eq!(
            state.queued_messages_for_view(),
            &["second".to_string()],
            "the text is now queued"
        );
    }

    #[test]
    fn submit_prompt_empty_is_a_silent_noop() {
        let mut state = WindowState {
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: String::new(),
            idempotency_key: None,
        });
        assert!(effects.is_empty());
        assert!(state.current_conversation().unwrap().messages.is_empty());
    }

    #[test]
    fn send_failed_rolls_back_the_matching_optimistic_tail() {
        let mut state = WindowState {
            ..Default::default()
        }
        .with_open(detail("c1", vec![msg("user", "doomed")]));
        let effects = state.apply(UiMessage::SendFailed {
            conversation_id: "c1".to_string(),
            prompt: "doomed".to_string(),
        });
        assert!(effects.is_empty());
        assert!(
            state.current_conversation().unwrap().messages.is_empty(),
            "the optimistic user bubble must be rolled back"
        );
    }

    #[test]
    fn send_failed_leaves_another_conversations_tail_intact() {
        // The user switched conversations between submit and the failure; the now
        // open conversation's transcript must not be touched (TUI-2).
        let mut state = WindowState {
            ..Default::default()
        }
        .with_open(detail("c2", vec![msg("user", "different")]));
        state.apply(UiMessage::SendFailed {
            conversation_id: "c1".to_string(),
            prompt: "doomed".to_string(),
        });
        assert_eq!(
            state.current_conversation().unwrap().messages.len(),
            1,
            "the other conversation's transcript stays intact"
        );
    }

    #[test]
    fn send_failed_does_not_pop_a_non_matching_tail() {
        // Something landed after the optimistic append (e.g. an inline note): only
        // an exact matching tail is rolled back, never an unrelated last message.
        let mut state = WindowState {
            ..Default::default()
        }
        .with_open(detail(
            "c1",
            vec![msg("user", "doomed"), msg("assistant", "(an aside)")],
        ));
        state.apply(UiMessage::SendFailed {
            conversation_id: "c1".to_string(),
            prompt: "doomed".to_string(),
        });
        assert_eq!(
            state.current_conversation().unwrap().messages.len(),
            2,
            "a non-matching tail must not be popped"
        );
    }

    // --- composer drafts (#2): per-conversation unsent text ---------------

    #[test]
    fn composer_draft_round_trips_and_is_per_conversation() {
        let mut state = WindowState::default();
        // Absent → empty.
        assert_eq!(state.composer_draft("c1"), "");
        // Independent drafts per conversation.
        state.set_composer_draft("c1", "half a thought".to_string());
        state.set_composer_draft("c2", "a different one".to_string());
        assert_eq!(state.composer_draft("c1"), "half a thought");
        assert_eq!(state.composer_draft("c2"), "a different one");
        // Overwrite replaces in place.
        state.set_composer_draft("c1", "rewritten".to_string());
        assert_eq!(state.composer_draft("c1"), "rewritten");
    }

    #[test]
    fn set_composer_draft_empty_clears_the_entry() {
        let mut state = WindowState::default();
        state.set_composer_draft("c1", "typing".to_string());
        // An empty snapshot drops the draft so the map only retains real ones.
        state.set_composer_draft("c1", String::new());
        assert_eq!(state.composer_draft("c1"), "");
    }

    #[test]
    fn submit_prompt_clears_the_sent_conversations_draft() {
        // A switch-away snapshot saved a draft for c1; sending it must drop the
        // saved draft so switching back can't resurrect the just-sent text.
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.set_composer_draft("c1", "hello".to_string());
        state.apply(UiMessage::SubmitPrompt {
            prompt: "hello".to_string(),
            idempotency_key: None,
        });
        assert_eq!(
            state.composer_draft("c1"),
            "",
            "a committed send clears the conversation's saved draft"
        );
    }

    #[test]
    fn a_queued_send_moves_the_draft_into_the_outbox() {
        // A send mid-stream is queued, not refused: the text moves from the
        // live composer into the outbox, so the saved draft is cleared (the
        // client clears its live composer via SetComposerText) and the message
        // now lives in the queue awaiting a combined flush.
        let mut state = mid_stream_state("c1", "c1");
        state.set_composer_draft("c1", "queued".to_string());
        state.apply(UiMessage::SubmitPrompt {
            prompt: "queued".to_string(),
            idempotency_key: None,
        });
        assert_eq!(
            state.composer_draft("c1"),
            "",
            "a queued send clears the saved draft (the text moved to the queue)"
        );
        assert_eq!(
            state.queued_messages_for_view(),
            &["queued".to_string()],
            "the text is now in the outbox"
        );
    }

    #[test]
    fn deleting_a_conversation_prunes_its_draft() {
        // GTK-9: per-conversation state is pruned on delete so a later id reuse
        // can't inherit a stale draft.
        let mut state = WindowState {
            conversations: vec![summary("c1", "one", false)],
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        state.set_composer_draft("c1", "orphan".to_string());
        state.apply(UiMessage::ConversationDeleted {
            id: "c1".to_string(),
        });
        assert_eq!(state.composer_draft("c1"), "");
        assert!(
            state.open.is_empty(),
            "the deleted conversation's model (and its draft) must be pruned"
        );
    }

    // --- GTK-2: in-flight stream vs conversation switch -------------------

    /// Pin a (claimed) in-flight stream onto conversation `from`'s model, viewed
    /// from `current`. When `from != current` both models exist in `open`: `from`
    /// holds the backgrounded stream, `current` is the open transcript.
    fn mid_stream_state(from: &str, current: &str) -> WindowState {
        WindowState::default()
            .with_stream(
                from,
                StreamState {
                    request_id: Some("req-real".to_string()),
                    buffer: "partial ".to_string(),
                    ..Default::default()
                },
            )
            .with_open(detail(current, vec![msg("user", "hi")]))
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
        // The chunk accumulates into the ORIGINATING conversation's own stream
        // (c1), not the open one — `streaming_buffer()` reads the open (c2)
        // conversation, which isn't streaming, so it stays empty.
        assert_eq!(
            state.stream_of("c1").unwrap().buffer,
            "partial more",
            "the chunk must still accumulate for the originating conversation"
        );
        assert_eq!(
            state.streaming_buffer(),
            "",
            "the open conversation (c2) isn't streaming, so the view buffer stays empty"
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
        let before = state.current_conversation().unwrap().messages.len();

        state.reset_streaming_state();

        assert!(!state.is_streaming(), "the pending stream must be cleared");
        assert_eq!(state.streaming_buffer(), "", "the partial must be dropped");
        // The emptied buffer is what makes the view's render guard inert
        // (`!buffer.is_empty() && active`): with no originating conversation
        // recorded, `streaming_is_active_for_view()` is vacuously true, so it's
        // the empty buffer — not the guard — that stops the partial painting.
        assert_eq!(
            state.current_conversation().unwrap().messages.len(),
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
        let current = state.current_conversation().unwrap();
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
        let current = state.current_conversation().unwrap();
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
        state.open.entry("c1".to_string()).or_default().adele_output = AdeleOutput::Always;
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "an answer".to_string(),
        });
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "a background conversation's reply must not be narrated: {effects:?}"
        );
    }

    // --- Concurrent per-conversation streams (Phase-2 Step-2b-ii) --------

    /// Two conversations streaming at once: `c1` is backgrounded (its claimed
    /// stream has a buffered prefix), `c2` is the open conversation and also
    /// streaming. The capability the per-conversation fold unlocks.
    fn two_streams_state() -> WindowState {
        WindowState::default()
            .with_stream(
                "c1",
                StreamState {
                    request_id: Some("req-c1".to_string()),
                    buffer: "c1 partial ".to_string(),
                    ..Default::default()
                },
            )
            .with_stream(
                "c2",
                StreamState {
                    request_id: Some("req-c2".to_string()),
                    buffer: "c2 partial ".to_string(),
                    ..Default::default()
                },
            )
            .with_open(detail("c2", vec![msg("user", "hi from c2")]))
    }

    /// A chunk for the BACKGROUNDED conversation accumulates into its own model
    /// and emits no render Effect into the open conversation — the core enabler
    /// of background streaming.
    #[test]
    fn concurrent_chunk_for_background_conversation_accumulates_silently() {
        let mut state = two_streams_state();
        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "req-c1".to_string(),
            chunk: "more c1".to_string(),
        });
        assert!(
            effects.is_empty(),
            "a backgrounded conversation's chunk must emit no view Effect: {effects:?}"
        );
        assert_eq!(
            state.stream_of("c1").unwrap().buffer,
            "c1 partial more c1",
            "the chunk must accumulate into c1's own stream"
        );
        // The open conversation's stream is untouched, and what the view reads
        // back is still c2's partial.
        assert_eq!(state.stream_of("c2").unwrap().buffer, "c2 partial ");
        assert_eq!(state.streaming_buffer(), "c2 partial ");
    }

    /// A chunk for the OPEN conversation renders live while the other streams in
    /// the background — the two never cross-wire.
    #[test]
    fn concurrent_chunk_for_open_conversation_renders_and_does_not_touch_other() {
        let mut state = two_streams_state();
        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "req-c2".to_string(),
            chunk: "more c2".to_string(),
        });
        assert!(
            matches!(effects.as_slice(), [Effect::ReceiveChunk(c)] if c == "more c2"),
            "the open conversation's chunk renders live (non-first, no status clear): {effects:?}"
        );
        assert_eq!(state.stream_of("c2").unwrap().buffer, "c2 partial more c2");
        assert_eq!(
            state.stream_of("c1").unwrap().buffer,
            "c1 partial ",
            "the backgrounded conversation's stream must be undisturbed"
        );
    }

    /// Switching TO a backgrounded streaming conversation re-seeds ITS partial
    /// (not the one we left) — its buffered prefix returns to the view.
    #[test]
    fn switching_to_other_streaming_conversation_reseeds_its_own_partial() {
        let mut state = two_streams_state();
        let effects = state.apply(UiMessage::ConversationLoaded(detail("c1", vec![])));
        let seed = effects
            .iter()
            .position(|e| matches!(e, Effect::ReceiveChunk(c) if c == "c1 partial "));
        let load = effects
            .iter()
            .position(|e| matches!(e, Effect::LoadConversationIntoChat(_)));
        assert!(
            seed.is_some(),
            "switching to c1 must re-seed c1's buffered partial: {effects:?}"
        );
        assert!(
            load < seed,
            "the seed must render after the load: {effects:?}"
        );
        // Both streams keep buffering — neither was finalized by the switch.
        assert!(state.stream_of("c1").is_some());
        assert!(state.stream_of("c2").is_some());
        // The view now reads c1's partial.
        assert_eq!(state.streaming_buffer(), "c1 partial ");
    }

    /// Completing ONE stream finalizes only its conversation; the other keeps
    /// streaming undisturbed.
    #[test]
    fn completing_one_stream_leaves_the_other_streaming() {
        let mut state = two_streams_state();
        // Complete the OPEN conversation's (c2) stream.
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-c2".to_string(),
            full_response: "c2 done".to_string(),
        });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::CompleteStreaming(c) if c == "c2 done")),
            "the open conversation's completion must finalize into the view: {effects:?}"
        );
        assert!(state.stream_of("c2").is_none(), "c2's stream is cleared");
        assert_eq!(
            state.stream_of("c1").unwrap().buffer,
            "c1 partial ",
            "c1 must still be streaming, undisturbed"
        );
        assert!(
            state.is_streaming(),
            "a turn (c1) is still in flight somewhere"
        );
    }

    /// Completing the BACKGROUNDED stream touches nothing in the open chat (no
    /// CompleteStreaming for the conversation not in view) and leaves the open
    /// conversation's stream intact. It reports the finished turn (#51), which
    /// renders nothing.
    #[test]
    fn completing_background_stream_does_not_touch_open_conversation() {
        let mut state = two_streams_state();
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-c1".to_string(),
            full_response: "c1 done".to_string(),
        });
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::TurnFinished { conversation_id, .. }] if conversation_id == "c1"
            ),
            "a backgrounded completion must not render into the open chat: {effects:?}"
        );
        assert!(state.stream_of("c1").is_none(), "c1's stream is cleared");
        assert_eq!(
            state.stream_of("c2").unwrap().buffer,
            "c2 partial ",
            "the open conversation's stream must be untouched"
        );
        // The open conversation's transcript must not have gained c1's reply.
        assert!(
            state
                .current_conversation()
                .unwrap()
                .messages
                .iter()
                .all(|m| m.content != "c1 done"),
            "c1's reply must not leak into c2's transcript"
        );
    }

    /// Erroring one stream surfaces its error but leaves the other streaming.
    #[test]
    fn erroring_one_stream_leaves_the_other_streaming() {
        let mut state = two_streams_state();
        let effects = state.apply(UiMessage::StreamError {
            request_id: "req-c1".to_string(),
            error: "c1 boom".to_string(),
        });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SetStatusText(t) if t == "Error: c1 boom")),
            "the error must surface on the global status line: {effects:?}"
        );
        // c1 was backgrounded, so its failure must NOT clear the open chat status.
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::ClearChatStatus)),
            "a backgrounded stream's error must not blank the open conversation's chat: {effects:?}"
        );
        assert!(state.stream_of("c1").is_none(), "c1's stream is cleared");
        assert_eq!(
            state.stream_of("c2").unwrap().buffer,
            "c2 partial ",
            "c2 must still be streaming, undisturbed"
        );
    }

    /// TUI-7 relaxed for per-conversation streams: a second send to a DIFFERENT,
    /// idle conversation is allowed while another streams in the background.
    #[test]
    fn second_send_to_idle_conversation_is_allowed_while_another_streams() {
        // c1 streams in the background; c3 (idle) is the open conversation.
        let mut state = WindowState::default()
            .with_stream(
                "c1",
                StreamState {
                    request_id: Some("req-c1".to_string()),
                    buffer: "c1 partial ".to_string(),
                    ..Default::default()
                },
            )
            .with_open(detail("c3", vec![]));
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: "hello c3".to_string(),
            idempotency_key: None,
        });
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::SendPrompt { conversation_id, prompt, .. }]
                    if conversation_id == "c3" && prompt == "hello c3"
            ),
            "a send to an idle conversation must be allowed while another streams: {effects:?}"
        );
        // The optimistic bubble landed in c3, and c1 keeps streaming.
        assert_eq!(state.current_conversation().unwrap().messages.len(), 1);
        assert_eq!(state.stream_of("c1").unwrap().buffer, "c1 partial ");
    }

    /// A second send to the SAME conversation that is already streaming is
    /// queued (its single stream slot renders one turn at a time) — the burst
    /// flushes as one combined follow-up when the reply finishes.
    #[test]
    fn second_send_to_the_streaming_conversation_is_queued() {
        // c1 streams AND is the open conversation: a send into it is queued.
        let mut state = WindowState::default()
            .with_stream(
                "c1",
                StreamState {
                    request_id: Some("req-c1".to_string()),
                    buffer: "c1 partial ".to_string(),
                    ..Default::default()
                },
            )
            .with_open(detail("c1", vec![msg("user", "first")]));
        let before = state.current_conversation().unwrap().messages.len();
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: "second".to_string(),
            idempotency_key: None,
        });
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::SendPrompt { .. })),
            "a send into the already-streaming open conversation must not emit an RPC: {effects:?}"
        );
        assert_eq!(
            state.queued_messages_for_view(),
            &["second".to_string()],
            "the send into the streaming conversation is queued"
        );
        assert_eq!(
            state.current_conversation().unwrap().messages.len(),
            before,
            "a queued send must not append a bubble yet"
        );
    }

    // --- Message queuing (queue-while-busy + edit) -----------------------

    #[test]
    fn queued_messages_flush_as_one_combined_send_on_stream_complete() {
        // The headline behavior: two Enters while a reply streams become ONE
        // newline-joined follow-up turn when the reply completes.
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "check the weather".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "in Boston".to_string(),
            idempotency_key: None,
        });
        assert_eq!(
            state.queued_messages_for_view(),
            &["check the weather".to_string(), "in Boston".to_string()],
            "both sends are queued in submit order"
        );
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "done".to_string(),
        });
        let sent = effects.iter().find_map(|e| match e {
            Effect::SendPrompt {
                conversation_id,
                prompt,
                ..
            } => Some((conversation_id.clone(), prompt.clone())),
            _ => None,
        });
        assert_eq!(
            sent,
            Some((
                "c1".to_string(),
                "check the weather\n\nin Boston".to_string()
            )),
            "the whole queue flushes as ONE newline-joined send: {effects:?}"
        );
        // The combined send is emitted AFTER the reply is finalized.
        let complete_at = effects
            .iter()
            .position(|e| matches!(e, Effect::CompleteStreaming(_)));
        let send_at = effects
            .iter()
            .position(|e| matches!(e, Effect::SendPrompt { .. }));
        assert!(
            complete_at < send_at,
            "the flush follows the finalized reply: {effects:?}"
        );
        assert!(
            state.queued_messages_for_view().is_empty(),
            "the queue is cleared after flush"
        );
    }

    #[test]
    fn a_flush_echo_arriving_before_the_ack_is_not_drawn_as_a_second_bubble() {
        // Regression (queue-flush double render): the reply completes, the queue
        // flushes as ONE combined send, and `commit_send` draws the optimistic
        // user bubble. The daemon — primed by the turn that just finished —
        // can echo `UserMessageAdded` for the follow-up BEFORE it acks the send
        // (`PromptSent`), so the dedup's `__pending__` stream does not exist yet
        // and the echo would fall through to the "external turn" path and draw
        // the bubble a SECOND time. It must instead be recognized as our own
        // send and rendered nothing.
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "a".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "b".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "done".to_string(),
        });
        let combined = format!("a{QUEUE_JOIN}b");
        // The echo arrives BEFORE the send's ack (`PromptSent`).
        let echo = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "req-2".to_string(),
            content: combined.clone(),
            idempotency_key: None,
        });
        assert!(
            !echo.iter().any(|e| matches!(e, Effect::AddUserMessage(_))),
            "the echo of our own flushed send must not draw a second bubble: {echo:?}"
        );
        let combined_bubbles = state
            .current_conversation()
            .expect("c1 is open")
            .messages
            .iter()
            .filter(|m| m.role == "user" && m.content == combined)
            .count();
        assert_eq!(
            combined_bubbles, 1,
            "exactly one combined user bubble, not two"
        );
        // The ack arrives after the echo (the flush race) and opens the pending
        // stream as usual; it draws no bubble, and the model still holds exactly
        // one combined bubble.
        let ack = state.apply(UiMessage::PromptSent {
            task_id: String::new(),
            conversation_id: "c1".to_string(),
            idempotency_key: None,
        });
        assert!(
            !ack.iter().any(|e| matches!(e, Effect::AddUserMessage(_))),
            "the late ack draws no bubble: {ack:?}"
        );
        assert!(
            state.current_stream().is_some(),
            "the follow-up turn is live after the ack"
        );
        let after_ack = state
            .current_conversation()
            .expect("c1 is open")
            .messages
            .iter()
            .filter(|m| m.role == "user" && m.content == combined)
            .count();
        assert_eq!(
            after_ack, 1,
            "still exactly one combined bubble after the ack"
        );
    }

    #[test]
    fn queued_messages_join_with_a_blank_line_between_them() {
        // A queued burst should read as separate paragraphs, not run-together
        // lines: each message is separated by a blank line (an EOL plus an
        // empty line) so the combined turn is legible.
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "one".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "two".to_string(),
            idempotency_key: None,
        });
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "done".to_string(),
        });
        let sent = effects.iter().find_map(|e| match e {
            Effect::SendPrompt { prompt, .. } => Some(prompt.clone()),
            _ => None,
        });
        assert_eq!(
            sent,
            Some("one\n\ntwo".to_string()),
            "queued messages join with a blank line between them: {effects:?}"
        );
    }

    #[test]
    fn queue_flushes_on_stream_error_too() {
        // A failed turn still flushes the user's queued follow-ups: they're the
        // user's messages, not the failed reply's.
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "one".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "two".to_string(),
            idempotency_key: None,
        });
        let effects = state.apply(UiMessage::StreamError {
            request_id: "req-real".to_string(),
            error: "boom".to_string(),
        });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SendPrompt { prompt, .. } if prompt == "one\n\ntwo")),
            "a failed turn flushes the queued follow-ups as one: {effects:?}"
        );
        assert!(state.queued_messages_for_view().is_empty());
    }

    // --- #25: queued prompts must survive a failed/abandoned flush ---

    #[test]
    fn queued_prompts_survive_flush_failure() {
        // #25: when an abandoned turn flushes the queue as one combined send but
        // that send then ALSO fails (the backend is still wedged), the user's
        // queued prompts must NOT vanish — they return to the queue so the user
        // can retry ("try again"). Today `flush_outbox` `mem::take`s the queue
        // and `SendFailed` never restores it, so they are lost.
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "one".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "two".to_string(),
            idempotency_key: None,
        });
        // The 90s watchdog abandons the turn -> the reducer flushes the queue.
        let flushed = state.apply(UiMessage::StreamError {
            request_id: "req-real".to_string(),
            error: "no response from the daemon for 90s; the turn was abandoned".to_string(),
        });
        assert!(
            flushed
                .iter()
                .any(|e| matches!(e, Effect::SendPrompt { prompt, .. } if prompt == "one\n\ntwo")),
            "abandonment flushes the queued follow-ups as one combined send: {flushed:?}"
        );
        // That combined send also fails (backend still wedged).
        state.apply(UiMessage::SendFailed {
            conversation_id: "c1".to_string(),
            prompt: "one\n\ntwo".to_string(),
        });
        // The queued prompts must survive for retry.
        assert_eq!(
            state.queued_messages_for_view(),
            vec!["one".to_string(), "two".to_string()],
            "queued prompts must return to the queue after a failed flush so the user can try again"
        );
    }

    #[test]
    fn flush_ack_discards_pending_so_a_later_failure_does_not_resurrect() {
        // Once the flush is acked (PromptSent), the held copy is discarded; a
        // stale/duplicate SendFailed arriving afterwards must not resurrect an
        // already-sent message.
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "one".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::StreamError {
            request_id: "req-real".to_string(),
            error: "boom".to_string(),
        });
        // The flush was accepted by the daemon.
        state.apply(UiMessage::PromptSent {
            task_id: "t1".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: None,
        });
        // A late/duplicate failure must not re-queue an already-sent message.
        state.apply(UiMessage::SendFailed {
            conversation_id: "c1".to_string(),
            prompt: "one".to_string(),
        });
        assert!(
            state.queued_messages_for_view().is_empty(),
            "an acked flush must not be resurrected by a later SendFailed"
        );
    }

    #[test]
    fn direct_send_failure_leaves_the_queue_untouched() {
        // A failed *direct* send (nothing was ever queued) must not fabricate
        // queue entries — the pending-flush restore applies only to flushes.
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.current_conversation_id = Some("c1".to_string());
        state.apply(UiMessage::SubmitPrompt {
            prompt: "hello".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::SendFailed {
            conversation_id: "c1".to_string(),
            prompt: "hello".to_string(),
        });
        assert!(
            state.queued_messages_for_view().is_empty(),
            "a failed direct send must not create phantom queued messages"
        );
    }

    #[test]
    fn a_flush_in_flight_blocks_a_second_overlapping_flush() {
        // Stage the sent-but-unacked window directly (same-module access): no
        // stream, a prior flush parked in `pending_flush`, and a freshly-queued
        // message in the outbox. A flush must NOT fire a second overlapping send
        // — the in-flight flush owns the queue until it is acked or restored.
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.current_conversation_id = Some("c1".to_string());
        {
            let model = state.open.get_mut("c1").expect("open model for c1");
            model.pending_flush = vec![QueuedMessage {
                text: "in-flight".to_string(),
                idempotency_key: None,
            }];
            model.outbox = vec![QueuedMessage {
                text: "new".to_string(),
                idempotency_key: None,
            }];
        }
        let effects = state.flush_outbox();
        assert!(
            effects.is_empty(),
            "a flush must not fire while one is already in flight: {effects:?}"
        );
    }

    #[test]
    fn flush_does_not_touch_the_live_composer() {
        // The user may be mid-typing a fresh (not-yet-Entered) message when the
        // reply completes. The flush sends only the committed queue and must NOT
        // emit SetComposerText, so the live draft survives.
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "queued".to_string(),
            idempotency_key: None,
        });
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "done".to_string(),
        });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SendPrompt { .. })),
            "the queue flushed: {effects:?}"
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::SetComposerText(_))),
            "flush must not touch the live composer (a fresh draft must survive): {effects:?}"
        );
    }

    #[test]
    fn submitting_while_idle_with_a_pending_queue_flushes_all_as_one() {
        // Defensive path (3): a conversation left idle with a pending queue (as a
        // backgrounded completion leaves it). The next Enter appends and flushes
        // the whole batch as one.
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "first".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "second".to_string(),
            idempotency_key: None,
        });
        // Force idle WITHOUT flushing (as a backgrounded completion would).
        state.reset_streaming_state();
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: "third".to_string(),
            idempotency_key: None,
        });
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::SendPrompt { prompt, .. } if prompt == "first\n\nsecond\n\nthird"
            )),
            "an Enter on an idle conversation with a pending queue flushes all: {effects:?}"
        );
        assert!(state.queued_messages_for_view().is_empty());
    }

    #[test]
    fn submitting_while_idle_with_no_queue_sends_immediately() {
        // Regression: the original single-send path is unchanged when idle with
        // an empty queue.
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: "hello".to_string(),
            idempotency_key: None,
        });
        assert!(
            matches!(effects.as_slice(), [Effect::SendPrompt { prompt, .. }] if prompt == "hello"),
            "an idle send with an empty queue goes out immediately: {effects:?}"
        );
        assert!(state.queued_messages_for_view().is_empty());
    }

    #[test]
    fn editing_a_queued_message_checks_it_out_and_resubmit_reinserts_in_place() {
        let mut state = mid_stream_state("c1", "c1");
        for m in ["alpha", "bravo", "charlie"] {
            state.apply(UiMessage::SubmitPrompt {
                prompt: m.to_string(),
                idempotency_key: None,
            });
        }
        let effects = state.apply(UiMessage::EditQueued { index: 1 });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SetComposerText(t) if t == "bravo")),
            "the checked-out message loads into the composer: {effects:?}"
        );
        assert_eq!(
            state.queued_messages_for_view(),
            &["alpha".to_string(), "charlie".to_string()],
            "the checked-out message leaves the visible queue"
        );
        assert_eq!(state.editing_queued_index(), Some(1));
        // Edit and re-submit while still streaming → stays queued, reinserted at 1.
        state.apply(UiMessage::SubmitPrompt {
            prompt: "bravo EDITED".to_string(),
            idempotency_key: None,
        });
        assert_eq!(
            state.queued_messages_for_view(),
            &[
                "alpha".to_string(),
                "bravo EDITED".to_string(),
                "charlie".to_string()
            ],
            "the edited message reinserts in its original slot"
        );
        assert_eq!(state.editing_queued_index(), None);
    }

    #[test]
    fn edit_checkout_reports_the_editing_index_in_the_snapshot() {
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "x".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "y".to_string(),
            idempotency_key: None,
        });
        let effects = state.apply(UiMessage::EditQueued { index: 0 });
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::SetQueuedMessages { editing: Some(0), messages } if messages == &["y".to_string()]
            )),
            "the snapshot reports the edited index and the remaining queue: {effects:?}"
        );
    }

    #[test]
    fn editing_an_out_of_range_index_is_a_safe_noop() {
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "only".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::EditQueued { index: 5 });
        assert_eq!(state.queued_messages_for_view(), &["only".to_string()]);
        assert_eq!(state.editing_queued_index(), None);
    }

    #[test]
    fn checking_out_another_message_returns_the_previous_one_to_the_queue() {
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "a".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "b".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::EditQueued { index: 1 }); // check out "b"
        assert_eq!(state.editing_queued_index(), Some(1));
        assert_eq!(state.queued_messages_for_view(), &["a".to_string()]);
        // Check out "a" without submitting: "b" returns to the queue first.
        let effects = state.apply(UiMessage::EditQueued { index: 0 });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SetComposerText(t) if t == "a")),
            "the newly checked-out message loads: {effects:?}"
        );
        assert_eq!(state.editing_queued_index(), Some(0));
        assert_eq!(
            state.queued_messages_for_view(),
            &["b".to_string()],
            "the previously checked-out message is back in the queue"
        );
    }

    #[test]
    fn removing_a_queued_message_drops_it() {
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "keep".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "drop".to_string(),
            idempotency_key: None,
        });
        let effects = state.apply(UiMessage::RemoveQueued { index: 1 });
        assert_eq!(state.queued_messages_for_view(), &["keep".to_string()]);
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::SetQueuedMessages { messages, .. }] if messages == &["keep".to_string()]
            ),
            "removal emits a fresh queue snapshot: {effects:?}"
        );
    }

    #[test]
    fn removing_an_out_of_range_index_is_ignored() {
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "only".to_string(),
            idempotency_key: None,
        });
        let effects = state.apply(UiMessage::RemoveQueued { index: 9 });
        assert!(
            effects.is_empty(),
            "an out-of-range remove is a no-op: {effects:?}"
        );
        assert_eq!(state.queued_messages_for_view(), &["only".to_string()]);
    }

    #[test]
    fn removing_an_earlier_queued_item_shifts_the_edit_slot() {
        let mut state = mid_stream_state("c1", "c1");
        for m in ["a", "b", "c"] {
            state.apply(UiMessage::SubmitPrompt {
                prompt: m.to_string(),
                idempotency_key: None,
            });
        }
        state.apply(UiMessage::EditQueued { index: 2 }); // check out "c"; queue ["a","b"]
        assert_eq!(state.editing_queued_index(), Some(2));
        state.apply(UiMessage::RemoveQueued { index: 0 }); // remove "a"
        assert_eq!(
            state.editing_queued_index(),
            Some(1),
            "the reinsert slot shifts down when an earlier item is removed"
        );
        assert_eq!(state.queued_messages_for_view(), &["b".to_string()]);
        state.apply(UiMessage::SubmitPrompt {
            prompt: "c".to_string(),
            idempotency_key: None,
        });
        assert_eq!(
            state.queued_messages_for_view(),
            &["b".to_string(), "c".to_string()],
            "the edited message reinserts after the surviving item"
        );
    }

    #[test]
    fn cancelling_an_edit_restores_the_original_message() {
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "original".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::EditQueued { index: 0 });
        assert!(state.queued_messages_for_view().is_empty());
        let effects = state.apply(UiMessage::CancelQueuedEdit);
        assert_eq!(
            state.queued_messages_for_view(),
            &["original".to_string()],
            "cancel returns the checked-out message to the queue unchanged"
        );
        assert_eq!(state.editing_queued_index(), None);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SetComposerText(t) if t.is_empty())),
            "cancel clears the composer: {effects:?}"
        );
    }

    #[test]
    fn cancel_with_nothing_checked_out_is_a_noop() {
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "q".to_string(),
            idempotency_key: None,
        });
        let effects = state.apply(UiMessage::CancelQueuedEdit);
        assert!(
            effects.is_empty(),
            "cancel with no edit in progress is a no-op: {effects:?}"
        );
        assert_eq!(state.queued_messages_for_view(), &["q".to_string()]);
    }

    #[test]
    fn submitting_an_emptied_edit_drops_the_message() {
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "a".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "b".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::EditQueued { index: 0 }); // check out "a"
        state.apply(UiMessage::SubmitPrompt {
            prompt: String::new(),
            idempotency_key: None,
        });
        assert_eq!(
            state.queued_messages_for_view(),
            &["b".to_string()],
            "clearing a checked-out message and submitting drops it"
        );
        assert_eq!(state.editing_queued_index(), None);
    }

    #[test]
    fn an_empty_submit_while_streaming_does_not_queue_a_blank() {
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "   ".to_string(),
            idempotency_key: None,
        });
        assert!(
            state.queued_messages_for_view().is_empty(),
            "a whitespace-only submit must not create a blank queued chip"
        );
    }

    #[test]
    fn queues_are_per_conversation() {
        // Queue into c1 (streaming), then open c2 (idle): c1's queue must not
        // appear for c2, and c1 keeps it while backgrounded.
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "for c1".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::ConversationLoaded(detail("c2", vec![])));
        assert!(
            state.queued_messages_for_view().is_empty(),
            "c2 has its own empty queue"
        );
        assert_eq!(
            state.queued_messages("c1"),
            &["for c1".to_string()],
            "c1 keeps its queue while backgrounded"
        );
    }

    #[test]
    fn a_backgrounded_conversations_queue_flushes_on_switch_back() {
        // Queue into c1 while it streams, switch away to c2, c1's reply completes
        // while backgrounded (no flush), then switch back to c1 → the queue
        // flushes as one.
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "later".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::ConversationLoaded(detail("c2", vec![])));
        let bg = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "c1 done".to_string(),
        });
        assert!(
            !bg.iter().any(|e| matches!(e, Effect::SendPrompt { .. })),
            "a backgrounded completion must not flush yet: {bg:?}"
        );
        assert_eq!(
            state.queued_messages("c1"),
            &["later".to_string()],
            "the queue waits until the conversation is back in view"
        );
        let effects = state.apply(UiMessage::ConversationLoaded(detail(
            "c1",
            vec![msg("user", "hi")],
        )));
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::SendPrompt { conversation_id, prompt, .. }
                    if conversation_id == "c1" && prompt == "later"
            )),
            "switching back to an idle conversation flushes its queued messages: {effects:?}"
        );
        assert!(state.queued_messages("c1").is_empty());
    }

    #[test]
    fn submitting_with_no_open_conversation_is_a_noop() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::SubmitPrompt {
            prompt: "hi".to_string(),
            idempotency_key: None,
        });
        assert!(effects.is_empty());
    }

    #[test]
    fn a_flush_that_fires_mid_edit_is_deferred_not_dropped() {
        // The user queues "a","b", then recalls "b" to fix it (checked out into
        // the composer). The reply completes while they're mid-edit. The flush
        // must NOT fire (it would drop the checked-out "b" or send its stale
        // original) — the queue stays intact until the edit is finished.
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "a".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "b".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::EditQueued { index: 1 }); // check out "b"
        assert_eq!(state.editing_queued_index(), Some(1));
        let done = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "done".to_string(),
        });
        assert!(
            !done.iter().any(|e| matches!(e, Effect::SendPrompt { .. })),
            "a flush mid-edit must be deferred, not fire: {done:?}"
        );
        assert_eq!(
            state.queued_messages_for_view(),
            &["a".to_string()],
            "the rest of the queue is untouched"
        );
        assert_eq!(
            state.editing_queued_index(),
            Some(1),
            "the edit is still checked out"
        );
        // Finishing the edit (now idle) flushes the WHOLE batch, edit included.
        let flushed = state.apply(UiMessage::SubmitPrompt {
            prompt: "b fixed".to_string(),
            idempotency_key: None,
        });
        assert!(
            flushed.iter().any(|e| matches!(
                e,
                Effect::SendPrompt { prompt, .. } if prompt == "a\n\nb fixed"
            )),
            "finishing the edit flushes the whole batch as one: {flushed:?}"
        );
        assert!(state.queued_messages_for_view().is_empty());
    }

    #[test]
    fn a_flush_preserves_an_unrelated_saved_composer_draft() {
        // A backgrounded reply completes and the queue flushes on the next
        // trigger. The user had a fresh, unsent draft saved for that
        // conversation — the flush sends the QUEUE, not the draft, so the saved
        // draft must survive (regression for the switch-back draft-clobber).
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "msg1".to_string(),
            idempotency_key: None,
        });
        // The user then typed a fresh draft (the client saved it).
        state.set_composer_draft("c1", "draft2".to_string());
        let done = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "done".to_string(),
        });
        assert!(
            done.iter()
                .any(|e| matches!(e, Effect::SendPrompt { prompt, .. } if prompt == "msg1")),
            "the queue flushed: {done:?}"
        );
        assert_eq!(
            state.composer_draft("c1"),
            "draft2",
            "the flush must not clobber the unrelated saved draft"
        );
    }

    #[test]
    fn flush_pending_queue_sends_an_idle_conversations_backlog() {
        // The public entry point for a client (the TUI) whose switch seeds detail
        // directly instead of routing through ConversationLoaded.
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "x".to_string(),
            idempotency_key: None,
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "y".to_string(),
            idempotency_key: None,
        });
        // Reply completed while backgrounded → idle with a pending queue.
        state.reset_streaming_state();
        let effects = state.flush_pending_queue();
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::SendPrompt { prompt, .. } if prompt == "x\n\ny"
            )),
            "flush_pending_queue sends the backlog as one: {effects:?}"
        );
        assert!(state.queued_messages_for_view().is_empty());
        // Idempotent: nothing left to flush.
        assert!(state.flush_pending_queue().is_empty());
    }

    /// Multi-stream TUI-8: a disconnect drops EVERY conversation's in-flight
    /// stream. The open conversation gets a `[Connection lost]` stub; the
    /// backgrounded one's partial is simply discarded (it was never persisted).
    #[test]
    fn disconnect_clears_all_streams_finalizing_only_the_open_one() {
        let mut state = two_streams_state();
        let effects = state.apply(UiMessage::Disconnected {
            reason: "socket closed".to_string(),
        });
        // No stream survives anywhere.
        assert!(
            !state.is_streaming(),
            "every stream must be cleared on disconnect"
        );
        assert!(state.stream_of("c1").is_none());
        assert!(state.stream_of("c2").is_none());
        // Exactly one [Connection lost] finalization — for the open conversation.
        let completions: Vec<&str> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::CompleteStreaming(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            completions,
            vec!["c2 partial \n\n[Connection lost]"],
            "only the open conversation's partial is finalized with the marker: {effects:?}"
        );
        // The stub landed in c2's transcript; c1's truncated partial is gone.
        let last = state
            .current_conversation()
            .unwrap()
            .messages
            .last()
            .unwrap();
        assert_eq!(last.content, "c2 partial \n\n[Connection lost]");
    }

    /// Reset (TUI-8 via the explicit accessor) drops EVERY conversation's stream
    /// without finalizing, so neither can mis-claim the next post-reconnect id.
    #[test]
    fn reset_streaming_state_clears_every_conversation_stream() {
        let mut state = two_streams_state();
        state.reset_streaming_state();
        assert!(!state.is_streaming());
        assert!(state.stream_of("c1").is_none());
        assert!(state.stream_of("c2").is_none());
        // A post-reset chunk for either old id is unrouted (no stream owns it).
        let e1 = state.apply(UiMessage::StreamChunk {
            request_id: "req-c1".to_string(),
            chunk: "zombie".to_string(),
        });
        let e2 = state.apply(UiMessage::StreamChunk {
            request_id: "req-c2".to_string(),
            chunk: "zombie".to_string(),
        });
        assert!(
            e1.is_empty() && e2.is_empty(),
            "no dead stream may be revived"
        );
    }

    /// GTK-2 under concurrency: a backgrounded turn's `AssistantStatus` must NOT
    /// paint the open conversation's chat status even when the open conversation
    /// has its own `__pending__` stream — the claimed-id match routes the status
    /// to its own (background) conversation, not the viewed one.
    #[test]
    fn background_assistant_status_does_not_paint_open_pending_conversation() {
        // c1 streams (claimed) in the background; c2 is open with a __pending__
        // stream of its own (id not yet claimed).
        let mut state = WindowState::default()
            .with_stream(
                "c1",
                StreamState {
                    request_id: Some("req-c1".to_string()),
                    buffer: "c1 partial ".to_string(),
                    ..Default::default()
                },
            )
            .with_stream("c2", StreamState::default())
            .with_open(detail("c2", vec![]));
        // A status for c1's background turn arrives.
        let bg = state.apply(UiMessage::AssistantStatus {
            request_id: "req-c1".to_string(),
            message: "c1 searching...".to_string(),
        });
        assert!(
            !bg.iter().any(|e| matches!(e, Effect::SetChatStatus(_))),
            "the background turn's status must not paint c2's chat: {bg:?}"
        );
        // A status for c2's own (still-pending) turn DOES paint — it routes to c2
        // as the unique pending stream.
        let own = state.apply(UiMessage::AssistantStatus {
            request_id: "req-c2-not-yet-claimed".to_string(),
            message: "c2 searching...".to_string(),
        });
        assert!(
            own.iter()
                .any(|e| matches!(e, Effect::SetChatStatus(m) if m == "c2 searching...")),
            "the open conversation's own pending-turn status must paint: {own:?}"
        );
    }

    /// Routing by id: with two claimed streams in flight, an unrelated request id
    /// (matching neither) is ignored — it does not get mis-attributed to either.
    #[test]
    fn concurrent_chunk_for_unrelated_id_is_ignored() {
        let mut state = two_streams_state();
        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "req-nobody".to_string(),
            chunk: "noise".to_string(),
        });
        assert!(
            effects.is_empty(),
            "an unowned id must not render: {effects:?}"
        );
        assert_eq!(state.stream_of("c1").unwrap().buffer, "c1 partial ");
        assert_eq!(state.stream_of("c2").unwrap().buffer, "c2 partial ");
    }

    #[test]
    fn first_stream_chunk_claims_real_request_id_from_pending_sentinel() {
        // A __pending__ stream (id not yet claimed) for the open conversation.
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        }
        .with_stream("c1", StreamState::default());
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
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        }
        .with_stream(
            "c1",
            StreamState {
                request_id: Some("req-real".to_string()),
                buffer: "hello".to_string(),
                ..Default::default()
            },
        );
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
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        }
        .with_stream(
            "c1",
            StreamState {
                request_id: Some("req-real".to_string()),
                buffer: "hello".to_string(),
                ..Default::default()
            },
        );
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
        // __pending__ stream (id not yet claimed) for the open conversation.
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        }
        .with_stream("c1", StreamState::default());
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
        let mut state = WindowState::default()
            .with_stream(
                "c1",
                StreamState {
                    buffer: "partial".to_string(),
                    ..Default::default()
                },
            )
            .with_open(detail("c1", vec![msg("user", "hi")]));
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "the answer".to_string(),
        });
        assert!(!state.is_streaming());
        assert!(state.streaming_buffer().is_empty());
        let conv = state.current_conversation().unwrap();
        assert_eq!(conv.messages.last().unwrap().role, "assistant");
        assert_eq!(conv.messages.last().unwrap().content, "the answer");
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::ClearChatStatus,
                    Effect::CompleteStreaming(c),
                    Effect::FetchScratchpad(conv),
                    Effect::TurnFinished {
                        outcome: TurnOutcome::Completed,
                        ..
                    },
                ] if c == "the answer" && conv == "c1"
            ),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn stream_error_clears_pending_and_sets_error_status() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        }
        .with_stream(
            "c1",
            StreamState {
                request_id: Some("req-real".to_string()),
                buffer: "partial".to_string(),
                ..Default::default()
            },
        );
        let effects = state.apply(UiMessage::StreamError {
            request_id: "req-real".to_string(),
            error: "boom".to_string(),
        });
        assert!(!state.is_streaming());
        assert!(state.streaming_buffer().is_empty());
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::ClearChatStatus,
                    Effect::SetStatusText(t),
                    Effect::TurnFinished {
                        outcome: TurnOutcome::Failed(e),
                        ..
                    },
                ] if t == "Error: boom" && e == "boom"
            ),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn disconnect_finalizes_in_progress_stream_with_connection_lost_marker() {
        let mut state = WindowState::default()
            .with_stream(
                "c1",
                StreamState {
                    request_id: Some("req-real".to_string()),
                    buffer: "half a thought".to_string(),
                    ..Default::default()
                },
            )
            .with_open(detail("c1", vec![]));
        let effects = state.apply(UiMessage::Disconnected {
            reason: "socket closed".to_string(),
        });
        assert!(!state.is_streaming());
        assert!(state.streaming_buffer().is_empty());
        // The partial response is committed to the conversation with the marker.
        let last = state
            .current_conversation()
            .unwrap()
            .messages
            .last()
            .unwrap();
        assert_eq!(last.content, "half a thought\n\n[Connection lost]");
        // Effects: clear client, desensitize send, status text, finalize, then
        // report the turn the disconnect ended.
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::ClearClient,
                    Effect::SetSendSensitive(false),
                    Effect::SetStatusText(t),
                    Effect::CompleteStreaming(c),
                    Effect::TurnFinished { conversation_id, .. },
                ] if t == "Disconnected: socket closed"
                    && c == "half a thought\n\n[Connection lost]"
                    && conversation_id == "c1"
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
            ..Default::default()
        }
        .with_open(detail("c1", vec![msg("user", "hi")]));
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
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
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
        assert!(state.current_conversation().is_some());
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
            ..Default::default()
        }
        .with_open(detail("c1", vec![msg("user", "hi")]));
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
                .current_conversation()
                .is_some_and(|c| c.messages.len() == 1),
            "the open conversation's cached detail must be preserved verbatim"
        );
    }

    #[test]
    fn deleting_active_conversation_clears_chat_and_re_ensures_active() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "one", false), summary("c2", "two", false)],
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        let effects = state.apply(UiMessage::ConversationDeleted {
            id: "c1".to_string(),
        });
        assert_eq!(state.conversations.len(), 1);
        assert_eq!(state.conversations[0].id, "c2");
        assert!(state.current_conversation_id.is_none());
        assert!(state.current_conversation().is_none());
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
            ..Default::default()
        }
        .with_open(detail("c2", vec![]));
        // Both conversations carry voice settings.
        {
            let m = state.open.entry("c1".to_string()).or_default();
            m.voice_in = true;
            m.adele_output = AdeleOutput::Always;
        }
        {
            let m = state.open.entry("c2".to_string()).or_default();
            m.voice_in = true;
            m.adele_output = AdeleOutput::OnDemand;
        }

        // Delete the inactive one.
        state.apply(UiMessage::ConversationDeleted {
            id: "c1".to_string(),
        });
        assert!(
            !state.open.contains_key("c1"),
            "the deleted conversation's model (You:/Adele:) must be pruned"
        );
        // The surviving conversation's settings are untouched.
        assert!(state.voice_in_for("c2"));
        assert_eq!(state.adele_output_for("c2"), AdeleOutput::OnDemand);

        // Deleting the active one prunes it too.
        state.apply(UiMessage::ConversationDeleted {
            id: "c2".to_string(),
        });
        assert!(state.open.is_empty());
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
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
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
        assert_eq!(state.current_conversation().unwrap().messages.len(), 4);
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
                Effect::SetQueuedMessages {
                    messages,
                    editing: None,
                },
            ] => {
                let roles: Vec<&str> = filtered.messages.iter().map(|m| m.role.as_str()).collect();
                assert_eq!(roles, vec!["user", "assistant"]);
                assert_eq!(filtered.messages[1].content, "answer");
                assert!(
                    messages.is_empty(),
                    "a freshly-loaded conversation has an empty queue"
                );
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
                Effect::SetQueuedMessages { editing: None, .. },
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
                Effect::SetQueuedMessages { editing: None, .. },
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
            ..Default::default()
        }
        .with_open(conv);
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
            ..Default::default()
        }
        .with_open(detail("c1", vec![msg("user", "earlier")]));
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
            state.current_conversation().is_some(),
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
            ..Default::default()
        }
        .with_open(conv);
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
                .current_conversation()
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
            ..Default::default()
        }
        .with_open(conv);
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
                .current_conversation()
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
        let model = state.open.entry("c1".to_string()).or_default();
        model.voice_in = voice_in;
        model.adele_output = adele;
        state
    }

    /// A `StreamComplete` for `c1` carrying `full_response`, against a freshly
    /// pinned pending request — the reply-narration trigger.
    fn stream_complete_in(state: &mut WindowState, full_response: &str) -> Vec<Effect> {
        state.open.entry("c1".to_string()).or_default().stream = Some(StreamState {
            request_id: Some("req".to_string()),
            ..Default::default()
        });
        state.cache_detail(detail("c1", vec![]));
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

    /// Default: a `say_this` produces the `SpeechDisabled` downgrade line (clean
    /// content, the marker is the executor's job), NO `Speak`, and ALWAYS a
    /// `SubmitClientToolResult` (the turn completes, can't hang).
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
        let inline = effects.iter().any(|e| {
            matches!(
                e,
                Effect::AddLocalMessage { content, kind: MessageKind::SpeechDisabled }
                    if content == "the aside"
            )
        });
        assert!(
            inline,
            "expected the SpeechDisabled downgrade line: {effects:?}"
        );
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
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        let effects = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "voice-req".to_string(),
            content: "what's the weather?".to_string(),
            idempotency_key: None,
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
            state.current_conversation().unwrap().messages.len(),
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
            idempotency_key: None,
        });
        let effects = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "real-req".to_string(),
            content: "typed this".to_string(),
            idempotency_key: None,
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
        state.cache_detail(detail("c1", vec![]));
        state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "voice-req".to_string(),
            content: "a question".to_string(),
            idempotency_key: None,
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
            ..Default::default()
        }
        .with_open(detail("c1", vec![]));
        let effects = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c2".to_string(),
            request_id: "bg-req".to_string(),
            content: "background".to_string(),
            idempotency_key: None,
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
        let mut state = WindowState::default()
            .with_open(detail("c1", vec![]))
            .with_stream(
                "c1",
                StreamState {
                    request_id: Some("mine".to_string()),
                    ..Default::default()
                },
            );
        let effects = state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "other".to_string(),
            content: "concurrent".to_string(),
            idempotency_key: None,
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

    /// Adele=Always: a `say_this` aside is NOT separately spoken (voice#126 —
    /// Always already narrates the whole reply; say_this is on-demand's channel).
    /// It downgrades to a shown SpeechDisabled line so nothing is lost.
    #[test]
    fn adele_always_does_not_speak_say_this_aside() {
        let mut state = state_with(false, AdeleOutput::Always);
        let effects = state.apply(say_this_call("c1", "hello aloud"));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "Always must not separately speak a say_this aside: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::AddLocalMessage { content, kind: MessageKind::SpeechDisabled }
                    if content == "hello aloud"
            )),
            "the aside downgrades to a shown line: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SubmitClientToolResult { result: Ok(_), .. })),
            "still resolves a result: {effects:?}"
        );
    }

    /// No double-speak: in on-demand mode the model voices an aside via
    /// `say_this`, and the `StreamComplete` reply narration stays silent — the
    /// user hears the aside once, not the aside AND the whole reply read aloud.
    /// (On-demand never auto-narrates, so suppression is automatic; the
    /// `say_this_spoken_this_turn` backstop covers a future mode change.)
    #[test]
    fn say_this_aside_suppresses_duplicate_reply_narration() {
        let mut state = state_with(false, AdeleOutput::OnDemand);
        // Simulate an in-flight turn for the open conversation.
        state.open.entry("c1".to_string()).or_default().stream = Some(StreamState {
            request_id: Some("req".to_string()),
            ..Default::default()
        });
        state.cache_detail(detail("c1", vec![]));

        // The model speaks an aside mid-turn.
        let aside = state.apply(say_this_call("c1", "the spoken answer"));
        assert!(
            aside
                .iter()
                .any(|e| matches!(e, Effect::Speak(t) if t == "the spoken answer")),
            "the say_this aside should be spoken: {aside:?}"
        );

        // The turn completes: the full reply must NOT be read aloud.
        let done = state.apply(UiMessage::StreamComplete {
            request_id: "req".to_string(),
            full_response: "the spoken answer, in more words".to_string(),
        });
        assert!(
            !done.iter().any(|e| matches!(e, Effect::Speak(_))),
            "on-demand must not narrate the full reply (double-speak): {done:?}"
        );
        assert!(
            done.iter()
                .any(|e| matches!(e, Effect::CompleteStreaming(_))),
            "the reply is still finalized in the chat: {done:?}"
        );
    }

    /// Adele=OnDemand never auto-narrates the reply — its spoken channel is
    /// `say_this` (voice#126). Decoupled from `You`: neither value narrates.
    /// The reply text is still finalized.
    #[test]
    fn adele_on_demand_does_not_auto_narrate_reply() {
        for voice_in in [false, true] {
            let mut state = state_with(voice_in, AdeleOutput::OnDemand);
            assert!(
                !state.narrate_for_current(),
                "OnDemand must not auto-narrate (You={voice_in})"
            );
            let effects = stream_complete_in(&mut state, "an answer");
            assert!(
                !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
                "OnDemand must not speak the reply (You={voice_in}): {effects:?}"
            );
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::CompleteStreaming(t) if t == "an answer")),
                "the reply text must still be finalized (You={voice_in}): {effects:?}"
            );
        }
    }

    /// Adele=OnDemand: a `say_this` aside speaks (its sole spoken channel) and
    /// also shows in the transcript tagged `Spoken` (voice#126). Independent of
    /// `You`.
    #[test]
    fn adele_on_demand_say_this_speaks_and_shows_spoken() {
        for voice_in in [false, true] {
            let mut state = state_with(voice_in, AdeleOutput::OnDemand);
            let effects = state.apply(say_this_call("c1", "an aside"));
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::Speak(t) if t == "an aside")),
                "OnDemand say_this must speak (You={voice_in}): {effects:?}"
            );
            assert!(
                effects.iter().any(|e| matches!(
                    e,
                    Effect::AddLocalMessage { content, kind: MessageKind::Spoken }
                        if content == "an aside"
                )),
                "the spoken line must show tagged Spoken (You={voice_in}): {effects:?}"
            );
            assert!(
                !effects.iter().any(|e| matches!(
                    e,
                    Effect::AddLocalMessage {
                        kind: MessageKind::SpeechDisabled,
                        ..
                    }
                )),
                "no SpeechDisabled downgrade when spoken (You={voice_in}): {effects:?}"
            );
        }
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

    /// GTK-3 acceptance: a turn whose conversation narrates (`Adele == Always`)
    /// produces EXACTLY ONE `Speak` effect — the reducer is the single narration
    /// source, so there is no double-speak.
    #[test]
    fn narrating_conversation_dictated_turn_emits_exactly_one_speak() {
        let mut state = state_with(false, AdeleOutput::Always);
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
        // The call's conversation is on-demand (its own spoken channel) — but it
        // isn't in view, so the aside still can't play here.
        state.open.entry("c2".to_string()).or_default().adele_output = AdeleOutput::OnDemand;
        let effects = state.apply(say_this_call("c2", "background aside"));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "a background conversation's say_this must never play audio: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::AddLocalMessage { content, kind: MessageKind::SpeechDisabled }
                    if content.contains("background aside")
            )),
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
        let mut state = state_with(false, AdeleOutput::OnDemand); // active c1, gate open
        let effects = state.apply(say_this_call("c2", "should not play"));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "c2's aside must not play under c1's gate: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::AddLocalMessage {
                    kind: MessageKind::SpeechDisabled,
                    ..
                }
            )),
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
        state.open.entry("c9".to_string()).or_default().adele_output = AdeleOutput::OnDemand; // unrelated
        let effects = state.apply(say_this_call("c1", "quiet aside"));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "c1 is Disabled; no audio: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::AddLocalMessage { content, kind: MessageKind::SpeechDisabled }
                    if content == "quiet aside"
            )),
            "expected the SpeechDisabled downgrade line: {effects:?}"
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
            state.adele_output_for("c2"),
            AdeleOutput::OnDemand,
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
        state.open.entry("c2".to_string()).or_default().adele_output = AdeleOutput::Always;
        let effects = state.apply(voice_tool_call("c2", "stop_voice"));
        assert_eq!(
            state.adele_output_for("c2"),
            AdeleOutput::Disabled,
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
            // No `_` here: the on-demand refinement names the `say_this` tool,
            // whose identifier legitimately contains an underscore. The real
            // formatting risks are emphasis/code/heading markers.
            for marker in ['*', '`', '#'] {
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

    // --- Turn-completion correlation (#51) --------------------------------
    //
    // A host that opens a per-turn span at submit must be able to close it at
    // the reply's end. It therefore needs two things the reducer did not give
    // it: a terminal event for EVERY routed stream (a backgrounded completion
    // used to return zero effects, so a host watching effects could not see it
    // at all), and enough on that event to name the submit it closes.

    /// Read the single `TurnFinished` out of a returned effect list, or `None`
    /// when there is none. Panics if a turn reports finished twice.
    fn turn_finished(effects: &[Effect]) -> Option<(&str, &str, Option<&str>, &TurnOutcome)> {
        let mut it = effects.iter().filter_map(|e| match e {
            Effect::TurnFinished {
                conversation_id,
                request_id,
                idempotency_key,
                outcome,
            } => Some((
                conversation_id.as_str(),
                request_id.as_str(),
                idempotency_key.as_deref(),
                outcome,
            )),
            _ => None,
        });
        let first = it.next();
        assert!(
            it.next().is_none(),
            "a single terminal event must report exactly one finished turn"
        );
        first
    }

    /// Send into `c1`, ack it, claim the daemon id, then switch the view to
    /// `c2`, so `c1`'s turn is still streaming, in the background, with the
    /// client-minted key `k` behind it.
    fn backgrounded_turn(key: Option<&str>) -> WindowState {
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: "how long did that take".to_string(),
            idempotency_key: key.map(str::to_string),
        });
        state.apply(UiMessage::PromptSent {
            task_id: "task-1".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: key.map(str::to_string),
        });
        state.apply(UiMessage::StreamChunk {
            request_id: "req-c1".to_string(),
            chunk: "thinking".to_string(),
        });
        state.apply(UiMessage::ConversationLoaded(detail("c2", vec![])));
        assert!(
            state.stream_of("c1").is_some(),
            "precondition: c1 still streams after the switch"
        );
        assert!(
            !state.is_active_conversation("c1"),
            "precondition: c1 is backgrounded"
        );
        state
    }

    /// The case nobody could observe: the person sends in one conversation,
    /// switches away, and the reply finishes while they are somewhere else. The
    /// completion must still reach the host, naming the conversation and the
    /// submit it closes.
    #[test]
    fn a_backgrounded_completion_reports_its_conversation_and_key() {
        let mut state = backgrounded_turn(Some("submit-key"));
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-c1".to_string(),
            full_response: "the answer".to_string(),
        });
        assert_eq!(
            turn_finished(&effects),
            Some(("c1", "req-c1", Some("submit-key"), &TurnOutcome::Completed)),
            "a backgrounded completion must report the finished turn: {effects:?}"
        );
    }

    /// The same for the failure path. A span closes on a failed turn too, or
    /// it never closes at all.
    #[test]
    fn a_backgrounded_error_reports_its_conversation_and_key() {
        let mut state = backgrounded_turn(Some("submit-key"));
        let effects = state.apply(UiMessage::StreamError {
            request_id: "req-c1".to_string(),
            error: "the provider timed out".to_string(),
        });
        assert_eq!(
            turn_finished(&effects),
            Some((
                "c1",
                "req-c1",
                Some("submit-key"),
                &TurnOutcome::Failed("the provider timed out".to_string())
            )),
            "a backgrounded error must report the finished turn: {effects:?}"
        );
    }

    /// The in-view path reports the same thing, so a host has one rule rather
    /// than two.
    #[test]
    fn an_in_view_completion_reports_the_finished_turn() {
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: "hello".to_string(),
            idempotency_key: Some("submit-key".to_string()),
        });
        state.apply(UiMessage::PromptSent {
            task_id: "task-1".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: Some("submit-key".to_string()),
        });
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-c1".to_string(),
            full_response: "the answer".to_string(),
        });
        assert_eq!(
            turn_finished(&effects),
            Some(("c1", "req-c1", Some("submit-key"), &TurnOutcome::Completed)),
            "an in-view completion must report the finished turn: {effects:?}"
        );
    }

    #[test]
    fn an_in_view_error_reports_the_finished_turn_as_failed() {
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: "hello".to_string(),
            idempotency_key: Some("submit-key".to_string()),
        });
        state.apply(UiMessage::PromptSent {
            task_id: "task-1".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: Some("submit-key".to_string()),
        });
        let effects = state.apply(UiMessage::StreamError {
            request_id: "req-c1".to_string(),
            error: "boom".to_string(),
        });
        assert_eq!(
            turn_finished(&effects),
            Some((
                "c1",
                "req-c1",
                Some("submit-key"),
                &TurnOutcome::Failed("boom".to_string())
            )),
            "an in-view error must report the finished turn: {effects:?}"
        );
    }

    /// A turn this client never sent (a voice turn, or another client) carries
    /// no key, so a host is told plainly that it holds no span for it.
    #[test]
    fn an_adopted_external_turn_reports_no_key() {
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.apply(UiMessage::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "req-ext".to_string(),
            content: "asked by voice".to_string(),
            idempotency_key: None,
        });
        assert!(
            state.stream_external(),
            "precondition: the turn was adopted"
        );
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-ext".to_string(),
            full_response: "the answer".to_string(),
        });
        assert_eq!(
            turn_finished(&effects),
            Some(("c1", "req-ext", None, &TurnOutcome::Completed)),
            "an adopted external turn reports its conversation but no key: {effects:?}"
        );
    }

    /// A keyless send (a host that mints no key) still reports its
    /// conversation, so correlation degrades rather than disappearing.
    #[test]
    fn a_keyless_send_still_reports_its_conversation() {
        let mut state = backgrounded_turn(None);
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-c1".to_string(),
            full_response: "the answer".to_string(),
        });
        assert_eq!(
            turn_finished(&effects),
            Some(("c1", "req-c1", None, &TurnOutcome::Completed)),
            "a keyless turn still names its conversation: {effects:?}"
        );
    }

    /// The completion of one turn must reach the host before the queue flush
    /// starts the next one, or a host nests the new turn inside the old span.
    #[test]
    fn a_finished_turn_is_reported_before_the_queue_flushs_next_send() {
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "and another thing".to_string(),
            idempotency_key: Some("queued-key".to_string()),
        });
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "the answer".to_string(),
        });
        let finished = effects
            .iter()
            .position(|e| matches!(e, Effect::TurnFinished { .. }))
            .expect("the completed turn must be reported");
        let next_send = effects
            .iter()
            .position(|e| matches!(e, Effect::SendPrompt { .. }))
            .expect("the queued follow-up must flush");
        assert!(
            finished < next_send,
            "the finished turn must be reported before the next send starts: {effects:?}"
        );
    }

    /// Several sends queued mid-stream flush as ONE turn that adopts the first
    /// queued key. The finished turn reports that same key, so the host closes
    /// the span the fold actually kept.
    #[test]
    fn a_flushed_turn_reports_the_key_the_flush_adopted() {
        let mut state = mid_stream_state("c1", "c1");
        state.apply(UiMessage::SubmitPrompt {
            prompt: "a".to_string(),
            idempotency_key: Some("ka".to_string()),
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "b".to_string(),
            idempotency_key: Some("kb".to_string()),
        });
        // Finish the first turn: the queue flushes as one combined send.
        state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "first answer".to_string(),
        });
        state.apply(UiMessage::PromptSent {
            task_id: "task-2".to_string(),
            conversation_id: "c1".to_string(),
            // The executor echoes the key it was handed on the flush's
            // `SendPrompt`, which is the first queued message's.
            idempotency_key: Some("ka".to_string()),
        });
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-flush".to_string(),
            full_response: "second answer".to_string(),
        });
        assert_eq!(
            turn_finished(&effects),
            Some(("c1", "req-flush", Some("ka"), &TurnOutcome::Completed)),
            "the flushed turn reports the key it adopted: {effects:?}"
        );
    }

    /// A completion for a stream the reducer does not own reports nothing. The
    /// reducer's own view is what a host acts on, so a stray daemon id must not
    /// close a span.
    #[test]
    fn an_unrouted_completion_reports_no_finished_turn() {
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-nobody-owns".to_string(),
            full_response: "the answer".to_string(),
        });
        assert_eq!(
            turn_finished(&effects),
            None,
            "an unrouted completion must report no finished turn: {effects:?}"
        );
    }

    /// A send that never reached the daemon leaves no key behind, so the next
    /// turn in the same conversation is not reported under the failed send's
    /// key.
    #[test]
    fn a_failed_send_does_not_leak_its_key_onto_the_next_turn() {
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: "lost".to_string(),
            idempotency_key: Some("dead-key".to_string()),
        });
        state.apply(UiMessage::SendFailed {
            conversation_id: "c1".to_string(),
            prompt: "lost".to_string(),
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "retry".to_string(),
            idempotency_key: Some("live-key".to_string()),
        });
        state.apply(UiMessage::PromptSent {
            task_id: "task-2".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: Some("live-key".to_string()),
        });
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-c1".to_string(),
            full_response: "the answer".to_string(),
        });
        assert_eq!(
            turn_finished(&effects),
            Some(("c1", "req-c1", Some("live-key"), &TurnOutcome::Completed)),
            "the finished turn must report the send that reached the daemon: {effects:?}"
        );
    }

    // --- Turns that end without a reply (#51) -----------------------------
    //
    // A completion and an error are not the only ways a turn ends. Teardown
    // drops every in-flight stream, and a turn dropped without a report leaves
    // a host span open for the life of the process.

    /// Every `TurnFinished` in an effect list, sorted by conversation, as
    /// (conversation, request id, key, outcome).
    fn turns_finished(effects: &[Effect]) -> Vec<(&str, &str, Option<&str>, &TurnOutcome)> {
        let mut found: Vec<_> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::TurnFinished {
                    conversation_id,
                    request_id,
                    idempotency_key,
                    outcome,
                } => Some((
                    conversation_id.as_str(),
                    request_id.as_str(),
                    idempotency_key.as_deref(),
                    outcome,
                )),
                _ => None,
            })
            .collect();
        found.sort_by_key(|(id, ..)| *id);
        found
    }

    /// A disconnect ends every turn in flight, in view and backgrounded alike.
    #[test]
    fn a_disconnect_reports_every_turn_it_ends() {
        let mut state = two_streams_state();
        let effects = state.apply(UiMessage::Disconnected {
            reason: "socket closed".to_string(),
        });
        let lost = TurnOutcome::Failed("Disconnected: socket closed".to_string());
        assert_eq!(
            turns_finished(&effects),
            vec![("c1", "req-c1", None, &lost), ("c2", "req-c2", None, &lost),],
            "a disconnect must report both the backgrounded turn and the one in view: {effects:?}"
        );
    }

    /// A turn torn down inside the `__pending__` window has no daemon id yet.
    /// It still reports, keyed by the send, so the host closes the right span.
    #[test]
    fn a_disconnect_reports_a_turn_whose_daemon_id_never_arrived() {
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: "hello".to_string(),
            idempotency_key: Some("submit-key".to_string()),
        });
        state.apply(UiMessage::PromptSent {
            task_id: "task-1".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: Some("submit-key".to_string()),
        });
        assert!(
            state.stream_unclaimed(),
            "precondition: the daemon id has not arrived yet"
        );
        let effects = state.apply(UiMessage::Disconnected {
            reason: "socket closed".to_string(),
        });
        let lost = TurnOutcome::Failed("Disconnected: socket closed".to_string());
        assert_eq!(
            turns_finished(&effects),
            vec![("c1", "", Some("submit-key"), &lost)],
            "an unclaimed turn reports an empty request id and its send key: {effects:?}"
        );
    }

    /// The TUI drives its own reconnect and resets the reducer's streaming
    /// state directly. That ends turns too, so it hands back the reports.
    #[test]
    fn resetting_streaming_state_reports_every_turn_it_ends() {
        let mut state = two_streams_state();
        let effects = state.reset_streaming_state();
        let lost = TurnOutcome::Failed("Streaming state reset".to_string());
        assert_eq!(
            turns_finished(&effects),
            vec![("c1", "req-c1", None, &lost), ("c2", "req-c2", None, &lost),],
            "a reset must report every turn it drops: {effects:?}"
        );
        assert!(!state.is_streaming(), "and it still drops them");
    }

    // --- Turns an ack replaces (#51) --------------------------------------
    //
    // A send leaves before the previous one is acked, so two acks arrive for
    // one conversation. The second replaces the first turn's stream. The turn
    // itself is lost, which is #53 and pre-existing, but its report must not
    // be: a host holds a span for it, and a span that never closes is a leak
    // while a span closed under another turn's key is a wrong number.

    /// The ack that replaces a turn reports the turn it replaced.
    #[test]
    fn an_ack_that_replaces_a_turn_reports_the_one_it_replaced() {
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.apply(UiMessage::PromptSent {
            task_id: "task-a".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: Some("key-a".to_string()),
        });
        state.apply(UiMessage::StreamChunk {
            request_id: "req-a".to_string(),
            chunk: "partial".to_string(),
        });
        let effects = state.apply(UiMessage::PromptSent {
            task_id: "task-b".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: Some("key-b".to_string()),
        });
        assert_eq!(
            turn_finished(&effects),
            Some((
                "c1",
                "req-a",
                Some("key-a"),
                &TurnOutcome::Failed("Replaced by a later send".to_string())
            )),
            "the replaced turn must be reported, under its OWN key: {effects:?}"
        );
    }

    /// Two sends that overlap close two spans, each under the key of the send
    /// that opened it. Neither host span is left open, and neither closes under
    /// the other's key.
    #[test]
    fn two_overlapping_sends_report_their_own_keys() {
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        // Both sends leave before either is acked: the reducer's send gate keys
        // off the stream, and no stream exists until an ack arrives.
        state.apply(UiMessage::SubmitPrompt {
            prompt: "first".to_string(),
            idempotency_key: Some("key-a".to_string()),
        });
        state.apply(UiMessage::SubmitPrompt {
            prompt: "second".to_string(),
            idempotency_key: Some("key-b".to_string()),
        });
        let first_ack = state.apply(UiMessage::PromptSent {
            task_id: "task-a".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: Some("key-a".to_string()),
        });
        assert_eq!(
            turn_finished(&first_ack),
            None,
            "the first ack replaces nothing: {first_ack:?}"
        );
        let second_ack = state.apply(UiMessage::PromptSent {
            task_id: "task-b".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: Some("key-b".to_string()),
        });
        assert_eq!(
            turn_finished(&second_ack).map(|(_, _, key, _)| key),
            Some(Some("key-a")),
            "the replaced turn closes under key-a, not key-b: {second_ack:?}"
        );
        let done = state.apply(UiMessage::StreamComplete {
            request_id: "req-b".to_string(),
            full_response: "the answer".to_string(),
        });
        assert_eq!(
            turn_finished(&done),
            Some(("c1", "req-b", Some("key-b"), &TurnOutcome::Completed)),
            "the surviving turn closes under key-b: {done:?}"
        );
    }

    /// The ack carries the key, so the reducer never guesses which send it
    /// belongs to. An ack whose send this client did not key reports no key
    /// rather than borrowing one from another send in flight.
    #[test]
    fn an_ack_without_a_key_does_not_borrow_one() {
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        state.apply(UiMessage::SubmitPrompt {
            prompt: "keyed".to_string(),
            idempotency_key: Some("key-a".to_string()),
        });
        state.apply(UiMessage::PromptSent {
            task_id: "task-x".to_string(),
            conversation_id: "c1".to_string(),
            idempotency_key: None,
        });
        let done = state.apply(UiMessage::StreamComplete {
            request_id: "req-x".to_string(),
            full_response: "the answer".to_string(),
        });
        assert_eq!(
            turn_finished(&done),
            Some(("c1", "req-x", None, &TurnOutcome::Completed)),
            "a keyless ack must not pick up another send's key: {done:?}"
        );
    }

    /// Teardown with nothing in flight reports nothing.
    #[test]
    fn a_disconnect_with_no_turn_in_flight_reports_none() {
        let mut state = WindowState::default().with_open(detail("c1", vec![]));
        let effects = state.apply(UiMessage::Disconnected {
            reason: "socket closed".to_string(),
        });
        assert_eq!(
            turns_finished(&effects),
            vec![],
            "no turn was in flight, so none finished: {effects:?}"
        );
    }
}
