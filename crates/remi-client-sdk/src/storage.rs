use crate::types::{
    ActionDefinition, ActionInvocationRecord, ActionInvocationSourceKind, AgentVersion,
    AgentVersionUpdate, ChatSession, ChatSessionUpdate, CoordinateSystem, CrdtDocumentRow,
    EvalDataset, EvalDatasetRun, EvalDatasetRunEval, EvalDatasetRunItem, EvalDatasetSession,
    EvalDatasetUpdate, EventPayload, LocationCacheEntry, NotificationEntry, NotificationGroup,
    NotificationResponseAction, NotificationSource, StoredEvent, StoredTrigger,
    ThingsChangeLogEntry, ThingsContentSnapshot, ThingsOperationType, TriggerInfo, TriggerLogEntry,
    TriggerLogLevel, TriggerRegistration, TriggerRunType,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, TimeZone, Utc};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

const CRDT_DOCUMENTS_REVISION_KEY: &str = "crdt_documents_revision";

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(test)]
static TEST_DB_OPEN_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_THINGS_STATE_GET: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_THINGS_STATE_SAVE: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_THINGS_CHANGE_LOG_INSERT: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_THINGS_CHANGE_LOG_FIND_RECENT: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_CRDT_DOCUMENT_SAVE: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TestSqliteCounters {
    pub open_connections: usize,
    pub things_state_get: usize,
    pub things_state_save: usize,
    pub change_log_insert: usize,
    pub change_log_find_recent: usize,
    pub crdt_document_save: usize,
}

#[cfg(test)]
pub(crate) fn test_sqlite_counters_reset() {
    TEST_DB_OPEN_CONNECTIONS.store(0, Ordering::Relaxed);
    TEST_THINGS_STATE_GET.store(0, Ordering::Relaxed);
    TEST_THINGS_STATE_SAVE.store(0, Ordering::Relaxed);
    TEST_THINGS_CHANGE_LOG_INSERT.store(0, Ordering::Relaxed);
    TEST_THINGS_CHANGE_LOG_FIND_RECENT.store(0, Ordering::Relaxed);
    TEST_CRDT_DOCUMENT_SAVE.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn test_sqlite_counters_get() -> TestSqliteCounters {
    TestSqliteCounters {
        open_connections: TEST_DB_OPEN_CONNECTIONS.load(Ordering::Relaxed),
        things_state_get: TEST_THINGS_STATE_GET.load(Ordering::Relaxed),
        things_state_save: TEST_THINGS_STATE_SAVE.load(Ordering::Relaxed),
        change_log_insert: TEST_THINGS_CHANGE_LOG_INSERT.load(Ordering::Relaxed),
        change_log_find_recent: TEST_THINGS_CHANGE_LOG_FIND_RECENT.load(Ordering::Relaxed),
        crdt_document_save: TEST_CRDT_DOCUMENT_SAVE.load(Ordering::Relaxed),
    }
}

#[derive(Clone)]
pub struct Storage {
    db_path: PathBuf,
}

impl Storage {
    fn bump_internal_kv_counter_tx(tx: &rusqlite::Transaction<'_>, key: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        tx.execute(
            r#"INSERT INTO internal_kv (key, value, updated_at)
               VALUES (?1, '1', ?2)
               ON CONFLICT(key) DO UPDATE SET
                 value = CAST(CAST(COALESCE(NULLIF(internal_kv.value, ''), '0') AS INTEGER) + 1 AS TEXT),
                 updated_at = excluded.updated_at"#,
            params![key, now],
        )
        .with_context(|| format!("Failed to bump internal_kv counter for key '{key}'"))?;
        Ok(())
    }

    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create db directory {}", parent.display())
                })?;
            }
        }

        let storage = Self { db_path: path };
        let conn = storage.connection()?;
        storage.bootstrap(&conn)?;
        Ok(storage)
    }

    pub(crate) fn cache_namespace(&self) -> String {
        self.db_path.to_string_lossy().into_owned()
    }

    fn connection(&self) -> Result<Connection> {
        use std::time::Instant;

        #[cfg(test)]
        TEST_DB_OPEN_CONNECTIONS.fetch_add(1, Ordering::Relaxed);

        let t0 = Instant::now();
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("Failed to open SQLite db {}", self.db_path.display()))?;
        let open_ms = t0.elapsed().as_millis();

        let t1 = Instant::now();
        conn.pragma_update(None, "journal_mode", &"WAL")
            .context("Failed to enable WAL mode")?;
        let pragma_ms = t1.elapsed().as_millis();

        tracing::debug!(
            path = %self.db_path.display(),
            open_ms,
            pragma_ms,
            "sqlite: open connection"
        );
        Ok(conn)
    }

    fn bootstrap(&self, conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS triggers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                trigger_uuid TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                version TEXT NOT NULL DEFAULT 'v1',
                precondition_json TEXT NOT NULL,
                condition_json TEXT NOT NULL,
                action_uuid TEXT,
                action_args_json TEXT NOT NULL DEFAULT '{}',
                next_fire_utc INTEGER,
                last_result INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS actions (
                action_uuid TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                title TEXT NOT NULL,
                description TEXT NOT NULL,
                version TEXT NOT NULL,
                category TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                metadata_json TEXT NOT NULL,
                script_source TEXT NOT NULL,
                input_schema_json TEXT NOT NULL,
                output_schema_json TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS action_invocations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                invocation_uuid TEXT NOT NULL UNIQUE,
                action_uuid TEXT NOT NULL,
                source_kind TEXT NOT NULL,
                source_entity_type TEXT,
                source_entity_uuid TEXT,
                args_json TEXT NOT NULL,
                result_json TEXT,
                console_logs_json TEXT NOT NULL,
                error_json TEXT,
                started_at INTEGER NOT NULL,
                finished_at INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                device_id TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_action_invocations_action_time
                ON action_invocations(action_uuid, started_at DESC);

            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_type TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                timestamp_utc INTEGER NOT NULL,
                metadata_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_triggers_next_fire ON triggers(next_fire_utc);
            CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp_utc DESC);

            CREATE TABLE IF NOT EXISTS preferences (
                key TEXT PRIMARY KEY,
                display_name TEXT,
                description TEXT,
                value_type TEXT NOT NULL,
                value_json TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_preferences_updated ON preferences(updated_at DESC);

            CREATE TABLE IF NOT EXISTS preference_sync_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                automerge_doc BLOB NOT NULL,
                last_sync_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS things_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                automerge_doc BLOB NOT NULL,
                sync_state BLOB NOT NULL,
                dirty INTEGER NOT NULL DEFAULT 0,
                last_sync_at TEXT
            );

            CREATE TABLE IF NOT EXISTS trigger_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                trigger_uuid TEXT NOT NULL,
                level TEXT NOT NULL,
                message TEXT NOT NULL,
                fire_time_utc INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                run_type TEXT NOT NULL DEFAULT 'automatic'
            );

            CREATE INDEX IF NOT EXISTS idx_trigger_logs_uuid_time
                ON trigger_logs(trigger_uuid, created_at DESC);

            CREATE TABLE IF NOT EXISTS trigger_bindings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                trigger_uuid TEXT NOT NULL,
                entity_type TEXT NOT NULL,
                entity_uuid TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                UNIQUE(entity_type, entity_uuid)
            );

            CREATE INDEX IF NOT EXISTS idx_trigger_bindings_trigger
                ON trigger_bindings(trigger_uuid);
            CREATE INDEX IF NOT EXISTS idx_trigger_bindings_entity
                ON trigger_bindings(entity_type, entity_uuid);

            CREATE TABLE IF NOT EXISTS chat_sessions (
                session_id TEXT PRIMARY KEY,
                title TEXT,
                created_at INTEGER NOT NULL,
                last_activity INTEGER NOT NULL,
                message_count INTEGER NOT NULL DEFAULT 0
            );

            -- Internal KV store for one-off migrations / bootstrap flows.
            CREATE TABLE IF NOT EXISTS internal_kv (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS chat_messages (
                session_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                message_json TEXT NOT NULL,
                PRIMARY KEY(session_id, message_id)
            );

            CREATE TABLE IF NOT EXISTS chat_runtime_state (
                session_id TEXT PRIMARY KEY,
                state_json TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS chat_model_inputs (
                session_id TEXT NOT NULL,
                run_id TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                snapshot_json TEXT NOT NULL,
                PRIMARY KEY(session_id, run_id)
            );

            CREATE TABLE IF NOT EXISTS agent_versions (
                version_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                name TEXT NOT NULL,
                raw_markdown TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                applied_at INTEGER,
                user_id TEXT DEFAULT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_agent_versions_agent_updated
                ON agent_versions(agent_id, updated_at DESC);

            CREATE TABLE IF NOT EXISTS eval_datasets (
                dataset_id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                user_id TEXT DEFAULT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_eval_datasets_updated
                ON eval_datasets(updated_at DESC);

            CREATE TABLE IF NOT EXISTS eval_dataset_sessions (
                dataset_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                added_at INTEGER NOT NULL,
                user_id TEXT DEFAULT NULL,
                PRIMARY KEY(dataset_id, session_id),
                FOREIGN KEY (dataset_id) REFERENCES eval_datasets(dataset_id) ON DELETE CASCADE,
                FOREIGN KEY (session_id) REFERENCES chat_sessions(session_id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_eval_dataset_sessions_dataset_added
                ON eval_dataset_sessions(dataset_id, added_at DESC);

            CREATE TABLE IF NOT EXISTS eval_dataset_runs (
                run_id TEXT PRIMARY KEY,
                dataset_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                agent_version_id TEXT,
                agent_version_name TEXT,
                variant_id TEXT NOT NULL,
                variant_label TEXT NOT NULL,
                source_session_count INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                user_id TEXT DEFAULT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_eval_dataset_runs_dataset_created
                ON eval_dataset_runs(dataset_id, created_at DESC);

            CREATE TABLE IF NOT EXISTS eval_dataset_run_items (
                run_id TEXT NOT NULL,
                dataset_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                final_text TEXT NOT NULL,
                reasoning TEXT,
                prompt_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL,
                tool_results_json TEXT NOT NULL,
                done INTEGER NOT NULL DEFAULT 0,
                cancelled INTEGER NOT NULL DEFAULT 0,
                interrupted INTEGER NOT NULL DEFAULT 0,
                error TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                user_id TEXT DEFAULT NULL,
                PRIMARY KEY(run_id, session_id)
            );

            CREATE INDEX IF NOT EXISTS idx_eval_dataset_run_items_run
                ON eval_dataset_run_items(run_id, created_at ASC);

            CREATE TABLE IF NOT EXISTS eval_dataset_run_evals (
                run_id TEXT NOT NULL,
                dataset_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                analysis_agent_id TEXT NOT NULL,
                score TEXT NOT NULL,
                summary TEXT NOT NULL,
                rationale TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                user_id TEXT DEFAULT NULL,
                PRIMARY KEY(run_id, session_id)
            );

            CREATE INDEX IF NOT EXISTS idx_eval_dataset_run_evals_run
                ON eval_dataset_run_evals(run_id, created_at ASC);

            CREATE INDEX IF NOT EXISTS idx_chat_sessions_last_activity
                ON chat_sessions(last_activity DESC);

            CREATE INDEX IF NOT EXISTS idx_chat_messages_session_time
                ON chat_messages(session_id, created_at_ms ASC);

            CREATE INDEX IF NOT EXISTS idx_chat_model_inputs_session_time
                ON chat_model_inputs(session_id, created_at_ms DESC);

            -- Things change log for tracking operations and enabling undo
            CREATE TABLE IF NOT EXISTS things_change_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                device_id TEXT NOT NULL,
                op_type TEXT NOT NULL,
                entity_type TEXT NOT NULL,
                entity_uuid TEXT NOT NULL,
                summary TEXT NOT NULL,
                details_json TEXT NOT NULL,
                parent_log_id INTEGER,
                cascade_log_ids_json TEXT,
                created_at INTEGER NOT NULL,
                can_undo INTEGER NOT NULL DEFAULT 1,
                synced INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY (parent_log_id) REFERENCES things_change_log(id)
            );

            CREATE INDEX IF NOT EXISTS idx_things_change_log_entity
                ON things_change_log(entity_type, entity_uuid);
            CREATE INDEX IF NOT EXISTS idx_things_change_log_created
                ON things_change_log(created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_things_change_log_parent
                ON things_change_log(parent_log_id);

            -- Content snapshots for edit operations (versioning)
            CREATE TABLE IF NOT EXISTS things_content_snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                device_id TEXT NOT NULL,
                thing_uuid TEXT NOT NULL,
                content_json TEXT NOT NULL,
                change_log_id INTEGER,
                created_at INTEGER NOT NULL,
                synced INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY (change_log_id) REFERENCES things_change_log(id)
            );

            CREATE INDEX IF NOT EXISTS idx_things_content_snapshots_thing
                ON things_content_snapshots(thing_uuid, created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_things_content_snapshots_log
                ON things_content_snapshots(change_log_id);

            -- Location cache for geocoding results
            CREATE TABLE IF NOT EXISTS location_cache (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                is_fuzzy INTEGER NOT NULL DEFAULT 0,
                latitude REAL,
                longitude REAL,
                coord_system TEXT NOT NULL DEFAULT 'wgs84',
                place_id TEXT,
                place_type TEXT,
                formatted_address TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_location_cache_name ON location_cache(name);

            -- V3 Multi-document CRDT storage (replaces things_state for v3)
            CREATE TABLE IF NOT EXISTS crdt_documents (
                uuid TEXT NOT NULL,
                data_type TEXT NOT NULL,
                automerge_doc BLOB NOT NULL,
                sync_state BLOB NOT NULL,
                dirty INTEGER NOT NULL DEFAULT 0,
                last_sync_at TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (uuid, data_type)
            );

            CREATE INDEX IF NOT EXISTS idx_crdt_documents_dirty
                ON crdt_documents(dirty, data_type);
            CREATE INDEX IF NOT EXISTS idx_crdt_documents_type
                ON crdt_documents(data_type);

            -- Notifications (aggregated by category / trigger_uuid)
            CREATE TABLE IF NOT EXISTS notifications (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source TEXT NOT NULL DEFAULT 'trigger',
                category TEXT NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                is_read INTEGER NOT NULL DEFAULT 0,
                response_action TEXT,
                responded_at INTEGER,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_notifications_category
                ON notifications(category, created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_notifications_unread
                ON notifications(is_read, created_at DESC);

            -- Actor attribution cache (synced from server after CRDT sync)
            CREATE TABLE IF NOT EXISTS things_actor_meta (
                uuid TEXT NOT NULL PRIMARY KEY,
                is_collection INTEGER NOT NULL DEFAULT 0,
                actor_type TEXT NOT NULL DEFAULT 'user',
                actor_app_id TEXT,
                actor_display_name TEXT,
                updated_at INTEGER NOT NULL
            );
            "#,
        )
        .context("Failed to run migrations")?;

        // Best-effort schema upgrade for deployments created before newer columns existed.
        let _ = conn.execute(
            "ALTER TABLE triggers ADD COLUMN version TEXT NOT NULL DEFAULT 'v1'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE triggers ADD COLUMN precondition_json TEXT NOT NULL DEFAULT '{}'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE triggers ADD COLUMN condition_json TEXT NOT NULL DEFAULT '{}'",
            [],
        );
        let _ = conn.execute("ALTER TABLE triggers ADD COLUMN action_uuid TEXT", []);
        let _ = conn.execute(
            "ALTER TABLE triggers ADD COLUMN action_args_json TEXT NOT NULL DEFAULT '{}'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE triggers ADD COLUMN api_version TEXT NOT NULL DEFAULT 'unknown'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE trigger_logs ADD COLUMN run_type TEXT NOT NULL DEFAULT 'automatic'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE triggers ADD COLUMN is_paused INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE notifications ADD COLUMN response_action TEXT",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE notifications ADD COLUMN responded_at INTEGER",
            [],
        );

        // Migrations for things_change_log
        let _ = conn.execute(
            "ALTER TABLE things_change_log ADD COLUMN device_id TEXT NOT NULL DEFAULT 'unknown'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE things_change_log ADD COLUMN synced INTEGER NOT NULL DEFAULT 0",
            [],
        );

        // Migrations for things_content_snapshots
        let _ = conn.execute(
            "ALTER TABLE things_content_snapshots ADD COLUMN device_id TEXT NOT NULL DEFAULT 'unknown'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE things_content_snapshots ADD COLUMN synced INTEGER NOT NULL DEFAULT 0",
            [],
        );

        // ========== User ownership columns ==========
        // Add user_id to all user-scoped tables for anonymous-to-authenticated data migration.
        // NULL means data was created while not logged in (anonymous).
        let user_id_tables = &[
            "triggers",
            "actions",
            "action_invocations",
            "events",
            "preferences",
            "things_state",
            "trigger_logs",
            "trigger_bindings",
            "chat_sessions",
            "chat_messages",
            "chat_model_inputs",
            "things_change_log",
            "things_content_snapshots",
            "crdt_documents",
            "internal_kv",
            "notifications",
        ];
        for table in user_id_tables {
            let _ = conn.execute(
                &format!("ALTER TABLE {} ADD COLUMN user_id TEXT DEFAULT NULL", table),
                [],
            );
        }

        let _ = conn.execute(
            "ALTER TABLE agent_versions ADD COLUMN user_id TEXT DEFAULT NULL",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE eval_datasets ADD COLUMN user_id TEXT DEFAULT NULL",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE eval_dataset_sessions ADD COLUMN user_id TEXT DEFAULT NULL",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE eval_dataset_runs ADD COLUMN user_id TEXT DEFAULT NULL",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE eval_dataset_run_items ADD COLUMN user_id TEXT DEFAULT NULL",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE eval_dataset_run_evals ADD COLUMN user_id TEXT DEFAULT NULL",
            [],
        );

        Ok(())
    }

    // ========== Data ownership lifecycle ==========

    /// Claim all anonymous (user_id IS NULL) data for the given user.
    /// Called after a successful login to attribute locally-created data to the authenticated user.
    pub fn claim_anonymous_data(&self, user_id: &str) -> Result<usize> {
        let conn = self.connection()?;
        let tables = &[
            "triggers",
            "events",
            "preferences",
            "things_state",
            "trigger_logs",
            "trigger_bindings",
            "chat_sessions",
            "chat_messages",
            "chat_model_inputs",
            "agent_versions",
            "eval_datasets",
            "eval_dataset_sessions",
            "eval_dataset_runs",
            "eval_dataset_run_items",
            "eval_dataset_run_evals",
            "things_change_log",
            "things_content_snapshots",
            "crdt_documents",
            "internal_kv",
            "notifications",
        ];
        let mut total_claimed = 0usize;
        for table in tables {
            let affected = conn
                .execute(
                    &format!("UPDATE {} SET user_id = ?1 WHERE user_id IS NULL", table),
                    params![user_id],
                )
                .with_context(|| format!("Failed to claim anonymous data in {}", table))?;
            total_claimed += affected;
        }
        tracing::info!(
            user_id = %user_id,
            total_claimed,
            "Claimed anonymous data for user"
        );
        Ok(total_claimed)
    }

    /// Wipe all user data from the local database.
    /// Called on logout to reset the device to a clean state.
    pub fn wipe_all_data(&self) -> Result<()> {
        let conn = self.connection()?;
        // Order matters for foreign key constraints (children before parents)
        let tables = &[
            "action_invocations",
            "actions",
            "chat_messages",
            "chat_sessions",
            "things_content_snapshots",
            "things_change_log",
            "crdt_documents",
            "trigger_logs",
            "trigger_bindings",
            "triggers",
            "events",
            "preferences",
            "preference_sync_state",
            "things_state",
            "location_cache",
            "internal_kv",
            "things_actor_meta",
        ];
        for table in tables {
            conn.execute(&format!("DELETE FROM {}", table), [])
                .with_context(|| format!("Failed to wipe table {}", table))?;
        }
        tracing::info!("Wiped all local data (logout cleanup)");
        Ok(())
    }

    /// Get the database file path
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn insert_trigger(
        &self,
        params: TriggerRegistration,
        next_fire: Option<DateTime<Utc>>,
    ) -> Result<String> {
        let TriggerRegistration {
            trigger_uuid,
            name,
            version,
            precondition,
            condition,
            action_uuid,
            action_args,
        } = params;

        let precondition_json =
            serde_json::to_string(&precondition).context("Failed to serialize precondition")?;
        let condition_json =
            serde_json::to_string(&condition).context("Failed to serialize condition")?;
        let action_args_json =
            serde_json::to_string(&action_args).context("Failed to serialize action args")?;

        let now = Utc::now().timestamp();
        let next_fire_ts = next_fire.map(|dt| dt.timestamp());
        // Cron registration id: stable trigger uuid + computed next expected execution time.
        let cron_registration_id = format!("{}:{}", trigger_uuid, next_fire_ts.unwrap_or(0));

        let conn = self.connection()?;
        conn.execute(
            r#"INSERT INTO triggers
            (trigger_uuid, name, version, precondition_json, condition_json, action_uuid, action_args_json, next_fire_utc, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
            ON CONFLICT(trigger_uuid) DO UPDATE SET
                name = excluded.name,
                version = excluded.version,
                precondition_json = excluded.precondition_json,
                condition_json = excluded.condition_json,
                action_uuid = excluded.action_uuid,
                action_args_json = excluded.action_args_json,
                next_fire_utc = excluded.next_fire_utc,
                last_result = NULL,
                updated_at = excluded.updated_at
            "#,
            params![
                trigger_uuid,
                name,
                version,
                precondition_json,
                condition_json,
                action_uuid,
                action_args_json,
                next_fire_ts,
                now
            ],
        )
        .context("Failed to insert trigger")?;

        Ok(cron_registration_id)
    }

    pub fn update_next_fire(
        &self,
        trigger_uuid: &str,
        fired_at: DateTime<Utc>,
        next_fire: Option<DateTime<Utc>>,
        last_result: bool,
    ) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            r#"UPDATE triggers SET
                next_fire_utc = ?1,
                last_result = ?2,
                updated_at = ?3
              WHERE trigger_uuid = ?4"#,
            params![
                next_fire.map(|dt| dt.timestamp()),
                if last_result { 1 } else { 0 },
                fired_at.timestamp(),
                trigger_uuid
            ],
        )
        .context("Failed to update trigger schedule")?;
        Ok(())
    }

    /// Pause or resume a trigger. Paused triggers are skipped by the scheduler.
    pub fn set_trigger_paused(&self, trigger_uuid: &str, paused: bool) -> Result<()> {
        let conn = self.connection()?;
        let now = Utc::now().timestamp();
        conn.execute(
            "UPDATE triggers SET is_paused = ?1, updated_at = ?2 WHERE trigger_uuid = ?3",
            params![if paused { 1 } else { 0 }, now, trigger_uuid],
        )
        .context("Failed to set trigger paused state")?;
        Ok(())
    }

    pub fn get_internal_kv(&self, key: &str) -> Result<Option<String>> {
        let conn = self.connection()?;
        let value = conn
            .query_row(
                "SELECT value FROM internal_kv WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(value)
    }

    pub fn set_internal_kv(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.connection()?;
        let now = Utc::now().timestamp();
        conn.execute(
            r#"INSERT INTO internal_kv (key, value, updated_at)
               VALUES (?1, ?2, ?3)
               ON CONFLICT(key) DO UPDATE SET
                 value = excluded.value,
                 updated_at = excluded.updated_at"#,
            params![key, value, now],
        )?;
        Ok(())
    }

    pub fn get_crdt_documents_revision(&self) -> Result<u64> {
        match self.get_internal_kv(CRDT_DOCUMENTS_REVISION_KEY)? {
            Some(value) => value
                .parse::<u64>()
                .with_context(|| format!("Invalid CRDT documents revision value: {value}")),
            None => Ok(0),
        }
    }

    pub fn delete_internal_kv(&self, key: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute("DELETE FROM internal_kv WHERE key = ?1", params![key])?;
        Ok(())
    }

    pub fn save_things_state_and_clear_kv(
        &self,
        doc: &[u8],
        sync_state: &[u8],
        dirty: bool,
        last_sync_at: Option<&str>,
        kv_keys_to_delete: &[&str],
    ) -> Result<()> {
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;

        tx.execute(
            r#"INSERT INTO things_state (id, automerge_doc, sync_state, dirty, last_sync_at)
               VALUES (1, ?1, ?2, ?3, ?4)
               ON CONFLICT(id) DO UPDATE SET
                   automerge_doc = excluded.automerge_doc,
                   sync_state = excluded.sync_state,
                   dirty = excluded.dirty,
                   last_sync_at = excluded.last_sync_at"#,
            params![doc, sync_state, if dirty { 1 } else { 0 }, last_sync_at],
        )?;

        for key in kv_keys_to_delete {
            tx.execute("DELETE FROM internal_kv WHERE key = ?1", params![key])?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn fetch_due_triggers(&self, now: DateTime<Utc>) -> Result<Vec<StoredTrigger>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
                        r#"SELECT trigger_uuid, name, version, precondition_json, condition_json, action_uuid, action_args_json, next_fire_utc
               FROM triggers
               WHERE next_fire_utc IS NOT NULL AND next_fire_utc <= ?1
                 AND (is_paused = 0 OR is_paused IS NULL)
               ORDER BY next_fire_utc ASC"#,
        )?;
        let rows = stmt
            .query_map([now.timestamp()], |row| {
                let next_fire = row
                    .get::<_, Option<i64>>(7)?
                    .and_then(|ts| Utc.timestamp_opt(ts, 0).single());
                Ok(StoredTrigger {
                    trigger_uuid: row.get(0)?,
                    name: row.get(1)?,
                    version: row.get(2)?,
                    precondition_json: row.get(3)?,
                    condition_json: row.get(4)?,
                    action_uuid: row.get(5)?,
                    action_args_json: row.get(6)?,
                    next_fire,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to read due triggers")?;
        Ok(rows)
    }

    pub fn fetch_trigger(&self, trigger_uuid: &str) -> Result<Option<StoredTrigger>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT trigger_uuid, name, version, precondition_json, condition_json, action_uuid, action_args_json, next_fire_utc
               FROM triggers
               WHERE trigger_uuid = ?1
               LIMIT 1"#,
        )?;
        let trigger = stmt
            .query_row([trigger_uuid], |row| {
                let next_fire = row
                    .get::<_, Option<i64>>(7)?
                    .and_then(|ts| Utc.timestamp_opt(ts, 0).single());
                Ok(StoredTrigger {
                    trigger_uuid: row.get(0)?,
                    name: row.get(1)?,
                    version: row.get(2)?,
                    precondition_json: row.get(3)?,
                    condition_json: row.get(4)?,
                    action_uuid: row.get(5)?,
                    action_args_json: row.get(6)?,
                    next_fire,
                })
            })
            .optional()
            .context("Failed to fetch trigger by uuid")?;
        Ok(trigger)
    }

    pub fn insert_event(&self, event: &EventPayload) -> Result<()> {
        let conn = self.connection()?;
        let metadata_json = serde_json::to_string(&event.metadata).unwrap_or_else(|_| "{}".into());
        conn.execute(
            r#"INSERT INTO events
            (event_type, timestamp, timestamp_utc, metadata_json, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5)"#,
            params![
                event.event_type,
                event.timestamp.to_rfc3339(),
                event.timestamp.timestamp(),
                metadata_json,
                Utc::now().timestamp()
            ],
        )
        .context("Failed to insert event")?;
        Ok(())
    }

    pub fn mark_trigger_due(&self, trigger_uuid: &str, due_at: DateTime<Utc>) -> Result<()> {
        let conn = self.connection()?;
        let now = Utc::now().timestamp();
        conn.execute(
            r#"UPDATE triggers SET
                next_fire_utc = ?1,
                updated_at = ?2
              WHERE trigger_uuid = ?3"#,
            params![due_at.timestamp(), now, trigger_uuid],
        )
        .context("Failed to mark trigger due")?;
        Ok(())
    }

    pub fn insert_trigger_log(
        &self,
        trigger_uuid: &str,
        level: TriggerLogLevel,
        message: &str,
        fire_time: DateTime<Utc>,
        run_type: TriggerRunType,
    ) -> Result<()> {
        let conn = self.connection()?;
        let now = Utc::now().timestamp();
        conn.execute(
            r#"INSERT INTO trigger_logs
            (trigger_uuid, level, message, fire_time_utc, created_at, run_type)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
            params![
                trigger_uuid,
                level.as_str(),
                message,
                fire_time.timestamp(),
                now,
                run_type.as_str()
            ],
        )
        .context("Failed to insert trigger log")?;
        Ok(())
    }

    pub fn fetch_events_recent(
        &self,
        cutoff: DateTime<Utc>,
        past_minutes: u32,
    ) -> Result<Vec<StoredEvent>> {
        let window_start = cutoff - Duration::minutes(i64::from(past_minutes));

        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT event_type, timestamp, metadata_json
               FROM events
               WHERE timestamp_utc BETWEEN ?1 AND ?2
               ORDER BY timestamp_utc DESC"#,
        )?;

        let events = stmt
            .query_map(
                params![window_start.timestamp(), cutoff.timestamp()],
                |row| Self::map_event_row(row),
            )?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to fetch recent events")?;

        Ok(events)
    }

    pub fn next_deadline(&self, now_unix: Option<i64>) -> Result<Option<DateTime<Utc>>> {
        let conn = self.connection()?;
        let ts = match now_unix {
            Some(now) => conn
                .query_row(
                    "SELECT next_fire_utc FROM triggers WHERE next_fire_utc IS NOT NULL AND next_fire_utc > ?1 ORDER BY next_fire_utc ASC LIMIT 1",
                    params![now],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .context("Failed to read next deadline")?,
            None => conn
                .query_row(
                    "SELECT next_fire_utc FROM triggers WHERE next_fire_utc IS NOT NULL ORDER BY next_fire_utc ASC LIMIT 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .context("Failed to read next deadline")?,
        };
        Ok(ts.and_then(|val| Utc.timestamp_opt(val, 0).single()))
    }

    pub fn list_events(&self, limit: Option<u32>, offset: u32) -> Result<Vec<StoredEvent>> {
        let conn = self.connection()?;
        let base_query =
            "SELECT event_type, timestamp, metadata_json FROM events ORDER BY timestamp_utc ASC";

        let rows = if let Some(limit) = limit.filter(|value| *value > 0) {
            let mut stmt = conn.prepare(&format!("{base_query} LIMIT ? OFFSET ?"))?;
            stmt.query_map(params![limit as i64, offset as i64], |row| {
                Self::map_event_row(row)
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to fetch limited events")?
        } else {
            let mut stmt = conn.prepare(base_query)?;
            stmt.query_map([], |row| Self::map_event_row(row))?
                .collect::<Result<Vec<_>, _>>()
                .context("Failed to fetch events")?
        };

        Ok(rows)
    }

    pub fn list_events_between_utc(
        &self,
        start_ts_utc: i64,
        end_ts_utc: i64,
    ) -> Result<Vec<StoredEvent>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT event_type, timestamp, metadata_json
               FROM events
               WHERE timestamp_utc BETWEEN ?1 AND ?2
               ORDER BY timestamp_utc ASC"#,
        )?;

        let rows = stmt
            .query_map(params![start_ts_utc, end_ts_utc], |row| {
                Self::map_event_row(row)
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to fetch events in range")?;

        Ok(rows)
    }

    pub fn list_trigger_logs(
        &self,
        trigger_uuid: &str,
        limit: Option<u32>,
        run_type: Option<TriggerRunType>,
    ) -> Result<Vec<TriggerLogEntry>> {
        let conn = self.connection()?;
        let limited = limit.filter(|v| *v > 0);

        let rows = match (run_type, limited) {
            (Some(run_type), Some(limit)) => {
                let mut stmt = conn.prepare(
                    "SELECT trigger_uuid, level, message, fire_time_utc, created_at, run_type
                     FROM trigger_logs
                     WHERE trigger_uuid = ?1 AND run_type = ?2
                     ORDER BY created_at DESC
                     LIMIT ?3",
                )?;
                stmt.query_map(
                    params![trigger_uuid, run_type.as_str(), limit as i64],
                    |row| Self::map_trigger_log_row(row),
                )?
                .collect::<Result<Vec<_>, _>>()
                .context("Failed to fetch filtered trigger logs")?
            }
            (Some(run_type), None) => {
                let mut stmt = conn.prepare(
                    "SELECT trigger_uuid, level, message, fire_time_utc, created_at, run_type
                     FROM trigger_logs
                     WHERE trigger_uuid = ?1 AND run_type = ?2
                     ORDER BY created_at DESC",
                )?;
                stmt.query_map(params![trigger_uuid, run_type.as_str()], |row| {
                    Self::map_trigger_log_row(row)
                })?
                .collect::<Result<Vec<_>, _>>()
                .context("Failed to fetch filtered trigger logs")?
            }
            (None, Some(limit)) => {
                let mut stmt = conn.prepare(
                    "SELECT trigger_uuid, level, message, fire_time_utc, created_at, run_type
                     FROM trigger_logs
                     WHERE trigger_uuid = ?1
                     ORDER BY created_at DESC
                     LIMIT ?2",
                )?;
                stmt.query_map(params![trigger_uuid, limit as i64], |row| {
                    Self::map_trigger_log_row(row)
                })?
                .collect::<Result<Vec<_>, _>>()
                .context("Failed to fetch limited trigger logs")?
            }
            (None, None) => {
                let mut stmt = conn.prepare(
                    "SELECT trigger_uuid, level, message, fire_time_utc, created_at, run_type
                     FROM trigger_logs
                     WHERE trigger_uuid = ?1
                     ORDER BY created_at DESC",
                )?;
                stmt.query_map([trigger_uuid], |row| Self::map_trigger_log_row(row))?
                    .collect::<Result<Vec<_>, _>>()
                    .context("Failed to fetch trigger logs")?
            }
        };

        Ok(rows)
    }

    pub fn export_trigger_logs(
        &self,
        trigger_uuid: &str,
        run_type: Option<TriggerRunType>,
    ) -> Result<Vec<TriggerLogEntry>> {
        self.list_trigger_logs(trigger_uuid, None, run_type)
    }

    // ===== Notification CRUD =====

    pub fn insert_notification(
        &self,
        source: &NotificationSource,
        category: &str,
        title: &str,
        body: &str,
    ) -> Result<i64> {
        let conn = self.connection()?;
        let now = Utc::now().timestamp();
        conn.execute(
            r#"INSERT INTO notifications (source, category, title, body, is_read, created_at)
               VALUES (?1, ?2, ?3, ?4, 0, ?5)"#,
            params![source.as_str(), category, title, body, now],
        )
        .context("Failed to insert notification")?;
        Ok(conn.last_insert_rowid())
    }

    pub fn record_notification_response(
        &self,
        notification_id: i64,
        action: &NotificationResponseAction,
    ) -> Result<()> {
        let conn = self.connection()?;
        let responded_at = Utc::now().timestamp();
        conn.execute(
            r#"UPDATE notifications
               SET response_action = ?1,
                   responded_at = ?2,
                   is_read = 1
               WHERE id = ?3"#,
            params![action.as_str(), responded_at, notification_id],
        )
        .context("Failed to record notification response")?;
        Ok(())
    }

    /// List notifications grouped by category, ordered by latest first.
    pub fn list_notifications_grouped(&self, limit: u32) -> Result<Vec<NotificationGroup>> {
        let conn = self.connection()?;
        // First get distinct categories ordered by latest notification
        let mut cat_stmt = conn.prepare(
            r#"SELECT category, title, source,
                      MAX(created_at) as latest_at,
                      SUM(CASE WHEN is_read = 0 THEN 1 ELSE 0 END) as unread_count,
                      COUNT(*) as total_count
               FROM notifications
               GROUP BY category
               ORDER BY latest_at DESC
               LIMIT ?1"#,
        )?;

        let groups: Vec<(String, String, String, i64, i64, i64)> = cat_stmt
            .query_map(params![limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to list notification groups")?;

        let mut result = Vec::with_capacity(groups.len());
        for (category, title, source_str, latest_ts, unread, total) in groups {
            let source =
                NotificationSource::from_str(&source_str).unwrap_or(NotificationSource::Trigger);
            let latest_at = Utc
                .timestamp_opt(latest_ts, 0)
                .single()
                .unwrap_or_else(Utc::now);
            let items = self.list_notifications_by_category(&category, limit)?;
            result.push(NotificationGroup {
                category,
                title,
                source,
                latest_at,
                unread_count: unread,
                total_count: total,
                items,
            });
        }
        Ok(result)
    }

    /// List individual notifications for a specific category.
    pub fn list_notifications_by_category(
        &self,
        category: &str,
        limit: u32,
    ) -> Result<Vec<NotificationEntry>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, source, category, title, body, is_read, response_action, responded_at, created_at
               FROM notifications
               WHERE category = ?1
               ORDER BY created_at DESC
               LIMIT ?2"#,
        )?;
        let rows = stmt
            .query_map(params![category, limit as i64], |row| {
                Self::map_notification_row(row)
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to list notifications by category")?;
        Ok(rows)
    }

    /// Flat timeline of all notifications, newest first.
    pub fn list_notifications_flat(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<NotificationEntry>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, source, category, title, body, is_read, response_action, responded_at, created_at
               FROM notifications
               ORDER BY created_at DESC
               LIMIT ?1 OFFSET ?2"#,
        )?;
        let rows = stmt
            .query_map(params![limit as i64, offset as i64], |row| {
                Self::map_notification_row(row)
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to list flat notifications")?;
        Ok(rows)
    }

    /// Get the latest single unread notification (for Overview banner).
    pub fn get_latest_unread_notification(&self) -> Result<Option<NotificationEntry>> {
        let conn = self.connection()?;
        let entry = conn
            .query_row(
                r#"SELECT id, source, category, title, body, is_read, response_action, responded_at, created_at
                   FROM notifications
                   WHERE is_read = 0
                   ORDER BY created_at DESC
                   LIMIT 1"#,
                [],
                |row| Self::map_notification_row(row),
            )
            .optional()
            .context("Failed to get latest unread notification")?;
        Ok(entry)
    }

    /// Total count of unread notifications.
    pub fn get_unread_notification_count(&self) -> Result<i64> {
        let conn = self.connection()?;
        let count = conn
            .query_row(
                "SELECT COUNT(*) FROM notifications WHERE is_read = 0",
                [],
                |row| row.get::<_, i64>(0),
            )
            .context("Failed to count unread notifications")?;
        Ok(count)
    }

    /// Mark a single notification as read.
    pub fn mark_notification_read(&self, notification_id: i64) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "UPDATE notifications SET is_read = 1 WHERE id = ?1",
            params![notification_id],
        )
        .context("Failed to mark notification as read")?;
        Ok(())
    }

    /// Mark all notifications in a category as read.
    pub fn mark_category_notifications_read(&self, category: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "UPDATE notifications SET is_read = 1 WHERE category = ?1",
            params![category],
        )
        .context("Failed to mark category notifications as read")?;
        Ok(())
    }

    /// Mark all notifications as read.
    pub fn mark_all_notifications_read(&self) -> Result<()> {
        let conn = self.connection()?;
        conn.execute("UPDATE notifications SET is_read = 1 WHERE is_read = 0", [])
            .context("Failed to mark all notifications as read")?;
        Ok(())
    }

    /// Delete all notifications for a given category.
    pub fn delete_notifications_by_category(&self, category: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "DELETE FROM notifications WHERE category = ?1",
            params![category],
        )
        .context("Failed to delete notifications by category")?;
        Ok(())
    }

    fn map_notification_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NotificationEntry> {
        let id: i64 = row.get(0)?;
        let source_str: String = row.get(1)?;
        let category: String = row.get(2)?;
        let title: String = row.get(3)?;
        let body: String = row.get(4)?;
        let is_read: bool = row.get::<_, i64>(5)? != 0;
        let response_action = row
            .get::<_, Option<String>>(6)?
            .and_then(|value| NotificationResponseAction::from_str(&value).ok());
        let responded_at = row
            .get::<_, Option<i64>>(7)?
            .and_then(|timestamp| Utc.timestamp_opt(timestamp, 0).single());
        let created_ts: i64 = row.get(8)?;
        let created_at = Utc
            .timestamp_opt(created_ts, 0)
            .single()
            .unwrap_or_else(Utc::now);
        let source =
            NotificationSource::from_str(&source_str).unwrap_or(NotificationSource::Trigger);
        Ok(NotificationEntry {
            id,
            source,
            category,
            title,
            body,
            is_read,
            response_action,
            responded_at,
            created_at,
        })
    }

    pub fn events_count(&self) -> Result<i64> {
        let conn = self.connection()?;
        let count = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| {
                row.get::<_, i64>(0)
            })
            .context("Failed to count events")?;
        Ok(count)
    }

    pub fn events_time_range(&self) -> Result<Option<(DateTime<Utc>, DateTime<Utc>)>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT MIN(timestamp_utc) as start_ts, MAX(timestamp_utc) as end_ts FROM events",
        )?;
        let result = stmt.query_row([], |row| {
            let start = row.get::<_, Option<i64>>(0)?;
            let end = row.get::<_, Option<i64>>(1)?;
            Ok((start, end))
        })?;

        match result {
            (Some(start), Some(end)) => {
                let start_dt = Utc.timestamp_opt(start, 0).single();
                let end_dt = Utc.timestamp_opt(end, 0).single();
                Ok(start_dt.and_then(|s| end_dt.map(|e| (s, e))))
            }
            _ => Ok(None),
        }
    }

    pub fn list_triggers(&self) -> Result<Vec<TriggerInfo>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT t.trigger_uuid, t.name, t.version, t.precondition_json, t.condition_json,
                 t.next_fire_utc, t.last_result, t.is_paused,
                 tb.entity_type, tb.entity_uuid, t.action_uuid, t.action_args_json
               FROM triggers t
               LEFT JOIN trigger_bindings tb ON tb.trigger_uuid = t.trigger_uuid
               ORDER BY t.created_at ASC"#,
        )?;

        let triggers = stmt
            .query_map([], |row| {
                let next_fire_ts = row.get::<_, Option<i64>>(5)?;
                let next_fire = next_fire_ts.and_then(|ts| Utc.timestamp_opt(ts, 0).single());
                let last_result = row.get::<_, Option<i64>>(6)?.map(|value| value != 0);
                let is_paused = row.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0;

                let precondition_json: String = row.get(3)?;
                let precondition = serde_json::from_str(&precondition_json).unwrap_or_default();

                let condition_json: String = row.get(4)?;
                let condition = serde_json::from_str(&condition_json).unwrap_or_default();
                let action_args_json: String = row.get(11)?;
                let action_args = serde_json::from_str(&action_args_json)
                    .unwrap_or_else(|_| Value::Object(Default::default()));

                Ok(TriggerInfo {
                    trigger_id: row.get(0)?,
                    name: row.get(1)?,
                    version: row.get(2)?,
                    precondition,
                    condition,
                    next_fire,
                    last_result,
                    is_paused,
                    bind_type: row.get(8)?,
                    bind_uuid: row.get(9)?,
                    action_uuid: row.get(10)?,
                    action_args,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to read trigger list")?;

        Ok(triggers)
    }

    pub fn list_triggers_for_context_prompt(&self, limit: usize) -> Result<Vec<TriggerInfo>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT t.trigger_uuid, t.name, t.version, t.precondition_json, t.condition_json,
                 t.next_fire_utc, t.last_result, t.is_paused,
                 tb.entity_type, tb.entity_uuid, t.action_uuid, t.action_args_json
               FROM triggers t
               LEFT JOIN trigger_bindings tb ON tb.trigger_uuid = t.trigger_uuid
               ORDER BY t.updated_at DESC
               LIMIT ?1"#,
        )?;

        let triggers = stmt
            .query_map([limit as i64], |row| {
                let next_fire_ts = row.get::<_, Option<i64>>(5)?;
                let next_fire = next_fire_ts.and_then(|ts| Utc.timestamp_opt(ts, 0).single());
                let last_result = row.get::<_, Option<i64>>(6)?.map(|value| value != 0);
                let is_paused = row.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0;

                let precondition_json: String = row.get(3)?;
                let precondition = serde_json::from_str(&precondition_json).unwrap_or_default();

                let condition_json: String = row.get(4)?;
                let condition = serde_json::from_str(&condition_json).unwrap_or_default();
                let action_args_json: String = row.get(11)?;
                let action_args = serde_json::from_str(&action_args_json)
                    .unwrap_or_else(|_| Value::Object(Default::default()));

                Ok(TriggerInfo {
                    trigger_id: row.get(0)?,
                    name: row.get(1)?,
                    version: row.get(2)?,
                    precondition,
                    condition,
                    next_fire,
                    last_result,
                    is_paused,
                    bind_type: row.get(8)?,
                    bind_uuid: row.get(9)?,
                    action_uuid: row.get(10)?,
                    action_args,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to read trigger list (context prompt)")?;

        Ok(triggers)
    }

    pub fn seed_builtin_actions(&self, actions: &[ActionDefinition]) -> Result<()> {
        let conn = self.connection()?;
        let now = Utc::now().timestamp();
        for action in actions {
            conn.execute(
                r#"INSERT INTO actions
                   (action_uuid, name, title, description, version, category, enabled, metadata_json, script_source, input_schema_json, output_schema_json, created_at, updated_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)
                   ON CONFLICT(action_uuid) DO UPDATE SET
                     name = excluded.name,
                     title = excluded.title,
                     description = excluded.description,
                     version = excluded.version,
                     category = excluded.category,
                     enabled = excluded.enabled,
                     metadata_json = excluded.metadata_json,
                     script_source = excluded.script_source,
                     input_schema_json = excluded.input_schema_json,
                     output_schema_json = excluded.output_schema_json,
                     updated_at = excluded.updated_at"#,
                params![
                    &action.action_uuid,
                    &action.name,
                    &action.title,
                    &action.description,
                    &action.version,
                    &action.category,
                    if action.enabled { 1 } else { 0 },
                    serde_json::to_string(&action.metadata_json)?,
                    &action.script_source,
                    serde_json::to_string(&action.input_schema_json)?,
                    action.output_schema_json.as_ref().map(serde_json::to_string).transpose()?,
                    now,
                ],
            )
            .context("Failed to seed builtin action")?;
        }
        Ok(())
    }

    pub fn list_actions(&self) -> Result<Vec<ActionDefinition>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT action_uuid, name, title, description, version, category, enabled,
                      metadata_json, script_source, input_schema_json, output_schema_json
               FROM actions
               ORDER BY created_at ASC"#,
        )?;

        let actions = stmt
            .query_map([], |row| {
                let metadata_json: String = row.get(7)?;
                let input_schema_json: String = row.get(9)?;
                let output_schema_json: Option<String> = row.get(10)?;

                Ok(ActionDefinition {
                    action_uuid: row.get(0)?,
                    name: row.get(1)?,
                    title: row.get(2)?,
                    description: row.get(3)?,
                    version: row.get(4)?,
                    category: row.get(5)?,
                    enabled: row.get::<_, i64>(6)? != 0,
                    metadata_json: serde_json::from_str(&metadata_json)
                        .unwrap_or_else(|_| Value::Object(Default::default())),
                    script_source: row.get(8)?,
                    input_schema_json: serde_json::from_str(&input_schema_json)
                        .unwrap_or_else(|_| Value::Object(Default::default())),
                    output_schema_json: output_schema_json
                        .and_then(|value| serde_json::from_str(&value).ok()),
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to read action list")?;

        Ok(actions)
    }

    pub fn fetch_action(&self, action_uuid: &str) -> Result<Option<ActionDefinition>> {
        let conn = self.connection()?;
        let action = conn
            .query_row(
                r#"SELECT action_uuid, name, title, description, version, category, enabled,
                          metadata_json, script_source, input_schema_json, output_schema_json
                   FROM actions
                   WHERE action_uuid = ?1
                   LIMIT 1"#,
                params![action_uuid],
                |row| {
                    let metadata_json: String = row.get(7)?;
                    let input_schema_json: String = row.get(9)?;
                    let output_schema_json: Option<String> = row.get(10)?;
                    Ok(ActionDefinition {
                        action_uuid: row.get(0)?,
                        name: row.get(1)?,
                        title: row.get(2)?,
                        description: row.get(3)?,
                        version: row.get(4)?,
                        category: row.get(5)?,
                        enabled: row.get::<_, i64>(6)? != 0,
                        metadata_json: serde_json::from_str(&metadata_json)
                            .unwrap_or_else(|_| Value::Object(Default::default())),
                        script_source: row.get(8)?,
                        input_schema_json: serde_json::from_str(&input_schema_json)
                            .unwrap_or_else(|_| Value::Object(Default::default())),
                        output_schema_json: output_schema_json
                            .and_then(|value| serde_json::from_str(&value).ok()),
                    })
                },
            )
            .optional()
            .context("Failed to fetch action by uuid")?;
        Ok(action)
    }

    pub fn insert_action_invocation(&self, record: &ActionInvocationRecord) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            r#"INSERT INTO action_invocations
               (invocation_uuid, action_uuid, source_kind, source_entity_type, source_entity_uuid,
                args_json, result_json, console_logs_json, error_json, started_at, finished_at,
                duration_ms, device_id)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"#,
            params![
                &record.invocation_uuid,
                &record.action_uuid,
                record.source_kind.as_str(),
                &record.source_entity_type,
                &record.source_entity_uuid,
                serde_json::to_string(&record.args_json)?,
                record
                    .result_json
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                serde_json::to_string(&record.console_logs)?,
                record
                    .error_json
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                record.started_at.timestamp(),
                record.finished_at.timestamp(),
                i64::try_from(record.duration_ms).unwrap_or(i64::MAX),
                &record.device_id,
            ],
        )
        .context("Failed to insert action invocation")?;
        Ok(())
    }

    pub fn latest_action_invocation(
        &self,
        action_uuid: &str,
    ) -> Result<Option<ActionInvocationRecord>> {
        let conn = self.connection()?;
        let record = conn
            .query_row(
                r#"SELECT invocation_uuid, action_uuid, source_kind, source_entity_type, source_entity_uuid,
                          args_json, result_json, console_logs_json, error_json, started_at, finished_at,
                          duration_ms, device_id
                   FROM action_invocations
                   WHERE action_uuid = ?1
                   ORDER BY started_at DESC
                   LIMIT 1"#,
                params![action_uuid],
                |row| {
                    let args_json: String = row.get(5)?;
                    let result_json: Option<String> = row.get(6)?;
                    let console_logs_json: String = row.get(7)?;
                    let error_json: Option<String> = row.get(8)?;
                    let started_at = row.get::<_, i64>(9)?;
                    let finished_at = row.get::<_, i64>(10)?;
                    let source_kind_raw: String = row.get(2)?;
                    let source_kind = ActionInvocationSourceKind::from_str(&source_kind_raw)
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                Type::Text,
                                Box::new(io::Error::new(io::ErrorKind::InvalidData, error)),
                            )
                        })?;
                    Ok(ActionInvocationRecord {
                        invocation_uuid: row.get(0)?,
                        action_uuid: row.get(1)?,
                        source_kind,
                        source_entity_type: row.get(3)?,
                        source_entity_uuid: row.get(4)?,
                        args_json: serde_json::from_str(&args_json)
                            .unwrap_or_else(|_| Value::Object(Default::default())),
                        result_json: result_json.and_then(|value| serde_json::from_str(&value).ok()),
                        console_logs: serde_json::from_str(&console_logs_json).unwrap_or_default(),
                        error_json: error_json.and_then(|value| serde_json::from_str(&value).ok()),
                        started_at: Utc.timestamp_opt(started_at, 0).single().unwrap_or_else(Utc::now),
                        finished_at: Utc.timestamp_opt(finished_at, 0).single().unwrap_or_else(Utc::now),
                        duration_ms: row.get::<_, i64>(11)?.max(0) as u64,
                        device_id: row.get(12)?,
                    })
                },
            )
            .optional()
            .context("Failed to fetch latest action invocation")?;
        Ok(record)
    }

    fn map_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredEvent> {
        let ts_str: String = row.get(1)?;
        let timestamp = DateTime::parse_from_rfc3339(&ts_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let metadata_json: String = row.get(2)?;
        let metadata: Value = serde_json::from_str(&metadata_json).unwrap_or(Value::Null);

        Ok(StoredEvent {
            event_type: row.get(0)?,
            timestamp,
            metadata,
        })
    }

    fn map_trigger_log_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TriggerLogEntry> {
        let level_raw: String = row.get(1)?;
        let level = TriggerLogLevel::from_str(&level_raw).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                Type::Text,
                Box::new(io::Error::new(io::ErrorKind::InvalidData, err)),
            )
        })?;
        let fire_time_ts: i64 = row.get(3)?;
        let created_ts: i64 = row.get(4)?;
        let run_type_raw: String = row.get(5)?;
        let run_type = TriggerRunType::from_str(&run_type_raw).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                Type::Text,
                Box::new(io::Error::new(io::ErrorKind::InvalidData, err)),
            )
        })?;
        Ok(TriggerLogEntry {
            trigger_id: row.get(0)?,
            level,
            message: row.get(2)?,
            fire_time: Utc
                .timestamp_opt(fire_time_ts, 0)
                .single()
                .unwrap_or_else(Utc::now),
            created_at: Utc
                .timestamp_opt(created_ts, 0)
                .single()
                .unwrap_or_else(Utc::now),
            run_type,
        })
    }

    // ===== Things (Automerge local-first) =====

    pub fn get_things_state(&self) -> Result<Option<(Vec<u8>, Vec<u8>, bool, Option<String>)>> {
        use std::time::Instant;

        #[cfg(test)]
        TEST_THINGS_STATE_GET.fetch_add(1, Ordering::Relaxed);

        let total = Instant::now();

        let t0 = Instant::now();
        let conn = self.connection()?;
        let conn_ms = t0.elapsed().as_millis();

        let t1 = Instant::now();
        let mut stmt = conn.prepare(
            "SELECT automerge_doc, sync_state, dirty, last_sync_at FROM things_state WHERE id = 1",
        )?;
        let prepare_ms = t1.elapsed().as_millis();

        let t2 = Instant::now();
        let row = stmt
            .query_row([], |row| {
                let doc = row.get::<_, Vec<u8>>(0)?;
                let sync_state = row.get::<_, Vec<u8>>(1)?;
                let dirty_int = row.get::<_, i64>(2)?;
                let last_sync_at = row.get::<_, Option<String>>(3)?;
                tracing::info!(
                    doc_bytes = doc.len(),
                    sync_state_bytes = sync_state.len(),
                    dirty = (dirty_int != 0),
                    last_sync_at = ?last_sync_at,
                    "sqlite: things_state row"
                );
                Ok((doc, sync_state, dirty_int != 0, last_sync_at))
            })
            .optional()
            .context("Failed to get things state")?;

        let query_ms = t2.elapsed().as_millis();
        tracing::info!(
            conn_ms,
            prepare_ms,
            query_ms,
            total_ms = total.elapsed().as_millis(),
            hit = row.is_some(),
            "sqlite: get_things_state"
        );
        Ok(row)
    }

    pub fn save_things_state(
        &self,
        doc: &[u8],
        sync_state: &[u8],
        dirty: bool,
        last_sync_at: Option<&str>,
    ) -> Result<()> {
        #[cfg(test)]
        TEST_THINGS_STATE_SAVE.fetch_add(1, Ordering::Relaxed);

        let conn = self.connection()?;
        conn.execute(
            r#"INSERT INTO things_state (id, automerge_doc, sync_state, dirty, last_sync_at)
               VALUES (1, ?1, ?2, ?3, ?4)
               ON CONFLICT(id) DO UPDATE SET
                   automerge_doc = excluded.automerge_doc,
                   sync_state = excluded.sync_state,
                   dirty = excluded.dirty,
                   last_sync_at = excluded.last_sync_at"#,
            params![doc, sync_state, if dirty { 1 } else { 0 }, last_sync_at],
        )
        .context("Failed to save things state")?;
        Ok(())
    }

    pub fn set_things_dirty(&self, dirty: bool) -> Result<()> {
        let existing = self.get_things_state()?;
        if let Some((doc, sync_state, _old_dirty, last_sync_at)) = existing {
            return self.save_things_state(&doc, &sync_state, dirty, last_sync_at.as_deref());
        }

        // Initialize with empty doc/sync state; caller will overwrite soon.
        self.save_things_state(&[], &[], dirty, None)
    }

    // ===== V3 CRDT Documents (Multi-document architecture) =====

    /// Get a CRDT document by UUID and data type
    pub fn get_crdt_document(
        &self,
        uuid: &str,
        data_type: &str,
    ) -> Result<Option<CrdtDocumentRow>> {
        let conn = self.connection()?;
        let row = conn
            .query_row(
                r#"SELECT uuid, data_type, automerge_doc, sync_state, dirty, last_sync_at, created_at, updated_at
                   FROM crdt_documents
                   WHERE uuid = ?1 AND data_type = ?2"#,
                params![uuid, data_type],
                |row| {
                    Ok(CrdtDocumentRow {
                        uuid: row.get(0)?,
                        data_type: row.get(1)?,
                        automerge_doc: row.get(2)?,
                        sync_state: row.get(3)?,
                        dirty: row.get::<_, i64>(4)? != 0,
                        last_sync_at: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                },
            )
            .optional()
            .context("Failed to get crdt document")?;
        Ok(row)
    }

    /// Save or update a CRDT document
    pub fn save_crdt_document(
        &self,
        uuid: &str,
        data_type: &str,
        automerge_doc: &[u8],
        sync_state: &[u8],
        dirty: bool,
        last_sync_at: Option<&str>,
    ) -> Result<()> {
        #[cfg(test)]
        TEST_CRDT_DOCUMENT_SAVE.fetch_add(1, Ordering::Relaxed);

        let mut conn = self.connection()?;
        let now = Utc::now().timestamp();
        let tx = conn.transaction()?;
        tx.execute(
            r#"INSERT INTO crdt_documents (uuid, data_type, automerge_doc, sync_state, dirty, last_sync_at, created_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
               ON CONFLICT(uuid, data_type) DO UPDATE SET
                   automerge_doc = excluded.automerge_doc,
                   sync_state = excluded.sync_state,
                   dirty = excluded.dirty,
                   last_sync_at = excluded.last_sync_at,
                   updated_at = excluded.updated_at"#,
            params![
                uuid,
                data_type,
                automerge_doc,
                sync_state,
                if dirty { 1 } else { 0 },
                last_sync_at,
                now
            ],
        )
        .context("Failed to save crdt document")?;
        Self::bump_internal_kv_counter_tx(&tx, CRDT_DOCUMENTS_REVISION_KEY)?;
        tx.commit()?;
        let revision = self.get_crdt_documents_revision()?;
        let namespace = self.cache_namespace();
        crate::crdt_cache::upsert_document(
            &namespace,
            revision,
            uuid,
            data_type,
            automerge_doc,
            sync_state,
            dirty,
            last_sync_at,
        );
        crate::crdt_cache::invalidate_namespace_revision_older_than(&namespace, revision);
        Ok(())
    }

    /// Get all dirty CRDT documents, ordered by data_type priority (root=0, collection=1, thing_markdown=2)
    pub fn get_dirty_crdt_documents(&self) -> Result<Vec<CrdtDocumentRow>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT uuid, data_type, automerge_doc, sync_state, dirty, last_sync_at, created_at, updated_at
               FROM crdt_documents
               WHERE dirty = 1
               ORDER BY CASE data_type
                   WHEN 'root' THEN 0
                   WHEN 'collection' THEN 1
                   WHEN 'thing_markdown' THEN 2
                   ELSE 3
               END"#,
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(CrdtDocumentRow {
                    uuid: row.get(0)?,
                    data_type: row.get(1)?,
                    automerge_doc: row.get(2)?,
                    sync_state: row.get(3)?,
                    dirty: row.get::<_, i64>(4)? != 0,
                    last_sync_at: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to get dirty crdt documents")?;
        Ok(rows)
    }

    /// Get all CRDT documents of a specific type
    pub fn get_crdt_documents_by_type(&self, data_type: &str) -> Result<Vec<CrdtDocumentRow>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT uuid, data_type, automerge_doc, sync_state, dirty, last_sync_at, created_at, updated_at
               FROM crdt_documents
               WHERE data_type = ?1"#,
        )?;
        let rows = stmt
            .query_map(params![data_type], |row| {
                Ok(CrdtDocumentRow {
                    uuid: row.get(0)?,
                    data_type: row.get(1)?,
                    automerge_doc: row.get(2)?,
                    sync_state: row.get(3)?,
                    dirty: row.get::<_, i64>(4)? != 0,
                    last_sync_at: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to get crdt documents by type")?;
        Ok(rows)
    }

    pub fn list_crdt_documents(&self) -> Result<Vec<CrdtDocumentRow>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT uuid, data_type, automerge_doc, sync_state, dirty, last_sync_at, created_at, updated_at
               FROM crdt_documents"#,
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(CrdtDocumentRow {
                    uuid: row.get(0)?,
                    data_type: row.get(1)?,
                    automerge_doc: row.get(2)?,
                    sync_state: row.get(3)?,
                    dirty: row.get::<_, i64>(4)? != 0,
                    last_sync_at: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to list crdt documents")?;
        Ok(rows)
    }

    /// Mark a CRDT document as dirty
    pub fn set_crdt_document_dirty(&self, uuid: &str, data_type: &str, dirty: bool) -> Result<()> {
        let mut conn = self.connection()?;
        let now = Utc::now().timestamp();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE crdt_documents SET dirty = ?1, updated_at = ?2 WHERE uuid = ?3 AND data_type = ?4",
            params![if dirty { 1 } else { 0 }, now, uuid, data_type],
        )
        .context("Failed to set crdt document dirty")?;
        Self::bump_internal_kv_counter_tx(&tx, CRDT_DOCUMENTS_REVISION_KEY)?;
        tx.commit()?;
        let revision = self.get_crdt_documents_revision()?;
        let namespace = self.cache_namespace();
        if let Some(row) = self.get_crdt_document(uuid, data_type)? {
            crate::crdt_cache::upsert_document(
                &namespace,
                revision,
                &row.uuid,
                &row.data_type,
                &row.automerge_doc,
                &row.sync_state,
                row.dirty,
                row.last_sync_at.as_deref(),
            );
        }
        crate::crdt_cache::invalidate_namespace_revision_older_than(&namespace, revision);
        Ok(())
    }

    /// Delete a CRDT document
    pub fn delete_crdt_document(&self, uuid: &str, data_type: &str) -> Result<()> {
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM crdt_documents WHERE uuid = ?1 AND data_type = ?2",
            params![uuid, data_type],
        )
        .context("Failed to delete crdt document")?;
        Self::bump_internal_kv_counter_tx(&tx, CRDT_DOCUMENTS_REVISION_KEY)?;
        tx.commit()?;
        let revision = self.get_crdt_documents_revision()?;
        let namespace = self.cache_namespace();
        crate::crdt_cache::remove_document(&namespace, revision, uuid, data_type);
        crate::crdt_cache::invalidate_namespace_revision_older_than(&namespace, revision);
        Ok(())
    }

    /// Delete all CRDT documents (used during bootstrap reset)
    pub fn delete_all_crdt_documents(&self) -> Result<()> {
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM crdt_documents", [])
            .context("Failed to delete all crdt documents")?;
        Self::bump_internal_kv_counter_tx(&tx, CRDT_DOCUMENTS_REVISION_KEY)?;
        tx.commit()?;
        let revision = self.get_crdt_documents_revision()?;
        crate::crdt_cache::invalidate_namespace_revision_older_than(
            &self.cache_namespace(),
            revision,
        );
        Ok(())
    }

    /// Get all CRDT document keys (uuid, data_type) pairs
    pub fn list_crdt_document_keys(&self) -> Result<Vec<(String, String)>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare("SELECT uuid, data_type FROM crdt_documents")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to list crdt document keys")?;
        Ok(rows)
    }

    // ===== Trigger Bindings =====

    /// Record a binding between a trigger and a thing/collection.
    ///
    /// The UNIQUE constraint on (entity_type, entity_uuid) ensures that each entity
    /// can only be bound to one trigger at a time. If a binding already exists for
    /// the entity, it will be replaced with the new trigger_uuid (rebinding scenario).
    pub fn upsert_trigger_binding(
        &self,
        trigger_uuid: &str,
        entity_type: &str,
        entity_uuid: &str,
    ) -> Result<()> {
        let conn = self.connection()?;
        let now = Utc::now().timestamp();
        conn.execute(
            r#"INSERT INTO trigger_bindings (trigger_uuid, entity_type, entity_uuid, created_at)
               VALUES (?1, ?2, ?3, ?4)
               ON CONFLICT(entity_type, entity_uuid) DO UPDATE SET
                   trigger_uuid = excluded.trigger_uuid,
                   created_at = excluded.created_at"#,
            params![trigger_uuid, entity_type, entity_uuid, now],
        )
        .context("Failed to upsert trigger binding")?;
        Ok(())
    }

    /// Remove a binding for a specific entity
    pub fn delete_trigger_binding(&self, entity_type: &str, entity_uuid: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "DELETE FROM trigger_bindings WHERE entity_type = ?1 AND entity_uuid = ?2",
            params![entity_type, entity_uuid],
        )
        .context("Failed to delete trigger binding")?;
        Ok(())
    }

    /// List all trigger bindings.
    pub fn list_trigger_bindings(&self) -> Result<Vec<(String, String, String)>> {
        let conn = self.connection()?;
        let mut stmt =
            conn.prepare("SELECT trigger_uuid, entity_type, entity_uuid FROM trigger_bindings")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to list trigger bindings")?;
        Ok(rows)
    }

    /// Get all trigger UUIDs that are bound to at least one entity
    pub fn list_bound_trigger_uuids(&self) -> Result<Vec<String>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare("SELECT DISTINCT trigger_uuid FROM trigger_bindings")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to list bound trigger UUIDs")?;
        Ok(rows)
    }

    /// Check if a trigger is bound to any entity
    pub fn is_trigger_bound(&self, trigger_uuid: &str) -> Result<bool> {
        let conn = self.connection()?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM trigger_bindings WHERE trigger_uuid = ?1",
                params![trigger_uuid],
                |row| row.get(0),
            )
            .context("Failed to check if trigger is bound")?;
        Ok(count > 0)
    }

    /// Get the trigger UUID currently bound to a specific entity, if any.
    /// This supplements the CRDT snapshot and handles edge cases where the CRDT
    /// may not reflect the current binding state.
    pub fn get_trigger_for_entity(
        &self,
        entity_type: &str,
        entity_uuid: &str,
    ) -> Result<Option<String>> {
        let conn = self.connection()?;
        let result = conn.query_row(
            "SELECT trigger_uuid FROM trigger_bindings WHERE entity_type = ?1 AND entity_uuid = ?2",
            params![entity_type, entity_uuid],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(uuid) => Ok(Some(uuid)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("Failed to get trigger for entity: {}", e)),
        }
    }

    /// Get all entities bound to a specific trigger
    pub fn get_entities_for_trigger(&self, trigger_uuid: &str) -> Result<Vec<(String, String)>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT entity_type, entity_uuid FROM trigger_bindings WHERE trigger_uuid = ?1",
        )?;
        let rows = stmt
            .query_map(params![trigger_uuid], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to get entities for trigger")?;
        Ok(rows)
    }

    /// Remove ALL trigger_bindings rows whose trigger_uuid matches.
    pub fn delete_all_bindings_for_trigger(&self, trigger_uuid: &str) -> Result<usize> {
        let conn = self.connection()?;
        let rows_affected = conn
            .execute(
                "DELETE FROM trigger_bindings WHERE trigger_uuid = ?1",
                params![trigger_uuid],
            )
            .context("Failed to delete all trigger bindings")?;
        Ok(rows_affected)
    }

    /// Remove a specific trigger if it exists
    pub fn delete_trigger(&self, trigger_uuid: &str) -> Result<bool> {
        let conn = self.connection()?;
        let rows_affected = conn
            .execute(
                "DELETE FROM triggers WHERE trigger_uuid = ?1",
                params![trigger_uuid],
            )
            .context("Failed to delete trigger")?;
        Ok(rows_affected > 0)
    }

    // ===== Chat Sessions =====

    /// Create or update a chat session
    pub fn upsert_chat_session(&self, session: &ChatSession) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            r#"INSERT INTO chat_sessions (session_id, title, created_at, last_activity, message_count)
               VALUES (?1, ?2, ?3, ?4, ?5)
               ON CONFLICT(session_id) DO UPDATE SET
                   title = excluded.title,
                   last_activity = excluded.last_activity,
                   message_count = excluded.message_count"#,
            params![
                session.session_id,
                session.title,
                session.created_at.timestamp(),
                session.last_activity.timestamp(),
                session.message_count
            ],
        )
        .context("Failed to upsert chat session")?;
        Ok(())
    }

    /// Get a specific chat session by ID
    pub fn get_chat_session(&self, session_id: &str) -> Result<Option<ChatSession>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, title, created_at, last_activity, message_count
             FROM chat_sessions
             WHERE session_id = ?1",
        )?;
        let session = stmt
            .query_row([session_id], |row| {
                let created_ts: i64 = row.get(2)?;
                let last_activity_ts: i64 = row.get(3)?;
                Ok(ChatSession {
                    session_id: row.get(0)?,
                    title: row.get(1)?,
                    created_at: Utc
                        .timestamp_opt(created_ts, 0)
                        .single()
                        .unwrap_or_else(Utc::now),
                    last_activity: Utc
                        .timestamp_opt(last_activity_ts, 0)
                        .single()
                        .unwrap_or_else(Utc::now),
                    message_count: row.get(4)?,
                })
            })
            .optional()
            .context("Failed to fetch chat session")?;
        Ok(session)
    }

    /// List all chat sessions, ordered by last activity (most recent first)
    pub fn list_chat_sessions(&self, limit: Option<u32>) -> Result<Vec<ChatSession>> {
        let conn = self.connection()?;

        let sessions = if let Some(limit) = limit {
            let mut stmt = conn.prepare(
                "SELECT session_id, title, created_at, last_activity, message_count
                 FROM chat_sessions
                 ORDER BY last_activity DESC
                 LIMIT ?1",
            )?;
            stmt.query_map([limit], |row| {
                let created_ts: i64 = row.get(2)?;
                let last_activity_ts: i64 = row.get(3)?;
                Ok(ChatSession {
                    session_id: row.get(0)?,
                    title: row.get(1)?,
                    created_at: Utc
                        .timestamp_opt(created_ts, 0)
                        .single()
                        .unwrap_or_else(Utc::now),
                    last_activity: Utc
                        .timestamp_opt(last_activity_ts, 0)
                        .single()
                        .unwrap_or_else(Utc::now),
                    message_count: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to fetch chat sessions")?
        } else {
            let mut stmt = conn.prepare(
                "SELECT session_id, title, created_at, last_activity, message_count
                 FROM chat_sessions
                 ORDER BY last_activity DESC",
            )?;
            stmt.query_map([], |row| {
                let created_ts: i64 = row.get(2)?;
                let last_activity_ts: i64 = row.get(3)?;
                Ok(ChatSession {
                    session_id: row.get(0)?,
                    title: row.get(1)?,
                    created_at: Utc
                        .timestamp_opt(created_ts, 0)
                        .single()
                        .unwrap_or_else(Utc::now),
                    last_activity: Utc
                        .timestamp_opt(last_activity_ts, 0)
                        .single()
                        .unwrap_or_else(Utc::now),
                    message_count: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to fetch chat sessions")?
        };

        Ok(sessions)
    }

    /// Update session activity (call when sending/receiving messages)
    pub fn update_session_activity(&self, update: &ChatSessionUpdate) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            r#"UPDATE chat_sessions
               SET title = ?1, last_activity = ?2, message_count = ?3
               WHERE session_id = ?4"#,
            params![
                update.title,
                update.last_activity.timestamp(),
                update.message_count,
                update.session_id
            ],
        )
        .context("Failed to update session activity")?;
        Ok(())
    }

    /// Delete a chat session
    pub fn delete_chat_session(&self, session_id: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "DELETE FROM eval_dataset_sessions WHERE session_id = ?1",
            [session_id],
        )
        .context("Failed to delete dataset session membership")?;
        conn.execute(
            "DELETE FROM chat_messages WHERE session_id = ?1",
            [session_id],
        )
        .context("Failed to delete chat messages")?;
        conn.execute(
            "DELETE FROM chat_runtime_state WHERE session_id = ?1",
            [session_id],
        )
        .context("Failed to delete chat runtime state")?;
        conn.execute(
            "DELETE FROM chat_model_inputs WHERE session_id = ?1",
            [session_id],
        )
        .context("Failed to delete chat model inputs")?;
        conn.execute(
            "DELETE FROM chat_sessions WHERE session_id = ?1",
            [session_id],
        )
        .context("Failed to delete chat session")?;
        Ok(())
    }

    // ===== Agent Versions =====

    pub fn create_agent_version(&self, version: &AgentVersion) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            r#"INSERT INTO agent_versions (version_id, agent_id, name, raw_markdown, created_at, updated_at, applied_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
            params![
                version.version_id,
                version.agent_id,
                version.name,
                version.raw_markdown,
                version.created_at.timestamp(),
                version.updated_at.timestamp(),
                version.applied_at.map(|value| value.timestamp()),
            ],
        )
        .context("Failed to create agent version")?;
        Ok(())
    }

    pub fn get_agent_version(&self, version_id: &str) -> Result<Option<AgentVersion>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT version_id, agent_id, name, raw_markdown, created_at, updated_at, applied_at FROM agent_versions WHERE version_id = ?1",
        )?;
        let version = stmt
            .query_row([version_id], |row| Self::map_agent_version_row(row))
            .optional()
            .context("Failed to fetch agent version")?;
        Ok(version)
    }

    pub fn list_agent_versions(&self, agent_id: &str) -> Result<Vec<AgentVersion>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT version_id, agent_id, name, raw_markdown, created_at, updated_at, applied_at FROM agent_versions WHERE agent_id = ?1 ORDER BY updated_at DESC, created_at DESC",
        )?;
        let versions = stmt
            .query_map([agent_id], Self::map_agent_version_row)?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to list agent versions")?;
        Ok(versions)
    }

    pub fn update_agent_version(&self, update: &AgentVersionUpdate) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            r#"UPDATE agent_versions
               SET name = ?1, raw_markdown = ?2, updated_at = ?3
               WHERE version_id = ?4"#,
            params![
                update.name,
                update.raw_markdown,
                update.updated_at.timestamp(),
                update.version_id,
            ],
        )
        .context("Failed to update agent version")?;
        Ok(())
    }

    pub fn mark_agent_version_applied(
        &self,
        version_id: &str,
        agent_id: &str,
        applied_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.connection()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE agent_versions SET applied_at = NULL WHERE agent_id = ?1",
            [agent_id],
        )
        .context("Failed to clear applied agent version markers")?;
        tx.execute(
            "UPDATE agent_versions SET applied_at = ?1, updated_at = ?1 WHERE version_id = ?2",
            params![applied_at.timestamp(), version_id],
        )
        .context("Failed to mark agent version as applied")?;
        tx.commit()
            .context("Failed to commit agent version apply marker")?;
        Ok(())
    }

    pub fn delete_agent_version(&self, version_id: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "DELETE FROM agent_versions WHERE version_id = ?1",
            [version_id],
        )
        .context("Failed to delete agent version")?;
        Ok(())
    }

    // ===== Eval Datasets =====

    pub fn create_eval_dataset(&self, dataset: &EvalDataset) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            r#"INSERT INTO eval_datasets (dataset_id, name, description, created_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5)"#,
            params![
                dataset.dataset_id,
                dataset.name,
                dataset.description,
                dataset.created_at.timestamp(),
                dataset.updated_at.timestamp(),
            ],
        )
        .context("Failed to create eval dataset")?;
        Ok(())
    }

    pub fn get_eval_dataset(&self, dataset_id: &str) -> Result<Option<EvalDataset>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT dataset_id, name, description, created_at, updated_at FROM eval_datasets WHERE dataset_id = ?1",
        )?;
        let dataset = stmt
            .query_row([dataset_id], |row| Self::map_eval_dataset_row(row))
            .optional()
            .context("Failed to fetch eval dataset")?;
        Ok(dataset)
    }

    pub fn list_eval_datasets(&self) -> Result<Vec<EvalDataset>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT dataset_id, name, description, created_at, updated_at FROM eval_datasets ORDER BY updated_at DESC, created_at DESC",
        )?;
        let datasets = stmt
            .query_map([], Self::map_eval_dataset_row)?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to list eval datasets")?;
        Ok(datasets)
    }

    pub fn update_eval_dataset(&self, update: &EvalDatasetUpdate) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            r#"UPDATE eval_datasets
               SET name = ?1, description = ?2, updated_at = ?3
               WHERE dataset_id = ?4"#,
            params![
                update.name,
                update.description,
                update.updated_at.timestamp(),
                update.dataset_id,
            ],
        )
        .context("Failed to update eval dataset")?;
        Ok(())
    }

    pub fn delete_eval_dataset(&self, dataset_id: &str) -> Result<()> {
        let conn = self.connection()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM eval_dataset_run_evals WHERE dataset_id = ?1",
            [dataset_id],
        )
        .context("Failed to delete eval dataset run evals")?;
        tx.execute(
            "DELETE FROM eval_dataset_run_items WHERE dataset_id = ?1",
            [dataset_id],
        )
        .context("Failed to delete eval dataset run items")?;
        tx.execute(
            "DELETE FROM eval_dataset_runs WHERE dataset_id = ?1",
            [dataset_id],
        )
        .context("Failed to delete eval dataset runs")?;
        tx.execute(
            "DELETE FROM eval_dataset_sessions WHERE dataset_id = ?1",
            [dataset_id],
        )
        .context("Failed to delete eval dataset sessions")?;
        tx.execute(
            "DELETE FROM eval_datasets WHERE dataset_id = ?1",
            [dataset_id],
        )
        .context("Failed to delete eval dataset")?;
        tx.commit()
            .context("Failed to commit eval dataset deletion")?;
        Ok(())
    }

    pub fn add_session_to_dataset(&self, item: &EvalDatasetSession) -> Result<()> {
        let conn = self.connection()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            r#"INSERT INTO eval_dataset_sessions (dataset_id, session_id, added_at)
               VALUES (?1, ?2, ?3)
               ON CONFLICT(dataset_id, session_id) DO UPDATE SET added_at = excluded.added_at"#,
            params![item.dataset_id, item.session_id, item.added_at.timestamp()],
        )
        .context("Failed to add session to dataset")?;
        tx.execute(
            "UPDATE eval_datasets SET updated_at = ?1 WHERE dataset_id = ?2",
            params![item.added_at.timestamp(), item.dataset_id],
        )
        .context("Failed to bump eval dataset updated_at after adding session")?;
        tx.commit()
            .context("Failed to commit dataset session add")?;
        Ok(())
    }

    pub fn remove_session_from_dataset(&self, dataset_id: &str, session_id: &str) -> Result<()> {
        let conn = self.connection()?;
        let now = Utc::now().timestamp();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM eval_dataset_sessions WHERE dataset_id = ?1 AND session_id = ?2",
            params![dataset_id, session_id],
        )
        .context("Failed to remove session from dataset")?;
        tx.execute(
            "UPDATE eval_datasets SET updated_at = ?1 WHERE dataset_id = ?2",
            params![now, dataset_id],
        )
        .context("Failed to bump eval dataset updated_at after removing session")?;
        tx.commit()
            .context("Failed to commit dataset session removal")?;
        Ok(())
    }

    pub fn list_dataset_sessions(&self, dataset_id: &str) -> Result<Vec<EvalDatasetSession>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT ds.dataset_id, ds.session_id, ds.added_at, cs.title, cs.last_activity, cs.message_count
               FROM eval_dataset_sessions ds
               LEFT JOIN chat_sessions cs ON cs.session_id = ds.session_id
               WHERE ds.dataset_id = ?1
               ORDER BY ds.added_at DESC"#,
        )?;
        let items = stmt
            .query_map([dataset_id], |row| {
                let added_at_ts: i64 = row.get(2)?;
                let last_activity_ts: Option<i64> = row.get(4)?;
                Ok(EvalDatasetSession {
                    dataset_id: row.get(0)?,
                    session_id: row.get(1)?,
                    added_at: Self::timestamp_or_now(added_at_ts),
                    title: row.get(3)?,
                    last_activity: last_activity_ts.map(Self::timestamp_or_now),
                    message_count: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to list dataset sessions")?;
        Ok(items)
    }

    pub fn create_eval_dataset_run(
        &self,
        run: &EvalDatasetRun,
        items: &[EvalDatasetRunItem],
    ) -> Result<()> {
        let conn = self.connection()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            r#"INSERT INTO eval_dataset_runs (
                    run_id, dataset_id, agent_id, agent_version_id, agent_version_name,
                    variant_id, variant_label, source_session_count, created_at, updated_at
               ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
            params![
                run.run_id,
                run.dataset_id,
                run.agent_id,
                run.agent_version_id,
                run.agent_version_name,
                run.variant_id,
                run.variant_label,
                run.source_session_count,
                run.created_at.timestamp(),
                run.updated_at.timestamp(),
            ],
        )
        .context("Failed to create eval dataset run")?;

        for item in items {
            tx.execute(
                r#"INSERT INTO eval_dataset_run_items (
                        run_id, dataset_id, session_id, final_text, reasoning,
                        prompt_tokens, completion_tokens, tool_results_json,
                        done, cancelled, interrupted, error, created_at, updated_at
                   ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"#,
                params![
                    item.run_id,
                    item.dataset_id,
                    item.session_id,
                    item.final_text,
                    item.reasoning,
                    item.prompt_tokens,
                    item.completion_tokens,
                    item.tool_results_json,
                    if item.done { 1 } else { 0 },
                    if item.cancelled { 1 } else { 0 },
                    if item.interrupted { 1 } else { 0 },
                    item.error,
                    item.created_at.timestamp(),
                    item.updated_at.timestamp(),
                ],
            )
            .context("Failed to create eval dataset run item")?;
        }

        tx.execute(
            "UPDATE eval_datasets SET updated_at = ?1 WHERE dataset_id = ?2",
            params![run.updated_at.timestamp(), run.dataset_id],
        )
        .context("Failed to bump eval dataset updated_at after replay run")?;
        tx.commit().context("Failed to commit eval dataset run")?;
        Ok(())
    }

    pub fn save_eval_dataset_run_evals(
        &self,
        run_id: &str,
        dataset_id: &str,
        evals: &[EvalDatasetRunEval],
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.connection()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM eval_dataset_run_evals WHERE run_id = ?1",
            [run_id],
        )
        .context("Failed to clear existing eval dataset run evals")?;

        for item in evals {
            tx.execute(
                r#"INSERT INTO eval_dataset_run_evals (
                        run_id, dataset_id, session_id, analysis_agent_id, score,
                        summary, rationale, created_at, updated_at
                   ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"#,
                params![
                    item.run_id,
                    item.dataset_id,
                    item.session_id,
                    item.analysis_agent_id,
                    item.score,
                    item.summary,
                    item.rationale,
                    item.created_at.timestamp(),
                    item.updated_at.timestamp(),
                ],
            )
            .context("Failed to persist eval dataset run eval")?;
        }

        tx.execute(
            "UPDATE eval_dataset_runs SET updated_at = ?1 WHERE run_id = ?2",
            params![updated_at.timestamp(), run_id],
        )
        .context("Failed to bump eval dataset run updated_at after analysis")?;
        tx.execute(
            "UPDATE eval_datasets SET updated_at = ?1 WHERE dataset_id = ?2",
            params![updated_at.timestamp(), dataset_id],
        )
        .context("Failed to bump eval dataset updated_at after analysis")?;
        tx.commit()
            .context("Failed to commit eval dataset run evals")?;
        Ok(())
    }

    pub fn list_eval_dataset_runs(&self, dataset_id: &str) -> Result<Vec<EvalDatasetRun>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT run_id, dataset_id, agent_id, agent_version_id, agent_version_name,
                      variant_id, variant_label, source_session_count, created_at, updated_at
               FROM eval_dataset_runs
               WHERE dataset_id = ?1
               ORDER BY created_at DESC"#,
        )?;
        let runs = stmt
            .query_map([dataset_id], |row| Self::map_eval_dataset_run_row(row))?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to list eval dataset runs")?;
        Ok(runs)
    }

    pub fn get_eval_dataset_run(&self, run_id: &str) -> Result<Option<EvalDatasetRun>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT run_id, dataset_id, agent_id, agent_version_id, agent_version_name,
                      variant_id, variant_label, source_session_count, created_at, updated_at
               FROM eval_dataset_runs
               WHERE run_id = ?1"#,
        )?;
        let run = stmt
            .query_row([run_id], |row| Self::map_eval_dataset_run_row(row))
            .optional()
            .context("Failed to fetch eval dataset run")?;
        Ok(run)
    }

    pub fn list_eval_dataset_run_items(&self, run_id: &str) -> Result<Vec<EvalDatasetRunItem>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT i.run_id, i.dataset_id, i.session_id, cs.title, cs.last_activity, cs.message_count,
                      i.final_text, i.reasoning, i.prompt_tokens, i.completion_tokens,
                      i.tool_results_json, i.done, i.cancelled, i.interrupted, i.error,
                      i.created_at, i.updated_at
               FROM eval_dataset_run_items i
               LEFT JOIN chat_sessions cs ON cs.session_id = i.session_id
               WHERE i.run_id = ?1
               ORDER BY i.created_at ASC"#,
        )?;
        let items = stmt
            .query_map([run_id], |row| {
                let last_activity_ts: Option<i64> = row.get(4)?;
                let done: i64 = row.get(11)?;
                let cancelled: i64 = row.get(12)?;
                let interrupted: i64 = row.get(13)?;
                let created_at_ts: i64 = row.get(15)?;
                let updated_at_ts: i64 = row.get(16)?;
                Ok(EvalDatasetRunItem {
                    run_id: row.get(0)?,
                    dataset_id: row.get(1)?,
                    session_id: row.get(2)?,
                    title: row.get(3)?,
                    last_activity: last_activity_ts.map(Self::timestamp_or_now),
                    message_count: row.get(5)?,
                    final_text: row.get(6)?,
                    reasoning: row.get(7)?,
                    prompt_tokens: row.get(8)?,
                    completion_tokens: row.get(9)?,
                    tool_results_json: row.get(10)?,
                    done: done != 0,
                    cancelled: cancelled != 0,
                    interrupted: interrupted != 0,
                    error: row.get(14)?,
                    created_at: Self::timestamp_or_now(created_at_ts),
                    updated_at: Self::timestamp_or_now(updated_at_ts),
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to list eval dataset run items")?;
        Ok(items)
    }

    pub fn list_eval_dataset_run_evals(&self, run_id: &str) -> Result<Vec<EvalDatasetRunEval>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT run_id, dataset_id, session_id, analysis_agent_id, score, summary, rationale, created_at, updated_at
               FROM eval_dataset_run_evals
               WHERE run_id = ?1
               ORDER BY created_at ASC"#,
        )?;
        let items = stmt
            .query_map([run_id], |row| {
                let created_at_ts: i64 = row.get(7)?;
                let updated_at_ts: i64 = row.get(8)?;
                Ok(EvalDatasetRunEval {
                    run_id: row.get(0)?,
                    dataset_id: row.get(1)?,
                    session_id: row.get(2)?,
                    analysis_agent_id: row.get(3)?,
                    score: row.get(4)?,
                    summary: row.get(5)?,
                    rationale: row.get(6)?,
                    created_at: Self::timestamp_or_now(created_at_ts),
                    updated_at: Self::timestamp_or_now(updated_at_ts),
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to list eval dataset run evals")?;
        Ok(items)
    }

    // ===== Chat Messages =====

    /// Create or update a chat message stored as JSON.
    ///
    /// The SDK stores the raw message JSON produced by the client UI, keyed by (session_id, message_id).
    /// This keeps the SDK storage flexible and avoids schema churn when the UI evolves.
    pub fn upsert_chat_message_json(
        &self,
        session_id: &str,
        message_id: &str,
        created_at_ms: i64,
        message_json: &str,
    ) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            r#"INSERT INTO chat_messages (session_id, message_id, created_at_ms, message_json)
               VALUES (?1, ?2, ?3, ?4)
               ON CONFLICT(session_id, message_id) DO UPDATE SET
                   created_at_ms = excluded.created_at_ms,
                   message_json = excluded.message_json"#,
            params![session_id, message_id, created_at_ms, message_json],
        )
        .context("Failed to upsert chat message")?;
        Ok(())
    }

    /// List chat messages for a session, ordered by timestamp ascending.
    /// Returns raw JSON strings.
    pub fn list_chat_messages_json(
        &self,
        session_id: &str,
        limit: Option<u32>,
        offset: u32,
    ) -> Result<Vec<String>> {
        let conn = self.connection()?;

        let mut messages = Vec::new();

        if let Some(limit) = limit {
            let mut stmt = conn.prepare(
                "SELECT message_json FROM chat_messages WHERE session_id = ?1 ORDER BY created_at_ms ASC LIMIT ?2 OFFSET ?3",
            )?;
            let rows = stmt.query_map(params![session_id, limit, offset], |row| {
                row.get::<_, String>(0)
            })?;
            for row in rows {
                messages.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT message_json FROM chat_messages WHERE session_id = ?1 ORDER BY created_at_ms ASC LIMIT -1 OFFSET ?2",
            )?;
            let rows =
                stmt.query_map(params![session_id, offset], |row| row.get::<_, String>(0))?;
            for row in rows {
                messages.push(row?);
            }
        }

        Ok(messages)
    }

    /// Delete all chat messages for a session.
    pub fn delete_chat_messages(&self, session_id: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "DELETE FROM chat_messages WHERE session_id = ?1",
            [session_id],
        )
        .context("Failed to delete chat messages")?;
        Ok(())
    }

    fn timestamp_or_now(timestamp: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(timestamp, 0)
            .single()
            .unwrap_or_else(Utc::now)
    }

    fn map_agent_version_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentVersion> {
        let created_at_ts: i64 = row.get(4)?;
        let updated_at_ts: i64 = row.get(5)?;
        let applied_at_ts: Option<i64> = row.get(6)?;
        Ok(AgentVersion {
            version_id: row.get(0)?,
            agent_id: row.get(1)?,
            name: row.get(2)?,
            raw_markdown: row.get(3)?,
            created_at: Self::timestamp_or_now(created_at_ts),
            updated_at: Self::timestamp_or_now(updated_at_ts),
            applied_at: applied_at_ts.map(Self::timestamp_or_now),
        })
    }

    fn map_eval_dataset_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EvalDataset> {
        let created_at_ts: i64 = row.get(3)?;
        let updated_at_ts: i64 = row.get(4)?;
        Ok(EvalDataset {
            dataset_id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            created_at: Self::timestamp_or_now(created_at_ts),
            updated_at: Self::timestamp_or_now(updated_at_ts),
        })
    }

    fn map_eval_dataset_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EvalDatasetRun> {
        let created_at_ts: i64 = row.get(8)?;
        let updated_at_ts: i64 = row.get(9)?;
        Ok(EvalDatasetRun {
            run_id: row.get(0)?,
            dataset_id: row.get(1)?,
            agent_id: row.get(2)?,
            agent_version_id: row.get(3)?,
            agent_version_name: row.get(4)?,
            variant_id: row.get(5)?,
            variant_label: row.get(6)?,
            source_session_count: row.get(7)?,
            created_at: Self::timestamp_or_now(created_at_ts),
            updated_at: Self::timestamp_or_now(updated_at_ts),
        })
    }

    /// Create or update runtime-owned chat protocol state for a session.
    pub fn upsert_chat_runtime_state_json(&self, session_id: &str, state_json: &str) -> Result<()> {
        let conn = self.connection()?;
        let now = Utc::now().timestamp();
        conn.execute(
            r#"INSERT INTO chat_runtime_state (session_id, state_json, updated_at)
               VALUES (?1, ?2, ?3)
               ON CONFLICT(session_id) DO UPDATE SET
                   state_json = excluded.state_json,
                   updated_at = excluded.updated_at"#,
            params![session_id, state_json, now],
        )
        .context("Failed to upsert chat runtime state")?;
        Ok(())
    }

    /// Fetch runtime-owned chat protocol state for a session.
    pub fn get_chat_runtime_state_json(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self.connection()?;
        conn.query_row(
            "SELECT state_json FROM chat_runtime_state WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("Failed to fetch chat runtime state")
    }

    /// Delete runtime-owned chat protocol state for a session.
    pub fn delete_chat_runtime_state(&self, session_id: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "DELETE FROM chat_runtime_state WHERE session_id = ?1",
            [session_id],
        )
        .context("Failed to delete chat runtime state")?;
        Ok(())
    }

    /// Create or update a per-run model input snapshot.
    pub fn upsert_chat_model_input_json(
        &self,
        session_id: &str,
        run_id: &str,
        created_at_ms: i64,
        snapshot_json: &str,
    ) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            r#"INSERT INTO chat_model_inputs (session_id, run_id, created_at_ms, snapshot_json)
               VALUES (?1, ?2, ?3, ?4)
               ON CONFLICT(session_id, run_id) DO UPDATE SET
                   created_at_ms = excluded.created_at_ms,
                   snapshot_json = excluded.snapshot_json"#,
            params![session_id, run_id, created_at_ms, snapshot_json],
        )
        .context("Failed to upsert chat model input")?;
        Ok(())
    }

    /// List model input snapshots for a session, newest first.
    pub fn list_chat_model_input_json(
        &self,
        session_id: &str,
        limit: Option<u32>,
    ) -> Result<Vec<String>> {
        let conn = self.connection()?;
        let mut items = Vec::new();
        if let Some(limit) = limit {
            let mut stmt = conn.prepare(
                "SELECT snapshot_json FROM chat_model_inputs WHERE session_id = ?1 ORDER BY created_at_ms DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![session_id, limit], |row| row.get::<_, String>(0))?;
            for row in rows {
                items.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT snapshot_json FROM chat_model_inputs WHERE session_id = ?1 ORDER BY created_at_ms DESC",
            )?;
            let rows = stmt.query_map([session_id], |row| row.get::<_, String>(0))?;
            for row in rows {
                items.push(row?);
            }
        }
        Ok(items)
    }

    /// Fetch one model input snapshot for a session/run.
    pub fn get_chat_model_input_json(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<Option<String>> {
        let conn = self.connection()?;
        conn.query_row(
            "SELECT snapshot_json FROM chat_model_inputs WHERE session_id = ?1 AND run_id = ?2",
            params![session_id, run_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("Failed to fetch chat model input")
    }

    /// Delete model input snapshots for a session.
    pub fn delete_chat_model_inputs(&self, session_id: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "DELETE FROM chat_model_inputs WHERE session_id = ?1",
            [session_id],
        )
        .context("Failed to delete chat model inputs")?;
        Ok(())
    }

    // ===== Things Change Log =====

    /// Insert a new change log entry and return the auto-generated ID.
    pub fn insert_things_change_log(
        &self,
        device_id: &str,
        op_type: ThingsOperationType,
        entity_type: &str,
        entity_uuid: &str,
        summary: &str,
        details_json: &str,
        parent_log_id: Option<i64>,
        can_undo: bool,
    ) -> Result<i64> {
        #[cfg(test)]
        TEST_THINGS_CHANGE_LOG_INSERT.fetch_add(1, Ordering::Relaxed);

        let conn = self.connection()?;
        let now = Utc::now().timestamp();

        conn.execute(
            r#"INSERT INTO things_change_log
               (device_id, op_type, entity_type, entity_uuid, summary, details_json, parent_log_id, created_at, can_undo, synced)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0)"#,
            params![
                device_id,
                op_type.as_str(),
                entity_type,
                entity_uuid,
                summary,
                details_json,
                parent_log_id,
                now,
                if can_undo { 1 } else { 0 }
            ],
        )
        .context("Failed to insert change log entry")?;

        Ok(conn.last_insert_rowid())
    }

    /// Update cascade log IDs for a parent log entry.
    pub fn update_things_change_log_cascade_ids(
        &self,
        log_id: i64,
        cascade_log_ids: &[i64],
    ) -> Result<()> {
        let conn = self.connection()?;
        let cascade_json = serde_json::to_string(cascade_log_ids)
            .context("Failed to serialize cascade log IDs")?;

        conn.execute(
            "UPDATE things_change_log SET cascade_log_ids_json = ?1 WHERE id = ?2",
            params![cascade_json, log_id],
        )
        .context("Failed to update cascade log IDs")?;

        Ok(())
    }

    /// Mark a change log entry as no longer undoable.
    pub fn mark_things_change_log_not_undoable(&self, log_id: i64) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "UPDATE things_change_log SET can_undo = 0 WHERE id = ?1",
            [log_id],
        )
        .context("Failed to mark log entry as not undoable")?;
        Ok(())
    }

    /// Get a single change log entry by ID.
    pub fn get_things_change_log(&self, log_id: i64) -> Result<Option<ThingsChangeLogEntry>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, device_id, op_type, entity_type, entity_uuid, summary, details_json,
                      parent_log_id, cascade_log_ids_json, created_at, can_undo, synced
               FROM things_change_log WHERE id = ?1"#,
        )?;

        stmt.query_row([log_id], |row| Self::row_to_change_log_entry(row))
            .optional()
            .context("Failed to get change log entry")
    }

    /// List recent change log entries with pagination.
    /// Returns entries ordered by created_at DESC (newest first).
    pub fn list_things_change_log(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<ThingsChangeLogEntry>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, device_id, op_type, entity_type, entity_uuid, summary, details_json,
                      parent_log_id, cascade_log_ids_json, created_at, can_undo, synced
               FROM things_change_log
               ORDER BY created_at DESC
               LIMIT ?1 OFFSET ?2"#,
        )?;

        let rows = stmt.query_map(params![limit, offset], |row| {
            Self::row_to_change_log_entry(row)
        })?;

        rows.collect::<Result<Vec<_>, _>>()
            .context("Failed to list change log entries")
    }

    /// List change log entries for a specific entity.
    pub fn list_things_change_log_for_entity(
        &self,
        entity_type: &str,
        entity_uuid: &str,
        limit: u32,
    ) -> Result<Vec<ThingsChangeLogEntry>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, device_id, op_type, entity_type, entity_uuid, summary, details_json,
                      parent_log_id, cascade_log_ids_json, created_at, can_undo, synced
               FROM things_change_log
               WHERE entity_type = ?1 AND entity_uuid = ?2
               ORDER BY created_at DESC
               LIMIT ?3"#,
        )?;

        let rows = stmt.query_map(params![entity_type, entity_uuid, limit], |row| {
            Self::row_to_change_log_entry(row)
        })?;

        rows.collect::<Result<Vec<_>, _>>()
            .context("Failed to list change log entries for entity")
    }

    /// Get cascade child entries for a parent log entry.
    pub fn get_things_change_log_cascades(
        &self,
        parent_log_id: i64,
    ) -> Result<Vec<ThingsChangeLogEntry>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, device_id, op_type, entity_type, entity_uuid, summary, details_json,
                      parent_log_id, cascade_log_ids_json, created_at, can_undo, synced
               FROM things_change_log
               WHERE parent_log_id = ?1
               ORDER BY id ASC"#,
        )?;

        let rows = stmt.query_map([parent_log_id], |row| Self::row_to_change_log_entry(row))?;

        rows.collect::<Result<Vec<_>, _>>()
            .context("Failed to get cascade log entries")
    }

    /// Find the most recent update log entry for a thing within a time window.
    /// Used for grouping consecutive edits (5-minute window).
    pub fn find_recent_thing_update_log(
        &self,
        thing_uuid: &str,
        within_seconds: i64,
    ) -> Result<Option<ThingsChangeLogEntry>> {
        #[cfg(test)]
        TEST_THINGS_CHANGE_LOG_FIND_RECENT.fetch_add(1, Ordering::Relaxed);

        let conn = self.connection()?;
        let cutoff = Utc::now().timestamp() - within_seconds;

        let mut stmt = conn.prepare(
            r#"SELECT id, device_id, op_type, entity_type, entity_uuid, summary, details_json,
                      parent_log_id, cascade_log_ids_json, created_at, can_undo, synced
               FROM things_change_log
               WHERE entity_type = 'thing' AND entity_uuid = ?1
                 AND op_type = 'update_thing' AND created_at >= ?2
               ORDER BY created_at DESC
               LIMIT 1"#,
        )?;

        stmt.query_row(params![thing_uuid, cutoff], |row| {
            Self::row_to_change_log_entry(row)
        })
        .optional()
        .context("Failed to find recent update log")
    }

    /// Mark a change log entry as undone (can_undo = false).
    pub fn mark_things_change_log_undone(&self, log_id: i64) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "UPDATE things_change_log SET can_undo = 0 WHERE id = ?1",
            [log_id],
        )?;
        Ok(())
    }

    /// Delete old change log entries (retention policy).
    pub fn cleanup_things_change_log(&self, older_than_days: i64) -> Result<u64> {
        let conn = self.connection()?;
        let cutoff = (Utc::now() - Duration::days(older_than_days)).timestamp();

        let deleted = conn.execute(
            "DELETE FROM things_change_log WHERE created_at < ?1",
            [cutoff],
        )? as u64;

        Ok(deleted)
    }

    fn row_to_change_log_entry(
        row: &rusqlite::Row,
    ) -> Result<ThingsChangeLogEntry, rusqlite::Error> {
        let op_type_str: String = row.get(2)?;
        let op_type = op_type_str
            .parse()
            .unwrap_or(ThingsOperationType::UpdateThing);
        let created_at_ts: i64 = row.get(9)?;
        let can_undo_int: i32 = row.get(10)?;
        let synced_int: i32 = row.get(11)?;

        Ok(ThingsChangeLogEntry {
            id: row.get(0)?,
            device_id: row.get(1)?,
            op_type,
            entity_type: row.get(3)?,
            entity_uuid: row.get(4)?,
            summary: row.get(5)?,
            details_json: row.get(6)?,
            parent_log_id: row.get(7)?,
            cascade_log_ids_json: row.get(8)?,
            created_at: Utc
                .timestamp_opt(created_at_ts, 0)
                .single()
                .unwrap_or_else(Utc::now),
            can_undo: can_undo_int != 0,
            synced: synced_int != 0,
        })
    }

    // ===== Things Content Snapshots =====

    /// Insert a content snapshot for a thing.
    pub fn insert_things_content_snapshot(
        &self,
        device_id: &str,
        thing_uuid: &str,
        content_json: &str,
        change_log_id: Option<i64>,
    ) -> Result<i64> {
        let conn = self.connection()?;
        let now = Utc::now().timestamp();

        conn.execute(
            r#"INSERT INTO things_content_snapshots
               (device_id, thing_uuid, content_json, change_log_id, created_at, synced)
               VALUES (?1, ?2, ?3, ?4, ?5, 0)"#,
            params![device_id, thing_uuid, content_json, change_log_id, now],
        )
        .context("Failed to insert content snapshot")?;

        Ok(conn.last_insert_rowid())
    }

    /// Get the most recent content snapshot for a thing.
    pub fn get_latest_things_content_snapshot(
        &self,
        thing_uuid: &str,
    ) -> Result<Option<ThingsContentSnapshot>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, device_id, thing_uuid, content_json, change_log_id, created_at, synced
               FROM things_content_snapshots
               WHERE thing_uuid = ?1
               ORDER BY created_at DESC
               LIMIT 1"#,
        )?;

        stmt.query_row([thing_uuid], |row| Self::row_to_content_snapshot(row))
            .optional()
            .context("Failed to get latest content snapshot")
    }

    /// Get a content snapshot by change log ID.
    pub fn get_things_content_snapshot_by_log_id(
        &self,
        change_log_id: i64,
    ) -> Result<Option<ThingsContentSnapshot>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, device_id, thing_uuid, content_json, change_log_id, created_at, synced
               FROM things_content_snapshots
               WHERE change_log_id = ?1"#,
        )?;

        stmt.query_row([change_log_id], |row| Self::row_to_content_snapshot(row))
            .optional()
            .context("Failed to get content snapshot by log ID")
    }

    /// List content snapshots for a thing with pagination.
    pub fn list_things_content_snapshots(
        &self,
        thing_uuid: &str,
        limit: u32,
    ) -> Result<Vec<ThingsContentSnapshot>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, device_id, thing_uuid, content_json, change_log_id, created_at, synced
               FROM things_content_snapshots
               WHERE thing_uuid = ?1
               ORDER BY created_at DESC
               LIMIT ?2"#,
        )?;

        let rows = stmt.query_map(params![thing_uuid, limit], |row| {
            Self::row_to_content_snapshot(row)
        })?;

        rows.collect::<Result<Vec<_>, _>>()
            .context("Failed to list content snapshots")
    }

    /// Delete old content snapshots (retention policy).
    pub fn cleanup_things_content_snapshots(&self, older_than_days: i64) -> Result<u64> {
        let conn = self.connection()?;
        let cutoff = (Utc::now() - Duration::days(older_than_days)).timestamp();

        let deleted = conn.execute(
            "DELETE FROM things_content_snapshots WHERE created_at < ?1",
            [cutoff],
        )? as u64;

        Ok(deleted)
    }

    fn row_to_content_snapshot(
        row: &rusqlite::Row,
    ) -> Result<ThingsContentSnapshot, rusqlite::Error> {
        let created_at_ts: i64 = row.get(5)?;
        let synced_int: i32 = row.get(6)?;

        Ok(ThingsContentSnapshot {
            id: row.get(0)?,
            device_id: row.get(1)?,
            thing_uuid: row.get(2)?,
            content_json: row.get(3)?,
            change_log_id: row.get(4)?,
            created_at: Utc
                .timestamp_opt(created_at_ts, 0)
                .single()
                .unwrap_or_else(Utc::now),
            synced: synced_int != 0,
        })
    }

    // ===== Change Log Sync Methods =====

    /// Get unsynced change log entries for upload to server.
    pub fn get_unsynced_change_logs(&self, limit: u32) -> Result<Vec<ThingsChangeLogEntry>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, device_id, op_type, entity_type, entity_uuid, summary, details_json,
                      parent_log_id, cascade_log_ids_json, created_at, can_undo, synced
               FROM things_change_log
               WHERE synced = 0
               ORDER BY created_at ASC
               LIMIT ?1"#,
        )?;

        let rows = stmt.query_map([limit], |row| Self::row_to_change_log_entry(row))?;

        rows.collect::<Result<Vec<_>, _>>()
            .context("Failed to get unsynced change logs")
    }

    /// Mark change log entries as synced.
    pub fn mark_change_logs_synced(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self.connection()?;
        let placeholders: Vec<_> = (0..ids.len()).map(|i| format!("?{}", i + 1)).collect();
        let sql = format!(
            "UPDATE things_change_log SET synced = 1 WHERE id IN ({})",
            placeholders.join(", ")
        );
        let params: Vec<_> = ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        conn.execute(&sql, params.as_slice())
            .context("Failed to mark change logs as synced")?;
        Ok(())
    }

    /// Get unsynced content snapshots for upload to server.
    pub fn get_unsynced_content_snapshots(&self, limit: u32) -> Result<Vec<ThingsContentSnapshot>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, device_id, thing_uuid, content_json, change_log_id, created_at, synced
               FROM things_content_snapshots
               WHERE synced = 0
               ORDER BY created_at ASC
               LIMIT ?1"#,
        )?;

        let rows = stmt.query_map([limit], |row| Self::row_to_content_snapshot(row))?;

        rows.collect::<Result<Vec<_>, _>>()
            .context("Failed to get unsynced content snapshots")
    }

    /// Mark content snapshots as synced.
    pub fn mark_content_snapshots_synced(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self.connection()?;
        let placeholders: Vec<_> = (0..ids.len()).map(|i| format!("?{}", i + 1)).collect();
        let sql = format!(
            "UPDATE things_content_snapshots SET synced = 1 WHERE id IN ({})",
            placeholders.join(", ")
        );
        let params: Vec<_> = ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        conn.execute(&sql, params.as_slice())
            .context("Failed to mark content snapshots as synced")?;
        Ok(())
    }

    /// Insert a change log entry received from server (already synced).
    pub fn insert_synced_change_log(
        &self,
        device_id: &str,
        op_type: ThingsOperationType,
        entity_type: &str,
        entity_uuid: &str,
        summary: &str,
        details_json: &str,
        created_at: i64,
    ) -> Result<i64> {
        let conn = self.connection()?;

        conn.execute(
            r#"INSERT INTO things_change_log
               (device_id, op_type, entity_type, entity_uuid, summary, details_json, parent_log_id, created_at, can_undo, synced)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, 0, 1)"#,
            params![
                device_id,
                op_type.as_str(),
                entity_type,
                entity_uuid,
                summary,
                details_json,
                created_at,
            ],
        )
        .context("Failed to insert synced change log entry")?;

        Ok(conn.last_insert_rowid())
    }

    /// Insert a content snapshot received from server (already synced).
    pub fn insert_synced_content_snapshot(
        &self,
        device_id: &str,
        thing_uuid: &str,
        content_json: &str,
        created_at: i64,
    ) -> Result<i64> {
        let conn = self.connection()?;

        conn.execute(
            r#"INSERT INTO things_content_snapshots
               (device_id, thing_uuid, content_json, change_log_id, created_at, synced)
               VALUES (?1, ?2, ?3, NULL, ?4, 1)"#,
            params![device_id, thing_uuid, content_json, created_at],
        )
        .context("Failed to insert synced content snapshot")?;

        Ok(conn.last_insert_rowid())
    }

    // ────────────────────────────────────────────────────────────────────────────
    // Location cache operations
    // ────────────────────────────────────────────────────────────────────────────

    /// Get a location from cache by name.
    pub fn get_location_cache(&self, name: &str) -> Result<Option<LocationCacheEntry>> {
        let conn = self.connection()?;
        let entry = conn
            .query_row(
                r#"SELECT name, is_fuzzy, latitude, longitude, coord_system,
                          place_id, place_type, formatted_address, created_at, updated_at
                   FROM location_cache
                   WHERE name = ?1"#,
                params![name],
                |row| {
                    let coord_system_str: String = row.get(4)?;
                    let coord_system = CoordinateSystem::from_str(&coord_system_str)
                        .unwrap_or(CoordinateSystem::Wgs84);

                    Ok(LocationCacheEntry {
                        name: row.get(0)?,
                        is_fuzzy: row.get::<_, i32>(1)? != 0,
                        latitude: row.get(2)?,
                        longitude: row.get(3)?,
                        coord_system,
                        place_id: row.get(5)?,
                        place_type: row.get(6)?,
                        formatted_address: row.get(7)?,
                        created_at: row.get(8)?,
                        updated_at: row.get(9)?,
                    })
                },
            )
            .optional()?;
        Ok(entry)
    }

    /// Insert or update a location cache entry.
    pub fn set_location_cache(&self, entry: &LocationCacheEntry) -> Result<i64> {
        let conn = self.connection()?;
        let now = Utc::now().timestamp();

        conn.execute(
            r#"INSERT INTO location_cache
               (name, is_fuzzy, latitude, longitude, coord_system, place_id, place_type, formatted_address, created_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
               ON CONFLICT(name) DO UPDATE SET
                   is_fuzzy = excluded.is_fuzzy,
                   latitude = excluded.latitude,
                   longitude = excluded.longitude,
                   coord_system = excluded.coord_system,
                   place_id = excluded.place_id,
                   place_type = excluded.place_type,
                   formatted_address = excluded.formatted_address,
                   updated_at = excluded.updated_at"#,
            params![
                entry.name,
                if entry.is_fuzzy { 1 } else { 0 },
                entry.latitude,
                entry.longitude,
                entry.coord_system.as_str(),
                entry.place_id,
                entry.place_type,
                entry.formatted_address,
                now,
            ],
        )
        .context("Failed to insert location cache entry")?;

        Ok(conn.last_insert_rowid())
    }

    /// Delete a location cache entry by name.
    pub fn delete_location_cache(&self, name: &str) -> Result<bool> {
        let conn = self.connection()?;
        let rows = conn
            .execute("DELETE FROM location_cache WHERE name = ?1", params![name])
            .context("Failed to delete location cache entry")?;
        Ok(rows > 0)
    }

    /// Clear all location cache entries.
    pub fn clear_location_cache(&self) -> Result<usize> {
        let conn = self.connection()?;
        let rows = conn
            .execute("DELETE FROM location_cache", [])
            .context("Failed to clear location cache")?;
        Ok(rows)
    }

    /// Get all non-fuzzy (exact) location cache entries.
    pub fn get_all_exact_locations(&self) -> Result<Vec<LocationCacheEntry>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            r#"SELECT name, is_fuzzy, latitude, longitude, coord_system,
                      place_id, place_type, formatted_address, created_at, updated_at
               FROM location_cache
               WHERE is_fuzzy = 0
               ORDER BY name ASC"#,
        )?;

        let rows = stmt.query_map([], |row| {
            let coord_system_str: String = row.get(4)?;
            let coord_system =
                CoordinateSystem::from_str(&coord_system_str).unwrap_or(CoordinateSystem::Wgs84);

            Ok(LocationCacheEntry {
                name: row.get(0)?,
                is_fuzzy: row.get::<_, i32>(1)? != 0,
                latitude: row.get(2)?,
                longitude: row.get(3)?,
                coord_system,
                place_id: row.get(5)?,
                place_type: row.get(6)?,
                formatted_address: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>()
            .context("Failed to get exact locations from cache")
    }

    // ===== Things Actor Metadata cache =====

    /// Batch-upsert actor metadata for things and collections fetched from the server.
    pub fn upsert_things_actor_meta_batch(&self, items: &[ActorMetaEntry]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let now = Utc::now().timestamp();
        for item in items {
            tx.execute(
                r#"INSERT INTO things_actor_meta (uuid, is_collection, actor_type, actor_app_id, actor_display_name, updated_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                   ON CONFLICT(uuid) DO UPDATE SET
                     actor_type = excluded.actor_type,
                     actor_app_id = excluded.actor_app_id,
                     actor_display_name = excluded.actor_display_name,
                     updated_at = excluded.updated_at"#,
                params![
                    item.uuid,
                    if item.is_collection { 1i32 } else { 0i32 },
                    item.actor_type,
                    item.actor_app_id,
                    item.actor_display_name,
                    now,
                ],
            )
            .context("Failed to upsert actor meta entry")?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Load all actor meta entries as a lookup map (uuid → entry).
    pub fn load_things_actor_meta_map(
        &self,
    ) -> Result<std::collections::HashMap<String, ActorMetaEntry>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT uuid, is_collection, actor_type, actor_app_id, actor_display_name FROM things_actor_meta",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ActorMetaEntry {
                uuid: row.get(0)?,
                is_collection: row.get::<_, i32>(1)? != 0,
                actor_type: row.get(2)?,
                actor_app_id: row.get(3)?,
                actor_display_name: row.get(4)?,
            })
        })?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let entry = row.context("Failed to read actor meta row")?;
            map.insert(entry.uuid.clone(), entry);
        }
        Ok(map)
    }
}

/// Actor attribution metadata entry (cached from server).
#[derive(Debug, Clone)]
pub struct ActorMetaEntry {
    pub uuid: String,
    pub is_collection: bool,
    pub actor_type: String,
    pub actor_app_id: Option<String>,
    pub actor_display_name: Option<String>,
}

#[cfg(test)]
mod tests {
    // TODO: Add tests for new JSON rule-based trigger storage
}
