use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{file_modified_since, parse_role, source_file, Connector};
use crate::model::{Agent, Conversation, Message, Role, source_fingerprint};

pub struct PiAgentConnector {
    home_dir: Option<PathBuf>,
}

impl PiAgentConnector {
    pub fn new() -> Self {
        Self {
            home_dir: dirs::home_dir(),
        }
    }

    fn sessions_dir(&self) -> Option<PathBuf> {
        // Check PI_CODING_AGENT_DIR env var first
        if let Ok(pi_dir) = std::env::var("PI_CODING_AGENT_DIR") {
            return Some(PathBuf::from(pi_dir).join("sessions"));
        }
        self.home_dir
            .as_ref()
            .map(|h| h.join(".pi").join("agent").join("sessions"))
    }

    fn pi_dir(&self) -> Option<PathBuf> {
        if let Ok(pi_dir) = std::env::var("PI_CODING_AGENT_DIR") {
            return Some(PathBuf::from(pi_dir));
        }
        self.home_dir.as_ref().map(|h| h.join(".pi").join("agent"))
    }

    fn shiv_dir(&self) -> Option<PathBuf> {
        if let Ok(shiv_dir) = std::env::var("SHIV_AGENT_DIR") {
            return Some(PathBuf::from(shiv_dir));
        }
        self.home_dir
            .as_ref()
            .map(|h| h.join(".local").join("share").join("shiv"))
    }
}

impl Default for PiAgentConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl Connector for PiAgentConnector {
    fn agent(&self) -> Agent {
        Agent::PiAgent
    }

    fn detect(&self) -> bool {
        self.sessions_dir().map(|p| p.exists()).unwrap_or(false)
            || self
                .shiv_dir()
                .map(|p| p.join("sessions").exists())
                .unwrap_or(false)
    }

    fn default_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(pi) = self.pi_dir() {
            roots.push(pi);
        }
        if let Some(shiv) = self.shiv_dir() {
            if !roots.contains(&shiv) {
                roots.push(shiv);
            }
        }
        roots
    }

    fn scan(&self, roots: &[PathBuf], since_ts: Option<i64>) -> Result<Vec<Conversation>> {
        let mut conversations = Vec::new();

        for root in roots {
            let sessions_root = root.join("sessions");
            if !sessions_root.exists() {
                continue;
            }

            // Walk session directories
            for entry in WalkDir::new(&sessions_root)
                .follow_links(true)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().is_file()
                        && e.path().extension().map(|ext| ext == "jsonl").unwrap_or(false)
                })
            {
                let path = entry.path();
                let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

                // Supported session naming conventions:
                // - pi-agent: {timestamp}_{uuid}.jsonl
                // - shiv archive: session-{timestamp}.jsonl
                if !(file_name.contains('_') || file_name.starts_with("session-")) {
                    continue;
                }

                // Check if file was modified since the given timestamp
                if !file_modified_since(path, since_ts) {
                    continue;
                }

                match parse_pi_session(path) {
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
                                tracing::warn!("Skipping remaining Pi Agent files due to error");
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

fn parse_pi_session(path: &Path) -> Result<Option<Conversation>> {
    let file = File::open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
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

        let entry_type = value.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("session") => {
                // Extract session info
                if let Some(cwd) = value.get("cwd").and_then(|v| v.as_str()) {
                    workspace = Some(PathBuf::from(cwd));
                }
                if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
                    external_id = Some(id.to_string());
                }
                if let Some(model) = value.get("modelId").and_then(|v| v.as_str()) {
                    current_model = Some(model.to_string());
                }
            }
            Some("model_change") => {
                // Update current model
                if let Some(model) = value.get("modelId").and_then(|v| v.as_str()) {
                    current_model = Some(model.to_string());
                }
            }
            Some("message") => {
                // Extract message
                if let Some(msg) = value.get("message") {
                    let role = msg
                        .get("role")
                        .and_then(|v| v.as_str())
                        .and_then(parse_role)
                        .unwrap_or(Role::User);

                    // Extract timestamp from outer value
                    let timestamp = value
                        .get("timestamp")
                        .and_then(|v| {
                            if let Some(s) = v.as_str() {
                                chrono::DateTime::parse_from_rfc3339(s)
                                    .ok()
                                    .map(|dt| dt.timestamp_millis())
                            } else {
                                v.as_i64()
                            }
                        });

                    if let Some(ts) = timestamp {
                        timestamps.push(ts);
                    }

                    // Extract content - handle array of content blocks
                    let content = if let Some(content) = msg.get("content") {
                        extract_pi_content(content)
                    } else {
                        String::new()
                    };

                    if !content.trim().is_empty() {
                        messages.push(Message {
                            idx: messages.len(),
                            role,
                            content,
                            timestamp,
                            model: current_model.clone(),
                        });
                    }
                }
            }
            Some("thinking_level_change") => {
                // Skip this type
            }
            _ => {
                // Unknown type, skip
            }
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    // If workspace is not set from session.cwd, try to decode from directory name
    let workspace = workspace.or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .and_then(decode_safe_path)
    });

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

    // Extract session ID from filename if not found in JSON
    let external_id = external_id.or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
    });

    Ok(Some(Conversation {
        agent: Agent::PiAgent,
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

/// Extract content from Pi-Agent message format.
fn extract_pi_content(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|block| {
                if let Value::Object(obj) = block {
                    let block_type = obj.get("type").and_then(|v| v.as_str());
                    match block_type {
                        Some("text") => obj.get("text").and_then(|v| v.as_str()).map(|s| s.to_string()),
                        Some("thinking") => {
                            obj.get("thinking").and_then(|v| v.as_str()).map(|s| format!("[Thinking] {}", s))
                        }
                        Some("toolCall") => {
                            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                            let args = obj
                                .get("arguments")
                                .map(|v| v.to_string())
                                .unwrap_or_default();
                            Some(format!("[Tool: {}] {}", name, args))
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Decode a safe path directory name to a workspace path.
fn decode_safe_path(safe_path: &str) -> Option<PathBuf> {
    // Try legacy URL-safe base64 without padding
    if let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(safe_path) {
        if let Ok(path) = String::from_utf8(bytes) {
            return Some(PathBuf::from(path));
        }
    }

    // Conservative modern fallback (best effort):
    // The slug layout is lossy for paths containing '-' so only accept a decoded
    // approximation if that path actually exists on disk.
    let trimmed = safe_path.strip_prefix("--")?.strip_suffix("--")?;
    let approx = PathBuf::from(format!("/ {}", trimmed.replace('-', "/")));
    if approx.exists() {
        Some(approx)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    #[test]
    fn test_parse_pi_session() {
        let jsonl_content = r#"
{"type":"session","id":"test-session","cwd":"/home/user/pi-project","provider":"anthropic","modelId":"claude-3-sonnet"}
{"type":"message","timestamp":"2024-01-15T10:00:00Z","message":{"role":"user","content":[{"type":"text","text":"Hello, can you help me?"}]}}
{"type":"message","timestamp":"2024-01-15T10:00:05Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me think..."},{"type":"text","text":"Sure! I'd be happy to help."}]}}
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(jsonl_content.as_bytes()).unwrap();

        let result = parse_pi_session(temp_file.path()).unwrap();
        assert!(result.is_some());

        let conv = result.unwrap();
        assert_eq!(conv.agent, Agent::PiAgent);
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.workspace, Some(PathBuf::from("/home/user/pi-project")));
        assert_eq!(conv.external_id, Some("test-session".to_string()));
        assert!(conv.started_at.is_some());
        assert!(conv.ended_at.is_some());
    }

    #[test]
    fn test_parse_pi_session_model_change() {
        let content = r#"{"type":"session","id":"s1","cwd":"/test","modelId":"model-a"}
{"type":"message","timestamp":"2024-01-15T10:00:00Z","message":{"role":"user","content":"First"}}
{"type":"model_change","modelId":"model-b"}
{"type":"message","timestamp":"2024-01-15T10:00:05Z","message":{"role":"user","content":"Second"}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conv = parse_pi_session(temp_file.path()).unwrap().unwrap();
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].model.as_deref(), Some("model-a"));
        assert_eq!(conv.messages[1].model.as_deref(), Some("model-b"));
    }

    #[test]
    fn test_parse_pi_session_empty() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(b"").unwrap();

        let result = parse_pi_session(temp_file.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_pi_session_only_session_entry() {
        let content = r#"{"type":"session","id":"s1","cwd":"/test","modelId":"model-a"}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let result = parse_pi_session(temp_file.path()).unwrap();
        assert!(result.is_none()); // No messages
    }

    #[test]
    fn test_parse_pi_session_skips_thinking_level_change() {
        let content = r#"{"type":"session","id":"s1","cwd":"/test","modelId":"m1"}
{"type":"thinking_level_change","level":"high"}
{"type":"message","timestamp":"2024-01-15T10:00:00Z","message":{"role":"user","content":"Hello"}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conv = parse_pi_session(temp_file.path()).unwrap().unwrap();
        assert_eq!(conv.messages.len(), 1);
    }

    #[test]
    fn test_parse_pi_session_string_content() {
        let content = r#"{"type":"session","id":"s1","cwd":"/test","modelId":"m1"}
{"type":"message","timestamp":"2024-01-15T10:00:00Z","message":{"role":"user","content":"Plain string content"}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conv = parse_pi_session(temp_file.path()).unwrap().unwrap();
        assert_eq!(conv.messages[0].content, "Plain string content");
    }

    #[test]
    fn test_parse_pi_session_external_id_fallback() {
        // Without explicit session ID, fallback to filename
        let content = r#"{"type":"message","timestamp":"2024-01-15T10:00:00Z","message":{"role":"user","content":"Hello"}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conv = parse_pi_session(temp_file.path()).unwrap().unwrap();
        assert!(conv.external_id.is_some()); // Derived from filename
    }

    #[test]
    fn test_extract_pi_content_text() {
        let content = serde_json::json!([{"type": "text", "text": "Hello"}]);
        assert_eq!(extract_pi_content(&content), "Hello");
    }

    #[test]
    fn test_extract_pi_content_thinking() {
        let content = serde_json::json!([{"type": "thinking", "thinking": "Thinking..."}]);
        assert!(extract_pi_content(&content).contains("Thinking"));
        assert!(extract_pi_content(&content).contains("[Thinking]"));
    }

    #[test]
    fn test_extract_pi_content_tool_call() {
        let content = serde_json::json!([{"type": "toolCall", "name": "read_file", "arguments": {"path": "/tmp/test"}}]);
        let result = extract_pi_content(&content);
        assert!(result.contains("read_file"));
        assert!(result.contains("[Tool:"));
    }

    #[test]
    fn test_extract_pi_content_mixed() {
        let content = serde_json::json!([
            {"type": "text", "text": "Hello"},
            {"type": "thinking", "thinking": "Deep thought"},
            {"type": "toolCall", "name": "bash", "arguments": {"command": "ls"}}
        ]);
        let result = extract_pi_content(&content);
        assert!(result.contains("Hello"));
        assert!(result.contains("Deep thought"));
        assert!(result.contains("bash"));
    }

    #[test]
    fn test_extract_pi_content_string() {
        let content = serde_json::json!("plain string");
        assert_eq!(extract_pi_content(&content), "plain string");
    }

    #[test]
    fn test_extract_pi_content_empty() {
        assert_eq!(extract_pi_content(&serde_json::json!(null)), "");
        assert_eq!(extract_pi_content(&serde_json::json!([])), "");
    }

    #[test]
    fn test_extract_pi_content_unknown_type() {
        let content = serde_json::json!([{"type": "unknown", "data": "stuff"}]);
        assert_eq!(extract_pi_content(&content), "");
    }

    // ── decode_safe_path ───────────────────────────────────

    #[test]
    fn test_decode_safe_path_base64() {
        // Test URL-safe base64 (legacy format)
        let path = "/home/user/project";
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(path);
        let decoded = decode_safe_path(&encoded);
        assert_eq!(decoded, Some(PathBuf::from(path)));
    }

    #[test]
    fn test_decode_safe_path_invalid() {
        // Not base64, not the slug format
        assert!(decode_safe_path("random-string").is_none());
    }

    #[test]
    fn test_decode_safe_path_slug_nonexistent() {
        // Slug format but path doesn't exist
        let result = decode_safe_path("--nonexistent-path-that-doesnt-exist--");
        assert!(result.is_none());
    }

    // ── Connector trait ────────────────────────────────────

    #[test]
    fn test_pi_connector_scan_with_naming_convention() {
        let dir = TempDir::new().unwrap();
        let sessions_dir = dir.path().join("sessions").join("--test--");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // File with underscore (matches naming convention)
        let session_file = sessions_dir.join("12345_uuid.jsonl");
        std::fs::write(&session_file, r#"{"type":"session","id":"s1","cwd":"/test","modelId":"m1"}
{"type":"message","timestamp":"2024-01-15T10:00:00Z","message":{"role":"user","content":"Hello"}}
"#).unwrap();

        // Shiv archive-style file name should also be accepted
        let archive_file = sessions_dir.join("session-1770371965142.jsonl");
        std::fs::write(&archive_file, r#"{"type":"session","id":"s2","cwd":"/test","modelId":"m1"}
{"type":"message","timestamp":"2024-01-15T10:01:00Z","message":{"role":"assistant","content":"Archived hello"}}
"#).unwrap();

        // File without underscore or session- prefix should be skipped
        let other_file = sessions_dir.join("nounder.jsonl");
        std::fs::write(&other_file, r#"{"type":"message","timestamp":"2024-01-15T10:00:00Z","message":{"role":"user","content":"Skip me"}}
"#).unwrap();

        let connector = PiAgentConnector {
            home_dir: Some(PathBuf::from("/nonexistent")),
        };

        let conversations = connector.scan(&[dir.path().to_path_buf()], None).unwrap();
        assert_eq!(conversations.len(), 2);
    }

    #[test]
    fn test_pi_connector_default_roots_include_shiv() {
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join(".pi/agent/sessions")).unwrap();
        std::fs::create_dir_all(home.path().join(".local/share/shiv/sessions")).unwrap();

        let connector = PiAgentConnector {
            home_dir: Some(home.path().to_path_buf()),
        };

        let roots = connector.default_roots();
        assert!(roots.contains(&home.path().join(".pi/agent")));
        assert!(roots.contains(&home.path().join(".local/share/shiv")));
        assert!(connector.detect());
    }

    #[test]
    fn test_pi_connector_scan_nonexistent() {
        let connector = PiAgentConnector {
            home_dir: Some(PathBuf::from("/nonexistent")),
        };
        let result = connector.scan(&[PathBuf::from("/nonexistent/root")], None).unwrap();
        assert!(result.is_empty());
    }
}
