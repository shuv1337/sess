use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

use session_search::model::{
    Agent, Conversation, Message, Role, SourceFile, UsageRecord, source_fingerprint,
};
use session_search::search::TantivyIndex;
use session_search::storage::Storage;

struct SeededData {
    _tmp: TempDir,
    data_dir: PathBuf,
    ids: SeededIds,
}

struct SeededIds {
    pi_voice_app_recent: i64,
    codex_voice_old: i64,
    claude_nonvoice: i64,
}

fn make_conversation(
    agent: Agent,
    workspace: &str,
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

    let (provider, model, input, output, estimate) = match agent {
        Agent::PiAgent => (Some("anthropic"), "claude-sonnet", 100, 50, Some(0.01)),
        Agent::Codex => (Some("openai"), "gpt-5", 200, 100, None),
        Agent::ClaudeCode => (None, "claude-opus", 80, 40, None),
        Agent::Hermes => (Some("openrouter"), "hermes-model", 50, 25, Some(0.02)),
        Agent::OpenCode => (Some("openai"), "gpt-5-mini", 40, 20, Some(0.001)),
    };

    Conversation {
        agent,
        external_id: None,
        title: Some(title.to_string()),
        workspace: Some(PathBuf::from(workspace)),
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
        usage: vec![UsageRecord {
            timestamp: Some(started_at),
            provider: provider.map(str::to_string),
            model: Some(model.to_string()),
            source_event_id: None,
            api_calls: 1,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: input + output,
            actual_cost_usd: None,
            estimated_cost_usd: estimate,
        }],
    }
}

fn seed_data() -> SeededData {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let db_path = data_dir.join("sess.db");
    let tantivy_path = data_dir.join("tantivy");

    let mut storage = Storage::new(&db_path).expect("storage");
    let mut tantivy = TantivyIndex::open_or_create(&tantivy_path).expect("tantivy");
    tantivy.start_writer().expect("writer");

    let now = chrono::Utc::now().timestamp_millis();
    let ten_days_ago = now - (10 * 24 * 60 * 60 * 1000);

    let conv_pi_voice_app_recent = make_conversation(
        Agent::PiAgent,
        "/home/user/repos/voice-app",
        "/tmp/sess/pi-voice-app.jsonl",
        "Voice regression in voice-app",
        "Investigate why voice recognition in voice-app misses words.",
        now,
    );

    let conv_codex_voice_old = make_conversation(
        Agent::Codex,
        "/home/user/repos/other-project",
        "/tmp/sess/codex-voice.jsonl",
        "Improve voice preprocessing",
        "Tune voice pipeline and audio normalization for commands.",
        ten_days_ago,
    );

    let conv_claude_nonvoice = make_conversation(
        Agent::ClaudeCode,
        "/home/user/repos/non-voice",
        "/tmp/sess/claude-nonvoice.jsonl",
        "Refactor auth middleware",
        "Implement token refresh with robust auth middleware.",
        now,
    );

    let up1 = storage
        .upsert_conversation(&conv_pi_voice_app_recent)
        .expect("upsert 1");
    tantivy
        .add_conversation(&conv_pi_voice_app_recent, up1.conversation_id)
        .expect("index 1");

    let up2 = storage
        .upsert_conversation(&conv_codex_voice_old)
        .expect("upsert 2");
    tantivy
        .add_conversation(&conv_codex_voice_old, up2.conversation_id)
        .expect("index 2");

    let up3 = storage
        .upsert_conversation(&conv_claude_nonvoice)
        .expect("upsert 3");
    tantivy
        .add_conversation(&conv_claude_nonvoice, up3.conversation_id)
        .expect("index 3");

    tantivy.commit().expect("commit");

    drop(tantivy);
    drop(storage);

    SeededData {
        _tmp: tmp,
        data_dir,
        ids: SeededIds {
            pi_voice_app_recent: up1.conversation_id,
            codex_voice_old: up2.conversation_id,
            claude_nonvoice: up3.conversation_id,
        },
    }
}

fn run_search(data_dir: &Path, args: &[&str]) -> Value {
    let mut cmd = Command::cargo_bin("sess").expect("sess binary");
    cmd.arg("--data-dir")
        .arg(data_dir)
        .arg("--no-auto-index")
        .arg("search");

    for arg in args {
        cmd.arg(arg);
    }

    let assert = cmd.assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");
    serde_json::from_str(&stdout).expect("json output")
}

fn run_usage_json(data_dir: &Path, args: &[&str]) -> Value {
    let mut cmd = Command::cargo_bin("sess").expect("sess binary");
    cmd.arg("--data-dir")
        .arg(data_dir)
        .arg("--no-auto-index")
        .arg("usage");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg("--json");
    let assert = cmd.assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");
    serde_json::from_str(&stdout).expect("json output")
}

#[test]
fn cli_voice_search_returns_voice_app_hits() {
    let seeded = seed_data();
    let json = run_search(&seeded.data_dir, &["voice", "--json", "--limit", "20"]);

    assert_eq!(json["query"], "voice");
    assert!(json["total_hits"].as_u64().unwrap() >= 2);

    let hits = json["hits"].as_array().expect("hits array");
    assert!(!hits.is_empty());

    let has_voice_app = hits.iter().any(|h| {
        h["workspace"]
            .as_str()
            .map(|w| w == "/home/user/repos/voice-app")
            .unwrap_or(false)
    });
    assert!(has_voice_app, "expected at least one voice-app hit");
}

#[test]
fn cli_workspace_filter_is_strict() {
    let seeded = seed_data();
    let json = run_search(
        &seeded.data_dir,
        &[
            "voice",
            "--workspace",
            "/home/user/repos/voice-app",
            "--json",
            "--limit",
            "20",
        ],
    );

    assert_eq!(json["total_hits"].as_u64().unwrap(), 1);

    let hits = json["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["workspace"], "/home/user/repos/voice-app");
    assert_eq!(
        hits[0]["id"].as_i64().unwrap(),
        seeded.ids.pi_voice_app_recent
    );
}

#[test]
fn cli_agent_filter_returns_only_requested_agent() {
    let seeded = seed_data();
    let json = run_search(
        &seeded.data_dir,
        &["voice", "--agent", "pi_agent", "--json", "--limit", "20"],
    );

    assert_eq!(json["total_hits"].as_u64().unwrap(), 1);

    let hits = json["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["agent"], "pi_agent");
    assert_eq!(
        hits[0]["id"].as_i64().unwrap(),
        seeded.ids.pi_voice_app_recent
    );
}

#[test]
fn cli_search_pagination_contract() {
    let seeded = seed_data();

    let page0 = run_search(
        &seeded.data_dir,
        &["voice", "--json", "--limit", "1", "--offset", "0"],
    );
    let page1 = run_search(
        &seeded.data_dir,
        &["voice", "--json", "--limit", "1", "--offset", "1"],
    );

    assert_eq!(page0["total_hits"], page1["total_hits"]);
    assert_eq!(page0["total_hits"].as_u64().unwrap(), 2);

    let p0_id = page0["hits"][0]["id"].as_i64().unwrap();
    let p1_id = page1["hits"][0]["id"].as_i64().unwrap();
    assert_ne!(p0_id, p1_id, "pages should not repeat first item");

    // Should include the old codex voice hit somewhere
    let ids = [p0_id, p1_id];
    assert!(ids.contains(&seeded.ids.codex_voice_old));
}

#[test]
fn cli_since_filter_applies_to_total_and_hits() {
    let seeded = seed_data();

    // Only the recent Pi voice-app conversation is from today.
    let json = run_search(
        &seeded.data_dir,
        &["voice", "--since", "today", "--json", "--limit", "20"],
    );

    assert_eq!(json["total_hits"].as_u64().unwrap(), 1);
    let hits = json["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0]["id"].as_i64().unwrap(),
        seeded.ids.pi_voice_app_recent
    );
}

#[test]
fn cli_json_shape_contract_for_bots() {
    let seeded = seed_data();
    let json = run_search(&seeded.data_dir, &["voice", "--json", "--limit", "5"]);

    assert!(json.get("query").is_some());
    assert!(json.get("total_hits").and_then(|v| v.as_u64()).is_some());
    assert!(json.get("query_time_ms").and_then(|v| v.as_u64()).is_some());
    assert!(json.get("hits").and_then(|v| v.as_array()).is_some());

    let hit = &json["hits"][0];
    assert!(hit.get("id").and_then(|v| v.as_i64()).is_some());
    assert!(hit.get("agent").and_then(|v| v.as_str()).is_some());
    assert!(hit.get("title").and_then(|v| v.as_str()).is_some());
    assert!(hit.get("source_path").and_then(|v| v.as_str()).is_some());
    assert!(hit.get("preview").and_then(|v| v.as_str()).is_some());
    assert!(hit.get("created_at").and_then(|v| v.as_str()).is_some());
    assert!(hit.get("score").and_then(|v| v.as_f64()).is_some());

    // Non-matching conversation should never appear in voice search
    let hits = json["hits"].as_array().unwrap();
    let has_nonvoice_conv = hits
        .iter()
        .any(|h| h["id"].as_i64() == Some(seeded.ids.claude_nonvoice));
    assert!(!has_nonvoice_conv);
}

#[test]
fn cli_usage_reports_harness_provider_and_model_splits() {
    let seeded = seed_data();
    let json = run_usage_json(&seeded.data_dir, &[]);

    assert_eq!(json["totals"]["tokens"]["total"], 570);
    assert_eq!(json["totals"]["api_calls"], 3);
    assert_eq!(json["by_harness"].as_array().unwrap().len(), 3);
    assert!(
        json["by_provider"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["label"] == "Unknown")
    );
    assert!(
        json["by_model"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["label"] == "gpt-5")
    );
}

#[test]
fn cli_usage_accepts_harness_alias_and_writes_standalone_html() {
    let seeded = seed_data();
    let filtered = run_usage_json(&seeded.data_dir, &["--harness", "codex"]);
    assert_eq!(filtered["totals"]["tokens"]["total"], 300);
    assert_eq!(filtered["by_harness"][0]["key"], "codex");

    let report_path = seeded.data_dir.join("reports/usage.html");
    let mut cmd = Command::cargo_bin("sess").expect("sess binary");
    cmd.arg("--data-dir")
        .arg(&seeded.data_dir)
        .arg("--no-auto-index")
        .arg("usage")
        .arg("--html")
        .arg(&report_path)
        .assert()
        .success();
    let html = std::fs::read_to_string(report_path).expect("usage report");
    assert!(html.contains("Agent usage"));
    assert!(html.contains("<svg"));
    assert!(!html.contains("https://"));
}

#[test]
fn cli_usage_json_keeps_stdout_machine_readable_on_fresh_storage() {
    let tmp = TempDir::new().expect("tempdir");
    let mut cmd = Command::cargo_bin("sess").expect("sess binary");
    let assert = cmd
        .arg("--data-dir")
        .arg(tmp.path())
        .arg("--no-auto-index")
        .arg("usage")
        .arg("--json")
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");
    let report: Value = serde_json::from_str(&stdout).expect("stdout is pure JSON");
    assert_eq!(report["totals"]["events"], 0);
}

#[test]
fn cli_usage_rejects_reversed_date_ranges() {
    let seeded = seed_data();
    let mut cmd = Command::cargo_bin("sess").expect("sess binary");
    cmd.arg("--data-dir")
        .arg(&seeded.data_dir)
        .arg("--no-auto-index")
        .arg("usage")
        .arg("--since")
        .arg("2026-07-20")
        .arg("--until")
        .arg("2026-07-10")
        .arg("--json")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--since must not be later than --until",
        ));
}
