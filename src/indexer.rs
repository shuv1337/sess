use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::connectors::{Connector, all_connectors};
use crate::model::{Agent, Conversation};
use crate::search::{SemanticIndex, TantivyIndex};
use crate::storage::sqlite::{MissingSource, StaleDeletionSummary};
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectorScanPlan {
    since_ts: Option<i64>,
    root_fingerprint: String,
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
            match SemanticIndex::new(data_dir) {
                Ok(idx) => {
                    tracing::info!("Semantic search enabled");
                    Some(idx)
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to initialize semantic search: {}. Use --no-semantic to suppress this warning.",
                        e
                    );
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

        let connectors = all_connectors();
        let detected_agents: HashSet<Agent> = connectors
            .iter()
            .filter(|c| c.detect())
            .map(|c| c.agent())
            .collect();
        let mut observed_root_fingerprints = HashMap::new();
        let mut completed_agents = HashSet::new();

        for connector in &connectors {
            if !detected_agents.contains(&connector.agent()) {
                observed_root_fingerprints
                    .insert(connector.agent(), Self::discovered_root_fingerprint(&[]));
                tracing::debug!("Agent {} not detected, skipping", connector.agent());
                continue;
            }

            tracing::info!("Scanning {} sessions...", connector.agent());

            let roots = connector.default_roots();
            observed_root_fingerprints
                .insert(connector.agent(), Self::discovered_root_fingerprint(&roots));
            let scan = connector.scan(&roots, None)?;
            if scan.complete {
                completed_agents.insert(connector.agent());
            } else {
                tracing::warn!(
                    agent = connector.agent().slug(),
                    "Connector scan was incomplete; migration cursor will be retried"
                );
            }

            stats.files_scanned += scan.len();

            for conv in scan {
                self.index_conversation(&conv, &mut stats)?;
            }
        }

        // Existence-based stale deletion (DB + Tantivy together).
        let summary = self.delete_missing(&connectors, &detected_agents)?;
        Self::log_stale_summary("full", &summary);

        // Commit changes
        self.tantivy.commit()?;
        for connector in &connectors {
            if completed_agents.contains(&connector.agent()) {
                self.record_connector_parser_revision(connector.as_ref())?;
                self.record_connector_root_fingerprint(
                    connector.as_ref(),
                    observed_root_fingerprints
                        .get(&connector.agent())
                        .expect("every connector root set is observed before commit"),
                )?;
            } else if detected_agents.contains(&connector.agent()) {
                self.mark_connector_scan_incomplete(connector.as_ref())?;
            } else {
                self.record_connector_root_fingerprint(
                    connector.as_ref(),
                    observed_root_fingerprints
                        .get(&connector.agent())
                        .expect("every connector root set is observed before commit"),
                )?;
            }
        }

        // Store embeddings if semantic search is enabled
        if self.semantic.is_some() {
            self.index_embeddings()?;
        }

        // Update last scan timestamp
        if completed_agents.len() == detected_agents.len() {
            self.storage.set_meta(
                "last_scan_ts",
                &chrono::Utc::now().timestamp_millis().to_string(),
            )?;
        }

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
        let since_ts = self
            .storage
            .get_meta("last_scan_ts")?
            .and_then(|s| s.parse().ok());

        tracing::info!("Incremental index since: {:?}", since_ts);

        let connectors = all_connectors();
        let detected_agents: HashSet<Agent> = connectors
            .iter()
            .filter(|c| c.detect())
            .map(|c| c.agent())
            .collect();
        let mut observed_root_fingerprints = HashMap::new();
        let mut completed_agents = HashSet::new();

        for connector in &connectors {
            if !detected_agents.contains(&connector.agent()) {
                observed_root_fingerprints
                    .insert(connector.agent(), Self::discovered_root_fingerprint(&[]));
                continue;
            }

            let roots = connector.default_roots();
            let scan_plan = self.connector_scan_plan(connector.as_ref(), &roots, since_ts)?;
            observed_root_fingerprints
                .insert(connector.agent(), scan_plan.root_fingerprint.clone());
            let scan = connector.scan(&roots, scan_plan.since_ts)?;
            if scan.complete {
                completed_agents.insert(connector.agent());
            } else {
                tracing::warn!(
                    agent = connector.agent().slug(),
                    "Connector scan was incomplete; migration cursor will be retried"
                );
            }

            stats.files_scanned += scan.len();

            for conv in scan {
                // Check if we need to reindex
                if !self
                    .storage
                    .needs_reindex(&conv.source_path, &conv.source_fingerprint)?
                {
                    continue; // Skip unchanged conversations
                }

                self.index_conversation(&conv, &mut stats)?;
            }
        }

        // Existence-based stale deletion (DB + Tantivy together).
        //
        // IMPORTANT: previously we built an "alive set" from the time-filtered
        // scan results and deleted any row not in it. Because connector scans
        // honor `since_ts`, that meant every agent whose files were unmodified
        // since the last scan had ALL of its rows wiped on the next
        // incremental run. See PLAN-stale-index.md Bug A.
        let summary = self.delete_missing(&connectors, &detected_agents)?;
        Self::log_stale_summary("incremental", &summary);

        // Commit changes
        self.tantivy.commit()?;
        for connector in &connectors {
            if completed_agents.contains(&connector.agent()) {
                self.record_connector_parser_revision(connector.as_ref())?;
                self.record_connector_root_fingerprint(
                    connector.as_ref(),
                    observed_root_fingerprints
                        .get(&connector.agent())
                        .expect("every connector root set is observed before commit"),
                )?;
            } else if detected_agents.contains(&connector.agent()) {
                self.mark_connector_scan_incomplete(connector.as_ref())?;
            } else {
                self.record_connector_root_fingerprint(
                    connector.as_ref(),
                    observed_root_fingerprints
                        .get(&connector.agent())
                        .expect("every connector root set is observed before commit"),
                )?;
            }
        }

        // Update embeddings for changed conversations
        if self.semantic.is_some() && stats.conversations_updated > 0 {
            self.index_embeddings()?;
        }

        // Update last scan timestamp
        if completed_agents.len() == detected_agents.len() {
            self.storage.set_meta(
                "last_scan_ts",
                &chrono::Utc::now().timestamp_millis().to_string(),
            )?;
        }

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

        tracing::info!(
            "Rebuild complete: {} conversations in {}ms",
            stats.conversations_indexed,
            stats.time_ms
        );

        Ok(stats)
    }

    /// Index a single conversation
    fn index_conversation(&mut self, conv: &Conversation, stats: &mut IndexStats) -> Result<()> {
        let outcome = self.storage.upsert_conversation(conv)?;

        if outcome.changed {
            self.tantivy
                .add_conversation(conv, outcome.conversation_id)?;

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

    /// Age of the last completed scan, if any.
    ///
    /// Returns `None` if no scan has ever been recorded. A future timestamp
    /// (clock skew) is clamped to zero duration so callers do not treat clock
    /// drift as freshness.
    pub fn last_scan_age(&self) -> Result<Option<Duration>> {
        let Some(raw) = self.storage.get_meta("last_scan_ts")? else {
            return Ok(None);
        };
        let Ok(ts) = raw.parse::<i64>() else {
            return Ok(None);
        };
        let now = chrono::Utc::now().timestamp_millis();
        let diff_ms = now.saturating_sub(ts);
        if diff_ms < 0 {
            tracing::warn!("last_scan_ts is in the future; clamping age to zero");
            return Ok(Some(Duration::ZERO));
        }
        Ok(Some(Duration::from_millis(diff_ms as u64)))
    }

    /// Whether the index should be refreshed under `max_age` policy.
    pub fn should_refresh(&self, max_age: Duration) -> Result<bool> {
        match self.last_scan_age()? {
            None => Ok(true),
            Some(age) => Ok(age > max_age),
        }
    }

    /// Whether a detected connector needs an unbounded migration scan because
    /// its parser revision or discovered source roots changed.
    pub fn needs_connector_rescan(&self) -> Result<bool> {
        for connector in all_connectors()
            .into_iter()
            .filter(|connector| connector.detect())
        {
            let roots = connector.default_roots();
            let plan = self.connector_scan_plan(connector.as_ref(), &roots, Some(0))?;
            if plan.since_ts.is_none() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Read-only dry-run: classify what an incremental index *would* do without
    /// touching SQLite or Tantivy.
    pub fn incremental_index_dry_run(&mut self) -> Result<IndexDryRunReport> {
        let mut report = IndexDryRunReport::default();

        let since_ts = self
            .storage
            .get_meta("last_scan_ts")?
            .and_then(|s| s.parse().ok());

        let connectors = all_connectors();
        let detected_agents: HashSet<Agent> = connectors
            .iter()
            .filter(|c| c.detect())
            .map(|c| c.agent())
            .collect();

        for connector in &connectors {
            if !detected_agents.contains(&connector.agent()) {
                continue;
            }
            let roots = connector.default_roots();
            let scan_plan = self.connector_scan_plan(connector.as_ref(), &roots, since_ts)?;
            let conversations = connector.scan(&roots, scan_plan.since_ts)?;
            let agent = connector.agent();
            *report.would_scan_by_agent.entry(agent).or_insert(0) += conversations.len();
            for conv in conversations {
                let exists_in_db = self
                    .storage
                    .needs_reindex(&conv.source_path, &conv.source_fingerprint)?;
                // needs_reindex returns true for both "new" and "changed" rows.
                // Distinguish via a fingerprint lookup.
                let already_present = self.storage.has_source_path(&conv.source_path)?;
                if !exists_in_db {
                    // already up-to-date
                    continue;
                }
                if already_present {
                    report.would_update += 1;
                } else {
                    report.would_insert += 1;
                }
            }
        }

        // Stale deletion preview (read-only).
        let (would_delete, uncertain) = self
            .storage
            .classify_missing_sources(&detected_agents, |agent, path| {
                Self::connector_source_exists(&connectors, agent, path)
            })?;
        report.would_delete = would_delete;
        report.uncertain_paths = uncertain;

        Ok(report)
    }

    fn connector_scan_plan(
        &self,
        connector: &dyn Connector,
        roots: &[PathBuf],
        since_ts: Option<i64>,
    ) -> Result<ConnectorScanPlan> {
        let mut requires_unbounded_scan = false;

        if let Some(revision) = connector.parser_revision() {
            let key = format!("connector_parser_revision_{}", connector.agent().slug());
            let indexed_revision = self.storage.get_meta(&key)?;
            if indexed_revision.as_deref() != Some(revision) {
                tracing::info!(
                    agent = connector.agent().slug(),
                    parser_revision = revision,
                    previous_revision = indexed_revision.as_deref().unwrap_or("none"),
                    "Connector parser revision changed; performing full source rescan"
                );
                requires_unbounded_scan = true;
            }
        }

        let root_fingerprint = Self::discovered_root_fingerprint(roots);
        let key = format!("connector_root_fingerprint_{}", connector.agent().slug());
        let indexed_root_fingerprint = self.storage.get_meta(&key)?;
        if indexed_root_fingerprint.as_deref() != Some(root_fingerprint.as_str()) {
            tracing::info!(
                agent = connector.agent().slug(),
                root_fingerprint,
                previous_root_fingerprint = indexed_root_fingerprint.as_deref().unwrap_or("none"),
                "Connector discovered root set changed; performing full source rescan"
            );
            requires_unbounded_scan = true;
        }

        Ok(ConnectorScanPlan {
            since_ts: if requires_unbounded_scan {
                None
            } else {
                since_ts
            },
            root_fingerprint,
        })
    }

    fn record_connector_parser_revision(&self, connector: &dyn Connector) -> Result<()> {
        let Some(revision) = connector.parser_revision() else {
            return Ok(());
        };

        let key = format!("connector_parser_revision_{}", connector.agent().slug());
        self.storage.set_meta(&key, revision)
    }

    fn discovered_root_fingerprint(roots: &[PathBuf]) -> String {
        // Only hash roots that currently exist. A configured/default root that
        // appears later then changes the cursor and gets one unbounded scan,
        // even when the files it contains predate the global last_scan_ts.
        let mut discovered_roots: Vec<Vec<u8>> = roots
            .iter()
            .filter(|root| root.exists())
            .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
            .map(|root| root.as_os_str().as_encoded_bytes().to_vec())
            .collect();
        discovered_roots.sort();
        discovered_roots.dedup();

        let mut hasher = blake3::Hasher::new();
        hasher.update(b"sess-connector-roots-v1\0");
        for root in discovered_roots {
            hasher.update(&(root.len() as u64).to_le_bytes());
            hasher.update(&root);
        }
        format!("v1:{}", hasher.finalize().to_hex())
    }

    fn record_connector_root_fingerprint(
        &self,
        connector: &dyn Connector,
        fingerprint: &str,
    ) -> Result<()> {
        let key = format!("connector_root_fingerprint_{}", connector.agent().slug());
        self.storage.set_meta(&key, fingerprint)
    }

    fn mark_connector_scan_incomplete(&self, connector: &dyn Connector) -> Result<()> {
        let key = format!("connector_root_fingerprint_{}", connector.agent().slug());
        self.storage.set_meta(&key, "__incomplete__")
    }

    fn connector_source_exists(
        connectors: &[Box<dyn Connector>],
        agent: Agent,
        path: &Path,
    ) -> Result<bool> {
        let connector = connectors
            .iter()
            .find(|connector| connector.agent() == agent)
            .ok_or_else(|| anyhow::anyhow!("No connector registered for {}", agent))?;
        connector.source_exists(path)
    }

    fn delete_missing(
        &mut self,
        connectors: &[Box<dyn Connector>],
        detected_agents: &HashSet<Agent>,
    ) -> Result<StaleDeletionSummary> {
        let summary = self
            .storage
            .delete_missing_sources_with(detected_agents, |agent, path| {
                Self::connector_source_exists(connectors, agent, path)
            })?;
        if !summary.deleted_ids.is_empty() {
            self.tantivy.delete_conversations(&summary.deleted_ids)?;
        }
        Ok(summary)
    }

    fn log_stale_summary(kind: &str, summary: &StaleDeletionSummary) {
        if !summary.deleted_ids.is_empty() {
            tracing::info!(
                "{} index: deleted {} stale conversations (DB + Tantivy)",
                kind,
                summary.deleted_ids.len()
            );
        }
        if !summary.uncertain_paths.is_empty() {
            tracing::warn!(
                "{} index: kept {} rows with uncertain source paths (see warnings above)",
                kind,
                summary.uncertain_paths.len()
            );
        }
    }
}

/// Dry-run preview for `sess index --dry-run`.
#[derive(Debug, Default)]
pub struct IndexDryRunReport {
    pub would_scan_by_agent: std::collections::HashMap<Agent, usize>,
    pub would_insert: usize,
    pub would_update: usize,
    pub would_delete: Vec<MissingSource>,
    pub uncertain_paths: Vec<(i64, PathBuf, String)>,
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
    fn test_connector_parser_revision_forces_one_full_rescan() {
        let temp_dir = TempDir::new().unwrap();
        let indexer = Indexer::new(&temp_dir.path().to_path_buf(), false).unwrap();
        let connector = crate::connectors::codex::CodexConnector::new();
        let roots = vec![temp_dir.path().join("codex")];
        std::fs::create_dir_all(&roots[0]).unwrap();
        let root_fingerprint = Indexer::discovered_root_fingerprint(&roots);
        indexer
            .record_connector_root_fingerprint(&connector, &root_fingerprint)
            .unwrap();

        assert_eq!(
            indexer
                .connector_scan_plan(&connector, &roots, Some(1234))
                .unwrap(),
            ConnectorScanPlan {
                since_ts: None,
                root_fingerprint: root_fingerprint.clone(),
            }
        );

        indexer
            .record_connector_parser_revision(&connector)
            .unwrap();
        assert_eq!(
            indexer
                .connector_scan_plan(&connector, &roots, Some(1234))
                .unwrap(),
            ConnectorScanPlan {
                since_ts: Some(1234),
                root_fingerprint,
            }
        );
    }

    #[test]
    fn test_unchanged_connector_roots_keep_incremental_since_timestamp() {
        let temp_dir = TempDir::new().unwrap();
        let indexer = Indexer::new(&temp_dir.path().to_path_buf(), false).unwrap();
        let connector = crate::connectors::pi_agent::PiAgentConnector::new();
        let roots = vec![temp_dir.path().join("pi-agent")];
        std::fs::create_dir_all(&roots[0]).unwrap();

        let first_plan = indexer
            .connector_scan_plan(&connector, &roots, Some(1234))
            .unwrap();
        assert_eq!(first_plan.since_ts, None);
        indexer
            .record_connector_root_fingerprint(&connector, &first_plan.root_fingerprint)
            .unwrap();
        indexer
            .record_connector_parser_revision(&connector)
            .unwrap();

        let unchanged_plan = indexer
            .connector_scan_plan(&connector, &roots, Some(1234))
            .unwrap();
        assert_eq!(unchanged_plan.since_ts, Some(1234));
        assert_eq!(unchanged_plan.root_fingerprint, first_plan.root_fingerprint);
    }

    #[test]
    fn test_incomplete_connector_scan_retries_without_time_bound() {
        let temp_dir = TempDir::new().unwrap();
        let indexer = Indexer::new(&temp_dir.path().to_path_buf(), false).unwrap();
        let connector = crate::connectors::pi_agent::PiAgentConnector::new();
        let roots = vec![temp_dir.path().join("pi-agent")];
        std::fs::create_dir_all(&roots[0]).unwrap();
        let root_fingerprint = Indexer::discovered_root_fingerprint(&roots);
        indexer
            .record_connector_root_fingerprint(&connector, &root_fingerprint)
            .unwrap();
        indexer
            .record_connector_parser_revision(&connector)
            .unwrap();

        assert_eq!(
            indexer
                .connector_scan_plan(&connector, &roots, Some(1234))
                .unwrap()
                .since_ts,
            Some(1234)
        );

        indexer.mark_connector_scan_incomplete(&connector).unwrap();
        let retry = indexer
            .connector_scan_plan(&connector, &roots, Some(1234))
            .unwrap();
        assert_eq!(retry.since_ts, None);
        assert_eq!(retry.root_fingerprint, root_fingerprint);
    }

    #[test]
    fn test_changed_connector_roots_force_unbounded_scan() {
        let temp_dir = TempDir::new().unwrap();
        let indexer = Indexer::new(&temp_dir.path().to_path_buf(), false).unwrap();
        let connector = crate::connectors::pi_agent::PiAgentConnector::new();
        let first_root = temp_dir.path().join("pi-agent");
        let additional_root = temp_dir.path().join("fleet-agent");
        std::fs::create_dir_all(&first_root).unwrap();
        std::fs::create_dir_all(&additional_root).unwrap();

        let initial_fingerprint =
            Indexer::discovered_root_fingerprint(std::slice::from_ref(&first_root));
        indexer
            .record_connector_root_fingerprint(&connector, &initial_fingerprint)
            .unwrap();

        let changed_plan = indexer
            .connector_scan_plan(&connector, &[first_root, additional_root], Some(1234))
            .unwrap();
        assert_eq!(changed_plan.since_ts, None);
        assert_ne!(changed_plan.root_fingerprint, initial_fingerprint);
    }

    #[test]
    fn test_late_discovered_connector_root_forces_unbounded_scan() {
        let temp_dir = TempDir::new().unwrap();
        let indexer = Indexer::new(&temp_dir.path().to_path_buf(), false).unwrap();
        let connector = crate::connectors::pi_agent::PiAgentConnector::new();
        let first_root = temp_dir.path().join("pi-agent");
        let late_root = temp_dir.path().join("later-fleet-agent");
        std::fs::create_dir_all(&first_root).unwrap();
        let configured_roots = vec![first_root, late_root.clone()];

        let before_discovery = Indexer::discovered_root_fingerprint(&configured_roots);
        indexer
            .record_connector_root_fingerprint(&connector, &before_discovery)
            .unwrap();
        std::fs::create_dir_all(late_root).unwrap();

        let discovered_plan = indexer
            .connector_scan_plan(&connector, &configured_roots, Some(1234))
            .unwrap();
        assert_eq!(discovered_plan.since_ts, None);
        assert_ne!(discovered_plan.root_fingerprint, before_discovery);
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
            messages: vec![crate::model::Message {
                idx: 0,
                role: crate::model::Role::User,
                content: "Hello".to_string(),
                timestamp: Some(1000),
                model: None,
            }],
            usage: vec![],
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
            usage: vec![],
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
