use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, types::Value as SqliteValue};

use crate::model::{Agent, Conversation, Message, Role, SourceFile};

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

fn agent_from_slug(slug: &str) -> Agent {
    match slug {
        "claude_code" => Agent::ClaudeCode,
        "codex" => Agent::Codex,
        "opencode" => Agent::OpenCode,
        "pi_agent" => Agent::PiAgent,
        _ => Agent::ClaudeCode,
    }
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
            "PRAGMA journal_mode = WAL;
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

    /// Run database migrations
    fn run_migrations(&mut self) -> Result<()> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at INTEGER NOT NULL
            )",
            [],
        )?;

        for migration in MIGRATIONS {
            let exists: bool = self
                .conn
                .query_row(
                    "SELECT 1 FROM schema_migrations WHERE version = ?",
                    [migration.version],
                    |_| Ok(true),
                )
                .unwrap_or(false);

            if !exists {
                tracing::info!(
                    "Applying migration {}: {}",
                    migration.version,
                    migration.name
                );
                self.conn.execute_batch(migration.sql)?;
                self.conn.execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?, ?)",
                    [
                        migration.version,
                        chrono::Utc::now().timestamp_millis() as u32,
                    ],
                )?;
            }
        }

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
                Ok((id, agent_from_slug(&agent_slug), PathBuf::from(path)))
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
                        source_fingerprint = ?
                    WHERE id = ?",
                    [
                        conv.agent.slug(),
                        conv.external_id.as_deref().unwrap_or(""),
                        conv.derive_title().as_str(),
                        workspace_str.as_deref().unwrap_or(""),
                        &conv.started_at.unwrap_or(0).to_string(),
                        &conv.ended_at.unwrap_or(0).to_string(),
                        &indexed_at.to_string(),
                        &mtime_max.to_string(),
                        &conv.source_fingerprint,
                        &id.to_string(),
                    ],
                )?;

                // Delete old messages
                tx.execute("DELETE FROM messages WHERE conversation_id = ?", [id])?;

                // Insert new messages
                insert_messages(&tx, id, &conv.messages)?;

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
                     started_at, ended_at, indexed_at, source_mtime_max, source_fingerprint)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                [
                    conv.agent.slug(),
                    conv.external_id.as_deref().unwrap_or(""),
                    conv.derive_title().as_str(),
                    workspace_str.as_deref().unwrap_or(""),
                    &source_path_str,
                    &conv.started_at.unwrap_or(0).to_string(),
                    &conv.ended_at.unwrap_or(0).to_string(),
                    &indexed_at.to_string(),
                    &mtime_max.to_string(),
                    &conv.source_fingerprint,
                ],
            )?;

            let id = tx.last_insert_rowid();

            // Insert messages
            insert_messages(&tx, id, &conv.messages)?;

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
                    started_at, ended_at, source_fingerprint
             FROM conversations
             ORDER BY id",
        )?;

        let rows = stmt.query_map([], |row| {
            let agent_slug: String = row.get(1)?;
            let agent = match agent_slug.as_str() {
                "claude_code" => Agent::ClaudeCode,
                "codex" => Agent::Codex,
                "opencode" => Agent::OpenCode,
                "pi_agent" => Agent::PiAgent,
                _ => Agent::ClaudeCode, // Default
            };

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
                    started_at, ended_at, source_fingerprint
             FROM conversations WHERE id = ?",
                [id],
                |row| {
                    let agent_slug: String = row.get(1)?;
                    let agent = match agent_slug.as_str() {
                        "claude_code" => Agent::ClaudeCode,
                        "codex" => Agent::Codex,
                        "opencode" => Agent::OpenCode,
                        "pi_agent" => Agent::PiAgent,
                        _ => Agent::ClaudeCode,
                    };

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
            let role = match role_str.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "tool" => Role::Tool,
                "system" => Role::System,
                _ => Role::User,
            };

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
        let mut stmt = self
            .conn
            .prepare("SELECT id, agent, source_path FROM conversations")?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut out = Vec::new();
        for (id, agent_slug, path) in rows {
            let agent = agent_from_slug(&agent_slug);
            if !detected_agents.contains(&agent) {
                continue;
            }
            let p = PathBuf::from(&path);
            match p.try_exists() {
                Ok(true) => {}
                Ok(false) => out.push(MissingSource {
                    id,
                    agent,
                    source_path: p,
                }),
                Err(_) => {} // uncertain -> keep, surfaced separately by delete_missing_sources
            }
        }
        Ok(out)
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
        let mut stmt = self
            .conn
            .prepare("SELECT id, agent, source_path FROM conversations")?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut to_delete: Vec<(i64, PathBuf)> = Vec::new();
        let mut uncertain: Vec<(i64, PathBuf, String)> = Vec::new();

        for (id, agent_slug, path) in rows {
            let agent = agent_from_slug(&agent_slug);
            if !detected_agents.contains(&agent) {
                continue;
            }
            let p = PathBuf::from(&path);
            match p.try_exists() {
                Ok(true) => {}
                Ok(false) => to_delete.push((id, p)),
                Err(e) => uncertain.push((id, p, e.to_string())),
            }
        }

        let mut summary = StaleDeletionSummary {
            uncertain_paths: uncertain,
            ..Default::default()
        };

        if to_delete.is_empty() {
            return Ok(summary);
        }

        let tx = self.conn.transaction()?;
        for (id, path) in &to_delete {
            tx.execute("DELETE FROM conversations WHERE id = ?", [*id])?;
            summary.deleted_ids.push(*id);
            summary.deleted_paths.push(path.clone());
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
            let agent = match agent_slug.as_str() {
                "claude_code" => Agent::ClaudeCode,
                "codex" => Agent::Codex,
                "opencode" => Agent::OpenCode,
                "pi_agent" => Agent::PiAgent,
                _ => Agent::ClaudeCode,
            };

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
            msg.timestamp.unwrap_or(0),
            msg.model.as_deref().unwrap_or(""),
            hash,
        ])?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

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
