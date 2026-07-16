use std::path::PathBuf;
use std::time::Duration;

use chrono::TimeZone;
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::search::RankingMode;
use crate::usage::UsageBucket;

/// Default freshness threshold for auto-refresh.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(15 * 60);

/// Parse `--max-age` values using humantime (e.g. "5m", "1h", "2h30m").
pub fn parse_max_age(s: &str) -> Result<Duration, String> {
    humantime::parse_duration(s).map_err(|e| format!("invalid duration '{}': {}", s, e))
}

/// Session search for coding agents
#[derive(Parser, Debug)]
#[command(name = "sess")]
#[command(about = "Search across your coding agent sessions")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Skip auto-indexing on startup
    #[arg(long, global = true)]
    pub no_auto_index: bool,

    /// Data directory
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,

    /// Disable semantic search
    #[arg(long, global = true)]
    pub no_semantic: bool,

    /// Suppress age-based auto-refresh on `search`, `usage`, and TUI launch.
    ///
    /// Explicit `sess index` is unaffected. Implied by `--no-auto-index`.
    #[arg(long, global = true)]
    pub no_refresh: bool,

    /// Max acceptable index staleness before auto-refresh kicks in.
    ///
    /// Examples: `5m`, `1h`, `2h30m`. Default: 15m.
    #[arg(long, global = true, value_name = "DURATION", value_parser = parse_max_age)]
    pub max_age: Option<Duration>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Search for sessions
    Search {
        /// Search query
        query: String,

        /// Filter by agent
        #[arg(short, long, value_name = "AGENT")]
        agent: Option<String>,

        /// Filter by workspace
        #[arg(short, long, value_name = "PATH")]
        workspace: Option<String>,

        /// Filter by start date (ISO date, "7d", "30d", "today")
        #[arg(long, value_name = "DATE")]
        since: Option<String>,

        /// Filter by end date
        #[arg(long, value_name = "DATE")]
        until: Option<String>,

        /// Number of results
        #[arg(short, long, default_value = "20")]
        limit: usize,

        /// Result offset for pagination
        #[arg(long, default_value = "0")]
        offset: usize,

        /// Ranking mode
        #[arg(short, long, value_enum, default_value = "recent")]
        ranking: RankingModeArg,

        /// Enable semantic search (hybrid mode)
        #[arg(long)]
        semantic: bool,

        /// RRF constant for hybrid ranking
        #[arg(long, default_value = "60")]
        rrf_k: u32,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Run indexing
    Index {
        /// Full reindex from scratch
        #[arg(long)]
        full: bool,

        /// Rebuild from SQLite (no rescan)
        #[arg(long)]
        rebuild: bool,

        /// Show what would change without writing to SQLite or Tantivy.
        #[arg(long)]
        dry_run: bool,
    },

    /// Show index statistics
    Stats {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Summarize provider, model, harness, token, and cost usage
    Usage {
        /// Filter by harness (repeat for multiple values)
        #[arg(short, long, visible_alias = "harness", value_name = "AGENT")]
        agent: Vec<String>,

        /// Filter by provider (repeat for multiple values)
        #[arg(short, long, value_name = "PROVIDER")]
        provider: Vec<String>,

        /// Filter by model (repeat for multiple values)
        #[arg(short, long, value_name = "MODEL")]
        model: Vec<String>,

        /// Filter by exact workspace path
        #[arg(short, long, value_name = "PATH")]
        workspace: Option<String>,

        /// Filter by start date (ISO date, "7d", "30d", "today")
        #[arg(long, value_name = "DATE")]
        since: Option<String>,

        /// Filter by end date
        #[arg(long, value_name = "DATE")]
        until: Option<String>,

        /// Timeline bucket size
        #[arg(long, value_enum, default_value = "auto")]
        bucket: UsageBucketArg,

        /// Maximum displayed rows per breakdown (terminal/HTML; JSON stays complete)
        #[arg(long, default_value = "10")]
        top: usize,

        /// Output the renderer-independent report as JSON
        #[arg(long, conflicts_with = "html")]
        json: bool,

        /// Write a standalone HTML report
        #[arg(long, value_name = "PATH", conflicts_with = "json")]
        html: Option<PathBuf>,
    },

    /// List detected agents
    Agents {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// View a specific conversation
    View {
        /// Source path or conversation ID
        path_or_id: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RankingModeArg {
    Recent,
    Balanced,
    Relevance,
    Newest,
    Oldest,
}

impl From<RankingModeArg> for RankingMode {
    fn from(arg: RankingModeArg) -> Self {
        match arg {
            RankingModeArg::Recent => RankingMode::RecentHeavy,
            RankingModeArg::Balanced => RankingMode::Balanced,
            RankingModeArg::Relevance => RankingMode::Relevance,
            RankingModeArg::Newest => RankingMode::Newest,
            RankingModeArg::Oldest => RankingMode::Oldest,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum UsageBucketArg {
    Auto,
    Day,
    Week,
    Month,
}

impl From<UsageBucketArg> for UsageBucket {
    fn from(arg: UsageBucketArg) -> Self {
        match arg {
            UsageBucketArg::Auto => UsageBucket::Auto,
            UsageBucketArg::Day => UsageBucket::Day,
            UsageBucketArg::Week => UsageBucket::Week,
            UsageBucketArg::Month => UsageBucket::Month,
        }
    }
}

/// Parse a date string (ISO date, relative like "7d", or "today")
pub fn parse_date(s: &str) -> anyhow::Result<i64> {
    let now = chrono::Local::now();

    if s == "today" {
        return local_midnight(now.date_naive());
    }

    // Try relative format like "7d" or "30d"
    if let Some(days_str) = s.strip_suffix('d')
        && let Ok(days) = days_str.parse::<i64>()
    {
        if days < 0 {
            anyhow::bail!("Relative day count must be non-negative: {s}");
        }
        let duration = chrono::Duration::try_days(days)
            .ok_or_else(|| anyhow::anyhow!("Relative date is outside the supported range: {s}"))?;
        let then = now
            .checked_sub_signed(duration)
            .ok_or_else(|| anyhow::anyhow!("Relative date is outside the supported range: {s}"))?;
        return Ok(then.timestamp_millis());
    }

    // Try ISO date format
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return local_midnight(date);
    }

    // Try full ISO datetime
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp_millis());
    }

    anyhow::bail!(
        "Invalid date format: {}. Use ISO date (2024-01-01), relative (7d), or 'today'",
        s
    )
}

/// Parse an inclusive upper date bound. Date-only values include the complete
/// named local calendar day; relative and RFC 3339 values remain exact times.
pub fn parse_until_date(s: &str) -> anyhow::Result<i64> {
    let date = if s == "today" {
        Some(chrono::Local::now().date_naive())
    } else {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
    };
    if let Some(date) = date {
        let next_day = date
            .succ_opt()
            .ok_or_else(|| anyhow::anyhow!("Date is outside the supported range: {s}"))?;
        return Ok(local_midnight(next_day)?.saturating_sub(1));
    }
    parse_date(s)
}

fn local_midnight(date: chrono::NaiveDate) -> anyhow::Result<i64> {
    let local = date
        .and_hms_opt(0, 0, 0)
        .and_then(|datetime| chrono::Local.from_local_datetime(&datetime).earliest())
        .ok_or_else(|| anyhow::anyhow!("Could not resolve local midnight for {date}"))?;
    Ok(local.timestamp_millis())
}

#[derive(Serialize)]
pub struct SearchOutput {
    pub query: String,
    pub total_hits: usize,
    pub query_time_ms: u64,
    pub hits: Vec<SearchHitOutput>,
}

#[derive(Serialize)]
pub struct SearchHitOutput {
    pub id: i64,
    pub agent: String,
    pub title: String,
    pub workspace: Option<String>,
    pub source_path: String,
    pub preview: String,
    pub created_at: String,
    pub score: f32,
    pub snippet: Option<String>,
}

#[derive(Serialize)]
pub struct StatsOutput {
    pub agents: std::collections::HashMap<String, AgentStatsOutput>,
    pub total_conversations: usize,
    pub total_messages: usize,
    pub index_size_bytes: u64,
    pub last_indexed_at: Option<String>,
}

#[derive(Serialize)]
pub struct AgentStatsOutput {
    pub conversations: usize,
    pub messages: usize,
}

#[derive(Serialize)]
pub struct AgentInfoOutput {
    pub slug: String,
    pub name: String,
    pub detected: bool,
}

impl SearchOutput {
    pub fn from_results(results: crate::search::SearchResults, query: &str) -> Self {
        Self {
            query: query.to_string(),
            total_hits: results.total_hits,
            query_time_ms: results.query_time_ms,
            hits: results
                .hits
                .into_iter()
                .map(|h| SearchHitOutput {
                    id: h.conversation_id,
                    agent: h.agent.slug().to_string(),
                    title: h.title.unwrap_or_else(|| "Untitled".to_string()),
                    workspace: h.workspace,
                    source_path: h.source_path,
                    preview: h.preview,
                    created_at: h
                        .created_at
                        .map(|ts| {
                            chrono::DateTime::from_timestamp_millis(ts)
                                .map(|dt| dt.to_rfc3339())
                                .unwrap_or_default()
                        })
                        .unwrap_or_default(),
                    score: h.score,
                    snippet: h.snippet,
                })
                .collect(),
        }
    }
}

impl StatsOutput {
    pub fn from_storage_stats(stats: crate::storage::StorageStats) -> Self {
        use std::collections::HashMap;

        let agents: HashMap<String, AgentStatsOutput> = stats
            .by_agent
            .into_iter()
            .map(|(agent, s)| {
                (
                    agent.slug().to_string(),
                    AgentStatsOutput {
                        conversations: s.conversations,
                        messages: s.messages,
                    },
                )
            })
            .collect();

        Self {
            agents,
            total_conversations: stats.total_conversations,
            total_messages: stats.total_messages,
            index_size_bytes: stats.db_size_bytes,
            last_indexed_at: stats.last_indexed_at.map(|ts| {
                chrono::DateTime::from_timestamp_millis(ts)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn max_age_parses_valid_durations() {
        assert_eq!(parse_max_age("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_max_age("1h"), Ok(Duration::from_secs(3600)));
        assert_eq!(parse_max_age("2h30m"), Ok(Duration::from_secs(9000)));
        assert_eq!(parse_max_age("0s"), Ok(Duration::ZERO));
    }

    #[test]
    fn max_age_rejects_garbage() {
        assert!(parse_max_age("forever").is_err());
        assert!(parse_max_age("").is_err());
        assert!(parse_max_age("5banana").is_err());
    }

    #[test]
    fn cli_parses_no_refresh_and_max_age() {
        let cli = Cli::try_parse_from(["sess", "--no-refresh", "--max-age", "1h", "stats"])
            .expect("parse");
        assert!(cli.no_refresh);
        assert_eq!(cli.max_age, Some(Duration::from_secs(3600)));
        assert!(!cli.no_auto_index);
    }

    #[test]
    fn cli_parses_no_auto_index_alone() {
        let cli = Cli::try_parse_from(["sess", "--no-auto-index", "stats"]).expect("parse");
        assert!(cli.no_auto_index);
        assert!(!cli.no_refresh);
        assert_eq!(cli.max_age, None);
    }

    #[test]
    fn cli_parses_index_dry_run() {
        let cli = Cli::try_parse_from(["sess", "index", "--dry-run"]).expect("parse");
        match cli.command {
            Some(Commands::Index {
                dry_run,
                full,
                rebuild,
            }) => {
                assert!(dry_run);
                assert!(!full);
                assert!(!rebuild);
            }
            _ => panic!("expected Index"),
        }
    }

    #[test]
    fn date_only_bounds_use_local_midnight_and_include_the_until_day() {
        let start = parse_date("2026-07-16").unwrap();
        let until = parse_until_date("2026-07-16").unwrap();
        let local_start = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(start)
            .unwrap()
            .with_timezone(&chrono::Local);
        let local_after_until = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(until + 1)
            .unwrap()
            .with_timezone(&chrono::Local);

        assert_eq!(local_start.date_naive().to_string(), "2026-07-16");
        assert_eq!(local_start.time(), chrono::NaiveTime::MIN);
        assert_eq!(local_after_until.date_naive().to_string(), "2026-07-17");
        assert_eq!(local_after_until.time(), chrono::NaiveTime::MIN);
    }

    #[test]
    fn relative_dates_reject_negative_and_out_of_range_values() {
        assert!(parse_date("-1d").is_err());
        assert!(parse_date("9223372036854775807d").is_err());
    }
}
