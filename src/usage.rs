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

use crate::model::{Agent, UsageGrain, route_host};

const UNKNOWN_KEY: &str = "__unknown__";
const UNKNOWN_LABEL: &str = "Unknown";
pub const LIST_PRICE_CATALOG_VERSION: &str = "public-list-2026-07-21";
pub const SOURCE_COVERAGE_SCOPE: &str = "full_corpus_raw_pre_report_dedup";

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
    pub logical_session_id: Option<String>,
    pub record_kind: String,
    pub is_synthetic: bool,
    pub timestamp: Option<i64>,
    pub interval_start: Option<i64>,
    pub interval_end: Option<i64>,
    pub usage_grain: UsageGrain,
    pub provider: Option<String>,
    pub provider_family: Option<String>,
    pub provider_inference_source: Option<String>,
    pub provider_inference_confidence: Option<String>,
    pub model: Option<String>,
    pub model_family: Option<String>,
    pub model_variant: Option<String>,
    pub task: Option<String>,
    pub billing_base_url: Option<String>,
    pub billing_mode: Option<String>,
    /// Stable source invocation identity used to suppress copied source rows.
    #[serde(skip)]
    pub source_event_id: Option<String>,
    /// Number of provider API calls represented by this row.
    pub api_calls: u64,
    /// All provider request attempts, including failed requests.
    pub request_attempts: u64,
    pub tokens: TokenCounts,
    pub reported_total_tokens: Option<u64>,
    pub component_total_tokens: Option<u64>,
    pub token_semantics: Option<String>,
    pub actual_cost_usd: Option<f64>,
    pub estimated_cost_usd: Option<f64>,
    pub cost_status: Option<String>,
    pub cost_source: Option<String>,
    pub cost_currency: Option<String>,
    pub pricing_version: Option<String>,
}

/// Indexed-data coverage by source harness. These are transcript-record facts,
/// distinct from provider API-call counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceCoverage {
    pub agent: String,
    pub transcript_records: u64,
    pub logical_sessions: u64,
    pub assistant_messages: u64,
    pub assistant_records: u64,
    pub usage_records: u64,
    pub usage_bearing_records: u64,
    pub assistant_without_usage_records: u64,
    pub api_calls: u64,
    pub request_attempts: u64,
    pub total_tokens: u64,
}

/// Raw usage rows plus corpus-level context used to communicate coverage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageDataset {
    pub events: Vec<UsageEventRow>,
    pub indexed_conversations: u64,
    pub indexed_messages: u64,
    pub source_coverage: Vec<SourceCoverage>,
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
    pub variants: Vec<String>,
    pub tasks: Vec<String>,
    pub exclude_synthetic: bool,
    /// Apply the bundled, versioned public list-price catalog where a model has
    /// an exact supported match. This is never treated as billing truth.
    pub estimate_list_costs: bool,
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
    /// Independent public-list estimate requested at report time.
    pub list_estimated_usd: Option<f64>,
    pub actual_events: u64,
    pub estimated_events: u64,
    pub list_estimated_events: u64,
    pub covered_tokens: u64,
    pub list_estimated_tokens: u64,
}

/// Aggregate totals for the selected usage rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    pub events: u64,
    pub api_calls: u64,
    pub request_attempts: u64,
    /// Physical transcript records represented by the selected rows.
    pub conversations: u64,
    pub logical_sessions: u64,
    pub tokens: TokenCounts,
    pub cost: UsageCost,
}

/// Completeness indicators that prevent partial source data from looking exact.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageCoverage {
    pub events_with_tokens: u64,
    pub api_calls_with_provider: u64,
    pub api_calls_with_model: u64,
    pub api_calls_with_provider_family: u64,
    pub api_calls_with_model_family: u64,
    pub tokens_with_provider: u64,
    pub tokens_with_model: u64,
    pub tokens_with_provider_family: u64,
    pub tokens_with_model_family: u64,
    pub events_with_reported_total: u64,
    pub events_with_component_total: u64,
    pub events_with_reconciled_total: u64,
    pub events_with_total_mismatch: u64,
    pub events_with_token_semantics: u64,
    pub tokens_with_token_semantics: u64,
    pub events_with_timestamp: u64,
    pub timestamped_tokens: u64,
    pub timeline_allocated_tokens: u64,
    pub temporally_unallocated_tokens: u64,
    pub temporally_excluded_tokens: u64,
    pub deduplicated_events: u64,
    pub deduplicated_tokens: u64,
    pub token_event_percent: f64,
    pub provider_percent: f64,
    pub model_percent: f64,
    pub provider_family_percent: f64,
    pub model_family_percent: f64,
    pub provider_token_percent: f64,
    pub model_token_percent: f64,
    pub provider_family_token_percent: f64,
    pub model_family_token_percent: f64,
    pub reconcilable_total_percent: f64,
    pub reconciled_total_percent: f64,
    pub token_semantics_percent: f64,
    pub token_semantics_token_percent: f64,
    pub timestamp_percent: f64,
    pub timestamp_token_percent: f64,
    pub cost_token_percent: f64,
    pub timeline_allocated_token_percent: f64,
}

/// One harness, provider, or model aggregate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageBreakdown {
    /// Stable machine key (`codex`, normalized provider/model, or `__unknown__`).
    pub key: String,
    pub label: String,
    pub events: u64,
    pub api_calls: u64,
    pub request_attempts: u64,
    pub conversations: u64,
    pub logical_sessions: u64,
    pub tokens: TokenCounts,
    pub cost: UsageCost,
}

/// Joint provider/model aggregate. Provider and model families are used when
/// known while raw dimensions stay available on JSON event data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsagePairBreakdown {
    pub provider_key: String,
    pub provider_label: String,
    pub model_key: String,
    pub model_label: String,
    pub events: u64,
    pub api_calls: u64,
    pub request_attempts: u64,
    pub conversations: u64,
    pub logical_sessions: u64,
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
    pub request_attempts: u64,
    pub conversations: u64,
    pub logical_sessions: u64,
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
    pub organic_totals: UsageTotals,
    pub coverage: UsageCoverage,
    pub by_harness: Vec<UsageBreakdown>,
    /// Raw provider adapter/route identifiers.
    pub by_provider: Vec<UsageBreakdown>,
    pub by_provider_family: Vec<UsageBreakdown>,
    /// Raw model identifiers.
    pub by_model: Vec<UsageBreakdown>,
    pub by_model_family: Vec<UsageBreakdown>,
    pub by_provider_model: Vec<UsagePairBreakdown>,
    pub by_model_variant: Vec<UsageBreakdown>,
    pub by_task: Vec<UsageBreakdown>,
    pub by_billing_mode: Vec<UsageBreakdown>,
    pub by_usage_grain: Vec<UsageBreakdown>,
    pub by_record_kind: Vec<UsageBreakdown>,
    pub by_provider_inference_source: Vec<UsageBreakdown>,
    pub by_provider_inference_confidence: Vec<UsageBreakdown>,
    pub by_token_semantics: Vec<UsageBreakdown>,
    pub by_cost_status: Vec<UsageBreakdown>,
    pub by_cost_source: Vec<UsageBreakdown>,
    pub by_pricing_version: Vec<UsageBreakdown>,
    pub trend: Vec<UsageTrendPoint>,
    pub indexed_conversations: u64,
    pub indexed_messages: u64,
    /// Scope of `source_coverage`. Assistant-bearing records without usage
    /// cannot be meaningfully filtered by provider/model/time, so this table is
    /// deliberately a raw full-corpus diagnostic rather than a selected-total
    /// breakdown.
    pub source_coverage_scope: String,
    pub source_coverage: Vec<SourceCoverage>,
    pub list_price_catalog_version: Option<String>,
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
    list_estimated_usd: f64,
    list_estimated_events: u64,
    list_estimated_tokens: u64,
}

impl CostAccumulator {
    fn add(&mut self, event: &UsageEventRow, estimate_list_costs: bool) {
        let actual = recorded_cost(event.actual_cost_usd, event.cost_status.as_deref(), true);
        let estimated = recorded_cost(
            event.estimated_cost_usd,
            event.cost_status.as_deref(),
            false,
        );

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
        }
        if actual.is_some() || estimated.is_some() {
            self.covered_tokens = self.covered_tokens.saturating_add(event.tokens.total);
        }
        if estimate_list_costs && let Some(estimate) = estimate_public_list_cost(event) {
            self.list_estimated_usd += estimate.usd;
            self.list_estimated_events += 1;
            self.list_estimated_tokens = self
                .list_estimated_tokens
                .saturating_add(estimate.covered_tokens);
        }
    }

    fn finish(self) -> UsageCost {
        UsageCost {
            actual_usd: (self.actual_events > 0).then_some(self.actual_usd),
            estimated_usd: (self.estimated_events > 0).then_some(self.estimated_usd),
            accounted_usd: (self.accounted_events > 0).then_some(self.accounted_usd),
            list_estimated_usd: (self.list_estimated_events > 0).then_some(self.list_estimated_usd),
            actual_events: self.actual_events,
            estimated_events: self.estimated_events,
            list_estimated_events: self.list_estimated_events,
            covered_tokens: self.covered_tokens,
            list_estimated_tokens: self.list_estimated_tokens,
        }
    }
}

#[derive(Debug, Default)]
struct AggregateAccumulator {
    events: u64,
    api_calls: u64,
    request_attempts: u64,
    conversations: HashSet<i64>,
    logical_sessions: HashSet<(Agent, String)>,
    tokens: TokenCounts,
    cost: CostAccumulator,
}

impl AggregateAccumulator {
    fn add(&mut self, event: &UsageEventRow, estimate_list_costs: bool) {
        self.events += 1;
        self.api_calls = self.api_calls.saturating_add(event.api_calls);
        self.request_attempts = self.request_attempts.saturating_add(event.request_attempts);
        self.conversations.insert(event.conversation_id);
        self.logical_sessions.insert((
            event.agent,
            event
                .logical_session_id
                .clone()
                .unwrap_or_else(|| format!("record:{}", event.conversation_id)),
        ));
        self.tokens.add_assign_saturating(&event.tokens);
        self.cost.add(event, estimate_list_costs);
    }

    fn finish(self) -> UsageTotals {
        UsageTotals {
            events: self.events,
            api_calls: self.api_calls,
            request_attempts: self.request_attempts,
            conversations: self.conversations.len() as u64,
            logical_sessions: self.logical_sessions.len() as u64,
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
    fn add(&mut self, label: &str, event: &UsageEventRow, estimate_list_costs: bool) {
        match self.label.as_ref() {
            Some(existing) if existing.as_str() <= label => {}
            _ => self.label = Some(label.to_string()),
        }
        self.aggregate.add(event, estimate_list_costs);
    }

    fn finish(self, key: String) -> UsageBreakdown {
        let totals = self.aggregate.finish();
        UsageBreakdown {
            key,
            label: self.label.unwrap_or_else(|| UNKNOWN_LABEL.to_string()),
            events: totals.events,
            api_calls: totals.api_calls,
            request_attempts: totals.request_attempts,
            conversations: totals.conversations,
            logical_sessions: totals.logical_sessions,
            tokens: totals.tokens,
            cost: totals.cost,
        }
    }
}

#[derive(Debug, Default)]
struct PairAccumulator {
    provider_label: Option<String>,
    model_label: Option<String>,
    aggregate: AggregateAccumulator,
}

impl PairAccumulator {
    fn add(
        &mut self,
        provider_label: &str,
        model_label: &str,
        event: &UsageEventRow,
        estimate_list_costs: bool,
    ) {
        self.provider_label
            .get_or_insert_with(|| provider_label.to_string());
        self.model_label
            .get_or_insert_with(|| model_label.to_string());
        self.aggregate.add(event, estimate_list_costs);
    }

    fn finish(self, provider_key: String, model_key: String) -> UsagePairBreakdown {
        let totals = self.aggregate.finish();
        UsagePairBreakdown {
            provider_key,
            provider_label: self
                .provider_label
                .unwrap_or_else(|| UNKNOWN_LABEL.to_string()),
            model_key,
            model_label: self
                .model_label
                .unwrap_or_else(|| UNKNOWN_LABEL.to_string()),
            events: totals.events,
            api_calls: totals.api_calls,
            request_attempts: totals.request_attempts,
            conversations: totals.conversations,
            logical_sessions: totals.logical_sessions,
            tokens: totals.tokens,
            cost: totals.cost,
        }
    }
}

/// Build a deterministic usage report from normalized usage events.
pub fn build_report(dataset: &UsageDataset, filters: &UsageFilters) -> UsageReport {
    let generated_at = Utc::now().timestamp_millis();
    let dimension_selected: Vec<_> = dataset
        .events
        .iter()
        .filter(|event| event_matches_dimensions(event, filters))
        .collect();
    let raw_event_count = dimension_selected.len() as u64;
    let raw_tokens = dimension_selected.iter().fold(0_u64, |total, event| {
        total.saturating_add(event.tokens.total)
    });
    let deduplicated = deduplicated_events(dimension_selected);
    let deduplicated_event_count = deduplicated.len() as u64;
    let deduplicated_tokens_total = deduplicated.iter().fold(0_u64, |total, event| {
        total.saturating_add(event.payload.tokens.total)
    });
    let temporally_excluded_tokens = deduplicated
        .iter()
        .filter(|event| !event_matches_time(event, filters))
        .fold(0_u64, |total, event| {
            total.saturating_add(event.payload.tokens.total)
        });
    let selected: Vec<_> = deduplicated
        .into_iter()
        .filter(|event| event_matches_time(event, filters))
        .collect();

    let observed_since = selected
        .iter()
        .filter_map(|event| event.logical_start)
        .min();
    let observed_until = selected.iter().filter_map(|event| event.logical_end).max();
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
    let mut organic_totals = AggregateAccumulator::default();
    let mut harnesses: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut providers: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut provider_families: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut models: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut model_families: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut provider_models: BTreeMap<(String, String), PairAccumulator> = BTreeMap::new();
    let mut model_variants: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut tasks: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut billing_modes: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut usage_grains: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut record_kinds: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut provider_inference_sources: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut provider_inference_confidences: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut token_semantics: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut cost_statuses: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut cost_sources: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut pricing_versions: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    let mut timeline: BTreeMap<i64, AggregateAccumulator> = BTreeMap::new();

    let mut events_with_tokens = 0_u64;
    let mut api_calls_with_provider = 0_u64;
    let mut api_calls_with_model = 0_u64;
    let mut api_calls_with_provider_family = 0_u64;
    let mut api_calls_with_model_family = 0_u64;
    let mut tokens_with_provider = 0_u64;
    let mut tokens_with_model = 0_u64;
    let mut tokens_with_provider_family = 0_u64;
    let mut tokens_with_model_family = 0_u64;
    let mut events_with_reported_total = 0_u64;
    let mut events_with_component_total = 0_u64;
    let mut events_with_reconciled_total = 0_u64;
    let mut events_with_total_mismatch = 0_u64;
    let mut events_with_token_semantics = 0_u64;
    let mut tokens_with_token_semantics = 0_u64;
    let mut events_with_timestamp = 0_u64;
    let mut timestamped_tokens = 0_u64;
    let mut timeline_allocated_tokens = 0_u64;

    for selected_event in &selected {
        let event = selected_event.payload;
        totals.add(event, filters.estimate_list_costs);
        if !event.is_synthetic {
            organic_totals.add(event, filters.estimate_list_costs);
        }

        if event.tokens.has_usage() {
            events_with_tokens += 1;
        }
        if dimension_is_known(event.provider.as_deref()) {
            api_calls_with_provider = api_calls_with_provider.saturating_add(event.api_calls);
            tokens_with_provider = tokens_with_provider.saturating_add(event.tokens.total);
        }
        if dimension_is_known(event.model.as_deref()) {
            api_calls_with_model = api_calls_with_model.saturating_add(event.api_calls);
            tokens_with_model = tokens_with_model.saturating_add(event.tokens.total);
        }
        if dimension_is_known(event.provider_family.as_deref()) {
            api_calls_with_provider_family =
                api_calls_with_provider_family.saturating_add(event.api_calls);
            tokens_with_provider_family =
                tokens_with_provider_family.saturating_add(event.tokens.total);
        }
        if dimension_is_known(event.model_family.as_deref()) {
            api_calls_with_model_family =
                api_calls_with_model_family.saturating_add(event.api_calls);
            tokens_with_model_family = tokens_with_model_family.saturating_add(event.tokens.total);
        }
        if event.reported_total_tokens.is_some() {
            events_with_reported_total += 1;
        }
        if event.component_total_tokens.is_some() {
            events_with_component_total += 1;
        }
        if let (Some(reported), Some(component)) =
            (event.reported_total_tokens, event.component_total_tokens)
        {
            if reported == component {
                events_with_reconciled_total += 1;
            } else {
                events_with_total_mismatch += 1;
            }
        }
        if dimension_is_known(event.token_semantics.as_deref()) {
            events_with_token_semantics += 1;
            tokens_with_token_semantics =
                tokens_with_token_semantics.saturating_add(event.tokens.total);
        }

        let harness_key = event.agent.slug().to_string();
        harnesses.entry(harness_key).or_default().add(
            event.agent.display_name(),
            event,
            filters.estimate_list_costs,
        );

        let (provider_key, provider_label) = dimension_key(event.provider.as_deref());
        providers.entry(provider_key).or_default().add(
            &provider_label,
            event,
            filters.estimate_list_costs,
        );

        let (provider_family_key, provider_family_label) =
            dimension_key(event.provider_family.as_deref());
        provider_families
            .entry(provider_family_key.clone())
            .or_default()
            .add(&provider_family_label, event, filters.estimate_list_costs);

        let (model_key, model_label) = dimension_key(event.model.as_deref());
        models
            .entry(model_key)
            .or_default()
            .add(&model_label, event, filters.estimate_list_costs);

        let (model_family_key, model_family_label) = dimension_key(event.model_family.as_deref());
        model_families
            .entry(model_family_key.clone())
            .or_default()
            .add(&model_family_label, event, filters.estimate_list_costs);
        provider_models
            .entry((provider_family_key, model_family_key))
            .or_default()
            .add(
                &provider_family_label,
                &model_family_label,
                event,
                filters.estimate_list_costs,
            );

        for (groups, value) in [
            (&mut model_variants, event.model_variant.as_deref()),
            (&mut tasks, event.task.as_deref()),
            (&mut billing_modes, event.billing_mode.as_deref()),
            (
                &mut provider_inference_sources,
                event.provider_inference_source.as_deref(),
            ),
            (
                &mut provider_inference_confidences,
                event.provider_inference_confidence.as_deref(),
            ),
            (&mut token_semantics, event.token_semantics.as_deref()),
            (&mut cost_sources, event.cost_source.as_deref()),
            (&mut pricing_versions, event.pricing_version.as_deref()),
        ] {
            let (key, label) = dimension_key(value);
            groups
                .entry(key)
                .or_default()
                .add(&label, event, filters.estimate_list_costs);
        }

        let grain = event.usage_grain.as_str();
        usage_grains.entry(grain.to_string()).or_default().add(
            &title_case(&grain.replace('_', " ")),
            event,
            filters.estimate_list_costs,
        );
        let record_kind = if event.record_kind.trim().is_empty() {
            "unknown"
        } else {
            event.record_kind.as_str()
        };
        record_kinds
            .entry(record_kind.to_ascii_lowercase())
            .or_default()
            .add(
                &title_case(&record_kind.replace('_', " ")),
                event,
                filters.estimate_list_costs,
            );
        let resolved_cost_status = resolved_cost_status(event, filters.estimate_list_costs);
        let (cost_key, cost_label) = dimension_key(Some(&resolved_cost_status));
        cost_statuses.entry(cost_key).or_default().add(
            &cost_label,
            event,
            filters.estimate_list_costs,
        );

        if event.usage_grain == UsageGrain::Event
            && let Some(timestamp) = selected_event.logical_start
        {
            events_with_timestamp += 1;
            timestamped_tokens = timestamped_tokens.saturating_add(event.tokens.total);
            if let Some(start) = bucket_start(timestamp, bucket) {
                timeline
                    .entry(start)
                    .or_default()
                    .add(event, filters.estimate_list_costs);
                timeline_allocated_tokens =
                    timeline_allocated_tokens.saturating_add(event.tokens.total);
            }
        } else if let (Some(start), Some(end)) =
            (selected_event.logical_start, selected_event.logical_end)
            && let (Some(start_bucket), Some(end_bucket)) =
                (bucket_start(start, bucket), bucket_start(end, bucket))
            && start_bucket == end_bucket
        {
            timeline
                .entry(start_bucket)
                .or_default()
                .add(event, filters.estimate_list_costs);
            timeline_allocated_tokens =
                timeline_allocated_tokens.saturating_add(event.tokens.total);
        }
    }

    let totals = totals.finish();
    let organic_totals = organic_totals.finish();
    let event_count = totals.events;
    let api_call_count = totals.api_calls;
    let coverage = UsageCoverage {
        events_with_tokens,
        api_calls_with_provider,
        api_calls_with_model,
        api_calls_with_provider_family,
        api_calls_with_model_family,
        tokens_with_provider,
        tokens_with_model,
        tokens_with_provider_family,
        tokens_with_model_family,
        events_with_reported_total,
        events_with_component_total,
        events_with_reconciled_total,
        events_with_total_mismatch,
        events_with_token_semantics,
        tokens_with_token_semantics,
        events_with_timestamp,
        timestamped_tokens,
        timeline_allocated_tokens,
        temporally_unallocated_tokens: totals
            .tokens
            .total
            .saturating_sub(timeline_allocated_tokens),
        temporally_excluded_tokens,
        deduplicated_events: raw_event_count.saturating_sub(deduplicated_event_count),
        deduplicated_tokens: raw_tokens.saturating_sub(deduplicated_tokens_total),
        token_event_percent: percent(events_with_tokens, event_count),
        provider_percent: percent(api_calls_with_provider, api_call_count),
        model_percent: percent(api_calls_with_model, api_call_count),
        provider_family_percent: percent(api_calls_with_provider_family, api_call_count),
        model_family_percent: percent(api_calls_with_model_family, api_call_count),
        provider_token_percent: percent(tokens_with_provider, totals.tokens.total),
        model_token_percent: percent(tokens_with_model, totals.tokens.total),
        provider_family_token_percent: percent(tokens_with_provider_family, totals.tokens.total),
        model_family_token_percent: percent(tokens_with_model_family, totals.tokens.total),
        reconcilable_total_percent: percent(
            events_with_reconciled_total.saturating_add(events_with_total_mismatch),
            event_count,
        ),
        reconciled_total_percent: percent(
            events_with_reconciled_total,
            events_with_reconciled_total.saturating_add(events_with_total_mismatch),
        ),
        token_semantics_percent: percent(events_with_token_semantics, event_count),
        token_semantics_token_percent: percent(tokens_with_token_semantics, totals.tokens.total),
        timestamp_percent: percent(events_with_timestamp, event_count),
        timestamp_token_percent: percent(timestamped_tokens, totals.tokens.total),
        cost_token_percent: percent(totals.cost.covered_tokens, totals.tokens.total),
        timeline_allocated_token_percent: percent(timeline_allocated_tokens, totals.tokens.total),
    };

    let mut by_harness = finish_breakdowns(harnesses);
    let mut by_provider = finish_breakdowns(providers);
    let mut by_provider_family = finish_breakdowns(provider_families);
    let mut by_model = finish_breakdowns(models);
    let mut by_model_family = finish_breakdowns(model_families);
    let mut by_model_variant = finish_breakdowns(model_variants);
    let mut by_task = finish_breakdowns(tasks);
    let mut by_billing_mode = finish_breakdowns(billing_modes);
    let mut by_usage_grain = finish_breakdowns(usage_grains);
    let mut by_record_kind = finish_breakdowns(record_kinds);
    let mut by_provider_inference_source = finish_breakdowns(provider_inference_sources);
    let mut by_provider_inference_confidence = finish_breakdowns(provider_inference_confidences);
    let mut by_token_semantics = finish_breakdowns(token_semantics);
    let mut by_cost_status = finish_breakdowns(cost_statuses);
    let mut by_cost_source = finish_breakdowns(cost_sources);
    let mut by_pricing_version = finish_breakdowns(pricing_versions);
    let mut by_provider_model: Vec<_> = provider_models
        .into_iter()
        .map(|((provider, model), group)| group.finish(provider, model))
        .collect();
    sort_breakdowns(&mut by_harness);
    sort_breakdowns(&mut by_provider);
    sort_breakdowns(&mut by_provider_family);
    sort_breakdowns(&mut by_model);
    sort_breakdowns(&mut by_model_family);
    sort_breakdowns(&mut by_model_variant);
    sort_breakdowns(&mut by_task);
    sort_breakdowns(&mut by_billing_mode);
    sort_breakdowns(&mut by_usage_grain);
    sort_breakdowns(&mut by_record_kind);
    sort_breakdowns(&mut by_provider_inference_source);
    sort_breakdowns(&mut by_provider_inference_confidence);
    sort_breakdowns(&mut by_token_semantics);
    sort_breakdowns(&mut by_cost_status);
    sort_breakdowns(&mut by_cost_source);
    sort_breakdowns(&mut by_pricing_version);
    by_provider_model.sort_by(|left, right| {
        right
            .tokens
            .total
            .cmp(&left.tokens.total)
            .then_with(|| left.provider_key.cmp(&right.provider_key))
            .then_with(|| left.model_key.cmp(&right.model_key))
    });

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
        organic_totals,
        coverage,
        by_harness,
        by_provider,
        by_provider_family,
        by_model,
        by_model_family,
        by_provider_model,
        by_model_variant,
        by_task,
        by_billing_mode,
        by_usage_grain,
        by_record_kind,
        by_provider_inference_source,
        by_provider_inference_confidence,
        by_token_semantics,
        by_cost_status,
        by_cost_source,
        by_pricing_version,
        trend,
        indexed_conversations: dataset.indexed_conversations,
        indexed_messages: dataset.indexed_messages,
        source_coverage_scope: SOURCE_COVERAGE_SCOPE.to_string(),
        source_coverage: dataset.source_coverage.clone(),
        list_price_catalog_version: filters
            .estimate_list_costs
            .then(|| LIST_PRICE_CATALOG_VERSION.to_string()),
    }
}

#[derive(Debug, Clone, Copy)]
struct DeduplicatedEvent<'a> {
    payload: &'a UsageEventRow,
    logical_start: Option<i64>,
    logical_end: Option<i64>,
    event_time_identity: bool,
}

impl<'a> DeduplicatedEvent<'a> {
    fn new(payload: &'a UsageEventRow) -> Self {
        let logical_start = event_start(payload);
        Self {
            payload,
            logical_start,
            logical_end: event_end(payload),
            event_time_identity: payload.usage_grain == UsageGrain::Event,
        }
    }

    fn merge(&mut self, candidate: &'a UsageEventRow) {
        let candidate_is_event = candidate.usage_grain == UsageGrain::Event;
        let event_time_identity = self.event_time_identity && candidate_is_event;
        self.logical_start = earliest_known(self.logical_start, event_start(candidate));
        self.logical_end = if event_time_identity {
            // Copied event rows can be re-emitted by a later transcript. Their
            // logical time is the earliest observation even when the later row
            // has the richer token/cost payload.
            self.logical_start
        } else {
            latest_known(self.logical_end, event_end(candidate))
        };
        self.event_time_identity = event_time_identity;

        if event_preference(candidate) > event_preference(self.payload) {
            self.payload = candidate;
        }
    }
}

fn earliest_known(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (known @ Some(_), None) | (None, known @ Some(_)) => known,
        (None, None) => None,
    }
}

fn latest_known(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (known @ Some(_), None) | (None, known @ Some(_)) => known,
        (None, None) => None,
    }
}

fn deduplicated_events<'a>(
    events: impl IntoIterator<Item = &'a UsageEventRow>,
) -> Vec<DeduplicatedEvent<'a>> {
    let mut identified: HashMap<(Agent, &str), DeduplicatedEvent<'a>> = HashMap::new();
    let mut output = Vec::new();

    for event in events {
        let Some(source_event_id) = event
            .source_event_id
            .as_deref()
            .map(str::trim)
            .filter(|identity| !identity.is_empty())
        else {
            output.push(DeduplicatedEvent::new(event));
            continue;
        };
        identified
            .entry((event.agent, source_event_id))
            .and_modify(|existing| existing.merge(event))
            .or_insert_with(|| DeduplicatedEvent::new(event));
    }

    output.extend(identified.into_values());
    output.sort_by_key(|event| {
        (
            event.logical_start.unwrap_or(i64::MIN),
            event.payload.conversation_id,
            event.payload.source_event_id.as_deref().unwrap_or(""),
        )
    });
    output
}

fn event_preference(
    event: &UsageEventRow,
) -> (
    u64,
    u8,
    bool,
    Reverse<i64>,
    bool,
    bool,
    bool,
    bool,
    Reverse<i64>,
) {
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
        event_start(event).is_some(),
        Reverse(event_start(event).unwrap_or(i64::MAX)),
        dimension_is_known(event.provider.as_deref()),
        dimension_is_known(event.model.as_deref()),
        dimension_is_known(event.provider_family.as_deref()),
        recorded_cost(event.actual_cost_usd, event.cost_status.as_deref(), true).is_some()
            || recorded_cost(
                event.estimated_cost_usd,
                event.cost_status.as_deref(),
                false,
            )
            .is_some(),
        Reverse(event.conversation_id),
    )
}

fn event_matches_dimensions(event: &UsageEventRow, filters: &UsageFilters) -> bool {
    if !filters.agents.is_empty() && !filters.agents.contains(&event.agent) {
        return false;
    }
    if filters.exclude_synthetic && event.is_synthetic {
        return false;
    }
    if !dimension_matches_either(
        event.provider.as_deref(),
        event.provider_family.as_deref(),
        &filters.providers,
    ) {
        return false;
    }
    if !dimension_matches_either(
        event.model.as_deref(),
        event.model_family.as_deref(),
        &filters.models,
    ) {
        return false;
    }
    if !dimension_matches(event.model_variant.as_deref(), &filters.variants) {
        return false;
    }
    if !dimension_matches(event.task.as_deref(), &filters.tasks) {
        return false;
    }
    if let Some(workspace) = filters.workspace.as_deref()
        && event.workspace.as_deref() != Some(workspace)
    {
        return false;
    }
    true
}

fn event_matches_time(event: &DeduplicatedEvent<'_>, filters: &UsageFilters) -> bool {
    if filters.since.is_some() || filters.until.is_some() {
        let (Some(start), Some(end)) = (event.logical_start, event.logical_end) else {
            return false;
        };
        // Aggregate rows are indivisible. Include them only when their complete
        // source interval is inside the requested range.
        if filters.since.is_some_and(|since| start < since)
            || filters.until.is_some_and(|until| end > until)
        {
            return false;
        }
    }
    true
}

fn event_start(event: &UsageEventRow) -> Option<i64> {
    match event.usage_grain {
        UsageGrain::Event => event.timestamp,
        UsageGrain::IntervalAggregate | UsageGrain::SessionAggregate => event.interval_start,
    }
}

fn event_end(event: &UsageEventRow) -> Option<i64> {
    match event.usage_grain {
        UsageGrain::Event => event.timestamp,
        UsageGrain::IntervalAggregate | UsageGrain::SessionAggregate => event.interval_end,
    }
}

fn dimension_matches_either(
    raw: Option<&str>,
    canonical: Option<&str>,
    filters: &[String],
) -> bool {
    filters.is_empty()
        || dimension_matches(raw, filters)
        || (canonical != raw && dimension_matches(canonical, filters))
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
        request_attempts: totals.request_attempts,
        conversations: totals.conversations,
        logical_sessions: totals.logical_sessions,
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

fn recorded_cost(value: Option<f64>, status: Option<&str>, actual: bool) -> Option<f64> {
    let value = value.filter(|cost| cost.is_finite() && *cost >= 0.0)?;
    if value > 0.0 {
        return Some(value);
    }
    let status = status.unwrap_or_default().trim().to_ascii_lowercase();
    let explicitly_recorded = if actual {
        matches!(status.as_str(), "actual" | "reported_actual")
    } else {
        matches!(
            status.as_str(),
            "estimated" | "source_estimated" | "source_reported_zero" | "included"
        )
    };
    explicitly_recorded.then_some(value)
}

fn resolved_cost_status(event: &UsageEventRow, estimate_list_costs: bool) -> String {
    if recorded_cost(event.actual_cost_usd, event.cost_status.as_deref(), true).is_some() {
        return "reported_actual".to_string();
    }
    if recorded_cost(
        event.estimated_cost_usd,
        event.cost_status.as_deref(),
        false,
    )
    .is_some()
    {
        return match event
            .cost_status
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "included" => "included".to_string(),
            "source_reported_zero" => "source_reported_zero".to_string(),
            _ => "source_estimated".to_string(),
        };
    }
    if estimate_list_costs && estimate_public_list_cost(event).is_some() {
        return "public_list_estimated".to_string();
    }
    event
        .cost_status
        .as_deref()
        .map(str::trim)
        .filter(|status| !status.is_empty() && !status.eq_ignore_ascii_case("unknown"))
        .unwrap_or("unavailable")
        .to_string()
}

#[derive(Debug, Clone, Copy)]
struct ListRates {
    input: f64,
    cached_input: f64,
    cache_write: f64,
    output: f64,
}

#[derive(Debug, Clone, Copy)]
struct ListEstimate {
    usd: f64,
    covered_tokens: u64,
}

/// Conservative, exact-match public list-price estimator.
///
/// Rates are USD per million tokens and intentionally cover only model/provider
/// pairs whose standard first-party token pricing has unambiguous semantics.
/// Known per-request long-context tiers are applied only when one event
/// represents one call. Contract, subscription, regional, priority, batch,
/// gateway, ambiguous aggregate, and tool fees are not guessed. The catalog
/// version is surfaced in every report that opts in.
fn estimate_public_list_cost(event: &UsageEventRow) -> Option<ListEstimate> {
    // Never price a source total whose billable components are incomplete.
    // A zero/partial component estimate presented as full coverage is worse
    // than leaving the row explicitly unpriced.
    let component_total = event
        .tokens
        .input
        .saturating_add(event.tokens.output)
        .saturating_add(event.tokens.cache_read)
        .saturating_add(event.tokens.cache_write);
    if event.tokens.total == 0
        || component_total != event.tokens.total
        || event.component_total_tokens != Some(event.tokens.total)
    {
        return None;
    }
    if event.billing_mode.as_deref().is_some_and(|mode| {
        let mode = mode.to_ascii_lowercase();
        [
            "batch",
            "flex",
            "priority",
            "regional",
            "residency",
            "fast",
            "subscription",
            "contract",
            "oauth",
            "provisioned",
        ]
        .iter()
        .any(|modifier| mode.contains(modifier))
    }) {
        return None;
    }

    let provider = event
        .provider_family
        .as_deref()?
        .trim()
        .to_ascii_lowercase();
    if !is_first_party_list_price_route(event, &provider) {
        return None;
    }
    let raw_model = event.model.as_deref()?.trim().to_ascii_lowercase();
    let model = raw_model.rsplit('/').next().unwrap_or(&raw_model);

    let rates = match provider.as_str() {
        "openai" => {
            let (rates, long_context_threshold, cache_write_supported) = match model {
                "gpt-5.6" | "gpt-5.6-sol" => (
                    ListRates {
                        input: 5.0,
                        cached_input: 0.5,
                        cache_write: 6.25,
                        output: 30.0,
                    },
                    Some(272_000),
                    true,
                ),
                "gpt-5.6-terra" => (
                    ListRates {
                        input: 2.5,
                        cached_input: 0.25,
                        cache_write: 3.125,
                        output: 15.0,
                    },
                    Some(272_000),
                    true,
                ),
                "gpt-5.6-luna" => (
                    ListRates {
                        input: 1.0,
                        cached_input: 0.1,
                        cache_write: 1.25,
                        output: 6.0,
                    },
                    Some(272_000),
                    true,
                ),
                "gpt-5.5" => (
                    ListRates {
                        input: 5.0,
                        cached_input: 0.5,
                        cache_write: 0.0,
                        output: 30.0,
                    },
                    Some(272_000),
                    false,
                ),
                "gpt-5.4" => (
                    ListRates {
                        input: 2.5,
                        cached_input: 0.25,
                        cache_write: 0.0,
                        output: 15.0,
                    },
                    Some(272_000),
                    false,
                ),
                "gpt-5.4-mini" => (
                    ListRates {
                        input: 0.75,
                        cached_input: 0.075,
                        cache_write: 0.0,
                        output: 4.5,
                    },
                    None,
                    false,
                ),
                "gpt-5.3-codex" | "gpt-5.2-codex" | "gpt-5.2" => (
                    ListRates {
                        input: 1.75,
                        cached_input: 0.175,
                        cache_write: 0.0,
                        output: 14.0,
                    },
                    None,
                    false,
                ),
                "gpt-5" | "gpt-5-codex" => (
                    ListRates {
                        input: 1.25,
                        cached_input: 0.125,
                        cache_write: 0.0,
                        output: 10.0,
                    },
                    None,
                    false,
                ),
                "gpt-5-mini" => (
                    ListRates {
                        input: 0.25,
                        cached_input: 0.025,
                        cache_write: 0.0,
                        output: 2.0,
                    },
                    None,
                    false,
                ),
                _ => return None,
            };
            if event.tokens.cache_write > 0 && !cache_write_supported {
                return None;
            }
            if let Some(threshold) = long_context_threshold {
                if event.usage_grain != UsageGrain::Event || event.api_calls != 1 {
                    return None;
                }
                let prompt_tokens = event
                    .tokens
                    .input
                    .saturating_add(event.tokens.cache_read)
                    .saturating_add(event.tokens.cache_write);
                if prompt_tokens > threshold {
                    return priced_estimate(event, rates, 2.0, 1.5);
                }
            }
            rates
        }
        "anthropic" => {
            // Cache creation does not retain the 5m/1h split in every harness,
            // so those rows cannot be priced exactly (1.25x vs 2x input).
            if event.tokens.cache_write > 0 {
                return None;
            }
            if model.starts_with("claude-opus-4-8")
                || model.starts_with("claude-opus-4-7")
                || model.starts_with("claude-opus-4-6")
                || model.starts_with("claude-opus-4-5")
                || matches!(
                    model,
                    "claude-opus-4.8" | "claude-opus-4.7" | "claude-opus-4.6" | "claude-opus-4.5"
                )
            {
                ListRates {
                    input: 5.0,
                    cached_input: 0.5,
                    cache_write: 0.0,
                    output: 25.0,
                }
            } else if matches!(
                model,
                "claude-opus-4-1" | "claude-opus-4.1" | "claude-opus-4"
            ) {
                ListRates {
                    input: 15.0,
                    cached_input: 1.5,
                    cache_write: 0.0,
                    output: 75.0,
                }
            } else if model == "claude-sonnet-5" {
                // Introductory rate in effect through 2026-08-31.
                ListRates {
                    input: 2.0,
                    cached_input: 0.2,
                    cache_write: 0.0,
                    output: 10.0,
                }
            } else if model.starts_with("claude-sonnet-4-") {
                ListRates {
                    input: 3.0,
                    cached_input: 0.3,
                    cache_write: 0.0,
                    output: 15.0,
                }
            } else if model == "claude-fable-5" || model == "claude-mythos-5" {
                ListRates {
                    input: 10.0,
                    cached_input: 1.0,
                    cache_write: 0.0,
                    output: 50.0,
                }
            } else {
                return None;
            }
        }
        "google"
            if matches!(
                model,
                "gemini-3.1-pro-preview" | "gemini-3.1-pro-preview-customtools"
            ) =>
        {
            if event.tokens.cache_write > 0 {
                return None;
            }
            // The source row must be one invocation because Google switches the
            // entire request to long-context rates above 200k prompt tokens.
            if event.usage_grain != UsageGrain::Event || event.api_calls != 1 {
                return None;
            }
            if event.tokens.input.saturating_add(event.tokens.cache_read) <= 200_000 {
                ListRates {
                    input: 2.0,
                    cached_input: 0.2,
                    cache_write: 0.0,
                    output: 12.0,
                }
            } else {
                ListRates {
                    input: 4.0,
                    cached_input: 0.4,
                    cache_write: 0.0,
                    output: 18.0,
                }
            }
        }
        "xai" => {
            if event.tokens.cache_write > 0 {
                return None;
            }
            match model {
                "grok-4.5" => ListRates {
                    input: 2.0,
                    cached_input: 0.3,
                    cache_write: 0.0,
                    output: 6.0,
                },
                "grok-4.3"
                | "grok-4.20-0309-reasoning"
                | "grok-4.20-0309-non-reasoning"
                | "grok-4.20-multi-agent-0309" => ListRates {
                    input: 1.25,
                    cached_input: 0.2,
                    cache_write: 0.0,
                    output: 2.5,
                },
                _ => return None,
            }
        }
        _ => return None,
    };
    priced_estimate(event, rates, 1.0, 1.0)
}

/// Public API list prices are only comparable for direct first-party routes.
/// Canonical provider families deliberately group hosted and subscription
/// adapters with their underlying model provider, so they are not sufficient
/// billing evidence on their own. An explicit first-party API URL may prove a
/// direct route even when the harness uses a custom or hosted provider label.
fn is_first_party_list_price_route(event: &UsageEventRow, provider_family: &str) -> bool {
    if let Some(base_url) = event
        .billing_base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let Some(host) = route_host(&base_url.to_ascii_lowercase()) else {
            return false;
        };
        return matches!(
            (provider_family, host.as_str()),
            ("openai", "api.openai.com")
                | ("anthropic", "api.anthropic.com")
                | ("google", "generativelanguage.googleapis.com")
                | ("xai", "api.x.ai")
        );
    }

    let raw_provider = event
        .provider
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        (provider_family, raw_provider.as_str()),
        ("openai", "openai")
            | ("anthropic", "anthropic")
            | ("google", "google")
            | ("xai", "xai" | "x-ai")
    )
}

fn priced_estimate(
    event: &UsageEventRow,
    rates: ListRates,
    input_multiplier: f64,
    output_multiplier: f64,
) -> Option<ListEstimate> {
    let million = 1_000_000.0;
    let usd = event.tokens.input as f64 * rates.input * input_multiplier / million
        + event.tokens.cache_read as f64 * rates.cached_input * input_multiplier / million
        + event.tokens.cache_write as f64 * rates.cache_write * input_multiplier / million
        + event.tokens.output as f64 * rates.output * output_multiplier / million;
    (usd.is_finite() && usd >= 0.0).then_some(ListEstimate {
        usd,
        covered_tokens: event.tokens.total,
    })
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
    writeln!(
        output,
        "Filters: {}",
        sanitize_terminal(&format_filters(&report.filters))
    )
    .unwrap();
    if report.range.trend_is_sparse {
        writeln!(
            output,
            "Timeline: zero-only gaps omitted; all observed buckets and range boundaries retained"
        )
        .unwrap();
    }
    writeln!(
        output,
        "Selected: {} API calls / {} attempts across {} logical sessions",
        format_count(report.totals.api_calls),
        format_count(report.totals.request_attempts),
        format_count(report.totals.logical_sessions),
    )
    .unwrap();
    writeln!(
        output,
        "Records: {} transcript records · {} usage records · organic {} tokens",
        format_count(report.totals.conversations),
        format_count(report.totals.events),
        format_count(report.organic_totals.tokens.total),
    )
    .unwrap();
    writeln!(
        output,
        "Indexed corpus: {} transcript records · {} messages",
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
        "  Raw provider IDs      {:>5.1}% calls · {:>5.1}% tokens",
        report.coverage.provider_percent, report.coverage.provider_token_percent,
    )
    .unwrap();
    writeln!(
        output,
        "  Raw model IDs         {:>5.1}% calls · {:>5.1}% tokens",
        report.coverage.model_percent, report.coverage.model_token_percent,
    )
    .unwrap();
    writeln!(
        output,
        "  Provider families     {:>5.1}% calls · {:>5.1}% tokens",
        report.coverage.provider_family_percent, report.coverage.provider_family_token_percent,
    )
    .unwrap();
    writeln!(
        output,
        "  Model families        {:>5.1}% calls · {:>5.1}% tokens",
        report.coverage.model_family_percent, report.coverage.model_family_token_percent,
    )
    .unwrap();
    writeln!(
        output,
        "  Token semantics       {:>5.1}% rows · {:>5.1}% tokens",
        report.coverage.token_semantics_percent, report.coverage.token_semantics_token_percent,
    )
    .unwrap();
    writeln!(
        output,
        "  Comparable totals      {:>5.1}% of rows · {} mismatches",
        report.coverage.reconcilable_total_percent,
        format_count(report.coverage.events_with_total_mismatch),
    )
    .unwrap();
    writeln!(
        output,
        "  ↳ Reconciled totals     {:>5.1}% of comparable rows",
        report.coverage.reconciled_total_percent,
    )
    .unwrap();
    writeln!(
        output,
        "  Exact-time tokens    {:>5.1}% · allocated {:>5.1}%",
        report.coverage.timestamp_token_percent, report.coverage.timeline_allocated_token_percent,
    )
    .unwrap();
    writeln!(
        output,
        "  Cost-recorded tokens {:>5.1}%",
        report.coverage.cost_token_percent
    )
    .unwrap();
    if report.coverage.temporally_unallocated_tokens > 0
        || report.coverage.temporally_excluded_tokens > 0
        || report.coverage.deduplicated_events > 0
    {
        writeln!(
            output,
            "  Unallocated {} · filter-excluded {} · dedup removed {} records / {} tokens",
            format_count(report.coverage.temporally_unallocated_tokens),
            format_count(report.coverage.temporally_excluded_tokens),
            format_count(report.coverage.deduplicated_events),
            format_count(report.coverage.deduplicated_tokens),
        )
        .unwrap();
    }

    if report.totals.cost.actual_usd.is_some()
        || report.totals.cost.estimated_usd.is_some()
        || report.totals.cost.list_estimated_usd.is_some()
    {
        writeln!(output).unwrap();
        writeln!(output, "Cost").unwrap();
        if let Some(cost) = report.totals.cost.actual_usd {
            writeln!(output, "  Reported actual  {:>16}", format_usd(cost)).unwrap();
        }
        if let Some(cost) = report.totals.cost.estimated_usd {
            writeln!(output, "  Source estimate  {:>16}", format_usd(cost)).unwrap();
        }
        if let Some(cost) = report.totals.cost.list_estimated_usd {
            writeln!(output, "  Public-list est. {:>16}", format_usd(cost)).unwrap();
            writeln!(
                output,
                "  Catalog {} · {:.1}% token coverage",
                report
                    .list_price_catalog_version
                    .as_deref()
                    .unwrap_or("unknown"),
                percent(
                    report.totals.cost.list_estimated_tokens,
                    report.totals.tokens.total,
                )
            )
            .unwrap();
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
        "By Provider Family (Canonical)",
        "Provider",
        &report.by_provider_family,
        report.totals.tokens.total,
        top,
        false,
    );
    render_terminal_breakdown(
        &mut output,
        "By Provider Route (Raw IDs)",
        "Route",
        &report.by_provider,
        report.totals.tokens.total,
        top,
        false,
    );
    render_terminal_breakdown(
        &mut output,
        "By Model Family (Canonical)",
        "Family",
        &report.by_model_family,
        report.totals.tokens.total,
        top,
        false,
    );
    render_terminal_breakdown(
        &mut output,
        "Top Models (Raw IDs)",
        "Model",
        &report.by_model,
        report.totals.tokens.total,
        top,
        false,
    );
    render_terminal_pairs(&mut output, report, top);
    render_terminal_breakdown(
        &mut output,
        "By Usage Grain",
        "Grain",
        &report.by_usage_grain,
        report.totals.tokens.total,
        top,
        false,
    );
    render_terminal_breakdown(
        &mut output,
        "By Transcript Kind",
        "Kind",
        &report.by_record_kind,
        report.totals.tokens.total,
        top,
        false,
    );
    render_terminal_breakdown(
        &mut output,
        "By Cost Status",
        "Status",
        &report.by_cost_status,
        report.totals.tokens.total,
        top,
        false,
    );
    render_terminal_breakdown(
        &mut output,
        "By Provider Inference Confidence",
        "Confidence",
        &report.by_provider_inference_confidence,
        report.totals.tokens.total,
        top,
        false,
    );
    render_terminal_breakdown(
        &mut output,
        "By Token Semantics",
        "Semantics",
        &report.by_token_semantics,
        report.totals.tokens.total,
        top,
        false,
    );
    render_source_coverage_terminal(&mut output, &report.source_coverage);

    writeln!(output).unwrap();
    writeln!(output, "Notes:").unwrap();
    writeln!(
        output,
        "  Cache buckets are separate from input; reasoning is a subset of output."
    )
    .unwrap();
    writeln!(
        output,
        "  Aggregate intervals are never assigned across bucket boundaries."
    )
    .unwrap();
    writeln!(output, "  Source and public-list estimates are not a bill.").unwrap();
    output
}

fn render_terminal_pairs(output: &mut String, report: &UsageReport, top: usize) {
    writeln!(output).unwrap();
    writeln!(output, "Top Provider × Model Families").unwrap();
    writeln!(
        output,
        "  {:<39} {:>16}  {:>7}  {:>10}",
        "Route", "Tokens", "Share", "Calls"
    )
    .unwrap();
    for row in report.by_provider_model.iter().take(top) {
        let label = truncate_chars(
            &sanitize_terminal(&format!("{} × {}", row.provider_label, row.model_label)),
            39,
        );
        writeln!(
            output,
            "  {label:<39} {:>16}  {:>6.1}%  {:>10}",
            format_count(row.tokens.total),
            percent(row.tokens.total, report.totals.tokens.total),
            format_count(row.api_calls),
        )
        .unwrap();
    }
}

fn render_source_coverage_terminal(output: &mut String, rows: &[SourceCoverage]) {
    if rows.is_empty() {
        return;
    }
    writeln!(output).unwrap();
    writeln!(output, "Full-Corpus Source Coverage (Unfiltered)").unwrap();
    writeln!(output, "  Raw indexed rows before report deduplication").unwrap();
    writeln!(
        output,
        "  {:<17} {:>10} {:>10} {:>10} {:>11} {:>12}",
        "Harness", "Records", "Logical", "Usage", "No-usage", "Tokens"
    )
    .unwrap();
    for row in rows {
        writeln!(
            output,
            "  {:<17} {:>10} {:>10} {:>10} {:>11} {:>12}",
            truncate_chars(&sanitize_terminal(&row.agent), 17),
            format_count(row.transcript_records),
            format_count(row.logical_sessions),
            format_count(row.usage_bearing_records),
            format_count(row.assistant_without_usage_records),
            format_compact(row.total_tokens),
        )
        .unwrap();
    }
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
    let visible = visible_rows(rows, top);
    let omitted = rows.len().saturating_sub(visible.len());
    for row in visible {
        let icon = if show_harness_icon {
            harness_icon(&row.key)
        } else {
            ""
        };
        let label = if icon.is_empty() {
            truncate_chars(&sanitize_terminal(&row.label), 29)
        } else {
            truncate_chars(&sanitize_terminal(&format!("{icon} {}", row.label)), 29)
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
    if omitted > 0 {
        writeln!(output, "  … {omitted} more").unwrap();
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
        .or_else(|| {
            report
                .totals
                .cost
                .list_estimated_usd
                .map(|cost| format!("list {}", format_approx_usd(cost)))
        })
        .unwrap_or_else(|| "Not available".to_string());
    let cost_label = match (
        report.totals.cost.actual_usd.is_some(),
        report.totals.cost.estimated_usd.is_some(),
    ) {
        (true, false) => "Reported cost",
        (false, true) => "Recorded estimate",
        (true, true) => "Cost accounted",
        (false, false) if report.totals.cost.list_estimated_usd.is_some() => "Public-list estimate",
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
        &report.by_provider_family,
        top,
        report.totals.tokens.total,
        "good",
        false,
    );
    let raw_providers = render_split_html(
        &report.by_provider,
        top,
        report.totals.tokens.total,
        "accent",
        false,
    );
    let model_families = render_split_html(
        &report.by_model_family,
        top,
        report.totals.tokens.total,
        "good",
        false,
    );
    let models = render_models_html(&report.by_model, top, report.totals.tokens.total);
    let provider_models = render_provider_models_html(report, top);
    let usage_grains = render_split_html(
        &report.by_usage_grain,
        top,
        report.totals.tokens.total,
        "accent",
        false,
    );
    let record_kinds = render_split_html(
        &report.by_record_kind,
        top,
        report.totals.tokens.total,
        "good",
        false,
    );
    let cost_statuses = render_split_html(
        &report.by_cost_status,
        top,
        report.totals.tokens.total,
        "warn",
        false,
    );
    let provider_confidences = render_split_html(
        &report.by_provider_inference_confidence,
        top,
        report.totals.tokens.total,
        "good",
        false,
    );
    let token_semantics = render_split_html(
        &report.by_token_semantics,
        top,
        report.totals.tokens.total,
        "accent",
        false,
    );
    let model_variants = render_split_html(
        &report.by_model_variant,
        top,
        report.totals.tokens.total,
        "accent",
        false,
    );
    let tasks = render_split_html(
        &report.by_task,
        top,
        report.totals.tokens.total,
        "good",
        false,
    );
    let billing_modes = render_split_html(
        &report.by_billing_mode,
        top,
        report.totals.tokens.total,
        "warn",
        false,
    );
    let cost_sources = render_split_html(
        &report.by_cost_source,
        top,
        report.totals.tokens.total,
        "warn",
        false,
    );
    let pricing_versions = render_split_html(
        &report.by_pricing_version,
        top,
        report.totals.tokens.total,
        "warn",
        false,
    );
    let source_coverage = render_source_coverage_html(&report.source_coverage);
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
    let organic_share = percent(
        report.organic_totals.tokens.total,
        report.totals.tokens.total,
    );
    let synthetic_tokens = report
        .totals
        .tokens
        .total
        .saturating_sub(report.organic_totals.tokens.total);
    let cost_note = if let Some(version) = report.list_price_catalog_version.as_deref() {
        format!(
            "Public-list catalog {} covers {:.1}% of selected tokens.",
            version,
            percent(
                report.totals.cost.list_estimated_tokens,
                report.totals.tokens.total,
            )
        )
    } else {
        "Public-list estimation was not requested.".to_string()
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
    .coverage{{display:grid;grid-template-columns:repeat(5,1fr);gap:12px;margin:0 0 34px}}
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
    .fill.good{{background:var(--good)}}.fill.warn{{background:var(--warn)}}
    .split-meta{{display:flex;justify-content:space-between;margin-top:5px;color:var(--text-dim);font-size:.7rem}}
    .omitted{{margin-top:14px;padding-top:10px;border-top:1px solid var(--border)}}
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
    .table-wrap{{overflow-x:auto}}
    table{{width:100%;border-collapse:collapse;font-size:.82rem}}
    th{{padding:0 10px 10px;color:var(--text-dim);font-size:.68rem;letter-spacing:.08em;text-align:right;text-transform:uppercase}}
    th:first-child,td:first-child{{text-align:left}}
    td{{padding:11px 10px;border-top:1px solid var(--border);text-align:right;white-space:nowrap}}
    td:first-child{{max-width:420px;color:var(--text-strong);font-weight:650;overflow:hidden;text-overflow:ellipsis}}
    .quality-grid{{display:grid;grid-template-columns:repeat(3,1fr);gap:12px}}
    .quality-card{{padding:16px;border:1px solid var(--border);border-radius:14px;background:var(--recessed)}}
    .quality-card strong{{display:block;color:var(--text-strong);font-size:1.25rem}}
    .quality-card span{{color:var(--text-dim);font-size:.75rem}}
    footer{{display:flex;justify-content:space-between;gap:20px;margin-top:30px;padding:18px 2px;color:var(--text-dim);font-size:.75rem}}
    @media(max-width:900px){{.hero{{grid-template-columns:1fr 1fr}}.metric.primary{{grid-column:1/-1}}.coverage{{grid-template-columns:1fr 1fr}}.two-up{{grid-template-columns:1fr}}.quality-grid{{grid-template-columns:1fr}}}}
    @media(max-width:560px){{main{{width:min(100% - 22px,1180px);padding-top:30px}}.hero{{grid-template-columns:1fr}}.metric.primary{{grid-column:auto}}.coverage{{grid-template-columns:1fr 1fr}}section{{padding:19px}}.model-row{{grid-template-columns:32px minmax(0,1fr) auto}}.model-share{{display:none}}footer{{display:block}}}}
  </style>
</head>
<body>
<main>
  <header>
    <div class="eyebrow">sess / usage intelligence</div>
    <h1>Agent usage,<br>made legible.</h1>
    <p class="lede">Provider-reported tokens across harnesses, routes, and models—with source grain, hierarchy, and attribution quality kept visible.</p>
    <div class="meta"><span>{range}</span><span>{filters}</span><span>{bucket} buckets · UTC</span><span>Generated {generated}</span></div>
  </header>

  <div class="hero">
    <div class="metric primary"><div class="metric-label">Reported tokens</div><div class="metric-value">{total_tokens}</div><div class="metric-note">Provider-reported total, including cache buckets where supplied</div></div>
    <div class="metric"><div class="metric-label">Transcript records</div><div class="metric-value">{conversations}</div><div class="metric-note">{logical_sessions} logical sessions · {events} usage records · {calls} API calls · {attempts} attempts</div></div>
    <div class="metric"><div class="metric-label">{cost_label}</div><div class="metric-value">{cost_value}</div><div class="metric-note">{cost_coverage:.1}% of tokens have recorded cost data</div></div>
    <div class="metric"><div class="metric-label">Organic share</div><div class="metric-value">{organic_share:.1}%</div><div class="metric-note">{synthetic_tokens} synthetic/test tokens remain visible</div></div>
  </div>

  <div class="coverage">
    <div class="coverage-item"><strong>{token_coverage:.1}%</strong><span>events with token data</span></div>
    <div class="coverage-item"><strong>{provider_coverage:.1}% / {provider_token_coverage:.1}%</strong><span>provider coverage by calls / tokens</span></div>
    <div class="coverage-item"><strong>{model_coverage:.1}% / {model_token_coverage:.1}%</strong><span>model coverage by calls / tokens</span></div>
    <div class="coverage-item"><strong>{timeline_allocated:.1}%</strong><span>tokens safely allocated to timeline buckets</span></div>
    <div class="coverage-item"><strong>{indexed_conversations}</strong><span>transcript records in indexed corpus</span></div>
  </div>

  <div class="quality-grid">
    <div class="quality-card"><strong>{provider_family_token_coverage:.1}%</strong><span>tokens with a canonical provider family</span></div>
    <div class="quality-card"><strong>{token_semantics_token_coverage:.1}%</strong><span>tokens with explicit counter semantics</span></div>
    <div class="quality-card"><strong>{reconciled_events} / {comparable_events}</strong><span>comparable reported/component totals reconcile · {mismatched_events} mismatches</span></div>
  </div>

  <section>
    <div class="section-head"><div><div class="section-kicker">Through time</div><h2>Safely allocated token trend</h2></div><div class="section-kicker">{trend_points_label}</div></div>
    {trend_svg}
    <div class="section-kicker">{unallocated_tokens} selected tokens span bucket boundaries or lack exact timing; {excluded_tokens} were outside a requested time window.</div>
  </section>

  <div class="two-up">
    <section><div class="section-head"><div><div class="section-kicker">Execution surface</div><h2>Harness split</h2></div></div>{harnesses}</section>
    <section><div class="section-head"><div><div class="section-kicker">Inference source</div><h2>Canonical provider family</h2></div></div>{providers}</section>
  </div>

  <div class="two-up">
    <section><div class="section-head"><div><div class="section-kicker">Adapter and route truth</div><h2>Raw provider IDs</h2></div></div>{raw_providers}</section>
    <section><div class="section-head"><div><div class="section-kicker">Cross-version grouping</div><h2>Canonical model family</h2></div></div>{model_families}</section>
  </div>

  <section>
    <div class="section-head"><div><div class="section-kicker">Joint concentration</div><h2>Provider × model family</h2></div><div class="section-kicker">Top {top}</div></div>
    {provider_models}
  </section>

  <section>
    <div class="section-head"><div><div class="section-kicker">Token concentration</div><h2>Model leaderboard</h2></div><div class="section-kicker">Top {top} + unknown</div></div>
    {models}
  </section>

  <div class="two-up">
    <section><div class="section-head"><div><div class="section-kicker">Measurement semantics</div><h2>Usage grain</h2></div></div>{usage_grains}</section>
    <section><div class="section-head"><div><div class="section-kicker">Corpus semantics</div><h2>Transcript kind</h2></div></div>{record_kinds}</section>
  </div>

  <div class="two-up">
    <section><div class="section-head"><div><div class="section-kicker">Attribution quality</div><h2>Provider inference confidence</h2></div></div>{provider_confidences}</section>
    <section><div class="section-head"><div><div class="section-kicker">Counter provenance</div><h2>Token semantics</h2></div></div>{token_semantics}</section>
  </div>

  <div class="two-up">
    <section><div class="section-head"><div><div class="section-kicker">Model configuration</div><h2>Variants and reasoning effort</h2></div></div>{model_variants}</section>
    <section><div class="section-head"><div><div class="section-kicker">Source operation</div><h2>Task kind</h2></div></div>{tasks}</section>
  </div>

  <section>
    <div class="section-head"><div><div class="section-kicker">Route economics</div><h2>Billing mode</h2></div></div>
    {billing_modes}
  </section>

  <div class="two-up">
    <section><div class="section-head"><div><div class="section-kicker">Billing provenance</div><h2>Cost status</h2></div></div>{cost_statuses}</section>
    <section><div class="section-head"><div><div class="section-kicker">Billing provenance</div><h2>Cost source</h2></div></div>{cost_sources}</section>
  </div>

  <section>
    <div class="section-head"><div><div class="section-kicker">Catalog provenance</div><h2>Source pricing version</h2></div><div class="section-kicker">public-list report catalog is disclosed separately</div></div>
    {pricing_versions}
  </section>

  <section>
    <div class="section-head"><div><div class="section-kicker">Source health · full corpus · unfiltered</div><h2>Raw indexed coverage by harness</h2></div><div class="section-kicker">before report deduplication; assistant-bearing records without usage stay explicit</div></div>
    {source_coverage}
  </section>

  <section>
    <div class="section-head"><div><div class="section-kicker">Reported components</div><h2>Token mix</h2></div></div>
    {token_mix}
  </section>

  <div class="caveat"><strong>Read this as measured usage, not billing truth.</strong><p>Cache buckets are separate from fresh input and reasoning is a subset of output. Aggregate rows stay in all-time totals but enter a date range only when fully contained and enter a trend bucket only when the entire interval fits. Raw provider/model values are retained; canonical families carry explicit inference provenance. {cost_note} {unknown_provider} API calls have no raw provider and {unknown_model} have no raw model. Source estimates and public-list estimates are never labeled actual.</p></div>
  <footer><span>Generated locally by sess</span><span>{indexed_messages} indexed messages · rendered offline · no remote assets</span></footer>
</main>
</body>
</html>"##,
        bucket = title_case(report.range.bucket.as_str()),
        total_tokens = format_compact(report.totals.tokens.total),
        conversations = format_count(report.totals.conversations),
        logical_sessions = format_count(report.totals.logical_sessions),
        calls = format_count(report.totals.api_calls),
        attempts = format_count(report.totals.request_attempts),
        events = format_count(report.totals.events),
        cost_coverage = report.coverage.cost_token_percent,
        model_coverage = report.coverage.model_percent,
        model_token_coverage = report.coverage.model_token_percent,
        token_coverage = report.coverage.token_event_percent,
        provider_coverage = report.coverage.provider_percent,
        provider_token_coverage = report.coverage.provider_token_percent,
        provider_family_token_coverage = report.coverage.provider_family_token_percent,
        token_semantics_token_coverage = report.coverage.token_semantics_token_percent,
        reconciled_events = format_count(report.coverage.events_with_reconciled_total),
        comparable_events = format_count(
            report
                .coverage
                .events_with_reconciled_total
                .saturating_add(report.coverage.events_with_total_mismatch),
        ),
        mismatched_events = format_count(report.coverage.events_with_total_mismatch),
        timeline_allocated = report.coverage.timeline_allocated_token_percent,
        unallocated_tokens = format_count(report.coverage.temporally_unallocated_tokens),
        excluded_tokens = format_count(report.coverage.temporally_excluded_tokens),
        organic_share = organic_share,
        synthetic_tokens = format_count(synthetic_tokens),
        indexed_conversations = format_count(report.indexed_conversations),
        indexed_messages = format_count(report.indexed_messages),
        trend_points_label = escape_html(&trend_points_label),
        cost_note = escape_html(&cost_note),
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
    let visible = visible_rows(rows, top);
    let omitted = rows.len().saturating_sub(visible.len());
    for row in visible {
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
        let class = match fill_class {
            "good" => "fill good",
            "warn" => "fill warn",
            _ => "fill",
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
    if omitted > 0 {
        write!(
            output,
            "<div class=\"section-kicker omitted\">{} additional groups omitted by --top</div>",
            format_count(omitted as u64),
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
    let visible = visible_rows(rows, top);
    let omitted = rows.len().saturating_sub(visible.len());
    for (index, row) in visible.into_iter().enumerate() {
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
    if omitted > 0 {
        write!(
            output,
            "<div class=\"section-kicker omitted\">{} additional models omitted by --top</div>",
            format_count(omitted as u64),
        )
        .unwrap();
    }
    output.push_str("</div>");
    output
}

fn render_provider_models_html(report: &UsageReport, top: usize) -> String {
    if report.by_provider_model.is_empty() {
        return "<div class=\"section-kicker\">No provider/model attribution in this selection.</div>"
            .to_string();
    }
    let mut output = String::from(
        "<div class=\"table-wrap\"><table><thead><tr><th>Provider × model</th><th>Tokens</th><th>Share</th><th>Calls</th><th>Logical sessions</th></tr></thead><tbody>",
    );
    for row in report.by_provider_model.iter().take(top) {
        write!(
            output,
            "<tr><td>{} × {}</td><td>{}</td><td>{:.1}%</td><td>{}</td><td>{}</td></tr>",
            escape_html(&row.provider_label),
            escape_html(&row.model_label),
            format_count(row.tokens.total),
            percent(row.tokens.total, report.totals.tokens.total),
            format_count(row.api_calls),
            format_count(row.logical_sessions),
        )
        .unwrap();
    }
    output.push_str("</tbody></table></div>");
    let omitted = report.by_provider_model.len().saturating_sub(top);
    if omitted > 0 {
        write!(
            output,
            "<div class=\"section-kicker omitted\">{} additional provider/model pairs omitted by --top</div>",
            format_count(omitted as u64),
        )
        .unwrap();
    }
    output
}

fn render_source_coverage_html(rows: &[SourceCoverage]) -> String {
    if rows.is_empty() {
        return "<div class=\"section-kicker\">No indexed source coverage available.</div>"
            .to_string();
    }
    let mut output = String::from(
        "<div class=\"table-wrap\"><table><thead><tr><th>Harness</th><th>Transcript records</th><th>Logical sessions</th><th>Assistant records</th><th>Usage-bearing</th><th>Assistant / no usage</th><th>API calls</th><th>Tokens</th></tr></thead><tbody>",
    );
    for row in rows {
        write!(
            output,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape_html(&row.agent),
            format_count(row.transcript_records),
            format_count(row.logical_sessions),
            format_count(row.assistant_records),
            format_count(row.usage_bearing_records),
            format_count(row.assistant_without_usage_records),
            format_count(row.api_calls),
            format_count(row.total_tokens),
        )
        .unwrap();
    }
    output.push_str("</tbody></table></div>");
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
    if !filters.variants.is_empty() {
        dimensions.push(format!("Variant {}", filters.variants.join(", ")));
    }
    if !filters.tasks.is_empty() {
        dimensions.push(format!("Task {}", filters.tasks.join(", ")));
    }
    if let Some(workspace) = filters.workspace.as_deref() {
        dimensions.push(format!("Workspace {workspace}"));
    }
    if filters.exclude_synthetic {
        dimensions.push("Organic only".to_string());
    }
    if filters.estimate_list_costs {
        dimensions.push(format!("List estimates {LIST_PRICE_CATALOG_VERSION}"));
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

/// Make source- and CLI-controlled labels safe to print to an interactive
/// terminal. C0/C1 controls cover ANSI CSI/OSC introducers, newlines, tabs,
/// carriage returns, and OSC terminators. Directional formatting characters
/// are replaced too so a label cannot visually reorder the surrounding table.
fn sanitize_terminal(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
            {
                '�'
            } else {
                character
            }
        })
        .collect()
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
            logical_session_id: Some(format!("session-{id}")),
            record_kind: "top_level".to_string(),
            is_synthetic: false,
            timestamp,
            interval_start: None,
            interval_end: None,
            usage_grain: UsageGrain::Event,
            provider: provider.map(str::to_string),
            provider_family: provider.map(|value| value.to_ascii_lowercase()),
            provider_inference_source: provider.map(|_| "raw_provider".to_string()),
            provider_inference_confidence: provider.map(|_| "high".to_string()),
            model: model.map(str::to_string),
            model_family: model.map(str::to_string),
            model_variant: None,
            task: None,
            billing_base_url: None,
            billing_mode: None,
            source_event_id: None,
            api_calls: 1,
            request_attempts: 1,
            tokens: tokens(total),
            reported_total_tokens: Some(total),
            component_total_tokens: Some(total),
            token_semantics: Some("test-v1".to_string()),
            actual_cost_usd: None,
            estimated_cost_usd: None,
            cost_status: None,
            cost_source: None,
            cost_currency: None,
            pricing_version: None,
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
            source_coverage: vec![],
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
        assert_eq!(report.source_coverage_scope, SOURCE_COVERAGE_SCOPE);
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
    fn copied_source_event_uses_earliest_timestamp_for_historical_ranges() {
        let mut original = event(
            1,
            Agent::Codex,
            Some(timestamp(2026, 7, 1)),
            Some("openai"),
            Some("gpt-5"),
            100,
        );
        original.source_event_id = Some("codex-call:shared".to_string());
        let mut replay = original.clone();
        replay.conversation_id = 2;
        replay.timestamp = Some(timestamp(2026, 8, 1));

        let report = build_report(
            &UsageDataset {
                events: vec![replay, original],
                ..UsageDataset::default()
            },
            &UsageFilters {
                since: Some(timestamp(2026, 7, 1)),
                until: Some(timestamp(2026, 7, 2)),
                ..UsageFilters::default()
            },
        );

        assert_eq!(report.totals.events, 1);
        assert_eq!(report.totals.tokens.total, 100);
        assert_eq!(report.coverage.temporally_excluded_tokens, 0);
    }

    #[test]
    fn copied_source_event_keeps_richer_payload_at_earliest_logical_time() {
        let mut original = event(
            1,
            Agent::Codex,
            Some(timestamp(2026, 7, 1)),
            Some("openai"),
            Some("gpt-5"),
            100,
        );
        original.source_event_id = Some("codex-call:richer-replay".to_string());
        let mut richer_replay = original.clone();
        richer_replay.conversation_id = 2;
        richer_replay.timestamp = Some(timestamp(2026, 8, 1));
        richer_replay.tokens = tokens(120);
        richer_replay.reported_total_tokens = Some(120);
        richer_replay.component_total_tokens = Some(120);

        let report = build_report(
            &UsageDataset {
                events: vec![richer_replay, original],
                ..UsageDataset::default()
            },
            &UsageFilters {
                since: Some(timestamp(2026, 7, 1)),
                until: Some(timestamp(2026, 7, 2)),
                bucket: UsageBucket::Day,
                ..UsageFilters::default()
            },
        );

        assert_eq!(report.totals.events, 1);
        assert_eq!(report.totals.tokens.total, 120);
        assert_eq!(report.range.observed_since, Some(timestamp(2026, 7, 1)));
        assert_eq!(
            report
                .trend
                .iter()
                .map(|point| point.tokens.total)
                .sum::<u64>(),
            120
        );
        assert_eq!(report.coverage.temporally_excluded_tokens, 0);
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
    fn source_reported_zero_is_covered_and_distinct_from_unlabelled_zero() {
        let mut labelled = event(
            1,
            Agent::PiAgent,
            None,
            Some("provider"),
            Some("model"),
            100,
        );
        labelled.estimated_cost_usd = Some(0.0);
        labelled.cost_status = Some("source_reported_zero".to_string());
        labelled.cost_currency = Some("USD".to_string());
        let mut unlabelled = event(
            2,
            Agent::PiAgent,
            None,
            Some("provider"),
            Some("model"),
            100,
        );
        unlabelled.estimated_cost_usd = Some(0.0);

        let report = build_report(
            &UsageDataset {
                events: vec![labelled, unlabelled],
                ..UsageDataset::default()
            },
            &UsageFilters::default(),
        );

        assert_eq!(report.totals.cost.estimated_usd, Some(0.0));
        assert_eq!(report.totals.cost.accounted_usd, Some(0.0));
        assert_eq!(report.totals.cost.estimated_events, 1);
        assert_eq!(report.totals.cost.covered_tokens, 100);
        assert_eq!(report.coverage.cost_token_percent, 50.0);
        let statuses = report
            .by_cost_status
            .iter()
            .map(|row| (row.key.as_str(), row.tokens.total))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(statuses.get("source_reported_zero"), Some(&100));
        assert_eq!(statuses.get("unavailable"), Some(&100));
    }

    #[test]
    fn explicit_zero_actual_cost_remains_reported_and_covered() {
        let mut row = event(
            1,
            Agent::Hermes,
            None,
            Some("anthropic"),
            Some("claude-opus-4-8"),
            100,
        );
        row.actual_cost_usd = Some(0.0);
        row.cost_status = Some("actual".to_string());
        let report = build_report(
            &UsageDataset {
                events: vec![row],
                ..UsageDataset::default()
            },
            &UsageFilters::default(),
        );
        assert_eq!(report.totals.cost.actual_usd, Some(0.0));
        assert_eq!(report.totals.cost.accounted_usd, Some(0.0));
        assert_eq!(report.coverage.cost_token_percent, 100.0);
        assert_eq!(report.by_cost_status[0].key, "reported_actual");
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
            ..UsageFilters::default()
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
            source_coverage: vec![SourceCoverage {
                agent: "codex".to_string(),
                transcript_records: 10,
                ..SourceCoverage::default()
            }],
        };
        let terminal = render_terminal(&build_report(&dataset, &UsageFilters::default()), 1);
        assert!(terminal.contains("1,234,568"));
        assert!(terminal.contains("By Harness"));
        assert!(terminal.contains("By Provider"));
        assert!(terminal.contains("Top Models"));
        assert!(terminal.contains("Full-Corpus Source Coverage (Unfiltered)"));
        assert!(terminal.contains("Unknown"));
    }

    #[test]
    fn terminal_replaces_controls_and_directional_formatting_in_dynamic_labels() {
        let unsafe_label = "provider\u{1b}]52;c;clipboard\u{7}\nspoof\u{202e}";
        let dataset = UsageDataset {
            events: vec![event(
                1,
                Agent::Codex,
                Some(timestamp(2026, 7, 1)),
                Some(unsafe_label),
                Some(unsafe_label),
                500,
            )],
            source_coverage: vec![SourceCoverage {
                agent: unsafe_label.to_string(),
                transcript_records: 1,
                ..SourceCoverage::default()
            }],
            ..UsageDataset::default()
        };
        let report = build_report(
            &dataset,
            &UsageFilters {
                providers: vec![unsafe_label.to_string()],
                ..UsageFilters::default()
            },
        );
        let terminal = render_terminal(&report, 10);

        assert!(!terminal.contains('\u{1b}'));
        assert!(!terminal.contains('\u{7}'));
        assert!(!terminal.contains('\u{202e}'));
        assert!(!terminal.contains("clipboard\nspoof"));
        assert!(terminal.contains("provider�]52;c;clipboard��spoof�"));
        assert!(
            terminal
                .chars()
                .all(|character| character == '\n' || !character.is_control())
        );
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
            source_coverage: vec![],
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
    fn html_discloses_every_top_limited_breakdown_and_keeps_unknown_visible() {
        let dataset = UsageDataset {
            events: vec![
                event(
                    1,
                    Agent::Codex,
                    Some(timestamp(2026, 7, 1)),
                    Some("provider-a"),
                    Some("model-a"),
                    400,
                ),
                event(
                    2,
                    Agent::Codex,
                    Some(timestamp(2026, 7, 2)),
                    Some("provider-b"),
                    Some("model-b"),
                    300,
                ),
                event(
                    3,
                    Agent::Codex,
                    Some(timestamp(2026, 7, 3)),
                    Some("provider-c"),
                    Some("model-c"),
                    200,
                ),
                event(
                    4,
                    Agent::Codex,
                    Some(timestamp(2026, 7, 4)),
                    None,
                    None,
                    100,
                ),
            ],
            ..UsageDataset::default()
        };

        let html = render_html(&build_report(&dataset, &UsageFilters::default()), 1);
        assert!(html.contains("2 additional groups omitted by --top"));
        assert!(html.contains("2 additional models omitted by --top"));
        assert!(html.contains("3 additional provider/model pairs omitted by --top"));
        assert!(html.contains(">Unknown<"));
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
    fn aggregate_intervals_allocate_only_when_fully_inside_one_bucket() {
        let day = timestamp(2026, 7, 1);
        let mut contained = event(1, Agent::Hermes, None, Some("xai"), Some("grok"), 100);
        contained.usage_grain = UsageGrain::IntervalAggregate;
        contained.interval_start = Some(day);
        contained.interval_end = Some(day + Duration::hours(2).num_milliseconds());
        let mut spanning = event(2, Agent::Hermes, None, Some("xai"), Some("grok"), 200);
        spanning.usage_grain = UsageGrain::IntervalAggregate;
        spanning.interval_start = Some(day);
        spanning.interval_end = Some(day + Duration::days(2).num_milliseconds());

        let dataset = UsageDataset {
            events: vec![contained, spanning],
            ..UsageDataset::default()
        };
        let report = build_report(
            &dataset,
            &UsageFilters {
                bucket: UsageBucket::Day,
                ..UsageFilters::default()
            },
        );
        assert_eq!(report.totals.tokens.total, 300);
        assert_eq!(
            report
                .trend
                .iter()
                .map(|point| point.tokens.total)
                .sum::<u64>(),
            100
        );
        assert_eq!(report.coverage.temporally_unallocated_tokens, 200);

        let filtered = build_report(
            &dataset,
            &UsageFilters {
                since: Some(day),
                until: Some(day + Duration::hours(23).num_milliseconds()),
                bucket: UsageBucket::Day,
                ..UsageFilters::default()
            },
        );
        assert_eq!(filtered.totals.tokens.total, 100);
        assert_eq!(filtered.coverage.temporally_excluded_tokens, 200);
    }

    #[test]
    fn organic_totals_and_filter_keep_synthetic_usage_explicit() {
        let organic = event(1, Agent::PiAgent, None, Some("openai"), Some("gpt"), 100);
        let mut synthetic = event(2, Agent::PiAgent, None, Some("faux"), Some("faux"), 40);
        synthetic.is_synthetic = true;
        synthetic.record_kind = "test".to_string();
        let dataset = UsageDataset {
            events: vec![organic, synthetic],
            ..UsageDataset::default()
        };

        let all = build_report(&dataset, &UsageFilters::default());
        assert_eq!(all.totals.tokens.total, 140);
        assert_eq!(all.organic_totals.tokens.total, 100);
        assert!(all.by_record_kind.iter().any(|row| row.key == "test"));

        let organic_only = build_report(
            &dataset,
            &UsageFilters {
                exclude_synthetic: true,
                ..UsageFilters::default()
            },
        );
        assert_eq!(organic_only.totals.tokens.total, 100);
    }

    #[test]
    fn token_weighted_dimension_coverage_exposes_high_volume_unknowns() {
        let known = event(1, Agent::Codex, None, Some("openai"), Some("gpt"), 1);
        let mut unknown = event(2, Agent::Codex, None, None, None, 999);
        unknown.api_calls = 1;
        let report = build_report(
            &UsageDataset {
                events: vec![known, unknown],
                ..UsageDataset::default()
            },
            &UsageFilters::default(),
        );
        assert_eq!(report.coverage.provider_percent, 50.0);
        assert_eq!(report.coverage.provider_token_percent, 0.1);
        assert_eq!(report.coverage.model_token_percent, 0.1);
    }

    #[test]
    fn public_list_estimator_is_opt_in_versioned_and_separate() {
        let mut row = event(
            1,
            Agent::Codex,
            Some(timestamp(2026, 7, 1)),
            Some("openai"),
            Some("gpt-5.6-sol"),
            3_000_000,
        );
        row.tokens = TokenCounts {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write: 0,
            reasoning: 0,
            total: 3_000_000,
        };
        let dataset = UsageDataset {
            events: vec![row],
            ..UsageDataset::default()
        };
        assert_eq!(
            build_report(&dataset, &UsageFilters::default())
                .totals
                .cost
                .list_estimated_usd,
            None
        );
        let report = build_report(
            &dataset,
            &UsageFilters {
                estimate_list_costs: true,
                ..UsageFilters::default()
            },
        );
        assert_eq!(report.totals.cost.list_estimated_usd, Some(56.0));
        assert_eq!(
            report.list_price_catalog_version.as_deref(),
            Some(LIST_PRICE_CATALOG_VERSION)
        );
        assert_eq!(report.totals.cost.accounted_usd, None);
        assert_eq!(report.by_cost_status[0].key, "public_list_estimated");
    }

    #[test]
    fn public_list_estimator_rejects_incomplete_or_indivisible_pricing_inputs() {
        let mut total_only = event(
            1,
            Agent::Codex,
            Some(timestamp(2026, 7, 1)),
            Some("openai"),
            Some("gpt-5.6-sol"),
            100,
        );
        total_only.tokens = TokenCounts {
            total: 100,
            ..TokenCounts::default()
        };
        total_only.component_total_tokens = Some(0);

        let mut aggregate = event(
            2,
            Agent::Hermes,
            None,
            Some("openai"),
            Some("gpt-5.6-sol"),
            100,
        );
        aggregate.usage_grain = UsageGrain::SessionAggregate;
        aggregate.interval_start = Some(timestamp(2026, 7, 1));
        aggregate.interval_end = Some(timestamp(2026, 7, 2));

        let report = build_report(
            &UsageDataset {
                events: vec![total_only, aggregate],
                ..UsageDataset::default()
            },
            &UsageFilters {
                estimate_list_costs: true,
                ..UsageFilters::default()
            },
        );
        assert_eq!(report.totals.cost.list_estimated_usd, None);
        assert_eq!(report.totals.cost.list_estimated_tokens, 0);
        assert!(
            report
                .by_cost_status
                .iter()
                .all(|row| row.key != "public_list_estimated")
        );
    }

    #[test]
    fn anthropic_cache_write_without_ttl_is_not_list_priced() {
        let mut row = event(
            1,
            Agent::ClaudeCode,
            Some(timestamp(2026, 7, 1)),
            Some("anthropic"),
            Some("claude-opus-4-8"),
            100,
        );
        row.tokens = TokenCounts {
            cache_write: 100,
            total: 100,
            ..TokenCounts::default()
        };
        row.component_total_tokens = Some(100);
        let report = build_report(
            &UsageDataset {
                events: vec![row],
                ..UsageDataset::default()
            },
            &UsageFilters {
                estimate_list_costs: true,
                ..UsageFilters::default()
            },
        );
        assert_eq!(report.totals.cost.list_estimated_usd, None);
        assert_eq!(report.totals.cost.list_estimated_tokens, 0);
    }

    #[test]
    fn public_list_estimator_rejects_hosted_and_subscription_routes() {
        let mut azure = event(
            1,
            Agent::PiAgent,
            Some(timestamp(2026, 7, 1)),
            Some("azure-openai"),
            Some("gpt-5.4"),
            100,
        );
        azure.provider_family = Some("openai".to_string());

        let mut codex_subscription = event(
            2,
            Agent::Codex,
            Some(timestamp(2026, 7, 1)),
            Some("openai-codex"),
            Some("gpt-5.4"),
            100,
        );
        codex_subscription.provider_family = Some("openai".to_string());
        codex_subscription.billing_base_url =
            Some("https://chatgpt.com/backend-api/codex".to_string());

        let mut bedrock = event(
            3,
            Agent::PiAgent,
            Some(timestamp(2026, 7, 1)),
            Some("bedrock-anthropic"),
            Some("claude-opus-4-8"),
            100,
        );
        bedrock.provider_family = Some("anthropic".to_string());

        let mut vertex = event(
            4,
            Agent::PiAgent,
            Some(timestamp(2026, 7, 1)),
            Some("google-vertex"),
            Some("gemini-3.1-pro-preview"),
            100,
        );
        vertex.provider_family = Some("google".to_string());

        let mut vertex_host = event(
            5,
            Agent::PiAgent,
            Some(timestamp(2026, 7, 1)),
            Some("google"),
            Some("gemini-3.1-pro-preview"),
            100,
        );
        vertex_host.provider_family = Some("google".to_string());
        vertex_host.billing_base_url =
            Some("https://us-central1-aiplatform.googleapis.com/v1".to_string());

        let mut subscription_mode = event(
            6,
            Agent::Codex,
            Some(timestamp(2026, 7, 1)),
            Some("openai"),
            Some("gpt-5.4"),
            100,
        );
        subscription_mode.billing_mode = Some("subscription_included".to_string());

        let report = build_report(
            &UsageDataset {
                events: vec![
                    azure,
                    codex_subscription,
                    bedrock,
                    vertex,
                    vertex_host,
                    subscription_mode,
                ],
                ..UsageDataset::default()
            },
            &UsageFilters {
                estimate_list_costs: true,
                ..UsageFilters::default()
            },
        );

        assert_eq!(report.totals.cost.list_estimated_usd, None);
        assert_eq!(report.totals.cost.list_estimated_events, 0);
        assert_eq!(report.totals.cost.list_estimated_tokens, 0);
    }

    #[test]
    fn explicit_first_party_api_url_can_prove_a_list_price_route() {
        let mut row = event(
            1,
            Agent::PiAgent,
            Some(timestamp(2026, 7, 1)),
            Some("azure-openai"),
            Some("gpt-5.4"),
            100,
        );
        row.provider_family = Some("openai".to_string());
        row.billing_base_url = Some("https://api.openai.com/v1".to_string());

        let report = build_report(
            &UsageDataset {
                events: vec![row],
                ..UsageDataset::default()
            },
            &UsageFilters {
                estimate_list_costs: true,
                ..UsageFilters::default()
            },
        );

        assert!(report.totals.cost.list_estimated_usd.is_some());
        assert_eq!(report.totals.cost.list_estimated_events, 1);
        assert_eq!(report.totals.cost.list_estimated_tokens, 100);
    }

    #[test]
    fn logical_sessions_are_distinct_from_transcript_records() {
        let mut parent = event(1, Agent::ClaudeCode, None, None, Some("claude"), 100);
        parent.logical_session_id = Some("shared".to_string());
        let mut child = event(2, Agent::ClaudeCode, None, None, Some("claude"), 50);
        child.logical_session_id = Some("shared".to_string());
        child.record_kind = "child_agent".to_string();
        let mut other_harness = event(3, Agent::Hermes, None, None, Some("claude"), 25);
        other_harness.logical_session_id = Some("shared".to_string());
        let report = build_report(
            &UsageDataset {
                events: vec![parent, child, other_harness],
                ..UsageDataset::default()
            },
            &UsageFilters::default(),
        );
        assert_eq!(report.totals.conversations, 3);
        assert_eq!(report.totals.logical_sessions, 2);
        assert_eq!(
            report
                .by_harness
                .iter()
                .find(|row| row.key == Agent::ClaudeCode.slug())
                .unwrap()
                .logical_sessions,
            1
        );
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
