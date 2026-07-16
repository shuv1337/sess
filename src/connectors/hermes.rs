use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

use crate::connectors::{
    Connector, ConnectorScan, database_source_path, file_modified_since, flatten_json_content,
    parse_database_source_path, parse_role, source_file,
};
use crate::model::{Agent, Conversation, Message, Role, SourceFile, UsageRecord};

const PARSER_REVISION: &str = "3";

pub struct HermesConnector {
    base_home: Option<PathBuf>,
}

impl HermesConnector {
    pub fn new() -> Self {
        let base_home = std::env::var_os("HERMES_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".hermes")));
        Self { base_home }
    }

    fn homes(&self) -> Vec<PathBuf> {
        let Some(base) = &self.base_home else {
            return Vec::new();
        };
        let mut homes = vec![base.clone()];
        let profiles = base.join("profiles");
        if let Ok(entries) = std::fs::read_dir(profiles) {
            homes.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir() && path.join("state.db").is_file()),
            );
        }
        homes.sort();
        homes.dedup();
        homes
    }
}

impl Default for HermesConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl Connector for HermesConnector {
    fn agent(&self) -> Agent {
        Agent::Hermes
    }

    fn detect(&self) -> bool {
        self.homes()
            .iter()
            .any(|home| home.join("state.db").is_file())
    }

    fn default_roots(&self) -> Vec<PathBuf> {
        self.homes()
    }

    fn scan(&self, roots: &[PathBuf], since_ts: Option<i64>) -> Result<ConnectorScan> {
        let mut conversations = Vec::new();
        let mut discovered = 0usize;
        let mut parsed = 0usize;
        let mut parse_errors = 0usize;

        tracing::info!(
            agent = Agent::Hermes.slug(),
            roots = roots.len(),
            "Starting Hermes session scan"
        );

        for root in roots {
            let (source_root, db_path) = if root.is_file() {
                (
                    root.parent().unwrap_or(Path::new(".")).to_path_buf(),
                    root.clone(),
                )
            } else {
                (root.clone(), root.join("state.db"))
            };
            if !db_path.is_file() || !database_modified_since(&db_path, since_ts) {
                continue;
            }

            match scan_database(&source_root, &db_path) {
                Ok(mut found) => {
                    discovered += found.len();
                    parsed += found.len();
                    conversations.append(&mut found);
                }
                Err(error) => {
                    parse_errors += 1;
                    tracing::warn!(
                        agent = Agent::Hermes.slug(),
                        root = %root.display(),
                        error = %error,
                        "Failed to scan Hermes state database"
                    );
                }
            }
        }

        tracing::info!(
            agent = Agent::Hermes.slug(),
            roots = roots.len(),
            discovered,
            parsed,
            parse_errors,
            "Completed Hermes session scan"
        );
        Ok(ConnectorScan::new(conversations, parse_errors == 0))
    }

    fn parser_revision(&self) -> Option<&'static str> {
        Some(PARSER_REVISION)
    }

    fn source_exists(&self, path: &Path) -> Result<bool> {
        let Some((root, session_id)) = parse_database_source_path(path, Agent::Hermes) else {
            return Ok(path.try_exists()?);
        };
        let db_path = root.join("state.db");
        if !db_path.is_file() {
            return Ok(false);
        }
        let connection = open_read_only(&db_path)?;
        let exists = connection
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ? LIMIT 1",
                [session_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(exists)
    }
}

fn open_read_only(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("Failed to open Hermes database {}", path.display()))?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(connection)
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn database_files(path: &Path) -> Result<Vec<SourceFile>> {
    let mut files = Vec::new();
    if let Some(file) = source_file(path) {
        files.push(file);
    }
    let wal = sidecar_path(path, "-wal");
    if let Some(file) = source_file(&wal) {
        files.push(file);
    }
    if files.is_empty() {
        anyhow::bail!(
            "Hermes database disappeared during scan: {}",
            path.display()
        );
    }
    Ok(files)
}

fn database_modified_since(path: &Path, since_ts: Option<i64>) -> bool {
    let wal = sidecar_path(path, "-wal");
    file_modified_since(path, since_ts) || (wal.exists() && file_modified_since(&wal, since_ts))
}

fn table_columns(connection: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<HashSet<_>, _>>()?;
    Ok(columns)
}

fn scan_database(root: &Path, db_path: &Path) -> Result<Vec<Conversation>> {
    let connection = open_read_only(db_path)?;
    let session_columns = table_columns(&connection, "sessions")?;
    let message_columns = table_columns(&connection, "messages")?;
    if !session_columns.contains("id") || !message_columns.contains("session_id") {
        anyhow::bail!(
            "{} does not contain the Hermes session schema",
            db_path.display()
        );
    }

    let cwd = if session_columns.contains("cwd") {
        "cwd"
    } else {
        "NULL"
    };
    let title = if session_columns.contains("title") {
        "title"
    } else {
        "NULL"
    };
    let ended_at = if session_columns.contains("ended_at") {
        "ended_at"
    } else {
        "NULL"
    };
    let input_tokens = optional_column(&session_columns, "input_tokens");
    let api_call_count = optional_column(&session_columns, "api_call_count");
    let output_tokens = optional_column(&session_columns, "output_tokens");
    let cache_read_tokens = optional_column(&session_columns, "cache_read_tokens");
    let cache_write_tokens = optional_column(&session_columns, "cache_write_tokens");
    let reasoning_tokens = optional_column(&session_columns, "reasoning_tokens");
    let billing_provider = optional_column(&session_columns, "billing_provider");
    let estimated_cost = optional_column(&session_columns, "estimated_cost_usd");
    let actual_cost = optional_column(&session_columns, "actual_cost_usd");
    let query = format!(
        "SELECT id, source, model, started_at, {ended_at}, {title}, {cwd}, \
         {api_call_count}, {input_tokens}, {output_tokens}, {cache_read_tokens}, {cache_write_tokens}, \
         {reasoning_tokens}, {billing_provider}, {estimated_cost}, {actual_cost} \
         FROM sessions ORDER BY started_at, id"
    );
    let source_files = database_files(db_path)?;
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map([], |row| {
        Ok(HermesSession {
            id: row.get(0)?,
            source: row.get(1)?,
            model: row.get(2)?,
            started_at: row.get(3)?,
            ended_at: row.get(4)?,
            title: row.get(5)?,
            cwd: row.get(6)?,
            api_calls: row.get::<_, Option<i64>>(7)?.unwrap_or(0).max(0) as u64,
            input_tokens: row.get::<_, Option<i64>>(8)?.unwrap_or(0).max(0) as u64,
            output_tokens: row.get::<_, Option<i64>>(9)?.unwrap_or(0).max(0) as u64,
            cache_read_tokens: row.get::<_, Option<i64>>(10)?.unwrap_or(0).max(0) as u64,
            cache_write_tokens: row.get::<_, Option<i64>>(11)?.unwrap_or(0).max(0) as u64,
            reasoning_tokens: row.get::<_, Option<i64>>(12)?.unwrap_or(0).max(0) as u64,
            billing_provider: row.get(13)?,
            estimated_cost_usd: row.get(14)?,
            actual_cost_usd: row.get(15)?,
        })
    })?;

    let mut conversations = Vec::new();
    for row in rows {
        let session = row?;
        let messages = load_messages(&connection, &message_columns, &session)?;
        let started_at = session.started_at.map(seconds_to_millis).or_else(|| {
            messages
                .iter()
                .filter_map(|message| message.timestamp)
                .min()
        });
        let ended_at = session.ended_at.map(seconds_to_millis).or_else(|| {
            messages
                .iter()
                .filter_map(|message| message.timestamp)
                .max()
        });
        let title = session.title.or_else(|| derive_title(&messages));
        let total_tokens = session
            .input_tokens
            .saturating_add(session.output_tokens)
            .saturating_add(session.cache_read_tokens)
            .saturating_add(session.cache_write_tokens);
        let fallback_usage = UsageRecord {
            // Session totals are aggregates, not event-exact observations.
            timestamp: None,
            provider: session.billing_provider.clone(),
            model: session.model.clone(),
            source_event_id: None,
            api_calls: session.api_calls,
            input_tokens: session.input_tokens,
            output_tokens: session.output_tokens,
            cache_read_tokens: session.cache_read_tokens,
            cache_write_tokens: session.cache_write_tokens,
            reasoning_tokens: session.reasoning_tokens,
            total_tokens,
            actual_cost_usd: session.actual_cost_usd.filter(|cost| *cost > 0.0),
            estimated_cost_usd: session.estimated_cost_usd.filter(|cost| *cost > 0.0),
        };
        let mut usage = load_model_usage(&connection, &session.id)?;
        append_usage_residual(&mut usage, fallback_usage);
        if messages.is_empty() && usage.is_empty() {
            continue;
        }
        let fingerprint = normalized_fingerprint(
            &session.id,
            &session.source,
            session.model.as_deref(),
            title.as_deref(),
            session.cwd.as_deref(),
            started_at,
            ended_at,
            &messages,
            &usage,
        )?;

        conversations.push(Conversation {
            agent: Agent::Hermes,
            external_id: Some(session.id.clone()),
            title,
            workspace: session.cwd.map(PathBuf::from),
            source_path: database_source_path(root, Agent::Hermes, &session.id),
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

fn append_usage_residual(usage: &mut Vec<UsageRecord>, aggregate: UsageRecord) {
    if !aggregate.has_usage() {
        return;
    }
    let sum_u64 = |value: fn(&UsageRecord) -> u64| {
        usage
            .iter()
            .fold(0_u64, |total, row| total.saturating_add(value(row)))
    };
    let sum_cost = |value: fn(&UsageRecord) -> Option<f64>| {
        usage
            .iter()
            .filter_map(value)
            .filter(|cost| cost.is_finite() && *cost > 0.0)
            .sum::<f64>()
    };
    let positive_residual = |total: Option<f64>, projected: f64| {
        total
            .filter(|cost| cost.is_finite() && *cost > 0.0)
            .map(|cost| (cost - projected).max(0.0))
            .filter(|cost| *cost > f64::EPSILON)
    };
    let residual = UsageRecord {
        timestamp: None,
        provider: aggregate.provider,
        model: aggregate.model,
        source_event_id: None,
        api_calls: aggregate
            .api_calls
            .saturating_sub(sum_u64(|row| row.api_calls)),
        input_tokens: aggregate
            .input_tokens
            .saturating_sub(sum_u64(|row| row.input_tokens)),
        output_tokens: aggregate
            .output_tokens
            .saturating_sub(sum_u64(|row| row.output_tokens)),
        cache_read_tokens: aggregate
            .cache_read_tokens
            .saturating_sub(sum_u64(|row| row.cache_read_tokens)),
        cache_write_tokens: aggregate
            .cache_write_tokens
            .saturating_sub(sum_u64(|row| row.cache_write_tokens)),
        reasoning_tokens: aggregate
            .reasoning_tokens
            .saturating_sub(sum_u64(|row| row.reasoning_tokens)),
        total_tokens: aggregate
            .total_tokens
            .saturating_sub(sum_u64(|row| row.total_tokens)),
        actual_cost_usd: positive_residual(
            aggregate.actual_cost_usd,
            sum_cost(|row| row.actual_cost_usd),
        ),
        estimated_cost_usd: positive_residual(
            aggregate.estimated_cost_usd,
            sum_cost(|row| row.estimated_cost_usd),
        ),
    };
    if residual.has_usage() {
        usage.push(residual);
    }
}

struct HermesSession {
    id: String,
    source: String,
    model: Option<String>,
    started_at: Option<f64>,
    ended_at: Option<f64>,
    title: Option<String>,
    cwd: Option<String>,
    api_calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
    billing_provider: Option<String>,
    estimated_cost_usd: Option<f64>,
    actual_cost_usd: Option<f64>,
}

fn optional_column<'a>(columns: &HashSet<String>, name: &'a str) -> &'a str {
    if columns.contains(name) { name } else { "NULL" }
}

fn load_model_usage(connection: &Connection, session_id: &str) -> Result<Vec<UsageRecord>> {
    let columns = table_columns(connection, "session_model_usage")?;
    if !columns.contains("session_id") || !columns.contains("model") {
        return Ok(Vec::new());
    }

    let mut statement = connection.prepare(
        "SELECT model, billing_provider, api_call_count, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, reasoning_tokens,
                estimated_cost_usd, actual_cost_usd
         FROM session_model_usage
         WHERE session_id = ?
         ORDER BY first_seen, model, billing_provider",
    )?;
    let rows = statement.query_map([session_id], |row| {
        let input = row.get::<_, i64>(3)?.max(0) as u64;
        let output = row.get::<_, i64>(4)?.max(0) as u64;
        let cache_read = row.get::<_, i64>(5)?.max(0) as u64;
        let cache_write = row.get::<_, i64>(6)?.max(0) as u64;
        let provider: String = row.get(1)?;
        let estimated = row.get::<_, f64>(8)?;
        let actual = row.get::<_, f64>(9)?;
        Ok(UsageRecord {
            // Model rows span first_seen..last_seen and cannot be placed in a
            // single day/week without inventing a distribution.
            timestamp: None,
            provider: (!provider.trim().is_empty()).then_some(provider),
            model: row.get(0)?,
            source_event_id: None,
            api_calls: row.get::<_, i64>(2)?.max(0) as u64,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            reasoning_tokens: row.get::<_, i64>(7)?.max(0) as u64,
            total_tokens: input
                .saturating_add(output)
                .saturating_add(cache_read)
                .saturating_add(cache_write),
            actual_cost_usd: (actual > 0.0).then_some(actual),
            estimated_cost_usd: (estimated > 0.0).then_some(estimated),
        })
    })?;
    Ok(rows
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(UsageRecord::has_usage)
        .collect())
}

fn load_messages(
    connection: &Connection,
    columns: &HashSet<String>,
    session: &HermesSession,
) -> Result<Vec<Message>> {
    let tool_calls = if columns.contains("tool_calls") {
        "tool_calls"
    } else {
        "NULL"
    };
    let tool_name = if columns.contains("tool_name") {
        "tool_name"
    } else {
        "NULL"
    };
    let visibility = if columns.contains("active") && columns.contains("compacted") {
        " AND (active = 1 OR compacted = 1)"
    } else {
        ""
    };
    let query = format!(
        "SELECT role, content, timestamp, {tool_calls}, {tool_name} \
         FROM messages WHERE session_id = ?{visibility} ORDER BY id"
    );
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map([&session.id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<f64>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;

    let mut messages = Vec::new();
    for row in rows {
        let (role_name, raw_content, timestamp, tool_calls, tool_name) = row?;
        let Some(role) = parse_role(&role_name) else {
            continue;
        };
        let timestamp = timestamp.map(seconds_to_millis);
        let content = raw_content
            .as_deref()
            .map(decode_content)
            .unwrap_or_default();
        if !content.trim().is_empty() {
            let content = if role == Role::Tool {
                tool_name
                    .as_deref()
                    .map(|name| format!("[Tool: {name}]\n{content}"))
                    .unwrap_or(content)
            } else {
                content
            };
            messages.push(Message {
                idx: messages.len(),
                role,
                content,
                timestamp,
                model: session.model.clone(),
            });
        }
        if let Some(summary) = tool_calls.as_deref().and_then(format_tool_calls) {
            messages.push(Message {
                idx: messages.len(),
                role: Role::Tool,
                content: summary,
                timestamp,
                model: session.model.clone(),
            });
        }
    }
    Ok(messages)
}

fn decode_content(raw: &str) -> String {
    serde_json::from_str::<Value>(raw)
        .ok()
        .map(|value| flatten_json_content(&value))
        .filter(|content| !content.is_empty())
        .unwrap_or_else(|| raw.to_string())
}

fn format_tool_calls(raw: &str) -> Option<String> {
    let calls: Value = serde_json::from_str(raw).ok()?;
    let calls = calls.as_array()?;
    let summaries: Vec<_> = calls
        .iter()
        .map(|call| {
            let function = call.get("function").unwrap_or(call);
            let name = function
                .get("name")
                .or_else(|| call.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let arguments = function
                .get("arguments")
                .or_else(|| call.get("input"))
                .map(value_text)
                .unwrap_or_default();
            if arguments.is_empty() {
                format!("[Tool: {name}]")
            } else {
                format!("[Tool: {name}] {arguments}")
            }
        })
        .collect();
    (!summaries.is_empty()).then(|| summaries.join("\n"))
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn seconds_to_millis(value: f64) -> i64 {
    if value > 1_000_000_000_000.0 {
        value as i64
    } else {
        (value * 1000.0) as i64
    }
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

#[allow(clippy::too_many_arguments)]
fn normalized_fingerprint(
    id: &str,
    source: &str,
    model: Option<&str>,
    title: Option<&str>,
    cwd: Option<&str>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    messages: &[Message],
    usage: &[UsageRecord],
) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(format!("hermes:{PARSER_REVISION}\0").as_bytes());
    hasher.update(
        serde_json::to_string(&(
            id, source, model, title, cwd, started_at, ended_at, messages, usage,
        ))?
        .as_bytes(),
    );
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_database(home: &Path) {
        let connection = Connection::open(home.join("state.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, source TEXT NOT NULL, model TEXT,
                    started_at REAL, ended_at REAL, title TEXT, cwd TEXT
                 );
                 CREATE TABLE messages (
                    id INTEGER PRIMARY KEY, session_id TEXT NOT NULL, role TEXT NOT NULL,
                    content TEXT, tool_calls TEXT, tool_name TEXT, timestamp REAL,
                    active INTEGER NOT NULL DEFAULT 1, compacted INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO sessions VALUES
                    ('session-1', 'desktop', 'gpt-test', 1700000000, 1700000010, 'Hermes test', '/tmp/work');
                 INSERT INTO messages VALUES
                    (1, 'session-1', 'user', 'hello Hermes', NULL, NULL, 1700000001, 1, 0),
                    (2, 'session-1', 'assistant', 'hello user', NULL, NULL, 1700000002, 1, 0),
                    (3, 'session-1', 'tool', 'tool result', NULL, 'read_file', 1700000003, 1, 0),
                    (4, 'session-1', 'session_meta', 'internal', NULL, NULL, 1700000004, 1, 0),
                    (5, 'session-1', 'user', 'rewound', NULL, NULL, 1700000005, 0, 0),
                    (6, 'session-1', 'assistant', 'archived but searchable', NULL, NULL, 1700000006, 0, 1);",
            )
            .unwrap();
    }

    #[test]
    fn scans_sqlite_sessions_and_filters_rewound_rows() {
        let home = TempDir::new().unwrap();
        create_database(home.path());
        let connector = HermesConnector {
            base_home: Some(home.path().to_path_buf()),
        };

        let conversations = connector.scan(&connector.default_roots(), None).unwrap();
        assert_eq!(conversations.len(), 1);
        let conversation = &conversations[0];
        assert_eq!(conversation.agent, Agent::Hermes);
        assert_eq!(conversation.external_id.as_deref(), Some("session-1"));
        assert_eq!(
            conversation.workspace.as_deref(),
            Some(Path::new("/tmp/work"))
        );
        assert_eq!(conversation.messages.len(), 4);
        assert!(
            conversation
                .messages
                .iter()
                .any(|message| message.content.contains("archived"))
        );
        assert!(
            !conversation
                .messages
                .iter()
                .any(|message| message.content.contains("rewound"))
        );
        assert!(connector.source_exists(&conversation.source_path).unwrap());
    }

    #[test]
    fn reconciles_partial_model_usage_with_session_totals() {
        let home = TempDir::new().unwrap();
        create_database(home.path());
        let connection = Connection::open(home.path().join("state.db")).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE sessions ADD COLUMN api_call_count INTEGER DEFAULT 0;
                 ALTER TABLE sessions ADD COLUMN input_tokens INTEGER DEFAULT 0;
                 ALTER TABLE sessions ADD COLUMN output_tokens INTEGER DEFAULT 0;
                 ALTER TABLE sessions ADD COLUMN cache_read_tokens INTEGER DEFAULT 0;
                 ALTER TABLE sessions ADD COLUMN cache_write_tokens INTEGER DEFAULT 0;
                 ALTER TABLE sessions ADD COLUMN reasoning_tokens INTEGER DEFAULT 0;
                 ALTER TABLE sessions ADD COLUMN billing_provider TEXT;
                 ALTER TABLE sessions ADD COLUMN estimated_cost_usd REAL;
                 ALTER TABLE sessions ADD COLUMN actual_cost_usd REAL;
                 UPDATE sessions SET
                    api_call_count = 99, input_tokens = 900, output_tokens = 800,
                    cache_read_tokens = 700, cache_write_tokens = 600,
                    reasoning_tokens = 500, billing_provider = 'fallback-provider',
                    estimated_cost_usd = 9.0, actual_cost_usd = 8.0;
                 CREATE TABLE session_model_usage (
                    session_id TEXT NOT NULL, model TEXT NOT NULL,
                    billing_provider TEXT NOT NULL DEFAULT '',
                    api_call_count INTEGER NOT NULL DEFAULT 0,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                    estimated_cost_usd REAL NOT NULL DEFAULT 0,
                    actual_cost_usd REAL NOT NULL DEFAULT 0,
                    first_seen REAL, last_seen REAL
                 );
                 INSERT INTO session_model_usage VALUES
                    ('session-1', 'gpt-split', 'openai-codex', 7,
                     10, 20, 30, 40, 5, 1.25, 0.75,
                     1700000001, 1700000009);
                 DELETE FROM messages;",
            )
            .unwrap();
        drop(connection);
        let connector = HermesConnector {
            base_home: Some(home.path().to_path_buf()),
        };

        let conversations = connector.scan(&connector.default_roots(), None).unwrap();
        assert_eq!(conversations.len(), 1);
        assert!(conversations[0].messages.is_empty());
        assert_eq!(conversations[0].usage.len(), 2);
        let usage = &conversations[0].usage[0];
        assert_eq!(usage.provider.as_deref(), Some("openai-codex"));
        assert_eq!(usage.model.as_deref(), Some("gpt-split"));
        assert_eq!(usage.api_calls, 7);
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_read_tokens, 30);
        assert_eq!(usage.cache_write_tokens, 40);
        assert_eq!(usage.reasoning_tokens, 5);
        assert_eq!(usage.total_tokens, 100);
        assert_eq!(usage.estimated_cost_usd, Some(1.25));
        assert_eq!(usage.actual_cost_usd, Some(0.75));
        let residual = &conversations[0].usage[1];
        assert_eq!(residual.provider.as_deref(), Some("fallback-provider"));
        assert_eq!(residual.model.as_deref(), Some("gpt-test"));
        assert_eq!(residual.api_calls, 92);
        assert_eq!(residual.input_tokens, 890);
        assert_eq!(residual.output_tokens, 780);
        assert_eq!(residual.cache_read_tokens, 670);
        assert_eq!(residual.cache_write_tokens, 560);
        assert_eq!(residual.reasoning_tokens, 495);
        assert_eq!(residual.total_tokens, 2_900);
        assert_eq!(residual.estimated_cost_usd, Some(7.75));
        assert_eq!(residual.actual_cost_usd, Some(7.25));
    }

    #[test]
    fn virtual_source_detects_deleted_session() {
        let home = TempDir::new().unwrap();
        create_database(home.path());
        let connector = HermesConnector {
            base_home: Some(home.path().to_path_buf()),
        };
        let source = database_source_path(home.path(), Agent::Hermes, "session-1");
        let connection = Connection::open(home.path().join("state.db")).unwrap();
        connection.execute("DELETE FROM sessions", []).unwrap();
        drop(connection);
        assert!(!connector.source_exists(&source).unwrap());
    }
}
