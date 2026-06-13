use serde::{Deserialize, Serialize};

/// SDK -> UI event stream for Event (log) updates.
///
/// This allows the UI to receive precise notifications when new events are recorded,
/// enabling real-time UI updates without polling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventsEvent {
    /// A new event was recorded.
    EventRecorded {
        /// The type of event (e.g., "Connectivity", "Location", "System").
        event_type: String,
        /// ISO8601 timestamp of the event.
        timestamp: String,
    },

    /// All local data has been wiped (logout). UI should clear all state.
    DataWiped,
}
