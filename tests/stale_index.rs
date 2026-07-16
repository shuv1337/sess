//! Integration tests for the stale-deletion fix (PLAN-stale-index.md).
//!
//! These tests directly exercise the storage + Tantivy stale-deletion APIs
//! that the indexer wires together. They avoid spinning up subprocess + env
//! var sandboxes because env vars are process-global and connector detection
//! depends on the host user's home dir; an in-process test is faster and
//! more reliable for proving Bug A doesn't recur.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;

use session_search::model::{Agent, Conversation, Message, Role, SourceFile, source_fingerprint};
use session_search::search::TantivyIndex;
use session_search::storage::Storage;

fn make_conv(agent: Agent, path: &std::path::Path, content: &str, ts: i64) -> Conversation {
    let source_files = vec![SourceFile {
        path: path.to_path_buf(),
        mtime: ts,
        size: content.len() as u64,
    }];
    Conversation {
        agent,
        external_id: None,
        title: Some("Title".into()),
        workspace: Some(PathBuf::from("/tmp/ws")),
        source_path: path.to_path_buf(),
        source_files: source_files.clone(),
        source_fingerprint: source_fingerprint(&source_files),
        started_at: Some(ts),
        ended_at: Some(ts + 1),
        messages: vec![Message {
            idx: 0,
            role: Role::User,
            content: content.into(),
            timestamp: Some(ts),
            model: None,
        }],
        usage: vec![],
    }
}

struct Fixture {
    _tmp: TempDir,
    data_dir: PathBuf,
    files: TempDir,
}

fn build_fixture() -> (Fixture, Storage, TantivyIndex, Vec<i64>) {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let files = TempDir::new().unwrap();

    let mut storage = Storage::new(&data_dir.join("sess.db")).unwrap();
    let mut tantivy = TantivyIndex::open_or_create(&data_dir.join("tantivy")).unwrap();
    tantivy.start_writer().unwrap();

    // 4 conversations across 2 agents, each backed by a real on-disk file.
    let a1 = files.path().join("pi-1.jsonl");
    let a2 = files.path().join("pi-2.jsonl");
    let b1 = files.path().join("oc-1.jsonl");
    let b2 = files.path().join("oc-2.jsonl");
    for p in [&a1, &a2, &b1, &b2] {
        fs::write(p, "{}\n").unwrap();
    }

    let convs = vec![
        make_conv(Agent::PiAgent, &a1, "alpha pi one", 1000),
        make_conv(Agent::PiAgent, &a2, "alpha pi two", 1100),
        make_conv(Agent::OpenCode, &b1, "alpha oc one", 1200),
        make_conv(Agent::OpenCode, &b2, "alpha oc two", 1300),
    ];

    let mut ids = Vec::new();
    for c in &convs {
        let up = storage.upsert_conversation(c).unwrap();
        tantivy.add_conversation(c, up.conversation_id).unwrap();
        ids.push(up.conversation_id);
    }
    tantivy.commit().unwrap();

    (
        Fixture {
            _tmp: tmp,
            data_dir,
            files,
        },
        storage,
        tantivy,
        ids,
    )
}

#[test]
fn incremental_stale_sweep_keeps_unchanged_agents() {
    // Reproduces Bug A: previously, an incremental run that scanned no files
    // would wipe every row. Now, with existence-based deletion, no rows go.
    let (_fx, mut storage, mut tantivy, _ids) = build_fixture();

    let baseline = storage.stats().unwrap().total_conversations;
    assert_eq!(baseline, 4);

    let mut detected = HashSet::new();
    detected.insert(Agent::PiAgent);
    detected.insert(Agent::OpenCode);

    // Simulate incremental sweep: no scan happened (since_ts filtered everything),
    // and we only invoke stale deletion. Files all still exist on disk.
    let summary = storage.delete_missing_sources(&detected).unwrap();
    assert!(summary.deleted_ids.is_empty(), "no rows must be deleted");
    tantivy.delete_conversations(&summary.deleted_ids).unwrap();
    tantivy.commit().unwrap();

    let after = storage.stats().unwrap();
    assert_eq!(after.total_conversations, 4);
}

#[test]
fn stale_sweep_drops_deleted_files_in_both_stores() {
    let (fx, mut storage, mut tantivy, ids) = build_fixture();

    // Delete one file on disk.
    let target = fx.files.path().join("oc-1.jsonl");
    fs::remove_file(&target).unwrap();
    let target_id = ids[2];

    let mut detected = HashSet::new();
    detected.insert(Agent::PiAgent);
    detected.insert(Agent::OpenCode);

    let summary = storage.delete_missing_sources(&detected).unwrap();
    assert_eq!(summary.deleted_ids, vec![target_id]);

    tantivy.delete_conversations(&summary.deleted_ids).unwrap();
    tantivy.commit().unwrap();

    // SQLite count drops by 1.
    let stats = storage.stats().unwrap();
    assert_eq!(stats.total_conversations, 3);

    // Tantivy count drops by 1.
    assert_eq!(tantivy.doc_count().unwrap(), 3);
}

#[test]
fn undetected_agent_rows_are_preserved_even_if_files_missing() {
    let (fx, mut storage, mut tantivy, _ids) = build_fixture();

    // Delete all OpenCode files on disk; pretend OpenCode is no longer
    // detected (env var disappeared / mount unmounted).
    for name in ["oc-1.jsonl", "oc-2.jsonl"] {
        fs::remove_file(fx.files.path().join(name)).unwrap();
    }

    let mut detected = HashSet::new();
    detected.insert(Agent::PiAgent); // OpenCode intentionally missing

    let summary = storage.delete_missing_sources(&detected).unwrap();
    assert!(
        summary.deleted_ids.is_empty(),
        "OpenCode rows must survive when agent is undetected"
    );
    tantivy.delete_conversations(&summary.deleted_ids).unwrap();
    tantivy.commit().unwrap();

    let stats = storage.stats().unwrap();
    assert_eq!(stats.total_conversations, 4);
}
