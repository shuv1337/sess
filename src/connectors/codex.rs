use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, ConnectorScan, file_modified_since, flatten_json_content, json_u64,
    normalized_token_total, parse_role, source_file,
};
use crate::model::{
    Agent, Conversation, ConversationMetadata, Message, Role, UsageMetadata, UsageRecord,
    parse_timestamp, source_fingerprint,
};

const CODEX_PARSER_REVISION: &str = "7";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CodexUsageSignature([u64; 9]);

impl CodexUsageSignature {
    fn from_usage(cumulative: &Value, last: &Value) -> Self {
        Self([
            json_u64(cumulative, &["/input_tokens"]),
            json_u64(cumulative, &["/output_tokens"]),
            json_u64(cumulative, &["/cached_input_tokens"]),
            json_u64(cumulative, &["/total_tokens"]),
            json_u64(last, &["/input_tokens"]),
            json_u64(last, &["/output_tokens"]),
            json_u64(last, &["/cached_input_tokens"]),
            json_u64(last, &["/reasoning_output_tokens"]),
            json_u64(last, &["/total_tokens"]),
        ])
    }

    /// Lossless identity for copied cumulative telemetry. Keeping the complete
    /// tuple in the ID avoids both hash collisions and false matches between
    /// calls that share their last-call counters but not cumulative state.
    /// The root session namespace preserves deduplication across copied child
    /// rollouts without collapsing identical counters from unrelated sessions.
    fn source_event_id(self, root_session_id: &str) -> String {
        let counters = self.0.map(|value| value.to_string()).join(":");
        format!("codex-cumulative-v2:{root_session_id}:{counters}")
    }
}

pub struct CodexConnector {
    home_dir: Option<PathBuf>,
}

impl CodexConnector {
    pub fn new() -> Self {
        Self {
            home_dir: dirs::home_dir(),
        }
    }

    fn codex_home(&self) -> Option<PathBuf> {
        std::env::var_os("CODEX_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| self.home_dir.as_ref().map(|home| home.join(".codex")))
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        self.codex_home()
            .into_iter()
            .flat_map(|home| [home.join("sessions"), home.join("archived_sessions")])
            .collect()
    }

    fn scan_matching(
        &self,
        roots: &[PathBuf],
        since_ts: Option<i64>,
        scan_kind: &'static str,
        should_parse: &dyn Fn(&Path) -> Result<bool>,
    ) -> Result<ConnectorScan> {
        let mut conversations = Vec::new();
        let mut complete = true;

        for root in roots {
            if !root.is_dir() {
                continue;
            }

            let mut files_discovered = 0usize;
            let mut files_parsed = 0usize;
            let mut parse_errors = 0usize;

            tracing::debug!(
                agent = Agent::Codex.slug(),
                root = %root.display(),
                since_ts,
                scan_kind,
                "Starting Codex session scan"
            );

            // Codex CLI stores active and archived rollouts as JSONL (with
            // legacy JSON files supported for compatibility).
            for entry in WalkDir::new(root).follow_links(true) {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        complete = false;
                        parse_errors += 1;
                        tracing::warn!(
                            agent = Agent::Codex.slug(),
                            root = %root.display(),
                            scan_kind,
                            error = %error,
                            "Failed to traverse Codex session storage"
                        );
                        continue;
                    }
                };
                if !entry.file_type().is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy();
                if !name.starts_with("rollout-")
                    || (!name.ends_with(".jsonl") && !name.ends_with(".json"))
                {
                    continue;
                }
                files_discovered += 1;
                let path = entry.path();

                if !should_parse(path)? {
                    continue;
                }

                match parse_codex_session(path) {
                    Ok(Some(conv)) => {
                        files_parsed += 1;
                        conversations.push(conv);
                    }
                    Ok(None) => {
                        // Empty or no messages, skip.
                    }
                    Err(error) => {
                        complete = false;
                        parse_errors += 1;
                        let action = self.on_parse_error(path, &error);
                        match action {
                            crate::connectors::ErrorAction::Skip => {
                                tracing::warn!(
                                    agent = Agent::Codex.slug(),
                                    source_path = %path.display(),
                                    scan_kind,
                                    error = %error,
                                    "Failed to parse Codex session"
                                );
                            }
                            crate::connectors::ErrorAction::Fail => return Err(error),
                            crate::connectors::ErrorAction::SkipAgent => {
                                tracing::warn!(
                                    scan_kind,
                                    "Skipping remaining Codex files due to error"
                                );
                                return Ok(ConnectorScan::new(conversations, false));
                            }
                        }
                    }
                }
            }

            tracing::debug!(
                agent = Agent::Codex.slug(),
                root = %root.display(),
                files_discovered,
                files_parsed,
                parse_errors,
                scan_kind,
                "Completed Codex session scan"
            );
        }

        Ok(ConnectorScan::new(conversations, complete))
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
        self.session_roots().iter().any(|path| path.is_dir())
    }

    fn default_roots(&self) -> Vec<PathBuf> {
        self.session_roots()
    }

    fn scan(&self, roots: &[PathBuf], since_ts: Option<i64>) -> Result<ConnectorScan> {
        self.scan_matching(roots, since_ts, "mtime", &|path| {
            Ok(file_modified_since(path, since_ts))
        })
    }

    fn scan_unindexed_sources(
        &self,
        roots: &[PathBuf],
        since_ts: Option<i64>,
        is_indexed: &dyn Fn(&Path) -> Result<bool>,
    ) -> Result<ConnectorScan> {
        let Some(_) = since_ts else {
            return Ok(ConnectorScan::new(Vec::new(), true));
        };

        self.scan_matching(roots, since_ts, "unindexed-backstop", &|path| {
            if file_modified_since(path, since_ts) {
                return Ok(false);
            }
            Ok(!is_indexed(path)?)
        })
    }

    fn parser_revision(&self) -> Option<&'static str> {
        Some(CODEX_PARSER_REVISION)
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
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);

    let source_file = source_file(path)
        .with_context(|| format!("Failed to get source file info for {}", path.display()))?;

    let mut messages: Vec<Message> = Vec::new();
    let mut external_id: Option<String> = None;
    let mut root_session_id: Option<String> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut current_model: Option<String> = None;
    let mut current_provider: Option<String> = None;
    let mut session_title: Option<String> = None;
    let mut parent_external_id: Option<String> = None;
    let mut is_subagent = false;
    let mut is_automation = false;
    let mut user_title_candidates: Vec<String> = Vec::new();
    let mut timestamps: Vec<i64> = Vec::new();
    let mut usage: Vec<UsageRecord> = Vec::new();
    let mut usage_signatures: Vec<Option<CodexUsageSignature>> = Vec::new();
    let mut seen_cumulative_signatures: HashSet<CodexUsageSignature> = HashSet::new();
    let mut usage_forwarded_by_pi = false;

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
            Some("session_meta") => {
                if let Some(payload) = value.get("payload") {
                    if is_pi_external_runtime(payload) {
                        usage_forwarded_by_pi = true;
                        // Codex external-runtime rollouts mirror the Pi
                        // runtime's cumulative token telemetry. The canonical
                        // per-call records are indexed from Pi's own session
                        // store, so retaining this copy would both sum
                        // cumulative snapshots and double-count the calls
                        // across harnesses.
                        usage.clear();
                        usage_signatures.clear();
                    }
                    if let Some(session_id) = payload.get("session_id").and_then(Value::as_str) {
                        // Modern copied/subagent rollouts retain the root
                        // session ID even when their physical `id` changes.
                        root_session_id = Some(session_id.to_owned());
                    } else if root_session_id.is_none() {
                        root_session_id =
                            payload.get("id").and_then(Value::as_str).map(str::to_owned);
                    }
                    if external_id.is_none() {
                        external_id = payload
                            .get("id")
                            .or_else(|| payload.get("session_id"))
                            .and_then(|v| v.as_str())
                            .map(str::to_owned);
                    }
                    parent_external_id = payload
                        .get("parent_thread_id")
                        .or_else(|| {
                            payload.pointer("/source/subagent/thread_spawn/parent_thread_id")
                        })
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(parent_external_id);
                    is_subagent |= payload.pointer("/source/subagent/thread_spawn").is_some()
                        || payload
                            .get("thread_source")
                            .and_then(Value::as_str)
                            .is_some_and(|source| source.eq_ignore_ascii_case("subagent"));
                    is_automation |= [
                        payload.get("originator").and_then(Value::as_str),
                        payload.get("thread_source").and_then(Value::as_str),
                    ]
                    .into_iter()
                    .flatten()
                    .any(|source| {
                        let source = source.to_ascii_lowercase();
                        source.contains("cron")
                            || source.contains("automation")
                            || source.contains("scheduled")
                    });
                    if let Some(cwd) = payload.get("cwd").and_then(|v| v.as_str()) {
                        workspace = Some(PathBuf::from(cwd));
                    }
                    current_provider = payload
                        .get("model_provider")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(current_provider);
                    if session_title.is_none() {
                        session_title = subagent_session_title(payload);
                    }
                }
            }
            Some("turn_context") => {
                current_model = value
                    .get("payload")
                    .and_then(|payload| payload.get("model"))
                    .and_then(|model| model.as_str())
                    .map(str::to_owned)
                    .or(current_model);
            }
            Some("response_item") => {
                if let Some(payload) = value.get("payload") {
                    // Modern rollouts distinguish messages from reasoning and
                    // tool-call response items. Older rollouts omitted `type`.
                    let payload_type = payload.get("type").and_then(|v| v.as_str());
                    if !matches!(payload_type, None | Some("message")) {
                        continue;
                    }

                    let content = payload
                        .get("content")
                        .map(flatten_json_content)
                        .unwrap_or_default();

                    let Some(role) = payload
                        .get("role")
                        .and_then(|v| v.as_str())
                        .and_then(|role| parse_codex_role(role, &content))
                    else {
                        continue;
                    };

                    push_message(
                        &mut messages,
                        &mut timestamps,
                        role,
                        content,
                        entry_timestamp(&value, payload),
                        current_model.clone(),
                    );
                }
            }
            Some("event_msg") => {
                // Handle event messages
                if let Some(payload) = value.get("payload") {
                    let event_type = payload.get("type").and_then(|v| v.as_str());

                    match event_type {
                        Some("user_message") => {
                            if let Some(message) = payload.get("message").and_then(|v| v.as_str()) {
                                let role = if is_codex_context_message(message) {
                                    Role::System
                                } else {
                                    Role::User
                                };
                                if role == Role::User
                                    && user_title_candidates.is_empty()
                                    && !message.trim().is_empty()
                                {
                                    user_title_candidates.push(message.trim().to_string());
                                }
                                push_message(
                                    &mut messages,
                                    &mut timestamps,
                                    role,
                                    message.to_string(),
                                    entry_timestamp(&value, payload),
                                    current_model.clone(),
                                );
                            }
                        }
                        Some("agent_message") => {
                            if let Some(message) = payload.get("message").and_then(|v| v.as_str()) {
                                push_message(
                                    &mut messages,
                                    &mut timestamps,
                                    Role::Assistant,
                                    message.to_string(),
                                    entry_timestamp(&value, payload),
                                    current_model.clone(),
                                );
                            }
                        }
                        Some("agent_reasoning") => {
                            if let Some(text) = payload.get("text").and_then(|v| v.as_str()) {
                                push_message(
                                    &mut messages,
                                    &mut timestamps,
                                    Role::Assistant,
                                    text.to_string(),
                                    entry_timestamp(&value, payload),
                                    current_model.clone(),
                                );
                            }
                        }
                        Some("token_count") => {
                            if !usage_forwarded_by_pi
                                && let Some(info) = payload.get("info")
                                && let Some(last) = info.get("last_token_usage")
                            {
                                let signature = info.get("total_token_usage").map(|cumulative| {
                                    CodexUsageSignature::from_usage(cumulative, last)
                                });
                                let is_new = signature
                                    .map(|signature| seen_cumulative_signatures.insert(signature))
                                    .unwrap_or(true);
                                if is_new {
                                    let raw_input = json_u64(last, &["/input_tokens"]);
                                    let output = json_u64(last, &["/output_tokens"]);
                                    let cache_read = json_u64(last, &["/cached_input_tokens"]);
                                    let input = raw_input.saturating_sub(cache_read);
                                    let reported_total = optional_json_u64(last, "/total_tokens");
                                    let total = normalized_token_total(
                                        reported_total.unwrap_or_default(),
                                        input,
                                        output,
                                        cache_read,
                                        0,
                                    );
                                    let mut record = UsageRecord {
                                        timestamp: entry_timestamp(&value, payload),
                                        provider: current_provider.clone(),
                                        model: current_model.clone(),
                                        source_event_id: None,
                                        api_calls: 1,
                                        input_tokens: input,
                                        output_tokens: output,
                                        cache_read_tokens: cache_read,
                                        cache_write_tokens: 0,
                                        reasoning_tokens: json_u64(
                                            last,
                                            &["/reasoning_output_tokens"],
                                        ),
                                        total_tokens: total,
                                        actual_cost_usd: None,
                                        estimated_cost_usd: None,
                                        metadata: UsageMetadata {
                                            reported_total_tokens: reported_total,
                                            token_semantics: Some(
                                                "codex-last-token-usage-v1".to_string(),
                                            ),
                                            ..UsageMetadata::default()
                                        },
                                    };
                                    record.enrich_metadata();
                                    if record.has_usage() {
                                        usage.push(record);
                                        usage_signatures.push(signature);
                                    }
                                }
                            }
                        }
                        _ => {
                            // Skip other event types like turn_aborted.
                        }
                    }
                }
            }
            _ => {
                // Unknown type, skip
            }
        }
    }

    if messages.is_empty() && usage.is_empty() {
        return Ok(None);
    }

    if let Some(root_session_id) = root_session_id.as_deref() {
        for (record, signature) in usage.iter_mut().zip(usage_signatures) {
            record.source_event_id =
                signature.map(|signature| signature.source_event_id(root_session_id));
        }
    }

    let title = user_title_candidates
        .first()
        .map(String::as_str)
        .or_else(|| {
            messages
                .iter()
                .find(|message| {
                    message.role == Role::User && !is_codex_context_message(&message.content)
                })
                .map(|message| message.content.as_str())
        })
        .map(|content| {
            let first_line = content.lines().next().unwrap_or(content);
            crate::model::truncate_title(first_line, 100)
        })
        .or(session_title);

    let started_at = timestamps.iter().min().copied();
    let ended_at = timestamps.iter().max().copied();

    let source_files = vec![source_file];
    let fingerprint = format!(
        "codex-v{CODEX_PARSER_REVISION}:{}",
        source_fingerprint(&source_files)
    );
    let logical_session_id = root_session_id
        .clone()
        .or_else(|| parent_external_id.clone())
        .or_else(|| external_id.clone());
    let record_kind = if is_subagent {
        "child_agent"
    } else if is_automation {
        "automation"
    } else {
        "top_level"
    };

    Ok(Some(Conversation {
        agent: Agent::Codex,
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
        metadata: ConversationMetadata {
            logical_session_id,
            parent_external_id,
            record_kind: Some(record_kind.to_string()),
            is_synthetic: false,
        },
    }))
}

fn optional_json_u64(value: &Value, pointer: &str) -> Option<u64> {
    let value = value.pointer(pointer)?;
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| number.try_into().ok()))
}

fn parse_codex_role(role: &str, content: &str) -> Option<Role> {
    match role.to_ascii_lowercase().as_str() {
        "developer" => Some(Role::System),
        "user" if content.trim_start().starts_with("<user_shell_command>") => Some(Role::Tool),
        "user" if is_codex_context_message(content) => Some(Role::System),
        other => parse_role(other),
    }
}

fn is_codex_context_message(content: &str) -> bool {
    let content = content.trim_start();
    [
        "# AGENTS.md instructions",
        "## Referenced ChatGPT conversation:",
        "<collaboration_mode>",
        "<environment_context>",
        "<model_switch>",
        "<multi_agent_mode>",
        "<permissions instructions>",
        "<recommended_plugins>",
        "<skill>",
    ]
    .iter()
    .any(|prefix| content.starts_with(prefix))
}

fn subagent_session_title(payload: &Value) -> Option<String> {
    let spawn = payload.pointer("/source/subagent/thread_spawn")?;
    let label = spawn
        .get("agent_path")
        .and_then(Value::as_str)
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .or_else(|| spawn.get("agent_nickname").and_then(Value::as_str))?;
    Some(format!("Subagent: {label}"))
}

fn is_pi_external_runtime(payload: &Value) -> bool {
    payload
        .pointer("/source/subagent/thread_spawn/agent_role")
        .and_then(Value::as_str)
        .is_some_and(|role| role.starts_with("pi-"))
}

fn entry_timestamp(value: &Value, payload: &Value) -> Option<i64> {
    value
        .get("timestamp")
        .and_then(parse_timestamp)
        .or_else(|| payload.get("timestamp").and_then(parse_timestamp))
}

fn push_message(
    messages: &mut Vec<Message>,
    timestamps: &mut Vec<i64>,
    role: Role,
    content: String,
    timestamp: Option<i64>,
    model: Option<String>,
) {
    let content = content.trim();
    if content.is_empty() {
        return;
    }

    // Codex emits the same visible message as both a response_item and an
    // event_msg. Keep the event fallback for older rollouts without indexing
    // adjacent duplicates from modern CLI versions.
    if messages.last().is_some_and(|previous| {
        previous.role == role && previous.content == content && previous.timestamp == timestamp
    }) {
        return;
    }

    if let Some(ts) = timestamp {
        timestamps.push(ts);
    }

    messages.push(Message {
        idx: messages.len(),
        role,
        content: content.to_string(),
        timestamp,
        model,
    });
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
    let mut file =
        File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
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
            let content = item
                .content
                .as_ref()
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

    let title = messages.iter().find(|m| m.role == Role::User).map(|m| {
        let first_line = m.content.lines().next().unwrap_or(&m.content);
        crate::model::truncate_title(first_line, 100)
    });

    let started_at = timestamps.iter().min().copied();
    let ended_at = timestamps.iter().max().copied();

    let source_files = vec![source_file];
    let fingerprint = format!(
        "codex-v{CODEX_PARSER_REVISION}:{}",
        source_fingerprint(&source_files)
    );

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
        usage: Vec::new(),
        metadata: ConversationMetadata {
            record_kind: Some("top_level".to_string()),
            ..ConversationMetadata::default()
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{FileTimes, OpenOptions};
    use std::io::Write;
    use std::time::{Duration, SystemTime};
    use tempfile::{NamedTempFile, TempDir};

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
        assert_eq!(
            conv.workspace,
            Some(PathBuf::from("/home/user/codex-project"))
        );
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, Role::User);
        assert_eq!(conv.messages[0].content, "Can you help with this code?");
        assert_eq!(conv.messages[1].role, Role::Assistant);
        assert!(conv.messages[1].content.contains("I'll help you"));
    }

    #[test]
    fn test_parse_codex_token_usage_uses_last_call_and_session_provider() {
        let content = r#"{"type":"session_meta","payload":{"id":"usage-root","cwd":"/project","model_provider":"custom-openai"}}
{"type":"turn_context","payload":{"model":"gpt-test"}}
{"type":"event_msg","timestamp":1705312800.5,"payload":{"type":"user_message","message":"Hello"}}
{"type":"event_msg","timestamp":1705312801.0,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":130},"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":130}}}}
{"type":"event_msg","timestamp":1705312801.1,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":130},"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":130}}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();
        assert_eq!(conversation.usage.len(), 1);
        let usage = &conversation.usage[0];
        assert_eq!(usage.provider.as_deref(), Some("custom-openai"));
        assert_eq!(usage.model.as_deref(), Some("gpt-test"));
        assert_eq!(usage.api_calls, 1);
        assert_eq!(usage.input_tokens, 60);
        assert_eq!(usage.cache_read_tokens, 40);
        assert_eq!(usage.output_tokens, 30);
        assert_eq!(usage.reasoning_tokens, 10);
        assert_eq!(usage.total_tokens, 130);
        assert_eq!(
            usage.source_event_id.as_deref(),
            Some("codex-cumulative-v2:usage-root:100:30:40:130:100:30:40:10:130")
        );
        assert_eq!(usage.metadata.request_attempts, 1);
        assert_eq!(usage.metadata.reported_total_tokens, Some(130));
        assert_eq!(
            usage.metadata.token_semantics.as_deref(),
            Some("codex-last-token-usage-v1")
        );
    }

    #[test]
    fn token_usage_without_cumulative_totals_keeps_distinct_calls() {
        let content = r#"{"type":"event_msg","timestamp":1705312800.5,"payload":{"type":"user_message","message":"Hello"}}
{"type":"event_msg","timestamp":1705312801.0,"payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}}
{"type":"event_msg","timestamp":1705312802.0,"payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();
        assert_eq!(conversation.usage.len(), 2);
        assert_eq!(conversation.usage[0].total_tokens, 15);
        assert_eq!(conversation.usage[1].total_tokens, 15);
        assert!(
            conversation
                .usage
                .iter()
                .all(|usage| usage.source_event_id.is_none())
        );
    }

    #[test]
    fn usage_only_session_is_preserved() {
        let content = r#"{"type":"session_meta","payload":{"id":"usage-only","cwd":"/project","model_provider":"openai"}}
{"type":"turn_context","payload":{"model":"gpt-test"}}
{"type":"event_msg","timestamp":1705312801.0,"payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.usage.len(), 1);
        assert_eq!(conversation.usage[0].total_tokens, 15);
    }

    #[test]
    fn cumulative_replay_after_model_change_is_not_counted_twice() {
        let content = r#"{"type":"turn_context","payload":{"model":"gpt-a"}}
{"type":"event_msg","timestamp":1705312800.5,"payload":{"type":"user_message","message":"Hello"}}
{"type":"event_msg","timestamp":1705312801.0,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130}}}}
{"type":"turn_context","payload":{"model":"gpt-b"}}
{"type":"event_msg","timestamp":1705312802.0,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130}}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();
        assert_eq!(conversation.usage.len(), 1);
        assert_eq!(conversation.usage[0].model.as_deref(), Some("gpt-a"));
    }

    #[test]
    fn non_adjacent_cumulative_replay_is_not_counted_twice() {
        let content = r#"{"type":"event_msg","timestamp":1705312800.5,"payload":{"type":"user_message","message":"Hello"}}
{"type":"event_msg","timestamp":1705312801.0,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130}}}}
{"type":"event_msg","timestamp":1705312802.0,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":210,"output_tokens":50,"total_tokens":260},"last_token_usage":{"input_tokens":110,"output_tokens":20,"total_tokens":130}}}}
{"type":"event_msg","timestamp":1705312803.0,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130}}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();
        assert_eq!(conversation.usage.len(), 2);
        assert_eq!(conversation.usage[0].total_tokens, 130);
        assert_eq!(conversation.usage[1].total_tokens, 130);
    }

    #[test]
    fn cumulative_identity_includes_total_and_last_counters() {
        let base = CodexUsageSignature([1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let mut identities = HashSet::from([base.source_event_id("root")]);
        for index in 0..base.0.len() {
            let mut changed = base.0;
            changed[index] += 100;
            identities.insert(CodexUsageSignature(changed).source_event_id("root"));
        }
        assert_eq!(identities.len(), 10);
    }

    #[test]
    fn cumulative_identity_namespaces_identical_counters_by_root_session() {
        let signature = CodexUsageSignature([1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_ne!(
            signature.source_event_id("root-a"),
            signature.source_event_id("root-b")
        );
    }

    #[test]
    fn same_last_call_with_different_cumulative_state_stays_distinct() {
        let content = r#"{"type":"session_meta","payload":{"id":"root"}}
{"type":"event_msg","timestamp":1705312800.5,"payload":{"type":"user_message","message":"Hello"}}
{"type":"event_msg","timestamp":1705312801.0,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}}
{"type":"event_msg","timestamp":1705312802.0,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":200,"output_tokens":60,"total_tokens":260},"last_token_usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();
        assert_eq!(conversation.usage.len(), 2);
        assert_ne!(
            conversation.usage[0].source_event_id,
            conversation.usage[1].source_event_id
        );
    }

    #[test]
    fn copied_rollouts_share_identity_despite_rewritten_envelope() {
        let first = r#"{"type":"session_meta","payload":{"id":"first","session_id":"shared-root","cwd":"/one","model_provider":"openai"}}
{"type":"turn_context","payload":{"model":"gpt-a"}}
{"type":"event_msg","timestamp":1705312801.0,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130}}}}
"#;
        let second = r#"{"type":"session_meta","payload":{"id":"rewritten","session_id":"shared-root","cwd":"/two"}}
{"type":"event_msg","timestamp":"2026-07-20T12:34:56Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130}}}}
"#;
        let mut first_file = NamedTempFile::new().unwrap();
        first_file.write_all(first.as_bytes()).unwrap();
        let mut second_file = NamedTempFile::new().unwrap();
        second_file.write_all(second.as_bytes()).unwrap();

        let first = parse_codex_jsonl(first_file.path()).unwrap().unwrap();
        let second = parse_codex_jsonl(second_file.path()).unwrap().unwrap();
        assert_eq!(
            first.usage[0].source_event_id,
            second.usage[0].source_event_id
        );
    }

    #[test]
    fn cumulative_replay_across_session_meta_is_not_counted_twice() {
        let content = r#"{"type":"session_meta","payload":{"id":"first","session_id":"shared-root"}}
{"type":"event_msg","timestamp":1705312801.0,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130}}}}
{"type":"session_meta","payload":{"id":"copied-history","session_id":"shared-root"}}
{"type":"event_msg","timestamp":1705312802.0,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130}}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();
        assert_eq!(conversation.usage.len(), 1);
    }

    #[test]
    fn pi_external_runtime_usage_is_left_to_the_pi_connector() {
        let content = r#"{"type":"session_meta","payload":{"cwd":"/project","model_provider":"anthropic","source":{"subagent":{"thread_spawn":{"agent_role":"pi-sonnet"}}}}}
{"type":"turn_context","payload":{"model":"claude-sonnet-5"}}
{"type":"event_msg","timestamp":1705312800.5,"payload":{"type":"user_message","message":"Hello"}}
{"type":"session_meta","payload":{"cwd":"/project","model_provider":"anthropic"}}
{"type":"event_msg","timestamp":1705312801.0,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130}}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();
        assert_eq!(conversation.messages.len(), 1);
        assert!(conversation.usage.is_empty());
    }

    #[test]
    fn depth_two_subagent_metadata_preserves_parent_and_logical_root() {
        let content = r#"{"type":"session_meta","payload":{"id":"child","session_id":"root","parent_thread_id":"parent","thread_source":"subagent","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent","agent_path":"/root/reviewer"}}}}}
{"type":"event_msg","timestamp":1705312800.5,"payload":{"type":"user_message","message":"Hello"}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();
        assert_eq!(
            conversation.metadata.logical_session_id.as_deref(),
            Some("root")
        );
        assert_eq!(
            conversation.metadata.parent_external_id.as_deref(),
            Some("parent")
        );
        assert_eq!(
            conversation.metadata.record_kind.as_deref(),
            Some("child_agent")
        );
    }

    #[test]
    fn preserved_mtime_archive_move_is_recovered_by_unknown_source_scan() {
        let temp = TempDir::new().unwrap();
        let active_root = temp.path().join("sessions");
        let archive_root = temp.path().join("archived_sessions");
        std::fs::create_dir_all(&active_root).unwrap();
        std::fs::create_dir_all(&archive_root).unwrap();

        let active_path = active_root.join("rollout-old.jsonl");
        let archived_path = archive_root.join("rollout-old.jsonl");
        std::fs::write(
            &active_path,
            r#"{"type":"event_msg","timestamp":1705312800.5,"payload":{"type":"user_message","message":"Archived session"}}
"#,
        )
        .unwrap();
        let old_time = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        OpenOptions::new()
            .write(true)
            .open(&active_path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old_time))
            .unwrap();

        let connector = CodexConnector {
            home_dir: Some(temp.path().to_path_buf()),
        };
        let roots = vec![active_root.clone(), archive_root.clone()];
        let initial = connector.scan(&roots, None).unwrap();
        assert_eq!(initial.len(), 1);

        std::fs::rename(&active_path, &archived_path).unwrap();
        let since_ts = 20_000;
        assert!(connector.scan(&roots, Some(since_ts)).unwrap().is_empty());

        let known_paths = HashSet::from([active_path]);
        let recovered = connector
            .scan_unindexed_sources(&roots, Some(since_ts), &|path| {
                Ok(known_paths.contains(path))
            })
            .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].source_path, archived_path);

        let already_indexed = connector
            .scan_unindexed_sources(&roots, Some(since_ts), &|_| Ok(true))
            .unwrap();
        assert!(already_indexed.is_empty());
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
        temp_file
            .as_file_mut()
            .set_len(json_content.len() as u64)
            .unwrap();
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
    fn test_parse_current_codex_cli_rollout() {
        let content = r##"{"timestamp":"2026-07-13T21:35:54.123Z","type":"session_meta","payload":{"id":"session-current","cwd":"/tmp/current-codex","cli_version":"0.144.3"}}
{"timestamp":"2026-07-13T21:35:55.000Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"system guidance"}]}}
{"timestamp":"2026-07-13T21:35:55.500Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\nInjected project context"}]}}
{"timestamp":"2026-07-13T21:35:56.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"current codex fixture prompt"}]}}
{"timestamp":"2026-07-13T21:35:56.000Z","type":"event_msg","payload":{"type":"user_message","message":"current codex fixture prompt"}}
{"timestamp":"2026-07-13T21:35:57.000Z","type":"event_msg","payload":{"type":"agent_message","message":"fixture answer"}}
{"timestamp":"2026-07-13T21:35:57.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fixture answer"}]}}
"##;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conv = parse_codex_jsonl(temp_file.path()).unwrap().unwrap();

        assert_eq!(conv.external_id.as_deref(), Some("session-current"));
        assert_eq!(conv.title.as_deref(), Some("current codex fixture prompt"));
        assert_eq!(conv.workspace, Some(PathBuf::from("/tmp/current-codex")));
        assert_eq!(conv.started_at, Some(1_783_978_555_000));
        assert_eq!(conv.ended_at, Some(1_783_978_557_000));
        assert_eq!(conv.messages.len(), 4);
        assert_eq!(conv.messages[0].role, Role::System);
        assert_eq!(conv.messages[1].role, Role::System);
        assert_eq!(conv.messages[2].role, Role::User);
        assert_eq!(conv.messages[3].role, Role::Assistant);
        assert!(
            conv.source_fingerprint
                .starts_with(&format!("codex-v{CODEX_PARSER_REVISION}:"))
        );
    }

    #[test]
    fn test_subagent_session_title_uses_agent_path() {
        let payload = serde_json::json!({
            "source": {
                "subagent": {
                    "thread_spawn": {
                        "agent_path": "/root/desktop_real_pi",
                        "agent_nickname": "Bohr"
                    }
                }
            }
        });

        assert_eq!(
            subagent_session_title(&payload).as_deref(),
            Some("Subagent: desktop_real_pi")
        );
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
        let result = connector
            .scan(&[PathBuf::from("/totally/nonexistent")], None)
            .unwrap();
        assert!(result.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn traversal_error_marks_codex_scan_incomplete() {
        let dir = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing"), dir.path().join("broken")).unwrap();
        let connector = CodexConnector {
            home_dir: Some(PathBuf::from("/nonexistent")),
        };

        let scan = connector.scan(&[dir.path().to_path_buf()], None).unwrap();
        assert!(!scan.complete);
    }
}
