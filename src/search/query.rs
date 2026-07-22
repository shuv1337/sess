use std::ops::Bound;
use std::time::Instant;

use anyhow::Result;
use tantivy::{
    Order, Term,
    collector::{Count, TopDocs},
    query::{AllQuery, BooleanQuery, Occur, PhraseQuery, Query, RangeQuery, TermQuery},
    schema::{IndexRecordOption, Value as TantivyValue},
};

use crate::model::Agent;
use crate::search::index::TantivyIndex;

/// Ranking mode for search results
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RankingMode {
    #[default]
    RecentHeavy, // BM25 * 0.3 + recency * 0.7
    Balanced,  // BM25 * 0.5 + recency * 0.5
    Relevance, // BM25 * 0.8 + recency * 0.2
    Newest,    // Pure date sort descending
    Oldest,    // Pure date sort ascending
}

impl RankingMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            RankingMode::RecentHeavy => "recent",
            RankingMode::Balanced => "balanced",
            RankingMode::Relevance => "relevance",
            RankingMode::Newest => "newest",
            RankingMode::Oldest => "oldest",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "recent" => Some(RankingMode::RecentHeavy),
            "balanced" => Some(RankingMode::Balanced),
            "relevance" => Some(RankingMode::Relevance),
            "newest" => Some(RankingMode::Newest),
            "oldest" => Some(RankingMode::Oldest),
            _ => None,
        }
    }

    pub fn blend_scores(&self, bm25: f32, recency: f32) -> f32 {
        match self {
            RankingMode::RecentHeavy => bm25 * 0.3 + recency * 0.7,
            RankingMode::Balanced => bm25 * 0.5 + recency * 0.5,
            RankingMode::Relevance => bm25 * 0.8 + recency * 0.2,
            RankingMode::Newest => recency,
            RankingMode::Oldest => -recency,
        }
    }
}

/// Search query parameters
#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub text: String,
    pub agent_filter: Option<Agent>,
    pub workspace_filter: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub limit: usize,
    pub offset: usize,
    pub ranking: RankingMode,
    pub rrf_k: u32,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            agent_filter: None,
            workspace_filter: None,
            since: None,
            until: None,
            limit: 20,
            offset: 0,
            ranking: RankingMode::default(),
            rrf_k: 60,
        }
    }
}

/// Single search result
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub conversation_id: i64,
    pub agent: Agent,
    pub title: Option<String>,
    pub workspace: Option<String>,
    pub source_path: String,
    pub preview: String,
    pub created_at: Option<i64>,
    pub score: f32,
    pub snippet: Option<String>,
}

/// Search results
#[derive(Debug, Clone)]
pub struct SearchResults {
    pub hits: Vec<SearchResult>,
    pub total_hits: usize,
    pub query_time_ms: u64,
}

/// Execute a search query
pub fn execute(query: &SearchQuery, index: &TantivyIndex) -> Result<SearchResults> {
    let start = Instant::now();
    let searcher = index.reader().searcher();

    // Build the query
    let schema = index.schema();
    let field_agent = schema.get_field("agent")?;
    let field_workspace = schema.get_field("workspace")?;
    let field_title = schema.get_field("title")?;
    let field_content = schema.get_field("content")?;
    let field_preview = schema.get_field("preview")?;
    let field_created_at = schema.get_field("created_at")?;
    let field_conv_db_id = schema.get_field("conv_db_id")?;
    let field_source_path = schema.get_field("source_path")?;

    let mut subqueries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();

    // Text query (if not empty)
    if !query.text.trim().is_empty() {
        let text_query = parse_text_query(&query.text, vec![field_title, field_content])?;
        subqueries.push((Occur::Must, text_query));
    }

    // Agent filter
    if let Some(agent) = query.agent_filter {
        let term = Term::from_field_text(field_agent, agent.slug());
        let term_query = TermQuery::new(term, IndexRecordOption::Basic);
        subqueries.push((Occur::Must, Box::new(term_query)));
    }

    // Workspace filter
    if let Some(ref workspace) = query.workspace_filter {
        let term = Term::from_field_text(field_workspace, workspace);
        let term_query = TermQuery::new(term, IndexRecordOption::Basic);
        subqueries.push((Occur::Must, Box::new(term_query)));
    }

    // Time range filter
    if query.since.is_some() || query.until.is_some() {
        let lower = query.since.map(Bound::Included).unwrap_or(Bound::Unbounded);
        let upper = query.until.map(Bound::Included).unwrap_or(Bound::Unbounded);
        let range_query = RangeQuery::new_i64_bounds("created_at".to_string(), lower, upper);
        subqueries.push((Occur::Must, Box::new(range_query)));
    }

    let final_query: Box<dyn tantivy::query::Query> = if subqueries.is_empty() {
        // Match all
        Box::new(AllQuery)
    } else if subqueries.len() == 1 {
        subqueries.into_iter().next().unwrap().1
    } else {
        Box::new(BooleanQuery::new(subqueries))
    };

    // Execute search
    let total_hits = searcher.search(&final_query, &Count)?;

    // When the user has no free-text query, BM25 scores carry no useful signal
    // (AllQuery / pure filter queries return uniform scores), so TopDocs picks
    // an arbitrary subset by internal docid. For "browse" cases (empty query,
    // or Newest/Oldest ranking) sort at the collector level using the
    // `created_at` fast field so we get the globally newest/oldest docs, not
    // the newest within an arbitrary 50-doc window.
    let no_text_query = query.text.trim().is_empty();
    let sort_by_date = matches!(query.ranking, RankingMode::Newest | RankingMode::Oldest)
        || (no_text_query
            && matches!(
                query.ranking,
                RankingMode::RecentHeavy | RankingMode::Balanced
            ));
    let date_order = if matches!(query.ranking, RankingMode::Oldest) {
        Order::Asc
    } else {
        Order::Desc
    };

    let mut date_sorted_docs: Vec<(i64, tantivy::DocAddress)> = Vec::new();
    let mut score_sorted_docs: Vec<(f32, tantivy::DocAddress)> = Vec::new();

    if sort_by_date {
        let collector = TopDocs::with_limit(query.limit + query.offset)
            .order_by_fast_field::<i64>("created_at", date_order.clone());
        date_sorted_docs = searcher.search(&final_query, &collector)?;
    } else {
        let collector = TopDocs::with_limit(query.limit + query.offset);
        score_sorted_docs = searcher.search(&final_query, &collector)?;
    }

    let mut hits = Vec::new();

    // Unified iteration over the chosen collector's output.
    let doc_iter: Box<dyn Iterator<Item = (f32, tantivy::DocAddress)>> = if sort_by_date {
        Box::new(
            date_sorted_docs
                .into_iter()
                .map(|(_ts, addr)| (0.0_f32, addr)),
        )
    } else {
        Box::new(score_sorted_docs.into_iter())
    };

    for (score, doc_address) in doc_iter.skip(query.offset) {
        let doc: tantivy::TantivyDocument = searcher.doc(doc_address)?;

        let conv_id: i64 = doc
            .get_first(field_conv_db_id)
            .and_then(|v| v.as_u64())
            .map(|v| v as i64)
            .unwrap_or(0);

        let agent_slug: String = doc
            .get_first(field_agent)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let agent = match agent_slug.as_str() {
            "claude_code" => Agent::ClaudeCode,
            "codex" => Agent::Codex,
            "hermes" => Agent::Hermes,
            "opencode" => Agent::OpenCode,
            "pi_agent" => Agent::PiAgent,
            _ => Agent::ClaudeCode,
        };

        let title = doc
            .get_first(field_title)
            .and_then(|v| v.as_str().map(|s| s.to_string()));

        let workspace = doc
            .get_first(field_workspace)
            .and_then(|v| v.as_str().map(|s| s.to_string()));

        let source_path = doc
            .get_first(field_source_path)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        let preview = doc
            .get_first(field_preview)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        let created_at = doc.get_first(field_created_at).and_then(|v| v.as_i64());

        // Calculate blended score
        let bm25_score = score;
        let recency = calculate_recency_score(created_at.unwrap_or(0));
        let blended_score = query.ranking.blend_scores(bm25_score, recency);

        // Generate snippet
        let snippet = generate_snippet(&preview, &query.text);

        hits.push(SearchResult {
            conversation_id: conv_id,
            agent,
            title,
            workspace,
            source_path,
            preview,
            created_at,
            score: blended_score,
            snippet,
        });
    }

    // Re-sort post-hoc. If we already collected in date order, this is a no-op
    // beyond confirming order. For score-based modes, sort by the blended score.
    if sort_by_date {
        if matches!(date_order, Order::Desc) {
            hits.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        } else {
            hits.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        }
    } else {
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    }

    let query_time_ms = start.elapsed().as_millis() as u64;

    Ok(SearchResults {
        hits,
        total_hits,
        query_time_ms,
    })
}

fn parse_text_query(
    text: &str,
    fields: Vec<tantivy::schema::Field>,
) -> Result<Box<dyn tantivy::query::Query>> {
    // Simple parsing for now - treat as terms
    let terms: Vec<&str> = text.split_whitespace().collect();

    if terms.is_empty() {
        return Ok(Box::new(AllQuery));
    }

    if terms.len() == 1 && fields.len() == 1 {
        // Single term, single field
        return Ok(lexical_term_query(fields[0], terms[0]));
    }

    // Multi-field OR query
    let mut field_queries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();

    for field in fields {
        let mut term_queries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
        for term_str in &terms {
            term_queries.push((Occur::Should, lexical_term_query(field, term_str)));
        }
        field_queries.push((Occur::Should, Box::new(BooleanQuery::new(term_queries))));
    }

    Ok(Box::new(BooleanQuery::new(field_queries)))
}

/// Build a literal term query, with one narrow compatibility path for
/// underscore-delimited identifiers. Tantivy's default text tokenizer indexes
/// `t_mrmy2fpm3jzp` as adjacent `t` and `mrmy2fpm3jzp` tokens, while a raw
/// `TermQuery` looks for the unsplit spelling and cannot match it.
///
/// Keep the literal query and OR it with an adjacent-token phrase over the
/// underscore components. This is deliberately separator-equivalent rather
/// than delimiter-exact (`t-foo` can match a query for `t_foo`), but requiring
/// adjacency avoids a broad match for components that occur elsewhere.
fn lexical_term_query(field: tantivy::schema::Field, text: &str) -> Box<dyn Query> {
    let literal_query: Box<dyn Query> = Box::new(TermQuery::new(
        Term::from_field_text(field, text),
        IndexRecordOption::WithFreqsAndPositions,
    ));
    let components: Vec<&str> = text
        .split('_')
        .filter(|component| !component.is_empty())
        .collect();

    if components.len() < 2
        || !components
            .iter()
            .all(|component| component.chars().all(|ch| ch.is_alphanumeric()))
    {
        return literal_query;
    }

    let phrase_terms = components
        .into_iter()
        .map(|component| Term::from_field_text(field, &component.to_lowercase()))
        .collect();
    let phrase_query: Box<dyn Query> = Box::new(PhraseQuery::new(phrase_terms));

    Box::new(BooleanQuery::new(vec![
        (Occur::Should, literal_query),
        (Occur::Should, phrase_query),
    ]))
}

fn calculate_recency_score(timestamp: i64) -> f32 {
    // Normalize recency to 0-1 range
    // Assume timestamps are in milliseconds
    let now = chrono::Utc::now().timestamp_millis();
    let age_days = (now - timestamp) / (1000 * 60 * 60 * 24);

    if age_days <= 0 {
        1.0
    } else if age_days > 365 {
        0.0
    } else {
        1.0 - (age_days as f32 / 365.0)
    }
}

fn generate_snippet(preview: &str, query: &str) -> Option<String> {
    if query.is_empty() {
        return None;
    }

    let terms: Vec<&str> = query.split_whitespace().collect();
    let preview_lower = preview.to_lowercase();

    for term in &terms {
        if let Some(pos) = preview_lower.find(&term.to_lowercase()) {
            let start = pos.saturating_sub(50);
            let end = (pos + term.len() + 100).min(preview.len());
            let snippet = &preview[start..end];

            // Highlight the term
            let highlighted = snippet.replace(
                &term.to_lowercase(),
                &format!(
                    "<mark>{}</mark>",
                    &snippet[pos - start..pos - start + term.len()]
                ),
            );

            return Some(highlighted);
        }
    }

    // Return first part of preview if no match found
    if preview.len() > 150 {
        Some(format!("{}...", &preview[..150]))
    } else {
        Some(preview.to_string())
    }
}

/// Perform RRF fusion of keyword and semantic results
pub fn rrf_fusion(
    keyword_results: &[SearchResult],
    semantic_results: &[(i64, f32)], // (conv_id, similarity_score)
    k: u32,
    limit: usize,
) -> Vec<SearchResult> {
    use std::collections::HashMap;

    let mut scores: HashMap<i64, f32> = HashMap::new();
    let mut result_map: HashMap<i64, SearchResult> = HashMap::new();

    // Add keyword scores
    for (rank, result) in keyword_results.iter().enumerate() {
        let rrf_score = 1.0 / (k as f32 + (rank + 1) as f32);
        *scores.entry(result.conversation_id).or_insert(0.0) += rrf_score;
        result_map.insert(result.conversation_id, result.clone());
    }

    // Add semantic scores
    for (rank, (conv_id, _sim)) in semantic_results.iter().enumerate() {
        let rrf_score = 1.0 / (k as f32 + (rank + 1) as f32);
        *scores.entry(*conv_id).or_insert(0.0) += rrf_score;
    }

    // Sort by RRF score
    let mut sorted: Vec<(i64, f32)> = scores.into_iter().collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    // Build results
    sorted
        .into_iter()
        .take(limit)
        .filter_map(|(conv_id, score)| {
            result_map.get_mut(&conv_id).map(|result| {
                result.score = score;
                result.clone()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_search_result(id: i64, agent: Agent, title: &str, score: f32) -> SearchResult {
        SearchResult {
            conversation_id: id,
            agent,
            title: Some(title.to_string()),
            workspace: Some("/test".to_string()),
            source_path: format!("/test/{}.jsonl", id),
            preview: format!("Preview for {}", title),
            created_at: Some(1705312800000 + id * 1000),
            score,
            snippet: None,
        }
    }

    // ── RankingMode ────────────────────────────────────────

    #[test]
    fn test_ranking_mode_as_str() {
        assert_eq!(RankingMode::RecentHeavy.as_str(), "recent");
        assert_eq!(RankingMode::Balanced.as_str(), "balanced");
        assert_eq!(RankingMode::Relevance.as_str(), "relevance");
        assert_eq!(RankingMode::Newest.as_str(), "newest");
        assert_eq!(RankingMode::Oldest.as_str(), "oldest");
    }

    #[test]
    fn test_ranking_mode_from_str() {
        assert_eq!(
            RankingMode::from_str("recent"),
            Some(RankingMode::RecentHeavy)
        );
        assert_eq!(
            RankingMode::from_str("balanced"),
            Some(RankingMode::Balanced)
        );
        assert_eq!(
            RankingMode::from_str("relevance"),
            Some(RankingMode::Relevance)
        );
        assert_eq!(RankingMode::from_str("newest"), Some(RankingMode::Newest));
        assert_eq!(RankingMode::from_str("oldest"), Some(RankingMode::Oldest));
        assert_eq!(RankingMode::from_str("NEWEST"), Some(RankingMode::Newest));
        assert_eq!(RankingMode::from_str("unknown"), None);
    }

    #[test]
    fn test_ranking_mode_blend_scores() {
        let bm25 = 1.0;
        let recency = 0.5;

        // RecentHeavy: 0.3 * 1.0 + 0.7 * 0.5 = 0.65
        let score = RankingMode::RecentHeavy.blend_scores(bm25, recency);
        assert!((score - 0.65).abs() < 0.001);

        // Balanced: 0.5 * 1.0 + 0.5 * 0.5 = 0.75
        let score = RankingMode::Balanced.blend_scores(bm25, recency);
        assert!((score - 0.75).abs() < 0.001);

        // Relevance: 0.8 * 1.0 + 0.2 * 0.5 = 0.9
        let score = RankingMode::Relevance.blend_scores(bm25, recency);
        assert!((score - 0.9).abs() < 0.001);

        // Newest: pure recency
        let score = RankingMode::Newest.blend_scores(bm25, recency);
        assert!((score - 0.5).abs() < 0.001);

        // Oldest: negated recency
        let score = RankingMode::Oldest.blend_scores(bm25, recency);
        assert!((score - (-0.5)).abs() < 0.001);
    }

    // ── SearchQuery defaults ──────────────────────────────

    #[test]
    fn test_search_query_defaults() {
        let q = SearchQuery::default();
        assert_eq!(q.text, "");
        assert!(q.agent_filter.is_none());
        assert!(q.workspace_filter.is_none());
        assert!(q.since.is_none());
        assert!(q.until.is_none());
        assert_eq!(q.limit, 20);
        assert_eq!(q.offset, 0);
        assert_eq!(q.ranking, RankingMode::RecentHeavy);
        assert_eq!(q.rrf_k, 60);
    }

    // ── calculate_recency_score ───────────────────────────

    #[test]
    fn test_recency_score_now() {
        let now = chrono::Utc::now().timestamp_millis();
        let score = calculate_recency_score(now);
        assert!(score > 0.99); // Just now → ~1.0
    }

    #[test]
    fn test_recency_score_old() {
        let old = chrono::Utc::now().timestamp_millis() - (400 * 24 * 60 * 60 * 1000); // 400 days ago
        let score = calculate_recency_score(old);
        assert!(score <= 0.0); // Over 365 days → 0
    }

    #[test]
    fn test_recency_score_half_year() {
        let half_year = chrono::Utc::now().timestamp_millis() - (182 * 24 * 60 * 60 * 1000);
        let score = calculate_recency_score(half_year);
        assert!(score > 0.4 && score < 0.6); // ~0.5
    }

    // ── generate_snippet ──────────────────────────────────

    #[test]
    fn test_generate_snippet_match() {
        let preview = "This is a test of the authentication middleware for our app.";
        let snippet = generate_snippet(preview, "authentication").unwrap();
        assert!(snippet.contains("<mark>"));
    }

    #[test]
    fn test_generate_snippet_no_match() {
        let preview = "This is a test preview that is quite long and contains many words to demonstrate truncation behavior.";
        let snippet = generate_snippet(preview, "zzzznonexistent");
        assert!(snippet.is_some()); // Falls back to first part of preview
    }

    #[test]
    fn test_generate_snippet_empty_query() {
        let snippet = generate_snippet("some preview", "");
        assert!(snippet.is_none());
    }

    #[test]
    fn test_generate_snippet_short_preview() {
        let snippet = generate_snippet("short", "short").unwrap();
        assert!(snippet.contains("<mark>"));
    }

    // ── rrf_fusion ────────────────────────────────────────

    #[test]
    fn test_rrf_fusion_keyword_only() {
        let keyword = vec![
            make_search_result(1, Agent::ClaudeCode, "First", 1.0),
            make_search_result(2, Agent::Codex, "Second", 0.8),
        ];

        let fused = rrf_fusion(&keyword, &[], 60, 10);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].conversation_id, 1); // Rank 1
        assert_eq!(fused[1].conversation_id, 2); // Rank 2
    }

    #[test]
    fn test_rrf_fusion_overlapping() {
        let keyword = vec![
            make_search_result(1, Agent::ClaudeCode, "Shared", 1.0),
            make_search_result(2, Agent::Codex, "Keyword only", 0.8),
        ];
        let semantic = vec![
            (1, 0.95), // Also found semantically
            (3, 0.90), // Semantic only (won't appear since not in result_map)
        ];

        let fused = rrf_fusion(&keyword, &semantic, 60, 10);

        // Conv 1 should be boosted (appears in both)
        let conv1 = fused.iter().find(|r| r.conversation_id == 1).unwrap();
        let conv2 = fused.iter().find(|r| r.conversation_id == 2).unwrap();
        assert!(conv1.score > conv2.score);
    }

    #[test]
    fn test_rrf_fusion_limit() {
        let keyword: Vec<SearchResult> = (0..20)
            .map(|i| {
                make_search_result(
                    i,
                    Agent::ClaudeCode,
                    &format!("Result {}", i),
                    1.0 - i as f32 * 0.01,
                )
            })
            .collect();

        let fused = rrf_fusion(&keyword, &[], 60, 5);
        assert_eq!(fused.len(), 5);
    }

    #[test]
    fn test_rrf_fusion_empty() {
        let fused = rrf_fusion(&[], &[], 60, 10);
        assert!(fused.is_empty());
    }

    #[test]
    fn test_rrf_fusion_k_parameter() {
        let keyword = vec![make_search_result(1, Agent::ClaudeCode, "First", 1.0)];

        // Different k values should produce different scores
        let fused_k60 = rrf_fusion(&keyword, &[], 60, 10);
        let fused_k10 = rrf_fusion(&keyword, &[], 10, 10);

        assert!(fused_k10[0].score > fused_k60[0].score); // Lower k → higher individual scores
    }

    // ── execute (integration with Tantivy) ────────────────

    #[test]
    fn test_execute_empty_index() {
        let temp_dir = TempDir::new().unwrap();
        let index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();

        let query = SearchQuery {
            text: "test query".to_string(),
            ..Default::default()
        };

        let results = execute(&query, &index).unwrap();
        assert_eq!(results.total_hits, 0);
        assert!(results.hits.is_empty());
    }

    #[test]
    fn test_execute_find_document() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        index.start_writer().unwrap();

        let conv = crate::model::Conversation {
            agent: Agent::ClaudeCode,
            external_id: Some("s1".to_string()),
            title: Some("Authentication middleware".to_string()),
            workspace: Some(PathBuf::from("/home/user/project")),
            source_path: PathBuf::from("/test/session.jsonl"),
            source_files: vec![],
            source_fingerprint: "fp".to_string(),
            started_at: Some(1705312800000),
            ended_at: Some(1705312900000),
            messages: vec![crate::model::Message {
                idx: 0,
                role: crate::model::Role::User,
                content: "Help me build authentication middleware".to_string(),
                timestamp: Some(1705312800000),
                model: None,
            }],
            usage: vec![],
            metadata: Default::default(),
        };

        index.add_conversation(&conv, 1).unwrap();
        index.commit().unwrap();

        // Search should find it
        let query = SearchQuery {
            text: "authentication".to_string(),
            ..Default::default()
        };
        let results = execute(&query, &index).unwrap();
        assert!(results.total_hits >= 1);
        assert_eq!(results.hits[0].conversation_id, 1);
        assert_eq!(results.hits[0].agent, Agent::ClaudeCode);
    }

    #[test]
    fn test_execute_matches_underscore_identifier_as_adjacent_tokens() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        index.start_writer().unwrap();

        let make_conversation = |title: &str, source: &str| crate::model::Conversation {
            agent: Agent::PiAgent,
            external_id: None,
            title: Some(title.to_string()),
            workspace: None,
            source_path: PathBuf::from(source),
            source_files: vec![],
            source_fingerprint: source.to_string(),
            started_at: Some(1705312800000),
            ended_at: None,
            messages: vec![],
            usage: vec![],
            metadata: Default::default(),
        };

        index
            .add_conversation(
                &make_conversation(
                    "Crew task t_mrmy2fpm3jzp finalization",
                    "/test/matching.jsonl",
                ),
                1,
            )
            .unwrap();
        index
            .add_conversation(
                &make_conversation(
                    "Crew task t-mrmy2fpm3jzp used another separator",
                    "/test/separator-equivalent.jsonl",
                ),
                2,
            )
            .unwrap();
        index
            .add_conversation(
                &make_conversation(
                    "The t stage mentions mrmy2fpm3jzp elsewhere",
                    "/test/non-adjacent.jsonl",
                ),
                3,
            )
            .unwrap();
        index
            .add_conversation(
                &make_conversation("Task mrmy2fpm3jzp", "/test/suffix-only.jsonl"),
                4,
            )
            .unwrap();
        index.commit().unwrap();

        let typed_identifier = SearchQuery {
            text: "t_mrmy2fpm3jzp".to_string(),
            ..Default::default()
        };
        let results = execute(&typed_identifier, &index).unwrap();

        assert_eq!(results.total_hits, 2);
        let matched_ids: HashSet<i64> = results
            .hits
            .iter()
            .map(|result| result.conversation_id)
            .collect();
        assert_eq!(matched_ids, HashSet::from([1, 2]));

        // Preserve the existing ability to search the distinctive component.
        let suffix = SearchQuery {
            text: "mrmy2fpm3jzp".to_string(),
            ..Default::default()
        };
        assert_eq!(execute(&suffix, &index).unwrap().total_hits, 4);
    }

    #[test]
    fn test_execute_agent_filter() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        index.start_writer().unwrap();

        // Add Claude conversation
        let conv1 = crate::model::Conversation {
            agent: Agent::ClaudeCode,
            external_id: None,
            title: Some("Claude auth".to_string()),
            workspace: None,
            source_path: PathBuf::from("/test/claude.jsonl"),
            source_files: vec![],
            source_fingerprint: "fp1".to_string(),
            started_at: Some(1705312800000),
            ended_at: None,
            messages: vec![crate::model::Message {
                idx: 0,
                role: crate::model::Role::User,
                content: "auth middleware help".to_string(),
                timestamp: None,
                model: None,
            }],
            usage: vec![],
            metadata: Default::default(),
        };
        index.add_conversation(&conv1, 1).unwrap();

        // Add Codex conversation
        let conv2 = crate::model::Conversation {
            agent: Agent::Codex,
            external_id: None,
            title: Some("Codex auth".to_string()),
            workspace: None,
            source_path: PathBuf::from("/test/codex.jsonl"),
            source_files: vec![],
            source_fingerprint: "fp2".to_string(),
            started_at: Some(1705312800000),
            ended_at: None,
            messages: vec![crate::model::Message {
                idx: 0,
                role: crate::model::Role::User,
                content: "auth middleware help".to_string(),
                timestamp: None,
                model: None,
            }],
            usage: vec![],
            metadata: Default::default(),
        };
        index.add_conversation(&conv2, 2).unwrap();
        index.commit().unwrap();

        // Filter by agent
        let query = SearchQuery {
            text: "auth".to_string(),
            agent_filter: Some(Agent::ClaudeCode),
            ..Default::default()
        };
        let results = execute(&query, &index).unwrap();
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].agent, Agent::ClaudeCode);
    }

    #[test]
    fn test_execute_all_query() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        index.start_writer().unwrap();

        let conv = crate::model::Conversation {
            agent: Agent::OpenCode,
            external_id: None,
            title: Some("Some title".to_string()),
            workspace: None,
            source_path: PathBuf::from("/test/oc.jsonl"),
            source_files: vec![],
            source_fingerprint: "fp".to_string(),
            started_at: Some(1705312800000),
            ended_at: None,
            messages: vec![crate::model::Message {
                idx: 0,
                role: crate::model::Role::User,
                content: "test content".to_string(),
                timestamp: None,
                model: None,
            }],
            usage: vec![],
            metadata: Default::default(),
        };
        index.add_conversation(&conv, 1).unwrap();
        index.commit().unwrap();

        // Empty text → match all
        let query = SearchQuery::default();
        let results = execute(&query, &index).unwrap();
        assert_eq!(results.total_hits, 1);
    }

    #[test]
    fn test_execute_since_filter() {
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        index.start_writer().unwrap();

        // Old conversation
        let conv = crate::model::Conversation {
            agent: Agent::ClaudeCode,
            external_id: None,
            title: Some("Old conv".to_string()),
            workspace: None,
            source_path: PathBuf::from("/test/old.jsonl"),
            source_files: vec![],
            source_fingerprint: "fp".to_string(),
            started_at: Some(1000), // Very old
            ended_at: None,
            messages: vec![crate::model::Message {
                idx: 0,
                role: crate::model::Role::User,
                content: "old conversation".to_string(),
                timestamp: None,
                model: None,
            }],
            usage: vec![],
            metadata: Default::default(),
        };
        index.add_conversation(&conv, 1).unwrap();
        index.commit().unwrap();

        // Search with since filter that should exclude it
        let query = SearchQuery {
            text: "conversation".to_string(),
            since: Some(1705000000000), // Recent
            ..Default::default()
        };
        let results = execute(&query, &index).unwrap();
        assert!(results.hits.is_empty());
        assert_eq!(results.total_hits, 0);
    }

    #[test]
    fn test_execute_empty_query_returns_newest_first() {
        // Regression: default "browse" view (no text query) used to return an
        // arbitrary 50-doc window because TopDocs ranked by uniform BM25 score.
        // It must now sort by `created_at` descending globally.
        let temp_dir = TempDir::new().unwrap();
        let mut index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();
        index.start_writer().unwrap();

        // Insert 100 conversations with monotonically increasing timestamps.
        // The newest should be db_id = 100.
        for i in 1..=100i64 {
            let conv = crate::model::Conversation {
                agent: Agent::ClaudeCode,
                external_id: None,
                title: Some(format!("Conv {}", i)),
                workspace: None,
                source_path: PathBuf::from(format!("/test/c{}.jsonl", i)),
                source_files: vec![],
                source_fingerprint: format!("fp{}", i),
                started_at: Some(1_700_000_000_000 + i * 1000),
                ended_at: None,
                messages: vec![crate::model::Message {
                    idx: 0,
                    role: crate::model::Role::User,
                    content: format!("body {}", i),
                    timestamp: None,
                    model: None,
                }],
                usage: vec![],
                metadata: Default::default(),
            };
            index.add_conversation(&conv, i).unwrap();
        }
        index.commit().unwrap();

        // Default ranking is RecentHeavy; empty text query should still
        // return the newest docs globally, not an arbitrary 50.
        let query = SearchQuery {
            limit: 5,
            ..Default::default()
        };
        let results = execute(&query, &index).unwrap();
        assert_eq!(results.total_hits, 100);
        assert_eq!(results.hits.len(), 5);
        let ids: Vec<i64> = results.hits.iter().map(|h| h.conversation_id).collect();
        assert_eq!(ids, vec![100, 99, 98, 97, 96]);
    }

    #[test]
    fn test_execute_timing() {
        let temp_dir = TempDir::new().unwrap();
        let index = TantivyIndex::open_or_create(temp_dir.path()).unwrap();

        let query = SearchQuery {
            text: "test".to_string(),
            ..Default::default()
        };
        let results = execute(&query, &index).unwrap();
        assert!(results.query_time_ms < 5000); // Should be fast
    }
}
