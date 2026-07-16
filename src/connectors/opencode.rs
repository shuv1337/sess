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
    Connector, ConnectorScan, database_source_path, file_modified_since, flatten_json_content,
    json_f64, json_u64, normalized_token_total, parse_database_source_path, source_file,
};
use crate::model::{
    Agent, Conversation, Message, Role, SourceFile, UsageRecord, source_fingerprint,
};

const PARSER_REVISION: &str = "5";

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

    fn scan(&self, roots: &[PathBuf], since_ts: Option<i64>) -> Result<ConnectorScan> {
        scan_opencode_roots(roots, since_ts)
    }

    fn parser_revision(&self) -> Option<&'static str> {
        Some(PARSER_REVISION)
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

fn scan_opencode_roots(roots: &[PathBuf], since_ts: Option<i64>) -> Result<ConnectorScan> {
    #[derive(Default)]
    struct Family {
        legacy_roots: Vec<PathBuf>,
        databases: Vec<PathBuf>,
    }

    let mut families: HashMap<PathBuf, Family> = HashMap::new();

    for root in roots {
        let virtual_root = if root.is_file() {
            root.parent().unwrap_or(Path::new(".")).to_path_buf()
        } else if root.file_name().is_some_and(|name| name == "storage") {
            root.parent().unwrap_or(root).to_path_buf()
        } else {
            root.clone()
        };
        let family = families.entry(virtual_root.clone()).or_default();
        if let Some(storage) = legacy_storage_root(root) {
            family.legacy_roots.push(storage);
        }
        if let Some(storage) = legacy_storage_root(&virtual_root) {
            family.legacy_roots.push(storage);
        }
        family.databases.extend(discover_databases(&virtual_root));
    }
    let mut families = families.into_iter().collect::<Vec<_>>();
    families.sort_by(|left, right| left.0.cmp(&right.0));
    for (_, family) in &mut families {
        family.legacy_roots.sort();
        family.legacy_roots.dedup();
        family.databases.sort();
        family.databases.dedup();
    }

    let all_legacy_roots = families
        .iter()
        .flat_map(|(_, family)| family.legacy_roots.iter().cloned())
        .collect::<Vec<_>>();
    let legacy_paths = inventory_legacy_sources(&all_legacy_roots);
    let database_count = families
        .iter()
        .map(|(_, family)| family.databases.len())
        .sum::<usize>();
    let mut by_session: HashMap<String, Conversation> = HashMap::new();
    let mut parse_errors = 0usize;

    for (virtual_root, family) in families {
        let family_changed = since_ts.is_none()
            || family
                .legacy_roots
                .iter()
                .any(|storage| legacy_storage_modified_since(storage, since_ts))
            || family
                .databases
                .iter()
                .any(|database| database_modified_since(database, since_ts));
        if !family_changed {
            continue;
        }

        for storage in &family.legacy_roots {
            match scan_legacy_storage(storage, None) {
                Ok((conversations, complete)) => {
                    if !complete {
                        parse_errors += 1;
                    }
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

        for database in family.databases {
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
    Ok(ConnectorScan::new(conversations, parse_errors == 0))
}

fn legacy_storage_modified_since(storage: &Path, since_ts: Option<i64>) -> bool {
    if since_ts.is_none() {
        return true;
    }
    for entry in WalkDir::new(storage).follow_links(true) {
        match entry {
            Ok(entry) if file_modified_since(entry.path(), since_ts) => return true,
            Ok(_) => {}
            Err(_) => return true,
        }
    }
    false
}

fn merge_conversation(conversations: &mut HashMap<String, Conversation>, candidate: Conversation) {
    let Some(id) = candidate.external_id.clone() else {
        return;
    };
    let Some(existing) = conversations.get_mut(&id) else {
        conversations.insert(id, candidate);
        return;
    };

    existing.source_path = canonical_source_path(&existing.source_path, &candidate.source_path);
    existing.source_files.extend(candidate.source_files);
    normalize_source_files(&mut existing.source_files);

    existing.started_at = match (existing.started_at, candidate.started_at) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    };
    existing.ended_at = match (existing.ended_at, candidate.ended_at) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    };
    existing.title = preferred_text(existing.title.take(), candidate.title);
    existing.workspace = preferred_path(existing.workspace.take(), candidate.workspace);

    existing.messages.extend(candidate.messages);
    normalize_messages(&mut existing.messages);
    existing.usage.extend(candidate.usage);
    normalize_usage(&mut existing.usage);
    existing.source_fingerprint = conversation_fingerprint(existing);
}

fn canonical_source_path(left: &Path, right: &Path) -> PathBuf {
    let left_exists = left.try_exists().unwrap_or(false);
    let right_exists = right.try_exists().unwrap_or(false);
    match (left_exists, right_exists) {
        (true, false) => left.to_path_buf(),
        (false, true) => right.to_path_buf(),
        _ => std::cmp::min(left.to_path_buf(), right.to_path_buf()),
    }
}

fn preferred_text(left: Option<String>, right: Option<String>) -> Option<String> {
    [left, right]
        .into_iter()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .max_by(|left, right| (left.len(), left).cmp(&(right.len(), right)))
}

fn preferred_path(left: Option<PathBuf>, right: Option<PathBuf>) -> Option<PathBuf> {
    [left, right].into_iter().flatten().max_by(|left, right| {
        let left = left.to_string_lossy();
        let right = right.to_string_lossy();
        (left.len(), left.as_ref()).cmp(&(right.len(), right.as_ref()))
    })
}

fn normalize_source_files(source_files: &mut Vec<SourceFile>) {
    source_files.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| right.mtime.cmp(&left.mtime))
            .then_with(|| right.size.cmp(&left.size))
    });
    source_files.dedup_by(|left, right| left.path == right.path);
}

fn normalize_messages(messages: &mut Vec<Message>) {
    messages.sort_by(|left, right| {
        (
            left.timestamp,
            left.role.as_str(),
            left.model.as_deref(),
            left.content.as_str(),
        )
            .cmp(&(
                right.timestamp,
                right.role.as_str(),
                right.model.as_deref(),
                right.content.as_str(),
            ))
    });
    messages.dedup_by(|left, right| {
        left.timestamp == right.timestamp
            && left.role == right.role
            && left.model == right.model
            && left.content == right.content
    });
    for (index, message) in messages.iter_mut().enumerate() {
        message.idx = index;
    }
}

fn normalize_usage(usage: &mut Vec<UsageRecord>) {
    let mut identified: HashMap<String, UsageRecord> = HashMap::new();
    let mut anonymous = Vec::new();
    for record in usage.drain(..) {
        if let Some(identity) = record
            .source_event_id
            .as_deref()
            .map(str::trim)
            .filter(|identity| !identity.is_empty())
        {
            identified
                .entry(identity.to_string())
                .and_modify(|existing| merge_duplicate_usage(existing, &record))
                .or_insert(record);
        } else if !anonymous
            .iter()
            .any(|existing| usage_equal(existing, &record))
        {
            anonymous.push(record);
        }
    }
    usage.extend(identified.into_values());
    usage.extend(anonymous);
    usage.sort_by(usage_cmp);
}

fn merge_duplicate_usage(existing: &mut UsageRecord, candidate: &UsageRecord) {
    existing.timestamp = match (existing.timestamp, candidate.timestamp) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    };
    existing.provider = preferred_text(existing.provider.take(), candidate.provider.clone());
    existing.model = preferred_text(existing.model.take(), candidate.model.clone());
    existing.api_calls = existing.api_calls.max(candidate.api_calls);
    existing.input_tokens = existing.input_tokens.max(candidate.input_tokens);
    existing.output_tokens = existing.output_tokens.max(candidate.output_tokens);
    existing.cache_read_tokens = existing.cache_read_tokens.max(candidate.cache_read_tokens);
    existing.cache_write_tokens = existing
        .cache_write_tokens
        .max(candidate.cache_write_tokens);
    existing.reasoning_tokens = existing.reasoning_tokens.max(candidate.reasoning_tokens);
    let merged_component_total = existing
        .input_tokens
        .saturating_add(existing.output_tokens)
        .saturating_add(existing.cache_read_tokens)
        .saturating_add(existing.cache_write_tokens);
    existing.total_tokens = existing
        .total_tokens
        .max(candidate.total_tokens)
        .max(merged_component_total);
    existing.actual_cost_usd = preferred_cost(existing.actual_cost_usd, candidate.actual_cost_usd);
    existing.estimated_cost_usd =
        preferred_cost(existing.estimated_cost_usd, candidate.estimated_cost_usd);
}

fn preferred_cost(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(if left.total_cmp(&right).is_ge() {
            left
        } else {
            right
        }),
        (left, right) => left.or(right),
    }
}

fn usage_equal(left: &UsageRecord, right: &UsageRecord) -> bool {
    left.timestamp == right.timestamp
        && left.provider == right.provider
        && left.model == right.model
        && left.api_calls == right.api_calls
        && left.input_tokens == right.input_tokens
        && left.output_tokens == right.output_tokens
        && left.cache_read_tokens == right.cache_read_tokens
        && left.cache_write_tokens == right.cache_write_tokens
        && left.reasoning_tokens == right.reasoning_tokens
        && left.total_tokens == right.total_tokens
        && option_f64_bits(left.actual_cost_usd) == option_f64_bits(right.actual_cost_usd)
        && option_f64_bits(left.estimated_cost_usd) == option_f64_bits(right.estimated_cost_usd)
}

fn option_f64_bits(value: Option<f64>) -> Option<u64> {
    value.map(f64::to_bits)
}

fn usage_cmp(left: &UsageRecord, right: &UsageRecord) -> std::cmp::Ordering {
    (
        (left.timestamp.is_none(), left.timestamp.unwrap_or_default()),
        left.source_event_id.as_deref(),
        left.provider.as_deref(),
        left.model.as_deref(),
        (
            left.api_calls,
            left.input_tokens,
            left.output_tokens,
            left.cache_read_tokens,
            left.cache_write_tokens,
            left.reasoning_tokens,
            left.total_tokens,
        ),
        option_f64_bits(left.actual_cost_usd),
        option_f64_bits(left.estimated_cost_usd),
    )
        .cmp(&(
            (
                right.timestamp.is_none(),
                right.timestamp.unwrap_or_default(),
            ),
            right.source_event_id.as_deref(),
            right.provider.as_deref(),
            right.model.as_deref(),
            (
                right.api_calls,
                right.input_tokens,
                right.output_tokens,
                right.cache_read_tokens,
                right.cache_write_tokens,
                right.reasoning_tokens,
                right.total_tokens,
            ),
            option_f64_bits(right.actual_cost_usd),
            option_f64_bits(right.estimated_cost_usd),
        ))
}

fn conversation_fingerprint(conversation: &Conversation) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(format!("opencode:{PARSER_REVISION}\0").as_bytes());
    hasher.update(
        &serde_json::to_vec(&(
            &conversation.external_id,
            &conversation.title,
            &conversation.workspace,
            conversation.started_at,
            conversation.ended_at,
            &conversation.messages,
            &conversation.usage,
        ))
        .expect("OpenCode normalized conversations are serializable"),
    );
    hasher.finalize().to_hex().to_string()
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

fn scan_legacy_storage(
    storage_root: &Path,
    since_ts: Option<i64>,
) -> Result<(Vec<Conversation>, bool)> {
    let session_dir = storage_root.join("session");
    if !session_dir.is_dir() {
        return Ok((Vec::new(), true));
    }
    let (sessions, sessions_complete) = load_sessions_with_status(&session_dir, since_ts)?;
    if sessions.is_empty() {
        return Ok((Vec::new(), sessions_complete));
    }
    let (message_map, session_messages, messages_complete) =
        load_messages_with_status(&storage_root.join("message"), &sessions)?;
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

    let conversations = sessions
        .into_par_iter()
        .map(
            |(session_id, session)| -> Result<(Option<Conversation>, bool)> {
                let mut complete = true;
                let mut messages = Vec::new();
                let usage = session_messages
                    .get(&session_id)
                    .into_iter()
                    .flatten()
                    .filter_map(|message_id| message_map.get(message_id))
                    .filter_map(|metadata| metadata.usage.clone())
                    .collect::<Vec<_>>();
                let mut all_source_files = session_sources
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_default();
                for message_id in session_messages.get(&session_id).into_iter().flatten() {
                    let Some(metadata) = message_map.get(message_id) else {
                        continue;
                    };
                    let message_part_dir = part_dir.join(message_id);
                    let (parts, parts_complete) = load_parts_with_status(&message_part_dir)?;
                    complete &= parts_complete;
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
                        if let Some(content) = content.filter(|content| !content.trim().is_empty())
                        {
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
                if messages.is_empty() && usage.is_empty() {
                    return Ok((None, complete));
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
                let fingerprint = format!(
                    "opencode-v{PARSER_REVISION}:{}",
                    source_fingerprint(&all_source_files)
                );
                Ok((
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
                        usage,
                    }),
                    complete,
                ))
            },
        )
        .collect::<Result<Vec<_>>>()?;
    let complete = sessions_complete
        && messages_complete
        && conversations.iter().all(|(_, complete)| *complete);
    Ok((
        conversations
            .into_iter()
            .filter_map(|(conversation, _)| conversation)
            .collect(),
        complete,
    ))
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

fn optional_table_column<'a>(columns: &HashSet<String>, name: &'a str) -> &'a str {
    if columns.contains(name) { name } else { "NULL" }
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
    let cost = optional_table_column(&columns, "cost");
    let tokens_input = optional_table_column(&columns, "tokens_input");
    let tokens_output = optional_table_column(&columns, "tokens_output");
    let tokens_reasoning = optional_table_column(&columns, "tokens_reasoning");
    let tokens_cache_read = optional_table_column(&columns, "tokens_cache_read");
    let tokens_cache_write = optional_table_column(&columns, "tokens_cache_write");
    let query = format!(
        "SELECT id, directory, title, time_created, time_updated, {model}, \
                {cost}, {tokens_input}, {tokens_output}, {tokens_reasoning}, \
                {tokens_cache_read}, {tokens_cache_write} \
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
            cost: row.get(6)?,
            input_tokens: row.get(7)?,
            output_tokens: row.get(8)?,
            reasoning_tokens: row.get(9)?,
            cache_read_tokens: row.get(10)?,
            cache_write_tokens: row.get(11)?,
        })
    })?;

    let mut conversations = Vec::new();
    for row in rows {
        let session = row?;
        let mut messages = Vec::new();
        let mut usage = Vec::new();
        if has_v1 {
            let (v1_messages, v1_usage) = load_v1_messages(&connection, &session)?;
            messages.extend(v1_messages);
            usage.extend(v1_usage);
        }
        if has_v2 {
            let (v2_messages, v2_usage) = load_v2_messages(&connection, &session)?;
            messages.extend(v2_messages);
            usage.extend(v2_usage);
        }
        normalize_messages(&mut messages);
        normalize_usage(&mut usage);
        append_session_usage_residual(&session, &mut usage);
        normalize_usage(&mut usage);
        if messages.is_empty() && usage.is_empty() {
            continue;
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
        let fingerprint = database_fingerprint(
            &session,
            title.as_deref(),
            started_at,
            ended_at,
            &messages,
            &usage,
        )?;
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
            usage,
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
    cost: Option<f64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
}

fn append_session_usage_residual(session: &DatabaseSession, usage: &mut Vec<UsageRecord>) {
    let nonnegative = |value: Option<i64>| value.unwrap_or_default().max(0) as u64;
    let input = nonnegative(session.input_tokens);
    let visible_output = nonnegative(session.output_tokens);
    let reasoning = nonnegative(session.reasoning_tokens);
    let output = visible_output.saturating_add(reasoning);
    let cache_read = nonnegative(session.cache_read_tokens);
    let cache_write = nonnegative(session.cache_write_tokens);
    let aggregate_total = input
        .saturating_add(output)
        .saturating_add(cache_read)
        .saturating_add(cache_write);
    let aggregate_cost = session.cost.filter(|cost| cost.is_finite() && *cost > 0.0);
    if aggregate_total == 0 && aggregate_cost.is_none() {
        return;
    }

    let sum = |value: fn(&UsageRecord) -> u64| {
        usage
            .iter()
            .fold(0_u64, |total, row| total.saturating_add(value(row)))
    };
    let projected_input = sum(|row| row.input_tokens);
    let projected_output = sum(|row| row.output_tokens);
    let projected_reasoning = sum(|row| row.reasoning_tokens);
    let projected_cache_read = sum(|row| row.cache_read_tokens);
    let projected_cache_write = sum(|row| row.cache_write_tokens);
    let projected_total = sum(|row| row.total_tokens);
    let projected_cost = usage
        .iter()
        .filter_map(|row| row.actual_cost_usd.or(row.estimated_cost_usd))
        .filter(|cost| cost.is_finite() && *cost > 0.0)
        .sum::<f64>();

    let residual_cost = aggregate_cost
        .map(|cost| (cost - projected_cost).max(0.0))
        .filter(|cost| *cost > f64::EPSILON);
    let residual = UsageRecord {
        // These are authoritative session aggregates, not event-exact rows.
        timestamp: None,
        provider: parse_provider(session.model.as_deref()),
        model: parse_model(session.model.as_deref()),
        source_event_id: Some(format!("opencode-session-aggregate:{}", session.id)),
        api_calls: 0,
        input_tokens: input.saturating_sub(projected_input),
        output_tokens: output.saturating_sub(projected_output),
        cache_read_tokens: cache_read.saturating_sub(projected_cache_read),
        cache_write_tokens: cache_write.saturating_sub(projected_cache_write),
        reasoning_tokens: reasoning.saturating_sub(projected_reasoning),
        total_tokens: aggregate_total.saturating_sub(projected_total),
        actual_cost_usd: None,
        estimated_cost_usd: residual_cost,
    };
    if residual.has_usage() {
        usage.push(residual);
    }
}

fn load_v1_messages(
    connection: &Connection,
    session: &DatabaseSession,
) -> Result<(Vec<Message>, Vec<UsageRecord>)> {
    let mut statement = connection.prepare(
        "SELECT m.id, m.time_created, m.data, p.data \
         FROM message m LEFT JOIN part p ON p.message_id = m.id \
         WHERE m.session_id = ? ORDER BY m.time_created, m.id, p.time_created, p.id",
    )?;
    let rows = statement.query_map([&session.id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let fallback_model = parse_model(session.model.as_deref());
    let fallback_provider = parse_provider(session.model.as_deref());
    let mut messages = Vec::new();
    let mut usage = Vec::new();
    let mut usage_messages = HashSet::new();
    for row in rows {
        let (message_id, created_at, message_json, part_json) = row?;
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
        if role == Role::Assistant
            && usage_messages.insert(message_id.clone())
            && let Some(record) = opencode_usage_record(
                &message_data,
                timestamp,
                fallback_provider.clone(),
                fallback_model.clone(),
                Some(opencode_usage_event_id(&session.id, &message_id)),
            )
        {
            usage.push(record);
        }
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
    Ok((messages, usage))
}

fn load_v2_messages(
    connection: &Connection,
    session: &DatabaseSession,
) -> Result<(Vec<Message>, Vec<UsageRecord>)> {
    let mut statement = connection.prepare(
        "SELECT id, type, time_created, data FROM session_message \
         WHERE session_id = ? ORDER BY seq, id",
    )?;
    let rows = statement.query_map([&session.id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let fallback_model = parse_model(session.model.as_deref());
    let fallback_provider = parse_provider(session.model.as_deref());
    let mut messages = Vec::new();
    let mut usage = Vec::new();
    for row in rows {
        let (message_id, message_type, created_at, raw_data) = row?;
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
                if let Some(record) = opencode_usage_record(
                    &data,
                    timestamp,
                    fallback_provider.clone(),
                    fallback_model.clone(),
                    Some(opencode_usage_event_id(&session.id, &message_id)),
                ) {
                    usage.push(record);
                }
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
    Ok((messages, usage))
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

fn opencode_usage_record(
    data: &Value,
    timestamp: Option<i64>,
    fallback_provider: Option<String>,
    fallback_model: Option<String>,
    source_event_id: Option<String>,
) -> Option<UsageRecord> {
    let tokens = data.get("tokens").or_else(|| data.get("usage"))?;
    let input = json_u64(tokens, &["/input", "/input_tokens"]);
    let visible_output = json_u64(tokens, &["/output", "/output_tokens"]);
    let reasoning = json_u64(tokens, &["/reasoning", "/reasoning_tokens"]);
    let output = visible_output.saturating_add(reasoning);
    let cache_read = json_u64(tokens, &["/cache/read", "/cacheRead"]);
    let cache_write = json_u64(tokens, &["/cache/write", "/cacheWrite"]);
    let record = UsageRecord {
        timestamp,
        provider: data
            .pointer("/model/providerID")
            .or_else(|| data.get("providerID"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(fallback_provider),
        model: message_model(data).or(fallback_model),
        source_event_id,
        api_calls: 1,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: reasoning,
        total_tokens: normalized_token_total(
            json_u64(tokens, &["/total", "/totalTokens", "/total_tokens"]),
            input,
            output,
            cache_read,
            cache_write,
        ),
        actual_cost_usd: None,
        estimated_cost_usd: json_f64(data, &["/cost", "/cost/total", "/cost_usd"]),
    };
    record.has_usage().then_some(record)
}

fn opencode_usage_event_id(session_id: &str, message_id: &str) -> String {
    format!("opencode-message:{session_id}:{message_id}")
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

fn parse_provider(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(raw).ok().and_then(|value| {
        value
            .get("providerID")
            .or_else(|| value.pointer("/model/providerID"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
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
    usage: &[UsageRecord],
) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(format!("opencode:{PARSER_REVISION}\0").as_bytes());
    hasher.update(
        serde_json::to_string(&(
            &session.id,
            &session.directory,
            title,
            started_at,
            ended_at,
            messages,
            usage,
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
    usage: Option<UsageRecord>,
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

#[cfg(test)]
fn load_sessions(
    session_dir: &Path,
    since_ts: Option<i64>,
) -> Result<HashMap<String, SessionMeta>> {
    Ok(load_sessions_with_status(session_dir, since_ts)?.0)
}

fn load_sessions_with_status(
    session_dir: &Path,
    since_ts: Option<i64>,
) -> Result<(HashMap<String, SessionMeta>, bool)> {
    let mut sessions = HashMap::new();
    let mut complete = true;

    for entry in WalkDir::new(session_dir).follow_links(true) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                complete = false;
                tracing::warn!(
                    root = %session_dir.display(),
                    error = %error,
                    "Failed to traverse OpenCode session storage"
                );
                continue;
            }
        };
        if !entry.file_type().is_file()
            || entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "json")
        {
            continue;
        }
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
                complete = false;
            }
        }
    }

    Ok((sessions, complete))
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

type MessageLoad = (
    HashMap<String, MessageMeta>,
    HashMap<String, Vec<String>>,
    bool,
);

fn load_messages_with_status(
    message_dir: &Path,
    sessions: &HashMap<String, SessionMeta>,
) -> Result<MessageLoad> {
    let mut message_map = HashMap::new();
    let mut session_messages: HashMap<String, Vec<String>> = HashMap::new();
    let mut complete = true;

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
                    complete = false;
                }
            }
        }
    }

    Ok((message_map, session_messages, complete))
}

#[derive(Deserialize)]
struct MessageJson {
    id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    role: String,
    #[serde(default)]
    time: Option<MessageTime>,
}

#[derive(Deserialize)]
struct MessageTime {
    #[serde(default)]
    created: Option<i64>,
}

fn parse_message_file(path: &Path) -> Result<Option<MessageMeta>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;

    let raw: Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let data: MessageJson = serde_json::from_value(raw.clone())
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

    let timestamp = data.time.as_ref().and_then(|t| t.created);
    let model = message_model(&raw);
    let usage = (role == Role::Assistant)
        .then(|| {
            opencode_usage_record(
                &raw,
                timestamp,
                None,
                model.clone(),
                Some(opencode_usage_event_id(&data.session_id, &data.id)),
            )
        })
        .flatten();

    Ok(Some(MessageMeta {
        id: data.id,
        session_id: data.session_id,
        role,
        created_at: timestamp,
        model,
        usage,
        source_file,
    }))
}

#[cfg(test)]
fn load_parts(part_dir: &Path) -> Result<Vec<PartMeta>> {
    Ok(load_parts_with_status(part_dir)?.0)
}

fn load_parts_with_status(part_dir: &Path) -> Result<(Vec<PartMeta>, bool)> {
    let mut parts = Vec::new();
    let mut complete = true;

    if !part_dir.is_dir() {
        return Ok((parts, true));
    }

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
                complete = false;
            }
        }
    }

    // Sort parts by ID for stable ordering
    parts.sort_by(|a, b| a.id.cmp(&b.id));

    Ok((parts, complete))
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
    fn test_parse_legacy_message_usage_from_top_level_fields() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("msg-usage.json");
        fs::write(
            &path,
            r#"{
                "id": "msg-usage",
                "sessionID": "sess1",
                "role": "assistant",
                "time": {"created": 1705312800000},
                "providerID": "openrouter",
                "modelID": "gpt-test",
                "tokens": {
                    "input": 100,
                    "output": 20,
                    "reasoning": 5,
                    "cache": {"read": 30, "write": 10},
                    "total": 165
                },
                "cost": 0.125
            }"#,
        )
        .unwrap();

        let message = parse_message_file(&path).unwrap().unwrap();
        assert_eq!(message.model.as_deref(), Some("gpt-test"));
        let usage = message.usage.as_ref().unwrap();
        assert_eq!(usage.provider.as_deref(), Some("openrouter"));
        assert_eq!(usage.model.as_deref(), Some("gpt-test"));
        assert_eq!(usage.api_calls, 1);
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 25);
        assert_eq!(usage.reasoning_tokens, 5);
        assert_eq!(usage.cache_read_tokens, 30);
        assert_eq!(usage.cache_write_tokens, 10);
        assert_eq!(usage.total_tokens, 165);
        assert_eq!(usage.estimated_cost_usd, Some(0.125));
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
    fn malformed_legacy_source_marks_scan_incomplete_but_keeps_valid_sessions() {
        let root = TempDir::new().unwrap();
        create_opencode_tree(root.path());
        fs::write(root.path().join("session/proj1/malformed.json"), "NOT JSON").unwrap();

        let scan = scan_opencode_roots(&[root.path().to_path_buf()], None).unwrap();
        assert!(!scan.complete);
        assert_eq!(scan.conversations.len(), 1);
        assert_eq!(scan.conversations[0].external_id.as_deref(), Some("sess1"));
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
                     '{"time":{"created":2000},"model":{"id":"gpt-v2","providerID":"test"},"tokens":{"input":100,"output":20,"reasoning":5,"cache":{"read":30,"write":10},"total":165},"cost":0.125,"content":[{"type":"text","text":"v2 assistant text"},{"type":"reasoning","text":"hidden"},{"type":"tool","name":"shell","state":{"status":"completed","input":{"command":"pwd"},"output":"/tmp/v2"}}]}');"#,
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
        assert_eq!(conversation.usage.len(), 1);
        let usage = &conversation.usage[0];
        assert_eq!(usage.provider.as_deref(), Some("test"));
        assert_eq!(usage.model.as_deref(), Some("gpt-v2"));
        assert_eq!(usage.output_tokens, 25);
        assert_eq!(usage.reasoning_tokens, 5);
        assert_eq!(usage.total_tokens, 165);
        assert_eq!(usage.estimated_cost_usd, Some(0.125));
    }

    #[test]
    fn v2_usage_only_session_is_preserved() {
        let root = TempDir::new().unwrap();
        let connection = Connection::open(root.path().join("usage-only.db")).unwrap();
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
                    ('usage-only', '/tmp/v2', 'Usage only', 1000, 2000, NULL);
                INSERT INTO session_message VALUES
                    ('a1', 'usage-only', 'assistant', 1, 1000, 2000,
                     '{"time":{"created":1000},"model":{"id":"gpt-v2","providerID":"test"},"tokens":{"input":10,"output":5},"content":[]}');"#,
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
        assert!(conversations[0].messages.is_empty());
        assert_eq!(conversations[0].usage.len(), 1);
        assert_eq!(conversations[0].usage[0].total_tokens, 15);
    }

    #[test]
    fn v2_session_aggregate_fills_missing_projected_usage() {
        let root = TempDir::new().unwrap();
        let connection = Connection::open(root.path().join("aggregate.db")).unwrap();
        connection
            .execute_batch(
                r#"CREATE TABLE session (
                    id TEXT PRIMARY KEY, directory TEXT NOT NULL, title TEXT NOT NULL,
                    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, model TEXT,
                    cost REAL NOT NULL DEFAULT 0,
                    tokens_input INTEGER NOT NULL DEFAULT 0,
                    tokens_output INTEGER NOT NULL DEFAULT 0,
                    tokens_reasoning INTEGER NOT NULL DEFAULT 0,
                    tokens_cache_read INTEGER NOT NULL DEFAULT 0,
                    tokens_cache_write INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE session_message (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL,
                    seq INTEGER NOT NULL, time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL, data TEXT NOT NULL
                );
                INSERT INTO session VALUES
                    ('aggregate', '/tmp/v2', 'Aggregate', 1000, 2000,
                     '{"id":"gpt-v2","providerID":"test"}',
                     0.2, 100, 20, 5, 30, 10);
                INSERT INTO session_message VALUES
                    ('a1', 'aggregate', 'assistant', 1, 1000, 2000,
                     '{"time":{"created":1000},"model":{"id":"gpt-v2","providerID":"test"},"tokens":{"input":40,"output":5,"reasoning":2,"cache":{"read":10,"write":0},"total":57},"cost":0.05,"content":[]}');"#,
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
        let usage = &conversations[0].usage;
        assert_eq!(usage.len(), 2);
        assert_eq!(usage.iter().map(|row| row.total_tokens).sum::<u64>(), 165);
        assert_eq!(usage.iter().map(|row| row.api_calls).sum::<u64>(), 1);
        assert_eq!(usage[1].timestamp, None);
        assert_eq!(usage[1].input_tokens, 60);
        assert_eq!(usage[1].output_tokens, 18);
        assert_eq!(usage[1].reasoning_tokens, 3);
        assert!((usage[1].estimated_cost_usd.unwrap() - 0.15).abs() < 1e-9);
    }

    #[test]
    fn hybrid_database_reconciles_v1_and_v2_rows_without_duplicate_usage() {
        let root = TempDir::new().unwrap();
        let database = root.path().join("hybrid.db");
        let connection = Connection::open(&database).unwrap();
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
                CREATE TABLE session_message (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL,
                    seq INTEGER NOT NULL, time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL, data TEXT NOT NULL
                );
                INSERT INTO session VALUES
                    ('hybrid', '/tmp/hybrid', 'Hybrid', 1000, 3000,
                     '{"id":"gpt-hybrid","providerID":"test"}');
                INSERT INTO message VALUES
                    ('m-user', 'hybrid', 1000, 1000,
                     '{"role":"user","time":{"created":1000},"model":{"modelID":"gpt-hybrid"}}'),
                    ('m-shared', 'hybrid', 2000, 2000,
                     '{"role":"assistant","time":{"created":2000},"model":{"modelID":"gpt-hybrid","providerID":"test"},"tokens":{"input":10,"output":5,"total":15}}');
                INSERT INTO part VALUES
                    ('p-user', 'm-user', 'hybrid', 1000, 1000,
                     '{"type":"text","text":"v1-only user"}'),
                    ('p-shared', 'm-shared', 'hybrid', 2000, 2000,
                     '{"type":"text","text":"shared answer"}');
                INSERT INTO session_message VALUES
                    ('m-shared', 'hybrid', 'assistant', 2, 2000, 2000,
                     '{"time":{"created":2000},"model":{"id":"gpt-hybrid","providerID":"test"},"tokens":{"input":10,"output":5,"total":15},"content":[{"type":"text","text":"shared answer"},{"type":"text","text":"v2-only continuation"}]}');"#,
            )
            .unwrap();
        drop(connection);

        let conversations = scan_opencode_database(&database, root.path()).unwrap();
        assert_eq!(conversations.len(), 1);
        let conversation = &conversations[0];
        assert_eq!(conversation.messages.len(), 3);
        assert!(conversation.full_text().contains("v1-only user"));
        assert!(conversation.full_text().contains("shared answer"));
        assert!(conversation.full_text().contains("v2-only continuation"));
        assert_eq!(conversation.usage.len(), 1);
        assert_eq!(conversation.usage[0].total_tokens, 15);
        assert_eq!(conversation.usage[0].api_calls, 1);
        assert_eq!(
            conversation.usage[0].source_event_id.as_deref(),
            Some("opencode-message:hybrid:m-shared")
        );
    }

    #[test]
    fn sibling_databases_union_complementary_usage_deterministically() {
        fn create_database(path: &Path, include_second_call: bool) {
            let connection = Connection::open(path).unwrap();
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
                        ('shared-usage', '/tmp/shared', 'Shared usage', 1000, 3000,
                         '{"id":"gpt-shared","providerID":"test"}');
                    INSERT INTO session_message VALUES
                        ('u1', 'shared-usage', 'user', 1, 1000, 1000,
                         '{"time":{"created":1000},"text":"same prompt"}'),
                        ('a1', 'shared-usage', 'assistant', 2, 2000, 2000,
                         '{"time":{"created":2000},"model":{"id":"gpt-shared","providerID":"test"},"tokens":{"input":10,"output":5,"total":15},"content":[{"type":"text","text":"first answer"}]}');"#,
                )
                .unwrap();
            if include_second_call {
                connection
                    .execute_batch(
                        r#"INSERT INTO session_message VALUES
                            ('a2', 'shared-usage', 'assistant', 3, 3000, 3000,
                             '{"time":{"created":3000},"model":{"id":"gpt-shared","providerID":"test"},"tokens":{"input":20,"output":7,"total":27},"content":[{"type":"text","text":"second answer"}]}');"#,
                    )
                    .unwrap();
            }
        }

        let root = TempDir::new().unwrap();
        let first = root.path().join("a.db");
        let second = root.path().join("b.db");
        create_database(&first, false);
        create_database(&second, true);

        let scan = scan_opencode_roots(&[root.path().to_path_buf()], None).unwrap();
        assert_eq!(scan.conversations.len(), 1);
        let conversation = &scan.conversations[0];
        assert_eq!(conversation.messages.len(), 3);
        assert!(conversation.full_text().contains("first answer"));
        assert!(conversation.full_text().contains("second answer"));
        assert_eq!(conversation.usage.len(), 2);
        assert_eq!(
            conversation
                .usage
                .iter()
                .map(|record| record.total_tokens)
                .sum::<u64>(),
            42
        );
        assert_eq!(conversation.source_files.len(), 2);
        assert_eq!(
            conversation.source_path,
            database_source_path(root.path(), Agent::OpenCode, "shared-usage")
        );

        let left = scan_opencode_database(&first, root.path())
            .unwrap()
            .remove(0);
        let right = scan_opencode_database(&second, root.path())
            .unwrap()
            .remove(0);
        let mut forward = HashMap::new();
        merge_conversation(&mut forward, left.clone());
        merge_conversation(&mut forward, right.clone());
        let mut reverse = HashMap::new();
        merge_conversation(&mut reverse, right);
        merge_conversation(&mut reverse, left);
        let forward = forward.remove("shared-usage").unwrap();
        let reverse = reverse.remove("shared-usage").unwrap();
        assert_eq!(
            serde_json::to_value((&forward.messages, &forward.usage)).unwrap(),
            serde_json::to_value((&reverse.messages, &reverse.usage)).unwrap()
        );
        assert_eq!(forward.source_fingerprint, reverse.source_fingerprint);
        assert_eq!(
            forward
                .source_files
                .iter()
                .map(|source| &source.path)
                .collect::<Vec<_>>(),
            reverse
                .source_files
                .iter()
                .map(|source| &source.path)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn changed_database_rescans_unchanged_legacy_family() {
        let root = TempDir::new().unwrap();
        let storage = root.path().join("storage");
        create_opencode_tree(&storage);
        for entry in WalkDir::new(&storage)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            std::fs::File::open(entry.path())
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new()
                        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)),
                )
                .unwrap();
        }

        let database = root.path().join("changed.db");
        let connection = Connection::open(&database).unwrap();
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
                    ('sess1', '/tmp/db', 'DB copy', 1000, 2000, NULL);
                INSERT INTO session_message VALUES
                    ('db-a1', 'sess1', 'assistant', 1, 2000, 2000,
                     '{"time":{"created":2000},"content":[{"type":"text","text":"changed DB content"}]}');"#,
            )
            .unwrap();
        drop(connection);

        let since = chrono::Utc::now().timestamp_millis() - 60_000;
        let scan = scan_opencode_roots(&[root.path().to_path_buf()], Some(since)).unwrap();
        assert_eq!(scan.conversations.len(), 1);
        assert!(
            scan.conversations[0]
                .full_text()
                .contains("Sure, I can help!")
        );
        assert!(
            scan.conversations[0]
                .full_text()
                .contains("changed DB content")
        );
        assert_eq!(
            scan.conversations[0].source_path,
            storage.join("session/proj1/sess1.json")
        );
    }

    #[test]
    fn sibling_session_aggregates_keep_one_conservative_residual() {
        fn create_database(path: &Path, aggregate_input: u64, aggregate_output: u64) {
            let connection = Connection::open(path).unwrap();
            connection
                .execute_batch(&format!(
                    r#"CREATE TABLE session (
                        id TEXT PRIMARY KEY, directory TEXT NOT NULL, title TEXT NOT NULL,
                        time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, model TEXT,
                        cost REAL NOT NULL DEFAULT 0, tokens_input INTEGER NOT NULL DEFAULT 0,
                        tokens_output INTEGER NOT NULL DEFAULT 0,
                        tokens_reasoning INTEGER NOT NULL DEFAULT 0,
                        tokens_cache_read INTEGER NOT NULL DEFAULT 0,
                        tokens_cache_write INTEGER NOT NULL DEFAULT 0
                    );
                    CREATE TABLE session_message (
                        id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL,
                        seq INTEGER NOT NULL, time_created INTEGER NOT NULL,
                        time_updated INTEGER NOT NULL, data TEXT NOT NULL
                    );
                    INSERT INTO session VALUES
                        ('aggregate-shared', '/tmp/shared', 'Aggregate', 1000, 2000,
                         '{{"id":"gpt-shared","providerID":"test"}}', 0,
                         {aggregate_input}, {aggregate_output}, 0, 0, 0);
                    INSERT INTO session_message VALUES
                        ('a1', 'aggregate-shared', 'assistant', 1, 1000, 1000,
                         '{{"time":{{"created":1000}},"model":{{"id":"gpt-shared","providerID":"test"}},"tokens":{{"input":10,"output":5,"total":15}},"content":[]}}');"#
                ))
                .unwrap();
        }

        let root = TempDir::new().unwrap();
        // Each sibling is more complete for a different monotonic counter. The
        // merged residual must keep the component-wise maxima and normalize
        // its total to match those merged buckets.
        create_database(&root.path().join("complete.db"), 100, 10);
        create_database(&root.path().join("partial.db"), 60, 20);
        let scan = scan_opencode_roots(&[root.path().to_path_buf()], None).unwrap();
        let usage = &scan.conversations[0].usage;
        assert_eq!(usage.len(), 2);
        assert_eq!(usage.iter().map(|row| row.total_tokens).sum::<u64>(), 120);
        assert_eq!(
            usage
                .iter()
                .filter(|row| row
                    .source_event_id
                    .as_deref()
                    .is_some_and(|id| id == "opencode-session-aggregate:aggregate-shared"))
                .count(),
            1
        );
    }

    #[test]
    fn incremental_database_scan_merges_unchanged_sibling_copies() {
        fn create_database(path: &Path, rich: bool) {
            let connection = Connection::open(path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE session (
                        id TEXT PRIMARY KEY, directory TEXT NOT NULL, title TEXT NOT NULL,
                        time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, model TEXT
                     );
                     CREATE TABLE session_message (
                        id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL,
                        seq INTEGER NOT NULL, time_created INTEGER NOT NULL,
                        time_updated INTEGER NOT NULL, data TEXT NOT NULL
                     );
                     INSERT INTO session VALUES
                        ('shared-session', '/tmp/shared', 'Shared', 1000, 2000, NULL);",
                )
                .unwrap();
            if rich {
                connection
                    .execute_batch(
                        r#"INSERT INTO session_message VALUES
                            ('u1', 'shared-session', 'user', 1, 1000, 1000,
                             '{"time":{"created":1000},"text":"richer canonical user"}'),
                            ('a1', 'shared-session', 'assistant', 2, 2000, 2000,
                             '{"time":{"created":2000},"content":[{"type":"text","text":"richer canonical answer"}]}');"#,
                    )
                    .unwrap();
            } else {
                connection
                    .execute_batch(
                        r#"INSERT INTO session_message VALUES
                            ('a1', 'shared-session', 'assistant', 1, 2000, 2000,
                             '{"time":{"created":2000},"content":[{"type":"text","text":"poorer copy"}]}');"#,
                    )
                    .unwrap();
            }
        }

        let root = TempDir::new().unwrap();
        let rich = root.path().join("rich.db");
        let poor = root.path().join("poor.db");
        create_database(&rich, true);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&rich)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)),
            )
            .unwrap();
        create_database(&poor, false);

        let connector = OpenCodeConnector {
            storage_root: None,
            data_root: Some(root.path().to_path_buf()),
            db_override: None,
        };
        let since = chrono::Utc::now().timestamp_millis() - 60_000;
        let conversations = connector
            .scan(&connector.default_roots(), Some(since))
            .unwrap();

        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].messages.len(), 3);
        assert!(
            conversations[0]
                .full_text()
                .contains("richer canonical user")
        );
        assert!(conversations[0].full_text().contains("poorer copy"));
    }
}
