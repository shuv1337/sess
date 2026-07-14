use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, file_modified_since, flatten_json_content, parse_role, source_file,
};
use crate::model::{Agent, Conversation, Message, Role, parse_timestamp, source_fingerprint};

const CODEX_PARSER_REVISION: &str = "2";

pub struct CodexConnector {
    home_dir: Option<PathBuf>,
}

impl CodexConnector {
    pub fn new() -> Self {
        Self {
            home_dir: dirs::home_dir(),
        }
    }

    fn codex_home(&self) -> Option<PathBuf> {
        std::env::var_os("CODEX_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| self.home_dir.as_ref().map(|home| home.join(".codex")))
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        self.codex_home()
            .into_iter()
            .flat_map(|home| [home.join("sessions"), home.join("archived_sessions")])
            .collect()
    }
}

impl Default for CodexConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl Connector for CodexConnector {
    fn agent(&self) -> Agent {
        Agent::Codex
    }

    fn detect(&self) -> bool {
        self.session_roots().iter().any(|path| path.is_dir())
    }

    fn default_roots(&self) -> Vec<PathBuf> {
        self.session_roots()
    }

    fn scan(&self, roots: &[PathBuf], since_ts: Option<i64>) -> Result<Vec<Conversation>> {
        let mut conversations = Vec::new();

        for root in roots {
            if !root.is_dir() {
                continue;
            }

            let mut files_discovered = 0usize;
            let mut files_parsed = 0usize;
            let mut parse_errors = 0usize;

            tracing::debug!(
                agent = Agent::Codex.slug(),
                root = %root.display(),
                since_ts,
                "Starting Codex session scan"
            );

            // Codex CLI stores active and archived rollouts as JSONL (with
            // legacy JSON files supported for compatibility).
            for entry in WalkDir::new(root)
                .follow_links(true)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    if !e.file_type().is_file() {
                        return false;
                    }
                    let name = e.file_name().to_string_lossy();
                    name.starts_with("rollout-")
                        && (name.ends_with(".jsonl") || name.ends_with(".json"))
                })
            {
                files_discovered += 1;
                let path = entry.path();

                // Check if file was modified since the given timestamp
                if !file_modified_since(path, since_ts) {
                    continue;
                }

                match parse_codex_session(path) {
                    Ok(Some(conv)) => {
                        files_parsed += 1;
                        conversations.push(conv);
                    }
                    Ok(None) => {
                        // Empty or no messages, skip
                    }
                    Err(e) => {
                        parse_errors += 1;
                        let action = self.on_parse_error(path, &e);
                        match action {
                            crate::connectors::ErrorAction::Skip => {
                                tracing::warn!(
                                    agent = Agent::Codex.slug(),
                                    source_path = %path.display(),
                                    error = %e,
                                    "Failed to parse Codex session"
                                );
                            }
                            crate::connectors::ErrorAction::Fail => {
                                return Err(e);
                            }
                            crate::connectors::ErrorAction::SkipAgent => {
                                tracing::warn!("Skipping remaining Codex files due to error");
                                return Ok(conversations);
                            }
                        }
                    }
                }
            }

            tracing::debug!(
                agent = Agent::Codex.slug(),
                root = %root.display(),
                files_discovered,
                files_parsed,
                parse_errors,
                "Completed Codex session scan"
            );
        }

        Ok(conversations)
    }

    fn parser_revision(&self) -> Option<&'static str> {
        Some(CODEX_PARSER_REVISION)
    }
}

fn parse_codex_session(path: &Path) -> Result<Option<Conversation>> {
    let ext = path.extension().and_then(|e| e.to_str());

    match ext {
        Some("jsonl") => parse_codex_jsonl(path),
        Some("json") => parse_codex_json(path),
        _ => Ok(None),
    }
}

fn parse_codex_jsonl(path: &Path) -> Result<Option<Conversation>> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);

    let source_file = source_file(path)
        .with_context(|| format!("Failed to get source file info for {}", path.display()))?;

    let mut messages: Vec<Message> = Vec::new();
    let mut external_id: Option<String> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut current_model: Option<String> = None;
    let mut session_title: Option<String> = None;
    let mut user_title_candidates: Vec<String> = Vec::new();
    let mut timestamps: Vec<i64> = Vec::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = line.with_context(|| {
            format!(
                "Failed to read line {} from {}",
                line_num + 1,
                path.display()
            )
        })?;

        if line.trim().is_empty() {
            continue;
        }

        let value: Value = serde_json::from_str(&line).with_context(|| {
            format!(
                "Failed to parse JSON on line {} of {}",
                line_num + 1,
                path.display()
            )
        })?;

        let entry_type = value.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("session_meta") => {
                if let Some(payload) = value.get("payload") {
                    if external_id.is_none() {
                        external_id = payload
                            .get("id")
                            .or_else(|| payload.get("session_id"))
                            .and_then(|v| v.as_str())
                            .map(str::to_owned);
                    }
                    if let Some(cwd) = payload.get("cwd").and_then(|v| v.as_str()) {
                        workspace = Some(PathBuf::from(cwd));
                    }
                    if session_title.is_none() {
                        session_title = subagent_session_title(payload);
                    }
                }
            }
            Some("turn_context") => {
                current_model = value
                    .get("payload")
                    .and_then(|payload| payload.get("model"))
                    .and_then(|model| model.as_str())
                    .map(str::to_owned)
                    .or(current_model);
            }
            Some("response_item") => {
                if let Some(payload) = value.get("payload") {
                    // Modern rollouts distinguish messages from reasoning and
                    // tool-call response items. Older rollouts omitted `type`.
                    let payload_type = payload.get("type").and_then(|v| v.as_str());
                    if !matches!(payload_type, None | Some("message")) {
                        continue;
                    }

                    let content = payload
                        .get("content")
                        .map(flatten_json_content)
                        .unwrap_or_default();

                    let Some(role) = payload
                        .get("role")
                        .and_then(|v| v.as_str())
                        .and_then(|role| parse_codex_role(role, &content))
                    else {
                        continue;
                    };

                    push_message(
                        &mut messages,
                        &mut timestamps,
                        role,
                        content,
                        entry_timestamp(&value, payload),
                        current_model.clone(),
                    );
                }
            }
            Some("event_msg") => {
                // Handle event messages
                if let Some(payload) = value.get("payload") {
                    let event_type = payload.get("type").and_then(|v| v.as_str());

                    match event_type {
                        Some("user_message") => {
                            if let Some(message) = payload.get("message").and_then(|v| v.as_str()) {
                                let role = if is_codex_context_message(message) {
                                    Role::System
                                } else {
                                    Role::User
                                };
                                if role == Role::User
                                    && user_title_candidates.is_empty()
                                    && !message.trim().is_empty()
                                {
                                    user_title_candidates.push(message.trim().to_string());
                                }
                                push_message(
                                    &mut messages,
                                    &mut timestamps,
                                    role,
                                    message.to_string(),
                                    entry_timestamp(&value, payload),
                                    current_model.clone(),
                                );
                            }
                        }
                        Some("agent_message") => {
                            if let Some(message) = payload.get("message").and_then(|v| v.as_str()) {
                                push_message(
                                    &mut messages,
                                    &mut timestamps,
                                    Role::Assistant,
                                    message.to_string(),
                                    entry_timestamp(&value, payload),
                                    current_model.clone(),
                                );
                            }
                        }
                        Some("agent_reasoning") => {
                            if let Some(text) = payload.get("text").and_then(|v| v.as_str()) {
                                push_message(
                                    &mut messages,
                                    &mut timestamps,
                                    Role::Assistant,
                                    text.to_string(),
                                    entry_timestamp(&value, payload),
                                    current_model.clone(),
                                );
                            }
                        }
                        _ => {
                            // Skip other event types like token_count, turn_aborted, etc.
                        }
                    }
                }
            }
            _ => {
                // Unknown type, skip
            }
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    let title = user_title_candidates
        .first()
        .map(String::as_str)
        .or_else(|| {
            messages
                .iter()
                .find(|message| {
                    message.role == Role::User && !is_codex_context_message(&message.content)
                })
                .map(|message| message.content.as_str())
        })
        .map(|content| {
            let first_line = content.lines().next().unwrap_or(content);
            crate::model::truncate_title(first_line, 100)
        })
        .or(session_title);

    let started_at = timestamps.iter().min().copied();
    let ended_at = timestamps.iter().max().copied();

    let source_files = vec![source_file];
    let fingerprint = format!(
        "codex-v{CODEX_PARSER_REVISION}:{}",
        source_fingerprint(&source_files)
    );

    Ok(Some(Conversation {
        agent: Agent::Codex,
        external_id,
        title,
        workspace,
        source_path: path.to_path_buf(),
        source_files,
        source_fingerprint: fingerprint,
        started_at,
        ended_at,
        messages,
    }))
}

fn parse_codex_role(role: &str, content: &str) -> Option<Role> {
    match role.to_ascii_lowercase().as_str() {
        "developer" => Some(Role::System),
        "user" if content.trim_start().starts_with("<user_shell_command>") => Some(Role::Tool),
        "user" if is_codex_context_message(content) => Some(Role::System),
        other => parse_role(other),
    }
}

fn is_codex_context_message(content: &str) -> bool {
    let content = content.trim_start();
    [
        "# AGENTS.md instructions",
        "## Referenced ChatGPT conversation:",
        "<collaboration_mode>",
        "<environment_context>",
        "<model_switch>",
        "<multi_agent_mode>",
        "<permissions instructions>",
        "<recommended_plugins>",
        "<skill>",
    ]
    .iter()
    .any(|prefix| content.starts_with(prefix))
}

fn subagent_session_title(payload: &Value) -> Option<String> {
    let spawn = payload.pointer("/source/subagent/thread_spawn")?;
    let label = spawn
        .get("agent_path")
        .and_then(Value::as_str)
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .or_else(|| spawn.get("agent_nickname").and_then(Value::as_str))?;
    Some(format!("Subagent: {label}"))
}

fn entry_timestamp(value: &Value, payload: &Value) -> Option<i64> {
    value
        .get("timestamp")
        .and_then(parse_timestamp)
        .or_else(|| payload.get("timestamp").and_then(parse_timestamp))
}

fn push_message(
    messages: &mut Vec<Message>,
    timestamps: &mut Vec<i64>,
    role: Role,
    content: String,
    timestamp: Option<i64>,
    model: Option<String>,
) {
    let content = content.trim();
    if content.is_empty() {
        return;
    }

    // Codex emits the same visible message as both a response_item and an
    // event_msg. Keep the event fallback for older rollouts without indexing
    // adjacent duplicates from modern CLI versions.
    if messages.last().is_some_and(|previous| {
        previous.role == role && previous.content == content && previous.timestamp == timestamp
    }) {
        return;
    }

    if let Some(ts) = timestamp {
        timestamps.push(ts);
    }

    messages.push(Message {
        idx: messages.len(),
        role,
        content: content.to_string(),
        timestamp,
        model,
    });
}

#[derive(Deserialize)]
struct LegacyCodexSession {
    #[serde(rename = "session")]
    session: LegacySessionMeta,
    #[serde(default)]
    items: Vec<LegacySessionItem>,
}

#[derive(Deserialize)]
struct LegacySessionMeta {
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct LegacySessionItem {
    role: String,
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    timestamp: Option<f64>,
}

fn parse_codex_json(path: &Path) -> Result<Option<Conversation>> {
    let mut file =
        File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let source_file = source_file(path)
        .with_context(|| format!("Failed to get source file info for {}", path.display()))?;

    // Try to parse as legacy JSON format
    let legacy: LegacyCodexSession = serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse legacy JSON from {}", path.display()))?;

    let workspace = legacy.session.cwd.map(PathBuf::from);
    let mut timestamps: Vec<i64> = Vec::new();

    let messages: Vec<Message> = legacy
        .items
        .into_iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            let role = parse_role(&item.role)?;
            let content = item
                .content
                .as_ref()
                .map(flatten_json_content)
                .unwrap_or_default();

            if content.trim().is_empty() {
                return None;
            }

            let timestamp = item.timestamp.map(|ts| (ts * 1000.0) as i64);
            if let Some(ts) = timestamp {
                timestamps.push(ts);
            }

            Some(Message {
                idx,
                role,
                content,
                timestamp,
                model: None,
            })
        })
        .collect();

    if messages.is_empty() {
        return Ok(None);
    }

    let title = messages.iter().find(|m| m.role == Role::User).map(|m| {
        let first_line = m.content.lines().next().unwrap_or(&m.content);
        crate::model::truncate_title(first_line, 100)
    });

    let started_at = timestamps.iter().min().copied();
    let ended_at = timestamps.iter().max().copied();

    let source_files = vec![source_file];
    let fingerprint = format!(
        "codex-v{CODEX_PARSER_REVISION}:{}",
        source_fingerprint(&source_files)
    );

    Ok(Some(Conversation {
        agent: Agent::Codex,
        external_id: None,
        title,
        workspace,
        source_path: path.to_path_buf(),
        source_files,
        source_fingerprint: fingerprint,
        started_at,
        ended_at,
        messages,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_codex_jsonl() {
        let jsonl_content = r#"
{"type":"session_meta","payload":{"cwd":"/home/user/codex-project","session_id":"test"}}
{"type":"event_msg","timestamp":1705312800.5,"payload":{"type":"user_message","message":"Can you help with this code?"}}
{"type":"response_item","timestamp":1705312805.5,"payload":{"role":"assistant","content":[{"type":"text","text":"I'll help you with that code."}]}}
"#;

        // parse_codex_jsonl works directly on the JSONL content
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(jsonl_content.as_bytes()).unwrap();

        let result = parse_codex_jsonl(temp_file.path()).unwrap();
        assert!(result.is_some());

        let conv = result.unwrap();
        assert_eq!(conv.agent, Agent::Codex);
        assert_eq!(
            conv.workspace,
            Some(PathBuf::from("/home/user/codex-project"))
        );
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, Role::User);
        assert_eq!(conv.messages[0].content, "Can you help with this code?");
        assert_eq!(conv.messages[1].role, Role::Assistant);
        assert!(conv.messages[1].content.contains("I'll help you"));
    }

    #[test]
    fn test_parse_codex_legacy_json() {
        let json_content = r#"
{
  "session": {"cwd": "/home/user/legacy-project"},
  "items": [
    {"role": "user", "content": "Hello", "timestamp": 1705312800.0},
    {"role": "assistant", "content": "Hi there!", "timestamp": 1705312805.0}
  ]
}
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(json_content.as_bytes()).unwrap();
        temp_file
            .as_file_mut()
            .set_len(json_content.len() as u64)
            .unwrap();
        let path = temp_file.path().with_extension("json");
        std::fs::copy(temp_file.path(), &path).unwrap();

        let result = parse_codex_session(&path).unwrap();
        assert!(result.is_some());

        let conv = result.unwrap();
        assert_eq!(conv.agent, Agent::Codex);
        assert_eq!(conv.messages.len(), 2);

        // Clean up
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_parse_codex_jsonl_empty() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(b"").unwrap();

        let result = parse_codex_jsonl(temp_file.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_codex_jsonl_only_meta() {
        let content = r#"{"type":"session_meta","payload":{"cwd":"/home/user/project"}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let result = parse_codex_jsonl(temp_file.path()).unwrap();
        assert!(result.is_none()); // No messages
    }

    #[test]
    fn test_parse_codex_jsonl_agent_reasoning() {
        let content = r#"{"type":"session_meta","payload":{"cwd":"/project"}}
{"type":"event_msg","timestamp":1705312800.5,"payload":{"type":"user_message","message":"What is this?"}}
{"type":"event_msg","timestamp":1705312801.0,"payload":{"type":"agent_reasoning","text":"Let me analyze the code..."}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conv = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, Role::User);
        assert_eq!(conv.messages[1].role, Role::Assistant);
        assert!(conv.messages[1].content.contains("analyze"));
    }

    #[test]
    fn test_parse_codex_jsonl_skips_token_count() {
        let content = r#"{"type":"event_msg","timestamp":1705312800.5,"payload":{"type":"user_message","message":"Hello"}}
{"type":"event_msg","timestamp":1705312801.0,"payload":{"type":"token_count","tokens":500}}
{"type":"event_msg","timestamp":1705312802.0,"payload":{"type":"turn_aborted"}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conv = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();
        assert_eq!(conv.messages.len(), 1); // Only the user_message
    }

    #[test]
    fn test_parse_codex_jsonl_timestamps() {
        let content = r#"{"type":"event_msg","timestamp":1705312800.5,"payload":{"type":"user_message","message":"First"}}
{"type":"event_msg","timestamp":1705312810.5,"payload":{"type":"user_message","message":"Second"}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conv = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();
        assert!(conv.started_at.is_some());
        assert!(conv.ended_at.is_some());
        assert!(conv.started_at.unwrap() < conv.ended_at.unwrap());
    }

    #[test]
    fn test_parse_current_codex_cli_rollout() {
        let content = r##"{"timestamp":"2026-07-13T21:35:54.123Z","type":"session_meta","payload":{"id":"session-current","cwd":"/tmp/current-codex","cli_version":"0.144.3"}}
{"timestamp":"2026-07-13T21:35:55.000Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"system guidance"}]}}
{"timestamp":"2026-07-13T21:35:55.500Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\nInjected project context"}]}}
{"timestamp":"2026-07-13T21:35:56.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"current codex fixture prompt"}]}}
{"timestamp":"2026-07-13T21:35:56.000Z","type":"event_msg","payload":{"type":"user_message","message":"current codex fixture prompt"}}
{"timestamp":"2026-07-13T21:35:57.000Z","type":"event_msg","payload":{"type":"agent_message","message":"fixture answer"}}
{"timestamp":"2026-07-13T21:35:57.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fixture answer"}]}}
"##;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conv = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();

        assert_eq!(conv.external_id.as_deref(), Some("session-current"));
        assert_eq!(conv.title.as_deref(), Some("current codex fixture prompt"));
        assert_eq!(conv.workspace, Some(PathBuf::from("/tmp/current-codex")));
        assert_eq!(conv.started_at, Some(1_783_978_555_000));
        assert_eq!(conv.ended_at, Some(1_783_978_557_000));
        assert_eq!(conv.messages.len(), 4);
        assert_eq!(conv.messages[0].role, Role::System);
        assert_eq!(conv.messages[1].role, Role::System);
        assert_eq!(conv.messages[2].role, Role::User);
        assert_eq!(conv.messages[3].role, Role::Assistant);
        assert!(conv.source_fingerprint.starts_with("codex-v2:"));
    }

    #[test]
    fn test_subagent_session_title_uses_agent_path() {
        let payload = serde_json::json!({
            "source": {
                "subagent": {
                    "thread_spawn": {
                        "agent_path": "/root/desktop_real_pi",
                        "agent_nickname": "Bohr"
                    }
                }
            }
        });

        assert_eq!(
            subagent_session_title(&payload).as_deref(),
            Some("Subagent: desktop_real_pi")
        );
    }

    #[test]
    fn test_parse_codex_legacy_json_empty_items() {
        let json_content = r#"{"session": {"cwd": "/project"}, "items": []}"#;
        let path = std::env::temp_dir().join("test_codex_empty.json");
        std::fs::write(&path, json_content).unwrap();

        let result = parse_codex_json(&path).unwrap();
        assert!(result.is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_parse_codex_legacy_json_skips_empty_content() {
        let json_content = r#"
{
  "session": {"cwd": "/project"},
  "items": [
    {"role": "user", "content": "", "timestamp": 1705312800.0},
    {"role": "user", "content": "Real message", "timestamp": 1705312801.0}
  ]
}
"#;
        let path = std::env::temp_dir().join("test_codex_empty_content.json");
        std::fs::write(&path, json_content).unwrap();

        let conv = parse_codex_json(&path).unwrap().unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].content, "Real message");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_parse_codex_session_dispatches_by_extension() {
        // .jsonl extension → parse_codex_jsonl
        let content = r#"{"type":"event_msg","timestamp":1705312800.5,"payload":{"type":"user_message","message":"Hello"}}
"#;
        let path = std::env::temp_dir().join("test_dispatch.jsonl");
        std::fs::write(&path, content).unwrap();

        let result = parse_codex_session(&path).unwrap();
        assert!(result.is_some());

        let _ = std::fs::remove_file(&path);

        // Unknown extension → None
        let path2 = std::env::temp_dir().join("test_dispatch.txt");
        std::fs::write(&path2, content).unwrap();
        let result2 = parse_codex_session(&path2).unwrap();
        assert!(result2.is_none());

        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn test_codex_connector_scan_nonexistent() {
        let connector = CodexConnector {
            home_dir: Some(PathBuf::from("/nonexistent")),
        };
        let result = connector
            .scan(&[PathBuf::from("/totally/nonexistent")], None)
            .unwrap();
        assert!(result.is_empty());
    }
}
