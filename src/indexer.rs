use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;

use crate::connectors::{all_connectors, Connector};
use crate::model::{Agent, Conversation};
use crate::search::{SemanticIndex, TantivyIndex};
use crate::storage::{Storage, UpsertOutcome};

/// Index progress information
#[derive(Debug, Clone)]
pub struct IndexProgress {
    pub agent: Agent,
    pub files_scanned: usize,
    pub files_total: usize,
    pub conversations_indexed: usize,
}

/// Index statistics
#[derive(Debug, Clone, Default)]
pub struct IndexStats {
    pub conversations_indexed: usize,
    pub conversations_updated: usize,
    pub conversations_inserted: usize,
    pub messages_indexed: usize,
    pub files_scanned: usize,
    pub time_ms: u64,
}

/// Main indexer that orchestrates scanning and indexing
pub struct Indexer {
    storage: Storage,
    tantivy: TantivyIndex,
    semantic: Option<SemanticIndex>,
    data_dir: PathBuf,
}

impl Indexer {
    /// Create a new indexer
    pub fn new(data_dir: &PathBuf, enable_semantic: bool) -> Result<Self> {
        let db_path = data_dir.join("sess.db");
        let tantivy_path = data_dir.join("tantivy");

        let storage = Storage::new(&db_path)?;
        // Don't start writer here - only start it when actually indexing
        // This avoids holding an exclusive file lock when just reading
        let tantivy = TantivyIndex::open_or_create(&tantivy_path)?;

        let semantic = if enable_semantic {
            match SemanticIndex::new() {
                Ok(idx) => {
                    tracing::info!("Semantic search enabled");
                    Some(idx)
                }
                Err(e) => {
                    tracing::warn!("Failed to initialize semantic search: {}. Use --no-semantic to suppress this warning.", e);
                    None
                }
            }
        } else {
            None
        };

        Ok(Self {
            storage,
            tantivy,
            semantic,
            data_dir: data_dir.clone(),
        })
    }

    /// Run a full index from scratch
    pub fn full_index(&mut self) -> Result<IndexStats> {
        let start = std::time::Instant::now();
        let mut stats = IndexStats::default();

        // Ensure writer is started for indexing
        self.tantivy.start_writer()?;

        // Collect all source paths for staleness detection
        let mut all_source_paths: HashSet<PathBuf> = HashSet::new();

        let connectors = all_connectors();
        for connector in &connectors {
            if !connector.detect() {
                tracing::debug!("Agent {} not detected, skipping", connector.agent());
                continue;
            }

            tracing::info!("Scanning {} sessions...", connector.agent());

            let roots = connector.default_roots();
            let conversations = connector.scan(&roots, None)?;

            stats.files_scanned += conversations.len();

            for conv in conversations {
                all_source_paths.insert(conv.source_path.clone());
                self.index_conversation(&conv, &mut stats)?;
            }
        }

        // Delete stale conversations
        let deleted = self.storage.delete_stale(&all_source_paths)?;
        if deleted > 0 {
            tracing::info!("Deleted {} stale conversations", deleted);
        }

        // Commit changes
        self.tantivy.commit()?;

        // Store embeddings if semantic search is enabled
        if self.semantic.is_some() {
            self.index_embeddings()?;
        }

        // Update last scan timestamp
        self.storage.set_meta("last_scan_ts", &chrono::Utc::now().timestamp_millis().to_string())?;

        stats.time_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            "Full index complete: {} conversations, {} messages in {}ms",
            stats.conversations_indexed,
            stats.messages_indexed,
            stats.time_ms
        );

        Ok(stats)
    }

    /// Run an incremental index
    pub fn incremental_index(&mut self) -> Result<IndexStats> {
        let start = std::time::Instant::now();
        let mut stats = IndexStats::default();

        // Ensure writer is started for indexing
        self.tantivy.start_writer()?;

        // Get last scan timestamp
        let since_ts = self.storage
            .get_meta("last_scan_ts")?
            .and_then(|s| s.parse().ok());

        tracing::info!("Incremental index since: {:?}", since_ts);

        // Collect all source paths for staleness detection
        let mut all_source_paths: HashSet<PathBuf> = HashSet::new();

        let connectors = all_connectors();
        for connector in &connectors {
            if !connector.detect() {
                continue;
            }

            let roots = connector.default_roots();
            let conversations = connector.scan(&roots, since_ts)?;

            stats.files_scanned += conversations.len();

            for conv in conversations {
                // Check if we need to reindex
                if !self.storage.needs_reindex(&conv.source_path, &conv.source_fingerprint)? {
                    all_source_paths.insert(conv.source_path.clone());
                    continue; // Skip unchanged conversations
                }

                all_source_paths.insert(conv.source_path.clone());
                self.index_conversation(&conv, &mut stats)?;
            }
        }

        // Delete stale conversations
        let deleted = self.storage.delete_stale(&all_source_paths)?;
        if deleted > 0 {
            tracing::info!("Deleted {} stale conversations", deleted);
        }

        // Commit changes
        self.tantivy.commit()?;

        // Update embeddings for changed conversations
        if self.semantic.is_some() && stats.conversations_updated > 0 {
            self.index_embeddings()?;
        }

        // Update last scan timestamp
        self.storage.set_meta("last_scan_ts", &chrono::Utc::now().timestamp_millis().to_string())?;

        stats.time_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            "Incremental index complete: {} new, {} updated, {} messages in {}ms",
            stats.conversations_inserted,
            stats.conversations_updated,
            stats.messages_indexed,
            stats.time_ms
        );

        Ok(stats)
    }

    /// Rebuild Tantivy index from SQLite (no rescan)
    pub fn rebuild(&mut self) -> Result<IndexStats> {
        let start = std::time::Instant::now();
        let mut stats = IndexStats::default();

        // Ensure writer is started for rebuilding
        self.tantivy.start_writer()?;

        // Get all conversations from SQLite
        let conversations = self.storage.get_all_conversations()?;

        // Prepare data for Tantivy
        let mut full_conversations = Vec::new();
        for row in conversations {
            if let Some(conv) = self.storage.get_conversation(row.id)? {
                let full_text = conv.full_text();
                full_conversations.push((row, full_text));
            }
        }

        // Rebuild Tantivy
        self.tantivy.rebuild_from_sqlite(&full_conversations)?;

        // Rebuild embeddings
        if self.semantic.is_some() {
            self.index_embeddings()?;
        }

        stats.conversations_indexed = full_conversations.len();
        stats.time_ms = start.elapsed().as_millis() as u64;

        tracing::info!("Rebuild complete: {} conversations in {}ms", stats.conversations_indexed, stats.time_ms);

        Ok(stats)
    }

    /// Index a single conversation
    fn index_conversation(
        &mut self,
        conv: &Conversation,
        stats: &mut IndexStats,
    ) -> Result<()> {
        let outcome = self.storage.upsert_conversation(conv)?;

        if outcome.changed {
            self.tantivy.add_conversation(conv, outcome.conversation_id)?;

            stats.messages_indexed += conv.messages.len();

            if outcome.inserted {
                stats.conversations_inserted += 1;
            } else {
                stats.conversations_updated += 1;
            }
        }

        stats.conversations_indexed += 1;

        Ok(())
    }

    /// Index embeddings for all conversations
    fn index_embeddings(&mut self) -> Result<()> {
        if let Some(ref semantic) = self.semantic {
            tracing::info!("Indexing embeddings...");

            let conversations = self.storage.get_all_conversations()?;

            for row in conversations {
                // Check if embedding already exists
                if self.storage.get_embedding(row.id)?.is_some() {
                    continue;
                }

                if let Some(conv) = self.storage.get_conversation(row.id)? {
                    let text = conv.full_text();
                    match semantic.embed(&text) {
                        Ok(embedding) => {
                            self.storage.store_embedding(row.id, &embedding)?;
                        }
                        Err(e) => {
                            tracing::warn!("Failed to embed conversation {}: {}", row.id, e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Get storage reference
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Get mutable storage reference
    pub fn storage_mut(&mut self) -> &mut Storage {
        &mut self.storage
    }

    /// Get Tantivy reference
    pub fn tantivy(&self) -> &TantivyIndex {
        &self.tantivy
    }

    /// Get semantic index reference
    pub fn semantic(&self) -> Option<&SemanticIndex> {
        self.semantic.as_ref()
    }

    /// Check if we need to auto-index on startup
    pub fn needs_initial_index(&self) -> Result<bool> {
        let stats = self.storage.stats()?;
        Ok(stats.total_conversations == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_indexer_creation() {
        let temp_dir = TempDir::new().unwrap();
        let indexer = Indexer::new(&temp_dir.path().to_path_buf(), false);
        assert!(indexer.is_ok());
    }

    #[test]
    fn test_indexer_creation_without_semantic() {
        let temp_dir = TempDir::new().unwrap();
        let indexer = Indexer::new(&temp_dir.path().to_path_buf(), false).unwrap();
        assert!(indexer.semantic().is_none());
    }

    #[test]
    fn test_indexer_needs_initial_index() {
        let temp_dir = TempDir::new().unwrap();
        let indexer = Indexer::new(&temp_dir.path().to_path_buf(), false).unwrap();
        // Fresh database should need initial index
        assert!(indexer.needs_initial_index().unwrap());
    }

    #[test]
    fn test_indexer_storage_access() {
        let temp_dir = TempDir::new().unwrap();
        let indexer = Indexer::new(&temp_dir.path().to_path_buf(), false).unwrap();

        let stats = indexer.storage().stats().unwrap();
        assert_eq!(stats.total_conversations, 0);
    }

    #[test]
    fn test_indexer_tantivy_access() {
        let temp_dir = TempDir::new().unwrap();
        let indexer = Indexer::new(&temp_dir.path().to_path_buf(), false).unwrap();

        let count = indexer.tantivy().doc_count().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_indexer_creates_data_dir() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().join("nested").join("data");

        let indexer = Indexer::new(&data_dir, false).unwrap();
        assert!(data_dir.join("sess.db").exists());
        assert!(data_dir.join("tantivy").exists());
    }

    #[test]
    fn test_index_conversation_direct() {
        let temp_dir = TempDir::new().unwrap();
        let mut indexer = Indexer::new(&temp_dir.path().to_path_buf(), false).unwrap();

        // Start writer for indexing
        indexer.tantivy.start_writer().unwrap();

        let conv = crate::model::Conversation {
            agent: Agent::ClaudeCode,
            external_id: Some("test".to_string()),
            title: Some("Test".to_string()),
            workspace: Some(std::path::PathBuf::from("/test")),
            source_path: std::path::PathBuf::from("/test/session.jsonl"),
            source_files: vec![crate::model::SourceFile {
                path: std::path::PathBuf::from("/test/session.jsonl"),
                mtime: 1000,
                size: 100,
            }],
            source_fingerprint: "fp123".to_string(),
            started_at: Some(1000),
            ended_at: Some(2000),
            messages: vec![
                crate::model::Message {
                    idx: 0,
                    role: crate::model::Role::User,
                    content: "Hello".to_string(),
                    timestamp: Some(1000),
                    model: None,
                },
            ],
        };

        let mut stats = IndexStats::default();
        indexer.index_conversation(&conv, &mut stats).unwrap();
        indexer.tantivy.commit().unwrap();

        assert_eq!(stats.conversations_indexed, 1);
        assert_eq!(stats.conversations_inserted, 1);
        assert_eq!(stats.messages_indexed, 1);

        // Verify in Tantivy
        assert_eq!(indexer.tantivy().doc_count().unwrap(), 1);

        // Verify in SQLite
        let db_stats = indexer.storage().stats().unwrap();
        assert_eq!(db_stats.total_conversations, 1);
    }

    #[test]
    fn test_index_conversation_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        let mut indexer = Indexer::new(&temp_dir.path().to_path_buf(), false).unwrap();

        // Start writer for indexing
        indexer.tantivy.start_writer().unwrap();

        let conv = crate::model::Conversation {
            agent: Agent::Codex,
            external_id: None,
            title: Some("Idempotent Test".to_string()),
            workspace: None,
            source_path: std::path::PathBuf::from("/test/idem.jsonl"),
            source_files: vec![crate::model::SourceFile {
                path: std::path::PathBuf::from("/test/idem.jsonl"),
                mtime: 1000,
                size: 50,
            }],
            source_fingerprint: "idem_fp".to_string(),
            started_at: Some(1000),
            ended_at: None,
            messages: vec![crate::model::Message {
                idx: 0,
                role: crate::model::Role::User,
                content: "Test".to_string(),
                timestamp: None,
                model: None,
            }],
        };

        // Index twice with same fingerprint
        let mut stats1 = IndexStats::default();
        indexer.index_conversation(&conv, &mut stats1).unwrap();
        indexer.tantivy.commit().unwrap();

        let mut stats2 = IndexStats::default();
        indexer.index_conversation(&conv, &mut stats2).unwrap();
        indexer.tantivy.commit().unwrap();

        // Second time should not change (same fingerprint)
        assert_eq!(stats2.conversations_updated, 0);
        assert_eq!(stats2.conversations_inserted, 0);

        // Still only 1 doc in Tantivy
        assert_eq!(indexer.tantivy().doc_count().unwrap(), 1);
    }

    #[test]
    fn test_rebuild_empty() {
        let temp_dir = TempDir::new().unwrap();
        let mut indexer = Indexer::new(&temp_dir.path().to_path_buf(), false).unwrap();

        let stats = indexer.rebuild().unwrap();
        assert_eq!(stats.conversations_indexed, 0);
    }
}
