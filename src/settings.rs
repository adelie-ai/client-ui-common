//! Settings view-model: the daemon's settings panels as plain data
//! (desktop-assistant#1281, epic #1297).
//!
//! A pure, view-agnostic model for the settings panels every UI (gtk, tui, web,
//! mac, and - via the ffi cdylib - kde) renders. It follows the rules
//! [`crate::mcp_servers`] set: everything here takes *already-resolved plain
//! data* and returns plain data, and it depends on no transport, so the crate
//! stays wasm-clean. The host reads the daemon however it likes, hands the wire
//! views in, and sends back the [`Command`] this module builds.
//!
//! [`Command`]: desktop_assistant_api_model::Command
//!
//! ## One snapshot per panel, not one signal per control
//!
//! A panel is one structured value - [`SettingsPanelView`] - that describes its
//! fields, their values, what is wrong with each, whether each is editable, what
//! needs a daemon restart, and which daemon instance the whole thing is bound
//! to. A view renders that description; it does not know what a base URL is. A
//! setting added later is new *data* in the snapshot, so a view picks it up
//! without being edited. One message per field per panel would put today's
//! five-way disparity in a shared crate instead of removing it.
//!
//! ## What this layer decides, and what it does not
//!
//! It decides: what a value must look like, which controls this connection may
//! change, what a restart report means for one panel, how a daemon failure is
//! classified, what a write carries, and whether Apply has anything to do.
//!
//! It does not decide whether a value is *acceptable to the daemon*. The daemon
//! stays the authority: its remote-URL policy, its embedding-model guard and its
//! config-write guards all run after this model has said yes. The local checks
//! here never widen what the daemon accepts - they catch the common typo early,
//! and a value that passes here can still be refused, which arrives as
//! [`SettingsError::Validation`] carrying the daemon's own code.

pub mod embeddings;

use desktop_assistant_api_model::{Capability, Config, ErrorCode, ErrorDetail};

/// Which daemon a panel is bound to.
///
/// Opaque on purpose: the instance list is desktop-assistant#999 and does not
/// exist yet, so the host supplies whatever identifier it already uses for the
/// connection it read the settings over (a socket path, a URL, a nickname). The
/// model never parses it - it only carries it, so a panel can always say which
/// daemon it is about and two panels for two daemons can never be confused.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InstanceId(String);

impl InstanceId {
    /// Wrap the host's identifier for one daemon connection.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier the host supplied, verbatim.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which settings panel a snapshot describes.
///
/// A closed set: a panel added later must be named here, so every place that
/// switches on a panel fails to compile until it handles the new one. Only
/// [`Self::Embeddings`] is modelled so far; the remaining panels
/// (database, backend tasks, WebSocket auth) follow as their own slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelId {
    /// The embedding backend: connector, model, base URL, and its live health.
    Embeddings,
}

impl PanelId {
    /// Heading for the panel. Plain, and the same in every client.
    pub fn title(self) -> &'static str {
        match self {
            PanelId::Embeddings => "Embeddings",
        }
    }

    /// The capability a write to this panel needs.
    ///
    /// The daemon's dispatcher classifies every `Get*Settings` as
    /// [`Capability::Tenant`] and every `Set*Settings` as [`Capability::Admin`]
    /// (`transport-dispatch::authz::required_capability`). Offering a control
    /// the caller will be refused is the defect this model exists to remove, so
    /// the panel asks this before it says a field is editable.
    pub fn write_capability(self) -> Capability {
        match self {
            PanelId::Embeddings => Capability::Admin,
        }
    }

    /// The config area whose restart report speaks for this panel.
    pub fn config_area(self) -> ConfigArea {
        match self {
            PanelId::Embeddings => ConfigArea::Embeddings,
        }
    }

    /// The field a daemon URL-policy refusal belongs to, when the panel has one.
    ///
    /// The daemon validates a settings URL with the shared remote-URL policy and
    /// reports the refusal under a `url_*` code. Naming the field here is what
    /// lets that refusal land on the control the person typed into, instead of
    /// as a panel-wide banner.
    pub fn url_field(self) -> Option<FieldId> {
        match self {
            PanelId::Embeddings => Some(FieldId::BaseUrl),
        }
    }
}

/// An area of the daemon's config file, as it appears in
/// [`Config::restart_required`].
///
/// Mirrors the daemon's `RestartArea` (`daemon/src/config/reload.rs`), which is
/// a deliberately closed enum whose `as_key` strings cross the wire. The mirror
/// is kept in step by hand - nothing links the two enums - so [`Self::Other`]
/// keeps a key this build does not know verbatim rather than dropping it. The
/// wire contract asks clients to render an unrecognized key rather than hide it,
/// and a dropped key would tell somebody their edit is live when it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigArea {
    /// The whole file would not load, so *nothing* in it is in force and the
    /// daemon is running built-in defaults. Not an area: it speaks for every
    /// panel at once.
    ConfigLoadFailed,
    /// `[database]`.
    Database,
    /// The embedding backend.
    Embeddings,
    /// `[ws_auth]`.
    WsAuth,
    /// `[tls]`.
    Tls,
    /// `[authz]`.
    Authz,
    /// `[recall]`.
    Recall,
    /// An area this build does not know, carried verbatim.
    Other(String),
}

impl ConfigArea {
    /// Read one key from the daemon's restart report.
    pub fn from_key(key: &str) -> ConfigArea {
        match key {
            "config_load_failed" => ConfigArea::ConfigLoadFailed,
            "database" => ConfigArea::Database,
            "embeddings" => ConfigArea::Embeddings,
            "ws_auth" => ConfigArea::WsAuth,
            "tls" => ConfigArea::Tls,
            "authz" => ConfigArea::Authz,
            "recall" => ConfigArea::Recall,
            other => ConfigArea::Other(other.to_string()),
        }
    }

    /// The stable key the daemon uses for this area.
    pub fn as_key(&self) -> &str {
        match self {
            ConfigArea::ConfigLoadFailed => "config_load_failed",
            ConfigArea::Database => "database",
            ConfigArea::Embeddings => "embeddings",
            ConfigArea::WsAuth => "ws_auth",
            ConfigArea::Tls => "tls",
            ConfigArea::Authz => "authz",
            ConfigArea::Recall => "recall",
            ConfigArea::Other(key) => key,
        }
    }
}

/// What the daemon's restart report says about one panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartState {
    /// Everything this panel configures is in force.
    InForce,
    /// The config file holds a value for this panel that the running process is
    /// not acting on. It is wired once at startup, so a restart applies it.
    RestartRequired,
    /// The daemon could not load its config file and is running built-in
    /// defaults, so nothing in the file is in force - for this panel or any
    /// other.
    ConfigNotLoaded,
}

impl RestartState {
    /// Read the daemon's restart report for one panel.
    ///
    /// `report` is [`Config::restart_required`] verbatim.
    /// [`ConfigArea::ConfigLoadFailed`] outranks a pending area: when the file
    /// did not load at all, saying "a restart applies your embeddings change"
    /// would be false - the file it came from is not in force.
    pub fn for_panel(panel: PanelId, report: &[String]) -> RestartState {
        let areas = || report.iter().map(|key| ConfigArea::from_key(key));
        if areas().any(|area| area == ConfigArea::ConfigLoadFailed) {
            return RestartState::ConfigNotLoaded;
        }
        let own = panel.config_area();
        if areas().any(|area| area == own) {
            return RestartState::RestartRequired;
        }
        RestartState::InForce
    }

    /// Text fit to show the person using the client, or `None` when there is
    /// nothing to say.
    pub fn message(self) -> Option<&'static str> {
        match self {
            RestartState::InForce => None,
            RestartState::RestartRequired => {
                Some("This is configured but not running. Restart the daemon to apply it.")
            }
            RestartState::ConfigNotLoaded => Some(
                "The daemon could not read its config file, so it is running built-in \
                 defaults. The values here are the file's, not the running ones. Repair the \
                 file to make them live.",
            ),
        }
    }
}

/// One field of a panel. Unique within its panel; the same id is reused across
/// panels for the same kind of setting, so one renderer serves both.
///
/// Closed, like [`ConfigArea`]: a field added later must be named here, and
/// [`Self::as_key`] is the stable string a view or the C ABI routes input events
/// back on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldId {
    /// Which provider serves the panel's backend.
    Connector,
    /// The model name.
    Model,
    /// The endpoint the connector is called on.
    BaseUrl,
    /// Whether a credential is stored - never the credential.
    ApiKeyPresent,
    /// Whether the configured backend is usable at all.
    Available,
    /// Whether the value is inherited rather than set explicitly.
    IsDefault,
    /// The backend's live health.
    Health,
}

impl FieldId {
    /// Stable key for this field, safe to put on the wire and to match on in a
    /// view or through the C ABI.
    pub fn as_key(self) -> &'static str {
        match self {
            FieldId::Connector => "connector",
            FieldId::Model => "model",
            FieldId::BaseUrl => "base_url",
            FieldId::ApiKeyPresent => "api_key_present",
            FieldId::Available => "available",
            FieldId::IsDefault => "is_default",
            FieldId::Health => "health",
        }
    }
}

/// How serious a [`FieldValue::Status`] is, so a view can pick a colour without
/// reading the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLevel {
    /// Working.
    Ok,
    /// A plain statement of fact - off by design, for example.
    Info,
    /// Configured but not working: something is degraded.
    Warning,
    /// Not determined. Deliberately distinct from "off by design", so a
    /// working-but-unreported backend is never drawn as switched off.
    Unknown,
}

/// The value of one field, in the shape a view renders.
///
/// SECURITY: no panel puts a secret in one. A credential is reported as
/// [`Self::SecretPresence`] - whether one is stored - which is all the daemon
/// reports for it and all a panel needs. That is a rule the panels keep, not one
/// the type enforces: [`Self::Text`] would hold anything given to it. A panel
/// that has to *take* a secret needs a variant of its own that a snapshot never
/// reads back, rather than reusing [`Self::Text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    /// A free-text value the person edits.
    Text(String),
    /// A yes/no fact.
    Flag(bool),
    /// Whether a credential is stored.
    SecretPresence(bool),
    /// A state the daemon reports, with the severity a view draws it at.
    Status {
        /// How serious the state is.
        level: StatusLevel,
        /// Short state name, e.g. `unavailable`.
        text: String,
        /// The daemon's explanation, when it gave one.
        detail: Option<String>,
    },
}

/// Why a field is shown but cannot be changed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyReason {
    /// The daemon reports this; it is not a setting. Health and "a credential is
    /// stored" are facts about the running system, not knobs.
    Derived,
    /// Changing this needs a capability this connection does not hold. `held` is
    /// what the daemon last reported for the connection, or `None` when it
    /// reported nothing.
    CapabilityRequired {
        /// The capability the write needs.
        required: Capability,
        /// The capability the connection holds, when the daemon said.
        held: Option<Capability>,
    },
    /// The daemon could not load its config file and is running built-in
    /// defaults, so the values shown are the file's content and not what the
    /// process is running.
    ConfigNotLoaded,
}

impl ReadOnlyReason {
    /// Text fit to show the person using the client.
    pub fn message(&self) -> String {
        match self {
            ReadOnlyReason::Derived => {
                "The daemon reports this. It is not a setting you can change.".to_string()
            }
            ReadOnlyReason::CapabilityRequired { required, held } => match held {
                Some(held) => format!(
                    "Changing this needs the {} capability. This connection has {}.",
                    required.label(),
                    held.label(),
                ),
                None => format!("Changing this needs the {} capability.", required.label()),
            },
            ReadOnlyReason::ConfigNotLoaded => {
                "The daemon could not read its config file and is running built-in defaults. \
                 Repair the file before you change these settings."
                    .to_string()
            }
        }
    }
}

/// Whether a view should offer a control for a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Editability {
    /// Offer the control.
    Editable,
    /// Show the value, refuse the edit, and say why.
    ReadOnly(ReadOnlyReason),
}

impl Editability {
    /// Whether a view should offer a control for this field.
    pub fn is_editable(&self) -> bool {
        matches!(self, Editability::Editable)
    }

    /// Whether this connection may write `panel`, given the capability the
    /// daemon reported and what its restart report says.
    ///
    /// Three rules, and the first is a backward-compatibility rule the wire
    /// contract states: a daemon that reports no capability predates the
    /// authorization tier and would accept the write, so the panel stays
    /// editable rather than hiding a control that works. An unrecognized
    /// capability grants nothing ([`Capability::permits`] is written out with no
    /// wildcard arm), so it lands read-only.
    ///
    /// The third rule is this layer's own policy, and it is deliberately wider
    /// than what any one daemon path refuses: a daemon running built-in defaults
    /// is read-only whatever the caller holds. The values a panel shows then
    /// come from a config file the running process is not acting on, so a person
    /// would be editing something other than what is live.
    ///
    /// What the daemon itself does is narrower and differs per command. A
    /// whole-config write is refused outright while the daemon runs defaults
    /// (`api_surface::refuse_if_overwrite_would_destroy_the_file`). A settings
    /// write goes straight to the file, so it fails only while the file will not
    /// parse - once the file parses again the daemon would take the write, even
    /// though it is still running the defaults it booted with, and this model
    /// still says read-only. One policy for every panel is worth that, because
    /// the alternative is per-command archaeology repeated in five clients.
    pub fn for_panel(
        panel: PanelId,
        held: Option<&Capability>,
        restart: RestartState,
    ) -> Editability {
        if restart == RestartState::ConfigNotLoaded {
            return Editability::ReadOnly(ReadOnlyReason::ConfigNotLoaded);
        }
        let required = panel.write_capability();
        match held {
            // The daemon reported nothing, so it predates the authorization
            // tier and would take the write. Hiding a control that works would
            // be the worse error.
            None => Editability::Editable,
            Some(held) if held.permits(&required) => Editability::Editable,
            Some(held) => Editability::ReadOnly(ReadOnlyReason::CapabilityRequired {
                required,
                held: Some(held.clone()),
            }),
        }
    }
}

/// What is wrong with one field's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// Which field the message belongs to.
    pub field: FieldId,
    /// What kind of thing is wrong, for a view that branches rather than reads.
    pub kind: ValidationKind,
    /// Text fit to show the person who typed the value.
    pub message: String,
}

/// Why a value was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationKind {
    /// The value must be a well-formed absolute `http`/`https` URL and is not.
    MalformedUrl,
    /// The value contains whitespace where none is allowed.
    UnexpectedWhitespace,
    /// The daemon refused the value; the checks here did not catch it. Carries
    /// the daemon's own stable code, so the reason survives without this model
    /// pretending it made the judgement.
    RefusedByDaemon(String),
}

/// One field of a panel, exactly as a view draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldView {
    /// Which field this is.
    pub id: FieldId,
    /// Label for the control.
    pub label: &'static str,
    /// One line of help, when the field needs it.
    pub help: Option<&'static str>,
    /// The value to draw: the person's edit when they made one, else the
    /// daemon's.
    pub value: FieldValue,
    /// Whether to offer a control, and why not when not.
    pub editability: Editability,
    /// What is wrong with the current value, when something is.
    pub error: Option<FieldError>,
    /// Whether this field carries an unapplied edit.
    pub dirty: bool,
}

/// Whether Apply has anything to do, and why not when it does not.
///
/// A view asks this instead of inventing the rule, which is how five clients
/// come to disagree about when Apply is enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyState {
    /// There are valid changes to write: enable Apply.
    Ready,
    /// Nothing was changed.
    NoChanges,
    /// At least one field is invalid. Apply stays disabled until it is fixed.
    Invalid(Vec<FieldError>),
    /// This connection may not write this panel.
    NotPermitted(ReadOnlyReason),
}

impl ApplyState {
    /// Whether a view should enable its Apply control.
    pub fn is_ready(&self) -> bool {
        matches!(self, ApplyState::Ready)
    }
}

/// One panel, as one value. This is what a view renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsPanelView {
    /// Which daemon this panel is about.
    pub instance: InstanceId,
    /// Which panel this is.
    pub panel: PanelId,
    /// Heading for the panel.
    pub title: &'static str,
    /// Every field, editable ones first and daemon-reported ones after, in a
    /// fixed order so a view never has to sort.
    pub fields: Vec<FieldView>,
    /// What the daemon's restart report says about this panel.
    pub restart: RestartState,
    /// Whether Apply has anything to do.
    pub apply: ApplyState,
    /// Whether any field carries an unapplied edit.
    pub dirty: bool,
    /// The last failure the host reported for this panel, when there is one.
    pub error: Option<SettingsError>,
}

/// A settings failure, classified. A transport failure, an authorization
/// refusal and a validation error demand three different responses - retry, ask
/// the operator, fix the value - so they never arrive here as one string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsError {
    /// The request did not get a verdict, or got one this model cannot
    /// classify. Retry is meaningful when the daemon said so.
    Transport {
        /// Text fit to show the person using the client.
        message: String,
        /// Whether repeating the identical request could plausibly succeed.
        retryable: bool,
    },
    /// The caller is authenticated but does not hold the capability the write
    /// needs. Never retryable: repeating it cannot change the answer.
    NotAuthorized {
        /// The capability the write needs.
        required: Capability,
        /// What the connection holds, when the daemon said.
        held: Option<Capability>,
        /// Text fit to show the person using the client.
        message: String,
    },
    /// A value was rejected - by this model before the send, or by the daemon
    /// after it.
    Validation {
        /// One entry per rejected field.
        errors: Vec<FieldError>,
    },
    /// This daemon does not implement the command, or has the feature switched
    /// off. Retrying cannot help; the control belongs hidden.
    Unsupported {
        /// Text fit to show the person using the client.
        message: String,
    },
    /// The daemon is not in a state where it can take this write, for a reason
    /// that is neither the caller's capability nor the value they typed. The one
    /// such reason today is a config file the daemon could not load, so it is
    /// running built-in defaults. Repeating the request cannot help; the file
    /// has to be fixed.
    ///
    /// Its own kind rather than one of the four above, because the response it
    /// asks for is different again: not retry, not re-authenticate, not fix the
    /// field, but repair the daemon's config file.
    DaemonNotWritable {
        /// Text fit to show the person using the client.
        message: String,
    },
}

/// The kind of a [`SettingsError`], without its payload - the discriminant a
/// view or the C ABI branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsErrorKind {
    /// See [`SettingsError::Transport`].
    Transport,
    /// See [`SettingsError::NotAuthorized`].
    NotAuthorized,
    /// See [`SettingsError::Validation`].
    Validation,
    /// See [`SettingsError::Unsupported`].
    Unsupported,
    /// See [`SettingsError::DaemonNotWritable`].
    DaemonNotWritable,
}

impl SettingsErrorKind {
    /// Stable identifier for the kind, for a log line or the C ABI.
    pub fn as_str(self) -> &'static str {
        match self {
            SettingsErrorKind::Transport => "transport",
            SettingsErrorKind::NotAuthorized => "not_authorized",
            SettingsErrorKind::Validation => "validation",
            SettingsErrorKind::Unsupported => "unsupported",
            SettingsErrorKind::DaemonNotWritable => "daemon_not_writable",
        }
    }
}

impl std::fmt::Display for SettingsError {
    /// The kind and the message. The kind is what a reader of a log needs
    /// first, because it says what would have to change for the request to
    /// succeed. Neither part can carry a credential: the model holds no secret
    /// value, and the daemon's own message is written not to quote one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind().as_str(), self.message())
    }
}

impl std::error::Error for SettingsError {}

impl SettingsError {
    /// Classify a daemon error frame for `panel`.
    ///
    /// `held` is the capability the daemon last reported for this connection, so
    /// a refusal can say what the caller holds as well as what it needed.
    ///
    /// The `url_*` codes come from the daemon's shared remote-URL policy, which
    /// runs on a settings write. They are a judgement about a *value*, so they
    /// land as [`SettingsError::Validation`] on the panel's URL field rather
    /// than as a technical failure a client would offer to retry.
    ///
    /// NOT every command carries them yet, and this model cannot make it so.
    /// The daemon classifies a refusal into a code on `CreateConnection`,
    /// `UpdateConnection` and `UpsertMcpServer`; `SetEmbeddingsSettings` and the
    /// other settings writes render theirs through `Display`, so a refused base
    /// URL arrives with no `ErrorDetail` at all and a host has nothing to
    /// classify it by ([`Self::from_link_failure`] is then the honest read).
    /// desktop-assistant#972 tracks classifying the rest. Until it lands, the
    /// local check in [`validate_base_url`] is what catches a bad URL for this
    /// panel, and it catches less than the daemon does.
    pub fn from_daemon(
        detail: &ErrorDetail,
        panel: PanelId,
        held: Option<&Capability>,
    ) -> SettingsError {
        match &detail.code {
            ErrorCode::NotAuthorized => SettingsError::NotAuthorized {
                required: panel.write_capability(),
                held: held.cloned(),
                message: detail.message.clone(),
            },
            ErrorCode::Unsupported => SettingsError::Unsupported {
                message: detail.message.clone(),
            },
            ErrorCode::Other(code) if is_url_policy_code(code) => {
                match panel.url_field() {
                    Some(field) => SettingsError::Validation {
                        errors: vec![FieldError {
                            field,
                            kind: ValidationKind::RefusedByDaemon(code.clone()),
                            message: detail.message.clone(),
                        }],
                    },
                    // The panel has no URL field, so the code cannot be about
                    // one of its values. Reporting it as a validation error on
                    // no field would say less than reporting it as it arrived.
                    None => SettingsError::Transport {
                        message: detail.message.clone(),
                        retryable: detail.retryable,
                    },
                }
            }
            _ => SettingsError::Transport {
                message: detail.message.clone(),
                retryable: detail.retryable,
            },
        }
    }

    /// A failure with no daemon verdict: the link broke, or the daemon predates
    /// the structured error detail. Always a transport failure, and retryable -
    /// nothing was learned about the request itself.
    pub fn from_link_failure(message: impl Into<String>) -> SettingsError {
        SettingsError::Transport {
            message: message.into(),
            retryable: true,
        }
    }

    /// The discriminant, for a view that branches on the kind.
    pub fn kind(&self) -> SettingsErrorKind {
        match self {
            SettingsError::Transport { .. } => SettingsErrorKind::Transport,
            SettingsError::NotAuthorized { .. } => SettingsErrorKind::NotAuthorized,
            SettingsError::Validation { .. } => SettingsErrorKind::Validation,
            SettingsError::Unsupported { .. } => SettingsErrorKind::Unsupported,
            SettingsError::DaemonNotWritable { .. } => SettingsErrorKind::DaemonNotWritable,
        }
    }

    /// Whether repeating the identical request could plausibly succeed.
    ///
    /// Only a transport failure ever is. A refusal, a rejected value and a
    /// command the daemon does not implement all answer the same way however
    /// often they are asked.
    pub fn retryable(&self) -> bool {
        match self {
            SettingsError::Transport { retryable, .. } => *retryable,
            SettingsError::NotAuthorized { .. }
            | SettingsError::Validation { .. }
            | SettingsError::Unsupported { .. }
            | SettingsError::DaemonNotWritable { .. } => false,
        }
    }

    /// Text fit to show the person using the client.
    pub fn message(&self) -> String {
        match self {
            SettingsError::Transport { message, .. }
            | SettingsError::NotAuthorized { message, .. }
            | SettingsError::Unsupported { message }
            | SettingsError::DaemonNotWritable { message } => message.clone(),
            SettingsError::Validation { errors } => errors
                .iter()
                .map(|error| error.message.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

/// What a panel needs to know about the daemon it is bound to, beyond the
/// settings values themselves.
///
/// Plain data the host fills in from a `GetConfig` reply (see
/// [`Self::from_config`]) or from whatever it already holds. Keeping it separate
/// from the settings view means a host that read one panel with a dedicated
/// `Get*Settings` command supplies the same context as one that read the whole
/// config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonContext {
    /// Which daemon this is.
    pub instance: InstanceId,
    /// The capability the daemon reported for this connection. `None` means the
    /// daemon reported none, which a panel reads as "this daemon predates the
    /// authorization tier" and not as "no capability" - so it leaves the
    /// controls editable.
    ///
    /// That makes `None` a claim about the daemon, and a host must not write it
    /// to mean "I did not ask". Only `GetConfig` and `ConfigChanged` carry the
    /// capability; a host that read a panel with a dedicated `Get*Settings`
    /// command has to carry the value forward from the last config it saw. A
    /// `None` written for convenience would offer a tenant every control and let
    /// the write fail on submit, which is the defect this model exists to
    /// remove.
    ///
    /// Deliberately the wire field's own shape rather than a third state for
    /// "not read yet": this mirrors [`Config::caller_capability`] one to one, so
    /// the two cannot come to mean different things, and a host that does not
    /// know the capability has a config read available to find out.
    pub caller_capability: Option<Capability>,
    /// The daemon's restart report, verbatim.
    pub restart_required: Vec<String>,
}

impl DaemonContext {
    /// Read the context out of a `GetConfig` reply.
    pub fn from_config(instance: InstanceId, config: &Config) -> DaemonContext {
        DaemonContext {
            instance,
            caller_capability: config.caller_capability.clone(),
            restart_required: config.restart_required.clone(),
        }
    }
}

/// Normalize a value the person typed the same way the daemon does before it
/// stores one: trim it, and read a blank as "clear this setting".
///
/// The daemon's `set_embeddings_settings` trims and drops empty values, so a
/// panel that compared raw text would call a leading space an unapplied change
/// and would send a blank as a value instead of a clear.
pub fn normalize_optional(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Accept a base URL only when it is an absolute `http`/`https` URL with a host.
///
/// This is a courtesy check that never widens what the daemon accepts: the
/// daemon's remote-URL policy additionally refuses cloud-metadata addresses,
/// plain `http` to a public host, and bare hostnames that carry a credential.
/// What this catches is the typo worth catching before a round trip - a missing
/// scheme, and a scheme that is not the web.
///
/// A value this accepts can still be refused. On a command the daemon
/// classifies, that refusal reads as [`ValidationKind::RefusedByDaemon`]; on a
/// settings write it currently arrives unclassified, which is
/// [`SettingsError::from_daemon`]'s note and desktop-assistant#972.
pub fn validate_base_url(value: &str) -> Result<(), ValidationKind> {
    let lower = value.trim().to_ascii_lowercase();
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
        .ok_or(ValidationKind::MalformedUrl)?;
    // A host has to be there and has to end somewhere: the authority runs up to
    // the first `/`, `?` or `#`. `https:///v1` and `https://` both leave it
    // empty, and a space inside it is a typed-in mistake rather than a host.
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return Err(ValidationKind::MalformedUrl);
    }
    Ok(())
}

/// Whether a daemon error code came from its remote-URL policy.
///
/// The codes are the policy's own (`mcp-client::url_policy::UrlPolicyError::code`),
/// which the daemon carries as `ErrorCode::Other` rather than inventing a second
/// classification. Matching them is what turns "the request failed" into "this
/// URL is not acceptable, and here is the field it was typed into".
fn is_url_policy_code(code: &str) -> bool {
    matches!(
        code,
        "url_malformed" | "url_scheme_not_allowed" | "url_insecure_scheme" | "url_target_blocked"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(code: ErrorCode, retryable: bool) -> ErrorDetail {
        ErrorDetail {
            code,
            description: "developer facing".to_string(),
            message: "person facing".to_string(),
            retryable,
        }
    }

    #[test]
    fn config_area_round_trips_the_daemon_key_strings() {
        for (area, key) in [
            (ConfigArea::ConfigLoadFailed, "config_load_failed"),
            (ConfigArea::Database, "database"),
            (ConfigArea::Embeddings, "embeddings"),
            (ConfigArea::WsAuth, "ws_auth"),
            (ConfigArea::Tls, "tls"),
            (ConfigArea::Authz, "authz"),
            (ConfigArea::Recall, "recall"),
        ] {
            assert_eq!(area.as_key(), key);
            assert_eq!(ConfigArea::from_key(key), area);
        }
    }

    #[test]
    fn an_unknown_area_key_is_kept_verbatim_rather_than_dropped() {
        let area = ConfigArea::from_key("some_future_area");
        assert_eq!(area, ConfigArea::Other("some_future_area".to_string()));
        assert_eq!(area.as_key(), "some_future_area");
    }

    #[test]
    fn an_empty_restart_report_leaves_a_panel_in_force() {
        assert_eq!(
            RestartState::for_panel(PanelId::Embeddings, &[]),
            RestartState::InForce
        );
    }

    #[test]
    fn a_panels_own_area_in_the_report_means_a_restart_is_required() {
        let report = ["embeddings".to_string()];
        assert_eq!(
            RestartState::for_panel(PanelId::Embeddings, &report),
            RestartState::RestartRequired
        );
    }

    #[test]
    fn another_panels_area_does_not_mark_this_panel() {
        let report = ["database".to_string(), "tls".to_string()];
        assert_eq!(
            RestartState::for_panel(PanelId::Embeddings, &report),
            RestartState::InForce
        );
    }

    #[test]
    fn config_load_failed_outranks_a_pending_area() {
        let report = ["config_load_failed".to_string(), "embeddings".to_string()];
        assert_eq!(
            RestartState::for_panel(PanelId::Embeddings, &report),
            RestartState::ConfigNotLoaded
        );
    }

    #[test]
    fn config_load_failed_speaks_for_a_panel_with_no_pending_area_of_its_own() {
        let report = ["config_load_failed".to_string()];
        assert_eq!(
            RestartState::for_panel(PanelId::Embeddings, &report),
            RestartState::ConfigNotLoaded
        );
    }

    #[test]
    fn only_a_restart_state_with_something_to_say_carries_a_message() {
        assert_eq!(RestartState::InForce.message(), None);
        assert!(RestartState::RestartRequired.message().is_some());
        assert!(RestartState::ConfigNotLoaded.message().is_some());
    }

    #[test]
    fn an_admin_may_edit_a_panel_that_needs_admin() {
        let editability = Editability::for_panel(
            PanelId::Embeddings,
            Some(&Capability::Admin),
            RestartState::InForce,
        );
        assert_eq!(editability, Editability::Editable);
        assert!(editability.is_editable());
    }

    #[test]
    fn a_tenant_may_not_edit_a_panel_that_needs_admin() {
        let editability = Editability::for_panel(
            PanelId::Embeddings,
            Some(&Capability::Tenant),
            RestartState::InForce,
        );
        assert_eq!(
            editability,
            Editability::ReadOnly(ReadOnlyReason::CapabilityRequired {
                required: Capability::Admin,
                held: Some(Capability::Tenant),
            })
        );
    }

    #[test]
    fn a_daemon_that_reports_no_capability_leaves_the_panel_editable() {
        // The wire contract: an absent capability means the daemon predates the
        // authorization tier and would accept the write.
        assert_eq!(
            Editability::for_panel(PanelId::Embeddings, None, RestartState::InForce),
            Editability::Editable
        );
    }

    #[test]
    fn an_unrecognized_capability_grants_nothing() {
        let held = Capability::Other("owner".to_string());
        assert_eq!(
            Editability::for_panel(PanelId::Embeddings, Some(&held), RestartState::InForce),
            Editability::ReadOnly(ReadOnlyReason::CapabilityRequired {
                required: Capability::Admin,
                held: Some(held),
            })
        );
    }

    #[test]
    fn a_daemon_running_built_in_defaults_is_read_only_even_for_an_admin() {
        assert_eq!(
            Editability::for_panel(
                PanelId::Embeddings,
                Some(&Capability::Admin),
                RestartState::ConfigNotLoaded
            ),
            Editability::ReadOnly(ReadOnlyReason::ConfigNotLoaded)
        );
    }

    #[test]
    fn a_pending_restart_does_not_make_a_panel_read_only() {
        // A restart-required area is a statement about what is live, not about
        // what may be changed: the edit is exactly how it gets fixed.
        assert_eq!(
            Editability::for_panel(
                PanelId::Embeddings,
                Some(&Capability::Admin),
                RestartState::RestartRequired
            ),
            Editability::Editable
        );
    }

    #[test]
    fn every_read_only_reason_says_something() {
        for reason in [
            ReadOnlyReason::Derived,
            ReadOnlyReason::CapabilityRequired {
                required: Capability::Admin,
                held: Some(Capability::Tenant),
            },
            ReadOnlyReason::ConfigNotLoaded,
        ] {
            assert!(!reason.message().is_empty());
        }
    }

    #[test]
    fn a_not_authorized_frame_becomes_an_authorization_refusal() {
        let error = SettingsError::from_daemon(
            &detail(ErrorCode::NotAuthorized, false),
            PanelId::Embeddings,
            Some(&Capability::Tenant),
        );
        assert_eq!(error.kind(), SettingsErrorKind::NotAuthorized);
        assert_eq!(
            error,
            SettingsError::NotAuthorized {
                required: Capability::Admin,
                held: Some(Capability::Tenant),
                message: "person facing".to_string(),
            }
        );
        assert!(!error.retryable());
    }

    #[test]
    fn a_url_policy_code_becomes_a_validation_error_on_the_url_field() {
        for code in [
            "url_malformed",
            "url_scheme_not_allowed",
            "url_insecure_scheme",
            "url_target_blocked",
        ] {
            let error = SettingsError::from_daemon(
                &detail(ErrorCode::Other(code.to_string()), false),
                PanelId::Embeddings,
                Some(&Capability::Admin),
            );
            assert_eq!(error.kind(), SettingsErrorKind::Validation);
            assert_eq!(
                error,
                SettingsError::Validation {
                    errors: vec![FieldError {
                        field: FieldId::BaseUrl,
                        kind: ValidationKind::RefusedByDaemon(code.to_string()),
                        message: "person facing".to_string(),
                    }],
                }
            );
            assert!(!error.retryable());
        }
    }

    #[test]
    fn an_unsupported_frame_becomes_the_unsupported_error() {
        let error = SettingsError::from_daemon(
            &detail(ErrorCode::Unsupported, false),
            PanelId::Embeddings,
            Some(&Capability::Admin),
        );
        assert_eq!(error.kind(), SettingsErrorKind::Unsupported);
        assert!(!error.retryable());
    }

    #[test]
    fn an_unclassified_frame_becomes_a_transport_error_and_keeps_its_retryability() {
        let error = SettingsError::from_daemon(
            &detail(ErrorCode::Other("storage_unavailable".to_string()), true),
            PanelId::Embeddings,
            Some(&Capability::Admin),
        );
        assert_eq!(error.kind(), SettingsErrorKind::Transport);
        assert!(error.retryable());

        let not_found = SettingsError::from_daemon(
            &detail(ErrorCode::NotFound, false),
            PanelId::Embeddings,
            Some(&Capability::Admin),
        );
        assert_eq!(not_found.kind(), SettingsErrorKind::Transport);
        assert!(!not_found.retryable());
    }

    #[test]
    fn a_link_failure_with_no_daemon_verdict_is_a_retryable_transport_error() {
        let error = SettingsError::from_link_failure("connection closed");
        assert_eq!(error.kind(), SettingsErrorKind::Transport);
        assert!(error.retryable());
        assert_eq!(error.message(), "connection closed");
    }

    #[test]
    fn daemon_and_link_failures_map_to_four_distinct_kinds() {
        let kinds = [
            SettingsError::from_daemon(
                &detail(ErrorCode::NotAuthorized, false),
                PanelId::Embeddings,
                None,
            )
            .kind(),
            SettingsError::from_daemon(
                &detail(ErrorCode::Other("url_malformed".to_string()), false),
                PanelId::Embeddings,
                None,
            )
            .kind(),
            SettingsError::from_daemon(
                &detail(ErrorCode::Unsupported, false),
                PanelId::Embeddings,
                None,
            )
            .kind(),
            SettingsError::from_link_failure("gone").kind(),
        ];
        for (i, a) in kinds.iter().enumerate() {
            for b in &kinds[i + 1..] {
                assert_ne!(a, b, "two distinct failures collapsed into one kind");
            }
        }
    }

    #[test]
    fn every_error_kind_says_something() {
        for error in [
            SettingsError::from_daemon(
                &detail(ErrorCode::NotAuthorized, false),
                PanelId::Embeddings,
                None,
            ),
            SettingsError::from_daemon(
                &detail(ErrorCode::Other("url_malformed".to_string()), false),
                PanelId::Embeddings,
                None,
            ),
            SettingsError::from_daemon(
                &detail(ErrorCode::Unsupported, false),
                PanelId::Embeddings,
                None,
            ),
            SettingsError::from_link_failure("gone"),
            SettingsError::DaemonNotWritable {
                message: "repair the config file".to_string(),
            },
        ] {
            assert!(!error.message().is_empty());
            assert!(error.to_string().contains(&error.message()));
        }
    }

    #[test]
    fn normalize_reads_a_blank_value_as_a_clear() {
        assert_eq!(normalize_optional(""), None);
        assert_eq!(normalize_optional("   "), None);
        assert_eq!(normalize_optional("\t\n"), None);
    }

    #[test]
    fn normalize_trims_a_value_it_keeps() {
        assert_eq!(
            normalize_optional("  nomic-embed-text  "),
            Some("nomic-embed-text".to_string())
        );
    }

    #[test]
    fn a_base_url_without_a_scheme_is_rejected() {
        assert_eq!(
            validate_base_url("embeddings.example.com/v1"),
            Err(ValidationKind::MalformedUrl)
        );
    }

    #[test]
    fn a_base_url_whose_scheme_is_not_the_web_is_rejected() {
        for value in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/plain,hi",
            "ws://example.com",
        ] {
            assert_eq!(
                validate_base_url(value),
                Err(ValidationKind::MalformedUrl),
                "{value} should be refused"
            );
        }
    }

    #[test]
    fn a_base_url_with_no_host_after_the_scheme_is_rejected() {
        assert_eq!(
            validate_base_url("https://"),
            Err(ValidationKind::MalformedUrl)
        );
        assert_eq!(
            validate_base_url("https:///v1"),
            Err(ValidationKind::MalformedUrl)
        );
    }

    #[test]
    fn a_base_url_whose_host_contains_a_space_is_rejected() {
        assert_eq!(
            validate_base_url("http://embed .example.com/v1"),
            Err(ValidationKind::MalformedUrl)
        );
    }

    #[test]
    fn an_http_or_https_base_url_with_a_host_is_accepted() {
        for value in [
            "http://127.0.0.1:11434",
            "https://embeddings.example.com/v1",
            "HTTPS://Embeddings.Example.Com/v1",
        ] {
            assert_eq!(
                validate_base_url(value),
                Ok(()),
                "{value} should be accepted"
            );
        }
    }

    #[test]
    fn no_two_field_keys_collide() {
        let ids = [
            FieldId::Connector,
            FieldId::Model,
            FieldId::BaseUrl,
            FieldId::ApiKeyPresent,
            FieldId::Available,
            FieldId::IsDefault,
            FieldId::Health,
        ];
        let mut keys: Vec<&str> = ids.iter().map(|id| id.as_key()).collect();
        keys.sort_unstable();
        let count = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), count, "two fields share a key");
        assert_eq!(FieldId::BaseUrl.as_key(), "base_url");
    }

    #[test]
    fn a_panel_needing_admin_names_admin_as_its_write_capability() {
        assert_eq!(PanelId::Embeddings.write_capability(), Capability::Admin);
        assert_eq!(PanelId::Embeddings.config_area(), ConfigArea::Embeddings);
        assert_eq!(PanelId::Embeddings.url_field(), Some(FieldId::BaseUrl));
    }

    #[test]
    fn an_instance_id_is_carried_verbatim() {
        let id = InstanceId::new("daemon-a");
        assert_eq!(id.as_str(), "daemon-a");
        assert_eq!(id.to_string(), "daemon-a");
        assert_ne!(id, InstanceId::new("daemon-b"));
    }
}
