use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    symbols::scrollbar,
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
};

use crate::model::{Agent, Role};
use crate::search::RankingMode;
use crate::storage::Storage;
use crate::tui::app::{App, Focus};

// ═══════════════════════════════════════════════════════════════════════════════
// Color Palette
// ═══════════════════════════════════════════════════════════════════════════════

/// Warm yellow for active/focused borders.
const C_BORDER_ACTIVE: Color = Color::Rgb(230, 190, 90);
/// Muted gray for inactive borders.
const C_BORDER_INACTIVE: Color = Color::Rgb(55, 55, 55);
/// Softer yellow for field labels.
const C_LABEL: Color = Color::Rgb(185, 175, 130);
/// Neutral medium gray for metadata.
const C_META: Color = Color::Rgb(115, 115, 115);
/// Dim gray for secondary text, inactive items, subtle backgrounds.
const C_DIM: Color = Color::Rgb(75, 75, 75);
/// Very dim for borders of empty states, placeholders.
const C_DIMMER: Color = Color::Rgb(45, 45, 45);
/// Bright accent for active filters and status highlights.
const C_ACCENT: Color = Color::Rgb(110, 210, 255);
/// Selection background – subtle blue-tinted dark.
const C_BG_SELECTED: Color = Color::Rgb(42, 42, 62);
/// Selection foreground – bright white.
const C_FG_SELECTED: Color = Color::White;
/// Subtle warm tint for alternating rows.
const C_BG_ALT: Color = Color::Rgb(28, 28, 30);
/// Scrollbar/track color.
const C_SCROLL: Color = Color::Rgb(70, 70, 90);
/// Background tint for message blocks.
const C_MSG_BG: Color = Color::Rgb(25, 25, 28);
/// Separator line between messages.
const C_SEPARATOR: Color = Color::Rgb(45, 45, 50);

// ═══════════════════════════════════════════════════════════════════════════════
// Public draw entrypoint
// ═══════════════════════════════════════════════════════════════════════════════

pub fn draw(f: &mut Frame, app: &App, _storage: &Storage) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Search bar
            Constraint::Min(10),   // Main content
            Constraint::Length(1), // Status bar
        ])
        .split(f.area());

    draw_search_bar(f, app, outer[0]);
    draw_main_content(f, app, outer[1]);
    draw_status_bar(f, app, outer[2]);

    if app.show_help {
        draw_help(f, app);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Search bar
// ═══════════════════════════════════════════════════════════════════════════════

fn draw_search_bar(f: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.focus == Focus::Search;
    let border_style = if is_focused {
        Style::default()
            .fg(C_BORDER_ACTIVE)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(C_BORDER_INACTIVE)
    };

    let title = if app.indexing {
        " sess │ indexing… ".to_string()
    } else {
        " sess ".to_string()
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    let input_text = if app.query.is_empty() {
        Span::styled(
            "Type to search sessions…",
            Style::default().fg(C_DIM).add_modifier(Modifier::ITALIC),
        )
    } else {
        Span::raw(app.query.clone())
    };

    let input = Paragraph::new(Text::from(Line::from(vec![input_text])))
        .block(block)
        .wrap(Wrap { trim: false });

    f.render_widget(input, area);

    if is_focused {
        let cursor_x = area.x + 1 + app.cursor_pos as u16;
        let cursor_y = area.y + 1;
        f.set_cursor_position((cursor_x, cursor_y));
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Main content (results + detail)
// ═══════════════════════════════════════════════════════════════════════════════

fn draw_main_content(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(42), // Results (slightly narrower)
            Constraint::Percentage(58), // Detail (slightly wider for readability)
        ])
        .split(area);

    draw_results_list(f, app, chunks[0]);
    draw_detail_pane(f, app, chunks[1]);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Results list
// ═══════════════════════════════════════════════════════════════════════════════

fn draw_results_list(f: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.focus == Focus::Results;
    let border_style = if is_focused {
        Style::default().fg(C_BORDER_ACTIVE)
    } else {
        Style::default().fg(C_BORDER_INACTIVE)
    };

    let title = if app.total_hits > 0 {
        format!(" Results ({}) ", app.total_hits)
    } else {
        " Results ".to_string()
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    // Available width inside borders and padding
    let inner_width = area.width.saturating_sub(4) as usize;

    let items: Vec<ListItem> = app
        .results
        .iter()
        .enumerate()
        .map(|(i, result)| {
            let is_selected = i == app.selected;
            let agent_color = agent_color(result.agent);

            let title_text = result
                .title
                .clone()
                .unwrap_or_else(|| "Untitled".to_string());

            let date = result
                .created_at
                .and_then(|ts| {
                    chrono::DateTime::from_timestamp_millis(ts)
                        .map(|dt| dt.format("%Y-%m-%d").to_string())
                })
                .unwrap_or_default();

            // Calculate widths for clean layout
            let agent_prefix = format!("{} ", result.agent.icon());
            let date_suffix = format!(" {}", date);
            let date_width = date_suffix.chars().count();
            let prefix_width = agent_prefix.chars().count();
            let available_for_title = inner_width.saturating_sub(prefix_width + date_width + 1);

            let truncated_title = if title_text.chars().count() > available_for_title {
                let mut truncated = String::new();
                let mut count = 0;
                for ch in title_text.chars() {
                    if count + 3 >= available_for_title {
                        break;
                    }
                    truncated.push(ch);
                    count += 1;
                }
                truncated.push('…');
                truncated
            } else {
                title_text
            };

            // Pad title to align dates
            let title_len = truncated_title.chars().count();
            let pad_needed = available_for_title.saturating_sub(title_len);
            let padding = " ".repeat(pad_needed);

            let line = Line::from(vec![
                Span::styled(agent_prefix, Style::default().fg(agent_color)),
                Span::raw(truncated_title),
                Span::raw(padding),
                Span::styled(date_suffix, Style::default().fg(C_DIM)),
            ]);

            let base_style = if is_selected {
                Style::default()
                    .bg(C_BG_SELECTED)
                    .fg(C_FG_SELECTED)
                    .add_modifier(Modifier::BOLD)
            } else if i % 2 == 1 {
                Style::default().bg(C_BG_ALT)
            } else {
                Style::default()
            };

            ListItem::new(line).style(base_style)
        })
        .collect();

    let list = List::new(items).block(block);
    f.render_widget(list, area);

    // Draw a subtle scrollbar on the right edge if results exceed visible area.
    let visible = area.height.saturating_sub(2) as usize; // minus borders
    if app.results.len() > visible {
        let mut sb_state = ScrollbarState::new(app.results.len()).position(app.selected);
        let sb_area = area.inner(Margin {
            horizontal: 0,
            vertical: 1,
        });
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(C_SCROLL))
                .track_style(Style::default().fg(C_DIMMER))
                .begin_symbol(None)
                .end_symbol(None),
            sb_area,
            &mut sb_state,
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Detail pane
// ═══════════════════════════════════════════════════════════════════════════════

fn draw_detail_pane(f: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.focus == Focus::Detail;
    let border_style = if is_focused {
        Style::default().fg(C_BORDER_ACTIVE)
    } else {
        Style::default().fg(C_BORDER_INACTIVE)
    };

    let block = Block::default()
        .title(" Detail ")
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(result) = app.results.get(app.selected) {
        let agent_color = agent_color(result.agent);
        let mut text = Text::default();

        // ── Header metadata ───────────────────────────────
        text.extend(vec![Line::from(vec![
            Span::styled("Agent:  ", Style::default().fg(C_LABEL)),
            Span::styled(
                format!("{} {}", result.agent.icon(), result.agent.display_name()),
                Style::default()
                    .fg(agent_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ])]);

        if let Some(ref title) = result.title {
            text.extend(vec![Line::from(vec![
                Span::styled("Title:  ", Style::default().fg(C_LABEL)),
                Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
            ])]);
        }

        if let Some(ref workspace) = result.workspace {
            // Truncate long workspace paths
            let display_workspace = if workspace.len() > 55 {
                format!("…{}", &workspace[workspace.len() - 52..])
            } else {
                workspace.clone()
            };
            text.extend(vec![Line::from(vec![
                Span::styled("Path:   ", Style::default().fg(C_LABEL)),
                Span::styled(display_workspace, Style::default().fg(C_META)),
            ])]);
        }

        if let Some(ts) = result.created_at {
            let date = chrono::DateTime::from_timestamp_millis(ts)
                .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_default();
            text.extend(vec![Line::from(vec![
                Span::styled("Date:   ", Style::default().fg(C_LABEL)),
                Span::styled(date, Style::default().fg(C_META)),
            ])]);
        }

        text.extend(vec![Line::from(vec![
            Span::styled("Score:  ", Style::default().fg(C_LABEL)),
            Span::styled(
                format!("{:.3}", result.score),
                Style::default().fg(C_ACCENT),
            ),
        ])]);

        // Separator line
        text.extend(vec![Line::from(Span::styled(
            "─".repeat(inner.width.saturating_sub(1) as usize),
            Style::default().fg(C_SEPARATOR),
        ))]);

        // ── Messages ────────────────────────────────────
        if let Some(conv) = &app.detail_conversation {
            text.extend(vec![Line::from(vec![
                Span::styled(
                    format!("Messages ({})  ", conv.messages.len()),
                    Style::default().fg(C_LABEL).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("· scroll: {}", app.detail_scroll + 1),
                    Style::default().fg(C_DIM).add_modifier(Modifier::ITALIC),
                ),
            ])]);
            text.extend(vec![Line::from("")]);

            for (idx, message) in conv.messages.iter().enumerate() {
                let role_label = format!("[{}]", message.role.as_str());
                let role_col = role_color(&message.role);

                // Message header line
                let mut header_parts = vec![
                    Span::styled("│ ", Style::default().fg(role_col)),
                    Span::styled(
                        role_label,
                        Style::default().fg(role_col).add_modifier(Modifier::BOLD),
                    ),
                ];

                if let Some(ts) = message.timestamp {
                    let ts_str = chrono::DateTime::from_timestamp_millis(ts)
                        .map(|dt| dt.format(" %H:%M:%S").to_string())
                        .unwrap_or_default();
                    header_parts.push(Span::styled(
                        ts_str,
                        Style::default().fg(C_DIM).add_modifier(Modifier::ITALIC),
                    ));
                }

                if let Some(model) = &message.model {
                    header_parts.push(Span::styled(
                        format!("  {}", model),
                        Style::default().fg(C_DIM).add_modifier(Modifier::ITALIC),
                    ));
                }

                text.extend(vec![Line::from(header_parts)]);

                // Message content
                let content_style = Style::default().fg(Color::Rgb(210, 210, 210));
                if message.content.is_empty() {
                    text.extend(vec![Line::from(vec![
                        Span::styled("│ ", Style::default().fg(role_col)),
                        Span::styled(
                            "(empty)",
                            Style::default().fg(C_DIM).add_modifier(Modifier::ITALIC),
                        ),
                    ])]);
                } else {
                    for line in message.content.lines() {
                        text.extend(vec![Line::from(vec![
                            Span::styled("│ ", Style::default().fg(role_col)),
                            Span::styled(line.to_string(), content_style),
                        ])]);
                    }
                }

                // Separator between messages (except last)
                if idx + 1 < conv.messages.len() {
                    text.extend(vec![Line::from("")]);
                    text.extend(vec![Line::from(Span::styled(
                        "·".repeat((inner.width / 2).saturating_sub(1) as usize),
                        Style::default().fg(C_DIMMER),
                    ))]);
                    text.extend(vec![Line::from("")]);
                }
            }
        } else {
            // Loading state
            let spinner = loading_spinner(app.selected);
            text.extend(vec![Line::from(vec![Span::styled(
                "Preview:  ",
                Style::default().fg(C_LABEL).add_modifier(Modifier::BOLD),
            )])]);
            text.extend(vec![Line::from(result.preview.clone())]);
            text.extend(vec![Line::from("")]);
            text.extend(vec![Line::from(vec![Span::styled(
                format!("{} Loading full conversation…", spinner),
                Style::default().fg(C_DIM).add_modifier(Modifier::ITALIC),
            )])]);
        }

        let paragraph = Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((app.detail_scroll as u16, 0));

        f.render_widget(paragraph, inner);
    } else {
        // Empty state
        let empty_text = Text::from(vec![Line::from("")]);
        let placeholder = Paragraph::new(empty_text)
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(C_DIMMER)),
            );
        f.render_widget(placeholder, inner);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Status bar
// ═══════════════════════════════════════════════════════════════════════════════

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    // Build filter indicators with active-state highlighting
    let agent_text = match app.agent_filter {
        Some(agent) => format!(" [F3] {} ", agent.display_name()),
        None => " [F3] All ".to_string(),
    };
    let agent_style = if app.agent_filter.is_some() {
        Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(C_DIM)
    };

    let time_text = format!(" [F5] {} ", app.time_filter.as_str());
    let time_style = if app.time_filter != crate::tui::app::TimeFilter::All {
        Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(C_DIM)
    };

    let ranking_text = format!(" [F12] {} ", ranking_label(app.ranking_mode));
    let ranking_style = if app.ranking_mode != RankingMode::RecentHeavy {
        Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(C_DIM)
    };

    // Compose status text (center)
    let status_text = if app.indexing {
        format!(
            " {} ",
            app.last_index_status.as_deref().unwrap_or("indexing…")
        )
    } else {
        format!(" {} ", app.status)
    };

    let help_text = " [?] Help  [q] Quit ";

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

    f.render_widget(Paragraph::new(agent_text).style(agent_style), chunks[0]);
    f.render_widget(Paragraph::new(time_text).style(time_style), chunks[1]);
    f.render_widget(Paragraph::new(ranking_text).style(ranking_style), chunks[2]);
    f.render_widget(
        Paragraph::new(status_text).style(Style::default().fg(C_META)),
        chunks[3],
    );
    f.render_widget(
        Paragraph::new(help_text).style(Style::default().fg(C_DIM)),
        chunks[4],
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Help popup
// ═══════════════════════════════════════════════════════════════════════════════

fn draw_help(f: &mut Frame, _app: &App) {
    let area = f.area();
    let popup_area = centered_rect(58, 72, area);

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" Keyboard Shortcuts ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_BORDER_ACTIVE))
        .title_style(
            Style::default()
                .fg(C_BORDER_ACTIVE)
                .add_modifier(Modifier::BOLD),
        );

    let mut help_lines = vec![
        Line::from(vec![Span::styled(
            "General",
            Style::default()
                .fg(C_LABEL)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )]),
        Line::from(vec![
            Span::styled("  ?          ", Style::default().fg(C_ACCENT)),
            Span::raw("Toggle this help"),
        ]),
        Line::from(vec![
            Span::styled("  q, Ctrl+C  ", Style::default().fg(C_ACCENT)),
            Span::raw("Quit"),
        ]),
        Line::from(""),
    ];

    help_lines.push(Line::from(vec![Span::styled(
        "Search",
        Style::default()
            .fg(C_LABEL)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )]));
    help_lines.push(Line::from(vec![
        Span::styled("  Type       ", Style::default().fg(C_ACCENT)),
        Span::raw("Search query (instant)"),
    ]));
    help_lines.push(Line::from(vec![
        Span::styled("  Ctrl+A     ", Style::default().fg(C_ACCENT)),
        Span::raw("Go to start of line"),
    ]));
    help_lines.push(Line::from(vec![
        Span::styled("  Ctrl+E     ", Style::default().fg(C_ACCENT)),
        Span::raw("Go to end of line"),
    ]));
    help_lines.push(Line::from(vec![
        Span::styled("  F3         ", Style::default().fg(C_ACCENT)),
        Span::raw("Cycle agent filter"),
    ]));
    help_lines.push(Line::from(vec![
        Span::styled("  F5         ", Style::default().fg(C_ACCENT)),
        Span::raw("Cycle time filter"),
    ]));
    help_lines.push(Line::from(vec![
        Span::styled("  F12        ", Style::default().fg(C_ACCENT)),
        Span::raw("Cycle ranking mode"),
    ]));
    help_lines.push(Line::from(""));

    help_lines.push(Line::from(vec![Span::styled(
        "Navigation",
        Style::default()
            .fg(C_LABEL)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )]));
    help_lines.push(Line::from(vec![
        Span::styled("  ↑ / ↓      ", Style::default().fg(C_ACCENT)),
        Span::raw("Navigate results / scroll"),
    ]));
    help_lines.push(Line::from(vec![
        Span::styled("  Tab        ", Style::default().fg(C_ACCENT)),
        Span::raw("Focus next pane"),
    ]));
    help_lines.push(Line::from(vec![
        Span::styled("  ← / Esc    ", Style::default().fg(C_ACCENT)),
        Span::raw("Focus previous pane"),
    ]));
    help_lines.push(Line::from(vec![
        Span::styled("  Enter      ", Style::default().fg(C_ACCENT)),
        Span::raw("Select / open detail"),
    ]));
    help_lines.push(Line::from(vec![
        Span::styled("  PgUp/Dn    ", Style::default().fg(C_ACCENT)),
        Span::raw("Page results / scroll detail faster"),
    ]));
    help_lines.push(Line::from(""));

    help_lines.push(Line::from(vec![Span::styled(
        "Filters",
        Style::default()
            .fg(C_LABEL)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )]));
    help_lines.push(Line::from(vec![
        Span::styled("  Agent:  ", Style::default().fg(C_META)),
        Span::raw("Claude Code ●  Codex ◆  Hermes ♦  OpenCode ■  Pi Agent ▲"),
    ]));
    help_lines.push(Line::from(vec![
        Span::styled("  Rank:   ", Style::default().fg(C_META)),
        Span::raw("Recent  Balanced  Relevance  Newest  Oldest"),
    ]));
    help_lines.push(Line::from(vec![
        Span::styled("  Time:   ", Style::default().fg(C_META)),
        Span::raw("All time  Today  Past week  Past month"),
    ]));

    let text = Text::from(help_lines);

    let paragraph = Paragraph::new(text).block(block).wrap(Wrap { trim: false });

    f.render_widget(paragraph, popup_area);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════════

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
        Role::User => Color::Rgb(110, 210, 255),      // Cyan-ish
        Role::Assistant => Color::Rgb(130, 230, 160), // Green-ish
        Role::Tool => Color::Rgb(230, 200, 110),      // Yellow-ish
        Role::System => Color::Rgb(140, 140, 140),    // Gray
    }
}

/// Rough line count estimate for the scroll indicator.
fn estimate_total_lines(_messages: &[crate::model::Message], _width: u16) -> usize {
    // Line count estimate is intentionally rough — wrapped lines
    // depend on runtime width which we don't pre-calculate.
    let mut count = 3; // header + separator + "Messages" line
    for msg in _messages {
        count += 2; // blank line + header
        let content_lines = msg.content.lines().count();
        count += content_lines.max(1);
        count += 3; // separator between messages
    }
    count
}

/// Simple animated spinner based on selection index (no frame counter needed).
fn loading_spinner(tick: usize) -> &'static str {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[tick % FRAMES.len()]
}

/// Create a centered rectangle.
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
