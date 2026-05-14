use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::model::{Agent, Conversation, Role, SourceFile, parse_timestamp};

/// Action to take when a file fails to parse.
#[derive(Debug, Clone, Copy, Default)]
pub enum ErrorAction {
    #[default]
    Skip, // Log warning and continue (default)
    Fail,      // Stop entire scan immediately
    SkipAgent, // Skip remaining files for this agent
}

/// Connector trait for scanning agent session files.
pub trait Connector: Send + Sync {
    /// Agent this connector handles.
    fn agent(&self) -> Agent;

    /// Check if this agent's session files exist on this system.
    fn detect(&self) -> bool;

    /// Default root paths to scan.
    fn default_roots(&self) -> Vec<PathBuf>;

    /// Scan session files and return normalized conversations.
    /// `since_ts` is a best-effort incremental hint.
    fn scan(&self, roots: &[PathBuf], since_ts: Option<i64>) -> Result<Vec<Conversation>>;

    /// Error handling policy for this connector.
    fn on_parse_error(&self, _path: &Path, _error: &anyhow::Error) -> ErrorAction {
        ErrorAction::Skip
    }
}

/// Check if a file was modified since the given timestamp.
pub fn file_modified_since(path: &Path, since_ts: Option<i64>) -> bool {
    if since_ts.is_none() {
        return true;
    }
    let since = since_ts.unwrap();

    if let Ok(metadata) = std::fs::metadata(path) {
        if let Ok(mtime) = metadata.modified() {
            if let Ok(duration) = mtime.duration_since(std::time::UNIX_EPOCH) {
                let mtime_millis = duration.as_millis() as i64;
                return mtime_millis >= since;
            }
        }
    }
    // If we can't determine mtime, include the file
    true
}

/// Get source file info for a path.
pub fn source_file(path: &Path) -> Option<SourceFile> {
    if let Ok(metadata) = std::fs::metadata(path) {
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        Some(SourceFile {
            path: path.to_path_buf(),
            mtime,
            size: metadata.len(),
        })
    } else {
        None
    }
}

/// Flatten content from various JSON structures.
pub fn flatten_json_content(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| {
                if let Value::Object(obj) = v {
                    // Handle content blocks from various agents
                    obj.get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| {
                            obj.get("thinking")
                                .and_then(|t| t.as_str())
                                .map(|s| s.to_string())
                        })
                        .or_else(|| {
                            // Handle nested content
                            if let Some(content) = obj.get("content") {
                                Some(flatten_json_content(content))
                            } else {
                                None
                            }
                        })
                } else if let Value::String(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(obj) => obj
            .get("text")
            .or_else(|| obj.get("content"))
            .or_else(|| obj.get("message"))
            .map(flatten_json_content)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Extract role from a string value.
pub fn parse_role(s: &str) -> Option<Role> {
    match s.to_lowercase().as_str() {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        "tool" | "toolresult" | "tool_result" => Some(Role::Tool),
        "system" => Some(Role::System),
        _ => None,
    }
}

/// Get the home directory path.
pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// Return all available connectors.
pub fn all_connectors() -> Vec<Box<dyn Connector>> {
    vec![
        Box::new(crate::connectors::claude_code::ClaudeCodeConnector::new()),
        Box::new(crate::connectors::codex::CodexConnector::new()),
        Box::new(crate::connectors::opencode::OpenCodeConnector::new()),
        Box::new(crate::connectors::pi_agent::PiAgentConnector::new()),
    ]
}

pub mod claude_code;
pub mod codex;
pub mod opencode;
pub mod pi_agent;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_flatten_json_content_string() {
        let val = Value::String("hello".to_string());
        assert_eq!(flatten_json_content(&val), "hello");
    }

    #[test]
    fn test_flatten_json_content_array() {
        let val = serde_json::json!([
            {"type": "text", "text": "Hello"},
            {"type": "thinking", "thinking": "Thinking..."}
        ]);
        let result = flatten_json_content(&val);
        assert!(result.contains("Hello"));
        assert!(result.contains("Thinking..."));
    }

    #[test]
    fn test_flatten_json_content_nested() {
        let val = serde_json::json!([
            {"content": [{"type": "text", "text": "Nested"}]}
        ]);
        let result = flatten_json_content(&val);
        assert!(result.contains("Nested"));
    }

    #[test]
    fn test_flatten_json_content_plain_strings_in_array() {
        let val = serde_json::json!(["Hello", "World"]);
        let result = flatten_json_content(&val);
        assert!(result.contains("Hello"));
        assert!(result.contains("World"));
    }

    #[test]
    fn test_flatten_json_content_object_text() {
        let val = serde_json::json!({"text": "from object"});
        assert_eq!(flatten_json_content(&val), "from object");
    }

    #[test]
    fn test_flatten_json_content_object_message() {
        let val = serde_json::json!({"message": "from message field"});
        assert_eq!(flatten_json_content(&val), "from message field");
    }

    #[test]
    fn test_flatten_json_content_empty() {
        assert_eq!(flatten_json_content(&serde_json::json!(null)), "");
        assert_eq!(flatten_json_content(&serde_json::json!(42)), "");
        assert_eq!(flatten_json_content(&serde_json::json!(true)), "");
    }

    // ── parse_role ─────────────────────────────────────────

    #[test]
    fn test_parse_role_variants() {
        assert_eq!(parse_role("user"), Some(Role::User));
        assert_eq!(parse_role("assistant"), Some(Role::Assistant));
        assert_eq!(parse_role("tool"), Some(Role::Tool));
        assert_eq!(parse_role("toolresult"), Some(Role::Tool));
        assert_eq!(parse_role("tool_result"), Some(Role::Tool));
        assert_eq!(parse_role("system"), Some(Role::System));
    }

    #[test]
    fn test_parse_role_case_insensitive() {
        assert_eq!(parse_role("USER"), Some(Role::User));
        assert_eq!(parse_role("Assistant"), Some(Role::Assistant));
    }

    #[test]
    fn test_parse_role_unknown() {
        assert_eq!(parse_role("moderator"), None);
        assert_eq!(parse_role(""), None);
    }

    // ── file_modified_since ────────────────────────────────

    #[test]
    fn test_file_modified_since_no_filter() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();
        // since_ts = None means always include
        assert!(file_modified_since(&path, None));
    }

    #[test]
    fn test_file_modified_since_recent_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();
        // A very old since_ts should include a recently created file
        assert!(file_modified_since(&path, Some(0)));
    }

    #[test]
    fn test_file_modified_since_future() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();
        // A future timestamp should exclude the file
        let future_ts = chrono::Utc::now().timestamp_millis() + 100_000;
        assert!(!file_modified_since(&path, Some(future_ts)));
    }

    #[test]
    fn test_file_modified_since_nonexistent() {
        // Non-existent file should default to include
        assert!(file_modified_since(
            std::path::Path::new("/nonexistent/file"),
            None
        ));
    }

    // ── source_file ────────────────────────────────────────

    #[test]
    fn test_source_file_valid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let sf = source_file(&path).unwrap();
        assert_eq!(sf.path, path);
        assert_eq!(sf.size, 11);
        assert!(sf.mtime > 0);
    }

    #[test]
    fn test_source_file_nonexistent() {
        let sf = source_file(std::path::Path::new("/nonexistent/file"));
        assert!(sf.is_none());
    }

    // ── all_connectors ─────────────────────────────────────

    #[test]
    fn test_all_connectors_returns_four() {
        let connectors = all_connectors();
        assert_eq!(connectors.len(), 4);

        let agents: Vec<Agent> = connectors.iter().map(|c| c.agent()).collect();
        assert!(agents.contains(&Agent::ClaudeCode));
        assert!(agents.contains(&Agent::Codex));
        assert!(agents.contains(&Agent::OpenCode));
        assert!(agents.contains(&Agent::PiAgent));
    }
}
