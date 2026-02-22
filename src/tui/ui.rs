use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use crate::model::{Agent, Role};
use crate::search::RankingMode;
use crate::storage::Storage;
use crate::tui::app::{App, Focus};

pub fn draw(f: &mut Frame, app: &App, _storage: &Storage) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Search bar
            Constraint::Min(10),    // Main content
            Constraint::Length(1),  // Status bar
        ])
        .split(f.area());

    draw_search_bar(f, app, chunks[0]);
    draw_main_content(f, app, chunks[1]);
    draw_status_bar(f, app, chunks[2]);

    if app.show_help {
        draw_help(f, app);
    }
}

fn draw_search_bar(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" sess ")
        .borders(Borders::ALL)
        .border_style(if app.focus == Focus::Search {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        });

    let input = Paragraph::new(app.query.clone())
        .block(block)
        .wrap(Wrap { trim: false });

    f.render_widget(input, area);

    // Position cursor
    if app.focus == Focus::Search {
        let x = area.x + 1 + app.cursor_pos as u16;
        let y = area.y + 1;
        f.set_cursor_position((x, y));
    }
}

fn draw_main_content(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50),
            Constraint::Percentage(50),
        ])
        .split(area);

    draw_results_list(f, app, chunks[0]);
    draw_detail_pane(f, app, chunks[1]);
}

fn draw_results_list(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(format!(" Results ({}) ", app.total_hits))
        .borders(Borders::ALL)
        .border_style(if app.focus == Focus::Results {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        });

    let items: Vec<ListItem> = app
        .results
        .iter()
        .enumerate()
        .map(|(i, result)| {
            let agent_color = agent_color(result.agent);
            let agent_icon = result.agent.icon();

            let title = result.title.clone().unwrap_or_else(|| "Untitled".to_string());
            let date = result.created_at.map(|ts| {
                chrono::DateTime::from_timestamp_millis(ts)
                    .map(|dt| dt.format("%Y-%m-%d").to_string())
                    .unwrap_or_default()
            }).unwrap_or_default();

            let line = Line::from(vec![
                Span::styled(
                    format!("{} ", agent_icon),
                    Style::default().fg(agent_color),
                ),
                Span::raw(title),
                Span::styled(
                    format!(" {}", date),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);

            ListItem::new(line).style(if i == app.selected {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            })
        })
        .collect();

    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn draw_detail_pane(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Detail ")
        .borders(Borders::ALL)
        .border_style(if app.focus == Focus::Detail {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        });

    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(result) = app.results.get(app.selected) {
        let agent_color = agent_color(result.agent);

        let mut text = Text::default();

        // Agent
        text.extend(vec![Line::from(vec![
            Span::styled("Agent: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                result.agent.display_name(),
                Style::default().fg(agent_color),
            ),
        ])]);

        // Title
        if let Some(ref title) = result.title {
            text.extend(vec![Line::from(vec![
                Span::styled("Title: ", Style::default().fg(Color::Yellow)),
                Span::raw(title),
            ])]);
        }

        // Workspace
        if let Some(ref workspace) = result.workspace {
            text.extend(vec![Line::from(vec![
                Span::styled("Workspace: ", Style::default().fg(Color::Yellow)),
                Span::raw(workspace),
            ])]);
        }

        // Date
        if let Some(ts) = result.created_at {
            let date = chrono::DateTime::from_timestamp_millis(ts)
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            text.extend(vec![Line::from(vec![
                Span::styled("Date: ", Style::default().fg(Color::Yellow)),
                Span::raw(date),
            ])]);
        }

        // Score
        text.extend(vec![Line::from(vec![
            Span::styled("Score: ", Style::default().fg(Color::Yellow)),
            Span::raw(format!("{:.3}", result.score)),
        ])]);

        text.extend(vec![Line::from("")]);

        if let Some(conv) = &app.detail_conversation {
            text.extend(vec![Line::from(Span::styled(
                format!("Messages ({}):", conv.messages.len()),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))]);
            text.extend(vec![Line::from("")]);

            for message in &conv.messages {
                let role_label = format!("[{}]", message.role.as_str());
                let mut header = vec![
                    Span::styled(role_label, Style::default().fg(role_color(&message.role)).add_modifier(Modifier::BOLD)),
                ];

                if let Some(ts) = message.timestamp {
                    let ts_str = chrono::DateTime::from_timestamp_millis(ts)
                        .map(|dt| dt.format(" %H:%M:%S").to_string())
                        .unwrap_or_default();
                    header.push(Span::styled(ts_str, Style::default().fg(Color::DarkGray)));
                }

                if let Some(model) = &message.model {
                    header.push(Span::styled(format!("  {}", model), Style::default().fg(Color::DarkGray)));
                }

                text.extend(vec![Line::from(header)]);

                for line in message.content.lines() {
                    text.extend(vec![Line::from(vec![Span::raw("  "), Span::raw(line.to_string())])]);
                }

                if message.content.is_empty() {
                    text.extend(vec![Line::from("  ")]);
                }

                text.extend(vec![Line::from("")]);
            }
        } else {
            // Fallback while loading detail
            text.extend(vec![Line::from(Span::styled(
                "Preview:",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))]);
            text.extend(vec![Line::from(result.preview.clone())]);
            text.extend(vec![Line::from("")]);
            text.extend(vec![Line::from(Span::styled(
                "Loading full conversation...",
                Style::default().fg(Color::DarkGray),
            ))]);
        }

        let paragraph = Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((app.detail_scroll as u16, 0));

        f.render_widget(paragraph, inner);
    } else {
        let placeholder = Paragraph::new("No results selected")
            .alignment(Alignment::Center);
        f.render_widget(placeholder, inner);
    }
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let agent_text = match app.agent_filter {
        Some(agent) => format!("[F3] Agent: {} ", agent.display_name()),
        None => "[F3] Agent: All ".to_string(),
    };

    let time_text = format!("[F5] {} ", app.time_filter.as_str());
    let ranking_text = format!("[F12] Rank: {} ", ranking_label(app.ranking_mode));

    let help_text = "[?] Help [q] Quit".to_string();

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(agent_text.len() as u16),
            Constraint::Length(time_text.len() as u16),
            Constraint::Length(ranking_text.len() as u16),
            Constraint::Min(0),
            Constraint::Length(help_text.len() as u16),
        ])
        .split(area);

    f.render_widget(
        Paragraph::new(agent_text).style(Style::default().fg(Color::Cyan)),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(time_text).style(Style::default().fg(Color::Cyan)),
        chunks[1],
    );
    f.render_widget(
        Paragraph::new(ranking_text).style(Style::default().fg(Color::Cyan)),
        chunks[2],
    );
    f.render_widget(
        Paragraph::new(app.status.clone()).style(Style::default().fg(Color::Gray)),
        chunks[3],
    );
    f.render_widget(
        Paragraph::new(help_text).style(Style::default().fg(Color::DarkGray)),
        chunks[4],
    );
}

fn draw_help(f: &mut Frame, _app: &App) {
    let area = f.area();
    let popup_area = centered_rect(60, 70, area);

    // Clear the background
    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let help_text = r#"
Keyboard Shortcuts

General:
  ?          Toggle this help
  q/Ctrl+C   Quit

Search:
  Type       Search query
  Ctrl+A     Go to start of line
  Ctrl+E     Go to end of line
  F3         Cycle agent filter
  F5         Cycle time filter
  F12        Cycle ranking mode

Results:
  ↑/↓        Navigate results
  Enter      Show detail
  Tab        Focus detail

Detail:
  ↑/↓        Scroll
  PageUp/Dn  Scroll faster
  Tab/←      Back to results
"#;

    let paragraph = Paragraph::new(help_text)
        .block(block)
        .wrap(Wrap { trim: false });

    f.render_widget(paragraph, popup_area);
}

fn agent_color(agent: Agent) -> Color {
    let (r, g, b) = agent.color_code();
    Color::Rgb(r, g, b)
}

fn ranking_label(mode: RankingMode) -> &'static str {
    match mode {
        RankingMode::RecentHeavy => "Recent",
        RankingMode::Balanced => "Balanced",
        RankingMode::Relevance => "Relevance",
        RankingMode::Newest => "Newest",
        RankingMode::Oldest => "Oldest",
    }
}

fn role_color(role: &Role) -> Color {
    match role {
        Role::User => Color::Cyan,
        Role::Assistant => Color::Green,
        Role::Tool => Color::Yellow,
        Role::System => Color::Gray,
    }
}

/// Helper to create a centered rectangle
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

