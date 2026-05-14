use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, file_modified_since, flatten_json_content, parse_role, source_file,
};
use crate::model::{Agent, Conversation, Message, Role, SourceFile, source_fingerprint};

pub struct ClaudeCodeConnector {
    home_dir: Option<PathBuf>,
}

impl ClaudeCodeConnector {
    pub fn new() -> Self {
        Self {
            home_dir: dirs::home_dir(),
        }
    }

    fn projects_dir(&self) -> Option<PathBuf> {
        self.home_dir
            .as_ref()
            .map(|h| h.join(".claude").join("projects"))
    }
}

impl Default for ClaudeCodeConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl Connector for ClaudeCodeConnector {
    fn agent(&self) -> Agent {
        Agent::ClaudeCode
    }

    fn detect(&self) -> bool {
        self.projects_dir().map(|p| p.exists()).unwrap_or(false)
    }

    fn default_roots(&self) -> Vec<PathBuf> {
        self.projects_dir().into_iter().collect()
    }

    fn scan(&self, roots: &[PathBuf], since_ts: Option<i64>) -> Result<Vec<Conversation>> {
        let mut conversations = Vec::new();

        for root in roots {
            if !root.exists() {
                continue;
            }

            for entry in WalkDir::new(root)
                .follow_links(true)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().is_file()
                        && e.path().extension().map(|e| e == "jsonl").unwrap_or(false)
                })
            {
                let path = entry.path();

                // Check if file was modified since the given timestamp
                if !file_modified_since(path, since_ts) {
                    continue;
                }

                match parse_claude_session(path) {
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
                                tracing::warn!("Skipping remaining Claude Code files due to error");
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

fn parse_claude_session(path: &Path) -> Result<Option<Conversation>> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);

    let source_file = source_file(path)
        .with_context(|| format!("Failed to get source file info for {}", path.display()))?;

    let mut messages: Vec<Message> = Vec::new();
    let mut workspace: Option<PathBuf> = None;
    let mut external_id: Option<String> = None;
    let mut timestamps: Vec<i64> = Vec::new();
    let mut current_model: Option<String> = None;

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

        // Skip non-message types
        let entry_type = value.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("file-history-snapshot") | Some("summary") | Some("files-read") => {
                // Skip these types
                continue;
            }
            Some("user") | Some("assistant") => {
                // Process message
            }
            _ => {
                // Try to detect if it's a message by looking for message field
                if value.get("message").is_none() {
                    continue;
                }
            }
        }

        // Extract session info from first user message
        if workspace.is_none() {
            if let Some(cwd) = value.get("cwd").and_then(|v| v.as_str()) {
                workspace = Some(PathBuf::from(cwd));
            }
        }

        if external_id.is_none() {
            if let Some(session_id) = value.get("sessionId").and_then(|v| v.as_str()) {
                external_id = Some(session_id.to_string());
            }
        }

        // Extract timestamp
        if let Some(ts) = value.get("timestamp").and_then(|v| {
            if let Some(s) = v.as_str() {
                // Try ISO 8601
                chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|dt| dt.timestamp_millis())
            } else {
                v.as_i64()
            }
        }) {
            timestamps.push(ts);
        }

        // Extract message
        if let Some(message) = value.get("message") {
            let role = message
                .get("role")
                .and_then(|v| v.as_str())
                .and_then(parse_role)
                .or_else(|| {
                    // Fallback to entry type
                    entry_type.and_then(parse_role)
                })
                .unwrap_or(Role::User);

            // Extract content
            let content = if let Some(content) = message.get("content") {
                flatten_json_content(content)
            } else {
                String::new()
            };

            // Extract model for assistant messages
            if role == Role::Assistant {
                if let Some(model) = message.get("model").and_then(|v| v.as_str()) {
                    current_model = Some(model.to_string());
                }
            }

            // Skip empty content
            if content.trim().is_empty() {
                continue;
            }

            messages.push(Message {
                idx: messages.len(),
                role,
                content,
                timestamp: timestamps.last().copied(),
                model: current_model.clone(),
            });
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    // Sort messages by index (they should already be in order)
    messages.sort_by_key(|m| m.idx);

    // Derive title from first user message
    let title = messages.iter().find(|m| m.role == Role::User).map(|m| {
        let first_line = m.content.lines().next().unwrap_or(&m.content);
        crate::model::truncate_title(first_line, 100)
    });

    let started_at = timestamps.iter().min().copied();
    let ended_at = timestamps.iter().max().copied();

    let source_files = vec![source_file];
    let fingerprint = source_fingerprint(&source_files);

    Ok(Some(Conversation {
        agent: Agent::ClaudeCode,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    #[test]
    fn test_parse_claude_session() {
        let jsonl_content = r#"
{"type":"file-history-snapshot","files":[]}
{"type":"user","sessionId":"test-session","cwd":"/home/user/project","message":{"role":"user","content":"Hello, can you help me?"},"timestamp":"2024-01-15T10:00:00Z"}
{"type":"assistant","message":{"role":"assistant","content":"Sure! I'd be happy to help.","model":"claude-opus-4"},"timestamp":"2024-01-15T10:00:05Z"}
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(jsonl_content.as_bytes()).unwrap();

        let result = parse_claude_session(temp_file.path()).unwrap();
        assert!(result.is_some());

        let conv = result.unwrap();
        assert_eq!(conv.agent, Agent::ClaudeCode);
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.workspace, Some(PathBuf::from("/home/user/project")));
        assert_eq!(conv.external_id, Some("test-session".to_string()));
        assert!(conv.started_at.is_some());
        assert!(conv.ended_at.is_some());
        assert!(conv.started_at.unwrap() <= conv.ended_at.unwrap());
    }

    #[test]
    fn test_parse_claude_empty_file() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(b"").unwrap();

        let result = parse_claude_session(temp_file.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_claude_only_snapshots() {
        let content = r#"{"type":"file-history-snapshot","files":[]}
{"type":"summary","content":"some summary"}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let result = parse_claude_session(temp_file.path()).unwrap();
        assert!(result.is_none()); // No user/assistant messages
    }

    #[test]
    fn test_parse_claude_content_array() {
        let content = r#"{"type":"user","cwd":"/test","message":{"role":"user","content":[{"type":"text","text":"Hello from array"}]},"timestamp":"2024-01-15T10:00:00Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Deep thought"},{"type":"text","text":"Response"}]},"timestamp":"2024-01-15T10:00:05Z"}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conv = parse_claude_session(temp_file.path()).unwrap().unwrap();
        assert_eq!(conv.messages.len(), 2);
        assert!(conv.messages[0].content.contains("Hello from array"));
        assert!(conv.messages[1].content.contains("Response"));
    }

    #[test]
    fn test_parse_claude_skips_empty_content() {
        let content = r#"{"type":"user","cwd":"/test","message":{"role":"user","content":""},"timestamp":"2024-01-15T10:00:00Z"}
{"type":"user","cwd":"/test","message":{"role":"user","content":"  "},"timestamp":"2024-01-15T10:00:01Z"}
{"type":"user","cwd":"/test","message":{"role":"user","content":"Actual message"},"timestamp":"2024-01-15T10:00:02Z"}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conv = parse_claude_session(temp_file.path()).unwrap().unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].content, "Actual message");
    }

    #[test]
    fn test_parse_claude_derives_title() {
        let content = r#"{"type":"user","cwd":"/test","message":{"role":"user","content":"Fix the login bug\nMore details here"},"timestamp":"2024-01-15T10:00:00Z"}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conv = parse_claude_session(temp_file.path()).unwrap().unwrap();
        assert_eq!(conv.title.as_deref(), Some("Fix the login bug"));
    }

    #[test]
    fn test_parse_claude_source_fingerprint() {
        let content = r#"{"type":"user","cwd":"/test","message":{"role":"user","content":"Hello"},"timestamp":"2024-01-15T10:00:00Z"}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conv = parse_claude_session(temp_file.path()).unwrap().unwrap();
        assert!(!conv.source_fingerprint.is_empty());
        assert_eq!(conv.source_files.len(), 1);
        assert_eq!(conv.source_files[0].path, temp_file.path());
    }

    #[test]
    fn test_claude_connector_scan() {
        let dir = TempDir::new().unwrap();
        let session_file = dir.path().join("session.jsonl");
        std::fs::write(&session_file, r#"{"type":"user","cwd":"/test","message":{"role":"user","content":"Hello"},"timestamp":"2024-01-15T10:00:00Z"}
"#).unwrap();

        let connector = ClaudeCodeConnector {
            home_dir: Some(PathBuf::from("/nonexistent")), // Don't use real home
        };

        let conversations = connector.scan(&[dir.path().to_path_buf()], None).unwrap();
        assert_eq!(conversations.len(), 1);
    }

    #[test]
    fn test_claude_connector_scan_nonexistent_root() {
        let connector = ClaudeCodeConnector {
            home_dir: Some(PathBuf::from("/nonexistent")),
        };
        let conversations = connector
            .scan(&[PathBuf::from("/totally/nonexistent")], None)
            .unwrap();
        assert!(conversations.is_empty());
    }

    #[test]
    fn test_claude_connector_scan_with_since() {
        let dir = TempDir::new().unwrap();
        let session_file = dir.path().join("session.jsonl");
        std::fs::write(&session_file, r#"{"type":"user","cwd":"/test","message":{"role":"user","content":"Hello"},"timestamp":"2024-01-15T10:00:00Z"}
"#).unwrap();

        let connector = ClaudeCodeConnector {
            home_dir: Some(PathBuf::from("/nonexistent")),
        };

        // Very far future timestamp should return nothing
        let future_ts = chrono::Utc::now().timestamp_millis() + 1_000_000;
        let conversations = connector
            .scan(&[dir.path().to_path_buf()], Some(future_ts))
            .unwrap();
        assert!(conversations.is_empty());
    }
}
