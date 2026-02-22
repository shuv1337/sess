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

use cli::{AgentInfoOutput, Cli, Commands, SearchOutput, StatsOutput};
use indexer::Indexer;

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

fn run() -> Result<()> {
    let cli = Cli::parse();

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
            let mut indexer = Indexer::new(&data_dir, !cli.no_semantic)?;

            // Auto-index if needed and not disabled
            if !cli.no_auto_index && indexer.needs_initial_index()? {
                println!("No existing index found. Running initial index...");
                indexer.full_index()?;
            }

            // Get references for TUI
            let tantivy = Arc::new(indexer.tantivy().clone());
            let storage = indexer.storage();

            // Run TUI
            tui::run_app(storage, &tantivy)?;
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
            let mut indexer = Indexer::new(&data_dir, !cli.no_semantic && semantic)?;

            // Auto-index if needed and not disabled
            if !cli.no_auto_index && indexer.needs_initial_index()? {
                eprintln!("No existing index found. Running initial index...");
                indexer.full_index()?;
            }

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
                println!("Found {} results in {}ms", keyword_results.total_hits, keyword_results.query_time_ms);
                println!();

                for (i, hit) in keyword_results.hits.iter().enumerate() {
                    let title = hit.title.clone().unwrap_or_else(|| "Untitled".to_string());
                    let date = hit.created_at.map(|ts| {
                        chrono::DateTime::from_timestamp_millis(ts)
                            .map(|dt| dt.format("%Y-%m-%d").to_string())
                            .unwrap_or_default()
                    }).unwrap_or_default();

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
        Some(Commands::Index { full, rebuild }) => {
            let mut indexer = Indexer::new(&data_dir, !cli.no_semantic)?;

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
                        if detected {
                            "detected"
                        } else {
                            "not detected"
                        }
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
