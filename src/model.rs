use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Which agent produced this conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Agent {
    ClaudeCode,
    Codex,
    Hermes,
    OpenCode,
    PiAgent,
}

impl Agent {
    pub fn slug(&self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude_code",
            Agent::Codex => "codex",
            Agent::Hermes => "hermes",
            Agent::OpenCode => "opencode",
            Agent::PiAgent => "pi_agent",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Hermes => "Hermes Agent",
            Agent::OpenCode => "OpenCode",
            Agent::PiAgent => "Pi Agent",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Agent::ClaudeCode => "●",
            Agent::Codex => "◆",
            Agent::Hermes => "♦",
            Agent::OpenCode => "■",
            Agent::PiAgent => "▲",
        }
    }

    pub fn color_code(&self) -> (u8, u8, u8) {
        match self {
            Agent::ClaudeCode => (147, 112, 219), // Purple
            Agent::Codex => (50, 205, 50),        // Green
            Agent::Hermes => (0, 206, 209),       // Dark turquoise
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
            "hermes" | "hermes_agent" | "hermesagent" => Ok(Agent::Hermes),
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

/// Temporal meaning of one usage row.
///
/// Event rows represent a single provider invocation. Aggregate rows preserve
/// source totals without pretending that all of their tokens happened at one
/// instant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageGrain {
    #[default]
    Event,
    IntervalAggregate,
    SessionAggregate,
}

impl UsageGrain {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::IntervalAggregate => "interval_aggregate",
            Self::SessionAggregate => "session_aggregate",
        }
    }
}

impl std::str::FromStr for UsageGrain {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "event" => Ok(Self::Event),
            "interval_aggregate" => Ok(Self::IntervalAggregate),
            "session_aggregate" => Ok(Self::SessionAggregate),
            _ => anyhow::bail!("Unknown usage grain: {value}"),
        }
    }
}

/// Source facts and derived provenance attached to one usage observation.
///
/// Raw `provider` and `model` remain on [`UsageRecord`]. These fields add
/// queryable semantics without overwriting what the harness reported.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageMetadata {
    /// Inclusive interval covered by an aggregate row, in Unix millis UTC.
    pub interval_start: Option<i64>,
    pub interval_end: Option<i64>,
    pub grain: UsageGrain,
    pub provider_family: Option<String>,
    pub provider_inference_source: Option<String>,
    pub provider_inference_confidence: Option<String>,
    pub model_family: Option<String>,
    pub model_variant: Option<String>,
    pub task: Option<String>,
    pub billing_base_url: Option<String>,
    pub billing_mode: Option<String>,
    /// Request attempts represented by the row, including failed requests.
    pub request_attempts: u64,
    /// Total exactly as reported by the source before connector normalization.
    pub reported_total_tokens: Option<u64>,
    /// Sum of the normalized non-overlapping token components, when known.
    pub component_total_tokens: Option<u64>,
    /// Versioned description of the source token-counter shape.
    pub token_semantics: Option<String>,
    pub cost_status: Option<String>,
    pub cost_source: Option<String>,
    pub cost_currency: Option<String>,
    pub pricing_version: Option<String>,
}

/// Provider-reported usage for one model invocation.
///
/// Token categories preserve the source harness semantics. `total_tokens` is
/// normalized by each connector to the source's reported total, or to the sum
/// of input, output, cache-read, and cache-write tokens when no total is
/// provided. Reasoning tokens are a subset of output tokens and are therefore
/// not added to the total a second time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub timestamp: Option<i64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Stable source invocation identity when the harness exposes one.
    #[serde(default)]
    pub source_event_id: Option<String>,
    pub api_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub actual_cost_usd: Option<f64>,
    pub estimated_cost_usd: Option<f64>,
    #[serde(default)]
    pub metadata: UsageMetadata,
}

impl UsageRecord {
    pub fn has_usage(&self) -> bool {
        self.api_calls > 0
            || self.total_tokens > 0
            || self.metadata.request_attempts > 0
            || self.actual_cost_usd.is_some()
            || self.estimated_cost_usd.is_some()
    }

    /// Fill canonical dimensions and token/cost provenance without changing
    /// the source-reported provider, model, counters, or costs.
    pub fn enrich_metadata(&mut self) {
        if self.metadata.provider_family.is_none() {
            let (family, source, confidence) = canonical_provider_family(
                self.provider.as_deref(),
                self.metadata.billing_base_url.as_deref(),
            );
            self.metadata.provider_family = family;
            self.metadata.provider_inference_source = source;
            self.metadata.provider_inference_confidence = confidence;
        }
        if self.metadata.model_family.is_none() {
            self.metadata.model_family = canonical_model_family(self.model.as_deref());
        }
        if self.metadata.request_attempts == 0 && self.api_calls > 0 {
            self.metadata.request_attempts = self.api_calls;
        }
        if self.metadata.component_total_tokens.is_none() {
            self.metadata.component_total_tokens = Some(
                self.input_tokens
                    .saturating_add(self.output_tokens)
                    .saturating_add(self.cache_read_tokens)
                    .saturating_add(self.cache_write_tokens),
            );
        }
        let has_positive_actual = self
            .actual_cost_usd
            .is_some_and(|cost| cost.is_finite() && cost > 0.0);
        let has_positive_estimate = self
            .estimated_cost_usd
            .is_some_and(|cost| cost.is_finite() && cost > 0.0);
        let cost_status = self
            .metadata
            .cost_status
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let has_explicit_recorded_actual = self
            .actual_cost_usd
            .is_some_and(|cost| cost.is_finite() && cost >= 0.0)
            && matches!(cost_status.as_str(), "actual" | "reported_actual");
        let has_explicit_recorded_estimate = self
            .estimated_cost_usd
            .is_some_and(|cost| cost.is_finite() && cost >= 0.0)
            && matches!(
                cost_status.as_str(),
                "estimated" | "source_estimated" | "source_reported_zero" | "included"
            );
        if self.metadata.cost_currency.is_none()
            && (has_positive_actual
                || has_positive_estimate
                || has_explicit_recorded_actual
                || has_explicit_recorded_estimate)
        {
            self.metadata.cost_currency = Some("USD".to_string());
        }
        if self.metadata.cost_status.is_none() {
            self.metadata.cost_status = if has_positive_actual {
                Some("reported_actual".to_string())
            } else if has_positive_estimate {
                Some("source_estimated".to_string())
            } else {
                Some("unknown".to_string())
            };
        }
    }
}

/// Resolve a canonical provider family from explicit routing evidence.
///
/// Raw provider identifiers such as adapters and auth routes are preserved on
/// the record. A base URL wins because it identifies the actual inference
/// endpoint. With neither signal, the family remains unknown.
pub fn canonical_provider_family(
    raw_provider: Option<&str>,
    billing_base_url: Option<&str>,
) -> (Option<String>, Option<String>, Option<String>) {
    if let Some(base_url) = billing_base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let normalized = base_url.to_ascii_lowercase();
        let host = route_host(&normalized);
        let family = [
            ("fireworks.ai", "fireworks"),
            ("x.ai", "xai"),
            ("chatgpt.com", "openai"),
            ("openai.com", "openai"),
            ("anthropic.com", "anthropic"),
            ("googleapis.com", "google"),
            ("openrouter.ai", "openrouter"),
            ("mistral.ai", "mistral"),
            ("deepseek.com", "deepseek"),
        ]
        .into_iter()
        .find_map(|(domain, family)| {
            host.as_deref()
                .is_some_and(|host| host == domain || host.ends_with(&format!(".{domain}")))
                .then_some(family)
        });
        if let Some(family) = family {
            return (
                Some(family.to_string()),
                Some("billing_base_url".to_string()),
                Some("high".to_string()),
            );
        }
        return (
            None,
            Some("billing_base_url_unmapped".to_string()),
            Some("none".to_string()),
        );
    }

    let Some(raw) = raw_provider
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return (None, None, None);
    };
    let normalized = raw.to_ascii_lowercase();
    let family = match normalized.as_str() {
        "openai" | "openai-codex" | "azure-openai" => "openai",
        "anthropic" | "bedrock-anthropic" | "vertex-anthropic" => "anthropic",
        "google" | "google-vertex" | "google-antigravity" | "antigravity" | "vertex" => "google",
        "xai" | "x-ai" | "xai-oauth" => "xai",
        "fireworks" | "fireworks-ai" => "fireworks",
        "openrouter" => "openrouter",
        "cursor" => "cursor",
        "github-copilot" | "copilot" => "github-copilot",
        "groq" => "groq",
        "mistral" => "mistral",
        "deepseek" => "deepseek",
        "kimi-coding" | "kimi-for-coding" | "moonshot" | "moonshot-plan" => "moonshot",
        "zai" | "zhipu" | "zai-coding-plan" => "zai",
        "minimax" => "minimax",
        "lmstudio" => "local",
        "cloudflare-workers-ai" => "cloudflare",
        "cerebras" => "cerebras",
        "together" | "together-ai" => "together",
        "cohere" => "cohere",
        // Adapter and subscription labels are deliberately not guessed without
        // route evidence. Raw values remain queryable on UsageRecord.
        "custom" | "auto" | "faux" | "firepass" | "firepass_chat" | "open-hax" | "proxx"
        | "opencode" | "opencode-go" | "unknown" => {
            return (
                None,
                Some("raw_provider_unmapped".to_string()),
                Some("none".to_string()),
            );
        }
        _ => {
            return (
                None,
                Some("raw_provider_unmapped".to_string()),
                Some("none".to_string()),
            );
        }
    };
    (
        Some(family.to_string()),
        Some("raw_provider".to_string()),
        Some("high".to_string()),
    )
}

/// Coarse, stable model family for cross-version trend grouping.
pub fn canonical_model_family(raw_model: Option<&str>) -> Option<String> {
    let raw = raw_model.map(str::trim).filter(|value| !value.is_empty())?;
    let normalized = raw.to_ascii_lowercase();
    let family = if normalized.contains("claude") {
        "claude"
    } else if normalized.contains("gemini") {
        "gemini"
    } else if normalized.contains("grok") {
        "grok"
    } else if normalized.contains("llama") {
        "llama"
    } else if normalized.contains("deepseek") {
        "deepseek"
    } else if normalized.contains("mistral") || normalized.contains("mixtral") {
        "mistral"
    } else if normalized.contains("kimi") {
        "kimi"
    } else if normalized.contains("minimax") {
        "minimax"
    } else if normalized.contains("glm") {
        "glm"
    } else if normalized.contains("composer") {
        "composer"
    } else if normalized.starts_with("gpt-") || normalized.contains("/gpt-") {
        "gpt"
    } else if ["o1", "o3", "o4"]
        .iter()
        .any(|prefix| normalized == *prefix || normalized.starts_with(&format!("{prefix}-")))
    {
        "openai-o"
    } else if normalized.contains("qwen") {
        "qwen"
    } else if normalized.contains("command-r") {
        "command-r"
    } else {
        return None;
    };
    Some(family.to_string())
}

pub(crate) fn route_host(value: &str) -> Option<String> {
    let authority_and_path = value
        .split_once("://")
        .map(|(_, remainder)| remainder)
        .unwrap_or(value);
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()?
        .rsplit('@')
        .next()?;
    let host = if authority.starts_with('[') {
        authority
            .strip_prefix('[')?
            .split_once(']')
            .map(|(host, _)| host)?
    } else {
        authority.split(':').next()?
    };
    (!host.is_empty()).then(|| host.trim_end_matches('.').to_string())
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
    #[serde(default)]
    pub usage: Vec<UsageRecord>,
    #[serde(default)]
    pub metadata: ConversationMetadata,
}

/// Relationship and provenance of one physical transcript record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationMetadata {
    /// Stable identity shared by physical transcript records in one logical
    /// session (for example Claude child-agent files and their parent).
    pub logical_session_id: Option<String>,
    pub parent_external_id: Option<String>,
    /// `top_level`, `child_agent`, `automation`, `test`, or `recovered`.
    pub record_kind: Option<String>,
    pub is_synthetic: bool,
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
        self.source_files.iter().map(|f| f.mtime).max().unwrap_or(0)
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
            } else if let Ok(dt) =
                chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%:z")
            {
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
        assert_eq!(Agent::Hermes.slug(), "hermes");
        assert_eq!(Agent::OpenCode.slug(), "opencode");
        assert_eq!(Agent::PiAgent.slug(), "pi_agent");
    }

    #[test]
    fn test_agent_display_name() {
        assert_eq!(Agent::ClaudeCode.display_name(), "Claude Code");
        assert_eq!(Agent::Codex.display_name(), "Codex");
        assert_eq!(Agent::Hermes.display_name(), "Hermes Agent");
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
        assert_eq!("hermes".parse::<Agent>().unwrap(), Agent::Hermes);
        assert_eq!("hermes_agent".parse::<Agent>().unwrap(), Agent::Hermes);
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
        let icons: Vec<&str> = [
            Agent::ClaudeCode,
            Agent::Codex,
            Agent::Hermes,
            Agent::OpenCode,
            Agent::PiAgent,
        ]
        .iter()
        .map(|a| a.icon())
        .collect();
        assert_eq!(icons.len(), 5);
        for i in 0..icons.len() {
            for j in (i + 1)..icons.len() {
                assert_ne!(icons[i], icons[j]);
            }
        }

        // Each agent also has a distinct RGB color.
        let colors: Vec<(u8, u8, u8)> = [
            Agent::ClaudeCode,
            Agent::Codex,
            Agent::Hermes,
            Agent::OpenCode,
            Agent::PiAgent,
        ]
        .iter()
        .map(Agent::color_code)
        .collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j]);
            }
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
            usage: vec![],
            metadata: Default::default(),
        }
    }

    #[test]
    fn test_first_user_message() {
        let conv = make_conv(vec![
            Message {
                idx: 0,
                role: Role::System,
                content: "System msg".into(),
                timestamp: None,
                model: None,
            },
            Message {
                idx: 1,
                role: Role::User,
                content: "Hello!".into(),
                timestamp: None,
                model: None,
            },
            Message {
                idx: 2,
                role: Role::User,
                content: "Second".into(),
                timestamp: None,
                model: None,
            },
        ]);
        assert_eq!(conv.first_user_message(), Some("Hello!"));
    }

    #[test]
    fn test_first_user_message_none() {
        let conv = make_conv(vec![Message {
            idx: 0,
            role: Role::Assistant,
            content: "Hi".into(),
            timestamp: None,
            model: None,
        }]);
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
        let conv = make_conv(vec![Message {
            idx: 0,
            role: Role::User,
            content: "Help me with auth\nMore details...".into(),
            timestamp: None,
            model: None,
        }]);
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
            Message {
                idx: 0,
                role: Role::User,
                content: "Hello".into(),
                timestamp: None,
                model: None,
            },
            Message {
                idx: 1,
                role: Role::Assistant,
                content: "World".into(),
                timestamp: None,
                model: None,
            },
        ]);
        let text = conv.full_text();
        assert!(text.contains("[user] Hello"));
        assert!(text.contains("[assistant] World"));
    }

    #[test]
    fn test_preview_short() {
        let conv = make_conv(vec![Message {
            idx: 0,
            role: Role::User,
            content: "Short".into(),
            timestamp: None,
            model: None,
        }]);
        let preview = conv.preview();
        assert!(!preview.ends_with("..."));
    }

    #[test]
    fn test_preview_long_truncated() {
        let long_msg = "x".repeat(500);
        let conv = make_conv(vec![Message {
            idx: 0,
            role: Role::User,
            content: long_msg,
            timestamp: None,
            model: None,
        }]);
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
                SourceFile {
                    path: PathBuf::from("/a"),
                    mtime: 100,
                    size: 10,
                },
                SourceFile {
                    path: PathBuf::from("/b"),
                    mtime: 300,
                    size: 20,
                },
                SourceFile {
                    path: PathBuf::from("/c"),
                    mtime: 200,
                    size: 30,
                },
            ],
            source_fingerprint: "x".into(),
            started_at: None,
            ended_at: None,
            messages: vec![],
            usage: vec![],
            metadata: Default::default(),
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
            SourceFile {
                path: PathBuf::from("/a/b.json"),
                mtime: 1000,
                size: 500,
            },
            SourceFile {
                path: PathBuf::from("/c/d.json"),
                mtime: 2000,
                size: 1000,
            },
        ];
        let fp1 = source_fingerprint(&files);
        let fp2 = source_fingerprint(&files);
        assert_eq!(fp1, fp2);

        // Order shouldn't matter
        let files_rev = vec![
            SourceFile {
                path: PathBuf::from("/c/d.json"),
                mtime: 2000,
                size: 1000,
            },
            SourceFile {
                path: PathBuf::from("/a/b.json"),
                mtime: 1000,
                size: 500,
            },
        ];
        let fp3 = source_fingerprint(&files_rev);
        assert_eq!(fp1, fp3);
    }

    #[test]
    fn test_source_fingerprint_changes_on_mtime() {
        let files1 = vec![SourceFile {
            path: PathBuf::from("/a"),
            mtime: 1000,
            size: 500,
        }];
        let files2 = vec![SourceFile {
            path: PathBuf::from("/a"),
            mtime: 2000,
            size: 500,
        }];
        assert_ne!(source_fingerprint(&files1), source_fingerprint(&files2));
    }

    #[test]
    fn test_source_fingerprint_changes_on_size() {
        let files1 = vec![SourceFile {
            path: PathBuf::from("/a"),
            mtime: 1000,
            size: 500,
        }];
        let files2 = vec![SourceFile {
            path: PathBuf::from("/a"),
            mtime: 1000,
            size: 600,
        }];
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

    #[test]
    fn canonical_provider_requires_a_known_route_or_provider() {
        assert_eq!(
            canonical_provider_family(
                Some("openai"),
                Some("https://api.openai.com.example.net/v1")
            )
            .0,
            None
        );
        assert_eq!(
            canonical_provider_family(Some("custom"), Some("https://api.openai.com/v1")).0,
            Some("openai".to_string())
        );
        let unmapped = canonical_provider_family(Some("open-hax"), None);
        assert_eq!(unmapped.0, None);
        assert_eq!(unmapped.1.as_deref(), Some("raw_provider_unmapped"));
    }

    #[test]
    fn canonical_model_family_does_not_relabel_unknown_ids_as_families() {
        assert_eq!(
            canonical_model_family(Some("gpt-5.6-sol")),
            Some("gpt".to_string())
        );
        assert_eq!(canonical_model_family(Some("private-router-model")), None);
    }

    #[test]
    fn usage_metadata_only_infers_cost_provenance_from_positive_finite_values() {
        let mut reported = UsageRecord {
            actual_cost_usd: Some(1.25),
            ..UsageRecord::default()
        };
        reported.enrich_metadata();
        assert_eq!(
            reported.metadata.cost_status.as_deref(),
            Some("reported_actual")
        );
        assert_eq!(reported.metadata.cost_currency.as_deref(), Some("USD"));

        let mut estimated = UsageRecord {
            estimated_cost_usd: Some(0.75),
            ..UsageRecord::default()
        };
        estimated.enrich_metadata();
        assert_eq!(
            estimated.metadata.cost_status.as_deref(),
            Some("source_estimated")
        );
        assert_eq!(estimated.metadata.cost_currency.as_deref(), Some("USD"));

        for invalid in [0.0, -1.0, f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let mut record = UsageRecord {
                actual_cost_usd: Some(invalid),
                ..UsageRecord::default()
            };
            record.enrich_metadata();
            assert_eq!(record.metadata.cost_status.as_deref(), Some("unknown"));
            assert_eq!(record.metadata.cost_currency, None);
        }
    }

    #[test]
    fn usage_metadata_preserves_explicit_zero_cost_status() {
        let mut record = UsageRecord {
            estimated_cost_usd: Some(0.0),
            metadata: UsageMetadata {
                cost_status: Some("source_reported_zero".to_string()),
                ..UsageMetadata::default()
            },
            ..UsageRecord::default()
        };

        record.enrich_metadata();

        assert_eq!(
            record.metadata.cost_status.as_deref(),
            Some("source_reported_zero")
        );
        assert_eq!(record.metadata.cost_currency.as_deref(), Some("USD"));
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
