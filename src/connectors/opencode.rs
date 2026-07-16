use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use rayon::prelude::*;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, database_source_path, file_modified_since, flatten_json_content,
    parse_database_source_path, source_file,
};
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
    data_root: Option<PathBuf>,
    db_override: Option<PathBuf>,
}

impl OpenCodeConnector {
    pub fn new() -> Self {
        let data_root = dirs::home_dir().map(|home| home.join(".local/share/opencode"));
        // Check OPENCODE_STORAGE_ROOT env var first for the legacy JSON tree.
        let storage_root = std::env::var("OPENCODE_STORAGE_ROOT")
            .ok()
            .map(PathBuf::from)
            .or_else(|| data_root.as_ref().map(|root| root.join("storage")));
        let db_override = std::env::var_os("OPENCODE_DB")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    data_root
                        .as_ref()
                        .map(|root| root.join(&path))
                        .unwrap_or(path)
                }
            });

        Self {
            storage_root,
            data_root,
            db_override,
        }
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
        self.default_roots().iter().any(|root| {
            legacy_storage_root(root).is_some_and(|storage| storage.join("session").is_dir())
                || !discover_databases(root).is_empty()
        })
    }

    fn default_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(root) = &self.storage_root {
            roots.push(root.clone());
        }
        if let Some(root) = &self.data_root {
            roots.push(root.clone());
        }
        if let Some(path) = &self.db_override {
            roots.push(path.clone());
        }
        roots.sort();
        roots.dedup();
        roots
    }

    fn scan(&self, roots: &[PathBuf], since_ts: Option<i64>) -> Result<Vec<Conversation>> {
        scan_opencode_roots(roots, since_ts)
    }

    fn parser_revision(&self) -> Option<&'static str> {
        Some("2")
    }

    fn source_exists(&self, path: &Path) -> Result<bool> {
        let Some((root, session_id)) = parse_database_source_path(path, Agent::OpenCode) else {
            return Ok(path.try_exists()?);
        };
        let mut first_error = None;
        for database in discover_databases(&root) {
            let connection = match open_read_only(&database) {
                Ok(connection) => connection,
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            if !table_exists(&connection, "session")? {
                continue;
            }
            if connection
                .query_row(
                    "SELECT 1 FROM session WHERE id = ? LIMIT 1",
                    [&session_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
            {
                return Ok(true);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(false),
        }
    }
}

fn scan_opencode_roots(roots: &[PathBuf], since_ts: Option<i64>) -> Result<Vec<Conversation>> {
    let mut legacy_roots = Vec::new();
    let mut databases: HashMap<PathBuf, PathBuf> = HashMap::new();

    for root in roots {
        if let Some(storage) = legacy_storage_root(root) {
            legacy_roots.push(storage);
        }
        let virtual_root = if root.is_file() {
            root.parent().unwrap_or(Path::new(".")).to_path_buf()
        } else if root.file_name().is_some_and(|name| name == "storage") {
            root.parent().unwrap_or(root).to_path_buf()
        } else {
            root.clone()
        };
        for database in discover_databases(root) {
            databases
                .entry(database)
                .or_insert_with(|| virtual_root.clone());
        }
    }
    legacy_roots.sort();
    legacy_roots.dedup();

    let legacy_paths = inventory_legacy_sources(&legacy_roots);
    let mut by_session: HashMap<String, Conversation> = HashMap::new();
    let mut parse_errors = 0usize;

    for storage in &legacy_roots {
        match scan_legacy_storage(storage, since_ts) {
            Ok(conversations) => {
                for conversation in conversations {
                    merge_conversation(&mut by_session, conversation);
                }
            }
            Err(error) => {
                parse_errors += 1;
                tracing::warn!(
                    agent = Agent::OpenCode.slug(),
                    root = %storage.display(),
                    error = %error,
                    "Failed to scan legacy OpenCode storage"
                );
            }
        }
    }

    let database_count = databases.len();
    for (database, virtual_root) in databases {
        if !database_modified_since(&database, since_ts) {
            continue;
        }
        match scan_opencode_database(&database, &virtual_root) {
            Ok(mut conversations) => {
                for conversation in &mut conversations {
                    if let Some(id) = &conversation.external_id
                        && let Some(path) = legacy_paths.get(id)
                    {
                        conversation.source_path = path.clone();
                    }
                }
                for conversation in conversations {
                    merge_conversation(&mut by_session, conversation);
                }
            }
            Err(error) => {
                parse_errors += 1;
                tracing::debug!(
                    agent = Agent::OpenCode.slug(),
                    database = %database.display(),
                    error = %error,
                    "Skipping unsupported OpenCode database"
                );
            }
        }
    }

    let mut conversations: Vec<_> = by_session.into_values().collect();
    conversations.sort_by(|left, right| left.source_path.cmp(&right.source_path));
    tracing::info!(
        agent = Agent::OpenCode.slug(),
        roots = roots.len(),
        databases = database_count,
        discovered = conversations.len(),
        parsed = conversations.len(),
        parse_errors,
        "Completed OpenCode session scan"
    );
    Ok(conversations)
}

fn merge_conversation(
    conversations: &mut HashMap<String, Conversation>,
    mut candidate: Conversation,
) {
    let Some(id) = candidate.external_id.clone() else {
        return;
    };
    let Some(existing) = conversations.get_mut(&id) else {
        conversations.insert(id, candidate);
        return;
    };

    let existing_score = (
        existing
            .ended_at
            .or(existing.started_at)
            .unwrap_or_default(),
        existing.messages.len(),
    );
    let candidate_score = (
        candidate
            .ended_at
            .or(candidate.started_at)
            .unwrap_or_default(),
        candidate.messages.len(),
    );
    if candidate_score > existing_score {
        if existing.source_path.try_exists().unwrap_or(false) {
            candidate.source_path = existing.source_path.clone();
        } else if !candidate.source_path.try_exists().unwrap_or(false) {
            candidate.source_path =
                std::cmp::min(existing.source_path.clone(), candidate.source_path.clone());
        }
        *existing = candidate;
    }
}

fn legacy_storage_root(root: &Path) -> Option<PathBuf> {
    if root.join("session").is_dir() {
        Some(root.to_path_buf())
    } else if root.join("storage/session").is_dir() {
        Some(root.join("storage"))
    } else {
        None
    }
}

fn inventory_legacy_sources(roots: &[PathBuf]) -> HashMap<String, PathBuf> {
    let mut sources = HashMap::new();
    for root in roots {
        let session_dir = root.join("session");
        for entry in WalkDir::new(session_dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.file_type().is_file()
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "json")
            })
        {
            if let Some(id) = entry.path().file_stem().and_then(|stem| stem.to_str()) {
                sources
                    .entry(id.to_string())
                    .or_insert_with(|| entry.path().to_path_buf());
            }
        }
    }
    sources
}

fn scan_legacy_storage(storage_root: &Path, since_ts: Option<i64>) -> Result<Vec<Conversation>> {
    let session_dir = storage_root.join("session");
    if !session_dir.is_dir() {
        return Ok(Vec::new());
    }
    let sessions = load_sessions(&session_dir, since_ts)?;
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    let (message_map, session_messages) = load_messages(&storage_root.join("message"), &sessions)?;
    let part_dir = storage_root.join("part");
    let session_sources: HashMap<String, Vec<SourceFile>> = sessions
        .iter()
        .map(|(id, session)| {
            let mut files = vec![session.source_file.clone()];
            if let Some(message_ids) = session_messages.get(id) {
                files.extend(
                    message_ids
                        .iter()
                        .filter_map(|id| message_map.get(id))
                        .map(|message| message.source_file.clone()),
                );
            }
            (id.clone(), files)
        })
        .collect();

    Ok(sessions
        .into_par_iter()
        .filter_map(|(session_id, session)| {
            let mut messages = Vec::new();
            let mut all_source_files = session_sources
                .get(&session_id)
                .cloned()
                .unwrap_or_default();
            for message_id in session_messages.get(&session_id).into_iter().flatten() {
                let Some(metadata) = message_map.get(message_id) else {
                    continue;
                };
                let message_part_dir = part_dir.join(message_id);
                let Ok(parts) = load_parts(&message_part_dir) else {
                    continue;
                };
                for part in parts {
                    all_source_files.push(part.source_file.clone());
                    let content = match part.part_type.as_str() {
                        "text" => part
                            .data
                            .get("text")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        "subtask" => part
                            .data
                            .get("prompt")
                            .and_then(Value::as_str)
                            .map(|prompt| format!("[Subtask] {prompt}")),
                        "tool" => part
                            .data
                            .get("name")
                            .and_then(Value::as_str)
                            .map(|name| format!("[Tool: {name}]"))
                            .or_else(|| Some("[Tool: unknown]".to_string())),
                        _ => None,
                    };
                    if let Some(content) = content.filter(|content| !content.trim().is_empty()) {
                        messages.push(Message {
                            idx: messages.len(),
                            role: metadata.role.clone(),
                            content,
                            timestamp: metadata.created_at,
                            model: metadata.model.clone(),
                        });
                    }
                }
            }
            if messages.is_empty() {
                return None;
            }
            messages.sort_by_key(|message| message.timestamp);
            for (index, message) in messages.iter_mut().enumerate() {
                message.idx = index;
            }
            let title = session.title.clone().or_else(|| derive_title(&messages));
            let started_at = messages
                .iter()
                .filter_map(|message| message.timestamp)
                .min()
                .or(session.created_at);
            let ended_at = messages
                .iter()
                .filter_map(|message| message.timestamp)
                .max()
                .or(session.updated_at);
            all_source_files.sort_by(|left, right| left.path.cmp(&right.path));
            let fingerprint = source_fingerprint(&all_source_files);
            Some(Conversation {
                agent: Agent::OpenCode,
                external_id: Some(session_id),
                title,
                workspace: session.directory,
                source_path: session.source_file.path,
                source_files: all_source_files,
                source_fingerprint: fingerprint,
                started_at,
                ended_at,
                messages,
            })
        })
        .collect())
}

fn discover_databases(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return (root.extension().is_some_and(|extension| extension == "db"))
            .then(|| root.to_path_buf())
            .into_iter()
            .collect();
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut databases: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().is_some_and(|extension| extension == "db")
        })
        .collect();
    databases.sort();
    databases
}

fn open_read_only(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(connection)
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn database_modified_since(path: &Path, since_ts: Option<i64>) -> bool {
    let wal = sidecar_path(path, "-wal");
    file_modified_since(path, since_ts) || (wal.exists() && file_modified_since(&wal, since_ts))
}

fn database_source_files(path: &Path) -> Result<Vec<SourceFile>> {
    let mut files = Vec::new();
    if let Some(file) = source_file(path) {
        files.push(file);
    }
    if let Some(file) = source_file(&sidecar_path(path, "-wal")) {
        files.push(file);
    }
    if files.is_empty() {
        anyhow::bail!(
            "OpenCode database disappeared during scan: {}",
            path.display()
        );
    }
    Ok(files)
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn table_columns(connection: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    Ok(statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<HashSet<_>, _>>()?)
}

fn scan_opencode_database(database: &Path, virtual_root: &Path) -> Result<Vec<Conversation>> {
    let connection = open_read_only(database)?;
    if !table_exists(&connection, "session")? {
        anyhow::bail!("missing session table");
    }
    let columns = table_columns(&connection, "session")?;
    for required in ["id", "directory", "title", "time_created", "time_updated"] {
        if !columns.contains(required) {
            anyhow::bail!("unsupported session schema: missing {required}");
        }
    }
    let model = if columns.contains("model") {
        "model"
    } else {
        "NULL"
    };
    let query = format!(
        "SELECT id, directory, title, time_created, time_updated, {model} \
         FROM session ORDER BY time_created, id"
    );
    let has_v1 = table_exists(&connection, "message")? && table_exists(&connection, "part")?;
    let has_v2 = table_exists(&connection, "session_message")?;
    let source_files = database_source_files(database)?;
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map([], |row| {
        Ok(DatabaseSession {
            id: row.get(0)?,
            directory: row.get(1)?,
            title: row.get(2)?,
            created_at: row.get(3)?,
            updated_at: row.get(4)?,
            model: row.get(5)?,
        })
    })?;

    let mut conversations = Vec::new();
    for row in rows {
        let session = row?;
        let mut messages = if has_v2 {
            load_v2_messages(&connection, &session)?
        } else {
            Vec::new()
        };
        if messages.is_empty() && has_v1 {
            messages = load_v1_messages(&connection, &session)?;
        }
        if messages.is_empty() {
            continue;
        }
        for (index, message) in messages.iter_mut().enumerate() {
            message.idx = index;
        }
        let title = Some(session.title.clone())
            .filter(|title| !title.trim().is_empty())
            .or_else(|| derive_title(&messages));
        let started_at = messages
            .iter()
            .filter_map(|message| message.timestamp)
            .min()
            .or(Some(session.created_at));
        let ended_at = messages
            .iter()
            .filter_map(|message| message.timestamp)
            .max()
            .or(Some(session.updated_at));
        let fingerprint =
            database_fingerprint(&session, title.as_deref(), started_at, ended_at, &messages)?;
        conversations.push(Conversation {
            agent: Agent::OpenCode,
            external_id: Some(session.id.clone()),
            title,
            workspace: Some(PathBuf::from(&session.directory)),
            source_path: database_source_path(virtual_root, Agent::OpenCode, &session.id),
            source_files: source_files.clone(),
            source_fingerprint: fingerprint,
            started_at,
            ended_at,
            messages,
        });
    }
    Ok(conversations)
}

struct DatabaseSession {
    id: String,
    directory: String,
    title: String,
    created_at: i64,
    updated_at: i64,
    model: Option<String>,
}

fn load_v1_messages(connection: &Connection, session: &DatabaseSession) -> Result<Vec<Message>> {
    let mut statement = connection.prepare(
        "SELECT m.time_created, m.data, p.data \
         FROM message m LEFT JOIN part p ON p.message_id = m.id \
         WHERE m.session_id = ? ORDER BY m.time_created, m.id, p.time_created, p.id",
    )?;
    let rows = statement.query_map([&session.id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let fallback_model = parse_model(session.model.as_deref());
    let mut messages = Vec::new();
    for row in rows {
        let (created_at, message_json, part_json) = row?;
        let message_data: Value = serde_json::from_str(&message_json)?;
        let role = message_data
            .get("role")
            .and_then(Value::as_str)
            .and_then(crate::connectors::parse_role)
            .unwrap_or(Role::User);
        let model = message_model(&message_data).or_else(|| fallback_model.clone());
        let timestamp = message_data
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .or(Some(created_at));
        let Some(part_json) = part_json else {
            continue;
        };
        let part: Value = serde_json::from_str(&part_json)?;
        let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
        let normalized = match part_type {
            "text" => part
                .get("text")
                .and_then(Value::as_str)
                .map(|text| (role.clone(), text.to_string())),
            "subtask" => part
                .get("prompt")
                .and_then(Value::as_str)
                .map(|prompt| (role.clone(), format!("[Subtask] {prompt}"))),
            "tool" => Some((Role::Tool, format_opencode_tool(&part))),
            "file" => Some((role.clone(), format_opencode_file(&part))),
            "patch" => Some((Role::Tool, format_opencode_patch(&part))),
            "compaction" => Some((Role::System, "[Compaction]".to_string())),
            _ => None,
        };
        if let Some((role, content)) = normalized.filter(|(_, content)| !content.trim().is_empty())
        {
            messages.push(Message {
                idx: messages.len(),
                role,
                content,
                timestamp,
                model,
            });
        }
    }
    Ok(messages)
}

fn load_v2_messages(connection: &Connection, session: &DatabaseSession) -> Result<Vec<Message>> {
    let mut statement = connection.prepare(
        "SELECT type, time_created, data FROM session_message \
         WHERE session_id = ? ORDER BY seq, id",
    )?;
    let rows = statement.query_map([&session.id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let fallback_model = parse_model(session.model.as_deref());
    let mut messages = Vec::new();
    for row in rows {
        let (message_type, created_at, raw_data) = row?;
        let data: Value = serde_json::from_str(&raw_data)?;
        let timestamp = data
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .or(Some(created_at));
        match message_type.as_str() {
            "user" => {
                let content = format_v2_user_message(&data);
                push_message(
                    &mut messages,
                    Role::User,
                    &content,
                    timestamp,
                    fallback_model.clone(),
                );
            }
            "synthetic" => push_message(
                &mut messages,
                Role::System,
                data.get("text").and_then(Value::as_str).unwrap_or(""),
                timestamp,
                fallback_model.clone(),
            ),
            "system" => push_message(
                &mut messages,
                Role::System,
                data.get("text").and_then(Value::as_str).unwrap_or(""),
                timestamp,
                fallback_model.clone(),
            ),
            "skill" => {
                let name = data
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let text = data.get("text").and_then(Value::as_str).unwrap_or("");
                push_message(
                    &mut messages,
                    Role::System,
                    &format!("[Skill: {name}]\n{text}"),
                    timestamp,
                    fallback_model.clone(),
                );
            }
            "shell" => {
                let command = data.get("command").and_then(Value::as_str).unwrap_or("");
                let output = data.get("output").map(json_value_text).unwrap_or_default();
                push_message(
                    &mut messages,
                    Role::Tool,
                    &format!("[Shell]\nCommand: {command}\nOutput: {output}"),
                    timestamp,
                    fallback_model.clone(),
                );
            }
            "compaction" => {
                let summary = data.get("summary").and_then(Value::as_str).unwrap_or("");
                let recent = data.get("recent").and_then(Value::as_str).unwrap_or("");
                push_message(
                    &mut messages,
                    Role::System,
                    &format!("[Compaction summary]\n{summary}\n{recent}"),
                    timestamp,
                    fallback_model.clone(),
                );
            }
            "assistant" => {
                let model = message_model(&data).or_else(|| fallback_model.clone());
                let before = messages.len();
                for content in data
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match content.get("type").and_then(Value::as_str) {
                        Some("text") => push_message(
                            &mut messages,
                            Role::Assistant,
                            content.get("text").and_then(Value::as_str).unwrap_or(""),
                            timestamp,
                            model.clone(),
                        ),
                        Some("tool") => {
                            let text = format_opencode_tool(content);
                            push_message(
                                &mut messages,
                                Role::Tool,
                                &text,
                                timestamp,
                                model.clone(),
                            );
                        }
                        Some("file") => {
                            let text = format_opencode_file(content);
                            push_message(
                                &mut messages,
                                Role::Assistant,
                                &text,
                                timestamp,
                                model.clone(),
                            );
                        }
                        Some("patch") => {
                            let text = format_opencode_patch(content);
                            push_message(
                                &mut messages,
                                Role::Tool,
                                &text,
                                timestamp,
                                model.clone(),
                            );
                        }
                        _ => {}
                    }
                }
                if messages.len() == before
                    && let Some(error) = data.get("error")
                {
                    push_message(
                        &mut messages,
                        Role::Assistant,
                        &format!("[Assistant error] {}", json_value_text(error)),
                        timestamp,
                        model,
                    );
                }
            }
            _ => {}
        }
    }
    Ok(messages)
}

fn push_message(
    messages: &mut Vec<Message>,
    role: Role,
    content: &str,
    timestamp: Option<i64>,
    model: Option<String>,
) {
    if content.trim().is_empty() {
        return;
    }
    messages.push(Message {
        idx: messages.len(),
        role,
        content: content.to_string(),
        timestamp,
        model,
    });
}

fn format_opencode_tool(value: &Value) -> String {
    let name = value
        .get("name")
        .or_else(|| value.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let state = value.get("state").unwrap_or(&Value::Null);
    let mut sections = vec![format!("[Tool: {name}]")];
    if let Some(input) = state.get("input").filter(|value| !value.is_null()) {
        sections.push(format!("Input: {}", json_value_text(input)));
    }
    if let Some(output) = state
        .get("output")
        .or_else(|| state.get("result"))
        .filter(|value| !value.is_null())
    {
        sections.push(format!("Output: {}", json_value_text(output)));
    } else if let Some(content) = state.get("content").filter(|value| !value.is_null()) {
        let text = flatten_json_content(content);
        sections.push(format!(
            "Output: {}",
            if text.is_empty() {
                json_value_text(content)
            } else {
                text
            }
        ));
    } else if let Some(structured) = state.get("structured").filter(|value| !value.is_null()) {
        sections.push(format!("Output: {}", json_value_text(structured)));
    } else if let Some(error) = state.get("error").filter(|value| !value.is_null()) {
        sections.push(format!("Error: {}", json_value_text(error)));
    }
    sections.join("\n")
}

fn format_v2_user_message(data: &Value) -> String {
    let mut sections = Vec::new();
    if let Some(text) = data.get("text").and_then(Value::as_str)
        && !text.trim().is_empty()
    {
        sections.push(text.to_string());
    }
    sections.extend(
        data.get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(format_opencode_file),
    );
    sections.join("\n")
}

fn format_opencode_file(value: &Value) -> String {
    let name = value
        .get("filename")
        .or_else(|| value.get("name"))
        .or_else(|| value.pointer("/source/path"))
        .or_else(|| value.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("unnamed");
    let mime = value.get("mime").and_then(Value::as_str);
    let mut sections = vec![match mime {
        Some(mime) => format!("[Attachment: {name} ({mime})]"),
        None => format!("[Attachment: {name}]"),
    }];
    if let Some(reference) = value.pointer("/source/text/value").and_then(Value::as_str)
        && !reference.trim().is_empty()
    {
        sections.push(reference.to_string());
    }
    if mime.is_some_and(|mime| mime.starts_with("text/") || mime == "application/json")
        && let Some(encoded) = value.get("data").and_then(Value::as_str)
        && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded)
        && let Ok(text) = String::from_utf8(bytes)
        && !text.trim().is_empty()
    {
        sections.push(text);
    }
    sections.join("\n")
}

fn format_opencode_patch(value: &Value) -> String {
    let files = value
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(json_value_text)
        .collect::<Vec<_>>();
    if files.is_empty() {
        "[Patch]".to_string()
    } else {
        format!("[Patch]\n{}", files.join("\n"))
    }
}

fn json_value_text(value: &Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn parse_model(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| message_model(&value))
        .or_else(|| Some(raw.to_string()))
}

fn message_model(data: &Value) -> Option<String> {
    data.pointer("/model/id")
        .or_else(|| data.pointer("/model/modelID"))
        .or_else(|| data.get("modelID"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn derive_title(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .find(|message| message.role == Role::User)
        .map(|message| {
            crate::model::truncate_title(
                message.content.lines().next().unwrap_or(&message.content),
                100,
            )
        })
}

fn database_fingerprint(
    session: &DatabaseSession,
    title: Option<&str>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    messages: &[Message],
) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"opencode:2\0");
    hasher.update(
        serde_json::to_string(&(
            &session.id,
            &session.directory,
            title,
            started_at,
            ended_at,
            messages,
        ))?
        .as_bytes(),
    );
    Ok(hasher.finalize().to_hex().to_string())
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

    let data: PartJson = parse_json_repairing_surrogates(&content)
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

fn parse_json_repairing_surrogates<T: DeserializeOwned>(content: &str) -> serde_json::Result<T> {
    match serde_json::from_str(content) {
        Ok(value) => Ok(value),
        Err(original_error) => {
            let repaired = replace_unpaired_surrogates(content);
            serde_json::from_str(&repaired).map_err(|_| original_error)
        }
    }
}

fn replace_unpaired_surrogates(content: &str) -> String {
    let bytes = content.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes.get(index..index + 2) == Some(br"\\") {
            output.extend_from_slice(br"\\");
            index += 2;
            continue;
        }
        let Some(codepoint) = unicode_escape_at(bytes, index) else {
            output.push(bytes[index]);
            index += 1;
            continue;
        };
        if (0xD800..=0xDBFF).contains(&codepoint)
            && unicode_escape_at(bytes, index + 6)
                .is_some_and(|next| (0xDC00..=0xDFFF).contains(&next))
        {
            output.extend_from_slice(&bytes[index..index + 12]);
            index += 12;
        } else if (0xD800..=0xDFFF).contains(&codepoint) {
            output.extend_from_slice(br"\uFFFD");
            index += 6;
        } else {
            output.extend_from_slice(&bytes[index..index + 6]);
            index += 6;
        }
    }
    String::from_utf8(output).expect("repair preserves UTF-8")
}

fn unicode_escape_at(bytes: &[u8], index: usize) -> Option<u16> {
    let escape = bytes.get(index..index + 6)?;
    if &escape[..2] != br"\u" {
        return None;
    }
    std::str::from_utf8(&escape[2..])
        .ok()
        .and_then(|digits| u16::from_str_radix(digits, 16).ok())
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
    fn test_parse_part_file_repairs_unpaired_surrogate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("part_surrogate.json");
        fs::write(
            &path,
            r#"{"id":"p3","messageID":"m1","type":"tool","tool":"websearch","state":{"output":"broken \ud83e escape"}}"#,
        )
        .unwrap();

        let part = parse_part_file(&path).unwrap().unwrap();
        assert_eq!(
            part.data.pointer("/state/output").unwrap(),
            "broken � escape"
        );
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
            data_root: None,
            db_override: None,
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
            data_root: None,
            db_override: None,
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
            data_root: None,
            db_override: None,
        };

        let conversations = connector.scan(&[dir.path().to_path_buf()], None).unwrap();
        assert!(conversations.is_empty()); // No messages → no conversation
    }

    #[test]
    fn test_connector_detect_nonexistent() {
        let connector = OpenCodeConnector {
            storage_root: Some(PathBuf::from("/nonexistent/storage")),
            data_root: None,
            db_override: None,
        };
        assert!(!connector.detect());
    }

    #[test]
    fn test_connector_default_roots() {
        let connector = OpenCodeConnector {
            storage_root: Some(PathBuf::from("/test/storage")),
            data_root: None,
            db_override: None,
        };
        let roots = connector.default_roots();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from("/test/storage"));
    }

    #[test]
    fn test_scan_late_v1_sqlite() {
        let root = TempDir::new().unwrap();
        let connection = Connection::open(root.path().join("opencode.db")).unwrap();
        connection
            .execute_batch(
                r#"CREATE TABLE session (
                    id TEXT PRIMARY KEY, directory TEXT NOT NULL, title TEXT NOT NULL,
                    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, model TEXT
                );
                CREATE TABLE message (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL, data TEXT NOT NULL
                );
                CREATE TABLE part (
                    id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
                    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL
                );
                INSERT INTO session VALUES ('sqlite-v1', '/tmp/v1', 'SQLite v1', 1000, 3000, NULL);
                INSERT INTO message VALUES
                    ('m1', 'sqlite-v1', 1000, 1000, '{"role":"user","time":{"created":1000},"model":{"modelID":"gpt-v1"}}'),
                    ('m2', 'sqlite-v1', 2000, 2000, '{"role":"assistant","time":{"created":2000},"modelID":"gpt-v1"}');
                INSERT INTO part VALUES
                    ('p1', 'm1', 'sqlite-v1', 1000, 1000, '{"type":"text","text":"v1 user text"}'),
                    ('p2', 'm2', 'sqlite-v1', 2000, 2000, '{"type":"text","text":"v1 assistant text"}'),
                    ('p3', 'm2', 'sqlite-v1', 2100, 2100, '{"type":"tool","tool":"read","state":{"status":"completed","input":{"path":"a"},"output":"done"}}'),
                    ('p4', 'm1', 'sqlite-v1', 1100, 1100, '{"type":"file","mime":"text/plain","filename":"notes.txt","source":{"text":{"value":"@notes.txt"}}}'),
                    ('p5', 'm2', 'sqlite-v1', 2200, 2200, '{"type":"patch","files":["/tmp/v1/src/main.rs"]}');"#,
            )
            .unwrap();
        drop(connection);
        let connector = OpenCodeConnector {
            storage_root: None,
            data_root: Some(root.path().to_path_buf()),
            db_override: None,
        };

        let conversations = connector.scan(&connector.default_roots(), None).unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].external_id.as_deref(), Some("sqlite-v1"));
        assert_eq!(conversations[0].messages.len(), 5);
        assert!(conversations[0].full_text().contains("Output: done"));
        assert!(conversations[0].full_text().contains("notes.txt"));
        assert!(conversations[0].full_text().contains("src/main.rs"));
        assert!(
            connector
                .source_exists(&conversations[0].source_path)
                .unwrap()
        );
    }

    #[test]
    fn test_scan_v2_projected_messages() {
        let root = TempDir::new().unwrap();
        let connection = Connection::open(root.path().join("opencode-next.db")).unwrap();
        connection
            .execute_batch(
                r#"CREATE TABLE session (
                    id TEXT PRIMARY KEY, directory TEXT NOT NULL, title TEXT NOT NULL,
                    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, model TEXT
                );
                CREATE TABLE session_message (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL,
                    seq INTEGER NOT NULL, time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL, data TEXT NOT NULL
                );
                INSERT INTO session VALUES
                    ('sqlite-v2', '/tmp/v2', 'SQLite v2', 1000, 4000, '{"id":"session-model","providerID":"test"}');
                INSERT INTO session_message VALUES
                    ('u1', 'sqlite-v2', 'user', 1, 1000, 1000, '{"time":{"created":1000},"text":"v2 user text","files":[{"name":"brief.txt","mime":"text/plain","data":"YXR0YWNobWVudCB0ZXh0"}]}'),
                    ('a1', 'sqlite-v2', 'assistant', 2, 2000, 3000,
                     '{"time":{"created":2000},"model":{"id":"gpt-v2","providerID":"test"},"content":[{"type":"text","text":"v2 assistant text"},{"type":"reasoning","text":"hidden"},{"type":"tool","name":"shell","state":{"status":"completed","input":{"command":"pwd"},"output":"/tmp/v2"}}]}');"#,
            )
            .unwrap();
        drop(connection);
        let connector = OpenCodeConnector {
            storage_root: None,
            data_root: Some(root.path().to_path_buf()),
            db_override: None,
        };

        let conversations = connector.scan(&connector.default_roots(), None).unwrap();
        assert_eq!(conversations.len(), 1);
        let conversation = &conversations[0];
        assert_eq!(conversation.external_id.as_deref(), Some("sqlite-v2"));
        assert_eq!(conversation.messages.len(), 3);
        assert!(conversation.messages[0].content.contains("brief.txt"));
        assert!(conversation.messages[0].content.contains("attachment text"));
        assert_eq!(conversation.messages[1].model.as_deref(), Some("gpt-v2"));
        assert_eq!(conversation.messages[2].role, Role::Tool);
        assert!(!conversation.full_text().contains("hidden"));
    }
}
