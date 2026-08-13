//! The embeddings settings panel (desktop-assistant#1281).
//!
//! The first panel built on the settings vocabulary in [`crate::settings`], and
//! the one where the defect the epic exists to remove is most visible: the KDE
//! client can read these settings and cannot write them, which nobody decided.
//!
//! The panel takes the daemon's [`EmbeddingsSettingsView`] plus a
//! [`DaemonContext`] and produces a [`SettingsPanelView`] a client draws, and a
//! `Command` a client sends. It holds the edits in between, so dirty tracking,
//! validation and the write rules below live in one place instead of five.
//!
//! ## Why a write carries fields the person did not touch
//!
//! The daemon's `SetEmbeddingsSettings` is a **replace**, not a patch: it
//! assigns all three of connector, model and base URL, and a `None` clears the
//! setting. A panel that sent only the edited field would silently clear the
//! other two.
//!
//! What the daemon returns is *resolved*: a value the config file does not set
//! reads as the connector's default, and only the connector says which it was
//! (`is_default`). So the write rules are:
//!
//! - An edited field is written as typed, trimmed, and a blank clears it.
//! - An untouched connector that was inherited is written as a clear, so it goes
//!   on following the main LLM connector rather than being pinned to whatever it
//!   happens to resolve to today.
//! - Any other untouched field is written back as it stands. Nothing says
//!   whether the model and the base URL were set explicitly, and writing a clear
//!   on a guess would silently drop an explicitly configured model. Writing the
//!   value back keeps the running configuration identical, at the cost of
//!   turning a default that was implicit into one that is written down.

use desktop_assistant_api_model::{Command, Config, EmbeddingHealth, EmbeddingsSettingsView};

use super::{
    ApplyState, DaemonContext, Editability, FieldError, FieldId, FieldValue, FieldView, InstanceId,
    PanelId, ReadOnlyReason, RestartState, SettingsError, SettingsPanelView, StatusLevel,
    ValidationKind, normalize_optional, validate_base_url,
};

/// The connector whose base URL is not a URL.
///
/// `docs/development.md` documents the legacy settings `base_url` for Bedrock as
/// dual-use - an AWS region (`us-east-1`) or a real endpoint - and the daemon
/// skips its URL policy for exactly this connector. Validating a bare region as
/// a URL here would refuse a working configuration.
const REGION_STYLE_CONNECTOR: &str = "bedrock";

/// The fields a person edits, in the order a panel draws them.
const EDITABLE_FIELDS: [FieldId; 3] = [FieldId::Connector, FieldId::Model, FieldId::BaseUrl];

/// The fields the daemon reports, in the order a panel draws them. They are
/// facts about the running system, so no view offers a control for one.
const DERIVED_FIELDS: [FieldId; 4] = [
    FieldId::ApiKeyPresent,
    FieldId::Available,
    FieldId::IsDefault,
    FieldId::Health,
];

/// The three values a write to this panel carries, already normalized the way
/// the daemon normalizes them. `None` clears the setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingsWrite {
    /// The provider, lowercased. `None` follows the main LLM connector.
    pub connector: Option<String>,
    /// The embedding model. `None` follows the connector's default.
    pub model: Option<String>,
    /// The endpoint. `None` follows the connector's default.
    pub base_url: Option<String>,
}

/// The embeddings panel: the daemon's values, the person's unapplied edits, and
/// every rule that decides what a view shows and what a write carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingsPanel {
    context: DaemonContext,
    loaded: EmbeddingsSettingsView,
    connector_draft: Option<String>,
    model_draft: Option<String>,
    base_url_draft: Option<String>,
    error: Option<SettingsError>,
}

impl EmbeddingsPanel {
    /// Build the panel from the daemon's values and the context they were read
    /// in.
    pub fn new(context: DaemonContext, loaded: EmbeddingsSettingsView) -> EmbeddingsPanel {
        EmbeddingsPanel {
            context,
            loaded,
            connector_draft: None,
            model_draft: None,
            base_url_draft: None,
            error: None,
        }
    }

    /// Build the panel from a `GetConfig` reply, which carries the embeddings
    /// view, the restart report and the caller's capability together.
    pub fn from_config(instance: InstanceId, config: &Config) -> EmbeddingsPanel {
        EmbeddingsPanel::new(
            DaemonContext::from_config(instance, config),
            config.embeddings.clone(),
        )
    }

    /// Which daemon this panel is bound to.
    pub fn instance(&self) -> &InstanceId {
        &self.context.instance
    }

    /// What the daemon's restart report says about this panel.
    pub fn restart(&self) -> RestartState {
        RestartState::for_panel(PanelId::Embeddings, &self.context.restart_required)
    }

    /// Record an edit.
    ///
    /// Refuses an edit to a field this connection may not change, and to a field
    /// that is not a setting at all - a view that offers such a control is
    /// working from a snapshot that told it not to, and a silently dropped edit
    /// would hide that.
    pub fn edit(&mut self, field: FieldId, value: impl Into<String>) -> Result<(), ReadOnlyReason> {
        if !EDITABLE_FIELDS.contains(&field) {
            return Err(ReadOnlyReason::Derived);
        }
        if let Editability::ReadOnly(reason) = self.panel_editability() {
            return Err(reason);
        }
        let value = value.into();
        match field {
            FieldId::Connector => self.connector_draft = Some(value),
            FieldId::Model => self.model_draft = Some(value),
            FieldId::BaseUrl => self.base_url_draft = Some(value),
            // Unreachable while `EDITABLE_FIELDS` names exactly the three above;
            // refusing rather than panicking keeps a later addition to that list
            // a bug that is reported, not one that aborts the client.
            _ => return Err(ReadOnlyReason::Derived),
        }
        // The recorded failure describes the values as they were sent. They are
        // no longer those values, so it would be read as a verdict on what is on
        // screen now.
        self.error = None;
        Ok(())
    }

    /// Discard every unapplied edit.
    pub fn revert(&mut self) {
        self.connector_draft = None;
        self.model_draft = None;
        self.base_url_draft = None;
    }

    /// Take the daemon's values again after a write, or after a `ConfigChanged`
    /// event. Clears the edits, because they are now what the daemon reports,
    /// and clears the last failure.
    pub fn read_back(&mut self, loaded: EmbeddingsSettingsView) {
        self.loaded = loaded;
        self.revert();
        self.error = None;
    }

    /// Record the failure of the last write, so the snapshot carries it.
    pub fn set_error(&mut self, error: SettingsError) {
        self.error = Some(error);
    }

    /// Whether any field carries an unapplied edit.
    pub fn is_dirty(&self) -> bool {
        EDITABLE_FIELDS
            .iter()
            .any(|field| self.is_field_dirty(*field))
    }

    /// Whether one field carries an unapplied edit.
    ///
    /// Compared after normalization, so retyping the value that is already
    /// stored - with a stray space, or a connector in another case - is not a
    /// change. The daemon would store exactly what it already has.
    fn is_field_dirty(&self, field: FieldId) -> bool {
        self.draft(field).is_some_and(|draft| {
            self.normalize(field, draft) != self.normalize(field, self.stored(field))
        })
    }

    /// The unapplied edit for a field, when there is one.
    fn draft(&self, field: FieldId) -> Option<&str> {
        match field {
            FieldId::Connector => self.connector_draft.as_deref(),
            FieldId::Model => self.model_draft.as_deref(),
            FieldId::BaseUrl => self.base_url_draft.as_deref(),
            _ => None,
        }
    }

    /// The value the daemon reported for a field.
    fn stored(&self, field: FieldId) -> &str {
        match field {
            FieldId::Connector => &self.loaded.connector,
            FieldId::Model => &self.loaded.model,
            FieldId::BaseUrl => &self.loaded.base_url,
            _ => "",
        }
    }

    /// What a field would be if the panel were applied now: the edit when there
    /// is one, else what the daemon reported.
    fn effective(&self, field: FieldId) -> &str {
        self.draft(field).unwrap_or_else(|| self.stored(field))
    }

    /// Normalize one field the way the daemon normalizes it before it stores
    /// one: every field is trimmed, and a connector is lowercased as well.
    fn normalize(&self, field: FieldId, value: &str) -> Option<String> {
        let normalized = normalize_optional(value)?;
        Some(match field {
            FieldId::Connector => normalized.to_lowercase(),
            _ => normalized,
        })
    }

    /// Whether this connection may change this panel at all.
    fn panel_editability(&self) -> Editability {
        Editability::for_panel(
            PanelId::Embeddings,
            self.context.caller_capability.as_ref(),
            self.restart(),
        )
    }

    /// Everything wrong with the values as they stand.
    ///
    /// The connector under test is the one that would be *applied*, not the one
    /// the daemon last reported: switching to Bedrock and typing a region in the
    /// same edit is a valid pair, and so is switching away from it and typing a
    /// URL.
    fn errors(&self) -> Vec<FieldError> {
        let mut errors = Vec::new();
        let connector = self.normalize(FieldId::Connector, self.effective(FieldId::Connector));

        if let Some(connector) = connector.as_deref()
            && connector.split_whitespace().count() > 1
        {
            errors.push(FieldError {
                field: FieldId::Connector,
                kind: ValidationKind::UnexpectedWhitespace,
                message: "A connector name has no spaces in it.".to_string(),
            });
        }

        let region_style = connector.as_deref() == Some(REGION_STYLE_CONNECTOR);
        if let Some(base_url) = self.normalize(FieldId::BaseUrl, self.effective(FieldId::BaseUrl))
            && !region_style
            && let Err(kind) = validate_base_url(&base_url)
        {
            errors.push(FieldError {
                field: FieldId::BaseUrl,
                kind,
                message: "Enter a full http:// or https:// address.".to_string(),
            });
        }

        errors
    }

    /// Whether Apply has anything to do, and why not when it does not.
    ///
    /// A refusal outranks a bad value: telling somebody to fix a field they
    /// cannot save is the wrong instruction.
    pub fn apply_state(&self) -> ApplyState {
        if let Editability::ReadOnly(reason) = self.panel_editability() {
            return ApplyState::NotPermitted(reason);
        }
        let errors = self.errors();
        if !errors.is_empty() {
            return ApplyState::Invalid(errors);
        }
        if !self.is_dirty() {
            return ApplyState::NoChanges;
        }
        ApplyState::Ready
    }

    /// The three values a write would carry. See the module docs for why an
    /// untouched field is not always left alone.
    pub fn pending_write(&self) -> EmbeddingsWrite {
        EmbeddingsWrite {
            // Keyed on the *change*, not on whether anything was typed: retyping
            // the connector that is already inherited is not a request to pin
            // it, and the panel has already reported that nothing changed.
            connector: match self.is_field_dirty(FieldId::Connector) {
                true => self.normalize(FieldId::Connector, self.effective(FieldId::Connector)),
                false if self.loaded.is_default => None,
                false => self.normalize(FieldId::Connector, self.stored(FieldId::Connector)),
            },
            model: self.normalize(FieldId::Model, self.effective(FieldId::Model)),
            base_url: self.normalize(FieldId::BaseUrl, self.effective(FieldId::BaseUrl)),
        }
    }

    /// The command that applies the panel, or the reason it cannot be applied.
    ///
    /// A panel with nothing changed still yields a command: the write is
    /// idempotent - the same values produce the same stored config and no
    /// further effect - so a client may re-apply without a special case.
    pub fn write_command(&self) -> Result<Command, SettingsError> {
        match self.apply_state() {
            ApplyState::Ready | ApplyState::NoChanges => {}
            ApplyState::Invalid(errors) => return Err(SettingsError::Validation { errors }),
            ApplyState::NotPermitted(reason) => {
                let message = reason.message();
                return Err(match reason {
                    ReadOnlyReason::CapabilityRequired { required, held } => {
                        SettingsError::NotAuthorized {
                            required,
                            held,
                            message,
                        }
                    }
                    // `Derived` never reaches here: it belongs to a field, not
                    // to a panel. Refusing on it anyway keeps the match total
                    // without a panic on a case a later change could create.
                    ReadOnlyReason::ConfigNotLoaded | ReadOnlyReason::Derived => {
                        SettingsError::DaemonNotWritable { message }
                    }
                });
            }
        }
        let write = self.pending_write();
        Ok(Command::SetEmbeddingsSettings {
            connector: write.connector,
            model: write.model,
            base_url: write.base_url,
        })
    }

    /// The whole panel as one value a view renders.
    pub fn view(&self) -> SettingsPanelView {
        let editability = self.panel_editability();
        let errors = self.errors();
        let error_for = |field: FieldId| errors.iter().find(|e| e.field == field).cloned();

        let mut fields = Vec::with_capacity(EDITABLE_FIELDS.len() + DERIVED_FIELDS.len());
        for field in EDITABLE_FIELDS {
            let (label, help) = labels(field);
            fields.push(FieldView {
                id: field,
                label,
                help,
                value: FieldValue::Text(self.effective(field).to_string()),
                editability: editability.clone(),
                error: error_for(field),
                dirty: self.is_field_dirty(field),
            });
        }
        for field in DERIVED_FIELDS {
            let (label, help) = labels(field);
            fields.push(FieldView {
                id: field,
                label,
                help,
                value: match field {
                    FieldId::ApiKeyPresent => FieldValue::SecretPresence(self.loaded.has_api_key),
                    FieldId::Available => FieldValue::Flag(self.loaded.available),
                    FieldId::IsDefault => FieldValue::Flag(self.loaded.is_default),
                    _ => health_value(&self.loaded.health),
                },
                editability: Editability::ReadOnly(ReadOnlyReason::Derived),
                error: None,
                dirty: false,
            });
        }

        SettingsPanelView {
            instance: self.context.instance.clone(),
            panel: PanelId::Embeddings,
            title: PanelId::Embeddings.title(),
            fields,
            restart: self.restart(),
            apply: self.apply_state(),
            dirty: self.is_dirty(),
            error: self.error.clone(),
        }
    }
}

/// The panel's read of one embedding backend's health, as a status a view can
/// draw without knowing what an embedding is.
///
/// `unknown` stays distinct from `disabled`: a backend whose health was never
/// determined must not be drawn as switched off.
pub fn health_value(health: &EmbeddingHealth) -> FieldValue {
    let (level, text, detail) = match health {
        EmbeddingHealth::Ok => (StatusLevel::Ok, "ok", None),
        EmbeddingHealth::Disabled => (StatusLevel::Info, "disabled", None),
        EmbeddingHealth::Unavailable { reason } => {
            (StatusLevel::Warning, "unavailable", Some(reason.clone()))
        }
        EmbeddingHealth::Unknown => (StatusLevel::Unknown, "unknown", None),
    };
    FieldValue::Status {
        level,
        text: text.to_string(),
        detail,
    }
}

/// The label and help line for one field. Static text: this crate carries no
/// translation layer, and every client says the same thing.
fn labels(field: FieldId) -> (&'static str, Option<&'static str>) {
    match field {
        FieldId::Connector => (
            "Connector",
            Some(
                "Which provider produces the embeddings. Leave it empty to follow the main model connector.",
            ),
        ),
        FieldId::Model => (
            "Model",
            Some("The embedding model. Leave it empty to use the connector's default."),
        ),
        FieldId::BaseUrl => (
            "Base URL",
            Some(
                "Where the connector is reached. Leave it empty to use the connector's default. For Bedrock this is an AWS region.",
            ),
        ),
        FieldId::ApiKeyPresent => (
            "API key",
            Some("Whether a credential is stored for this connector. The value is never shown."),
        ),
        FieldId::Available => (
            "Available",
            Some("Whether this connector can produce embeddings at all."),
        ),
        FieldId::IsDefault => (
            "Inherited",
            Some(
                "Whether the connector follows the main model connector rather than being set here.",
            ),
        ),
        FieldId::Health => (
            "Health",
            Some(
                "What the daemon sees right now. Without a working backend, search falls back to full text.",
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use desktop_assistant_api_model::Capability;

    use super::super::{Editability, FieldError, FieldView, StatusLevel, ValidationKind};
    use super::*;

    fn loaded_view() -> EmbeddingsSettingsView {
        EmbeddingsSettingsView {
            connector: "ollama".to_string(),
            model: "nomic-embed-text".to_string(),
            base_url: "http://127.0.0.1:11434".to_string(),
            has_api_key: false,
            available: true,
            is_default: false,
            health: EmbeddingHealth::Ok,
        }
    }

    fn context(capability: Option<Capability>, restart: &[&str]) -> DaemonContext {
        DaemonContext {
            instance: InstanceId::new("daemon-a"),
            caller_capability: capability,
            restart_required: restart.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn admin_panel() -> EmbeddingsPanel {
        EmbeddingsPanel::new(context(Some(Capability::Admin), &[]), loaded_view())
    }

    fn field(view: &SettingsPanelView, id: FieldId) -> &FieldView {
        view.fields
            .iter()
            .find(|f| f.id == id)
            .expect("the panel must carry every field")
    }

    #[test]
    fn panel_carries_every_embeddings_field_the_daemon_reports() {
        let view = admin_panel().view();
        let ids: Vec<FieldId> = view.fields.iter().map(|f| f.id).collect();
        assert_eq!(
            ids,
            vec![
                FieldId::Connector,
                FieldId::Model,
                FieldId::BaseUrl,
                FieldId::ApiKeyPresent,
                FieldId::Available,
                FieldId::IsDefault,
                FieldId::Health,
            ]
        );
        assert_eq!(view.panel, PanelId::Embeddings);
        assert_eq!(view.title, "Embeddings");
    }

    #[test]
    fn panel_values_come_from_the_daemons_view() {
        let view = admin_panel().view();
        assert_eq!(
            field(&view, FieldId::Connector).value,
            FieldValue::Text("ollama".to_string())
        );
        assert_eq!(
            field(&view, FieldId::Model).value,
            FieldValue::Text("nomic-embed-text".to_string())
        );
        assert_eq!(
            field(&view, FieldId::BaseUrl).value,
            FieldValue::Text("http://127.0.0.1:11434".to_string())
        );
        assert_eq!(
            field(&view, FieldId::Available).value,
            FieldValue::Flag(true)
        );
        assert_eq!(
            field(&view, FieldId::IsDefault).value,
            FieldValue::Flag(false)
        );
    }

    #[test]
    fn the_panel_reports_the_instance_it_is_bound_to() {
        let panel = admin_panel();
        assert_eq!(panel.instance().as_str(), "daemon-a");
        assert_eq!(panel.view().instance, InstanceId::new("daemon-a"));
    }

    #[test]
    fn a_panel_built_from_a_config_reply_takes_its_capability_and_restart_report() {
        let config = Config {
            embeddings: loaded_view(),
            personality: Default::default(),
            restart_required: vec!["embeddings".to_string()],
            caller_capability: Some(Capability::Tenant),
        };
        let panel = EmbeddingsPanel::from_config(InstanceId::new("daemon-b"), &config);
        let view = panel.view();
        assert_eq!(view.instance, InstanceId::new("daemon-b"));
        assert_eq!(view.restart, RestartState::RestartRequired);
        assert!(!field(&view, FieldId::Connector).editability.is_editable());
    }

    #[test]
    fn the_api_key_field_carries_only_whether_a_key_is_stored() {
        let mut loaded = loaded_view();
        loaded.has_api_key = true;
        let panel = EmbeddingsPanel::new(context(Some(Capability::Admin), &[]), loaded);
        assert_eq!(
            field(&panel.view(), FieldId::ApiKeyPresent).value,
            FieldValue::SecretPresence(true)
        );
    }

    #[test]
    fn daemon_reported_fields_are_read_only_because_they_are_not_settings() {
        let view = admin_panel().view();
        for id in [
            FieldId::ApiKeyPresent,
            FieldId::Available,
            FieldId::IsDefault,
            FieldId::Health,
        ] {
            assert_eq!(
                field(&view, id).editability,
                Editability::ReadOnly(ReadOnlyReason::Derived),
                "{} must not be offered as a control",
                id.as_key()
            );
        }
    }

    #[test]
    fn an_admin_may_edit_the_three_configured_fields() {
        let view = admin_panel().view();
        for id in [FieldId::Connector, FieldId::Model, FieldId::BaseUrl] {
            assert_eq!(field(&view, id).editability, Editability::Editable);
        }
    }

    #[test]
    fn a_tenant_may_edit_nothing_and_is_told_why() {
        let panel = EmbeddingsPanel::new(context(Some(Capability::Tenant), &[]), loaded_view());
        let view = panel.view();
        for id in [FieldId::Connector, FieldId::Model, FieldId::BaseUrl] {
            assert_eq!(
                field(&view, id).editability,
                Editability::ReadOnly(ReadOnlyReason::CapabilityRequired {
                    required: Capability::Admin,
                    held: Some(Capability::Tenant),
                })
            );
        }
        assert_eq!(
            view.apply,
            ApplyState::NotPermitted(ReadOnlyReason::CapabilityRequired {
                required: Capability::Admin,
                held: Some(Capability::Tenant),
            })
        );
    }

    #[test]
    fn a_tenants_edit_is_refused_rather_than_dropped() {
        let mut panel = EmbeddingsPanel::new(context(Some(Capability::Tenant), &[]), loaded_view());
        assert_eq!(
            panel.edit(FieldId::Model, "mxbai-embed-large"),
            Err(ReadOnlyReason::CapabilityRequired {
                required: Capability::Admin,
                held: Some(Capability::Tenant),
            })
        );
        assert!(!panel.is_dirty());
    }

    #[test]
    fn an_edit_to_a_daemon_reported_field_is_refused() {
        let mut panel = admin_panel();
        assert_eq!(
            panel.edit(FieldId::Health, "ok"),
            Err(ReadOnlyReason::Derived)
        );
        assert_eq!(
            panel.edit(FieldId::ApiKeyPresent, "true"),
            Err(ReadOnlyReason::Derived)
        );
        assert!(!panel.is_dirty());
    }

    #[test]
    fn health_unavailable_is_a_warning_carrying_the_daemons_reason() {
        let mut loaded = loaded_view();
        loaded.health = EmbeddingHealth::Unavailable {
            reason: "the probe failed".to_string(),
        };
        let panel = EmbeddingsPanel::new(context(Some(Capability::Admin), &[]), loaded);
        assert_eq!(
            field(&panel.view(), FieldId::Health).value,
            FieldValue::Status {
                level: StatusLevel::Warning,
                text: "unavailable".to_string(),
                detail: Some("the probe failed".to_string()),
            }
        );
    }

    #[test]
    fn health_disabled_is_stated_without_a_warning() {
        assert_eq!(
            health_value(&EmbeddingHealth::Disabled),
            FieldValue::Status {
                level: StatusLevel::Info,
                text: "disabled".to_string(),
                detail: None,
            }
        );
    }

    #[test]
    fn health_unknown_is_not_reported_as_disabled() {
        let unknown = health_value(&EmbeddingHealth::Unknown);
        assert_eq!(
            unknown,
            FieldValue::Status {
                level: StatusLevel::Unknown,
                text: "unknown".to_string(),
                detail: None,
            }
        );
        assert_ne!(unknown, health_value(&EmbeddingHealth::Disabled));
    }

    #[test]
    fn health_ok_is_reported_as_working() {
        assert_eq!(
            health_value(&EmbeddingHealth::Ok),
            FieldValue::Status {
                level: StatusLevel::Ok,
                text: "ok".to_string(),
                detail: None,
            }
        );
    }

    #[test]
    fn a_freshly_loaded_panel_has_nothing_to_apply() {
        let panel = admin_panel();
        assert!(!panel.is_dirty());
        assert_eq!(panel.apply_state(), ApplyState::NoChanges);
        assert!(!panel.apply_state().is_ready());
        assert!(!panel.view().dirty);
    }

    #[test]
    fn an_edit_marks_its_own_field_and_the_panel_dirty() {
        let mut panel = admin_panel();
        panel.edit(FieldId::Model, "mxbai-embed-large").unwrap();
        assert!(panel.is_dirty());
        assert_eq!(panel.apply_state(), ApplyState::Ready);

        let view = panel.view();
        assert!(view.dirty);
        assert!(field(&view, FieldId::Model).dirty);
        assert!(!field(&view, FieldId::Connector).dirty);
        assert_eq!(
            field(&view, FieldId::Model).value,
            FieldValue::Text("mxbai-embed-large".to_string())
        );
    }

    #[test]
    fn retyping_the_loaded_value_leaves_the_panel_clean() {
        let mut panel = admin_panel();
        panel.edit(FieldId::Model, "  nomic-embed-text  ").unwrap();
        assert!(!panel.is_dirty());
        assert_eq!(panel.apply_state(), ApplyState::NoChanges);
    }

    #[test]
    fn a_connector_retyped_in_a_different_case_is_not_a_change() {
        // The daemon lowercases a connector before it stores one.
        let mut panel = admin_panel();
        panel.edit(FieldId::Connector, "Ollama").unwrap();
        assert!(!panel.is_dirty());
    }

    #[test]
    fn revert_discards_every_edit() {
        let mut panel = admin_panel();
        panel.edit(FieldId::Model, "mxbai-embed-large").unwrap();
        panel
            .edit(FieldId::BaseUrl, "https://embed.example.com")
            .unwrap();
        panel.revert();
        assert!(!panel.is_dirty());
        assert_eq!(
            field(&panel.view(), FieldId::Model).value,
            FieldValue::Text("nomic-embed-text".to_string())
        );
    }

    #[test]
    fn reading_back_after_a_write_clears_the_edits_and_the_last_failure() {
        let mut panel = admin_panel();
        panel.edit(FieldId::Model, "mxbai-embed-large").unwrap();
        panel.set_error(SettingsError::from_link_failure("connection closed"));

        let mut applied = loaded_view();
        applied.model = "mxbai-embed-large".to_string();
        panel.read_back(applied);

        assert!(!panel.is_dirty());
        assert_eq!(panel.apply_state(), ApplyState::NoChanges);
        let view = panel.view();
        assert_eq!(view.error, None);
        assert_eq!(
            field(&view, FieldId::Model).value,
            FieldValue::Text("mxbai-embed-large".to_string())
        );
    }

    #[test]
    fn a_recorded_failure_is_carried_on_the_snapshot() {
        let mut panel = admin_panel();
        panel.set_error(SettingsError::from_link_failure("connection closed"));
        assert_eq!(
            panel.view().error,
            Some(SettingsError::from_link_failure("connection closed"))
        );
    }

    #[test]
    fn a_base_url_without_a_scheme_blocks_apply_and_names_the_field() {
        let mut panel = admin_panel();
        panel.edit(FieldId::BaseUrl, "embed.example.com").unwrap();

        let expected = FieldError {
            field: FieldId::BaseUrl,
            kind: ValidationKind::MalformedUrl,
            message: "Enter a full http:// or https:// address.".to_string(),
        };
        assert_eq!(
            panel.apply_state(),
            ApplyState::Invalid(vec![expected.clone()])
        );
        assert_eq!(
            field(&panel.view(), FieldId::BaseUrl).error,
            Some(expected.clone())
        );
        assert_eq!(
            panel.write_command(),
            Err(SettingsError::Validation {
                errors: vec![expected],
            })
        );
    }

    #[test]
    fn a_bedrock_base_url_may_be_a_region_rather_than_a_url() {
        let mut loaded = loaded_view();
        loaded.connector = "bedrock".to_string();
        loaded.base_url = "us-east-1".to_string();
        let mut panel = EmbeddingsPanel::new(context(Some(Capability::Admin), &[]), loaded);
        panel.edit(FieldId::BaseUrl, "eu-west-2").unwrap();
        assert_eq!(panel.apply_state(), ApplyState::Ready);
        assert_eq!(field(&panel.view(), FieldId::BaseUrl).error, None);
    }

    #[test]
    fn switching_to_bedrock_in_the_same_edit_accepts_a_region() {
        // The connector under validation is the one being applied, not the one
        // the daemon last reported.
        let mut panel = admin_panel();
        panel.edit(FieldId::Connector, "bedrock").unwrap();
        panel.edit(FieldId::BaseUrl, "us-east-1").unwrap();
        assert_eq!(panel.apply_state(), ApplyState::Ready);
    }

    #[test]
    fn switching_away_from_bedrock_in_the_same_edit_requires_a_url() {
        let mut loaded = loaded_view();
        loaded.connector = "bedrock".to_string();
        loaded.base_url = "us-east-1".to_string();
        let mut panel = EmbeddingsPanel::new(context(Some(Capability::Admin), &[]), loaded);
        panel.edit(FieldId::Connector, "ollama").unwrap();
        assert!(matches!(panel.apply_state(), ApplyState::Invalid(_)));
    }

    #[test]
    fn a_connector_with_a_space_in_it_is_rejected() {
        let mut panel = admin_panel();
        panel.edit(FieldId::Connector, "open ai").unwrap();
        assert_eq!(
            panel.apply_state(),
            ApplyState::Invalid(vec![FieldError {
                field: FieldId::Connector,
                kind: ValidationKind::UnexpectedWhitespace,
                message: "A connector name has no spaces in it.".to_string(),
            }])
        );
    }

    #[test]
    fn a_cleared_field_is_written_as_a_clear() {
        let mut panel = admin_panel();
        panel.edit(FieldId::Model, "   ").unwrap();
        assert_eq!(panel.pending_write().model, None);
        assert!(panel.is_dirty());
    }

    #[test]
    fn an_edited_value_is_trimmed_before_it_is_written() {
        let mut panel = admin_panel();
        panel
            .edit(FieldId::Model, "  mxbai-embed-large \n")
            .unwrap();
        assert_eq!(
            panel.pending_write().model,
            Some("mxbai-embed-large".to_string())
        );
    }

    #[test]
    fn an_edited_connector_is_lowercased_before_it_is_written() {
        let mut panel = admin_panel();
        panel.edit(FieldId::Connector, "OpenAI").unwrap();
        assert_eq!(panel.pending_write().connector, Some("openai".to_string()));
    }

    #[test]
    fn retyping_an_inherited_connector_unchanged_does_not_pin_it() {
        let mut loaded = loaded_view();
        loaded.is_default = true;
        let mut panel = EmbeddingsPanel::new(context(Some(Capability::Admin), &[]), loaded);
        panel.edit(FieldId::Connector, "ollama").unwrap();
        panel.edit(FieldId::Model, "mxbai-embed-large").unwrap();
        assert_eq!(panel.pending_write().connector, None);
    }

    #[test]
    fn changing_an_inherited_connector_pins_it() {
        let mut loaded = loaded_view();
        loaded.is_default = true;
        let mut panel = EmbeddingsPanel::new(context(Some(Capability::Admin), &[]), loaded);
        panel.edit(FieldId::Connector, "openai").unwrap();
        assert_eq!(panel.pending_write().connector, Some("openai".to_string()));
    }

    #[test]
    fn an_edit_clears_a_failure_that_described_the_old_values() {
        let mut panel = admin_panel();
        panel.set_error(SettingsError::from_link_failure("connection closed"));
        panel.edit(FieldId::Model, "mxbai-embed-large").unwrap();
        assert_eq!(panel.view().error, None);
    }

    #[test]
    fn an_untouched_inherited_connector_is_written_as_a_clear() {
        let mut loaded = loaded_view();
        loaded.is_default = true;
        let mut panel = EmbeddingsPanel::new(context(Some(Capability::Admin), &[]), loaded);
        panel.edit(FieldId::Model, "mxbai-embed-large").unwrap();
        // Left implicit, so it goes on following the main LLM connector.
        assert_eq!(panel.pending_write().connector, None);
    }

    #[test]
    fn an_untouched_explicit_connector_is_written_back_unchanged() {
        let mut panel = admin_panel();
        panel.edit(FieldId::Model, "mxbai-embed-large").unwrap();
        assert_eq!(panel.pending_write().connector, Some("ollama".to_string()));
    }

    #[test]
    fn an_untouched_model_is_written_back_so_a_base_url_edit_cannot_clear_it() {
        let mut panel = admin_panel();
        panel
            .edit(FieldId::BaseUrl, "https://embed.example.com")
            .unwrap();
        let write = panel.pending_write();
        assert_eq!(write.model, Some("nomic-embed-text".to_string()));
        assert_eq!(
            write.base_url,
            Some("https://embed.example.com".to_string())
        );
    }

    #[test]
    fn write_command_builds_the_daemons_set_command_from_the_pending_write() {
        let mut panel = admin_panel();
        panel.edit(FieldId::Model, "mxbai-embed-large").unwrap();
        assert_eq!(
            panel.write_command(),
            Ok(Command::SetEmbeddingsSettings {
                connector: Some("ollama".to_string()),
                model: Some("mxbai-embed-large".to_string()),
                base_url: Some("http://127.0.0.1:11434".to_string()),
            })
        );
    }

    #[test]
    fn write_command_on_an_unchanged_panel_is_an_idempotent_rewrite() {
        let panel = admin_panel();
        assert_eq!(panel.apply_state(), ApplyState::NoChanges);
        assert_eq!(
            panel.write_command(),
            Ok(Command::SetEmbeddingsSettings {
                connector: Some("ollama".to_string()),
                model: Some("nomic-embed-text".to_string()),
                base_url: Some("http://127.0.0.1:11434".to_string()),
            })
        );
    }

    #[test]
    fn write_command_refuses_a_tenant_with_the_authorization_error() {
        let panel = EmbeddingsPanel::new(context(Some(Capability::Tenant), &[]), loaded_view());
        assert_eq!(
            panel.write_command(),
            Err(SettingsError::NotAuthorized {
                required: Capability::Admin,
                held: Some(Capability::Tenant),
                message: ReadOnlyReason::CapabilityRequired {
                    required: Capability::Admin,
                    held: Some(Capability::Tenant),
                }
                .message(),
            })
        );
    }

    #[test]
    fn a_daemon_running_built_in_defaults_shows_the_panel_read_only() {
        let panel = EmbeddingsPanel::new(
            context(Some(Capability::Admin), &["config_load_failed"]),
            loaded_view(),
        );
        let view = panel.view();
        assert_eq!(view.restart, RestartState::ConfigNotLoaded);
        assert_eq!(
            field(&view, FieldId::Model).editability,
            Editability::ReadOnly(ReadOnlyReason::ConfigNotLoaded)
        );
        assert_eq!(
            view.apply,
            ApplyState::NotPermitted(ReadOnlyReason::ConfigNotLoaded)
        );
        assert!(matches!(
            panel.write_command(),
            Err(SettingsError::DaemonNotWritable { .. })
        ));
    }

    #[test]
    fn a_pending_restart_is_reported_without_blocking_the_edit() {
        let mut panel = EmbeddingsPanel::new(
            context(Some(Capability::Admin), &["embeddings"]),
            loaded_view(),
        );
        assert_eq!(panel.view().restart, RestartState::RestartRequired);
        panel.edit(FieldId::Model, "mxbai-embed-large").unwrap();
        assert_eq!(panel.apply_state(), ApplyState::Ready);
    }

    #[test]
    fn a_capability_refusal_outranks_an_invalid_value() {
        // The panel is not writable at all, so "fix the field" would be the
        // wrong thing to tell somebody.
        let mut panel = EmbeddingsPanel::new(context(Some(Capability::Tenant), &[]), loaded_view());
        // A host may still push a value in; the edit itself is refused.
        assert!(panel.edit(FieldId::BaseUrl, "not a url").is_err());
        assert_eq!(
            panel.apply_state(),
            ApplyState::NotPermitted(ReadOnlyReason::CapabilityRequired {
                required: Capability::Admin,
                held: Some(Capability::Tenant),
            })
        );
    }

    #[test]
    fn a_daemon_that_reports_no_capability_leaves_the_panel_editable() {
        let mut panel = EmbeddingsPanel::new(context(None, &[]), loaded_view());
        assert!(panel.edit(FieldId::Model, "mxbai-embed-large").is_ok());
        assert_eq!(panel.apply_state(), ApplyState::Ready);
    }
}
