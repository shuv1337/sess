use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument as Document, Term,
    schema::{FAST, INDEXED, STORED, STRING, Schema, TEXT},
};

use crate::model::Conversation;
use crate::storage::ConversationRow;

/// Tantivy search index
pub struct TantivyIndex {
    index: Index,
    schema: Schema,
    writer: Option<IndexWriter>,
    reader: IndexReader,

    // Field handles
    field_agent: tantivy::schema::Field,
    field_workspace: tantivy::schema::Field,
    field_source_path: tantivy::schema::Field,
    field_title: tantivy::schema::Field,
    field_content: tantivy::schema::Field,
    field_preview: tantivy::schema::Field,
    field_created_at: tantivy::schema::Field,
    field_conv_db_id: tantivy::schema::Field,
}

impl Clone for TantivyIndex {
    fn clone(&self) -> Self {
        // Create new reader for the clone
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .expect("Failed to clone reader");

        // IMPORTANT: Reload the reader to ensure it sees existing index data.
        // The OnCommitWithDelay policy only reloads on FUTURE commits,
        // so we must manually reload to see data that was committed before
        // this reader was created.
        reader.reload().expect("Failed to reload reader");

        Self {
            index: self.index.clone(),
            schema: self.schema.clone(),
            writer: None, // Writer cannot be cloned
            reader,
            field_agent: self.field_agent,
            field_workspace: self.field_workspace,
            field_source_path: self.field_source_path,
            field_title: self.field_title,
            field_content: self.field_content,
            field_preview: self.field_preview,
            field_created_at: self.field_created_at,
            field_conv_db_id: self.field_conv_db_id,
        }
    }
}

impl TantivyIndex {
    pub fn open_or_create(path: &Path) -> Result<Self> {
        fs::create_dir_all(path)?;

        let schema = build_schema();

        // Check for schema hash file
        let hash_path = path.join("schema_hash.json");
        let current_hash = schema_hash(&schema);

        let needs_rebuild = if hash_path.exists() {
            let stored_hash = fs::read_to_string(&hash_path).unwrap_or_default();
            stored_hash != current_hash
        } else {
            true
        };

        if needs_rebuild && path.join("meta.json").exists() {
            tracing::info!("Schema changed, rebuilding Tantivy index");
            fs::remove_dir_all(path)?;
            fs::create_dir_all(path)?;
        }

        let index = Index::open_or_create(
            tantivy::directory::MmapDirectory::open(path)?,
            schema.clone(),
        )?;

        // Write schema hash
        fs::write(hash_path, current_hash)?;

        let field_agent = schema.get_field("agent")?;
        let field_workspace = schema.get_field("workspace")?;
        let field_source_path = schema.get_field("source_path")?;
        let field_title = schema.get_field("title")?;
        let field_content = schema.get_field("content")?;
        let field_preview = schema.get_field("preview")?;
        let field_created_at = schema.get_field("created_at")?;
        let field_conv_db_id = schema.get_field("conv_db_id")?;

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        // Reload reader to ensure we see any existing index data
        reader.reload()?;

        Ok(Self {
            index,
            schema,
            writer: None,
            reader,
            field_agent,
            field_workspace,
            field_source_path,
            field_title,
            field_content,
            field_preview,
            field_created_at,
            field_conv_db_id,
        })
    }

    pub fn start_writer(&mut self) -> Result<()> {
        if self.writer.is_none() {
            let writer = self.index.writer(50_000_000)?; // 50MB buffer
            self.writer = Some(writer);
        }
        Ok(())
    }

    pub fn add_conversation(&mut self, conv: &Conversation, db_id: i64) -> Result<()> {
        // Delete existing document for this conversation first
        self.remove_conversation(db_id)?;

        let writer = self.writer.as_mut().context("Writer not started")?;

        // Create new document using the new Document API
        let mut doc = Document::default();

        doc.add_text(self.field_agent, conv.agent.slug());

        if let Some(ref workspace) = conv.workspace {
            doc.add_text(self.field_workspace, workspace.to_string_lossy());
        }

        doc.add_text(self.field_source_path, conv.source_path.to_string_lossy());

        let title = conv.derive_title();
        doc.add_text(self.field_title, &title);

        let full_text = conv.full_text();
        doc.add_text(self.field_content, &full_text);

        let preview = conv.preview();
        doc.add_text(self.field_preview, &preview);

        let created_at = conv.started_at.unwrap_or(0);
        doc.add_i64(self.field_created_at, created_at);

        doc.add_u64(self.field_conv_db_id, db_id as u64);

        writer.add_document(doc)?;

        Ok(())
    }

    pub fn remove_conversation(&mut self, db_id: i64) -> Result<()> {
        if let Some(ref mut writer) = self.writer {
            let term = Term::from_field_u64(self.field_conv_db_id, db_id as u64);
            writer.delete_term(term);
        }
        Ok(())
    }

    /// Batch-delete conversations from the index by their SQLite IDs.
    ///
    /// Caller is responsible for calling [`Self::commit`] afterwards so the
    /// reader can see the change. Starts the writer if it has not been started
    /// yet so background refresh paths can call this without separate setup.
    pub fn delete_conversations(&mut self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        if self.writer.is_none() {
            self.start_writer()?;
        }
        let writer = self.writer.as_mut().context("Writer not started")?;
        for id in ids {
            let term = Term::from_field_u64(self.field_conv_db_id, *id as u64);
            writer.delete_term(term);
        }
        Ok(())
    }

    /// Reload the search reader so subsequent searches see the latest commit.
    pub fn reload_reader(&self) -> Result<()> {
        self.reader.reload()?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        if let Some(ref mut writer) = self.writer {
            writer.commit()?;
            self.reader.reload()?;
        }
        Ok(())
    }

    pub fn reader(&self) -> &IndexReader {
        &self.reader
    }

    pub fn doc_count(&self) -> Result<usize> {
        let searcher = self.reader.searcher();
        Ok(searcher.num_docs() as usize)
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Rebuild the entire index from SQLite conversations
    pub fn rebuild_from_sqlite(
        &mut self,
        conversations: &[(ConversationRow, String)], // (row, full_text)
    ) -> Result<()> {
        // Clear existing index
        if let Some(ref mut writer) = self.writer {
            writer.delete_all_documents()?;
        } else {
            self.start_writer()?;
        }

        let writer = self.writer.as_mut().unwrap();

        for (row, full_text) in conversations {
            let mut doc = Document::default();

            doc.add_text(self.field_agent, row.agent.slug());

            if let Some(ref workspace) = row.workspace {
                doc.add_text(self.field_workspace, workspace);
            }

            doc.add_text(self.field_source_path, &row.source_path);

            let title = row.title.clone().unwrap_or_else(|| "Untitled".to_string());
            doc.add_text(self.field_title, &title);
            doc.add_text(self.field_content, full_text);

            let preview = if full_text.len() > 300 {
                format!("{}...", &full_text[..300])
            } else {
                full_text.clone()
            };
            doc.add_text(self.field_preview, &preview);

            let created_at = row.started_at.unwrap_or(0);
            doc.add_i64(self.field_created_at, created_at);

            doc.add_u64(self.field_conv_db_id, row.id as u64);

            writer.add_document(doc)?;
        }

        self.commit()?;

        Ok(())
    }
}

fn build_schema() -> Schema {
    let mut builder = Schema::builder();

    builder.add_text_field("agent", STRING | STORED);
    builder.add_text_field("workspace", STRING | STORED);
    builder.add_text_field("source_path", STORED);
    builder.add_text_field("title", TEXT | STORED);
    builder.add_text_field("content", TEXT | STORED);
    builder.add_text_field("preview", STORED);
    builder.add_i64_field("created_at", INDEXED | STORED | FAST);
    builder.add_u64_field("conv_db_id", INDEXED | STORED);

    builder.build()
}

fn schema_hash(schema: &Schema) -> String {
    use serde::Serialize;

    #[derive(Serialize)]
    struct FieldInfo {
        name: String,
        type_: String,
    }

    let fields: Vec<FieldInfo> = schema
        .fields()
        .map(|(field, _)| {
            let entry = schema.get_field_entry(field);
            FieldInfo {
                name: entry.name().to_string(),
                type_: format!("{:?}", entry.field_type()),
            }
        })
        .collect();

    let json = serde_json::to_string(&fields).unwrap();
    blake3::hash(json.as_bytes()).to_hex()[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn create_test_conversation() -> Conversation {
        Conversation {
            agent: crate::model::Agent::ClaudeCode,
            external_id: Some("test-123".to_string()),
            title: Some("Test Conversation".to_string()),
            workspace: Some(PathBuf::from("/test/workspace")),
            source_path: PathBuf::from("/test/session.jsonl"),
            source_files: vec![],
            source_fingerprint: "abc123".to_string(),
            started_at: Some(1000),
            ended_at: Some(2000),
            messages: vec![crate::model::Message {
                idx: 0,
                role: crate::model::Role::User,
                content: "Hello world".to_string(),
                timestamp: Some(1000),
                model: None,
            }],
            usage: vec![],
            metadata: Default::default(),
        }
    }

    #[test]
    fn test_index_basic() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();

        index.start_writer().unwrap();

        let conv = create_test_conversation();
        index.add_conversation(&conv, 1).unwrap();
        index.commit().unwrap();

        let count = index.doc_count().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_index_upsert() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();

        index.start_writer().unwrap();

        let mut conv = create_test_conversation();
        index.add_conversation(&conv, 1).unwrap();
        index.commit().unwrap();

        // Update same conversation
        conv.messages[0].content = "Updated content".to_string();
        index.add_conversation(&conv, 1).unwrap();
        index.commit().unwrap();

        let count = index.doc_count().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_index_multiple_documents() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        index.start_writer().unwrap();

        for i in 0..5 {
            let mut conv = create_test_conversation();
            conv.source_path = PathBuf::from(format!("/test/{}.jsonl", i));
            index.add_conversation(&conv, i as i64).unwrap();
        }
        index.commit().unwrap();

        assert_eq!(index.doc_count().unwrap(), 5);
    }

    #[test]
    fn test_index_remove_conversation() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        index.start_writer().unwrap();

        let conv = create_test_conversation();
        index.add_conversation(&conv, 1).unwrap();
        index.commit().unwrap();
        assert_eq!(index.doc_count().unwrap(), 1);

        index.remove_conversation(1).unwrap();
        index.commit().unwrap();
        assert_eq!(index.doc_count().unwrap(), 0);
    }

    #[test]
    fn delete_conversations_removes_multiple_docs_with_single_commit() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        index.start_writer().unwrap();
        for i in 1..=5 {
            let mut conv = create_test_conversation();
            conv.source_path = PathBuf::from(format!("/test/{}.jsonl", i));
            index.add_conversation(&conv, i).unwrap();
        }
        index.commit().unwrap();
        assert_eq!(index.doc_count().unwrap(), 5);

        index.delete_conversations(&[2, 4]).unwrap();
        index.commit().unwrap();
        assert_eq!(index.doc_count().unwrap(), 3);
    }

    #[test]
    fn delete_conversations_noop_on_empty_input() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        // Note: writer not started — must still succeed.
        index.delete_conversations(&[]).unwrap();
    }

    #[test]
    fn test_index_remove_nonexistent() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        index.start_writer().unwrap();

        // Removing nonexistent should not fail
        index.remove_conversation(999).unwrap();
        index.commit().unwrap();
        assert_eq!(index.doc_count().unwrap(), 0);
    }

    #[test]
    fn test_index_reopen() {
        let temp_dir = TempDir::new().unwrap();

        // Create and populate
        {
            let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
            index.start_writer().unwrap();
            let conv = create_test_conversation();
            index.add_conversation(&conv, 1).unwrap();
            index.commit().unwrap();
        }

        // Reopen and verify
        {
            let index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
            assert_eq!(index.doc_count().unwrap(), 1);
        }
    }

    #[test]
    fn test_index_clone() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        index.start_writer().unwrap();

        let conv = create_test_conversation();
        index.add_conversation(&conv, 1).unwrap();
        index.commit().unwrap();

        let cloned = index.clone();
        assert_eq!(cloned.doc_count().unwrap(), 1);
        assert!(cloned.writer.is_none()); // Clone has no writer
    }

    #[test]
    fn test_index_rebuild_from_sqlite() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        index.start_writer().unwrap();

        let rows = vec![
            (
                ConversationRow {
                    id: 1,
                    agent: crate::model::Agent::ClaudeCode,
                    external_id: Some("s1".to_string()),
                    title: Some("Test".to_string()),
                    workspace: Some("/test".to_string()),
                    source_path: "/test/s1.jsonl".to_string(),
                    started_at: Some(1000),
                    ended_at: Some(2000),
                    source_fingerprint: "fp1".to_string(),
                    logical_session_id: Some("s1".to_string()),
                    parent_external_id: None,
                    record_kind: "top_level".to_string(),
                    is_synthetic: false,
                },
                "[user] Hello\n[assistant] World".to_string(),
            ),
            (
                ConversationRow {
                    id: 2,
                    agent: crate::model::Agent::Codex,
                    external_id: None,
                    title: None,
                    workspace: None,
                    source_path: "/test/s2.jsonl".to_string(),
                    started_at: None,
                    ended_at: None,
                    source_fingerprint: "fp2".to_string(),
                    logical_session_id: None,
                    parent_external_id: None,
                    record_kind: "top_level".to_string(),
                    is_synthetic: false,
                },
                "[user] Code review".to_string(),
            ),
        ];

        index.rebuild_from_sqlite(&rows).unwrap();
        assert_eq!(index.doc_count().unwrap(), 2);
    }

    #[test]
    fn test_schema_is_consistent() {
        let temp_dir = TempDir::new().unwrap();
        let index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        let schema = index.schema();

        // Verify all expected fields exist
        assert!(schema.get_field("agent").is_ok());
        assert!(schema.get_field("workspace").is_ok());
        assert!(schema.get_field("source_path").is_ok());
        assert!(schema.get_field("title").is_ok());
        assert!(schema.get_field("content").is_ok());
        assert!(schema.get_field("preview").is_ok());
        assert!(schema.get_field("created_at").is_ok());
        assert!(schema.get_field("conv_db_id").is_ok());
    }

    #[test]
    fn test_start_writer_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();

        index.start_writer().unwrap();
        index.start_writer().unwrap(); // Second call should be a no-op
    }

    #[test]
    fn test_add_without_writer_fails() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        // Don't start writer

        let conv = create_test_conversation();
        let result = index.add_conversation(&conv, 1);
        assert!(result.is_err());
    }
}
