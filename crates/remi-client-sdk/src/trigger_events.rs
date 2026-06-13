use serde::{Deserialize, Serialize};

/// SDK -> UI event stream for Trigger updates.
///
/// This is designed to support precise, incremental UI updates (e.g. NEXT FIRE)
/// without requiring polling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerEvent {
    /// A trigger fired (either automatically or manually) and the scheduler state advanced.
    TriggerFired {
        trigger_uuid: String,
        fired_at: String,
        next_fire: Option<String>,
        result: bool,
    },

    /// Trigger definition changed / was installed.
    ///
    /// UI should typically refresh its trigger list.
    TriggerUpsert { trigger_uuid: String },

    /// Trigger definition was removed.
    ///
    /// UI should typically refresh its trigger list.
    TriggerDelete { trigger_uuid: String },

    /// All local data has been wiped (logout). UI should clear all state.
    DataWiped,
}
