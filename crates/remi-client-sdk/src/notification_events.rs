use serde::{Deserialize, Serialize};

use crate::types::{NotificationResponseAction, NotificationSource};

/// SDK -> UI event stream for notification updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotificationEvent {
    Added {
        notification_id: i64,
        category: String,
        source: NotificationSource,
        title: String,
    },
    Read {
        notification_id: i64,
    },
    Responded {
        notification_id: i64,
        action: NotificationResponseAction,
    },
    CategoryRead {
        category: String,
    },
    AllRead,
    CategoryDeleted {
        category: String,
    },
    DataWiped,
}
