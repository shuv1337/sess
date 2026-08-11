//! Integration contract for directory-based project scoping.
//!
//! Running sess inside a repository scopes search/stats/usage to sessions
//! whose workspace lives in that repository (across all harnesses), keeps
//! workspace-less sessions visible, and `-g/--all` restores the global view.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

use session_search::model::{Agent, Conversation, Message, Role, SourceFile, source_fingerprint};
use session_search::search::TantivyIndex;
use session_search::storage::Storage;

fn make_conversation(
    agent: Agent,
    workspace: Option<&Path>,
    source_path: &str,
    title: &str,
    content: &str,
    started_at: i64,
) -> Conversation {
    let source_files = vec![SourceFile {
        path: PathBuf::from(source_path),
        mtime: started_at,
        size: content.len() as u64,
    }];
    Conversation {
        agent,
        external_id: None,
        title: Some(title.to_string()),
        workspace: workspace.map(Path::to_path_buf),
        source_path: PathBuf::from(source_path),
        source_files: source_files.clone(),
        source_fingerprint: source_fingerprint(&source_files),
        started_at: Some(started_at),
        ended_at: Some(started_at + 1000),
        messages: vec![Message {
            idx: 0,
            role: Role::User,
            content: content.to_string(),
            timestamp: Some(started_at),
            model: None,
        }],
        usage: vec![],
        metadata: Default::default(),
    }
}

struct Seeded {
    _tmp: TempDir,
    data_dir: PathBuf,
    /// Fake repo root; contains a real `.git` dir so scope resolution stops
    /// here deterministically regardless of the host filesystem above it.
    repo_dir: PathBuf,
}

fn seed() -> Seeded {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().join("data");
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    std::fs::create_dir_all(repo_dir.join(".git")).expect("repo .git");
    std::fs::create_dir_all(repo_dir.join("src")).expect("repo subdir");

    let mut storage = Storage::new(&data_dir.join("sess.db")).expect("storage");
    let mut tantivy = TantivyIndex::open_or_create(&data_dir.join("tantivy")).expect("tantivy");
    tantivy.start_writer().expect("writer");

    let now = chrono::Utc::now().timestamp_millis();
    let convs = [
        make_conversation(
            Agent::PiAgent,
            Some(&repo_dir),
            "/tmp/scope/in-root.jsonl",
            "Fix parser in repo root",
            "parser panic fix session",
            now,
        ),
        make_conversation(
            Agent::Codex,
            Some(&repo_dir.join("src")),
            "/tmp/scope/in-subdir.jsonl",
            "Refactor src module",
            "parser refactor in subdirectory",
            now - 1000,
        ),
        make_conversation(
            Agent::ClaudeCode,
            Some(Path::new("/somewhere/else")),
            "/tmp/scope/outside.jsonl",
            "Unrelated project work",
            "parser work for another project",
            now - 2000,
        ),
        make_conversation(
            Agent::Hermes,
            None,
            "/tmp/scope/no-workspace.jsonl",
            "Recorded without cwd",
            "parser conversation with no workspace",
            now - 3000,
        ),
    ];
    for conv in &convs {
        let up = storage.upsert_conversation(conv).expect("upsert");
        tantivy
            .add_conversation(conv, up.conversation_id)
            .expect("index");
    }
    tantivy.commit().expect("commit");
    // Seeded documents already use the current doc format; suppress the
    // one-time auto-rebuild (kept meta in sync the way a real index run does).
    storage
        .set_meta("tantivy_doc_version", "2")
        .expect("doc version meta");

    Seeded {
        _tmp: tmp,
        data_dir,
        repo_dir,
    }
}

fn run_json(seeded: &Seeded, cwd: &Path, args: &[&str]) -> Value {
    let mut cmd = Command::cargo_bin("sess").expect("sess binary");
    cmd.current_dir(cwd)
        .arg("--data-dir")
        .arg(&seeded.data_dir)
        .arg("--no-auto-index");
    for arg in args {
        cmd.arg(arg);
    }
    let assert = cmd.assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");
    serde_json::from_str(&stdout).expect("json output")
}

#[test]
fn search_inside_repo_scopes_to_project_and_keeps_null_workspaces() {
    let seeded = seed();
    let out = run_json(
        &seeded,
        &seeded.repo_dir.join("src"),
        &["search", "parser", "--json"],
    );

    assert_eq!(out["total_hits"], 3, "root + subdir + null-workspace");
    let titles: Vec<&str> = out["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["title"].as_str().unwrap())
        .collect();
    assert!(titles.contains(&"Fix parser in repo root"));
    assert!(titles.contains(&"Refactor src module"));
    assert!(titles.contains(&"Recorded without cwd"));
    assert!(!titles.contains(&"Unrelated project work"));
}

#[test]
fn search_with_all_flag_is_global() {
    let seeded = seed();
    let out = run_json(
        &seeded,
        &seeded.repo_dir,
        &["--all", "search", "parser", "--json"],
    );
    assert_eq!(out["total_hits"], 4);

    // Short form.
    let out = run_json(
        &seeded,
        &seeded.repo_dir,
        &["-g", "search", "parser", "--json"],
    );
    assert_eq!(out["total_hits"], 4);
}

#[test]
fn explicit_workspace_filter_overrides_scope() {
    let seeded = seed();
    let out = run_json(
        &seeded,
        &seeded.repo_dir,
        &[
            "search",
            "parser",
            "--workspace",
            "/somewhere/else",
            "--json",
        ],
    );
    assert_eq!(out["total_hits"], 1);
    assert_eq!(out["hits"][0]["title"], "Unrelated project work");
}

#[test]
fn stats_inside_repo_are_scoped_and_report_scope() {
    let seeded = seed();
    let scoped = run_json(&seeded, &seeded.repo_dir, &["stats", "--json"]);
    assert_eq!(scoped["total_conversations"], 3);
    assert!(scoped["scope"].as_str().is_some(), "scope label reported");

    let global = run_json(&seeded, &seeded.repo_dir, &["-g", "stats", "--json"]);
    assert_eq!(global["total_conversations"], 4);
    assert!(global.get("scope").is_none());
}
