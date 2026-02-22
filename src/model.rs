use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Which agent produced this conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Agent {
    ClaudeCode,
    Codex,
    OpenCode,
    PiAgent,
}

impl Agent {
    pub fn slug(&self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude_code",
            Agent::Codex => "codex",
            Agent::OpenCode => "opencode",
            Agent::PiAgent => "pi_agent",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::Codex => "Codex",
            Agent::OpenCode => "OpenCode",
            Agent::PiAgent => "Pi Agent",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Agent::ClaudeCode => "●",
            Agent::Codex => "◆",
            Agent::OpenCode => "■",
            Agent::PiAgent => "▲",
        }
    }

    pub fn color_code(&self) -> (u8, u8, u8) {
        match self {
            Agent::ClaudeCode => (147, 112, 219), // Purple
            Agent::Codex => (50, 205, 50),        // Green
            Agent::OpenCode => (30, 144, 255),    // Blue
            Agent::PiAgent => (255, 165, 0),      // Orange
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

impl std::str::FromStr for Agent {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "claude_code" | "claude" | "claudecode" => Ok(Agent::ClaudeCode),
            "codex" => Ok(Agent::Codex),
            "opencode" | "open_code" => Ok(Agent::OpenCode),
            "pi_agent" | "piagent" | "pi" => Ok(Agent::PiAgent),
            _ => anyhow::bail!("Unknown agent: {}", s),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    Tool,
    System,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::System => "system",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for Role {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "user" => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
            "tool" | "toolresult" | "tool_result" => Ok(Role::Tool),
            "system" => Ok(Role::System),
            _ => anyhow::bail!("Unknown role: {}", s),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub idx: usize,
    pub role: Role,
    pub content: String,
    pub timestamp: Option<i64>, // Unix millis UTC
    pub model: Option<String>,  // e.g. "claude-opus-4-5", "gpt-5.1-codex"
}

impl Message {
    pub fn preview(&self, max_len: usize) -> String {
        let content = self.content.trim();
        if content.len() <= max_len {
            content.to_string()
        } else {
            format!("{}...", &content[..max_len])
        }
    }
}

/// One physical file that contributed to this normalized conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: PathBuf,
    pub mtime: i64, // Unix millis UTC
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub agent: Agent,
    pub external_id: Option<String>,
    pub title: Option<String>,
    pub workspace: Option<PathBuf>, // cwd the agent ran in
    pub source_path: PathBuf,       // canonical source key (session file for OpenCode)
    pub source_files: Vec<SourceFile>,
    pub source_fingerprint: String, // blake3 over sorted source_files tuples
    pub started_at: Option<i64>,    // Unix millis UTC
    pub ended_at: Option<i64>,
    pub messages: Vec<Message>,
}

impl Conversation {
    /// Get the first user message content, if any.
    pub fn first_user_message(&self) -> Option<&str> {
        self.messages
            .iter()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.as_str())
    }

    /// Derive a title from the first user message.
    pub fn derive_title(&self) -> String {
        if let Some(title) = &self.title {
            return truncate_title(title, 100);
        }

        if let Some(first) = self.first_user_message() {
            let first_line = first.lines().next().unwrap_or(first);
            return truncate_title(first_line, 100);
        }

        "Untitled".to_string()
    }

    /// Get the full text content of all messages for indexing.
    pub fn full_text(&self) -> String {
        self.messages
            .iter()
            .map(|m| format!("[{}] {}", m.role.as_str(), m.content))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Get a preview of the conversation (first 300 chars).
    pub fn preview(&self) -> String {
        let full = self.full_text();
        if full.len() <= 300 {
            full
        } else {
            // Find a valid UTF-8 boundary near 300 chars
            let mut idx = 300;
            while idx > 0 && !full.is_char_boundary(idx) {
                idx -= 1;
            }
            format!("{}...", &full[..idx])
        }
    }

    /// Calculate the max mtime across all source files.
    pub fn source_mtime_max(&self) -> i64 {
        self.source_files
            .iter()
            .map(|f| f.mtime)
            .max()
            .unwrap_or(0)
    }
}

/// Compute a stable fingerprint from source files.
pub fn source_fingerprint(files: &[SourceFile]) -> String {
    let mut items: Vec<_> = files
        .iter()
        .map(|f| {
            let path_str = f.path.to_string_lossy();
            format!("{}:{}:{}", path_str, f.mtime, f.size)
        })
        .collect();

    // Stable sort by path
    items.sort();

    let combined = items.join("|");
    blake3::hash(combined.as_bytes()).to_hex().to_string()
}

/// Truncate a string to max length, adding ellipsis if truncated.
pub fn truncate_title(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        s.to_string()
    } else {
        // Find a valid UTF-8 boundary
        let mut idx = max.saturating_sub(3);
        while idx > 0 && !s.is_char_boundary(idx) {
            idx -= 1;
        }
        format!("{}...", &s[..idx])
    }
}

/// Parse a timestamp value into Unix millis.
pub fn parse_timestamp(val: &serde_json::Value) -> Option<i64> {
    match val {
        serde_json::Value::Number(n) => {
            // Try as millis first, then seconds
            if let Some(millis) = n.as_i64() {
                if millis > 1_000_000_000_000 {
                    // Already millis
                    Some(millis)
                } else {
                    // Assume seconds, convert to millis
                    Some(millis * 1000)
                }
            } else if let Some(secs) = n.as_f64() {
                Some((secs * 1000.0) as i64)
            } else {
                None
            }
        }
        serde_json::Value::String(s) => {
            // Try ISO 8601 parsing
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                Some(dt.timestamp_millis())
            } else if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%:z") {
                Some(dt.and_utc().timestamp_millis())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Recursively extract text content from JSON values.
pub fn flatten_content(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| {
                if let serde_json::Value::Object(obj) = v {
                    // Handle content blocks
                    if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
                        Some(text.to_string())
                    } else if let Some(thinking) = obj.get("thinking").and_then(|t| t.as_str()) {
                        Some(thinking.to_string())
                    } else if let Some(prompt) = obj.get("prompt").and_then(|t| t.as_str()) {
                        Some(prompt.to_string())
                    } else {
                        None
                    }
                } else {
                    flatten_content(v).into()
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::Value::Object(obj) => {
            // Try common content fields
            obj.get("text")
                .or_else(|| obj.get("content"))
                .or_else(|| obj.get("message"))
                .map(flatten_content)
                .unwrap_or_default()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Agent ──────────────────────────────────────────────

    #[test]
    fn test_agent_slug() {
        assert_eq!(Agent::ClaudeCode.slug(), "claude_code");
        assert_eq!(Agent::Codex.slug(), "codex");
        assert_eq!(Agent::OpenCode.slug(), "opencode");
        assert_eq!(Agent::PiAgent.slug(), "pi_agent");
    }

    #[test]
    fn test_agent_display_name() {
        assert_eq!(Agent::ClaudeCode.display_name(), "Claude Code");
        assert_eq!(Agent::Codex.display_name(), "Codex");
        assert_eq!(Agent::OpenCode.display_name(), "OpenCode");
        assert_eq!(Agent::PiAgent.display_name(), "Pi Agent");
    }

    #[test]
    fn test_agent_display_trait() {
        assert_eq!(format!("{}", Agent::ClaudeCode), "Claude Code");
        assert_eq!(format!("{}", Agent::PiAgent), "Pi Agent");
    }

    #[test]
    fn test_agent_from_str() {
        assert_eq!("claude_code".parse::<Agent>().unwrap(), Agent::ClaudeCode);
        assert_eq!("claude".parse::<Agent>().unwrap(), Agent::ClaudeCode);
        assert_eq!("claudecode".parse::<Agent>().unwrap(), Agent::ClaudeCode);
        assert_eq!("codex".parse::<Agent>().unwrap(), Agent::Codex);
        assert_eq!("opencode".parse::<Agent>().unwrap(), Agent::OpenCode);
        assert_eq!("open_code".parse::<Agent>().unwrap(), Agent::OpenCode);
        assert_eq!("pi_agent".parse::<Agent>().unwrap(), Agent::PiAgent);
        assert_eq!("piagent".parse::<Agent>().unwrap(), Agent::PiAgent);
        assert_eq!("pi".parse::<Agent>().unwrap(), Agent::PiAgent);
        // Case insensitive
        assert_eq!("CLAUDE".parse::<Agent>().unwrap(), Agent::ClaudeCode);
        assert_eq!("Codex".parse::<Agent>().unwrap(), Agent::Codex);
        // Unknown agent
        assert!("unknown".parse::<Agent>().is_err());
    }

    #[test]
    fn test_agent_icon_and_color() {
        // Ensure each agent has a unique icon
        let icons: Vec<&str> = [Agent::ClaudeCode, Agent::Codex, Agent::OpenCode, Agent::PiAgent]
            .iter()
            .map(|a| a.icon())
            .collect();
        assert_eq!(icons.len(), 4);
        for i in 0..icons.len() {
            for j in (i+1)..icons.len() {
                assert_ne!(icons[i], icons[j]);
            }
        }

        // Color codes are valid RGB tuples
        for agent in [Agent::ClaudeCode, Agent::Codex, Agent::OpenCode, Agent::PiAgent] {
            let (r, g, b) = agent.color_code();
            assert!(r <= 255 && g <= 255 && b <= 255);
        }
    }

    // ── Role ───────────────────────────────────────────────

    #[test]
    fn test_role_as_str() {
        assert_eq!(Role::User.as_str(), "user");
        assert_eq!(Role::Assistant.as_str(), "assistant");
        assert_eq!(Role::Tool.as_str(), "tool");
        assert_eq!(Role::System.as_str(), "system");
    }

    #[test]
    fn test_role_display() {
        assert_eq!(format!("{}", Role::User), "user");
        assert_eq!(format!("{}", Role::Assistant), "assistant");
    }

    #[test]
    fn test_role_from_str() {
        assert_eq!("user".parse::<Role>().unwrap(), Role::User);
        assert_eq!("assistant".parse::<Role>().unwrap(), Role::Assistant);
        assert_eq!("tool".parse::<Role>().unwrap(), Role::Tool);
        assert_eq!("toolresult".parse::<Role>().unwrap(), Role::Tool);
        assert_eq!("tool_result".parse::<Role>().unwrap(), Role::Tool);
        assert_eq!("system".parse::<Role>().unwrap(), Role::System);
        // Case insensitive
        assert_eq!("USER".parse::<Role>().unwrap(), Role::User);
        // Unknown
        assert!("moderator".parse::<Role>().is_err());
    }

    // ── Message ────────────────────────────────────────────

    #[test]
    fn test_message_preview() {
        let msg = Message {
            idx: 0,
            role: Role::User,
            content: "Hello world, this is a test".to_string(),
            timestamp: None,
            model: None,
        };
        assert_eq!(msg.preview(100), "Hello world, this is a test");
        assert_eq!(msg.preview(10), "Hello worl...");
    }

    #[test]
    fn test_message_preview_whitespace() {
        let msg = Message {
            idx: 0,
            role: Role::User,
            content: "  trimmed  ".to_string(),
            timestamp: None,
            model: None,
        };
        assert_eq!(msg.preview(100), "trimmed");
    }

    // ── Conversation ───────────────────────────────────────

    fn make_conv(messages: Vec<Message>) -> Conversation {
        Conversation {
            agent: Agent::ClaudeCode,
            external_id: None,
            title: None,
            workspace: None,
            source_path: PathBuf::from("/test.jsonl"),
            source_files: vec![],
            source_fingerprint: "abc".to_string(),
            started_at: None,
            ended_at: None,
            messages,
        }
    }

    #[test]
    fn test_first_user_message() {
        let conv = make_conv(vec![
            Message { idx: 0, role: Role::System, content: "System msg".into(), timestamp: None, model: None },
            Message { idx: 1, role: Role::User, content: "Hello!".into(), timestamp: None, model: None },
            Message { idx: 2, role: Role::User, content: "Second".into(), timestamp: None, model: None },
        ]);
        assert_eq!(conv.first_user_message(), Some("Hello!"));
    }

    #[test]
    fn test_first_user_message_none() {
        let conv = make_conv(vec![
            Message { idx: 0, role: Role::Assistant, content: "Hi".into(), timestamp: None, model: None },
        ]);
        assert_eq!(conv.first_user_message(), None);
    }

    #[test]
    fn test_derive_title_from_title() {
        let mut conv = make_conv(vec![]);
        conv.title = Some("My Custom Title".to_string());
        assert_eq!(conv.derive_title(), "My Custom Title");
    }

    #[test]
    fn test_derive_title_from_first_user_message() {
        let conv = make_conv(vec![
            Message { idx: 0, role: Role::User, content: "Help me with auth\nMore details...".into(), timestamp: None, model: None },
        ]);
        assert_eq!(conv.derive_title(), "Help me with auth");
    }

    #[test]
    fn test_derive_title_untitled() {
        let conv = make_conv(vec![]);
        assert_eq!(conv.derive_title(), "Untitled");
    }

    #[test]
    fn test_derive_title_long_truncation() {
        let long_title = "a".repeat(200);
        let mut conv = make_conv(vec![]);
        conv.title = Some(long_title);
        let derived = conv.derive_title();
        assert!(derived.len() <= 103); // 100 + "..."
        assert!(derived.ends_with("..."));
    }

    #[test]
    fn test_full_text() {
        let conv = make_conv(vec![
            Message { idx: 0, role: Role::User, content: "Hello".into(), timestamp: None, model: None },
            Message { idx: 1, role: Role::Assistant, content: "World".into(), timestamp: None, model: None },
        ]);
        let text = conv.full_text();
        assert!(text.contains("[user] Hello"));
        assert!(text.contains("[assistant] World"));
    }

    #[test]
    fn test_preview_short() {
        let conv = make_conv(vec![
            Message { idx: 0, role: Role::User, content: "Short".into(), timestamp: None, model: None },
        ]);
        let preview = conv.preview();
        assert!(!preview.ends_with("..."));
    }

    #[test]
    fn test_preview_long_truncated() {
        let long_msg = "x".repeat(500);
        let conv = make_conv(vec![
            Message { idx: 0, role: Role::User, content: long_msg, timestamp: None, model: None },
        ]);
        let preview = conv.preview();
        assert!(preview.len() <= 310); // ~300 + "..."
        assert!(preview.ends_with("..."));
    }

    #[test]
    fn test_source_mtime_max() {
        let conv = Conversation {
            agent: Agent::Codex,
            external_id: None,
            title: None,
            workspace: None,
            source_path: PathBuf::from("/test"),
            source_files: vec![
                SourceFile { path: PathBuf::from("/a"), mtime: 100, size: 10 },
                SourceFile { path: PathBuf::from("/b"), mtime: 300, size: 20 },
                SourceFile { path: PathBuf::from("/c"), mtime: 200, size: 30 },
            ],
            source_fingerprint: "x".into(),
            started_at: None,
            ended_at: None,
            messages: vec![],
        };
        assert_eq!(conv.source_mtime_max(), 300);
    }

    #[test]
    fn test_source_mtime_max_empty() {
        let conv = make_conv(vec![]);
        assert_eq!(conv.source_mtime_max(), 0);
    }

    // ── source_fingerprint ─────────────────────────────────

    #[test]
    fn test_source_fingerprint_stable() {
        let files = vec![
            SourceFile { path: PathBuf::from("/a/b.json"), mtime: 1000, size: 500 },
            SourceFile { path: PathBuf::from("/c/d.json"), mtime: 2000, size: 1000 },
        ];
        let fp1 = source_fingerprint(&files);
        let fp2 = source_fingerprint(&files);
        assert_eq!(fp1, fp2);

        // Order shouldn't matter
        let files_rev = vec![
            SourceFile { path: PathBuf::from("/c/d.json"), mtime: 2000, size: 1000 },
            SourceFile { path: PathBuf::from("/a/b.json"), mtime: 1000, size: 500 },
        ];
        let fp3 = source_fingerprint(&files_rev);
        assert_eq!(fp1, fp3);
    }

    #[test]
    fn test_source_fingerprint_changes_on_mtime() {
        let files1 = vec![SourceFile { path: PathBuf::from("/a"), mtime: 1000, size: 500 }];
        let files2 = vec![SourceFile { path: PathBuf::from("/a"), mtime: 2000, size: 500 }];
        assert_ne!(source_fingerprint(&files1), source_fingerprint(&files2));
    }

    #[test]
    fn test_source_fingerprint_changes_on_size() {
        let files1 = vec![SourceFile { path: PathBuf::from("/a"), mtime: 1000, size: 500 }];
        let files2 = vec![SourceFile { path: PathBuf::from("/a"), mtime: 1000, size: 600 }];
        assert_ne!(source_fingerprint(&files1), source_fingerprint(&files2));
    }

    #[test]
    fn test_source_fingerprint_empty() {
        let fp = source_fingerprint(&[]);
        assert!(!fp.is_empty()); // Should still produce a hash
    }

    // ── truncate_title ─────────────────────────────────────

    #[test]
    fn test_truncate_title() {
        assert_eq!(truncate_title("hello", 10), "hello");
        assert_eq!(truncate_title("hello world this is long", 10), "hello w...");
    }

    #[test]
    fn test_truncate_title_exact_length() {
        assert_eq!(truncate_title("1234567890", 10), "1234567890");
    }

    #[test]
    fn test_truncate_title_trims_whitespace() {
        assert_eq!(truncate_title("  hello  ", 100), "hello");
    }

    #[test]
    fn test_truncate_title_empty() {
        assert_eq!(truncate_title("", 10), "");
    }

    // ── parse_timestamp ────────────────────────────────────

    #[test]
    fn test_parse_timestamp_millis() {
        let val = serde_json::json!(1705312800000_i64);
        let ts = parse_timestamp(&val).unwrap();
        assert_eq!(ts, 1705312800000);
    }

    #[test]
    fn test_parse_timestamp_seconds() {
        let val = serde_json::json!(1705312800_i64);
        let ts = parse_timestamp(&val).unwrap();
        assert_eq!(ts, 1705312800000); // Converted to millis
    }

    #[test]
    fn test_parse_timestamp_float() {
        let val = serde_json::json!(1705312800.5);
        let ts = parse_timestamp(&val).unwrap();
        assert_eq!(ts, 1705312800500);
    }

    #[test]
    fn test_parse_timestamp_iso_string() {
        let val = serde_json::json!("2024-01-15T10:00:00Z");
        let ts = parse_timestamp(&val).unwrap();
        assert!(ts > 0);
    }

    #[test]
    fn test_parse_timestamp_invalid() {
        assert!(parse_timestamp(&serde_json::json!(null)).is_none());
        assert!(parse_timestamp(&serde_json::json!(true)).is_none());
        assert!(parse_timestamp(&serde_json::json!("not-a-date")).is_none());
    }

    // ── flatten_content ────────────────────────────────────

    #[test]
    fn test_flatten_content_string() {
        let val = serde_json::json!("hello world");
        assert_eq!(flatten_content(&val), "hello world");
    }

    #[test]
    fn test_flatten_content_array_text_blocks() {
        let val = serde_json::json!([
            {"type": "text", "text": "Hello"},
            {"type": "text", "text": "World"}
        ]);
        let result = flatten_content(&val);
        assert!(result.contains("Hello"));
        assert!(result.contains("World"));
    }

    #[test]
    fn test_flatten_content_array_thinking() {
        let val = serde_json::json!([
            {"type": "thinking", "thinking": "Let me think..."}
        ]);
        assert!(flatten_content(&val).contains("Let me think"));
    }

    #[test]
    fn test_flatten_content_object_text_field() {
        let val = serde_json::json!({"text": "hello"});
        assert_eq!(flatten_content(&val), "hello");
    }

    #[test]
    fn test_flatten_content_object_content_field() {
        let val = serde_json::json!({"content": "nested"});
        assert_eq!(flatten_content(&val), "nested");
    }

    #[test]
    fn test_flatten_content_null() {
        assert_eq!(flatten_content(&serde_json::json!(null)), "");
    }

    #[test]
    fn test_flatten_content_number() {
        assert_eq!(flatten_content(&serde_json::json!(42)), "");
    }

    #[test]
    fn test_flatten_content_empty_array() {
        assert_eq!(flatten_content(&serde_json::json!([])), "");
    }

    #[test]
    fn test_flatten_content_mixed_array() {
        let val = serde_json::json!([
            {"type": "text", "text": "Hello"},
            {"type": "tool_use", "id": "123"},
            {"type": "thinking", "thinking": "Deep thought"},
            {"type": "prompt", "prompt": "Do this"}
        ]);
        let result = flatten_content(&val);
        assert!(result.contains("Hello"));
        assert!(result.contains("Deep thought"));
        assert!(result.contains("Do this"));
    }
}
