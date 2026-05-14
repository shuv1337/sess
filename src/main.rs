use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;

mod cli;
mod connectors;
mod indexer;
mod model;
mod search;
mod storage;
mod tui;

use cli::{AgentInfoOutput, Cli, Commands, DEFAULT_MAX_AGE, SearchOutput, StatsOutput};
use indexer::Indexer;

fn effective_max_age(flags: &GlobalFlags) -> std::time::Duration {
    flags.max_age.unwrap_or(DEFAULT_MAX_AGE)
}

/// Run an auto-refresh if the index is stale.
///
/// `--no-auto-index` suppresses both initial and freshness refresh.
/// `--no-refresh` suppresses only age-based refresh; initial (empty DB) still runs.
fn maybe_auto_refresh(flags: &GlobalFlags, indexer: &mut Indexer) -> Result<()> {
    if flags.no_auto_index {
        return Ok(());
    }
    if indexer.needs_initial_index()? {
        eprintln!("No existing index found. Running initial index...");
        indexer.full_index()?;
        return Ok(());
    }
    if flags.no_refresh {
        return Ok(());
    }
    let max_age = effective_max_age(flags);
    if indexer.should_refresh(max_age)? {
        eprintln!(
            "Index is stale (>{:?}); running incremental refresh...",
            max_age
        );
        indexer.incremental_index()?;
    }
    Ok(())
}

fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter("sess=info,warn")
        .init();

    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

/// Subset of CLI flags shared across subcommand arms after destructuring.
struct GlobalFlags {
    no_auto_index: bool,
    no_refresh: bool,
    no_semantic: bool,
    max_age: Option<std::time::Duration>,
}

impl GlobalFlags {
    fn from_cli(cli: &Cli) -> Self {
        Self {
            no_auto_index: cli.no_auto_index,
            no_refresh: cli.no_refresh,
            no_semantic: cli.no_semantic,
            max_age: cli.max_age,
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let flags = GlobalFlags::from_cli(&cli);

    // Determine data directory
    let data_dir = cli
        .data_dir
        .clone()
        .or_else(|| dirs::data_local_dir().map(|d| d.join("sess")))
        .context("Could not determine data directory. Use --data-dir or set SESS_DATA_DIR.")?;

    std::fs::create_dir_all(&data_dir)?;

    match cli.command {
        None => {
            // Default: launch TUI
            let mut indexer = Indexer::new(&data_dir, !flags.no_semantic)?;

            maybe_auto_refresh(&flags, &mut indexer)?;

            // Get references for TUI
            let tantivy = Arc::new(indexer.tantivy().clone());
            let refresh_cfg = tui::RefreshConfig {
                data_dir: data_dir.clone(),
                enable_semantic: !flags.no_semantic,
                max_age: effective_max_age(&flags),
                interval: std::time::Duration::from_secs(5 * 60),
                enabled: !flags.no_auto_index && !flags.no_refresh,
            };
            let storage = indexer.storage();

            // Run TUI
            tui::run_app(storage, &tantivy, refresh_cfg)?;
        }
        Some(Commands::Search {
            query,
            agent,
            workspace,
            since,
            until,
            limit,
            offset,
            ranking,
            semantic,
            rrf_k,
            json,
        }) => {
            let mut indexer = Indexer::new(&data_dir, !flags.no_semantic && semantic)?;

            maybe_auto_refresh(&flags, &mut indexer)?;

            // Build search query
            let mut search_query = search::SearchQuery {
                text: query.clone(),
                agent_filter: agent.as_ref().and_then(|a| a.parse().ok()),
                workspace_filter: workspace,
                since: since.and_then(|s| cli::parse_date(&s).ok()),
                until: until.and_then(|u| cli::parse_date(&u).ok()),
                limit,
                offset,
                ranking: ranking.into(),
                rrf_k,
            };

            // Execute keyword search
            let mut keyword_results = search::query::execute(&search_query, indexer.tantivy())?;

            // Execute semantic search if enabled
            if semantic && indexer.semantic().is_some() {
                if let Some(semantic_idx) = indexer.semantic() {
                    // Get all embeddings from storage
                    let embeddings = indexer.storage().get_all_embeddings()?;

                    if !embeddings.is_empty() {
                        let semantic_results = semantic_idx.search(&query, &embeddings, 50)?;

                        // Merge using RRF
                        keyword_results.hits = search::query::rrf_fusion(
                            &keyword_results.hits,
                            &semantic_results,
                            rrf_k,
                            limit,
                        );
                    }
                }
            }

            if json {
                let output = SearchOutput::from_results(keyword_results, &query);
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                // Human-friendly output
                println!("Query: {}", query);
                println!(
                    "Found {} results in {}ms",
                    keyword_results.total_hits, keyword_results.query_time_ms
                );
                println!();

                for (i, hit) in keyword_results.hits.iter().enumerate() {
                    let title = hit.title.clone().unwrap_or_else(|| "Untitled".to_string());
                    let date = hit
                        .created_at
                        .map(|ts| {
                            chrono::DateTime::from_timestamp_millis(ts)
                                .map(|dt| dt.format("%Y-%m-%d").to_string())
                                .unwrap_or_default()
                        })
                        .unwrap_or_default();

                    println!("{}. [{}] {} - {}", i + 1, hit.agent.icon(), title, date);
                    println!("   {}", hit.source_path);
                    if !hit.preview.is_empty() {
                        let preview = if hit.preview.len() > 100 {
                            format!("{}...", &hit.preview[..100])
                        } else {
                            hit.preview.clone()
                        };
                        println!("   {}", preview);
                    }
                    println!();
                }
            }
        }
        Some(Commands::Index {
            full,
            rebuild,
            dry_run,
        }) => {
            let mut indexer = Indexer::new(&data_dir, !flags.no_semantic)?;

            if dry_run {
                println!("Running dry-run incremental index (no writes)...");
                let report = indexer.incremental_index_dry_run()?;
                let total_scanned: usize = report.would_scan_by_agent.values().sum();
                println!("Would scan: {} files", total_scanned);
                for (agent, count) in &report.would_scan_by_agent {
                    println!("  {}: {}", agent.display_name(), count);
                }
                println!("Would insert: {}", report.would_insert);
                println!("Would update: {}", report.would_update);
                println!("Would delete: {}", report.would_delete.len());
                for m in &report.would_delete {
                    println!(
                        "  - [{}] id={} {}",
                        m.agent.slug(),
                        m.id,
                        m.source_path.display()
                    );
                }
                if !report.uncertain_paths.is_empty() {
                    println!(
                        "Uncertain (kept): {} rows could not be verified",
                        report.uncertain_paths.len()
                    );
                    for (id, p, e) in &report.uncertain_paths {
                        println!("  ? id={} {} ({})", id, p.display(), e);
                    }
                }
                return Ok(());
            }

            let stats = if rebuild {
                println!("Rebuilding index from SQLite...");
                indexer.rebuild()?
            } else if full {
                println!("Running full index...");
                indexer.full_index()?
            } else {
                println!("Running incremental index...");
                indexer.incremental_index()?
            };

            println!(
                "Indexed {} conversations ({} new, {} updated) with {} messages in {}ms",
                stats.conversations_indexed,
                stats.conversations_inserted,
                stats.conversations_updated,
                stats.messages_indexed,
                stats.time_ms
            );
        }
        Some(Commands::Stats { json }) => {
            let _ = &flags;
            let indexer = Indexer::new(&data_dir, false)?;
            let stats = indexer.storage().stats()?;

            if json {
                let output = StatsOutput::from_storage_stats(stats);
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("Storage Statistics");
                println!("==================");
                println!();
                println!("Total Conversations: {}", stats.total_conversations);
                println!("Total Messages: {}", stats.total_messages);
                println!("Database Size: {} bytes", stats.db_size_bytes);
                if let Some(last_indexed) = stats.last_indexed_at {
                    let date = chrono::DateTime::from_timestamp_millis(last_indexed)
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_default();
                    println!("Last Indexed: {}", date);
                }
                println!();
                println!("By Agent:");
                for (agent, agent_stats) in &stats.by_agent {
                    println!(
                        "  {}: {} conversations, {} messages",
                        agent.display_name(),
                        agent_stats.conversations,
                        agent_stats.messages
                    );
                }
            }
        }
        Some(Commands::Agents { json }) => {
            use connectors::Connector;

            let connectors = connectors::all_connectors();

            if json {
                let agents: Vec<AgentInfoOutput> = connectors
                    .iter()
                    .map(|c| AgentInfoOutput {
                        slug: c.agent().slug().to_string(),
                        name: c.agent().display_name().to_string(),
                        detected: c.detect(),
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&agents)?);
            } else {
                println!("Detected Agents");
                println!("===============");
                println!();
                for connector in connectors {
                    let detected = connector.detect();
                    let status = if detected { "✓" } else { "✗" };
                    println!(
                        "{} {} - {}",
                        status,
                        connector.agent().display_name(),
                        if detected { "detected" } else { "not detected" }
                    );
                }
            }
        }
        Some(Commands::View { path_or_id, json }) => {
            let indexer = Indexer::new(&data_dir, false)?;

            // Try to parse as ID first
            let conversation = if let Ok(id) = path_or_id.parse::<i64>() {
                indexer.storage().get_conversation(id)?
            } else {
                // Try to find by source path
                let all = indexer.storage().get_all_conversations()?;
                let found = all.into_iter().find(|r| r.source_path == path_or_id);
                match found {
                    Some(row) => indexer.storage().get_conversation(row.id)?,
                    None => None,
                }
            };

            match conversation {
                Some(conv) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&conv)?);
                    } else {
                        println!("Conversation");
                        println!("============");
                        println!();
                        println!("Agent: {}", conv.agent.display_name());
                        if let Some(ref title) = conv.title {
                            println!("Title: {}", title);
                        }
                        if let Some(ref workspace) = conv.workspace {
                            println!("Workspace: {}", workspace.display());
                        }
                        println!("Source: {}", conv.source_path.display());
                        if let Some(started) = conv.started_at {
                            let date = chrono::DateTime::from_timestamp_millis(started)
                                .map(|dt| dt.to_rfc3339())
                                .unwrap_or_default();
                            println!("Started: {}", date);
                        }
                        println!();
                        println!("Messages ({}):", conv.messages.len());
                        for msg in &conv.messages {
                            let role_color = match msg.role {
                                model::Role::User => "cyan",
                                model::Role::Assistant => "green",
                                model::Role::Tool => "yellow",
                                model::Role::System => "gray",
                            };
                            println!("\n[{}] {:?}", role_color, msg.role);
                            println!("{}", msg.content);
                            if let Some(ref model) = msg.model {
                                println!("  (model: {})", model);
                            }
                        }
                    }
                }
                None => {
                    eprintln!("Conversation not found: {}", path_or_id);
                    std::process::exit(1);
                }
            }
        }
    }

    Ok(())
}
