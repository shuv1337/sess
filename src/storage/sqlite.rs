use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
    types::Type as SqliteType,
};

#[cfg(test)]
use rusqlite::types::Value as SqliteValue;

use crate::model::{
    Agent, Conversation, ConversationMetadata, Message, Role, SourceFile, UsageGrain,
    UsageMetadata, UsageRecord,
};
use crate::usage::{SourceCoverage, TokenCounts, UsageDataset, UsageEventRow};

/// A DB row whose backing source file is missing on disk.
#[derive(Debug, Clone)]
pub struct MissingSource {
    pub id: i64,
    pub agent: Agent,
    pub source_path: PathBuf,
}

/// Outcome of a stale-deletion sweep.
///
/// `deleted_*` are the rows that were actually removed from SQLite. `uncertain_paths`
/// are rows where the existence check returned an error (permission denied,
/// transient mount errors, etc.) — those rows are intentionally kept.
#[derive(Debug, Default, Clone)]
pub struct StaleDeletionSummary {
    pub deleted_ids: Vec<i64>,
    pub deleted_paths: Vec<PathBuf>,
    pub uncertain_paths: Vec<(i64, PathBuf, String)>,
}

pub type MissingSourceClassification = (Vec<MissingSource>, Vec<(i64, PathBuf, String)>);

fn invalid_persisted_enum(column: usize, kind: &str, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        SqliteType::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Unknown persisted {kind}: {value}"),
        )),
    )
}

fn agent_from_slug(slug: &str, column: usize) -> rusqlite::Result<Agent> {
    match slug {
        "claude_code" => Ok(Agent::ClaudeCode),
        "codex" => Ok(Agent::Codex),
        "hermes" => Ok(Agent::Hermes),
        "opencode" => Ok(Agent::OpenCode),
        "pi_agent" => Ok(Agent::PiAgent),
        _ => Err(invalid_persisted_enum(column, "agent", slug)),
    }
}

fn role_from_slug(slug: &str, column: usize) -> rusqlite::Result<Role> {
    match slug {
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        "system" => Ok(Role::System),
        _ => Err(invalid_persisted_enum(column, "message role", slug)),
    }
}

fn usage_grain_from_slug(slug: &str, column: usize) -> rusqlite::Result<UsageGrain> {
    match slug {
        "event" => Ok(UsageGrain::Event),
        "interval_aggregate" => Ok(UsageGrain::IntervalAggregate),
        "session_aggregate" => Ok(UsageGrain::SessionAggregate),
        _ => Err(invalid_persisted_enum(column, "usage grain", slug)),
    }
}

fn latest_supported_schema_version() -> u32 {
    MIGRATIONS.last().map_or(0, |migration| migration.version)
}

fn migration_table_exists(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'schema_migrations'
         )",
        [],
        |row| row.get(0),
    )?)
}

fn ensure_supported_schema_version(conn: &Connection) -> Result<()> {
    if !migration_table_exists(conn)? {
        return Ok(());
    }

    let version: Option<i64> =
        conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })?;
    if let Some(version) = version {
        let supported = i64::from(latest_supported_schema_version());
        if version > supported {
            anyhow::bail!(
                "Database schema version {version} is newer than this sess build supports ({supported})"
            );
        }
        if version < 0 {
            anyhow::bail!("Database contains an invalid negative schema version: {version}");
        }
    }
    Ok(())
}

fn migrations_are_current(conn: &Connection) -> Result<bool> {
    if !migration_table_exists(conn)? {
        return Ok(false);
    }
    ensure_supported_schema_version(conn)?;
    for migration in MIGRATIONS {
        let applied = conn
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = ?",
                [migration.version],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some();
        if !applied {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Migration definition
pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial",
        sql: include_str!("migrations/001_initial.sql"),
    },
    Migration {
        version: 2,
        name: "add_embeddings",
        sql: include_str!("migrations/002_add_embeddings.sql"),
    },
    Migration {
        version: 3,
        name: "add_usage_events",
        sql: include_str!("migrations/003_add_usage_events.sql"),
    },
    Migration {
        version: 4,
        name: "usage_provenance",
        sql: include_str!("migrations/004_usage_provenance.sql"),
    },
];

/// Storage statistics
#[derive(Debug, Clone, Default)]
pub struct StorageStats {
    pub total_conversations: usize,
    pub total_messages: usize,
    pub by_agent: HashMap<Agent, AgentStats>,
    pub db_size_bytes: u64,
    pub last_indexed_at: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentStats {
    pub conversations: usize,
    pub messages: usize,
}

/// Upsert outcome
#[derive(Debug, Clone)]
pub struct UpsertOutcome {
    pub conversation_id: i64,
    pub changed: bool,
    pub inserted: bool,
}

/// SQLite storage backend
pub struct Storage {
    conn: Connection,
    path: PathBuf,
}

impl Storage {
    /// Open or create the database
    pub fn new(path: &Path) -> Result<Self> {
        fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))
            .context("Failed to create database directory")?;

        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open database at {}", path.display()))?;

        // Enable WAL mode and other optimizations
        conn.execute_batch(
            "PRAGMA busy_timeout = 5000;  -- 5s, lets concurrent opens/indexes wait
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA cache_size = -64000;  -- 64MB
            ",
        )
        .context("Failed to set database pragmas")?;

        let mut storage = Self {
            conn,
            path: path.to_path_buf(),
        };

        storage.run_migrations()?;

        Ok(storage)
    }

    /// Open an existing database without creating files, changing persistent
    /// journal state, or applying migrations. This is used by index previews
    /// whose contract is filesystem read-only, including against an older schema.
    pub fn open_read_only(path: &Path) -> Result<Option<Self>> {
        if !path
            .try_exists()
            .with_context(|| format!("Failed to inspect database path {}", path.display()))?
        {
            return Ok(None);
        }

        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("Failed to open database read-only at {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA query_only = ON;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )
        .context("Failed to configure read-only database connection")?;
        ensure_supported_schema_version(&conn)?;

        Ok(Some(Self {
            conn,
            path: path.to_path_buf(),
        }))
    }

    /// Run database migrations
    fn run_migrations(&mut self) -> Result<()> {
        // Avoid taking a writer lock on every read-oriented CLI invocation once
        // the schema is current. The locked path below rechecks all markers, so
        // this optimistic check does not weaken concurrent first-open safety.
        if migrations_are_current(&self.conn)? {
            return Ok(());
        }

        // Acquire the writer lock before inspecting migration markers. A second
        // process opening the same database waits, then observes the markers
        // committed by the first instead of racing the same ALTER statements.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at INTEGER NOT NULL
            )",
            [],
        )?;
        ensure_supported_schema_version(&tx)?;

        for migration in MIGRATIONS {
            let exists = tx
                .query_row(
                    "SELECT 1 FROM schema_migrations WHERE version = ?",
                    [migration.version],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .is_some();

            if !exists {
                tracing::info!(
                    "Applying migration {}: {}",
                    migration.version,
                    migration.name
                );
                // Schema changes and their markers share this transaction, so
                // interruption cannot leave a partially applied migration.
                tx.execute_batch(migration.sql)?;
                tx.execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?, ?)",
                    rusqlite::params![migration.version, chrono::Utc::now().timestamp_millis(),],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// True if a row with this source_path already exists.
    pub fn has_source_path(&self, source_path: &Path) -> Result<bool> {
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM conversations WHERE source_path = ?",
                [source_path.to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(existing.is_some())
    }

    /// All `(id, agent, source_path)` rows. Used by dry-run preview.
    pub fn list_all_source_rows(&self) -> Result<Vec<(i64, Agent, PathBuf)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, agent, source_path FROM conversations")?;
        let rows = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let agent_slug: String = row.get(1)?;
                let path: String = row.get(2)?;
                Ok((id, agent_from_slug(&agent_slug, 1)?, PathBuf::from(path)))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Check if a conversation needs reindexing
    pub fn needs_reindex(&self, source_path: &Path, fingerprint: &str) -> Result<bool> {
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT source_fingerprint FROM conversations WHERE source_path = ?",
                [source_path.to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .optional()?;

        match existing {
            Some(existing_fp) => Ok(existing_fp != fingerprint),
            None => Ok(true),
        }
    }

    /// Upsert a conversation
    pub fn upsert_conversation(&mut self, conv: &Conversation) -> Result<UpsertOutcome> {
        let tx = self.conn.transaction()?;

        let source_path_str = conv.source_path.to_string_lossy().to_string();
        let workspace_str = conv
            .workspace
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let indexed_at = chrono::Utc::now().timestamp_millis();
        let mtime_max = conv.source_mtime_max();
        let logical_session_id = conv.metadata.logical_session_id.as_deref().or_else(|| {
            conv.external_id
                .as_deref()
                .filter(|value| !value.is_empty())
        });
        let record_kind = conv
            .metadata
            .record_kind
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("top_level");
        let is_synthetic = conv.metadata.is_synthetic
            || conv.usage.iter().any(|record| {
                record
                    .provider
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case("faux"))
                    || record
                        .model
                        .as_deref()
                        .is_some_and(|value| value.eq_ignore_ascii_case("faux"))
            });

        // Check if conversation exists
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM conversations WHERE source_path = ?",
                [&source_path_str],
                |row| row.get(0),
            )
            .optional()?;

        let (conv_id, inserted, changed) = if let Some(id) = existing {
            // Check fingerprint to see if we need to update
            let existing_fp: String = tx.query_row(
                "SELECT source_fingerprint FROM conversations WHERE id = ?",
                [id],
                |row| row.get(0),
            )?;

            if existing_fp != conv.source_fingerprint {
                // Update the conversation
                tx.execute(
                    "UPDATE conversations SET
                        agent = ?,
                        external_id = ?,
                        title = ?,
                        workspace = ?,
                        started_at = ?,
                        ended_at = ?,
                        indexed_at = ?,
                        source_mtime_max = ?,
                        source_fingerprint = ?,
                        logical_session_id = ?,
                        parent_external_id = ?,
                        record_kind = ?,
                        is_synthetic = ?
                    WHERE id = ?",
                    rusqlite::params![
                        conv.agent.slug(),
                        conv.external_id.as_deref(),
                        conv.derive_title().as_str(),
                        workspace_str.as_deref(),
                        conv.started_at,
                        conv.ended_at,
                        indexed_at,
                        mtime_max,
                        &conv.source_fingerprint,
                        logical_session_id,
                        conv.metadata.parent_external_id.as_deref(),
                        record_kind,
                        is_synthetic,
                        id,
                    ],
                )?;

                // Delete old messages
                tx.execute("DELETE FROM messages WHERE conversation_id = ?", [id])?;
                tx.execute("DELETE FROM usage_events WHERE conversation_id = ?", [id])?;

                // Insert new messages
                insert_messages(&tx, id, &conv.messages)?;
                insert_usage_events(&tx, id, &conv.usage)?;

                (id, false, true)
            } else {
                // No change needed
                (id, false, false)
            }
        } else {
            // Insert new conversation
            tx.execute(
                "INSERT INTO conversations
                    (agent, external_id, title, workspace, source_path,
                     started_at, ended_at, indexed_at, source_mtime_max, source_fingerprint,
                     logical_session_id, parent_external_id, record_kind, is_synthetic)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    conv.agent.slug(),
                    conv.external_id.as_deref(),
                    conv.derive_title().as_str(),
                    workspace_str.as_deref(),
                    &source_path_str,
                    conv.started_at,
                    conv.ended_at,
                    indexed_at,
                    mtime_max,
                    &conv.source_fingerprint,
                    logical_session_id,
                    conv.metadata.parent_external_id.as_deref(),
                    record_kind,
                    is_synthetic,
                ],
            )?;

            let id = tx.last_insert_rowid();

            // Insert messages
            insert_messages(&tx, id, &conv.messages)?;
            insert_usage_events(&tx, id, &conv.usage)?;

            (id, true, true)
        };

        tx.commit()?;

        Ok(UpsertOutcome {
            conversation_id: conv_id,
            changed,
            inserted,
        })
    }

    /// Get all conversations (for rebuilding Tantivy index)
    pub fn get_all_conversations(&self) -> Result<Vec<ConversationRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, agent, external_id, title, workspace, source_path,
                    started_at, ended_at, source_fingerprint, logical_session_id,
                    parent_external_id, record_kind, is_synthetic
             FROM conversations
             ORDER BY id",
        )?;

        let rows = stmt.query_map([], |row| {
            let agent_slug: String = row.get(1)?;
            let agent = agent_from_slug(&agent_slug, 1)?;

            Ok(ConversationRow {
                id: row.get(0)?,
                agent,
                external_id: row.get(2)?,
                title: row.get(3)?,
                workspace: row.get(4)?,
                source_path: row.get(5)?,
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                source_fingerprint: row.get(8)?,
                logical_session_id: row.get(9)?,
                parent_external_id: row.get(10)?,
                record_kind: row.get(11)?,
                is_synthetic: row.get(12)?,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }

        Ok(results)
    }

    /// Get a conversation with all messages
    pub fn get_conversation(&self, id: i64) -> Result<Option<Conversation>> {
        let row: Option<ConversationRow> = self
            .conn
            .query_row(
                "SELECT id, agent, external_id, title, workspace, source_path,
                    started_at, ended_at, source_fingerprint, logical_session_id,
                    parent_external_id, record_kind, is_synthetic
             FROM conversations WHERE id = ?",
                [id],
                |row| {
                    let agent_slug: String = row.get(1)?;
                    let agent = agent_from_slug(&agent_slug, 1)?;

                    Ok(ConversationRow {
                        id: row.get(0)?,
                        agent,
                        external_id: row.get(2)?,
                        title: row.get(3)?,
                        workspace: row.get(4)?,
                        source_path: row.get(5)?,
                        started_at: row.get(6)?,
                        ended_at: row.get(7)?,
                        source_fingerprint: row.get(8)?,
                        logical_session_id: row.get(9)?,
                        parent_external_id: row.get(10)?,
                        record_kind: row.get(11)?,
                        is_synthetic: row.get(12)?,
                    })
                },
            )
            .optional()?;

        let row = match row {
            Some(r) => r,
            None => return Ok(None),
        };

        // Get messages
        let messages = self.get_messages(row.id)?;
        let usage = self.get_usage(row.id)?;

        // Reconstruct source_files from the fingerprint (simplified)
        let source_file = SourceFile {
            path: PathBuf::from(&row.source_path),
            mtime: 0,
            size: 0,
        };

        Ok(Some(Conversation {
            agent: row.agent,
            external_id: row.external_id,
            title: row.title,
            workspace: row.workspace.map(PathBuf::from),
            source_path: PathBuf::from(row.source_path),
            source_files: vec![source_file],
            source_fingerprint: row.source_fingerprint,
            started_at: row.started_at,
            ended_at: row.ended_at,
            messages,
            usage,
            metadata: ConversationMetadata {
                logical_session_id: row.logical_session_id,
                parent_external_id: row.parent_external_id,
                record_kind: Some(row.record_kind),
                is_synthetic: row.is_synthetic,
            },
        }))
    }

    /// Get messages for a conversation
    pub fn get_messages(&self, conversation_id: i64) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT idx, role, content, timestamp, model
             FROM messages
             WHERE conversation_id = ?
             ORDER BY idx",
        )?;

        let rows = stmt.query_map([conversation_id], |row| {
            let role_str: String = row.get(1)?;
            let role = role_from_slug(&role_str, 1)?;

            Ok(Message {
                idx: row.get(0)?,
                role,
                content: row.get(2)?,
                timestamp: row.get(3)?,
                model: row.get(4)?,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }

        Ok(results)
    }

    /// Get normalized provider usage for a conversation.
    pub fn get_usage(&self, conversation_id: i64) -> Result<Vec<UsageRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT timestamp, provider, model, source_event_id, api_calls, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens, reasoning_tokens,
                    total_tokens, actual_cost_usd, estimated_cost_usd,
                    interval_start, interval_end, usage_grain, provider_family,
                    provider_inference_source, provider_inference_confidence,
                    model_family, model_variant, task, billing_base_url, billing_mode,
                    request_attempts, reported_total_tokens, component_total_tokens,
                    token_semantics, cost_status, cost_source, cost_currency, pricing_version
             FROM usage_events
             WHERE conversation_id = ?
             ORDER BY idx",
        )?;
        let rows = stmt.query_map([conversation_id], |row| {
            Ok(UsageRecord {
                timestamp: row.get(0)?,
                provider: row.get(1)?,
                model: row.get(2)?,
                source_event_id: row.get(3)?,
                api_calls: row.get(4)?,
                input_tokens: row.get(5)?,
                output_tokens: row.get(6)?,
                cache_read_tokens: row.get(7)?,
                cache_write_tokens: row.get(8)?,
                reasoning_tokens: row.get(9)?,
                total_tokens: row.get(10)?,
                actual_cost_usd: row.get(11)?,
                estimated_cost_usd: row.get(12)?,
                metadata: UsageMetadata {
                    interval_start: row.get(13)?,
                    interval_end: row.get(14)?,
                    grain: usage_grain_from_slug(&row.get::<_, String>(15)?, 15)?,
                    provider_family: row.get(16)?,
                    provider_inference_source: row.get(17)?,
                    provider_inference_confidence: row.get(18)?,
                    model_family: row.get(19)?,
                    model_variant: row.get(20)?,
                    task: row.get(21)?,
                    billing_base_url: row.get(22)?,
                    billing_mode: row.get(23)?,
                    request_attempts: row.get::<_, i64>(24)?.max(0) as u64,
                    reported_total_tokens: row
                        .get::<_, Option<i64>>(25)?
                        .map(|value| value.max(0) as u64),
                    component_total_tokens: row
                        .get::<_, Option<i64>>(26)?
                        .map(|value| value.max(0) as u64),
                    token_semantics: row.get(27)?,
                    cost_status: row.get(28)?,
                    cost_source: row.get(29)?,
                    cost_currency: row.get(30)?,
                    pricing_version: row.get(31)?,
                },
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Load normalized usage events with the conversation dimensions needed by
    /// the analytics renderers.
    pub fn usage_dataset(&self) -> Result<UsageDataset> {
        let mut stmt = self.conn.prepare(
            "SELECT u.conversation_id, c.agent, c.workspace, c.logical_session_id,
                    c.record_kind, c.is_synthetic, u.timestamp, u.interval_start,
                    u.interval_end, u.usage_grain, u.provider, u.provider_family,
                    u.provider_inference_source, u.provider_inference_confidence,
                    u.model, u.model_family, u.model_variant, u.task,
                    u.billing_base_url, u.billing_mode, u.source_event_id, u.api_calls,
                    u.request_attempts, u.input_tokens, u.output_tokens,
                    u.cache_read_tokens, u.cache_write_tokens, u.reasoning_tokens,
                    u.total_tokens, u.reported_total_tokens, u.component_total_tokens,
                    u.token_semantics, u.actual_cost_usd, u.estimated_cost_usd,
                    u.cost_status, u.cost_source, u.cost_currency, u.pricing_version
             FROM usage_events u
             JOIN conversations c ON c.id = u.conversation_id
             ORDER BY COALESCE(u.timestamp, u.interval_start), u.conversation_id, u.idx",
        )?;
        let rows = stmt.query_map([], |row| {
            let agent: String = row.get(1)?;
            let nonnegative = |value: i64| value.max(0) as u64;
            Ok(UsageEventRow {
                conversation_id: row.get(0)?,
                agent: agent_from_slug(&agent, 1)?,
                workspace: row.get(2)?,
                logical_session_id: row.get(3)?,
                record_kind: row.get(4)?,
                is_synthetic: row.get(5)?,
                timestamp: row.get(6)?,
                interval_start: row.get(7)?,
                interval_end: row.get(8)?,
                usage_grain: usage_grain_from_slug(&row.get::<_, String>(9)?, 9)?,
                provider: row.get(10)?,
                provider_family: row.get(11)?,
                provider_inference_source: row.get(12)?,
                provider_inference_confidence: row.get(13)?,
                model: row.get(14)?,
                model_family: row.get(15)?,
                model_variant: row.get(16)?,
                task: row.get(17)?,
                billing_base_url: row.get(18)?,
                billing_mode: row.get(19)?,
                source_event_id: row.get(20)?,
                api_calls: nonnegative(row.get(21)?),
                request_attempts: nonnegative(row.get(22)?),
                tokens: TokenCounts {
                    input: nonnegative(row.get(23)?),
                    output: nonnegative(row.get(24)?),
                    cache_read: nonnegative(row.get(25)?),
                    cache_write: nonnegative(row.get(26)?),
                    reasoning: nonnegative(row.get(27)?),
                    total: nonnegative(row.get(28)?),
                },
                reported_total_tokens: row.get::<_, Option<i64>>(29)?.map(nonnegative),
                component_total_tokens: row.get::<_, Option<i64>>(30)?.map(nonnegative),
                token_semantics: row.get(31)?,
                actual_cost_usd: row.get(32)?,
                estimated_cost_usd: row.get(33)?,
                cost_status: row.get(34)?,
                cost_source: row.get(35)?,
                cost_currency: row.get(36)?,
                pricing_version: row.get(37)?,
            })
        })?;
        let events = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut coverage_stmt = self.conn.prepare(
            "WITH message_counts AS (
                 SELECT conversation_id,
                        SUM(CASE WHEN role = 'assistant' THEN 1 ELSE 0 END) AS assistant_messages
                 FROM messages GROUP BY conversation_id
             ), usage_counts AS (
                 SELECT conversation_id, COUNT(*) AS usage_records,
                        SUM(api_calls) AS api_calls,
                        SUM(request_attempts) AS request_attempts,
                        SUM(total_tokens) AS total_tokens
                 FROM usage_events GROUP BY conversation_id
             )
             SELECT c.agent, COUNT(*),
                    COUNT(DISTINCT COALESCE(NULLIF(c.logical_session_id, ''), 'record:' || c.id)),
                    SUM(COALESCE(m.assistant_messages, 0)),
                    SUM(CASE WHEN COALESCE(m.assistant_messages, 0) > 0 THEN 1 ELSE 0 END),
                    SUM(COALESCE(u.usage_records, 0)),
                    SUM(CASE WHEN COALESCE(u.usage_records, 0) > 0 THEN 1 ELSE 0 END),
                    SUM(CASE WHEN COALESCE(m.assistant_messages, 0) > 0
                                  AND COALESCE(u.usage_records, 0) = 0 THEN 1 ELSE 0 END),
                    SUM(COALESCE(u.api_calls, 0)),
                    SUM(COALESCE(u.request_attempts, 0)),
                    SUM(COALESCE(u.total_tokens, 0))
             FROM conversations c
             LEFT JOIN message_counts m ON m.conversation_id = c.id
             LEFT JOIN usage_counts u ON u.conversation_id = c.id
             GROUP BY c.agent ORDER BY c.agent",
        )?;
        let source_coverage = coverage_stmt
            .query_map([], |row| {
                Ok(SourceCoverage {
                    agent: row.get(0)?,
                    transcript_records: nonnegative_i64(row.get(1)?),
                    logical_sessions: nonnegative_i64(row.get(2)?),
                    assistant_messages: nonnegative_i64(row.get(3)?),
                    assistant_records: nonnegative_i64(row.get(4)?),
                    usage_records: nonnegative_i64(row.get(5)?),
                    usage_bearing_records: nonnegative_i64(row.get(6)?),
                    assistant_without_usage_records: nonnegative_i64(row.get(7)?),
                    api_calls: nonnegative_i64(row.get(8)?),
                    request_attempts: nonnegative_i64(row.get(9)?),
                    total_tokens: nonnegative_i64(row.get(10)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let stats = self.stats()?;
        Ok(UsageDataset {
            events,
            indexed_conversations: stats.total_conversations as u64,
            indexed_messages: stats.total_messages as u64,
            source_coverage,
        })
    }

    /// Find DB rows whose backing source file no longer exists on disk.
    ///
    /// Only inspects rows for agents in `detected_agents`. Rows whose agent is
    /// not currently detected (env var/root temporarily disappeared) are skipped
    /// to avoid wiping the entire agent's history.
    ///
    /// Existence is checked via [`Path::try_exists`], which is tri-state:
    /// - `Ok(true)` -> keep
    /// - `Ok(false)` -> include in result (deletion candidate)
    /// - `Err(_)` -> caller's responsibility; this function only returns confirmed misses.
    ///   Use [`Self::delete_missing_sources`] to also collect uncertain rows.
    pub fn find_missing_sources(
        &self,
        detected_agents: &HashSet<Agent>,
    ) -> Result<Vec<MissingSource>> {
        let (missing, _) =
            self.classify_missing_sources(detected_agents, |_, path| Ok(path.try_exists()?))?;
        Ok(missing)
    }

    /// Classify stale rows using a connector-aware source existence check.
    /// Database connectors use this for virtual per-session source paths.
    pub fn classify_missing_sources<F>(
        &self,
        detected_agents: &HashSet<Agent>,
        source_exists: F,
    ) -> Result<MissingSourceClassification>
    where
        F: Fn(Agent, &Path) -> Result<bool>,
    {
        let mut stmt = self
            .conn
            .prepare("SELECT id, agent, source_path FROM conversations")?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut missing = Vec::new();
        let mut uncertain = Vec::new();
        for (id, agent_slug, path) in rows {
            let agent = agent_from_slug(&agent_slug, 1)?;
            if !detected_agents.contains(&agent) {
                continue;
            }
            let p = PathBuf::from(&path);
            match source_exists(agent, &p) {
                Ok(true) => {}
                Ok(false) => missing.push(MissingSource {
                    id,
                    agent,
                    source_path: p,
                }),
                Err(error) => uncertain.push((id, p, error.to_string())),
            }
        }
        Ok((missing, uncertain))
    }

    /// Delete DB rows whose backing source file is confirmed missing on disk.
    ///
    /// Fail-safe semantics:
    /// - Rows for agents not in `detected_agents` are always kept.
    /// - Rows whose existence check errors are kept and reported in
    ///   `uncertain_paths` with the OS error message.
    /// - Only rows whose `try_exists()` returns `Ok(false)` are deleted.
    ///
    /// Returns the IDs and paths deleted plus the uncertain rows. The caller
    /// is responsible for issuing the corresponding Tantivy deletions so the
    /// two stores stay consistent.
    pub fn delete_missing_sources(
        &mut self,
        detected_agents: &HashSet<Agent>,
    ) -> Result<StaleDeletionSummary> {
        self.delete_missing_sources_with(detected_agents, |_, path| Ok(path.try_exists()?))
    }

    /// Delete stale rows using a connector-aware source existence check.
    pub fn delete_missing_sources_with<F>(
        &mut self,
        detected_agents: &HashSet<Agent>,
        source_exists: F,
    ) -> Result<StaleDeletionSummary>
    where
        F: Fn(Agent, &Path) -> Result<bool>,
    {
        let (to_delete, uncertain) =
            self.classify_missing_sources(detected_agents, source_exists)?;

        let mut summary = StaleDeletionSummary {
            uncertain_paths: uncertain,
            ..Default::default()
        };

        if to_delete.is_empty() {
            return Ok(summary);
        }

        let tx = self.conn.transaction()?;
        for missing in &to_delete {
            tx.execute("DELETE FROM conversations WHERE id = ?", [missing.id])?;
            summary.deleted_ids.push(missing.id);
            summary.deleted_paths.push(missing.source_path.clone());
        }
        tx.commit()?;

        if !summary.uncertain_paths.is_empty() {
            for (id, p, err) in &summary.uncertain_paths {
                tracing::warn!(
                    "Could not verify existence of source for conversation {} ({}): {} — keeping row",
                    id,
                    p.display(),
                    err
                );
            }
        }

        Ok(summary)
    }

    /// Get storage statistics
    pub fn stats(&self) -> Result<StorageStats> {
        let total_conversations: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM conversations", [], |row| row.get(0))?;

        let total_messages: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;

        // Get stats by agent
        let mut stmt = self.conn.prepare(
            "SELECT agent, COUNT(*) as conv_count,
             (SELECT COUNT(*) FROM messages WHERE conversation_id IN
              (SELECT id FROM conversations c2 WHERE c2.agent = c1.agent)) as msg_count
             FROM conversations c1
             GROUP BY agent",
        )?;

        let mut by_agent = HashMap::new();
        let rows = stmt.query_map([], |row| {
            let agent_slug: String = row.get(0)?;
            let agent = agent_from_slug(&agent_slug, 0)?;

            Ok((
                agent,
                AgentStats {
                    conversations: row.get::<_, i64>(1)? as usize,
                    messages: row.get::<_, i64>(2)? as usize,
                },
            ))
        })?;

        for row in rows {
            let (agent, stats) = row?;
            by_agent.insert(agent, stats);
        }

        // Get database file size
        let db_size = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);

        // Get last indexed timestamp
        let last_indexed: Option<i64> =
            self.conn
                .query_row("SELECT MAX(indexed_at) FROM conversations", [], |row| {
                    row.get::<_, Option<i64>>(0)
                })?;

        Ok(StorageStats {
            total_conversations: total_conversations as usize,
            total_messages: total_messages as usize,
            by_agent,
            db_size_bytes: db_size,
            last_indexed_at: last_indexed,
        })
    }

    /// Store an embedding
    pub fn store_embedding(&self, conv_id: i64, embedding: &[f32]) -> Result<()> {
        let bytes = embedding
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect::<Vec<u8>>();

        self.conn.execute(
            "INSERT OR REPLACE INTO embeddings (conversation_id, embedding) VALUES (?, ?)",
            rusqlite::params![conv_id, bytes],
        )?;

        Ok(())
    }

    /// Get an embedding
    pub fn get_embedding(&self, conv_id: i64) -> Result<Option<Vec<f32>>> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT embedding FROM embeddings WHERE conversation_id = ?",
                [conv_id],
                |row| row.get(0),
            )
            .optional()?;

        match bytes {
            Some(b) => {
                let floats = b
                    .chunks_exact(4)
                    .map(|chunk| {
                        let bytes: [u8; 4] = chunk.try_into().unwrap();
                        f32::from_le_bytes(bytes)
                    })
                    .collect();
                Ok(Some(floats))
            }
            None => Ok(None),
        }
    }

    /// Get all embeddings
    pub fn get_all_embeddings(&self) -> Result<Vec<(i64, Vec<f32>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT conversation_id, embedding FROM embeddings")?;

        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let bytes: Vec<u8> = row.get(1)?;
            let floats = bytes
                .chunks_exact(4)
                .map(|chunk| {
                    let bytes: [u8; 4] = chunk.try_into().unwrap();
                    f32::from_le_bytes(bytes)
                })
                .collect();
            Ok((id, floats))
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }

        Ok(results)
    }

    /// Load fingerprint map for a specific agent (optimization for incremental indexing)
    pub fn load_fingerprint_map(&self, agent: Agent) -> Result<HashMap<PathBuf, String>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_path, source_fingerprint
             FROM conversations
             WHERE agent = ?",
        )?;

        let rows = stmt.query_map([agent.slug()], |row| {
            let path: String = row.get(0)?;
            let fp: String = row.get(1)?;
            Ok((PathBuf::from(path), fp))
        })?;

        let mut map = HashMap::new();
        for row in rows {
            let (path, fp) = row?;
            map.insert(path, fp);
        }

        Ok(map)
    }

    /// Set a metadata value
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)",
            [key, value],
        )?;
        Ok(())
    }

    /// Get a metadata value
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let value: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?", [key], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(value)
    }
}

/// Conversation row (simplified, without messages)
#[derive(Debug, Clone)]
pub struct ConversationRow {
    pub id: i64,
    pub agent: Agent,
    pub external_id: Option<String>,
    pub title: Option<String>,
    pub workspace: Option<String>,
    pub source_path: String,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub source_fingerprint: String,
    pub logical_session_id: Option<String>,
    pub parent_external_id: Option<String>,
    pub record_kind: String,
    pub is_synthetic: bool,
}

fn insert_messages(tx: &Transaction, conversation_id: i64, messages: &[Message]) -> Result<()> {
    let mut stmt = tx.prepare(
        "INSERT INTO messages
            (conversation_id, idx, role, content, timestamp, model, content_hash)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )?;

    for msg in messages {
        // Compute content hash
        let hash = blake3::hash(msg.content.as_bytes()).to_hex().to_string();

        stmt.execute(rusqlite::params![
            conversation_id,
            msg.idx as i64,
            msg.role.as_str(),
            msg.content.as_str(),
            msg.timestamp,
            msg.model.as_deref(),
            hash,
        ])?;
    }

    Ok(())
}

fn insert_usage_events(
    tx: &Transaction,
    conversation_id: i64,
    usage: &[UsageRecord],
) -> Result<()> {
    let mut stmt = tx.prepare(
        "INSERT INTO usage_events
            (conversation_id, idx, timestamp, provider, model, source_event_id, api_calls, input_tokens,
             output_tokens, cache_read_tokens, cache_write_tokens,
             reasoning_tokens, total_tokens, actual_cost_usd, estimated_cost_usd,
             interval_start, interval_end, usage_grain, provider_family,
             provider_inference_source, provider_inference_confidence,
             model_family, model_variant, task, billing_base_url, billing_mode,
             request_attempts, reported_total_tokens, component_total_tokens,
             token_semantics, cost_status, cost_source, cost_currency, pricing_version)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?;

    for (idx, record) in usage.iter().enumerate() {
        let mut record = record.clone();
        record.enrich_metadata();
        stmt.execute(rusqlite::params![
            conversation_id,
            idx as i64,
            record.timestamp,
            record.provider.as_deref(),
            record.model.as_deref(),
            record.source_event_id.as_deref(),
            sqlite_counter(record.api_calls),
            sqlite_counter(record.input_tokens),
            sqlite_counter(record.output_tokens),
            sqlite_counter(record.cache_read_tokens),
            sqlite_counter(record.cache_write_tokens),
            sqlite_counter(record.reasoning_tokens),
            sqlite_counter(record.total_tokens),
            record.actual_cost_usd,
            record.estimated_cost_usd,
            record.metadata.interval_start,
            record.metadata.interval_end,
            record.metadata.grain.as_str(),
            record.metadata.provider_family.as_deref(),
            record.metadata.provider_inference_source.as_deref(),
            record.metadata.provider_inference_confidence.as_deref(),
            record.metadata.model_family.as_deref(),
            record.metadata.model_variant.as_deref(),
            record.metadata.task.as_deref(),
            record.metadata.billing_base_url.as_deref(),
            record.metadata.billing_mode.as_deref(),
            sqlite_counter(record.metadata.request_attempts),
            record.metadata.reported_total_tokens.map(sqlite_counter),
            record.metadata.component_total_tokens.map(sqlite_counter),
            record.metadata.token_semantics.as_deref(),
            record.metadata.cost_status.as_deref(),
            record.metadata.cost_source.as_deref(),
            record.metadata.cost_currency.as_deref(),
            record.metadata.pricing_version.as_deref(),
        ])?;
    }

    Ok(())
}

fn sqlite_counter(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn nonnegative_i64(value: i64) -> u64 {
    value.max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn create_v3_database(path: &Path) -> Connection {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (
                    version INTEGER PRIMARY KEY,
                    applied_at INTEGER NOT NULL
                );",
            )
            .unwrap();
        for migration in MIGRATIONS.iter().filter(|migration| migration.version <= 3) {
            connection.execute_batch(migration.sql).unwrap();
            connection
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?, 0)",
                    [migration.version],
                )
                .unwrap();
        }
        connection
    }

    fn create_test_conversation() -> Conversation {
        Conversation {
            agent: Agent::ClaudeCode,
            external_id: Some("test-123".to_string()),
            title: Some("Test Conversation".to_string()),
            workspace: Some(PathBuf::from("/test/workspace")),
            source_path: PathBuf::from("/test/session.jsonl"),
            source_files: vec![SourceFile {
                path: PathBuf::from("/test/session.jsonl"),
                mtime: 1000,
                size: 500,
            }],
            source_fingerprint: "abc123".to_string(),
            started_at: Some(1000),
            ended_at: Some(2000),
            messages: vec![
                Message {
                    idx: 0,
                    role: Role::User,
                    content: "Hello".to_string(),
                    timestamp: Some(1000),
                    model: None,
                },
                Message {
                    idx: 1,
                    role: Role::Assistant,
                    content: "Hi there!".to_string(),
                    timestamp: Some(2000),
                    model: Some("claude-3".to_string()),
                },
            ],
            usage: vec![],
            metadata: Default::default(),
        }
    }

    #[test]
    fn test_storage_basic() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();

        let conv = create_test_conversation();
        let outcome = storage.upsert_conversation(&conv).unwrap();

        assert!(outcome.inserted);
        assert!(outcome.changed);
        assert!(outcome.conversation_id > 0);

        // Second insert should not change
        let outcome2 = storage.upsert_conversation(&conv).unwrap();
        assert!(!outcome2.inserted);
        assert!(!outcome2.changed);

        // Get conversation
        let retrieved = storage.get_conversation(outcome.conversation_id).unwrap();
        assert!(retrieved.is_some());

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.messages.len(), 2);

        // Stats
        let stats = storage.stats().unwrap();
        assert_eq!(stats.total_conversations, 1);
        assert_eq!(stats.total_messages, 2);
    }

    #[test]
    fn usage_round_trips_and_is_replaced_atomically_on_update() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();
        let mut conv = create_test_conversation();
        conv.metadata = ConversationMetadata {
            logical_session_id: Some("logical-session".to_string()),
            parent_external_id: Some("parent-session".to_string()),
            record_kind: Some("child_agent".to_string()),
            is_synthetic: true,
        };
        conv.usage = vec![UsageRecord {
            timestamp: Some(2_000),
            provider: Some("anthropic".to_string()),
            model: Some("claude-3".to_string()),
            source_event_id: Some("message:test".to_string()),
            api_calls: 2,
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 25,
            cache_write_tokens: 5,
            reasoning_tokens: 10,
            total_tokens: 180,
            actual_cost_usd: None,
            estimated_cost_usd: Some(0.25),
            metadata: UsageMetadata {
                interval_start: Some(1_000),
                interval_end: Some(2_000),
                grain: UsageGrain::IntervalAggregate,
                provider_family: Some("anthropic".to_string()),
                provider_inference_source: Some("raw_provider".to_string()),
                provider_inference_confidence: Some("high".to_string()),
                model_family: Some("claude".to_string()),
                model_variant: Some("high".to_string()),
                task: Some("review".to_string()),
                billing_base_url: Some("https://api.anthropic.com".to_string()),
                billing_mode: Some("api".to_string()),
                request_attempts: 3,
                reported_total_tokens: Some(180),
                component_total_tokens: Some(180),
                token_semantics: Some("test-v1".to_string()),
                cost_status: Some("source_estimated".to_string()),
                cost_source: Some("fixture".to_string()),
                cost_currency: Some("USD".to_string()),
                pricing_version: Some("fixture-v1".to_string()),
            },
        }];
        conv.usage[0].enrich_metadata();

        let outcome = storage.upsert_conversation(&conv).unwrap();
        let stored = storage
            .get_conversation(outcome.conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.metadata, conv.metadata);
        assert_eq!(stored.usage, conv.usage);

        let dataset = storage.usage_dataset().unwrap();
        assert_eq!(dataset.events.len(), 1);
        assert_eq!(dataset.events[0].agent, Agent::ClaudeCode);
        assert_eq!(dataset.events[0].api_calls, 2);
        assert_eq!(dataset.events[0].request_attempts, 3);
        assert_eq!(dataset.events[0].tokens.total, 180);
        assert_eq!(
            dataset.events[0].provider_family.as_deref(),
            Some("anthropic")
        );
        assert_eq!(dataset.events[0].model_family.as_deref(), Some("claude"));
        assert_eq!(dataset.events[0].model_variant.as_deref(), Some("high"));
        assert_eq!(dataset.events[0].task.as_deref(), Some("review"));
        assert_eq!(
            dataset.events[0].token_semantics.as_deref(),
            Some("test-v1")
        );
        assert_eq!(
            dataset.events[0].pricing_version.as_deref(),
            Some("fixture-v1")
        );
        assert_eq!(dataset.indexed_conversations, 1);
        assert_eq!(dataset.indexed_messages, 2);

        conv.source_fingerprint = "replacement".to_string();
        conv.usage[0].total_tokens = 200;
        storage.upsert_conversation(&conv).unwrap();
        let replaced = storage.get_usage(outcome.conversation_id).unwrap();
        assert_eq!(replaced.len(), 1);
        assert_eq!(replaced[0].total_tokens, 200);
    }

    #[test]
    fn migration_004_preserves_raw_dimensions_and_backfills_only_known_provenance() {
        let temp_file = NamedTempFile::new().unwrap();
        let connection = create_v3_database(temp_file.path());
        connection
            .execute(
                "INSERT INTO conversations
                    (agent, external_id, title, workspace, source_path, started_at, ended_at,
                     indexed_at, source_mtime_max, source_fingerprint)
                 VALUES ('opencode', 'legacy-session', 'Legacy', '/workspace', '/legacy/session',
                         0, 0, 3000, 4000, 'legacy-fingerprint')",
                [],
            )
            .unwrap();
        connection
            .execute_batch(
                "UPDATE conversations SET workspace = '' WHERE id = 1;
                 INSERT INTO conversations
                    (agent, external_id, title, workspace, source_path, started_at, ended_at,
                     indexed_at, source_mtime_max, source_fingerprint)
                 VALUES ('opencode', '', 'No external ID', '', '/legacy/no-id',
                         0, 0, 3000, 4000, 'legacy-no-id');
                 INSERT INTO messages
                    (conversation_id, idx, role, content, timestamp, model, content_hash)
                 VALUES (1, 0, 'assistant', 'Legacy message', 0, '', 'hash');
                 INSERT INTO usage_events
                    (conversation_id, idx, provider, model, api_calls, input_tokens,
                     output_tokens, cache_read_tokens, cache_write_tokens, total_tokens,
                     actual_cost_usd, estimated_cost_usd)
                 VALUES
                    (1, 0, 'vertex', 'gpt-5.6-sol', 2, 10, 20, 3, 4, 37, 0, 0),
                    (1, 1, 'anthropic', 'claude-4', 1, 1, 2, 0, 0, 3, 1.25, 2.5),
                    (1, 2, 'openai', 'gpt-5', 1, 2, 3, 0, 0, 5, NULL, 0.75),
                    (1, 3, 'openai', 'gpt-5', 1, 2, 3, 0, 0, 5, 1e999, NULL),
                    (1, 4, 'openai', 'gpt-5', 1, 9223372036854775807, 1, 0, 0,
                     9223372036854775807, NULL, NULL);",
            )
            .unwrap();
        drop(connection);

        let storage = Storage::new(temp_file.path()).unwrap();
        let conversation = storage.get_conversation(1).unwrap().unwrap();
        assert_eq!(
            conversation.metadata.logical_session_id.as_deref(),
            Some("legacy-session")
        );
        assert_eq!(conversation.metadata.parent_external_id, None);
        assert_eq!(
            conversation.metadata.record_kind.as_deref(),
            Some("top_level")
        );
        assert!(!conversation.metadata.is_synthetic);
        assert_eq!(conversation.workspace, None);
        assert_eq!(conversation.started_at, None);
        assert_eq!(conversation.ended_at, None);
        assert_eq!(conversation.messages[0].timestamp, None);
        assert_eq!(conversation.messages[0].model, None);

        let no_external_id = storage.get_conversation(2).unwrap().unwrap();
        assert_eq!(no_external_id.external_id, None);
        assert_eq!(no_external_id.workspace, None);
        assert_eq!(no_external_id.metadata.logical_session_id, None);

        let usage = conversation.usage;
        assert_eq!(usage.len(), 5);
        assert_eq!(usage[0].provider.as_deref(), Some("vertex"));
        assert_eq!(usage[0].model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(usage[0].metadata.provider_family, None);
        assert_eq!(usage[0].metadata.provider_inference_source, None);
        assert_eq!(usage[0].metadata.provider_inference_confidence, None);
        assert_eq!(usage[0].metadata.model_family, None);
        assert_eq!(usage[0].metadata.request_attempts, 2);
        assert_eq!(usage[0].metadata.reported_total_tokens, None);
        assert_eq!(usage[0].metadata.component_total_tokens, Some(37));
        assert_eq!(usage[0].metadata.token_semantics, None);
        assert_eq!(usage[0].metadata.cost_status.as_deref(), Some("unknown"));
        assert_eq!(usage[0].metadata.cost_currency, None);

        assert_eq!(
            usage[1].metadata.cost_status.as_deref(),
            Some("reported_actual")
        );
        assert_eq!(usage[1].metadata.cost_currency.as_deref(), Some("USD"));
        assert_eq!(
            usage[2].metadata.cost_status.as_deref(),
            Some("source_estimated")
        );
        assert_eq!(usage[2].metadata.cost_currency.as_deref(), Some("USD"));
        assert_eq!(usage[3].metadata.cost_status.as_deref(), Some("unknown"));
        assert_eq!(usage[3].metadata.cost_currency, None);
        assert_eq!(
            usage[4].metadata.component_total_tokens,
            Some(i64::MAX as u64)
        );

        let latest: u32 = storage
            .conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(latest, 4);
    }

    #[test]
    fn optional_conversation_and_message_fields_round_trip_as_null() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();
        let mut conversation = create_test_conversation();
        conversation.external_id = None;
        conversation.workspace = None;
        conversation.started_at = None;
        conversation.ended_at = None;
        for message in &mut conversation.messages {
            message.timestamp = None;
            message.model = None;
        }

        let outcome = storage.upsert_conversation(&conversation).unwrap();
        let stored = storage
            .get_conversation(outcome.conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.external_id, None);
        assert_eq!(stored.workspace, None);
        assert_eq!(stored.started_at, None);
        assert_eq!(stored.ended_at, None);
        assert!(
            stored
                .messages
                .iter()
                .all(|message| message.timestamp.is_none())
        );
        assert!(
            stored
                .messages
                .iter()
                .all(|message| message.model.is_none())
        );

        conversation.external_id = Some("temporary-id".to_string());
        conversation.workspace = Some(PathBuf::from("/temporary"));
        conversation.started_at = Some(10);
        conversation.ended_at = Some(20);
        conversation.messages[0].timestamp = Some(10);
        conversation.messages[0].model = Some("temporary-model".to_string());
        conversation.source_fingerprint = "temporary-values".to_string();
        storage.upsert_conversation(&conversation).unwrap();

        conversation.external_id = None;
        conversation.workspace = None;
        conversation.started_at = None;
        conversation.ended_at = None;
        conversation.messages[0].timestamp = None;
        conversation.messages[0].model = None;
        conversation.source_fingerprint = "cleared-values".to_string();
        storage.upsert_conversation(&conversation).unwrap();
        let cleared = storage
            .get_conversation(outcome.conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(cleared.external_id, None);
        assert_eq!(cleared.workspace, None);
        assert_eq!(cleared.started_at, None);
        assert_eq!(cleared.ended_at, None);
        assert_eq!(cleared.messages[0].timestamp, None);
        assert_eq!(cleared.messages[0].model, None);
    }

    #[test]
    fn future_schema_versions_are_rejected_without_applying_missing_migrations() {
        let temp_file = NamedTempFile::new().unwrap();
        let connection = create_v3_database(temp_file.path());
        connection
            .execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (5, 0)",
                [],
            )
            .unwrap();
        drop(connection);

        let error = match Storage::new(temp_file.path()) {
            Ok(_) => panic!("future schema must be rejected"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("schema version 5 is newer"));
        let read_only_error = match Storage::open_read_only(temp_file.path()) {
            Ok(_) => panic!("future schema must be rejected read-only"),
            Err(error) => error,
        };
        assert!(format!("{read_only_error:#}").contains("schema version 5 is newer"));

        let connection = Connection::open_with_flags(
            temp_file.path(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let v4_columns: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('conversations')
                 WHERE name = 'logical_session_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v4_columns, 0);
    }

    #[test]
    fn unknown_persisted_enum_values_return_errors() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();
        let mut conversation = create_test_conversation();
        conversation.usage = vec![UsageRecord {
            api_calls: 1,
            total_tokens: 1,
            ..UsageRecord::default()
        }];
        let outcome = storage.upsert_conversation(&conversation).unwrap();

        storage
            .conn
            .execute(
                "UPDATE conversations SET agent = 'future_agent' WHERE id = ?",
                [outcome.conversation_id],
            )
            .unwrap();
        let agent_error = storage
            .get_conversation(outcome.conversation_id)
            .unwrap_err();
        assert!(format!("{agent_error:#}").contains("Unknown persisted agent"));
        storage
            .conn
            .execute(
                "UPDATE conversations SET agent = 'claude_code' WHERE id = ?",
                [outcome.conversation_id],
            )
            .unwrap();

        storage
            .conn
            .execute(
                "UPDATE messages SET role = 'future_role' WHERE conversation_id = ?",
                [outcome.conversation_id],
            )
            .unwrap();
        let role_error = storage.get_messages(outcome.conversation_id).unwrap_err();
        assert!(format!("{role_error:#}").contains("Unknown persisted message role"));
        storage
            .conn
            .execute(
                "UPDATE messages SET role = 'user' WHERE conversation_id = ?",
                [outcome.conversation_id],
            )
            .unwrap();

        storage
            .conn
            .execute(
                "UPDATE usage_events SET usage_grain = 'future_grain'
                 WHERE conversation_id = ?",
                [outcome.conversation_id],
            )
            .unwrap();
        let grain_error = storage.get_usage(outcome.conversation_id).unwrap_err();
        assert!(format!("{grain_error:#}").contains("Unknown persisted usage grain"));
        let dataset_error = storage.usage_dataset().unwrap_err();
        assert!(format!("{dataset_error:#}").contains("Unknown persisted usage grain"));
    }

    #[test]
    fn concurrent_openers_serialize_and_recheck_migrations() {
        let temp_file = NamedTempFile::new().unwrap();
        drop(create_v3_database(temp_file.path()));
        let path = temp_file.path().to_path_buf();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    Storage::new(&path).map(drop)
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        let storage = Storage::new(&path).unwrap();
        let v4_markers: i64 = storage
            .conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 4",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v4_markers, 1);
    }

    #[test]
    fn oversized_usage_counters_saturate_at_sqlite_integer_limit() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();
        let mut conv = create_test_conversation();
        conv.usage = vec![UsageRecord {
            api_calls: u64::MAX,
            input_tokens: u64::MAX,
            total_tokens: u64::MAX,
            ..UsageRecord::default()
        }];

        let outcome = storage.upsert_conversation(&conv).unwrap();
        let stored = storage.get_usage(outcome.conversation_id).unwrap();

        assert_eq!(stored[0].api_calls, i64::MAX as u64);
        assert_eq!(stored[0].input_tokens, i64::MAX as u64);
        assert_eq!(stored[0].total_tokens, i64::MAX as u64);
    }

    #[test]
    fn test_embedding_storage() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();

        // Must insert a conversation first to satisfy FK constraint
        let conv = create_test_conversation();
        let outcome = storage.upsert_conversation(&conv).unwrap();

        let embedding: Vec<f32> = vec![0.1, 0.2, 0.3, 0.4, 0.5];
        storage
            .store_embedding(outcome.conversation_id, &embedding)
            .unwrap();

        let retrieved = storage.get_embedding(outcome.conversation_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap(), embedding);
    }

    #[test]
    fn test_embedding_not_found() {
        let temp_file = NamedTempFile::new().unwrap();
        let storage = Storage::new(temp_file.path()).unwrap();

        let result = storage.get_embedding(999).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_embedding_overwrite() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();

        let conv = create_test_conversation();
        let outcome = storage.upsert_conversation(&conv).unwrap();

        let emb1: Vec<f32> = vec![1.0, 2.0, 3.0];
        storage
            .store_embedding(outcome.conversation_id, &emb1)
            .unwrap();

        let emb2: Vec<f32> = vec![4.0, 5.0, 6.0];
        storage
            .store_embedding(outcome.conversation_id, &emb2)
            .unwrap();

        let retrieved = storage
            .get_embedding(outcome.conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(retrieved, emb2);
    }

    #[test]
    fn test_get_all_embeddings() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();

        // Insert 2 conversations
        let mut conv1 = create_test_conversation();
        let o1 = storage.upsert_conversation(&conv1).unwrap();

        let mut conv2 = create_test_conversation();
        conv2.source_path = PathBuf::from("/test/session2.jsonl");
        conv2.source_fingerprint = "def456".to_string();
        let o2 = storage.upsert_conversation(&conv2).unwrap();

        storage
            .store_embedding(o1.conversation_id, &[1.0, 2.0])
            .unwrap();
        storage
            .store_embedding(o2.conversation_id, &[3.0, 4.0])
            .unwrap();

        let all = storage.get_all_embeddings().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_needs_reindex_new_path() {
        let temp_file = NamedTempFile::new().unwrap();
        let storage = Storage::new(temp_file.path()).unwrap();

        // Non-existent path always needs reindex
        let needs = storage
            .needs_reindex(Path::new("/new/path"), "fp123")
            .unwrap();
        assert!(needs);
    }

    #[test]
    fn test_needs_reindex_same_fingerprint() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();

        let conv = create_test_conversation();
        storage.upsert_conversation(&conv).unwrap();

        let needs = storage
            .needs_reindex(&conv.source_path, &conv.source_fingerprint)
            .unwrap();
        assert!(!needs);
    }

    #[test]
    fn test_needs_reindex_changed_fingerprint() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();

        let conv = create_test_conversation();
        storage.upsert_conversation(&conv).unwrap();

        let needs = storage
            .needs_reindex(&conv.source_path, "different_fingerprint")
            .unwrap();
        assert!(needs);
    }

    #[test]
    fn test_upsert_changed_fingerprint() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();

        let mut conv = create_test_conversation();
        let o1 = storage.upsert_conversation(&conv).unwrap();
        assert!(o1.inserted);
        assert!(o1.changed);

        // Change the fingerprint → should update
        conv.source_fingerprint = "new_fingerprint".to_string();
        conv.messages.push(Message {
            idx: 2,
            role: Role::User,
            content: "New message".into(),
            timestamp: Some(3000),
            model: None,
        });
        let o2 = storage.upsert_conversation(&conv).unwrap();
        assert!(!o2.inserted);
        assert!(o2.changed);
        assert_eq!(o1.conversation_id, o2.conversation_id); // Same row

        // Verify messages were replaced
        let retrieved = storage
            .get_conversation(o2.conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.messages.len(), 3);
    }

    #[test]
    fn test_get_conversation_not_found() {
        let temp_file = NamedTempFile::new().unwrap();
        let storage = Storage::new(temp_file.path()).unwrap();

        let result = storage.get_conversation(999).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_get_conversation_fields() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();

        let conv = create_test_conversation();
        let outcome = storage.upsert_conversation(&conv).unwrap();

        let retrieved = storage
            .get_conversation(outcome.conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.agent, Agent::ClaudeCode);
        assert_eq!(retrieved.external_id, Some("test-123".to_string()));
        assert_eq!(retrieved.title, Some("Test Conversation".to_string()));
        assert_eq!(retrieved.workspace, Some(PathBuf::from("/test/workspace")));
        assert_eq!(retrieved.messages.len(), 2);
        assert_eq!(retrieved.messages[0].role, Role::User);
        assert_eq!(retrieved.messages[0].content, "Hello");
        assert_eq!(retrieved.messages[1].role, Role::Assistant);
        assert_eq!(retrieved.messages[1].content, "Hi there!");
        assert_eq!(retrieved.messages[1].model, Some("claude-3".to_string()));
    }

    #[test]
    fn test_get_all_conversations() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();

        let mut conv1 = create_test_conversation();
        storage.upsert_conversation(&conv1).unwrap();

        let mut conv2 = create_test_conversation();
        conv2.source_path = PathBuf::from("/test/session2.jsonl");
        conv2.source_fingerprint = "def456".to_string();
        conv2.agent = Agent::Codex;
        storage.upsert_conversation(&conv2).unwrap();

        let rows = storage.get_all_conversations().unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn delete_missing_sources_keeps_existing_files() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let db_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(db_file.path()).unwrap();

        // Create 3 real files on disk
        let p1 = tmp.path().join("s1.jsonl");
        let p2 = tmp.path().join("s2.jsonl");
        let p3 = tmp.path().join("s3.jsonl");
        for p in [&p1, &p2, &p3] {
            fs::write(p, "x").unwrap();
        }

        for (i, p) in [&p1, &p2, &p3].iter().enumerate() {
            let mut conv = create_test_conversation();
            conv.source_path = (*p).clone();
            conv.source_fingerprint = format!("fp{}", i);
            conv.agent = Agent::PiAgent;
            storage.upsert_conversation(&conv).unwrap();
        }

        // Delete file #2 from disk
        fs::remove_file(&p2).unwrap();

        let mut detected = HashSet::new();
        detected.insert(Agent::PiAgent);

        let summary = storage.delete_missing_sources(&detected).unwrap();
        assert_eq!(summary.deleted_ids.len(), 1);
        assert_eq!(summary.deleted_paths, vec![p2]);
        assert!(summary.uncertain_paths.is_empty());

        let stats = storage.stats().unwrap();
        assert_eq!(stats.total_conversations, 2);
    }

    #[test]
    fn delete_missing_sources_ignores_undetected_agents() {
        let db_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(db_file.path()).unwrap();

        // Two rows for two different agents, both pointing at paths that do not exist.
        let mut a = create_test_conversation();
        a.agent = Agent::PiAgent;
        a.source_path = PathBuf::from("/definitely/missing/pi.jsonl");
        a.source_fingerprint = "fp-pi".to_string();
        storage.upsert_conversation(&a).unwrap();

        let mut b = create_test_conversation();
        b.agent = Agent::OpenCode;
        b.source_path = PathBuf::from("/definitely/missing/oc.jsonl");
        b.source_fingerprint = "fp-oc".to_string();
        storage.upsert_conversation(&b).unwrap();

        // Only PiAgent is currently detected -> OpenCode row must survive.
        let mut detected = HashSet::new();
        detected.insert(Agent::PiAgent);

        let summary = storage.delete_missing_sources(&detected).unwrap();
        assert_eq!(summary.deleted_ids.len(), 1);

        let stats = storage.stats().unwrap();
        assert_eq!(stats.total_conversations, 1);
    }

    #[cfg(unix)]
    #[test]
    fn delete_missing_sources_keeps_rows_on_metadata_error() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        // Skip when running as root — root bypasses directory permissions.
        if std::env::var("USER").as_deref() == Ok("root") {
            return;
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let locked_dir = tmp.path().join("locked");
        fs::create_dir(&locked_dir).unwrap();
        let hidden = locked_dir.join("s.jsonl");
        fs::write(&hidden, "x").unwrap();

        let db_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(db_file.path()).unwrap();

        let mut conv = create_test_conversation();
        conv.source_path = hidden.clone();
        conv.agent = Agent::PiAgent;
        storage.upsert_conversation(&conv).unwrap();

        // Remove all permissions on the parent so try_exists() returns Err.
        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o000)).unwrap();

        let mut detected = HashSet::new();
        detected.insert(Agent::PiAgent);

        let result = storage.delete_missing_sources(&detected);

        // Restore perms before any assertion so TempDir can clean up.
        let _ = fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o755));

        let summary = result.unwrap();
        assert!(
            summary.deleted_ids.is_empty(),
            "row must be preserved on metadata error"
        );
        assert_eq!(summary.uncertain_paths.len(), 1);
        assert_eq!(summary.uncertain_paths[0].0, 1);

        let stats = storage.stats().unwrap();
        assert_eq!(stats.total_conversations, 1);
    }

    #[test]
    fn test_stats_by_agent() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();

        let mut conv1 = create_test_conversation();
        conv1.agent = Agent::ClaudeCode;
        storage.upsert_conversation(&conv1).unwrap();

        let mut conv2 = create_test_conversation();
        conv2.agent = Agent::Codex;
        conv2.source_path = PathBuf::from("/test/codex.jsonl");
        conv2.source_fingerprint = "codex_fp".to_string();
        storage.upsert_conversation(&conv2).unwrap();

        let stats = storage.stats().unwrap();
        assert_eq!(stats.total_conversations, 2);
        assert_eq!(stats.by_agent[&Agent::ClaudeCode].conversations, 1);
        assert_eq!(stats.by_agent[&Agent::Codex].conversations, 1);
    }

    #[test]
    fn test_meta_set_get() {
        let temp_file = NamedTempFile::new().unwrap();
        let storage = Storage::new(temp_file.path()).unwrap();

        storage.set_meta("last_scan_ts", "1234567890").unwrap();
        let val = storage.get_meta("last_scan_ts").unwrap();
        assert_eq!(val, Some("1234567890".to_string()));
    }

    #[test]
    fn test_meta_overwrite() {
        let temp_file = NamedTempFile::new().unwrap();
        let storage = Storage::new(temp_file.path()).unwrap();

        storage.set_meta("key", "value1").unwrap();
        storage.set_meta("key", "value2").unwrap();

        let val = storage.get_meta("key").unwrap();
        assert_eq!(val, Some("value2".to_string()));
    }

    #[test]
    fn test_meta_not_found() {
        let temp_file = NamedTempFile::new().unwrap();
        let storage = Storage::new(temp_file.path()).unwrap();

        let val = storage.get_meta("nonexistent").unwrap();
        assert!(val.is_none());
    }

    #[test]
    fn test_load_fingerprint_map() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();

        let mut conv1 = create_test_conversation();
        conv1.agent = Agent::ClaudeCode;
        storage.upsert_conversation(&conv1).unwrap();

        let mut conv2 = create_test_conversation();
        conv2.agent = Agent::Codex;
        conv2.source_path = PathBuf::from("/test/codex.jsonl");
        conv2.source_fingerprint = "codex_fp".to_string();
        storage.upsert_conversation(&conv2).unwrap();

        // Only get ClaudeCode fingerprints
        let map = storage.load_fingerprint_map(Agent::ClaudeCode).unwrap();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&PathBuf::from("/test/session.jsonl")));

        let codex_map = storage.load_fingerprint_map(Agent::Codex).unwrap();
        assert_eq!(codex_map.len(), 1);

        let empty_map = storage.load_fingerprint_map(Agent::PiAgent).unwrap();
        assert!(empty_map.is_empty());
    }

    #[test]
    fn test_multiple_conversations() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(temp_file.path()).unwrap();

        for i in 0..10 {
            let mut conv = create_test_conversation();
            conv.source_path = PathBuf::from(format!("/test/session_{}.jsonl", i));
            conv.source_fingerprint = format!("fp_{}", i);
            storage.upsert_conversation(&conv).unwrap();
        }

        let stats = storage.stats().unwrap();
        assert_eq!(stats.total_conversations, 10);
        assert_eq!(stats.total_messages, 20); // 2 messages each
    }

    #[test]
    fn test_migration_idempotent() {
        let temp_file = NamedTempFile::new().unwrap();

        // Open twice - migrations should be idempotent
        {
            let _storage = Storage::new(temp_file.path()).unwrap();
        }
        {
            let storage = Storage::new(temp_file.path()).unwrap();
            let stats = storage.stats().unwrap();
            assert_eq!(stats.total_conversations, 0);
        }
    }
}
