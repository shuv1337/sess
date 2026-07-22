use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, ConnectorScan, file_modified_since, json_f64, json_u64, normalized_token_total,
    parse_role, source_file,
};
use crate::model::{
    Agent, Conversation, ConversationMetadata, Message, Role, UsageMetadata, UsageRecord,
    source_fingerprint,
};

const PARSER_REVISION: &str = "4";

const PI_STANDARD_TOKEN_SEMANTICS: &str = "pi.reported-total-with-additive-components.v1";
const PI_CURSOR_TOKEN_SEMANTICS: &str = "pi.cursor-sdk.cumulative-reported-total.v1";
const PI_GOOGLE_TOKEN_SEMANTICS: &str = "pi.google.input-includes-cache-read.v1";

pub struct PiAgentConnector {
    home_dir: Option<PathBuf>,
}

fn push_unique_root(roots: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, root: PathBuf) {
    if !root.as_os_str().is_empty() && seen.insert(path_identity(&root)) {
        roots.push(root);
    }
}

fn path_identity(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

impl PiAgentConnector {
    pub fn new() -> Self {
        Self {
            home_dir: dirs::home_dir(),
        }
    }

    fn pi_dirs(&self) -> Vec<PathBuf> {
        let legacy_dir = std::env::var_os("PI_CODING_AGENT_DIR").map(PathBuf::from);
        let additional_dirs = std::env::var_os("SESS_PI_AGENT_DIRS");
        Self::pi_dirs_for(self.home_dir.as_deref(), legacy_dir, additional_dirs)
    }

    fn pi_dirs_for(
        home_dir: Option<&Path>,
        legacy_dir: Option<PathBuf>,
        additional_dirs: Option<OsString>,
    ) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        let mut seen = HashSet::new();

        if let Some(root) = legacy_dir {
            push_unique_root(&mut roots, &mut seen, root);
        }
        if let Some(paths) = additional_dirs {
            for root in std::env::split_paths(&paths) {
                push_unique_root(&mut roots, &mut seen, root);
            }
        }
        if let Some(home) = home_dir {
            // Keep personal, Codex external-runtime, and shuvhelm Pi sessions
            // even when an override is set.
            push_unique_root(&mut roots, &mut seen, home.join(".pi/agent"));
            push_unique_root(&mut roots, &mut seen, home.join(".shuvpi/agent"));
            push_unique_root(&mut roots, &mut seen, home.join(".shuvhelm/pi-agent"));
            push_unique_root(&mut roots, &mut seen, home.join(".shuvhelm/mate"));
        }

        roots
    }

    fn shiv_dir(&self) -> Option<PathBuf> {
        if let Ok(shiv_dir) = std::env::var("SHIV_AGENT_DIR") {
            return Some(PathBuf::from(shiv_dir));
        }
        self.home_dir
            .as_ref()
            .map(|h| h.join(".local").join("share").join("shiv"))
    }

    fn openclaw_dir(&self) -> Option<PathBuf> {
        if let Ok(openclaw_dir) = std::env::var("OPENCLAW_HOME") {
            return Some(PathBuf::from(openclaw_dir));
        }
        self.home_dir.as_ref().map(|h| h.join(".openclaw"))
    }

    fn session_roots_for_scan(&self, root: &Path) -> (Vec<PathBuf>, bool) {
        let mut session_roots = Vec::new();
        let mut complete = true;

        let direct_sessions = root.join("sessions");
        if directory_is_accessible(&direct_sessions, &mut complete) {
            session_roots.push(direct_sessions);
        }

        // OpenClaw layout: ~/.openclaw/agents/<agent>/sessions
        let agents_dir = root.join("agents");
        if directory_is_accessible(&agents_dir, &mut complete) {
            match std::fs::read_dir(&agents_dir) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) => {
                                let agent_root = entry.path();
                                match std::fs::metadata(&agent_root) {
                                    Ok(metadata) if metadata.is_dir() => {}
                                    Ok(_) => continue,
                                    Err(error) => {
                                        complete = false;
                                        tracing::warn!(
                                            agent = Agent::PiAgent.slug(),
                                            root = %agent_root.display(),
                                            error = %error,
                                            "Failed to inspect an OpenClaw agent entry"
                                        );
                                        continue;
                                    }
                                }
                                let path = agent_root.join("sessions");
                                if directory_is_accessible(&path, &mut complete) {
                                    session_roots.push(path);
                                }
                            }
                            Err(error) => {
                                complete = false;
                                tracing::warn!(
                                    agent = Agent::PiAgent.slug(),
                                    root = %agents_dir.display(),
                                    error = %error,
                                    "Failed to inspect an OpenClaw agent directory"
                                );
                            }
                        }
                    }
                }
                Err(error) => {
                    complete = false;
                    tracing::warn!(
                        agent = Agent::PiAgent.slug(),
                        root = %agents_dir.display(),
                        error = %error,
                        "Failed to read OpenClaw agent directories"
                    );
                }
            }
        }

        (session_roots, complete)
    }

    fn scan_matching(
        &self,
        roots: &[PathBuf],
        scan_kind: &'static str,
        should_parse: &dyn Fn(&Path) -> Result<bool>,
    ) -> Result<ConnectorScan> {
        let mut conversations = Vec::new();
        let mut seen_session_roots = HashSet::new();
        let mut complete = true;

        for root in roots {
            let (session_roots, roots_complete) = self.session_roots_for_scan(root);
            complete &= roots_complete;
            for sessions_root in session_roots {
                if !seen_session_roots.insert(path_identity(&sessions_root)) {
                    continue;
                }
                for entry in WalkDir::new(&sessions_root).follow_links(true) {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            complete = false;
                            tracing::warn!(
                                agent = Agent::PiAgent.slug(),
                                root = %sessions_root.display(),
                                scan_kind,
                                error = %error,
                                "Failed to traverse Pi Agent session storage"
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
                    let file_name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("");
                    if !is_supported_session_filename(file_name) || !should_parse(path)? {
                        continue;
                    }

                    match parse_pi_session(path) {
                        Ok(Some(conv)) => conversations.push(conv),
                        Ok(None) => {}
                        Err(error) => {
                            complete = false;
                            match self.on_parse_error(path, &error) {
                                crate::connectors::ErrorAction::Skip => {
                                    tracing::warn!(
                                        agent = Agent::PiAgent.slug(),
                                        source_path = %path.display(),
                                        scan_kind,
                                        error = %error,
                                        "Failed to parse Pi Agent session"
                                    );
                                }
                                crate::connectors::ErrorAction::Fail => return Err(error),
                                crate::connectors::ErrorAction::SkipAgent => {
                                    tracing::warn!(
                                        scan_kind,
                                        "Skipping remaining Pi Agent files due to error"
                                    );
                                    return Ok(ConnectorScan::new(conversations, false));
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(ConnectorScan::new(conversations, complete))
    }
}

impl Default for PiAgentConnector {
    fn default() -> Self {
        Self::new()
    }
}

fn directory_is_accessible(path: &Path, complete: &mut bool) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => true,
        Ok(_) => {
            *complete = false;
            tracing::warn!(
                agent = Agent::PiAgent.slug(),
                root = %path.display(),
                "Pi Agent session path exists but is not a directory"
            );
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            *complete = false;
            tracing::warn!(
                agent = Agent::PiAgent.slug(),
                root = %path.display(),
                error = %error,
                "Failed to inspect Pi Agent session directory"
            );
            false
        }
    }
}

impl Connector for PiAgentConnector {
    fn agent(&self) -> Agent {
        Agent::PiAgent
    }

    fn detect(&self) -> bool {
        self.default_roots().iter().any(|root| {
            let (session_roots, complete) = self.session_roots_for_scan(root);
            !session_roots.is_empty() || !complete
        })
    }

    fn default_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.pi_dirs();
        let mut seen: HashSet<PathBuf> = roots.iter().map(|root| path_identity(root)).collect();
        if let Some(shiv) = self.shiv_dir() {
            push_unique_root(&mut roots, &mut seen, shiv);
        }
        if let Some(openclaw) = self.openclaw_dir() {
            push_unique_root(&mut roots, &mut seen, openclaw);
        }
        roots
    }

    fn scan(&self, roots: &[PathBuf], since_ts: Option<i64>) -> Result<ConnectorScan> {
        self.scan_matching(roots, "mtime", &|path| {
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

        self.scan_matching(roots, "unindexed-backstop", &|path| {
            if file_modified_since(path, since_ts) {
                return Ok(false);
            }
            Ok(!is_indexed(path)?)
        })
    }

    fn parser_revision(&self) -> Option<&'static str> {
        Some(PARSER_REVISION)
    }
}

fn is_supported_session_filename(file_name: &str) -> bool {
    // pi-agent: {timestamp}_{uuid}.jsonl
    if file_name.contains('_') {
        return true;
    }

    // shiv archive format: session-{timestamp}.jsonl
    if file_name.starts_with("session-") {
        return true;
    }

    // openclaw: {uuid}.jsonl
    if let Some(stem) = file_name.strip_suffix(".jsonl") {
        if uuid::Uuid::parse_str(stem).is_ok() {
            return true;
        }
    }

    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NormalizedPiUsage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    total: u64,
    reported_total: Option<u64>,
    component_total: u64,
    token_semantics: &'static str,
}

fn optional_json_u64(value: &Value, pointers: &[&str]) -> Option<u64> {
    pointers.iter().find_map(|pointer| {
        let value = value.pointer(pointer)?;
        value
            .as_u64()
            .or_else(|| value.as_i64().and_then(|number| number.try_into().ok()))
    })
}

/// Normalize the provider-specific token shapes emitted by Pi-compatible
/// harnesses without overwriting the source-reported total.
fn normalize_pi_usage(
    raw_usage: &Value,
    provider: Option<&str>,
    api: Option<&str>,
) -> NormalizedPiUsage {
    let raw_input = json_u64(raw_usage, &["/input", "/inputTokens"]);
    let output = json_u64(raw_usage, &["/output", "/outputTokens"]);
    let cache_read = json_u64(raw_usage, &["/cacheRead", "/cache_read_tokens"]);
    let cache_write = json_u64(raw_usage, &["/cacheWrite", "/cache_write_tokens"]);
    let reported_total = optional_json_u64(raw_usage, &["/totalTokens", "/total_tokens"]);

    let provider = provider.unwrap_or_default();
    let api = api.unwrap_or_default();
    let cursor_shape =
        provider.eq_ignore_ascii_case("cursor") || api.eq_ignore_ascii_case("cursor-sdk");
    let google_shape = (provider.eq_ignore_ascii_case("google")
        || api.eq_ignore_ascii_case("google-generative-ai"))
        && cache_read <= raw_input;

    // Google's Pi adapter reports cached prompt tokens inside `input`. Store
    // non-overlapping categories so input + cache + output reconciles to total.
    let input = if google_shape {
        raw_input.saturating_sub(cache_read)
    } else {
        raw_input
    };
    let component_total = input
        .saturating_add(output)
        .saturating_add(cache_read)
        .saturating_add(cache_write);

    // cursor-sdk's `totalTokens` is a cumulative context counter repeated on
    // every message. Its component counters are per invocation, so summing the
    // components is the only additive event total.
    let total = if cursor_shape {
        component_total
    } else {
        normalized_token_total(
            reported_total.unwrap_or_default(),
            input,
            output,
            cache_read,
            cache_write,
        )
    };
    let token_semantics = if cursor_shape {
        PI_CURSOR_TOKEN_SEMANTICS
    } else if google_shape {
        PI_GOOGLE_TOKEN_SEMANTICS
    } else {
        PI_STANDARD_TOKEN_SEMANTICS
    };

    NormalizedPiUsage {
        input,
        output,
        cache_read,
        cache_write,
        total,
        reported_total,
        component_total,
        token_semantics,
    }
}

fn pi_session_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let candidate = stem.rsplit_once('_').map(|(_, id)| id).unwrap_or(stem);
    uuid::Uuid::parse_str(candidate)
        .ok()
        .map(|_| candidate.to_string())
}

fn parent_session_id(raw_parent: &str, source_path: &Path) -> Option<String> {
    let parent = PathBuf::from(raw_parent);
    let parent = if parent.is_absolute() {
        parent
    } else {
        source_path.parent().unwrap_or(Path::new(".")).join(parent)
    };
    pi_session_id_from_path(&parent).or_else(|| {
        let file = File::open(parent).ok()?;
        BufReader::new(file)
            .lines()
            .map_while(std::result::Result::ok)
            .filter(|line| !line.trim().is_empty())
            .find_map(|line| {
                let value: Value = serde_json::from_str(&line).ok()?;
                (value.get("type").and_then(Value::as_str) == Some("session"))
                    .then(|| value.get("id").and_then(Value::as_str).map(str::to_owned))
                    .flatten()
            })
    })
}

fn is_temporary_workspace(workspace: Option<&Path>) -> bool {
    workspace.is_some_and(|workspace| {
        let workspace = workspace.to_string_lossy();
        workspace == "/tmp"
            || workspace.starts_with("/tmp/")
            || workspace == "/var/tmp"
            || workspace.starts_with("/var/tmp/")
            || workspace == "/private/tmp"
            || workspace.starts_with("/private/tmp/")
    })
}

fn path_has_segment(path: &Path, needle: &str) -> bool {
    path.to_string_lossy().contains(needle)
}

fn parse_pi_session(path: &Path) -> Result<Option<Conversation>> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);

    let source_file = source_file(path)
        .with_context(|| format!("Failed to get source file info for {}", path.display()))?;

    let mut messages: Vec<Message> = Vec::new();
    let mut workspace: Option<PathBuf> = None;
    let mut external_id: Option<String> = None;
    let mut timestamps: Vec<i64> = Vec::new();
    let mut current_model: Option<String> = None;
    let mut current_provider: Option<String> = None;
    let mut parent_external_id: Option<String> = None;
    let mut usage: Vec<UsageRecord> = Vec::new();

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
                if let Some(provider) = value.get("provider").and_then(Value::as_str) {
                    current_provider = Some(provider.to_string());
                }
                if let Some(parent) = value.get("parentSession").and_then(Value::as_str) {
                    parent_external_id = parent_session_id(parent, path);
                }
            }
            Some("model_change") => {
                // Update current model
                if let Some(model) = value.get("modelId").and_then(|v| v.as_str()) {
                    current_model = Some(model.to_string());
                }
                if let Some(provider) = value.get("provider").and_then(Value::as_str) {
                    current_provider = Some(provider.to_string());
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
                    let timestamp = value.get("timestamp").and_then(|v| {
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

                    let message_model = msg
                        .get("model")
                        .or_else(|| msg.get("modelId"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| current_model.clone());
                    let message_provider = msg
                        .get("provider")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| current_provider.clone());
                    if role == Role::Assistant
                        && let Some(raw_usage) = msg.get("usage")
                    {
                        let source_event_id = value
                            .get("id")
                            .or_else(|| msg.get("id"))
                            .or_else(|| msg.get("responseId"))
                            .or_else(|| msg.get("response_id"))
                            .and_then(Value::as_str)
                            .and_then(|id| {
                                timestamp.map(|timestamp| format!("pi-message:{id}:{timestamp}"))
                            });
                        let api = msg.get("api").and_then(Value::as_str);
                        let normalized =
                            normalize_pi_usage(raw_usage, message_provider.as_deref(), api);
                        let estimated_cost_usd =
                            json_f64(raw_usage, &["/cost/total", "/costUsd", "/cost_usd"]);
                        let mut record = UsageRecord {
                            timestamp,
                            provider: message_provider.clone(),
                            model: message_model.clone(),
                            source_event_id,
                            api_calls: 1,
                            input_tokens: normalized.input,
                            output_tokens: normalized.output,
                            cache_read_tokens: normalized.cache_read,
                            cache_write_tokens: normalized.cache_write,
                            reasoning_tokens: json_u64(
                                raw_usage,
                                &["/reasoning", "/reasoningTokens"],
                            ),
                            total_tokens: normalized.total,
                            actual_cost_usd: None,
                            estimated_cost_usd,
                            metadata: UsageMetadata {
                                request_attempts: 1,
                                reported_total_tokens: normalized.reported_total,
                                component_total_tokens: Some(normalized.component_total),
                                token_semantics: Some(normalized.token_semantics.to_string()),
                                cost_status: estimated_cost_usd
                                    .filter(|cost| cost.abs() <= f64::EPSILON)
                                    .map(|_| "source_reported_zero".to_string()),
                                cost_source: estimated_cost_usd
                                    .map(|_| "pi_usage.cost.total".to_string()),
                                ..UsageMetadata::default()
                            },
                        };
                        record.enrich_metadata();
                        if record.has_usage() {
                            usage.push(record);
                        }
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
                            model: message_model,
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

    if messages.is_empty() && usage.is_empty() {
        return Ok(None);
    }

    // If workspace is not set from session.cwd, try to decode from directory name
    let workspace = workspace.or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .and_then(decode_safe_path)
    });

    let title = messages.iter().find(|m| m.role == Role::User).map(|m| {
        let first_line = m.content.lines().next().unwrap_or(&m.content);
        crate::model::truncate_title(first_line, 100)
    });

    let started_at = timestamps.iter().min().copied();
    let ended_at = timestamps.iter().max().copied();

    let source_files = vec![source_file];
    let fingerprint = format!(
        "pi-v{PARSER_REVISION}:{}",
        source_fingerprint(&source_files)
    );

    // Extract session ID from filename if not found in JSON
    let external_id = external_id.or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
    });

    let explicit_faux = current_provider
        .as_deref()
        .is_some_and(|provider| provider.eq_ignore_ascii_case("faux"))
        || current_model
            .as_deref()
            .is_some_and(|model| model.eq_ignore_ascii_case("faux") || model.starts_with("faux-"))
        || usage.iter().any(|record| {
            record
                .provider
                .as_deref()
                .is_some_and(|provider| provider.eq_ignore_ascii_case("faux"))
                || record.model.as_deref().is_some_and(|model| {
                    model.eq_ignore_ascii_case("faux") || model.starts_with("faux-")
                })
        });
    let is_synthetic = explicit_faux || is_temporary_workspace(workspace.as_deref());
    let hierarchy_child =
        parent_external_id.is_some() || path_has_segment(path, "/.shuvhelm/mate/");
    let record_kind = if is_synthetic {
        "test"
    } else if hierarchy_child {
        "child_agent"
    } else if path_has_segment(path, "/.openclaw/agents/") {
        "automation"
    } else {
        "top_level"
    };
    let logical_session_id = parent_external_id.clone().or_else(|| external_id.clone());

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
        usage,
        metadata: ConversationMetadata {
            logical_session_id,
            parent_external_id,
            record_kind: Some(record_kind.to_string()),
            is_synthetic,
        },
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
                        Some("text") => obj
                            .get("text")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        Some("thinking") => obj
                            .get("thinking")
                            .and_then(|v| v.as_str())
                            .map(|s| format!("[Thinking] {}", s)),
                        Some("toolCall") => {
                            let name = obj
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
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
    if approx.exists() { Some(approx) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{FileTimes, OpenOptions};
    use std::io::Write;
    use std::time::{Duration, SystemTime};
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
    fn test_parse_pi_assistant_usage() {
        let content = r#"{"type":"session","id":"usage-session","cwd":"/test","provider":"fallback","modelId":"fallback-model"}
{"type":"message","timestamp":"2024-01-15T10:00:00Z","message":{"role":"user","content":"Hello"}}
{"type":"message","id":"assistant-1","timestamp":"2024-01-15T10:00:05Z","message":{"role":"assistant","provider":"anthropic","model":"claude-fable-5","content":"Done","usage":{"input":2,"output":74,"cacheRead":22295,"cacheWrite":0,"reasoning":23,"totalTokens":22371,"cost":{"total":0.026015}}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_pi_session(temp_file.path()).unwrap().unwrap();
        assert_eq!(conversation.usage.len(), 1);
        let usage = &conversation.usage[0];
        assert_eq!(usage.provider.as_deref(), Some("anthropic"));
        assert_eq!(usage.model.as_deref(), Some("claude-fable-5"));
        assert_eq!(usage.api_calls, 1);
        assert_eq!(usage.input_tokens, 2);
        assert_eq!(usage.output_tokens, 74);
        assert_eq!(usage.reasoning_tokens, 23);
        assert_eq!(usage.cache_read_tokens, 22_295);
        assert_eq!(usage.cache_write_tokens, 0);
        assert_eq!(usage.total_tokens, 22_371);
        assert_eq!(usage.estimated_cost_usd, Some(0.026015));
        assert_eq!(usage.metadata.reported_total_tokens, Some(22_371));
        assert_eq!(usage.metadata.component_total_tokens, Some(22_371));
        assert_eq!(
            usage.metadata.token_semantics.as_deref(),
            Some(PI_STANDARD_TOKEN_SEMANTICS)
        );
        assert_eq!(
            usage.source_event_id.as_deref(),
            Some("pi-message:assistant-1:1705312805000")
        );
    }

    #[test]
    fn cursor_cumulative_total_uses_per_event_components() {
        let content = r#"{"type":"session","id":"cursor-session","cwd":"/work","provider":"cursor","modelId":"composer-2-5"}
{"type":"message","id":"a1","timestamp":"2026-06-16T22:32:20Z","message":{"role":"assistant","api":"cursor-sdk","provider":"cursor","model":"composer-2-5","content":"one","usage":{"input":100,"output":10,"cacheRead":0,"cacheWrite":0,"totalTokens":110}}}
{"type":"message","id":"a2","timestamp":"2026-06-16T22:32:21Z","message":{"role":"assistant","api":"cursor-sdk","provider":"cursor","model":"composer-2-5","content":"two","usage":{"input":20,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":135}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_pi_session(temp_file.path()).unwrap().unwrap();
        assert_eq!(conversation.usage.len(), 2);
        assert_eq!(
            conversation
                .usage
                .iter()
                .map(|record| record.total_tokens)
                .sum::<u64>(),
            135
        );
        assert_eq!(conversation.usage[1].total_tokens, 25);
        assert_eq!(
            conversation.usage[1].metadata.reported_total_tokens,
            Some(135)
        );
        assert_eq!(
            conversation.usage[1].metadata.component_total_tokens,
            Some(25)
        );
        assert_eq!(
            conversation.usage[1].metadata.token_semantics.as_deref(),
            Some(PI_CURSOR_TOKEN_SEMANTICS)
        );
    }

    #[test]
    fn google_input_including_cache_is_split_into_non_overlapping_components() {
        let content = r#"{"type":"session","id":"google-session","cwd":"/work","provider":"google","modelId":"gemini-3.1-pro-preview-customtools"}
{"type":"message","id":"a1","timestamp":"2026-02-26T01:29:21Z","message":{"role":"assistant","api":"google-generative-ai","provider":"google","model":"gemini-3.1-pro-preview-customtools","content":"done","usage":{"input":10432,"output":152,"cacheRead":7649,"cacheWrite":0,"totalTokens":10584}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_pi_session(temp_file.path()).unwrap().unwrap();
        let usage = &conversation.usage[0];
        assert_eq!(usage.input_tokens, 2_783);
        assert_eq!(usage.cache_read_tokens, 7_649);
        assert_eq!(usage.output_tokens, 152);
        assert_eq!(usage.total_tokens, 10_584);
        assert_eq!(usage.metadata.component_total_tokens, Some(10_584));
        assert_eq!(usage.metadata.reported_total_tokens, Some(10_584));
        assert_eq!(
            usage.metadata.token_semantics.as_deref(),
            Some(PI_GOOGLE_TOKEN_SEMANTICS)
        );
    }

    #[test]
    fn faux_or_temporary_pi_sessions_are_explicitly_classified_as_tests() {
        let content = r#"{"type":"session","id":"synthetic-session","cwd":"/tmp/pi-runtime-test","provider":"faux","modelId":"faux-1"}
{"type":"message","id":"a1","timestamp":"2026-07-21T20:24:52Z","message":{"role":"assistant","provider":"faux","model":"faux-1","content":"one","usage":{"input":10,"output":1,"cacheWrite":10,"totalTokens":21,"cost":{"total":0}}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_pi_session(temp_file.path()).unwrap().unwrap();
        assert!(conversation.metadata.is_synthetic);
        assert_eq!(conversation.metadata.record_kind.as_deref(), Some("test"));
        // Raw source dimensions stay intact for transparent all-vs-organic views.
        assert_eq!(conversation.usage[0].provider.as_deref(), Some("faux"));
        assert_eq!(conversation.usage[0].model.as_deref(), Some("faux-1"));
        assert_eq!(
            conversation.usage[0].metadata.cost_status.as_deref(),
            Some("source_reported_zero")
        );
    }

    #[test]
    fn parent_session_path_preserves_pi_hierarchy() {
        let directory = TempDir::new().unwrap();
        let parent_id = "019f865a-47eb-7022-9bed-d17daa6cfdb8";
        let child_id = "019f865a-4800-7f59-9939-e02050edf589";
        let parent = directory
            .path()
            .join(format!("2026-07-21T20-24-51Z_{parent_id}.jsonl"));
        let child = directory
            .path()
            .join(format!("2026-07-21T20-24-52Z_{child_id}.jsonl"));
        std::fs::write(
            &child,
            format!(
                "{{\"type\":\"session\",\"id\":\"{child_id}\",\"cwd\":\"/work\",\"parentSession\":{}}}\n{{\"type\":\"message\",\"timestamp\":\"2026-07-21T20:24:52Z\",\"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n",
                serde_json::to_string(&parent).unwrap()
            ),
        )
        .unwrap();

        let conversation = parse_pi_session(&child).unwrap().unwrap();
        assert_eq!(
            conversation.metadata.parent_external_id.as_deref(),
            Some(parent_id)
        );
        assert_eq!(
            conversation.metadata.logical_session_id.as_deref(),
            Some(parent_id)
        );
        assert_eq!(
            conversation.metadata.record_kind.as_deref(),
            Some("child_agent")
        );
        assert!(!conversation.metadata.is_synthetic);
    }

    #[test]
    fn usage_only_session_is_preserved() {
        let content = r#"{"type":"session","id":"usage-only","cwd":"/test","provider":"anthropic","modelId":"claude-test"}
{"type":"message","id":"assistant-1","timestamp":"2024-01-15T10:00:05Z","message":{"role":"assistant","content":"","usage":{"input":10,"output":5,"totalTokens":15}}}
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let conversation = parse_pi_session(temp_file.path()).unwrap().unwrap();
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.usage.len(), 1);
        assert_eq!(conversation.usage[0].total_tokens, 15);
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
        std::fs::write(
            &session_file,
            r#"{"type":"session","id":"s1","cwd":"/test","modelId":"m1"}
{"type":"message","timestamp":"2024-01-15T10:00:00Z","message":{"role":"user","content":"Hello"}}
"#,
        )
        .unwrap();

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

    #[cfg(unix)]
    #[test]
    fn traversal_error_marks_pi_scan_incomplete() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::os::unix::fs::symlink(sessions.join("missing"), sessions.join("broken")).unwrap();
        let connector = PiAgentConnector {
            home_dir: Some(PathBuf::from("/nonexistent")),
        };

        let scan = connector.scan(&[dir.path().to_path_buf()], None).unwrap();
        assert!(!scan.complete);
    }

    #[test]
    fn invalid_openclaw_agents_path_marks_pi_scan_incomplete() {
        let home = TempDir::new().unwrap();
        let openclaw = home.path().join(".openclaw");
        std::fs::create_dir_all(&openclaw).unwrap();
        std::fs::write(openclaw.join("agents"), "not a directory").unwrap();
        let connector = PiAgentConnector {
            home_dir: Some(home.path().to_path_buf()),
        };

        assert!(connector.detect());
        let scan = connector.scan(&connector.default_roots(), None).unwrap();
        assert!(!scan.complete);
    }

    #[test]
    fn persona_files_in_agents_directory_are_ignored() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("sessions")).unwrap();
        std::fs::create_dir_all(root.path().join("agents")).unwrap();
        std::fs::write(root.path().join("agents/planner.md"), "# Planner").unwrap();
        let connector = PiAgentConnector {
            home_dir: Some(PathBuf::from("/nonexistent")),
        };

        let (session_roots, complete) = connector.session_roots_for_scan(root.path());
        assert!(complete);
        assert_eq!(session_roots, vec![root.path().join("sessions")]);
    }

    #[test]
    fn test_pi_connector_default_roots_include_shiv_and_openclaw() {
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join(".pi/agent/sessions")).unwrap();
        std::fs::create_dir_all(home.path().join(".local/share/shiv/sessions")).unwrap();
        std::fs::create_dir_all(home.path().join(".openclaw/agents/main/sessions")).unwrap();

        let connector = PiAgentConnector {
            home_dir: Some(home.path().to_path_buf()),
        };

        let roots = connector.default_roots();
        assert!(roots.contains(&home.path().join(".pi/agent")));
        assert!(roots.contains(&home.path().join(".shuvpi/agent")));
        assert!(roots.contains(&home.path().join(".shuvhelm/pi-agent")));
        assert!(roots.contains(&home.path().join(".shuvhelm/mate")));
        assert!(roots.contains(&home.path().join(".local/share/shiv")));
        assert!(roots.contains(&home.path().join(".openclaw")));
        assert!(connector.detect());
    }

    #[test]
    fn test_pi_dirs_are_additive_and_deduplicated() {
        let home = TempDir::new().unwrap();
        let personal = home.path().join(".pi/agent");
        let extra = home.path().join("extra-agent");
        let path_list =
            std::env::join_paths([personal.as_path(), extra.as_path(), extra.as_path()]).unwrap();

        let roots = PiAgentConnector::pi_dirs_for(
            Some(home.path()),
            Some(personal.clone()),
            Some(path_list),
        );

        assert_eq!(roots.iter().filter(|root| **root == personal).count(), 1);
        assert_eq!(roots.iter().filter(|root| **root == extra).count(), 1);
        assert!(roots.contains(&home.path().join(".shuvpi/agent")));
        assert!(roots.contains(&home.path().join(".shuvhelm/pi-agent")));
        assert!(roots.contains(&home.path().join(".shuvhelm/mate")));
    }

    #[test]
    fn test_pi_connector_scans_multiple_roots_once() {
        let temp = TempDir::new().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(first.join("sessions")).unwrap();
        std::fs::create_dir_all(second.join("sessions")).unwrap();
        std::fs::write(
            first.join("sessions/2026-07-16T00-00-00-000Z_first.jsonl"),
            r#"{"type":"session","id":"first","cwd":"/first","modelId":"m1"}
{"type":"message","timestamp":"2026-07-16T00:00:00Z","message":{"role":"user","content":"first"}}
"#,
        )
        .unwrap();
        std::fs::write(
            second.join("sessions/2026-07-16T00-00-01-000Z_second.jsonl"),
            r#"{"type":"session","id":"second","cwd":"/second","modelId":"m1"}
{"type":"message","timestamp":"2026-07-16T00:00:01Z","message":{"role":"user","content":"second"}}
"#,
        )
        .unwrap();

        let connector = PiAgentConnector {
            home_dir: Some(temp.path().to_path_buf()),
        };
        let conversations = connector
            .scan(&[first.clone(), second, first], None)
            .unwrap();

        assert_eq!(conversations.len(), 2);
        assert!(
            conversations
                .iter()
                .any(|conversation| conversation.external_id.as_deref() == Some("first"))
        );
        assert!(
            conversations
                .iter()
                .any(|conversation| conversation.external_id.as_deref() == Some("second"))
        );
    }

    #[test]
    fn test_pi_connector_scans_shuvhelm_fleet_and_mate_layouts() {
        let home = TempDir::new().unwrap();
        let fleet = home.path().join(".shuvhelm/pi-agent");
        let mate = home.path().join(".shuvhelm/mate");
        std::fs::create_dir_all(fleet.join("sessions/--crew--")).unwrap();
        std::fs::create_dir_all(mate.join("sessions")).unwrap();
        std::fs::write(
            fleet.join("sessions/--crew--/2026-07-16T00-00-00-000Z_fleet.jsonl"),
            r#"{"type":"session","id":"fleet","cwd":"/fleet","modelId":"m1"}
{"type":"message","timestamp":"2026-07-16T00:00:00Z","message":{"role":"user","content":"fleet"}}
"#,
        )
        .unwrap();
        std::fs::write(
            mate.join("sessions/2026-07-16T00-00-01-000Z_mate.jsonl"),
            r#"{"type":"session","id":"mate","cwd":"/mate","modelId":"m1"}
{"type":"message","timestamp":"2026-07-16T00:00:01Z","message":{"role":"user","content":"mate"}}
"#,
        )
        .unwrap();

        let connector = PiAgentConnector {
            home_dir: Some(home.path().to_path_buf()),
        };
        let roots = PiAgentConnector::pi_dirs_for(Some(home.path()), None, None);
        let conversations = connector.scan(&roots, None).unwrap();

        assert_eq!(conversations.len(), 2);
        assert!(
            conversations
                .iter()
                .any(|conversation| conversation.external_id.as_deref() == Some("fleet"))
        );
        assert!(
            conversations
                .iter()
                .any(|conversation| conversation.external_id.as_deref() == Some("mate"))
        );
    }

    #[test]
    fn test_supported_session_filename_openclaw_uuid() {
        assert!(is_supported_session_filename(
            "8a39b2de-8817-4448-84fb-3733494d81d7.jsonl"
        ));
        assert!(is_supported_session_filename("12345_uuid.jsonl"));
        assert!(is_supported_session_filename("session-1770371965142.jsonl"));
        assert!(!is_supported_session_filename("notes.jsonl"));
    }

    #[test]
    fn test_pi_connector_scan_openclaw_layout() {
        let home = TempDir::new().unwrap();
        let sessions_dir = home.path().join(".openclaw/agents/main/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_file = sessions_dir.join("8a39b2de-8817-4448-84fb-3733494d81d7.jsonl");
        std::fs::write(
            &session_file,
            r#"{"type":"session","id":"s1","cwd":"/home/user/project","modelId":"m1"}
{"type":"message","timestamp":"2024-01-15T10:00:00Z","message":{"role":"user","content":"hello openclaw"}}
"#,
        )
        .unwrap();

        let connector = PiAgentConnector {
            home_dir: Some(home.path().to_path_buf()),
        };

        let conversations = connector
            .scan(&[home.path().join(".openclaw")], None)
            .unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(
            conversations[0].workspace,
            Some(PathBuf::from("/home/user/project"))
        );
    }

    #[test]
    fn test_pi_connector_scan_nonexistent() {
        let connector = PiAgentConnector {
            home_dir: Some(PathBuf::from("/nonexistent")),
        };
        let result = connector
            .scan(&[PathBuf::from("/nonexistent/root")], None)
            .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn preserved_mtime_import_is_recovered_by_unknown_source_scan() {
        let root = TempDir::new().unwrap();
        let sessions = root.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let session_file = sessions.join("2024-01-15T10-00-00Z_imported.jsonl");
        std::fs::write(
            &session_file,
            r#"{"type":"session","id":"imported","cwd":"/work","modelId":"m1"}
{"type":"message","timestamp":"2024-01-15T10:00:00Z","message":{"role":"user","content":"Imported session"}}
"#,
        )
        .unwrap();
        OpenOptions::new()
            .write(true)
            .open(&session_file)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(10)),
            )
            .unwrap();

        let connector = PiAgentConnector {
            home_dir: Some(PathBuf::from("/nonexistent")),
        };
        let roots = vec![root.path().to_path_buf()];
        let since_ts = 20_000;
        assert!(connector.scan(&roots, Some(since_ts)).unwrap().is_empty());

        let recovered = connector
            .scan_unindexed_sources(&roots, Some(since_ts), &|_| Ok(false))
            .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].source_path, session_file);

        let already_indexed = connector
            .scan_unindexed_sources(&roots, Some(since_ts), &|_| Ok(true))
            .unwrap();
        assert!(already_indexed.is_empty());
    }
}
