use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{Connector, file_modified_since, source_file};
use crate::model::{Agent, Conversation, Message, Role, SourceFile, source_fingerprint};

/// Progress information for OpenCode scanning.
#[derive(Debug, Clone)]
pub struct OpenCodeProgress {
    pub phase: OpenCodePhase,
    pub sessions_loaded: usize,
    pub sessions_total: usize,
    pub messages_loaded: usize,
    pub parts_loaded: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenCodePhase {
    Sessions,
    Messages,
    Parts,
    Assembly,
}

pub struct OpenCodeConnector {
    storage_root: Option<PathBuf>,
}

impl OpenCodeConnector {
    pub fn new() -> Self {
        // Check OPENCODE_STORAGE_ROOT env var first
        let storage_root = std::env::var("OPENCODE_STORAGE_ROOT")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                dirs::home_dir().map(|h| {
                    h.join(".local")
                        .join("share")
                        .join("opencode")
                        .join("storage")
                })
            });

        Self { storage_root }
    }

    fn session_dir(&self) -> Option<PathBuf> {
        self.storage_root.as_ref().map(|r| r.join("session"))
    }

    fn message_dir(&self) -> Option<PathBuf> {
        self.storage_root.as_ref().map(|r| r.join("message"))
    }

    fn part_dir(&self) -> Option<PathBuf> {
        self.storage_root.as_ref().map(|r| r.join("part"))
    }
}

impl Default for OpenCodeConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl Connector for OpenCodeConnector {
    fn agent(&self) -> Agent {
        Agent::OpenCode
    }

    fn detect(&self) -> bool {
        self.session_dir().map(|p| p.exists()).unwrap_or(false)
    }

    fn default_roots(&self) -> Vec<PathBuf> {
        self.storage_root.clone().into_iter().collect()
    }

    fn scan(&self, _roots: &[PathBuf], since_ts: Option<i64>) -> Result<Vec<Conversation>> {
        let session_dir = match self.session_dir() {
            Some(d) if d.exists() => d,
            _ => return Ok(Vec::new()),
        };

        let message_dir = self.message_dir();
        let part_dir = self.part_dir();

        // Phase 1: Load all sessions
        let sessions = load_sessions(&session_dir, since_ts)?;
        tracing::info!("Loaded {} OpenCode sessions", sessions.len());

        if sessions.is_empty() {
            return Ok(Vec::new());
        }

        // Phase 2: Inventory all messages
        let (message_map, session_messages) = if let Some(ref msg_dir) = message_dir {
            load_messages(msg_dir, &sessions)?
        } else {
            (HashMap::new(), HashMap::new())
        };

        tracing::info!("Loaded {} OpenCode messages", message_map.len());

        // Phase 3: Build source inventory and compute fingerprints
        let session_sources: HashMap<String, Vec<SourceFile>> = sessions
            .iter()
            .map(|(id, session)| {
                let mut files = vec![session.source_file.clone()];

                // Add message files for this session
                if let Some(msg_ids) = session_messages.get(id) {
                    for msg_id in msg_ids {
                        if let Some(msg) = message_map.get(msg_id) {
                            files.push(msg.source_file.clone());
                        }
                    }
                }

                (id.clone(), files)
            })
            .collect();

        // Phase 4: Parse parts and assemble conversations
        let conversations: Vec<Conversation> = sessions
            .into_par_iter()
            .filter_map(|(session_id, session)| {
                // Get message IDs for this session
                let msg_ids = session_messages
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_default();

                // Load parts for each message
                let mut messages: Vec<Message> = Vec::new();
                let mut all_source_files = session_sources
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_default();

                for msg_id in &msg_ids {
                    if let Some(msg_meta) = message_map.get(msg_id) {
                        // Load parts for this message
                        if let Some(ref part_dir) = part_dir {
                            let msg_part_dir = part_dir.join(msg_id);
                            if msg_part_dir.exists() {
                                if let Ok(parts) = load_parts(&msg_part_dir) {
                                    for part in parts {
                                        all_source_files.push(part.source_file.clone());

                                        let content = match part.part_type.as_str() {
                                            "text" => part
                                                .data
                                                .get("text")
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string()),
                                            "subtask" => part
                                                .data
                                                .get("prompt")
                                                .and_then(|v| v.as_str())
                                                .map(|s| format!("[Subtask] {}", s)),
                                            "tool" => {
                                                let tool_name = part
                                                    .data
                                                    .get("name")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("unknown");
                                                Some(format!("[Tool: {}]", tool_name))
                                            }
                                            _ => None,
                                        };

                                        if let Some(content) = content {
                                            if !content.trim().is_empty() {
                                                messages.push(Message {
                                                    idx: messages.len(),
                                                    role: msg_meta.role.clone(),
                                                    content,
                                                    timestamp: msg_meta.created_at,
                                                    model: msg_meta.model.clone(),
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                if messages.is_empty() {
                    return None;
                }

                // Sort messages by timestamp
                messages.sort_by_key(|m| m.timestamp);
                // Re-assign indices after sorting
                for (idx, msg) in messages.iter_mut().enumerate() {
                    msg.idx = idx;
                }

                let title = session.title.clone().or_else(|| {
                    messages.iter().find(|m| m.role == Role::User).map(|m| {
                        let first_line = m.content.lines().next().unwrap_or(&m.content);
                        crate::model::truncate_title(first_line, 100)
                    })
                });

                let started_at = messages.iter().filter_map(|m| m.timestamp).min();
                let ended_at = messages.iter().filter_map(|m| m.timestamp).max();

                // Sort source files for stable fingerprinting
                all_source_files.sort_by(|a, b| a.path.cmp(&b.path));
                let fingerprint = source_fingerprint(&all_source_files);

                Some(Conversation {
                    agent: Agent::OpenCode,
                    external_id: Some(session_id),
                    title,
                    workspace: session.directory.clone(),
                    source_path: session.source_file.path.clone(),
                    source_files: all_source_files,
                    source_fingerprint: fingerprint,
                    started_at,
                    ended_at,
                    messages,
                })
            })
            .collect();

        Ok(conversations)
    }
}

#[derive(Debug, Clone)]
struct SessionMeta {
    id: String,
    project_id: String,
    directory: Option<PathBuf>,
    title: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    source_file: SourceFile,
}

#[derive(Debug, Clone)]
struct MessageMeta {
    id: String,
    session_id: String,
    role: Role,
    created_at: Option<i64>,
    model: Option<String>,
    source_file: SourceFile,
}

#[derive(Debug, Clone)]
struct PartMeta {
    id: String,
    message_id: String,
    part_type: String,
    data: Value,
    source_file: SourceFile,
}

fn load_sessions(
    session_dir: &Path,
    since_ts: Option<i64>,
) -> Result<HashMap<String, SessionMeta>> {
    let mut sessions = HashMap::new();

    for entry in WalkDir::new(session_dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file() && e.path().extension().map(|e| e == "json").unwrap_or(false)
        })
    {
        let path = entry.path();

        if !file_modified_since(path, since_ts) {
            continue;
        }

        match parse_session_file(path) {
            Ok(Some(session)) => {
                sessions.insert(session.id.clone(), session);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("Failed to parse session file {}: {}", path.display(), e);
            }
        }
    }

    Ok(sessions)
}

#[derive(Deserialize)]
struct SessionJson {
    id: String,
    #[serde(rename = "projectID")]
    project_id: String,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    time: Option<SessionTime>,
}

#[derive(Deserialize)]
struct SessionTime {
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    updated: Option<i64>,
}

fn parse_session_file(path: &Path) -> Result<Option<SessionMeta>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;

    let data: SessionJson = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;

    let source_file = source_file(path)
        .with_context(|| format!("Failed to get source file info for {}", path.display()))?;

    Ok(Some(SessionMeta {
        id: data.id,
        project_id: data.project_id,
        directory: data.directory.map(PathBuf::from),
        title: data.title,
        created_at: data.time.as_ref().and_then(|t| t.created),
        updated_at: data.time.as_ref().and_then(|t| t.updated),
        source_file,
    }))
}

fn load_messages(
    message_dir: &Path,
    sessions: &HashMap<String, SessionMeta>,
) -> Result<(HashMap<String, MessageMeta>, HashMap<String, Vec<String>>)> {
    let mut message_map = HashMap::new();
    let mut session_messages: HashMap<String, Vec<String>> = HashMap::new();

    // Only scan message directories for sessions we know about
    for session_id in sessions.keys() {
        let session_msg_dir = message_dir.join(session_id);
        if !session_msg_dir.exists() {
            continue;
        }

        for entry in fs::read_dir(&session_msg_dir)? {
            let entry = entry?;
            let path = entry.path();

            if !path.is_file() || path.extension().map(|e| e != "json").unwrap_or(true) {
                continue;
            }

            match parse_message_file(&path) {
                Ok(Some(msg)) => {
                    let msg_id = msg.id.clone();
                    session_messages
                        .entry(session_id.clone())
                        .or_default()
                        .push(msg_id.clone());
                    message_map.insert(msg_id, msg);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("Failed to parse message file {}: {}", path.display(), e);
                }
            }
        }
    }

    Ok((message_map, session_messages))
}

#[derive(Deserialize)]
struct MessageJson {
    id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    role: String,
    #[serde(default)]
    time: Option<MessageTime>,
    #[serde(default)]
    model: Option<MessageModel>,
}

#[derive(Deserialize)]
struct MessageTime {
    #[serde(default)]
    created: Option<i64>,
}

#[derive(Deserialize)]
struct MessageModel {
    #[serde(default, rename = "modelID")]
    model_id: Option<String>,
}

fn parse_message_file(path: &Path) -> Result<Option<MessageMeta>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;

    let data: MessageJson = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;

    let source_file = source_file(path)
        .with_context(|| format!("Failed to get source file info for {}", path.display()))?;

    let role = match data.role.to_lowercase().as_str() {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        "system" => Role::System,
        _ => Role::User, // Default to user for unknown roles
    };

    Ok(Some(MessageMeta {
        id: data.id,
        session_id: data.session_id,
        role,
        created_at: data.time.as_ref().and_then(|t| t.created),
        model: data.model.and_then(|m| m.model_id),
        source_file,
    }))
}

fn load_parts(part_dir: &Path) -> Result<Vec<PartMeta>> {
    let mut parts = Vec::new();

    for entry in fs::read_dir(part_dir)? {
        let entry = entry?;
        let path = entry.path();

        if !path.is_file() || path.extension().map(|e| e != "json").unwrap_or(true) {
            continue;
        }

        match parse_part_file(&path) {
            Ok(Some(part)) => {
                parts.push(part);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("Failed to parse part file {}: {}", path.display(), e);
            }
        }
    }

    // Sort parts by ID for stable ordering
    parts.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(parts)
}

#[derive(Deserialize)]
struct PartJson {
    id: String,
    #[serde(rename = "messageID")]
    message_id: String,
    #[serde(rename = "type")]
    part_type: String,
    #[serde(flatten)]
    data: Value,
}

fn parse_part_file(path: &Path) -> Result<Option<PartMeta>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;

    let data: PartJson = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;

    let source_file = source_file(path)
        .with_context(|| format!("Failed to get source file info for {}", path.display()))?;

    Ok(Some(PartMeta {
        id: data.id,
        message_id: data.message_id,
        part_type: data.part_type,
        data: data.data,
        source_file,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper to create a full OpenCode storage tree for testing.
    fn create_opencode_tree(base: &Path) {
        let session_dir = base.join("session").join("proj1");
        let message_dir = base.join("message").join("sess1");
        let part_dir_msg1 = base.join("part").join("msg1");
        let part_dir_msg2 = base.join("part").join("msg2");

        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&message_dir).unwrap();
        fs::create_dir_all(&part_dir_msg1).unwrap();
        fs::create_dir_all(&part_dir_msg2).unwrap();

        // Session file
        fs::write(
            session_dir.join("sess1.json"),
            r#"{
                "id": "sess1",
                "projectID": "proj1",
                "directory": "/home/user/myproject",
                "title": "Test Session",
                "time": {"created": 1705312800000, "updated": 1705312900000}
            }"#,
        )
        .unwrap();

        // Message files
        fs::write(
            message_dir.join("msg1.json"),
            r#"{
                "id": "msg1",
                "sessionID": "sess1",
                "role": "user",
                "time": {"created": 1705312800000},
                "model": {"modelID": "gpt-4"}
            }"#,
        )
        .unwrap();

        fs::write(
            message_dir.join("msg2.json"),
            r#"{
                "id": "msg2",
                "sessionID": "sess1",
                "role": "assistant",
                "time": {"created": 1705312805000},
                "model": {"modelID": "gpt-4"}
            }"#,
        )
        .unwrap();

        // Part files
        fs::write(
            part_dir_msg1.join("part1.json"),
            r#"{
                "id": "part1",
                "messageID": "msg1",
                "type": "text",
                "text": "Hello, can you help me?"
            }"#,
        )
        .unwrap();

        fs::write(
            part_dir_msg2.join("part2.json"),
            r#"{
                "id": "part2",
                "messageID": "msg2",
                "type": "text",
                "text": "Sure, I can help!"
            }"#,
        )
        .unwrap();
    }

    #[test]
    fn test_parse_session_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sess1.json");
        fs::write(
            &path,
            r#"{
                "id": "sess1",
                "projectID": "proj1",
                "directory": "/test/project",
                "title": "My Session",
                "time": {"created": 1705312800000, "updated": 1705312900000}
            }"#,
        )
        .unwrap();

        let session = parse_session_file(&path).unwrap().unwrap();
        assert_eq!(session.id, "sess1");
        assert_eq!(session.project_id, "proj1");
        assert_eq!(session.directory, Some(PathBuf::from("/test/project")));
        assert_eq!(session.title, Some("My Session".to_string()));
        assert_eq!(session.created_at, Some(1705312800000));
        assert_eq!(session.updated_at, Some(1705312900000));
    }

    #[test]
    fn test_parse_session_file_minimal() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sess2.json");
        fs::write(&path, r#"{"id": "sess2", "projectID": "proj1"}"#).unwrap();

        let session = parse_session_file(&path).unwrap().unwrap();
        assert_eq!(session.id, "sess2");
        assert!(session.directory.is_none());
        assert!(session.title.is_none());
        assert!(session.created_at.is_none());
    }

    #[test]
    fn test_parse_message_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("msg1.json");
        fs::write(
            &path,
            r#"{
                "id": "msg1",
                "sessionID": "sess1",
                "role": "assistant",
                "time": {"created": 1705312800000},
                "model": {"modelID": "claude-3-opus"}
            }"#,
        )
        .unwrap();

        let msg = parse_message_file(&path).unwrap().unwrap();
        assert_eq!(msg.id, "msg1");
        assert_eq!(msg.session_id, "sess1");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.created_at, Some(1705312800000));
        assert_eq!(msg.model, Some("claude-3-opus".to_string()));
    }

    #[test]
    fn test_parse_message_file_unknown_role() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("msg_unknown.json");
        fs::write(
            &path,
            r#"{"id": "msg1", "sessionID": "s1", "role": "unknown_role"}"#,
        )
        .unwrap();

        let msg = parse_message_file(&path).unwrap().unwrap();
        assert_eq!(msg.role, Role::User); // Defaults to user
    }

    #[test]
    fn test_parse_part_file_text() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("part1.json");
        fs::write(
            &path,
            r#"{"id": "p1", "messageID": "m1", "type": "text", "text": "Hello world"}"#,
        )
        .unwrap();

        let part = parse_part_file(&path).unwrap().unwrap();
        assert_eq!(part.id, "p1");
        assert_eq!(part.message_id, "m1");
        assert_eq!(part.part_type, "text");
        assert_eq!(
            part.data.get("text").unwrap().as_str().unwrap(),
            "Hello world"
        );
    }

    #[test]
    fn test_parse_part_file_subtask() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("part_sub.json");
        fs::write(
            &path,
            r#"{"id": "p2", "messageID": "m1", "type": "subtask", "prompt": "Analyze the code"}"#,
        )
        .unwrap();

        let part = parse_part_file(&path).unwrap().unwrap();
        assert_eq!(part.part_type, "subtask");
        assert_eq!(
            part.data.get("prompt").unwrap().as_str().unwrap(),
            "Analyze the code"
        );
    }

    #[test]
    fn test_parse_part_file_tool() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("part_tool.json");
        fs::write(
            &path,
            r#"{"id": "p3", "messageID": "m1", "type": "tool", "name": "read_file"}"#,
        )
        .unwrap();

        let part = parse_part_file(&path).unwrap().unwrap();
        assert_eq!(part.part_type, "tool");
    }

    #[test]
    fn test_load_sessions() {
        let dir = TempDir::new().unwrap();
        let session_dir = dir.path().join("proj1");
        fs::create_dir_all(&session_dir).unwrap();

        fs::write(
            session_dir.join("s1.json"),
            r#"{"id": "s1", "projectID": "proj1", "directory": "/test"}"#,
        )
        .unwrap();

        fs::write(
            session_dir.join("s2.json"),
            r#"{"id": "s2", "projectID": "proj1", "directory": "/test2"}"#,
        )
        .unwrap();

        let sessions = load_sessions(dir.path(), None).unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.contains_key("s1"));
        assert!(sessions.contains_key("s2"));
    }

    #[test]
    fn test_load_sessions_with_since_ts() {
        let dir = TempDir::new().unwrap();
        let session_dir = dir.path().join("proj1");
        fs::create_dir_all(&session_dir).unwrap();

        fs::write(
            session_dir.join("s1.json"),
            r#"{"id": "s1", "projectID": "proj1"}"#,
        )
        .unwrap();

        // Far future timestamp should exclude files
        let future = chrono::Utc::now().timestamp_millis() + 1_000_000;
        let sessions = load_sessions(dir.path(), Some(future)).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_load_sessions_skips_malformed() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("bad.json"), "NOT JSON").unwrap();
        fs::write(
            dir.path().join("good.json"),
            r#"{"id": "s1", "projectID": "proj1"}"#,
        )
        .unwrap();

        let sessions = load_sessions(dir.path(), None).unwrap();
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn test_load_parts_sorted() {
        let dir = TempDir::new().unwrap();

        // Write parts in reverse order
        fs::write(
            dir.path().join("b_part.json"),
            r#"{"id": "b", "messageID": "m1", "type": "text", "text": "Second"}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("a_part.json"),
            r#"{"id": "a", "messageID": "m1", "type": "text", "text": "First"}"#,
        )
        .unwrap();

        let parts = load_parts(dir.path()).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].id, "a"); // Sorted by ID
        assert_eq!(parts[1].id, "b");
    }

    #[test]
    fn test_full_scan() {
        let dir = TempDir::new().unwrap();
        create_opencode_tree(dir.path());

        let connector = OpenCodeConnector {
            storage_root: Some(dir.path().to_path_buf()),
        };

        assert!(connector.detect());

        let conversations = connector.scan(&[dir.path().to_path_buf()], None).unwrap();
        assert_eq!(conversations.len(), 1);

        let conv = &conversations[0];
        assert_eq!(conv.agent, Agent::OpenCode);
        assert_eq!(conv.external_id, Some("sess1".to_string()));
        assert_eq!(conv.workspace, Some(PathBuf::from("/home/user/myproject")));
        assert_eq!(conv.title, Some("Test Session".to_string()));
        assert_eq!(conv.messages.len(), 2);

        // Check messages are ordered
        assert_eq!(conv.messages[0].role, Role::User);
        assert_eq!(conv.messages[1].role, Role::Assistant);

        // Source files should include session + message + part files
        assert!(conv.source_files.len() >= 3);
        assert!(!conv.source_fingerprint.is_empty());
    }

    #[test]
    fn test_scan_empty_storage() {
        let dir = TempDir::new().unwrap();
        let session_dir = dir.path().join("session");
        fs::create_dir_all(&session_dir).unwrap();

        let connector = OpenCodeConnector {
            storage_root: Some(dir.path().to_path_buf()),
        };

        let conversations = connector.scan(&[dir.path().to_path_buf()], None).unwrap();
        assert!(conversations.is_empty());
    }

    #[test]
    fn test_scan_session_no_messages() {
        let dir = TempDir::new().unwrap();
        let session_dir = dir.path().join("session").join("proj1");
        fs::create_dir_all(&session_dir).unwrap();

        fs::write(
            session_dir.join("orphan.json"),
            r#"{"id": "orphan", "projectID": "proj1", "title": "No Messages"}"#,
        )
        .unwrap();

        let connector = OpenCodeConnector {
            storage_root: Some(dir.path().to_path_buf()),
        };

        let conversations = connector.scan(&[dir.path().to_path_buf()], None).unwrap();
        assert!(conversations.is_empty()); // No messages → no conversation
    }

    #[test]
    fn test_connector_detect_nonexistent() {
        let connector = OpenCodeConnector {
            storage_root: Some(PathBuf::from("/nonexistent/storage")),
        };
        assert!(!connector.detect());
    }

    #[test]
    fn test_connector_default_roots() {
        let connector = OpenCodeConnector {
            storage_root: Some(PathBuf::from("/test/storage")),
        };
        let roots = connector.default_roots();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from("/test/storage"));
    }
}
