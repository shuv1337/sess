use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{file_modified_since, flatten_json_content, parse_role, source_file, Connector};
use crate::model::{Agent, Conversation, Message, Role, SourceFile, source_fingerprint};

pub struct CodexConnector {
    home_dir: Option<PathBuf>,
}

impl CodexConnector {
    pub fn new() -> Self {
        Self {
            home_dir: dirs::home_dir(),
        }
    }

    fn sessions_dir(&self) -> Option<PathBuf> {
        // Check CODEX_HOME env var first
        if let Ok(codex_home) = std::env::var("CODEX_HOME") {
            return Some(PathBuf::from(codex_home).join("sessions"));
        }
        self.home_dir.as_ref().map(|h| h.join(".codex"))
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
        self.sessions_dir()
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    fn default_roots(&self) -> Vec<PathBuf> {
        self.sessions_dir().into_iter().collect()
    }

    fn scan(&self, roots: &[PathBuf], since_ts: Option<i64>) -> Result<Vec<Conversation>> {
        let mut conversations = Vec::new();

        for root in roots {
            if !root.exists() {
                continue;
            }

            // Find both JSONL and JSON files
            for entry in WalkDir::new(root)
                .follow_links(true)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    if !e.file_type().is_file() {
                        return false;
                    }
                    let name = e.file_name().to_string_lossy();
                    name.starts_with("rollout-") && 
                        (name.ends_with(".jsonl") || name.ends_with(".json"))
                })
            {
                let path = entry.path();

                // Check if file was modified since the given timestamp
                if !file_modified_since(path, since_ts) {
                    continue;
                }

                match parse_codex_session(path) {
                    Ok(Some(conv)) => {
                        conversations.push(conv);
                    }
                    Ok(None) => {
                        // Empty or no messages, skip
                    }
                    Err(e) => {
                        let action = self.on_parse_error(path, &e);
                        match action {
                            crate::connectors::ErrorAction::Skip => {
                                tracing::warn!("Failed to parse {}: {}", path.display(), e);
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
        }

        Ok(conversations)
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
    let file = File::open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);

    let source_file = source_file(path)
        .with_context(|| format!("Failed to get source file info for {}", path.display()))?;

    let mut messages: Vec<Message> = Vec::new();
    let mut workspace: Option<PathBuf> = None;
    let mut timestamps: Vec<i64> = Vec::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("Failed to read line {} from {}", line_num + 1, path.display()))?;

        if line.trim().is_empty() {
            continue;
        }

        let value: Value = serde_json::from_str(&line)
            .with_context(|| format!("Failed to parse JSON on line {} of {}", line_num + 1, path.display()))?;

        let entry_type = value.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("session_meta") => {
                // Extract workspace from session metadata
                if let Some(cwd) = value.get("payload").and_then(|p| p.get("cwd")).and_then(|v| v.as_str()) {
                    workspace = Some(PathBuf::from(cwd));
                }
            }
            Some("response_item") => {
                // Extract message from response_item
                if let Some(payload) = value.get("payload") {
                    let role = payload
                        .get("role")
                        .and_then(|v| v.as_str())
                        .and_then(parse_role)
                        .unwrap_or(Role::User);

                    let content = if let Some(content) = payload.get("content") {
                        flatten_json_content(content)
                    } else {
                        String::new()
                    };

                    if !content.trim().is_empty() {
                        // Try to get timestamp from the payload or outer value
                        let timestamp = value.get("timestamp")
                            .and_then(|v| v.as_f64())
                            .map(|ts| (ts * 1000.0) as i64)
                            .or_else(|| {
                                payload.get("timestamp")
                                    .and_then(|v| v.as_f64())
                                    .map(|ts| (ts * 1000.0) as i64)
                            });

                        if let Some(ts) = timestamp {
                            timestamps.push(ts);
                        }

                        messages.push(Message {
                            idx: messages.len(),
                            role,
                            content,
                            timestamp,
                            model: None,
                        });
                    }
                }
            }
            Some("event_msg") => {
                // Handle event messages
                if let Some(payload) = value.get("payload") {
                    let event_type = payload.get("type").and_then(|v| v.as_str());

                    match event_type {
                        Some("user_message") => {
                            if let Some(message) = payload.get("message").and_then(|v| v.as_str()) {
                                let timestamp = value.get("timestamp")
                                    .and_then(|v| v.as_f64())
                                    .map(|ts| (ts * 1000.0) as i64);

                                if let Some(ts) = timestamp {
                                    timestamps.push(ts);
                                }

                                messages.push(Message {
                                    idx: messages.len(),
                                    role: Role::User,
                                    content: message.to_string(),
                                    timestamp,
                                    model: None,
                                });
                            }
                        }
                        Some("agent_reasoning") => {
                            if let Some(text) = payload.get("text").and_then(|v| v.as_str()) {
                                let timestamp = value.get("timestamp")
                                    .and_then(|v| v.as_f64())
                                    .map(|ts| (ts * 1000.0) as i64);

                                if let Some(ts) = timestamp {
                                    timestamps.push(ts);
                                }

                                messages.push(Message {
                                    idx: messages.len(),
                                    role: Role::Assistant,
                                    content: text.to_string(),
                                    timestamp,
                                    model: None,
                                });
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

    let title = messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| {
            let first_line = m.content.lines().next().unwrap_or(&m.content);
            crate::model::truncate_title(first_line, 100)
        });

    let started_at = timestamps.iter().min().copied();
    let ended_at = timestamps.iter().max().copied();

    let source_files = vec![source_file];
    let fingerprint = source_fingerprint(&source_files);

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
    let mut file = File::open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
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
            let content = item.content.as_ref()
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

    let title = messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| {
            let first_line = m.content.lines().next().unwrap_or(&m.content);
            crate::model::truncate_title(first_line, 100)
        });

    let started_at = timestamps.iter().min().copied();
    let ended_at = timestamps.iter().max().copied();

    let source_files = vec![source_file];
    let fingerprint = source_fingerprint(&source_files);

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
        assert_eq!(conv.workspace, Some(PathBuf::from("/home/user/codex-project")));
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
        temp_file.as_file_mut().set_len(json_content.len() as u64).unwrap();
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
        let result = connector.scan(&[PathBuf::from("/totally/nonexistent")], None).unwrap();
        assert!(result.is_empty());
    }
}
