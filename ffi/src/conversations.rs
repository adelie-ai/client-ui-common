//! The conversation inventory: which population this core reads, and how it
//! moves a conversation into or out of the archive.
//!
//! Why the two live together, off the transport: an (un)archive is only
//! finished when the refreshed inventory is on its way to the view, and the
//! refreshed inventory is only useful if it contains the archived rows. Both
//! answers must be the same at every call site, and both must be testable
//! without a daemon.

use client_ui_common::UiMessage;
use desktop_assistant_api_model::client::ConversationSummary;
use desktop_assistant_client_common::AssistantClient;

/// What a transport round-trip failed with.
///
/// An error object rather than a `String` so the cause stays reachable through
/// `source()`. The one caller today renders only the outermost message, which is
/// what the other conversation RPCs in the executor report.
type TransportError = Box<dyn std::error::Error + Send + Sync>;

/// Which way an (un)archive moves a conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveChange {
    /// Put the conversation away.
    Archive,
    /// Bring it back out.
    Unarchive,
}

impl ArchiveChange {
    /// The word this change is reported by when it fails.
    fn verb(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Unarchive => "unarchive",
        }
    }

    /// The same word as it reads in a report of what did not happen.
    fn past(self) -> &'static str {
        match self {
            Self::Archive => "archived",
            Self::Unarchive => "unarchived",
        }
    }
}

/// The conversation-inventory calls this core makes on a transport.
///
/// Why a trait of its own over [`AssistantClient`], which already has all four
/// underlying calls: the transport offers two list calls, and which population
/// this core reads is one answer, written once here. The trait states the
/// answer; what holds the executor to it is
/// `the_executor_never_reads_the_active_only_list`, since a blanket impl cannot
/// hide the narrower call from a call site that still has it in scope.
pub(crate) trait ConversationInventory {
    /// Every conversation the account holds, archived rows included, in the
    /// daemon's order.
    ///
    /// Why the wider population: each row carries `archived`, so a client can
    /// group or hide them, and a client that hides them cannot do so from a list
    /// that never contained them.
    async fn list_all(&self) -> Result<Vec<ConversationSummary>, TransportError>;

    /// Move the conversation `id` into or out of the archive.
    async fn set_archived(&self, id: &str, change: ArchiveChange) -> Result<(), TransportError>;
}

impl<T: AssistantClient + ?Sized> ConversationInventory for T {
    async fn list_all(&self) -> Result<Vec<ConversationSummary>, TransportError> {
        self.list_conversations_with_archived()
            .await
            .map_err(Into::into)
    }

    async fn set_archived(&self, id: &str, change: ArchiveChange) -> Result<(), TransportError> {
        match change {
            ArchiveChange::Archive => self.archive_conversation(id).await,
            ArchiveChange::Unarchive => self.unarchive_conversation(id).await,
        }
        .map_err(Into::into)
    }
}

/// Perform an (un)archive and re-read the inventory, returning the messages the
/// actor delivers, in order.
///
/// Why the re-read belongs here: the daemon answers an (un)archive with an
/// acknowledgement and nothing else, so a client that does not re-list keeps
/// showing the conversation exactly as it was.
///
/// The refresh travels as [`UiMessage::ConversationListRefetched`], which
/// repaints the sidebar only: the open transcript and the model picker are not
/// this change's business and must survive it.
pub(crate) async fn archive_and_relist<C>(
    client: &C,
    id: &str,
    change: ArchiveChange,
) -> Vec<UiMessage>
where
    C: ConversationInventory + ?Sized,
{
    if let Err(e) = client.set_archived(id, change).await {
        return vec![UiMessage::Error(format!(
            "{} conversation: {e}",
            change.verb()
        ))];
    }
    match client.list_all().await {
        Ok(convs) => vec![UiMessage::ConversationListRefetched(convs)],
        Err(e) => vec![UiMessage::Error(format!("load conversations: {e}"))],
    }
}

/// The report for an (un)archive that was never attempted, because the core has
/// no connection.
///
/// Why it is reported rather than dropped: the change did not happen, and a
/// person who clicked Archive and was told nothing has no way to tell that from
/// a change that landed. The list on screen is already correct, so nothing
/// repaints.
pub(crate) fn disconnected_report(change: ArchiveChange) -> UiMessage {
    UiMessage::Error(format!(
        "Not connected - conversation not {}",
        change.past()
    ))
}

/// Whether the conversation the view already has open stays open.
///
/// It does whenever the inventory still holds it, archived or not. A
/// conversation archived while someone is reading it, by this client or by
/// another, must not be closed out from under the reader. Only the conversation
/// this core opens *for* a reader passes over archived rows, in
/// [`auto_open_target`].
pub(crate) fn keeps_open(conversations: &[ConversationSummary], current: Option<&str>) -> bool {
    current.is_some_and(|id| conversations.iter().any(|c| c.id == id))
}

/// The conversation to open when the view has none: the most recent one that is
/// not archived.
///
/// Why not simply the first row: the inventory carries archived conversations
/// too, and opening one of those would put back on screen what the user filed
/// away. `None` means there is nothing to open, and the caller creates a
/// conversation - what it already did for an empty list.
pub(crate) fn auto_open_target(
    conversations: &[ConversationSummary],
) -> Option<&ConversationSummary> {
    conversations.iter().find(|c| !c.archived)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use async_trait::async_trait;
    use desktop_assistant_api_model as api;
    use desktop_assistant_client_common::ConversationDetail;

    fn summary(id: &str, archived: bool) -> ConversationSummary {
        ConversationSummary {
            id: id.to_string(),
            title: format!("Conversation {id}"),
            message_count: 3,
            archived,
        }
    }

    /// What the transport was asked to do, in the order it was asked.
    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Archive(String),
        Unarchive(String),
        /// `list_conversations` - the active-only population.
        ListActive,
        /// `list_conversations_with_archived` - the whole population.
        ListAll,
    }

    /// A daemon stand-in that answers the four conversation-inventory calls and
    /// records each one.
    ///
    /// It models the daemon's own split: `list_conversations` answers with the
    /// not-archived rows, `list_conversations_with_archived` with all of them.
    /// So a test that finds an archived row in the result has proven which call
    /// was made.
    struct FakeDaemon {
        calls: Mutex<Vec<Call>>,
        conversations: Vec<ConversationSummary>,
        archive_error: Option<&'static str>,
        list_error: Option<&'static str>,
    }

    impl FakeDaemon {
        fn holding(conversations: Vec<ConversationSummary>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                conversations,
                archive_error: None,
                list_error: None,
            }
        }

        fn failing_to_archive(error: &'static str) -> Self {
            Self {
                archive_error: Some(error),
                ..Self::holding(vec![summary("c1", false)])
            }
        }

        fn failing_to_list(error: &'static str) -> Self {
            Self {
                list_error: Some(error),
                ..Self::holding(vec![summary("c1", false)])
            }
        }

        fn record(&self, call: Call) {
            self.calls
                .lock()
                .expect("fake daemon call log is never poisoned")
                .push(call);
        }

        fn calls(&self) -> std::sync::MutexGuard<'_, Vec<Call>> {
            self.calls
                .lock()
                .expect("fake daemon call log is never poisoned")
        }
    }

    #[async_trait]
    impl AssistantClient for FakeDaemon {
        async fn list_conversations(&self) -> anyhow::Result<Vec<ConversationSummary>> {
            self.record(Call::ListActive);
            if let Some(e) = self.list_error {
                return Err(anyhow::anyhow!(e));
            }
            Ok(self
                .conversations
                .iter()
                .filter(|c| !c.archived)
                .cloned()
                .collect())
        }

        async fn list_conversations_with_archived(
            &self,
        ) -> anyhow::Result<Vec<ConversationSummary>> {
            self.record(Call::ListAll);
            if let Some(e) = self.list_error {
                return Err(anyhow::anyhow!(e));
            }
            Ok(self.conversations.clone())
        }

        async fn archive_conversation(&self, id: &str) -> anyhow::Result<()> {
            self.record(Call::Archive(id.to_string()));
            match self.archive_error {
                Some(e) => Err(anyhow::anyhow!(e)),
                None => Ok(()),
            }
        }

        async fn unarchive_conversation(&self, id: &str) -> anyhow::Result<()> {
            self.record(Call::Unarchive(id.to_string()));
            match self.archive_error {
                Some(e) => Err(anyhow::anyhow!(e)),
                None => Ok(()),
            }
        }

        // --- not exercised by the conversation inventory ---------------------

        async fn get_conversation(&self, _id: &str) -> anyhow::Result<ConversationDetail> {
            unimplemented!("the inventory never fetches a conversation")
        }

        async fn get_messages(
            &self,
            _conversation_id: &str,
            _tail: i32,
            _after_count: i32,
            _include_roles: Vec<String>,
        ) -> anyhow::Result<api::MessagesView> {
            unimplemented!("the inventory never fetches messages")
        }

        async fn create_conversation(&self, _title: &str) -> anyhow::Result<String> {
            unimplemented!("the inventory never creates a conversation")
        }

        async fn delete_conversation(&self, _id: &str) -> anyhow::Result<()> {
            unimplemented!("the inventory never deletes a conversation")
        }

        async fn rename_conversation(&self, _id: &str, _title: &str) -> anyhow::Result<()> {
            unimplemented!("the inventory never renames a conversation")
        }

        async fn send_prompt(
            &self,
            _conversation_id: &str,
            _prompt: &str,
        ) -> anyhow::Result<String> {
            unimplemented!("the inventory never sends a prompt")
        }

        async fn list_knowledge_entries(
            &self,
            _limit: u32,
            _offset: u32,
            _tag_filter: Option<Vec<String>>,
        ) -> anyhow::Result<Vec<api::KnowledgeEntryView>> {
            unimplemented!("the inventory never touches knowledge")
        }

        async fn get_knowledge_entry(
            &self,
            _id: &str,
        ) -> anyhow::Result<Option<api::KnowledgeEntryView>> {
            unimplemented!("the inventory never touches knowledge")
        }

        async fn search_knowledge_entries(
            &self,
            _query: &str,
            _tag_filter: Option<Vec<String>>,
            _limit: u32,
        ) -> anyhow::Result<Vec<api::KnowledgeEntryView>> {
            unimplemented!("the inventory never touches knowledge")
        }

        async fn create_knowledge_entry(
            &self,
            _content: &str,
            _tags: Vec<String>,
            _metadata: serde_json::Value,
        ) -> anyhow::Result<api::KnowledgeEntryView> {
            unimplemented!("the inventory never touches knowledge")
        }

        async fn update_knowledge_entry(
            &self,
            _id: &str,
            _content: &str,
            _tags: Vec<String>,
            _metadata: serde_json::Value,
        ) -> anyhow::Result<api::KnowledgeEntryView> {
            unimplemented!("the inventory never touches knowledge")
        }

        async fn delete_knowledge_entry(&self, _id: &str) -> anyhow::Result<()> {
            unimplemented!("the inventory never touches knowledge")
        }

        async fn start_knowledge_maintenance(
            &self,
            _op: api::MaintenanceOp,
        ) -> anyhow::Result<String> {
            unimplemented!("the inventory never touches knowledge")
        }
    }

    /// The ids and archived flags of a `ConversationListRefetched`, or a panic
    /// naming what arrived instead.
    fn refreshed_list(messages: &[UiMessage]) -> Vec<(String, bool)> {
        match messages {
            [UiMessage::ConversationListRefetched(convs)] => {
                convs.iter().map(|c| (c.id.clone(), c.archived)).collect()
            }
            other => panic!("expected one refreshed conversation list, got {other:?}"),
        }
    }

    /// Acceptance criterion: archiving a conversation refreshes the list, with
    /// no reconnect and no extra call from the client.
    #[tokio::test]
    async fn archiving_a_conversation_refreshes_the_list() {
        let daemon = FakeDaemon::holding(vec![summary("c1", true), summary("c2", false)]);

        let messages = archive_and_relist(&daemon, "c1", ArchiveChange::Archive).await;

        assert_eq!(
            refreshed_list(&messages),
            vec![("c1".to_string(), true), ("c2".to_string(), false)]
        );
        assert_eq!(
            *daemon.calls(),
            vec![Call::Archive("c1".to_string()), Call::ListAll],
            "the archive must be followed by a re-read of the inventory"
        );
    }

    /// Acceptance criterion: unarchiving does the same.
    #[tokio::test]
    async fn unarchiving_a_conversation_refreshes_the_list() {
        let daemon = FakeDaemon::holding(vec![summary("c1", false), summary("c2", false)]);

        let messages = archive_and_relist(&daemon, "c1", ArchiveChange::Unarchive).await;

        assert_eq!(
            refreshed_list(&messages),
            vec![("c1".to_string(), false), ("c2".to_string(), false)]
        );
        assert_eq!(
            *daemon.calls(),
            vec![Call::Unarchive("c1".to_string()), Call::ListAll],
            "the unarchive must be followed by a re-read of the inventory"
        );
    }

    /// Acceptance criterion: an archived conversation appears in the inventory,
    /// flagged `archived`.
    #[tokio::test]
    async fn the_inventory_contains_the_archived_conversations() {
        let daemon = FakeDaemon::holding(vec![summary("active", false), summary("filed", true)]);

        let convs = daemon
            .list_all()
            .await
            .expect("the fake daemon lists without failing");

        assert_eq!(
            convs
                .iter()
                .map(|c| (c.id.as_str(), c.archived))
                .collect::<Vec<_>>(),
            vec![("active", false), ("filed", true)],
            "the core must read the population that includes archived rows"
        );
        assert_eq!(
            *daemon.calls(),
            vec![Call::ListAll],
            "the core must ask for the archived population by name"
        );
    }

    /// Acceptance criterion: an active conversation is still flagged
    /// not-archived, and ordering is unchanged.
    ///
    /// Asserted on the list the actor delivers, not on the transport's answer,
    /// so a refresh that sorted, filtered, or re-flagged the rows on the way
    /// through would fail here.
    #[tokio::test]
    async fn the_refreshed_list_keeps_the_daemons_order_and_flags() {
        let daemon = FakeDaemon::holding(vec![
            summary("newest", false),
            summary("filed", true),
            summary("oldest", false),
        ]);

        let messages = archive_and_relist(&daemon, "filed", ArchiveChange::Archive).await;

        assert_eq!(
            refreshed_list(&messages),
            vec![
                ("newest".to_string(), false),
                ("filed".to_string(), true),
                ("oldest".to_string(), false),
            ]
        );
    }

    /// Acceptance criterion: a failed archive leaves the list as it was and
    /// reports the failure.
    #[tokio::test]
    async fn a_failed_archive_reports_it_and_does_not_refresh_the_list() {
        let daemon = FakeDaemon::failing_to_archive("daemon said no");

        let messages = archive_and_relist(&daemon, "c1", ArchiveChange::Archive).await;

        match messages.as_slice() {
            [UiMessage::Error(text)] => {
                assert!(text.contains("archive"), "{text}");
                assert!(text.contains("daemon said no"), "{text}");
            }
            other => panic!("expected one error message, got {other:?}"),
        }
        assert_eq!(
            *daemon.calls(),
            vec![Call::Archive("c1".to_string())],
            "a change that never landed must not repaint the list"
        );
    }

    /// The unarchive half of the same criterion: the failure is reported by the
    /// word the user asked for, and nothing repaints.
    #[tokio::test]
    async fn a_failed_unarchive_reports_it_and_does_not_refresh_the_list() {
        let daemon = FakeDaemon::failing_to_archive("daemon said no");

        let messages = archive_and_relist(&daemon, "c1", ArchiveChange::Unarchive).await;

        match messages.as_slice() {
            [UiMessage::Error(text)] => assert!(text.contains("unarchive"), "{text}"),
            other => panic!("expected one error message, got {other:?}"),
        }
        assert_eq!(*daemon.calls(), vec![Call::Unarchive("c1".to_string())]);
    }

    /// The change landed but the re-read did not: the failure is reported, and
    /// no half-built list reaches the view.
    #[tokio::test]
    async fn a_failed_refresh_after_a_landed_archive_is_reported() {
        let daemon = FakeDaemon::failing_to_list("connection lost");

        let messages = archive_and_relist(&daemon, "c1", ArchiveChange::Archive).await;

        match messages.as_slice() {
            [UiMessage::Error(text)] => assert!(text.contains("connection lost"), "{text}"),
            other => panic!("expected one error message, got {other:?}"),
        }
    }

    /// The second defect in issue #78 was one call site at a time: every path
    /// that read the list asked for the active-only population, so no client
    /// ever saw an archived conversation. [`ConversationInventory::list_all`] is
    /// now the single reader, but a blanket impl cannot take the narrower call
    /// away from a call site that still has [`AssistantClient`] in scope - a
    /// reverted line would compile and pass every behavioural test in this file
    /// while the archived rows vanished again.
    ///
    /// So this reads the crate's own sources and requires that no call site
    /// spells the narrower call. Comment lines are skipped, so writing the name
    /// in prose stays possible; the needle is assembled rather than written out,
    /// so this file does not match itself.
    #[test]
    fn no_call_site_reads_the_active_only_list() {
        let narrower = format!("list_conversations{}", "()");
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&src).expect("the crate has a src directory") {
            let path = entry.expect("a readable directory entry").path();
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("a readable source file");
            for (n, line) in text.lines().enumerate() {
                if !line.trim_start().starts_with("//") && line.contains(&narrower) {
                    offenders.push(format!("{}:{}", path.display(), n + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "every list read must go through list_all, which asks for the \
             archived population too; found the narrower call at {offenders:?}"
        );
    }

    /// An (un)archive with no connection never reaches the daemon. It is still a
    /// change that did not happen, so it is reported rather than dropped, and it
    /// is reported by the word the user asked for.
    #[test]
    fn a_change_that_cannot_be_sent_reports_itself() {
        match disconnected_report(ArchiveChange::Archive) {
            UiMessage::Error(text) => {
                assert!(text.contains("Not connected"), "{text}");
                assert!(text.contains("not archived"), "{text}");
            }
            other => panic!("expected an error message, got {other:?}"),
        }
        match disconnected_report(ArchiveChange::Unarchive) {
            UiMessage::Error(text) => assert!(text.contains("not unarchived"), "{text}"),
            other => panic!("expected an error message, got {other:?}"),
        }
    }

    #[test]
    fn a_conversation_archived_while_open_stays_open() {
        let convs = vec![summary("open", true), summary("active", false)];
        assert!(
            keeps_open(&convs, Some("open")),
            "a reader must not have an archived conversation closed out from \
             under them"
        );
    }

    #[test]
    fn a_conversation_that_left_the_list_does_not_stay_open() {
        let convs = vec![summary("active", false)];
        assert!(!keeps_open(&convs, Some("deleted-elsewhere")));
    }

    #[test]
    fn nothing_stays_open_when_nothing_is_open() {
        assert!(!keeps_open(&[summary("active", false)], None));
    }

    #[test]
    fn auto_open_takes_the_most_recent_active_conversation() {
        let convs = vec![summary("newest", false), summary("older", false)];
        assert_eq!(
            auto_open_target(&convs).map(|c| c.id.as_str()),
            Some("newest")
        );
    }

    #[test]
    fn auto_open_skips_an_archived_conversation() {
        let convs = vec![summary("filed", true), summary("active", false)];
        assert_eq!(
            auto_open_target(&convs).map(|c| c.id.as_str()),
            Some("active"),
            "a conversation the user filed away must not be reopened for them"
        );
    }

    #[test]
    fn auto_open_finds_nothing_when_every_conversation_is_archived() {
        let convs = vec![summary("filed", true), summary("also-filed", true)];
        assert_eq!(auto_open_target(&convs).map(|c| c.id.as_str()), None);
    }

    #[test]
    fn auto_open_finds_nothing_in_an_empty_list() {
        assert!(auto_open_target(&[]).is_none());
    }
}
