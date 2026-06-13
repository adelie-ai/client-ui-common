//! The `SelectedModel` (connection, model) pair shared with the model picker.
//!
//! Extracted from adele-gtk's `selected_models.rs` so it can ride inside
//! [`crate::Effect::SetDefaultModel`]. The persistence half (`SelectedModelsStore`,
//! file I/O) stays client-side in adele-gtk, which re-exports this struct.

use serde::{Deserialize, Serialize};

/// Identifies a single (connection, model) pair the user has chosen to keep in
/// the model picker dropdown. The set is filtered client-side so the dropdown
/// stays manageable even when a connector exposes hundreds of models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedModel {
    pub connection_id: String,
    pub model_id: String,
}
