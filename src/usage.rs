//! Usage analytics domain model and renderers.
//!
//! The report is deliberately renderer-agnostic: terminal, JSON (through
//! `serde`), and HTML all consume the same [`UsageReport`]. This keeps totals
//! and coverage caveats consistent across every output format.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::model::Agent;

const UNKNOWN_KEY: &str = "__unknown__";
const UNKNOWN_LABEL: &str = "Unknown";

/// Calendar granularity used for the report timeline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageBucket {
    #[default]
    Auto,
    Day,
    Week,
    Month,
}

impl UsageBucket {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }
}

/// Provider-reported token counters.
///
/// Input excludes cache buckets. Output includes reasoning, while `reasoning`
/// annotates that subset and must not be added to output again. The explicit
/// `total` remains provider-reported and authoritative; it is not reconstructed
/// here because some sources only expose a subset of components.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    pub total: u64,
}

impl TokenCounts {
    pub fn has_usage(&self) -> bool {
        self.total > 0
            || self.input > 0
            || self.output > 0
            || self.cache_read > 0
            || self.cache_write > 0
            || self.reasoning > 0
    }

    fn add_assign_saturating(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
        self.total = self.total.saturating_add(other.total);
    }
}

/// One normalized usage observation loaded from SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEventRow {
    pub conversation_id: i64,
    pub agent: Agent,
    pub workspace: Option<String>,
    pub timestamp: Option<i64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Stable source invocation identity used to suppress copied source rows.
    #[serde(skip)]
    pub source_event_id: Option<String>,
    /// Number of provider API calls represented by this row.
    pub api_calls: u64,
    pub tokens: TokenCounts,
    pub actual_cost_usd: Option<f64>,
    pub estimated_cost_usd: Option<f64>,
}

/// Raw usage rows plus corpus-level context used to communicate coverage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageDataset {
    pub events: Vec<UsageEventRow>,
    pub indexed_conversations: u64,
    pub indexed_messages: u64,
}

/// Filters applied before any report aggregation.
///
/// Values within a dimension are ORed; dimensions are ANDed together.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageFilters {
    pub agents: Vec<Agent>,
    pub providers: Vec<String>,
    pub models: Vec<String>,
    pub workspace: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub bucket: UsageBucket,
}

/// Requested and observed time bounds for a report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRange {
    pub requested_since: Option<i64>,
    pub requested_until: Option<i64>,
    pub observed_since: Option<i64>,
    pub observed_until: Option<i64>,
    /// Resolved calendar bucket. This is never `auto` in a built report.
    pub bucket: UsageBucket,
    /// True when an extremely wide requested timeline omits zero-only buckets
    /// while preserving every observed bucket and both range boundaries.
    pub trend_is_sparse: bool,
}

/// Cost values carried by a total or one breakdown row.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageCost {
    /// Actual cost reported by a source, where available.
    pub actual_usd: Option<f64>,
    /// Cost estimate recorded by the source harness, where available.
    pub estimated_usd: Option<f64>,
    /// Actual cost when present for an event, otherwise its estimate.
    pub accounted_usd: Option<f64>,
    pub actual_events: u64,
    pub estimated_events: u64,
    pub covered_tokens: u64,
}

/// Aggregate totals for the selected usage rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    pub events: u64,
    pub api_calls: u64,
    pub conversations: u64,
    pub tokens: TokenCounts,
    pub cost: UsageCost,
}

/// Completeness indicators that prevent partial source data from looking exact.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageCoverage {
    pub events_with_tokens: u64,
    pub api_calls_with_provider: u64,
    pub api_calls_with_model: u64,
    pub events_with_timestamp: u64,
    pub timestamped_tokens: u64,
    pub token_event_percent: f64,
    pub provider_percent: f64,
    pub model_percent: f64,
    pub timestamp_percent: f64,
    pub timestamp_token_percent: f64,
    pub cost_token_percent: f64,
}

/// One harness, provider, or model aggregate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageBreakdown {
    /// Stable machine key (`codex`, normalized provider/model, or `__unknown__`).
    pub key: String,
    pub label: String,
    pub events: u64,
    pub api_calls: u64,
    pub conversations: u64,
    pub tokens: TokenCounts,
    pub cost: UsageCost,
}

/// One UTC calendar bucket in the trend series.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageTrendPoint {
    pub bucket_start: i64,
    pub label: String,
    pub events: u64,
    pub api_calls: u64,
    pub conversations: u64,
    pub tokens: TokenCounts,
    pub cost: UsageCost,
}

/// Complete renderer-independent usage report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageReport {
    pub generated_at: i64,
    pub filters: UsageFilters,
    pub range: UsageRange,
    pub totals: UsageTotals,
    pub coverage: UsageCoverage,
    pub by_harness: Vec<UsageBreakdown>,
    pub by_provider: Vec<UsageBreakdown>,
    pub by_model: Vec<UsageBreakdown>,
    pub trend: Vec<UsageTrendPoint>,
    pub indexed_conversations: u64,
    pub indexed_messages: u64,
}

#[derive(Debug, Default)]
struct CostAccumulator {
    actual_usd: f64,
    estimated_usd: f64,
    accounted_usd: f64,
    actual_events: u64,
    estimated_events: u64,
    accounted_events: u64,
    covered_tokens: u64,
}

impl CostAccumulator {
    fn add(&mut self, event: &UsageEventRow) {
        let actual = event.actual_cost_usd.filter(|cost| valid_cost(*cost));
        let estimated = event.estimated_cost_usd.filter(|cost| valid_cost(*cost));

        if let Some(cost) = actual {
            self.actual_usd += cost;
            self.actual_events += 1;
        }
        if let Some(cost) = estimated {
            self.estimated_usd += cost;
            self.estimated_events += 1;
        }
        if let Some(cost) = actual.or(estimated) {
            self.accounted_usd += cost;
            self.accounted_events += 1;
            self.covered_tokens = self.covered_tokens.saturating_add(event.tokens.total);
        }
    }

    fn finish(self) -> UsageCost {
        UsageCost {
            actual_usd: (self.actual_events > 0).then_some(self.actual_usd),
            estimated_usd: (self.estimated_events > 0).then_some(self.estimated_usd),
            accounted_usd: (self.accounted_events > 0).then_some(self.accounted_usd),
            actual_events: self.actual_events,
            estimated_events: self.estimated_events,
            covered_tokens: self.covered_tokens,
        }
    }
}

#[derive(Debug, Default)]
struct AggregateAccumulator {
    events: u64,
    api_calls: u64,
    conversations: HashSet<i64>,
    tokens: TokenCounts,
    cost: CostAccumulator,
}

impl AggregateAccumulator {
    fn add(&mut self, event: &UsageEventRow) {
        self.events += 1;
        self.api_calls = self.api_calls.saturating_add(event.api_calls);
        self.conversations.insert(event.conversation_id);
        self.tokens.add_assign_saturating(&event.tokens);
        self.cost.add(event);
    }

    fn finish(self) -> UsageTotals {
        UsageTotals {
            events: self.events,
            api_calls: self.api_calls,
            conversations: self.conversations.len() as u64,
            tokens: self.tokens,
            cost: self.cost.finish(),
        }
    }
}

#[derive(Debug, Default)]
struct GroupAccumulator {
    label: Option<String>,
    aggregate: AggregateAccumulator,
}

impl GroupAccumulator {
    fn add(&mut self, label: &str, event: &UsageEventRow) {
        match self.label.as_ref() {
            Some(existing) if existing.as_str() <= label => {}
            _ => self.label = Some(label.to_string()),
        }
        self.aggregate.add(event);
    }

    fn finish(self, key: String) -> UsageBreakdown {
        let totals = self.aggregate.finish();
        UsageBreakdown {
            key,
            label: self.label.unwrap_or_else(|| UNKNOWN_LABEL.to_string()),
            events: totals.events,
            api_calls: totals.api_calls,
            conversations: totals.conversations,
            tokens: totals.tokens,
            cost: totals.cost,
        }
    }
}

/// Build a deterministic usage report from normalized usage events.
pub fn build_report(dataset: &UsageDataset, filters: &UsageFilters) -> UsageReport {
    let generated_at = Utc::now().timestamp_millis();
    let selected = deduplicated_events(
        dataset
            .events
            .iter()
            .filter(|event| event_matches(event, filters)),
    );

    let observed_since = selected.iter().filter_map(|event| event.timestamp).min();
    let observed_until = selected.iter().filter_map(|event| event.timestamp).max();
    let timeline_since = filters.since.or(observed_since);
    let timeline_until = filters
        .until
        .or_else(|| {
            filters
                .since
                .map(|_| observed_until.unwrap_or(generated_at).max(generated_at))
        })
        .or(observed_until);
    let bucket = resolve_bucket(filters.bucket, timeline_since, timeline_until);

    let mut totals = AggregateAccumulator::default();
    let mut harnesses: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut providers: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut models: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut timeline: BTreeMap<i64, AggregateAccumulator> = BTreeMap::new();

    let mut events_with_tokens = 0_u64;
    let mut api_calls_with_provider = 0_u64;
    let mut api_calls_with_model = 0_u64;
    let mut events_with_timestamp = 0_u64;
    let mut timestamped_tokens = 0_u64;

    for event in &selected {
        totals.add(event);

        if event.tokens.has_usage() {
            events_with_tokens += 1;
        }
        if dimension_is_known(event.provider.as_deref()) {
            api_calls_with_provider = api_calls_with_provider.saturating_add(event.api_calls);
        }
        if dimension_is_known(event.model.as_deref()) {
            api_calls_with_model = api_calls_with_model.saturating_add(event.api_calls);
        }

        let harness_key = event.agent.slug().to_string();
        harnesses
            .entry(harness_key)
            .or_default()
            .add(event.agent.display_name(), event);

        let (provider_key, provider_label) = dimension_key(event.provider.as_deref());
        providers
            .entry(provider_key)
            .or_default()
            .add(&provider_label, event);

        let (model_key, model_label) = dimension_key(event.model.as_deref());
        models
            .entry(model_key)
            .or_default()
            .add(&model_label, event);

        if let Some(timestamp) = event.timestamp {
            events_with_timestamp += 1;
            timestamped_tokens = timestamped_tokens.saturating_add(event.tokens.total);
            if let Some(start) = bucket_start(timestamp, bucket) {
                timeline.entry(start).or_default().add(event);
            }
        }
    }

    let totals = totals.finish();
    let event_count = totals.events;
    let api_call_count = totals.api_calls;
    let coverage = UsageCoverage {
        events_with_tokens,
        api_calls_with_provider,
        api_calls_with_model,
        events_with_timestamp,
        timestamped_tokens,
        token_event_percent: percent(events_with_tokens, event_count),
        provider_percent: percent(api_calls_with_provider, api_call_count),
        model_percent: percent(api_calls_with_model, api_call_count),
        timestamp_percent: percent(events_with_timestamp, event_count),
        timestamp_token_percent: percent(timestamped_tokens, totals.tokens.total),
        cost_token_percent: percent(totals.cost.covered_tokens, totals.tokens.total),
    };

    let mut by_harness = finish_breakdowns(harnesses);
    let mut by_provider = finish_breakdowns(providers);
    let mut by_model = finish_breakdowns(models);
    sort_breakdowns(&mut by_harness);
    sort_breakdowns(&mut by_provider);
    sort_breakdowns(&mut by_model);

    let (trend, trend_is_sparse) =
        finish_timeline(timeline, bucket, timeline_since, timeline_until);

    UsageReport {
        generated_at,
        filters: filters.clone(),
        range: UsageRange {
            requested_since: filters.since,
            requested_until: filters.until,
            observed_since,
            observed_until,
            bucket,
            trend_is_sparse,
        },
        totals,
        coverage,
        by_harness,
        by_provider,
        by_model,
        trend,
        indexed_conversations: dataset.indexed_conversations,
        indexed_messages: dataset.indexed_messages,
    }
}

fn deduplicated_events<'a>(
    events: impl IntoIterator<Item = &'a UsageEventRow>,
) -> Vec<&'a UsageEventRow> {
    let mut identified: HashMap<(Agent, &str), &UsageEventRow> = HashMap::new();
    let mut output = Vec::new();

    for event in events {
        let Some(source_event_id) = event
            .source_event_id
            .as_deref()
            .map(str::trim)
            .filter(|identity| !identity.is_empty())
        else {
            output.push(event);
            continue;
        };
        identified
            .entry((event.agent, source_event_id))
            .and_modify(|existing| {
                if event_preference(event) > event_preference(existing) {
                    *existing = event;
                }
            })
            .or_insert(event);
    }

    output.extend(identified.into_values());
    output.sort_by_key(|event| {
        (
            event.timestamp.unwrap_or(i64::MIN),
            event.conversation_id,
            event.source_event_id.as_deref().unwrap_or(""),
        )
    });
    output
}

fn event_preference(event: &UsageEventRow) -> (u64, u8, bool, bool, bool, i64, Reverse<i64>) {
    let component_detail = [
        event.tokens.input,
        event.tokens.output,
        event.tokens.cache_read,
        event.tokens.cache_write,
        event.tokens.reasoning,
    ]
    .into_iter()
    .filter(|value| *value > 0)
    .count() as u8;
    (
        event.tokens.total,
        component_detail,
        dimension_is_known(event.provider.as_deref()),
        dimension_is_known(event.model.as_deref()),
        event.actual_cost_usd.is_some_and(valid_cost)
            || event.estimated_cost_usd.is_some_and(valid_cost),
        event.timestamp.unwrap_or(i64::MIN),
        Reverse(event.conversation_id),
    )
}

fn event_matches(event: &UsageEventRow, filters: &UsageFilters) -> bool {
    if !filters.agents.is_empty() && !filters.agents.contains(&event.agent) {
        return false;
    }
    if !dimension_matches(event.provider.as_deref(), &filters.providers) {
        return false;
    }
    if !dimension_matches(event.model.as_deref(), &filters.models) {
        return false;
    }
    if let Some(workspace) = filters.workspace.as_deref()
        && event.workspace.as_deref() != Some(workspace)
    {
        return false;
    }
    if filters.since.is_some() || filters.until.is_some() {
        let Some(timestamp) = event.timestamp else {
            return false;
        };
        if filters.since.is_some_and(|since| timestamp < since)
            || filters.until.is_some_and(|until| timestamp > until)
        {
            return false;
        }
    }
    true
}

fn dimension_matches(value: Option<&str>, filters: &[String]) -> bool {
    filters.is_empty()
        || filters.iter().any(|filter| {
            let (_, label) = dimension_key(value);
            label.eq_ignore_ascii_case(filter.trim())
                || (label == UNKNOWN_LABEL && filter.trim().eq_ignore_ascii_case("unknown"))
        })
}

fn dimension_is_known(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

fn dimension_key(value: Option<&str>) -> (String, String) {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => (value.to_lowercase(), value.to_string()),
        None => (UNKNOWN_KEY.to_string(), UNKNOWN_LABEL.to_string()),
    }
}

fn finish_breakdowns(groups: BTreeMap<String, GroupAccumulator>) -> Vec<UsageBreakdown> {
    groups
        .into_iter()
        .map(|(key, group)| group.finish(key))
        .collect()
}

fn sort_breakdowns(rows: &mut [UsageBreakdown]) {
    rows.sort_by(|left, right| {
        right
            .tokens
            .total
            .cmp(&left.tokens.total)
            .then_with(|| right.api_calls.cmp(&left.api_calls))
            .then_with(|| left.key.cmp(&right.key))
    });
}

fn resolve_bucket(
    requested: UsageBucket,
    observed_since: Option<i64>,
    observed_until: Option<i64>,
) -> UsageBucket {
    if requested != UsageBucket::Auto {
        return requested;
    }
    let span = observed_since
        .zip(observed_until)
        .map(|(start, end)| end.saturating_sub(start))
        .unwrap_or(0);
    if span <= Duration::days(60).num_milliseconds() {
        UsageBucket::Day
    } else if span <= Duration::days(730).num_milliseconds() {
        UsageBucket::Week
    } else {
        UsageBucket::Month
    }
}

fn bucket_start(timestamp: i64, bucket: UsageBucket) -> Option<i64> {
    let datetime = DateTime::<Utc>::from_timestamp_millis(timestamp)?;
    let date = match bucket {
        UsageBucket::Auto | UsageBucket::Day => datetime.date_naive(),
        UsageBucket::Week => {
            datetime.date_naive() - Duration::days(datetime.weekday().num_days_from_monday() as i64)
        }
        UsageBucket::Month => {
            chrono::NaiveDate::from_ymd_opt(datetime.year(), datetime.month(), 1)?
        }
    };
    let midnight = date.and_hms_opt(0, 0, 0)?;
    Some(Utc.from_utc_datetime(&midnight).timestamp_millis())
}

fn next_bucket(start: i64, bucket: UsageBucket) -> Option<i64> {
    let datetime = DateTime::<Utc>::from_timestamp_millis(start)?;
    match bucket {
        UsageBucket::Auto | UsageBucket::Day => {
            Some((datetime + Duration::days(1)).timestamp_millis())
        }
        UsageBucket::Week => Some((datetime + Duration::days(7)).timestamp_millis()),
        UsageBucket::Month => {
            let (year, month) = if datetime.month() == 12 {
                (datetime.year() + 1, 1)
            } else {
                (datetime.year(), datetime.month() + 1)
            };
            let date = chrono::NaiveDate::from_ymd_opt(year, month, 1)?;
            Some(
                Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?)
                    .timestamp_millis(),
            )
        }
    }
}

fn finish_timeline(
    mut timeline: BTreeMap<i64, AggregateAccumulator>,
    bucket: UsageBucket,
    extent_since: Option<i64>,
    extent_until: Option<i64>,
) -> (Vec<UsageTrendPoint>, bool) {
    let observed_first = timeline.first_key_value().map(|(&start, _)| start);
    let observed_last = timeline.last_key_value().map(|(&start, _)| start);
    let requested_first = extent_since.and_then(|timestamp| bucket_start(timestamp, bucket));
    let requested_last = extent_until.and_then(|timestamp| bucket_start(timestamp, bucket));
    let first = requested_first.or(observed_first).or(requested_last);
    let last = requested_last.or(observed_last).or(requested_first);
    let (Some(first), Some(last)) = (first, last) else {
        return (Vec::new(), false);
    };

    const MAX_DENSE_BUCKETS: usize = 10_000;
    let mut dense_starts = Vec::new();
    let mut cursor = first;
    loop {
        dense_starts.push(cursor);
        if cursor >= last {
            break;
        }
        if dense_starts.len() >= MAX_DENSE_BUCKETS {
            // Avoid materializing years of zero-only daily buckets. Unlike the
            // old hard cap, this sparse path retains every observed bucket and
            // both requested boundaries, and advertises that fact in the report.
            timeline.entry(first).or_default();
            timeline.entry(last).or_default();
            return (
                timeline
                    .into_iter()
                    .map(|(start, aggregate)| trend_point(start, bucket, aggregate.finish()))
                    .collect(),
                true,
            );
        }
        let Some(next) = next_bucket(cursor, bucket).filter(|next| *next > cursor) else {
            timeline.entry(first).or_default();
            timeline.entry(last).or_default();
            return (
                timeline
                    .into_iter()
                    .map(|(start, aggregate)| trend_point(start, bucket, aggregate.finish()))
                    .collect(),
                true,
            );
        };
        cursor = next;
    }

    (
        dense_starts
            .into_iter()
            .map(|start| {
                let totals = timeline.remove(&start).unwrap_or_default().finish();
                trend_point(start, bucket, totals)
            })
            .collect(),
        false,
    )
}

fn trend_point(start: i64, bucket: UsageBucket, totals: UsageTotals) -> UsageTrendPoint {
    UsageTrendPoint {
        bucket_start: start,
        label: bucket_label(start, bucket),
        events: totals.events,
        api_calls: totals.api_calls,
        conversations: totals.conversations,
        tokens: totals.tokens,
        cost: totals.cost,
    }
}

fn bucket_label(timestamp: i64, bucket: UsageBucket) -> String {
    let Some(datetime) = DateTime::<Utc>::from_timestamp_millis(timestamp) else {
        return String::new();
    };
    match bucket {
        UsageBucket::Auto | UsageBucket::Day => datetime.format("%b %-d").to_string(),
        UsageBucket::Week => format!("Week of {}", datetime.format("%b %-d")),
        UsageBucket::Month => datetime.format("%b %Y").to_string(),
    }
}

fn valid_cost(cost: f64) -> bool {
    cost.is_finite() && cost > 0.0
}

fn percent(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

/// Render a compact, deterministic terminal summary.
pub fn render_terminal(report: &UsageReport, top: usize) -> String {
    let mut output = String::new();
    writeln!(output, "Usage Summary").unwrap();
    writeln!(output, "=============").unwrap();
    writeln!(output).unwrap();
    writeln!(
        output,
        "Range: {}  ·  {} buckets (UTC)",
        format_range(&report.range),
        title_case(report.range.bucket.as_str())
    )
    .unwrap();
    writeln!(output, "Filters: {}", format_filters(&report.filters)).unwrap();
    if report.range.trend_is_sparse {
        writeln!(
            output,
            "Timeline: zero-only gaps omitted; all observed buckets and range boundaries retained"
        )
        .unwrap();
    }
    writeln!(
        output,
        "Selected: {} API calls across {} conversations ({} usage records)",
        format_count(report.totals.api_calls),
        format_count(report.totals.conversations),
        format_count(report.totals.events)
    )
    .unwrap();
    writeln!(
        output,
        "Indexed corpus: {} conversations · {} messages",
        format_count(report.indexed_conversations),
        format_count(report.indexed_messages)
    )
    .unwrap();

    writeln!(output).unwrap();
    writeln!(output, "Tokens").unwrap();
    writeln!(
        output,
        "  Total          {:>16}",
        format_count(report.totals.tokens.total)
    )
    .unwrap();
    writeln!(
        output,
        "  Input          {:>16}",
        format_count(report.totals.tokens.input)
    )
    .unwrap();
    writeln!(
        output,
        "  Output         {:>16}  (includes reasoning)",
        format_count(report.totals.tokens.output)
    )
    .unwrap();
    writeln!(
        output,
        "  Cache read     {:>16}",
        format_count(report.totals.tokens.cache_read)
    )
    .unwrap();
    writeln!(
        output,
        "  Cache write    {:>16}",
        format_count(report.totals.tokens.cache_write)
    )
    .unwrap();
    writeln!(
        output,
        "  ↳ Reasoning    {:>16}  (output subset)",
        format_count(report.totals.tokens.reasoning)
    )
    .unwrap();

    writeln!(output).unwrap();
    writeln!(output, "Coverage").unwrap();
    writeln!(
        output,
        "  Token events       {:>6.1}%  ({}/{})",
        report.coverage.token_event_percent,
        format_count(report.coverage.events_with_tokens),
        format_count(report.totals.events)
    )
    .unwrap();
    writeln!(
        output,
        "  Provider calls      {:>6.1}%",
        report.coverage.provider_percent
    )
    .unwrap();
    writeln!(
        output,
        "  Model calls         {:>6.1}%",
        report.coverage.model_percent
    )
    .unwrap();
    writeln!(
        output,
        "  Timeline tokens     {:>6.1}%",
        report.coverage.timestamp_token_percent
    )
    .unwrap();
    writeln!(
        output,
        "  Cost-recorded tokens {:>5.1}%",
        report.coverage.cost_token_percent
    )
    .unwrap();

    if report.totals.cost.actual_usd.is_some() || report.totals.cost.estimated_usd.is_some() {
        writeln!(output).unwrap();
        writeln!(output, "Cost").unwrap();
        if let Some(cost) = report.totals.cost.actual_usd {
            writeln!(output, "  Reported actual  {:>16}", format_usd(cost)).unwrap();
        }
        if let Some(cost) = report.totals.cost.estimated_usd {
            writeln!(output, "  Source estimate  {:>16}", format_usd(cost)).unwrap();
        }
        writeln!(
            output,
            "  Coverage marks events with a recorded actual cost or source estimate."
        )
        .unwrap();
    }

    render_terminal_breakdown(
        &mut output,
        "By Harness",
        "Harness",
        &report.by_harness,
        report.totals.tokens.total,
        top,
        true,
    );
    render_terminal_breakdown(
        &mut output,
        "By Provider",
        "Provider",
        &report.by_provider,
        report.totals.tokens.total,
        top,
        false,
    );
    render_terminal_breakdown(
        &mut output,
        "Top Models",
        "Model",
        &report.by_model,
        report.totals.tokens.total,
        top,
        false,
    );

    writeln!(output).unwrap();
    writeln!(
        output,
        "Notes: cache buckets are separate from input; reasoning is a subset of output. Reported totals remain authoritative. Recorded estimates are not a bill."
    )
    .unwrap();
    output
}

fn render_terminal_breakdown(
    output: &mut String,
    title: &str,
    heading: &str,
    rows: &[UsageBreakdown],
    total_tokens: u64,
    top: usize,
    show_harness_icon: bool,
) {
    writeln!(output).unwrap();
    writeln!(output, "{title}").unwrap();
    writeln!(
        output,
        "  {heading:<29} {:>16}  {:>7}  {:>10}  {:>12}",
        "Tokens", "Share", "Calls", "Cost*"
    )
    .unwrap();
    for row in visible_rows(rows, top) {
        let icon = if show_harness_icon {
            harness_icon(&row.key)
        } else {
            ""
        };
        let label = if icon.is_empty() {
            truncate_chars(&row.label, 29)
        } else {
            truncate_chars(&format!("{icon} {}", row.label), 29)
        };
        let cost = row
            .cost
            .accounted_usd
            .map(format_approx_usd)
            .unwrap_or_else(|| "—".to_string());
        writeln!(
            output,
            "  {label:<29} {:>16}  {:>6.1}%  {:>10}  {:>12}",
            format_count(row.tokens.total),
            percent(row.tokens.total, total_tokens),
            format_count(row.api_calls),
            cost,
        )
        .unwrap();
    }
    if rows.len() > top {
        writeln!(output, "  … {} more", rows.len().saturating_sub(top)).unwrap();
    }
}

fn visible_rows(rows: &[UsageBreakdown], top: usize) -> Vec<&UsageBreakdown> {
    let mut visible: Vec<&UsageBreakdown> = rows.iter().take(top).collect();
    if let Some(unknown) = rows.iter().find(|row| row.key == UNKNOWN_KEY)
        && !visible.iter().any(|row| row.key == UNKNOWN_KEY)
    {
        visible.push(unknown);
    }
    visible
}

fn harness_icon(key: &str) -> &'static str {
    match key {
        "claude_code" => "●",
        "codex" => "◆",
        "hermes" => "♦",
        "opencode" => "■",
        "pi_agent" => "▲",
        _ => "",
    }
}

/// Render a self-contained, offline HTML report with inline CSS and SVG.
pub fn render_html(report: &UsageReport, top: usize) -> String {
    let range = escape_html(&format_range(&report.range));
    let filters = escape_html(&format_filters(&report.filters));
    let generated = format_datetime(report.generated_at);
    let cost_value = report
        .totals
        .cost
        .accounted_usd
        .map(format_approx_usd)
        .unwrap_or_else(|| "Not available".to_string());
    let cost_label = match (
        report.totals.cost.actual_usd.is_some(),
        report.totals.cost.estimated_usd.is_some(),
    ) {
        (true, false) => "Reported cost",
        (false, true) => "Recorded estimate",
        (true, true) => "Cost accounted",
        (false, false) => "Cost coverage",
    };
    let trend_svg = render_trend_svg(&report.trend);
    let harnesses = render_split_html(
        &report.by_harness,
        top,
        report.totals.tokens.total,
        "accent",
        true,
    );
    let providers = render_split_html(
        &report.by_provider,
        top,
        report.totals.tokens.total,
        "good",
        false,
    );
    let models = render_models_html(&report.by_model, top, report.totals.tokens.total);
    let token_mix = render_token_mix(report);
    let unknown_provider = report
        .totals
        .api_calls
        .saturating_sub(report.coverage.api_calls_with_provider);
    let unknown_model = report
        .totals
        .api_calls
        .saturating_sub(report.coverage.api_calls_with_model);
    let trend_points_label = if report.range.trend_is_sparse {
        format!(
            "{} observed/boundary buckets · zero-only gaps omitted",
            format_count(report.trend.len() as u64)
        )
    } else {
        format!(
            "{} calendar buckets",
            format_count(report.trend.len() as u64)
        )
    };

    format!(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <meta name="color-scheme" content="light dark">
  <title>sess usage report</title>
  <style>
    :root {{
      --bg:#F7F6F2;--surface:#FFFFFF;--recessed:#EFEDE7;
      --text-strong:#14181F;--text:#3D4451;--text-dim:#757A85;
      --border:#D4D2CC;--accent:#38617B;--good:#6E8B5C;
      --warn:#B07636;--bad:#B23838;--shadow:0 18px 48px rgba(20,24,31,.08);
    }}
    @media (prefers-color-scheme:dark) {{ :root {{
      --bg:#14181F;--surface:#1C2129;--recessed:#0E1117;
      --text-strong:#ECEDF0;--text:#C8CBD2;--text-dim:#8B8E96;
      --border:rgba(232,234,240,.10);--accent:#7AA0BD;--good:#9FB87E;
      --warn:#D49B5C;--bad:#D67373;--shadow:0 24px 64px rgba(0,0,0,.28);
    }} }}
    *{{box-sizing:border-box}}
    html{{background:var(--bg)}}
    body{{margin:0;color:var(--text);background:
      radial-gradient(circle at 15% -10%,color-mix(in srgb,var(--accent) 12%,transparent),transparent 36rem),
      var(--bg);font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;
      font-variant-numeric:tabular-nums;line-height:1.45}}
    main{{width:min(1180px,calc(100% - 36px));margin:0 auto;padding:54px 0 70px}}
    .eyebrow{{font-size:.72rem;font-weight:800;letter-spacing:.16em;text-transform:uppercase;color:var(--accent)}}
    h1{{max-width:820px;margin:.45rem 0 .7rem;color:var(--text-strong);font-size:clamp(2.5rem,6vw,5.8rem);line-height:.93;letter-spacing:-.065em}}
    .lede{{max-width:720px;margin:0;color:var(--text);font-size:clamp(1rem,2vw,1.25rem)}}
    .meta{{display:flex;flex-wrap:wrap;gap:.55rem 1rem;margin-top:1.3rem;color:var(--text-dim);font-size:.82rem}}
    .hero{{display:grid;grid-template-columns:1.7fr repeat(3,1fr);gap:1px;margin:44px 0 22px;background:var(--border);border:1px solid var(--border);border-radius:22px;overflow:hidden;box-shadow:var(--shadow)}}
    .metric{{min-width:0;padding:24px;background:var(--surface)}}
    .metric.primary{{padding:30px}}
    .metric-label{{font-size:.73rem;font-weight:750;letter-spacing:.09em;text-transform:uppercase;color:var(--text-dim)}}
    .metric-value{{margin-top:.35rem;color:var(--text-strong);font-size:clamp(1.55rem,3.4vw,3.4rem);font-weight:760;line-height:1;letter-spacing:-.045em}}
    .metric.primary .metric-value{{font-size:clamp(2.5rem,5vw,5rem)}}
    .metric-note{{margin-top:.65rem;color:var(--text-dim);font-size:.78rem}}
    .coverage{{display:grid;grid-template-columns:repeat(4,1fr);gap:12px;margin:0 0 34px}}
    .coverage-item{{padding:14px 16px;border-radius:14px;background:color-mix(in srgb,var(--accent) 6%,var(--surface));border:1px solid var(--border)}}
    .coverage-item strong{{display:block;color:var(--text-strong);font-size:1.2rem}}
    .coverage-item span{{color:var(--text-dim);font-size:.76rem}}
    section{{margin-top:22px;padding:26px;border:1px solid var(--border);border-radius:20px;background:var(--surface);box-shadow:0 8px 30px rgba(20,24,31,.035)}}
    .section-head{{display:flex;align-items:end;justify-content:space-between;gap:20px;margin-bottom:22px}}
    h2{{margin:0;color:var(--text-strong);font-size:1.35rem;letter-spacing:-.025em}}
    .section-kicker{{color:var(--text-dim);font-size:.78rem}}
    .trend{{width:100%;height:auto;display:block;overflow:visible}}
    .trend text{{fill:var(--text-dim);font:12px ui-sans-serif,system-ui,sans-serif}}
    .trend .grid{{stroke:var(--border);stroke-width:1}}
    .trend .area{{fill:url(#usage-area)}}
    .trend .line{{fill:none;stroke:var(--accent);stroke-width:3;stroke-linecap:round;stroke-linejoin:round}}
    .trend .point{{fill:var(--surface);stroke:var(--accent);stroke-width:2}}
    .two-up{{display:grid;grid-template-columns:1fr 1fr;gap:22px}}
    .split-list{{display:grid;gap:15px}}
    .split-top{{display:flex;align-items:baseline;justify-content:space-between;gap:16px}}
    .split-name{{min-width:0;color:var(--text-strong);font-weight:680;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}}
    .split-value{{color:var(--text);font-size:.86rem;white-space:nowrap}}
    .track{{height:7px;margin-top:7px;border-radius:99px;background:var(--recessed);overflow:hidden}}
    .fill{{display:block;height:100%;min-width:2px;border-radius:inherit;background:var(--accent)}}
    .fill.good{{background:var(--good)}}
    .split-meta{{display:flex;justify-content:space-between;margin-top:5px;color:var(--text-dim);font-size:.7rem}}
    .model-list{{display:grid}}
    .model-row{{display:grid;grid-template-columns:38px minmax(0,1fr) auto auto;gap:14px;align-items:center;padding:13px 2px;border-top:1px solid var(--border)}}
    .model-row:first-child{{border-top:0}}
    .rank{{display:grid;place-items:center;width:28px;height:28px;border-radius:50%;background:var(--recessed);color:var(--text-dim);font-size:.72rem;font-weight:800}}
    .model-name{{min-width:0;color:var(--text-strong);font-weight:650;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}}
    .model-tokens{{color:var(--text-strong);font-weight:720}}
    .model-share{{min-width:48px;text-align:right;color:var(--text-dim);font-size:.78rem}}
    .mix{{display:flex;height:18px;border-radius:99px;overflow:hidden;background:var(--recessed)}}
    .mix span{{display:block;height:100%;min-width:1px}}
    .mix-input{{background:var(--accent)}}.mix-output{{background:var(--good)}}
    .mix-cache{{background:var(--warn)}}.mix-reasoning{{background:var(--bad)}}
    .legend{{display:flex;flex-wrap:wrap;gap:10px 18px;margin-top:14px;color:var(--text-dim);font-size:.75rem}}
    .dot{{display:inline-block;width:8px;height:8px;margin-right:6px;border-radius:50%;background:currentColor}}
    .c-accent{{color:var(--accent)}}.c-good{{color:var(--good)}}.c-warn{{color:var(--warn)}}.c-bad{{color:var(--bad)}}
    .caveat{{margin-top:22px;padding:20px 22px;border:1px solid color-mix(in srgb,var(--warn) 30%,var(--border));border-radius:16px;background:color-mix(in srgb,var(--warn) 9%,var(--surface))}}
    .caveat strong{{color:var(--text-strong)}}.caveat p{{margin:.35rem 0 0;font-size:.86rem}}
    footer{{display:flex;justify-content:space-between;gap:20px;margin-top:30px;padding:18px 2px;color:var(--text-dim);font-size:.75rem}}
    @media(max-width:900px){{.hero{{grid-template-columns:1fr 1fr}}.metric.primary{{grid-column:1/-1}}.coverage{{grid-template-columns:1fr 1fr}}.two-up{{grid-template-columns:1fr}}}}
    @media(max-width:560px){{main{{width:min(100% - 22px,1180px);padding-top:30px}}.hero{{grid-template-columns:1fr}}.metric.primary{{grid-column:auto}}.coverage{{grid-template-columns:1fr 1fr}}section{{padding:19px}}.model-row{{grid-template-columns:32px minmax(0,1fr) auto}}.model-share{{display:none}}footer{{display:block}}}}
  </style>
</head>
<body>
<main>
  <header>
    <div class="eyebrow">sess / usage intelligence</div>
    <h1>Agent usage,<br>made legible.</h1>
    <p class="lede">Provider-reported tokens across harnesses, providers, and models—grounded in the sessions already indexed on this machine.</p>
    <div class="meta"><span>{range}</span><span>{filters}</span><span>{bucket} buckets · UTC</span><span>Generated {generated}</span></div>
  </header>

  <div class="hero">
    <div class="metric primary"><div class="metric-label">Reported tokens</div><div class="metric-value">{total_tokens}</div><div class="metric-note">Provider-reported total, including cache buckets where supplied</div></div>
    <div class="metric"><div class="metric-label">Conversations</div><div class="metric-value">{conversations}</div><div class="metric-note">{calls} API calls · {events} usage records</div></div>
    <div class="metric"><div class="metric-label">{cost_label}</div><div class="metric-value">{cost_value}</div><div class="metric-note">{cost_coverage:.1}% of tokens have recorded cost data</div></div>
    <div class="metric"><div class="metric-label">Model coverage</div><div class="metric-value">{model_coverage:.1}%</div><div class="metric-note">{unknown_model} API calls unattributed</div></div>
  </div>

  <div class="coverage">
    <div class="coverage-item"><strong>{token_coverage:.1}%</strong><span>events with token data</span></div>
    <div class="coverage-item"><strong>{provider_coverage:.1}%</strong><span>API calls with provider attribution</span></div>
    <div class="coverage-item"><strong>{timestamp_token_coverage:.1}%</strong><span>tokens with exact timeline</span></div>
    <div class="coverage-item"><strong>{indexed_conversations}</strong><span>conversations in indexed corpus</span></div>
  </div>

  <section>
    <div class="section-head"><div><div class="section-kicker">Through time</div><h2>Token usage trend</h2></div><div class="section-kicker">{trend_points_label}</div></div>
    {trend_svg}
  </section>

  <div class="two-up">
    <section><div class="section-head"><div><div class="section-kicker">Execution surface</div><h2>Harness split</h2></div></div>{harnesses}</section>
    <section><div class="section-head"><div><div class="section-kicker">Inference source</div><h2>Provider split</h2></div></div>{providers}</section>
  </div>

  <section>
    <div class="section-head"><div><div class="section-kicker">Token concentration</div><h2>Model leaderboard</h2></div><div class="section-kicker">Top {top} + unknown</div></div>
    {models}
  </section>

  <section>
    <div class="section-head"><div><div class="section-kicker">Reported components</div><h2>Token mix</h2></div></div>
    {token_mix}
  </section>

  <div class="caveat"><strong>Read this as measured coverage, not billing truth.</strong><p>Cache buckets are separate from input. Reasoning annotates a subset of output and is never added twice; the provider-reported total remains authoritative when source detail is incomplete. Session-model aggregates without event-exact timing remain in totals but are excluded from time filters and trends. Cost estimates are positive values recorded by source harnesses, not an independent pricing calculation or a bill. {unknown_provider} API calls have no provider and {unknown_model} have no model; they remain visible as Unknown rather than being guessed.</p></div>
  <footer><span>Generated locally by sess</span><span>{indexed_messages} indexed messages · no data left this machine</span></footer>
</main>
</body>
</html>"##,
        bucket = title_case(report.range.bucket.as_str()),
        total_tokens = format_compact(report.totals.tokens.total),
        conversations = format_count(report.totals.conversations),
        calls = format_count(report.totals.api_calls),
        events = format_count(report.totals.events),
        cost_coverage = report.coverage.cost_token_percent,
        model_coverage = report.coverage.model_percent,
        token_coverage = report.coverage.token_event_percent,
        provider_coverage = report.coverage.provider_percent,
        timestamp_token_coverage = report.coverage.timestamp_token_percent,
        indexed_conversations = format_count(report.indexed_conversations),
        indexed_messages = format_count(report.indexed_messages),
        trend_points_label = escape_html(&trend_points_label),
        top = top,
    )
}

fn render_trend_svg(points: &[UsageTrendPoint]) -> String {
    if points.is_empty() {
        return "<div class=\"section-kicker\">No timestamped usage events in this selection.</div>"
            .to_string();
    }

    const WIDTH: f64 = 960.0;
    const HEIGHT: f64 = 270.0;
    const LEFT: f64 = 62.0;
    const RIGHT: f64 = 18.0;
    const TOP: f64 = 18.0;
    const BOTTOM: f64 = 42.0;
    let plot_width = WIDTH - LEFT - RIGHT;
    let plot_height = HEIGHT - TOP - BOTTOM;
    let maximum = points
        .iter()
        .map(|point| point.tokens.total)
        .max()
        .unwrap_or(0)
        .max(1) as f64;
    let first_timestamp = points.first().map(|point| point.bucket_start).unwrap_or(0);
    let last_timestamp = points.last().map(|point| point.bucket_start).unwrap_or(0);
    let x_for = |timestamp: i64| {
        if first_timestamp == last_timestamp {
            LEFT + plot_width / 2.0
        } else {
            let offset = timestamp.saturating_sub(first_timestamp) as f64;
            let span = last_timestamp.saturating_sub(first_timestamp) as f64;
            LEFT + plot_width * offset / span
        }
    };
    let y_for = |tokens: u64| TOP + plot_height * (1.0 - tokens as f64 / maximum);

    let coordinates: Vec<(f64, f64)> = points
        .iter()
        .map(|point| (x_for(point.bucket_start), y_for(point.tokens.total)))
        .collect();
    let line = coordinates
        .iter()
        .map(|(x, y)| format!("{x:.1},{y:.1}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut area = format!("M {:.1} {:.1}", coordinates[0].0, TOP + plot_height);
    for (x, y) in &coordinates {
        write!(area, " L {x:.1} {y:.1}").unwrap();
    }
    write!(
        area,
        " L {:.1} {:.1} Z",
        coordinates.last().unwrap().0,
        TOP + plot_height
    )
    .unwrap();

    let mut svg = format!(
        r#"<svg class="trend" viewBox="0 0 {WIDTH:.0} {HEIGHT:.0}" role="img" aria-label="Token usage trend"><defs><linearGradient id="usage-area" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="var(--accent)" stop-opacity=".28"/><stop offset="1" stop-color="var(--accent)" stop-opacity=".02"/></linearGradient></defs>"#
    );
    for step in 0..=4 {
        let fraction = step as f64 / 4.0;
        let y = TOP + plot_height * fraction;
        let value = (maximum * (1.0 - fraction)).round() as u64;
        write!(
            svg,
            r#"<line class="grid" x1="{LEFT:.1}" y1="{y:.1}" x2="{right:.1}" y2="{y:.1}"/><text x="{text_x:.1}" y="{text_y:.1}" text-anchor="end">{label}</text>"#,
            right = WIDTH - RIGHT,
            text_x = LEFT - 10.0,
            text_y = y + 4.0,
            label = escape_html(&format_compact(value)),
        )
        .unwrap();
    }
    write!(
        svg,
        r#"<path class="area" d="{area}"/><polyline class="line" points="{line}"/>"#
    )
    .unwrap();
    for (index, ((x, y), point)) in coordinates.iter().zip(points).enumerate() {
        write!(
            svg,
            r#"<circle class="point" cx="{x:.1}" cy="{y:.1}" r="3.5"><title>{label}: {tokens} tokens</title></circle>"#,
            label = escape_html(&point.label),
            tokens = format_count(point.tokens.total),
        )
        .unwrap();
        if index == 0 || index == points.len() / 2 || index + 1 == points.len() {
            let anchor = if index == 0 {
                "start"
            } else if index + 1 == points.len() {
                "end"
            } else {
                "middle"
            };
            write!(
                svg,
                r#"<text x="{x:.1}" y="{y:.1}" text-anchor="{anchor}">{label}</text>"#,
                y = HEIGHT - 12.0,
                label = escape_html(&point.label),
            )
            .unwrap();
        }
    }
    svg.push_str("</svg>");
    svg
}

fn render_split_html(
    rows: &[UsageBreakdown],
    top: usize,
    total_tokens: u64,
    fill_class: &str,
    show_icon: bool,
) -> String {
    if rows.is_empty() {
        return "<div class=\"section-kicker\">No usage in this selection.</div>".to_string();
    }
    let mut output = String::from("<div class=\"split-list\">");
    for row in visible_rows(rows, top) {
        let share = percent(row.tokens.total, total_tokens);
        let icon = if show_icon {
            harness_icon(&row.key)
        } else {
            ""
        };
        let label = if icon.is_empty() {
            escape_html(&row.label)
        } else {
            format!("{} {}", escape_html(icon), escape_html(&row.label))
        };
        let class = if fill_class == "good" {
            "fill good"
        } else {
            "fill"
        };
        write!(
            output,
            r#"<div><div class="split-top"><span class="split-name" title="{title}">{label}</span><span class="split-value">{tokens}</span></div><div class="track"><span class="{class}" style="width:{width:.2}%"></span></div><div class="split-meta"><span>{share:.1}% of tokens</span><span>{calls} calls</span></div></div>"#,
            title = escape_html(&row.label),
            tokens = format_count(row.tokens.total),
            width = share.clamp(0.0, 100.0),
            calls = format_count(row.api_calls),
        )
        .unwrap();
    }
    output.push_str("</div>");
    output
}

fn render_models_html(rows: &[UsageBreakdown], top: usize, total_tokens: u64) -> String {
    if rows.is_empty() {
        return "<div class=\"section-kicker\">No model attribution in this selection.</div>"
            .to_string();
    }
    let mut output = String::from("<div class=\"model-list\">");
    for (index, row) in visible_rows(rows, top).into_iter().enumerate() {
        write!(
            output,
            r#"<div class="model-row"><span class="rank">{rank}</span><span class="model-name" title="{title}">{label}</span><span class="model-tokens">{tokens}</span><span class="model-share">{share:.1}%</span></div>"#,
            rank = index + 1,
            title = escape_html(&row.label),
            label = escape_html(&row.label),
            tokens = format_count(row.tokens.total),
            share = percent(row.tokens.total, total_tokens),
        )
        .unwrap();
    }
    output.push_str("</div>");
    output
}

fn render_token_mix(report: &UsageReport) -> String {
    let tokens = &report.totals.tokens;
    let visible_output = tokens.output.saturating_sub(tokens.reasoning);
    let mix_total = tokens
        .input
        .saturating_add(visible_output)
        .saturating_add(tokens.cache_read)
        .saturating_add(tokens.cache_write)
        .saturating_add(tokens.reasoning);
    if mix_total == 0 {
        return "<div class=\"section-kicker\">No component counters were reported.</div>"
            .to_string();
    }
    let input = percent(tokens.input, mix_total);
    let output = percent(visible_output, mix_total);
    let cache = percent(
        tokens.cache_read.saturating_add(tokens.cache_write),
        mix_total,
    );
    let reasoning = percent(tokens.reasoning, mix_total);
    format!(
        r#"<div class="mix" role="img" aria-label="Reported token component mix"><span class="mix-input" style="width:{input:.2}%"></span><span class="mix-output" style="width:{output:.2}%"></span><span class="mix-cache" style="width:{cache:.2}%"></span><span class="mix-reasoning" style="width:{reasoning:.2}%"></span></div><div class="legend"><span class="c-accent"><i class="dot"></i>Input {input_count}</span><span class="c-good"><i class="dot"></i>Output (non-reasoning) {output_count}</span><span class="c-warn"><i class="dot"></i>Cache {cache_count}</span><span class="c-bad"><i class="dot"></i>Reasoning {reasoning_count}</span></div>"#,
        input_count = format_count(tokens.input),
        output_count = format_count(visible_output),
        cache_count = format_count(tokens.cache_read.saturating_add(tokens.cache_write)),
        reasoning_count = format_count(tokens.reasoning),
    )
}

fn format_range(range: &UsageRange) -> String {
    match (range.requested_since, range.requested_until) {
        (Some(start), Some(end)) => {
            format!("{} – {}", format_date(start), format_date(end))
        }
        (Some(start), None) => format!("{} onward", format_date(start)),
        (None, Some(end)) => format!("Through {}", format_date(end)),
        (None, None) => "All indexed time".to_string(),
    }
}

fn format_filters(filters: &UsageFilters) -> String {
    let mut dimensions = Vec::new();
    if !filters.agents.is_empty() {
        dimensions.push(format!(
            "Harness {}",
            filters
                .agents
                .iter()
                .map(Agent::display_name)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !filters.providers.is_empty() {
        dimensions.push(format!("Provider {}", filters.providers.join(", ")));
    }
    if !filters.models.is_empty() {
        dimensions.push(format!("Model {}", filters.models.join(", ")));
    }
    if let Some(workspace) = filters.workspace.as_deref() {
        dimensions.push(format!("Workspace {workspace}"));
    }
    if dimensions.is_empty() {
        "All harnesses, providers, models, and workspaces".to_string()
    } else {
        dimensions.join(" · ")
    }
}

fn format_date(timestamp: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(timestamp)
        .map(|datetime| {
            datetime
                .with_timezone(&chrono::Local)
                .format("%b %-d, %Y")
                .to_string()
        })
        .unwrap_or_else(|| "Unknown date".to_string())
}

fn format_datetime(timestamp: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(timestamp)
        .map(|datetime| datetime.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "at an unknown time".to_string())
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    output
}

fn format_compact(value: u64) -> String {
    const UNITS: &[(u64, &str)] = &[
        (1_000_000_000_000, "T"),
        (1_000_000_000, "B"),
        (1_000_000, "M"),
        (1_000, "K"),
    ];
    for (threshold, suffix) in UNITS {
        if value >= *threshold {
            let scaled = value as f64 / *threshold as f64;
            return if scaled >= 100.0 {
                format!("{scaled:.0}{suffix}")
            } else if scaled >= 10.0 {
                format!("{scaled:.1}{suffix}")
            } else {
                format!("{scaled:.2}{suffix}")
            };
        }
    }
    format_count(value)
}

fn format_usd(value: f64) -> String {
    if !value.is_finite() {
        return "—".to_string();
    }
    let cents = (value.max(0.0) * 100.0).round() as u64;
    format!("${}.{:02}", format_count(cents / 100), cents % 100)
}

fn format_approx_usd(value: f64) -> String {
    format!("≈{}", format_usd(value))
}

fn title_case(value: &str) -> String {
    let mut characters = value.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    if value.chars().count() <= maximum {
        return value.to_string();
    }
    let keep = maximum.saturating_sub(1);
    format!("{}…", value.chars().take(keep).collect::<String>())
}

fn escape_html(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(character),
        }
    }
    output
}

/// Atomically write an HTML report, creating its parent directory if needed.
pub fn write_html(path: &Path, html: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create report directory {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("HTML report path must name a file")?;
    let temporary = path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        Utc::now().timestamp_micros()
    ));
    fs::write(&temporary, html.as_bytes())
        .with_context(|| format!("failed to write temporary report {}", temporary.display()))?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("failed to publish HTML report {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tokens(total: u64) -> TokenCounts {
        TokenCounts {
            input: total / 2,
            output: total / 4,
            cache_read: total / 4,
            cache_write: 0,
            reasoning: 0,
            total,
        }
    }

    fn event(
        id: i64,
        agent: Agent,
        timestamp: Option<i64>,
        provider: Option<&str>,
        model: Option<&str>,
        total: u64,
    ) -> UsageEventRow {
        UsageEventRow {
            conversation_id: id,
            agent,
            workspace: Some("/work/project".to_string()),
            timestamp,
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
            source_event_id: None,
            api_calls: 1,
            tokens: tokens(total),
            actual_cost_usd: None,
            estimated_cost_usd: None,
        }
    }

    fn timestamp(year: i32, month: u32, day: u32) -> i64 {
        Utc.with_ymd_and_hms(year, month, day, 12, 0, 0)
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn report_aggregates_and_sorts_all_dimensions() {
        let mut first = event(
            1,
            Agent::Codex,
            Some(timestamp(2026, 7, 1)),
            Some("OpenAI"),
            Some("gpt-5"),
            1_000,
        );
        first.actual_cost_usd = Some(1.25);
        let mut second = event(
            2,
            Agent::PiAgent,
            Some(timestamp(2026, 7, 2)),
            Some("Anthropic"),
            Some("claude-opus"),
            2_000,
        );
        second.estimated_cost_usd = Some(2.50);
        second.api_calls = 8;
        let unknown = event(1, Agent::Codex, None, None, None, 500);
        let dataset = UsageDataset {
            events: vec![first, second, unknown],
            indexed_conversations: 20,
            indexed_messages: 200,
        };

        let report = build_report(&dataset, &UsageFilters::default());

        assert_eq!(report.totals.events, 3);
        assert_eq!(report.totals.api_calls, 10);
        assert_eq!(report.totals.conversations, 2);
        assert_eq!(report.totals.tokens.total, 3_500);
        assert_eq!(report.by_harness[0].key, "pi_agent");
        assert_eq!(report.by_harness[0].api_calls, 8);
        assert_eq!(report.by_harness[1].key, "codex");
        assert_eq!(report.by_provider[0].label, "Anthropic");
        assert!(report.by_provider.iter().any(|row| row.key == UNKNOWN_KEY));
        assert_eq!(report.coverage.provider_percent, 90.0);
        assert_eq!(report.coverage.model_percent, 90.0);
        assert_eq!(report.coverage.timestamp_percent, 200.0 / 3.0);
        assert_eq!(report.coverage.cost_token_percent, 3000.0 / 3500.0 * 100.0);
        assert_eq!(report.totals.cost.actual_usd, Some(1.25));
        assert_eq!(report.totals.cost.estimated_usd, Some(2.5));
        assert_eq!(report.totals.cost.accounted_usd, Some(3.75));
    }

    #[test]
    fn copied_source_events_are_counted_once_across_conversations() {
        let mut original = event(
            1,
            Agent::ClaudeCode,
            Some(timestamp(2026, 7, 1)),
            None,
            Some("claude"),
            100,
        );
        original.source_event_id = Some("message:shared".to_string());
        let mut more_complete_copy = original.clone();
        more_complete_copy.conversation_id = 2;
        more_complete_copy.timestamp = Some(timestamp(2026, 7, 2));
        more_complete_copy.tokens = tokens(120);

        let report = build_report(
            &UsageDataset {
                events: vec![original, more_complete_copy],
                ..UsageDataset::default()
            },
            &UsageFilters::default(),
        );

        assert_eq!(report.totals.events, 1);
        assert_eq!(report.totals.api_calls, 1);
        assert_eq!(report.totals.conversations, 1);
        assert_eq!(report.totals.tokens.total, 120);
    }

    #[test]
    fn copied_source_event_prefers_richer_attribution_when_tokens_match() {
        let mut unattributed = event(
            1,
            Agent::ClaudeCode,
            Some(timestamp(2026, 7, 2)),
            None,
            None,
            100,
        );
        unattributed.source_event_id = Some("message:shared".to_string());
        let mut attributed = unattributed.clone();
        attributed.conversation_id = 2;
        attributed.timestamp = Some(timestamp(2026, 7, 1));
        attributed.provider = Some("anthropic".to_string());
        attributed.model = Some("claude".to_string());
        attributed.estimated_cost_usd = Some(0.25);

        let report = build_report(
            &UsageDataset {
                events: vec![unattributed, attributed],
                ..UsageDataset::default()
            },
            &UsageFilters::default(),
        );

        assert_eq!(report.totals.events, 1);
        assert_eq!(report.by_provider[0].key, "anthropic");
        assert_eq!(report.by_model[0].key, "claude");
        assert_eq!(report.totals.cost.accounted_usd, Some(0.25));
    }

    #[test]
    fn copied_source_events_are_deduplicated_after_filtering() {
        let mut wanted = event(
            1,
            Agent::PiAgent,
            Some(timestamp(2026, 7, 1)),
            Some("anthropic"),
            Some("claude"),
            100,
        );
        wanted.workspace = Some("/wanted".to_string());
        wanted.source_event_id = Some("pi-message:shared".to_string());
        let mut outside = wanted.clone();
        outside.conversation_id = 2;
        outside.workspace = Some("/other".to_string());
        outside.tokens = tokens(200);

        let report = build_report(
            &UsageDataset {
                events: vec![wanted, outside],
                ..UsageDataset::default()
            },
            &UsageFilters {
                workspace: Some("/wanted".to_string()),
                ..UsageFilters::default()
            },
        );

        assert_eq!(report.totals.events, 1);
        assert_eq!(report.totals.tokens.total, 100);
    }

    #[test]
    fn zero_cost_without_status_does_not_claim_cost_coverage() {
        let mut row = event(
            1,
            Agent::PiAgent,
            None,
            Some("provider"),
            Some("model"),
            100,
        );
        row.estimated_cost_usd = Some(0.0);
        let report = build_report(
            &UsageDataset {
                events: vec![row],
                ..UsageDataset::default()
            },
            &UsageFilters::default(),
        );
        assert_eq!(report.totals.cost.estimated_usd, None);
        assert_eq!(report.totals.cost.accounted_usd, None);
        assert_eq!(report.coverage.cost_token_percent, 0.0);
    }

    #[test]
    fn filters_or_within_dimensions_and_and_across_them() {
        let dataset = UsageDataset {
            events: vec![
                event(
                    1,
                    Agent::Codex,
                    Some(timestamp(2026, 7, 1)),
                    Some("openai"),
                    Some("gpt-a"),
                    100,
                ),
                event(
                    2,
                    Agent::Codex,
                    Some(timestamp(2026, 7, 2)),
                    Some("openai"),
                    Some("gpt-b"),
                    200,
                ),
                event(
                    3,
                    Agent::PiAgent,
                    Some(timestamp(2026, 7, 3)),
                    Some("anthropic"),
                    Some("claude"),
                    400,
                ),
            ],
            ..UsageDataset::default()
        };
        let filters = UsageFilters {
            agents: vec![Agent::Codex, Agent::PiAgent],
            providers: vec!["OPENAI".to_string()],
            models: vec!["gpt-a".to_string(), "gpt-b".to_string()],
            workspace: Some("/work/project".to_string()),
            since: Some(timestamp(2026, 7, 1)),
            until: Some(timestamp(2026, 7, 2) + Duration::days(1).num_milliseconds()),
            bucket: UsageBucket::Day,
        };

        let report = build_report(&dataset, &filters);
        assert_eq!(report.totals.events, 2);
        assert_eq!(report.totals.tokens.total, 300);
    }

    #[test]
    fn time_filters_exclude_undated_events_but_unknown_dimensions_can_be_selected() {
        let dataset = UsageDataset {
            events: vec![event(1, Agent::Codex, None, None, None, 50)],
            ..UsageDataset::default()
        };
        let unknown_only = UsageFilters {
            providers: vec!["unknown".to_string()],
            ..UsageFilters::default()
        };
        assert_eq!(build_report(&dataset, &unknown_only).totals.events, 1);

        let dated = UsageFilters {
            since: Some(timestamp(2026, 1, 1)),
            ..UsageFilters::default()
        };
        assert_eq!(build_report(&dataset, &dated).totals.events, 0);
    }

    #[test]
    fn auto_bucket_resolves_and_fills_calendar_gaps() {
        let dataset = UsageDataset {
            events: vec![
                event(
                    1,
                    Agent::Codex,
                    Some(timestamp(2026, 7, 1)),
                    Some("openai"),
                    Some("gpt"),
                    100,
                ),
                event(
                    2,
                    Agent::Codex,
                    Some(timestamp(2026, 7, 3)),
                    Some("openai"),
                    Some("gpt"),
                    300,
                ),
            ],
            ..UsageDataset::default()
        };
        let report = build_report(&dataset, &UsageFilters::default());
        assert_eq!(report.range.bucket, UsageBucket::Day);
        assert_eq!(report.trend.len(), 3);
        assert_eq!(report.trend[1].tokens.total, 0);
        assert_eq!(report.trend[2].tokens.total, 300);
    }

    #[test]
    fn requested_window_controls_auto_bucket_and_zero_padding() {
        let since = timestamp(2026, 4, 1);
        let until = timestamp(2026, 7, 16);
        let dataset = UsageDataset {
            events: vec![event(
                1,
                Agent::Codex,
                Some(timestamp(2026, 6, 15)),
                Some("openai"),
                Some("gpt"),
                100,
            )],
            ..UsageDataset::default()
        };
        let report = build_report(
            &dataset,
            &UsageFilters {
                since: Some(since),
                until: Some(until),
                ..UsageFilters::default()
            },
        );

        assert_eq!(report.range.bucket, UsageBucket::Week);
        assert!(report.trend.len() > 10);
        assert_eq!(report.trend.first().unwrap().tokens.total, 0);
        assert_eq!(report.trend.last().unwrap().tokens.total, 0);
        assert_eq!(
            report
                .trend
                .iter()
                .map(|point| point.tokens.total)
                .sum::<u64>(),
            100
        );
    }

    #[test]
    fn open_ended_since_window_extends_through_report_generation() {
        let since = Utc::now().timestamp_millis() - Duration::days(10).num_milliseconds();
        let dataset = UsageDataset {
            events: vec![event(
                1,
                Agent::Codex,
                Some(since + Duration::days(1).num_milliseconds()),
                Some("openai"),
                Some("gpt"),
                100,
            )],
            ..UsageDataset::default()
        };
        let report = build_report(
            &dataset,
            &UsageFilters {
                since: Some(since),
                ..UsageFilters::default()
            },
        );

        assert_eq!(report.range.bucket, UsageBucket::Day);
        assert!(report.trend.len() >= 10);
        assert_eq!(report.trend.last().unwrap().tokens.total, 0);
    }

    #[test]
    fn very_wide_explicit_timeline_keeps_late_usage_without_dense_zero_padding() {
        let since = timestamp(1990, 1, 1);
        let until = timestamp(2026, 7, 16);
        let event_timestamp = timestamp(2026, 7, 15);
        let dataset = UsageDataset {
            events: vec![event(
                1,
                Agent::Codex,
                Some(event_timestamp),
                Some("openai"),
                Some("gpt"),
                100,
            )],
            ..UsageDataset::default()
        };
        let report = build_report(
            &dataset,
            &UsageFilters {
                since: Some(since),
                until: Some(until),
                bucket: UsageBucket::Day,
                ..UsageFilters::default()
            },
        );

        assert!(report.range.trend_is_sparse);
        assert!(report.trend.len() < 10_000);
        assert_eq!(
            report.trend.first().unwrap().bucket_start,
            bucket_start(since, UsageBucket::Day).unwrap()
        );
        assert_eq!(
            report.trend.last().unwrap().bucket_start,
            bucket_start(until, UsageBucket::Day).unwrap()
        );
        assert_eq!(
            report.trend.iter().fold(0_u64, |total, point| total
                .saturating_add(point.tokens.total)),
            100
        );
        assert!(report.trend.iter().any(|point| point.bucket_start
            == bucket_start(event_timestamp, UsageBucket::Day).unwrap()
            && point.tokens.total == 100));
    }

    #[test]
    fn terminal_uses_commas_and_keeps_unknown_visible_beyond_top_n() {
        let dataset = UsageDataset {
            events: vec![
                event(
                    1,
                    Agent::Codex,
                    Some(timestamp(2026, 7, 1)),
                    Some("openai"),
                    Some("gpt"),
                    1_234_567,
                ),
                event(2, Agent::PiAgent, None, None, None, 1),
            ],
            indexed_conversations: 10,
            indexed_messages: 100,
        };
        let terminal = render_terminal(&build_report(&dataset, &UsageFilters::default()), 1);
        assert!(terminal.contains("1,234,568"));
        assert!(terminal.contains("By Harness"));
        assert!(terminal.contains("By Provider"));
        assert!(terminal.contains("Top Models"));
        assert!(terminal.contains("Unknown"));
    }

    #[test]
    fn html_is_offline_responsive_svg_and_escapes_source_labels() {
        let dataset = UsageDataset {
            events: vec![event(
                1,
                Agent::Codex,
                Some(timestamp(2026, 7, 1)),
                Some("<script>alert('provider')</script>"),
                Some("model & friends"),
                500,
            )],
            indexed_conversations: 1,
            indexed_messages: 2,
        };
        let html = render_html(&build_report(&dataset, &UsageFilters::default()), 10);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("prefers-color-scheme:dark"));
        assert!(html.contains("<svg"));
        assert!(html.contains("&lt;script&gt;alert(&#39;provider&#39;)&lt;/script&gt;"));
        assert!(!html.contains("<script>alert('provider')</script>"));
        assert!(!html.contains("http://"));
        assert!(!html.contains("https://"));
        assert!(!html.contains("border-left"));
    }

    #[test]
    fn reasoning_is_rendered_as_an_output_subset_without_double_counting() {
        let mut row = event(
            1,
            Agent::Codex,
            Some(timestamp(2026, 7, 1)),
            Some("openai"),
            Some("gpt"),
            180,
        );
        row.tokens = TokenCounts {
            input: 100,
            output: 80,
            cache_read: 0,
            cache_write: 0,
            reasoning: 30,
            total: 180,
        };
        let report = build_report(
            &UsageDataset {
                events: vec![row],
                ..UsageDataset::default()
            },
            &UsageFilters::default(),
        );
        let terminal = render_terminal(&report, 10);
        let html = render_html(&report, 10);

        assert!(terminal.contains("includes reasoning"));
        assert!(terminal.contains("output subset"));
        assert!(html.contains("Output (non-reasoning) 50"));
        assert!(html.contains("Reasoning 30"));
        assert!(!html.contains("known model pricing"));
    }

    #[test]
    fn write_html_creates_parent_and_publishes_complete_file() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nested/reports/usage.html");
        write_html(&path, "<!doctype html><title>usage</title>").unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "<!doctype html><title>usage</title>"
        );
    }

    #[test]
    fn empty_dataset_renders_without_nan_or_panics() {
        let report = build_report(&UsageDataset::default(), &UsageFilters::default());
        let terminal = render_terminal(&report, 10);
        let html = render_html(&report, 10);
        assert_eq!(report.totals.tokens.total, 0);
        assert!(!terminal.contains("NaN"));
        assert!(!html.contains("NaN"));
        assert!(html.contains("No timestamped usage events"));
    }
}
