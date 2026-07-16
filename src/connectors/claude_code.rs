use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, ConnectorScan, file_modified_since, flatten_json_content, json_u64,
    normalized_token_total, parse_role, source_file,
};
use crate::model::{Agent, Conversation, Message, Role, UsageRecord, source_fingerprint};

const PARSER_REVISION: &str = "2";

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

    fn scan(&self, roots: &[PathBuf], since_ts: Option<i64>) -> Result<ConnectorScan> {
        let mut conversations = Vec::new();
        let mut complete = true;

        for root in roots {
            if !root.exists() {
                continue;
            }

            for entry in WalkDir::new(root).follow_links(true) {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        complete = false;
                        tracing::warn!(
                            agent = Agent::ClaudeCode.slug(),
                            root = %root.display(),
                            error = %error,
                            "Failed to traverse Claude Code session storage"
                        );
                        continue;
                    }
                };
                if !entry.file_type().is_file()
                    || entry
                        .path()
                        .extension()
                        .is_none_or(|extension| extension != "jsonl")
                {
                    continue;
                }
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
                        complete = false;
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
                                return Ok(ConnectorScan::new(conversations, false));
                            }
                        }
                    }
                }
            }
        }

        Ok(ConnectorScan::new(conversations, complete))
    }

    fn parser_revision(&self) -> Option<&'static str> {
        Some(PARSER_REVISION)
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
    let mut usage_by_message: HashMap<String, UsageRecord> = HashMap::new();

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
                if let Some(raw_usage) = message.get("usage") {
                    let input = json_u64(raw_usage, &["/input_tokens"]);
                    let output = json_u64(raw_usage, &["/output_tokens"]);
                    let cache_read = json_u64(raw_usage, &["/cache_read_input_tokens"]);
                    let cache_write = json_u64(raw_usage, &["/cache_creation_input_tokens"]);
                    let total = normalized_token_total(0, input, output, cache_read, cache_write);
                    let source_event_id = message
                        .get("id")
                        .and_then(Value::as_str)
                        .map(|id| format!("message:{id}"))
                        .or_else(|| {
                            value
                                .get("requestId")
                                .and_then(Value::as_str)
                                .map(|id| format!("request:{id}"))
                        });
                    let record = UsageRecord {
                        timestamp: timestamps.last().copied(),
                        provider: message
                            .get("provider")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        model: current_model.clone(),
                        source_event_id: source_event_id.clone(),
                        api_calls: 1,
                        input_tokens: input,
                        output_tokens: output,
                        cache_read_tokens: cache_read,
                        cache_write_tokens: cache_write,
                        reasoning_tokens: json_u64(raw_usage, &["/reasoning_tokens"]),
                        total_tokens: total,
                        actual_cost_usd: None,
                        estimated_cost_usd: None,
                    };
                    let synthetic_zero = record.total_tokens == 0
                        && record.reasoning_tokens == 0
                        && record.model.as_deref().is_some_and(|model| {
                            model
                                .trim_matches(&['<', '>'][..])
                                .eq_ignore_ascii_case("synthetic")
                        });
                    if record.has_usage() && !synthetic_zero {
                        let key = source_event_id.unwrap_or_else(|| format!("line-{line_num}"));
                        usage_by_message
                            .entry(key)
                            .and_modify(|existing| {
                                if (record.total_tokens, record.timestamp)
                                    > (existing.total_tokens, existing.timestamp)
                                {
                                    *existing = record.clone();
                                }
                            })
                            .or_insert(record);
                    }
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

    let mut usage: Vec<_> = usage_by_message.into_values().collect();
    if messages.is_empty() && usage.is_empty() {
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
    let fingerprint = format!(
        "claude-v{PARSER_REVISION}:{}",
        source_fingerprint(&source_files)
    );
    usage.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.provider.cmp(&right.provider))
            .then_with(|| left.model.cmp(&right.model))
            .then_with(|| left.total_tokens.cmp(&right.total_tokens))
            .then_with(|| left.input_tokens.cmp(&right.input_tokens))
            .then_with(|| left.output_tokens.cmp(&right.output_tokens))
    });

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
        usage,
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
    fn test_parse_claude_usage_deduplicates_response_rows() {
        let content = r#"{"type":"user","cwd":"/test","message":{"role":"user","content":"Hello"},"timestamp":"2024-01-15T10:00:00Z"}
{"type":"assistant","message":{"id":"msg-usage","role":"assistant","content":[{"type":"thinking","thinking":"hidden"}],"provider":"vertex","model":"claude-test","usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30,"cache_creation_input_tokens":40}},"timestamp":"2024-01-15T10:00:05Z"}
{"type":"assistant","message":{"id":"msg-usage","role":"assistant","content":[{"type":"text","text":"Done"}],"provider":"vertex","model":"claude-test","usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30,"cache_creation_input_tokens":40}},"timestamp":"2024-01-15T10:00:06Z"}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_claude_session(temp_file.path()).unwrap().unwrap();
        assert_eq!(conversation.usage.len(), 1);
        let usage = &conversation.usage[0];
        assert_eq!(usage.provider.as_deref(), Some("vertex"));
        assert_eq!(usage.model.as_deref(), Some("claude-test"));
        assert_eq!(usage.source_event_id.as_deref(), Some("message:msg-usage"));
        assert_eq!(usage.api_calls, 1);
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_read_tokens, 30);
        assert_eq!(usage.cache_write_tokens, 40);
        assert_eq!(usage.total_tokens, 100);
    }

    #[test]
    fn synthetic_zero_usage_is_not_counted_as_an_api_call() {
        let content = r#"{"type":"user","message":{"role":"user","content":"Hello"}}
{"type":"assistant","message":{"id":"synthetic","role":"assistant","content":"bookkeeping","model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_claude_session(temp_file.path()).unwrap().unwrap();
        assert!(conversation.usage.is_empty());
    }

    #[test]
    fn usage_only_session_is_preserved() {
        let content = r#"{"type":"assistant","message":{"id":"usage-only","role":"assistant","content":"","model":"claude-test","usage":{"input_tokens":10,"output_tokens":5}},"timestamp":"2024-01-15T10:00:05Z"}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_claude_session(temp_file.path()).unwrap().unwrap();
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.usage.len(), 1);
        assert_eq!(conversation.usage[0].total_tokens, 15);
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

    #[cfg(unix)]
    #[test]
    fn traversal_error_marks_claude_scan_incomplete() {
        let dir = TempDir::new().unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing"), dir.path().join("broken")).unwrap();
        let connector = ClaudeCodeConnector {
            home_dir: Some(PathBuf::from("/nonexistent")),
        };

        let scan = connector.scan(&[dir.path().to_path_buf()], None).unwrap();
        assert!(!scan.complete);
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
