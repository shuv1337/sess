use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend},
};

use crate::model::{Agent, Conversation};
use crate::search::{RankingMode, SearchQuery, SearchResult};
use crate::storage::Storage;
use crate::tui::refresh::{RefreshConfig, RefreshEvent, RefreshThread};
use crate::tui::search::SearchThread;
use crate::tui::ui;

const PAGE_SCROLL_LINES: usize = 10;

/// Time filter for TUI
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeFilter {
    All,
    Today,
    Week,
    Month,
}

impl TimeFilter {
    pub fn as_str(&self) -> &'static str {
        match self {
            TimeFilter::All => "All time",
            TimeFilter::Today => "Today",
            TimeFilter::Week => "Past week",
            TimeFilter::Month => "Past month",
        }
    }

    fn to_since_timestamp(&self) -> Option<i64> {
        let now = chrono::Local::now();
        match self {
            TimeFilter::All => None,
            TimeFilter::Today => {
                let today = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
                Some(today.and_utc().timestamp_millis())
            }
            TimeFilter::Week => {
                let week_ago = now - chrono::Duration::days(7);
                Some(week_ago.timestamp_millis())
            }
            TimeFilter::Month => {
                let month_ago = now - chrono::Duration::days(30);
                Some(month_ago.timestamp_millis())
            }
        }
    }
}

/// App state
pub struct App {
    pub query: String,
    pub cursor_pos: usize,
    pub results: Vec<SearchResult>,
    pub selected: usize,
    pub detail_scroll: usize,
    pub detail_conversation: Option<Conversation>,
    pub detail_loaded_for: Option<i64>,
    pub agent_filter: Option<Agent>,
    pub time_filter: TimeFilter,
    pub ranking_mode: RankingMode,
    pub status: String,
    pub search_time_ms: u64,
    pub total_hits: usize,
    pub search_generation: Arc<AtomicU64>,
    pub focus: Focus,
    pub show_help: bool,
    pub indexing: bool,
    pub last_index_status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Search,
    Results,
    Detail,
}

impl App {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            cursor_pos: 0,
            results: Vec::new(),
            selected: 0,
            detail_scroll: 0,
            detail_conversation: None,
            detail_loaded_for: None,
            agent_filter: None,
            time_filter: TimeFilter::All,
            ranking_mode: RankingMode::RecentHeavy,
            status: "Type to search...".to_string(),
            search_time_ms: 0,
            total_hits: 0,
            search_generation: Arc::new(AtomicU64::new(0)),
            focus: Focus::Search,
            show_help: false,
            indexing: false,
            last_index_status: None,
        }
    }

    /// Apply a refresh-thread event to TUI state. Returns true if the search
    /// generation should be bumped (i.e. fresh data is available).
    pub fn on_refresh_event(&mut self, ev: RefreshEvent) -> bool {
        match ev {
            RefreshEvent::Started => {
                self.indexing = true;
                self.last_index_status = Some("↻ indexing…".into());
                false
            }
            RefreshEvent::Finished { stats, .. } => {
                self.indexing = false;
                self.last_index_status = Some(format!(
                    "index fresh ({}+{}, {}ms)",
                    stats.conversations_inserted, stats.conversations_updated, stats.time_ms
                ));
                true
            }
            RefreshEvent::SkippedFresh => {
                self.indexing = false;
                self.last_index_status = Some("index fresh".into());
                false
            }
            RefreshEvent::BusySkipped => {
                self.indexing = false;
                self.last_index_status = Some("index busy; skipped".into());
                false
            }
            RefreshEvent::Failed(msg) => {
                self.indexing = false;
                self.last_index_status = Some(format!("index failed: {}", msg));
                false
            }
        }
    }

    pub fn build_search_query(&self) -> SearchQuery {
        SearchQuery {
            text: self.query.clone(),
            agent_filter: self.agent_filter,
            workspace_filter: None,
            since: self.time_filter.to_since_timestamp(),
            until: None,
            limit: 50,
            offset: 0,
            ranking: self.ranking_mode,
            rrf_k: 60,
        }
    }

    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        // Global keys
        match key.code {
            KeyCode::Char('q') if key.modifiers.is_empty() => return false,
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Char('?') => {
                self.show_help = !self.show_help;
                return true;
            }
            _ => {}
        }

        if self.show_help {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.show_help = false;
                }
                _ => {}
            }
            return true;
        }

        // Handle focus-specific keys
        match self.focus {
            Focus::Search => self.on_key_search(key),
            Focus::Results => self.on_key_results(key),
            Focus::Detail => self.on_key_detail(key),
        }

        true
    }

    fn on_key_search(&mut self, key: crossterm::event::KeyEvent) {
        match key.code {
            KeyCode::Char(c) => {
                self.query.insert(self.cursor_pos, c);
                self.cursor_pos += 1;
                self.trigger_search();
            }
            KeyCode::Backspace => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                    self.query.remove(self.cursor_pos);
                    self.trigger_search();
                }
            }
            KeyCode::Delete => {
                if self.cursor_pos < self.query.len() {
                    self.query.remove(self.cursor_pos);
                    self.trigger_search();
                }
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                }
            }
            KeyCode::Right => {
                if self.cursor_pos < self.query.len() {
                    self.cursor_pos += 1;
                }
            }
            KeyCode::Home => {
                self.cursor_pos = 0;
            }
            KeyCode::End => {
                self.cursor_pos = self.query.len();
            }
            KeyCode::Down | KeyCode::Tab => {
                if !self.results.is_empty() {
                    self.focus = Focus::Results;
                    self.selected = 0;
                }
            }
            KeyCode::Enter => {
                if !self.results.is_empty() {
                    self.focus = Focus::Results;
                    self.selected = 0;
                }
            }
            KeyCode::F(3) => {
                self.cycle_agent_filter();
            }
            KeyCode::F(5) => {
                self.cycle_time_filter();
            }
            KeyCode::F(12) => {
                self.cycle_ranking_mode();
            }
            _ => {}
        }
    }

    fn on_key_results(&mut self, key: crossterm::event::KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                } else {
                    self.focus = Focus::Search;
                }
            }
            KeyCode::Down => {
                if self.selected + 1 < self.results.len() {
                    self.selected += 1;
                }
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(PAGE_SCROLL_LINES);
            }
            KeyCode::PageDown => {
                if !self.results.is_empty() {
                    self.selected = (self.selected + PAGE_SCROLL_LINES).min(self.results.len() - 1);
                }
            }
            KeyCode::Enter => {
                self.focus = Focus::Detail;
                self.detail_scroll = 0;
            }
            KeyCode::Right | KeyCode::Tab => {
                if !self.results.is_empty() {
                    self.focus = Focus::Detail;
                    self.detail_scroll = 0;
                }
            }
            KeyCode::Esc => {
                self.focus = Focus::Search;
            }
            KeyCode::F(3) => {
                self.cycle_agent_filter();
            }
            KeyCode::F(5) => {
                self.cycle_time_filter();
            }
            KeyCode::F(12) => {
                self.cycle_ranking_mode();
            }
            _ => {}
        }
    }

    fn on_key_detail(&mut self, key: crossterm::event::KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if self.detail_scroll > 0 {
                    self.detail_scroll -= 1;
                }
            }
            KeyCode::Down => {
                self.detail_scroll += 1;
            }
            KeyCode::PageUp => {
                self.detail_scroll = self.detail_scroll.saturating_sub(PAGE_SCROLL_LINES);
            }
            KeyCode::PageDown => {
                self.detail_scroll += PAGE_SCROLL_LINES;
            }
            KeyCode::Left | KeyCode::Esc | KeyCode::Tab => {
                self.focus = Focus::Results;
            }
            _ => {}
        }
    }

    pub fn trigger_search(&mut self) {
        // Will be handled by the search thread
        self.search_generation.fetch_add(1, Ordering::SeqCst);
    }

    fn cycle_agent_filter(&mut self) {
        self.agent_filter = match self.agent_filter {
            None => Some(Agent::ClaudeCode),
            Some(Agent::ClaudeCode) => Some(Agent::Codex),
            Some(Agent::Codex) => Some(Agent::Hermes),
            Some(Agent::Hermes) => Some(Agent::OpenCode),
            Some(Agent::OpenCode) => Some(Agent::PiAgent),
            Some(Agent::PiAgent) => None,
        };
        self.trigger_search();
    }

    fn cycle_time_filter(&mut self) {
        self.time_filter = match self.time_filter {
            TimeFilter::All => TimeFilter::Today,
            TimeFilter::Today => TimeFilter::Week,
            TimeFilter::Week => TimeFilter::Month,
            TimeFilter::Month => TimeFilter::All,
        };
        self.trigger_search();
    }

    fn cycle_ranking_mode(&mut self) {
        self.ranking_mode = match self.ranking_mode {
            RankingMode::RecentHeavy => RankingMode::Balanced,
            RankingMode::Balanced => RankingMode::Relevance,
            RankingMode::Relevance => RankingMode::Newest,
            RankingMode::Newest => RankingMode::Oldest,
            RankingMode::Oldest => RankingMode::RecentHeavy,
        };
        self.trigger_search();
    }

    pub fn get_current_search_generation(&self) -> u64 {
        self.search_generation.load(Ordering::SeqCst)
    }

    fn selected_conversation_id(&self) -> Option<i64> {
        self.results.get(self.selected).map(|r| r.conversation_id)
    }

    pub fn ensure_detail_loaded(&mut self, storage: &Storage) {
        let selected_id = self.selected_conversation_id();

        if selected_id.is_none() {
            self.detail_conversation = None;
            self.detail_loaded_for = None;
            return;
        }

        if self.detail_loaded_for == selected_id {
            return;
        }

        if let Some(id) = selected_id {
            match storage.get_conversation(id) {
                Ok(conv) => {
                    self.detail_conversation = conv;
                    self.detail_loaded_for = Some(id);
                    self.detail_scroll = 0;
                }
                Err(e) => {
                    self.detail_conversation = None;
                    self.detail_loaded_for = None;
                    self.status = format!("Failed to load detail: {}", e);
                }
            }
        }
    }

    pub fn update_results(
        &mut self,
        results: Vec<SearchResult>,
        total: usize,
        time_ms: u64,
        generation: u64,
    ) {
        // Only update if this is the latest search
        if generation == self.get_current_search_generation() {
            self.results = results;
            self.total_hits = total;
            self.search_time_ms = time_ms;
            self.status = format!("{} hits in {}ms", total, time_ms);

            if self.results.is_empty() {
                self.selected = 0;
            }

            // Adjust selection if out of bounds
            if self.selected >= self.results.len() && !self.results.is_empty() {
                self.selected = self.results.len() - 1;
            }

            // Force detail reload for new result set / selection
            self.detail_conversation = None;
            self.detail_loaded_for = None;
        }
    }
}

/// Run the TUI application
pub fn run_app(
    storage: &Storage,
    tantivy: &Arc<crate::search::index::TantivyIndex>,
    refresh_cfg: RefreshConfig,
) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create search thread
    let search_thread = SearchThread::new(tantivy.clone());

    // Spawn background refresh thread.
    let refresh = RefreshThread::spawn(refresh_cfg);

    // Create app
    let mut app = App::new();

    // Initial search
    search_thread.search(
        app.build_search_query(),
        app.get_current_search_generation(),
    );

    // Run main loop
    let res = run_app_loop(
        &mut terminal,
        &mut app,
        &search_thread,
        storage,
        &refresh,
        tantivy.as_ref(),
    );

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    res
}

fn run_app_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    search_thread: &SearchThread,
    storage: &Storage,
    refresh: &RefreshThread,
    tantivy: &crate::search::index::TantivyIndex,
) -> Result<()> {
    let mut last_search_gen = 0u64;

    loop {
        // Keep detail pane synchronized with current selection
        app.ensure_detail_loaded(storage);

        // Draw UI
        terminal.draw(|f| ui::draw(f, app, storage))?;

        // Drain refresh events.
        while let Some(ev) = refresh.try_recv() {
            let should_refresh_search = app.on_refresh_event(ev);
            if should_refresh_search {
                // Reload Tantivy reader so the next search sees committed docs.
                if let Err(e) = tantivy.reload_reader() {
                    app.status = format!("reload_reader failed: {}", e);
                }
                app.trigger_search();
            }
        }

        // Check for new search results
        if let Some(response) = search_thread.try_recv() {
            app.update_results(
                response.results.hits,
                response.results.total_hits,
                response.results.query_time_ms,
                response.generation,
            );
        }

        // Trigger search if needed
        let current_gen = app.get_current_search_generation();
        if current_gen != last_search_gen {
            search_thread.search(app.build_search_query(), current_gen);
            last_search_gen = current_gen;
        }

        // Handle events with timeout for debounced search
        if event::poll(Duration::from_millis(150))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if !app.on_key(key) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, Role, SourceFile};
    use crate::search::SearchResult;
    use crate::storage::Storage;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tempfile::NamedTempFile;

    fn sample_result(id: i64) -> SearchResult {
        SearchResult {
            conversation_id: id,
            agent: Agent::PiAgent,
            title: Some("test".to_string()),
            workspace: Some("/tmp".to_string()),
            source_path: "/tmp/session.jsonl".to_string(),
            preview: "preview".to_string(),
            created_at: Some(1),
            score: 1.0,
            snippet: None,
        }
    }

    #[test]
    fn update_results_accepts_matching_generation() {
        let mut app = App::new();
        app.trigger_search(); // generation = 1
        app.update_results(vec![sample_result(1)], 1, 5, 1);

        assert_eq!(app.results.len(), 1);
        assert_eq!(app.total_hits, 1);
        assert_eq!(app.search_time_ms, 5);
    }

    #[test]
    fn update_results_ignores_stale_generation() {
        let mut app = App::new();
        app.trigger_search(); // generation = 1
        app.update_results(vec![sample_result(1)], 1, 5, 0); // stale

        assert!(app.results.is_empty());
        assert_eq!(app.total_hits, 0);
    }

    #[test]
    fn ensure_detail_loaded_populates_full_conversation() {
        let db_file = NamedTempFile::new().unwrap();
        let mut storage = Storage::new(db_file.path()).unwrap();

        let conv = Conversation {
            agent: Agent::PiAgent,
            external_id: Some("test-session".to_string()),
            title: Some("A long detail test".to_string()),
            workspace: Some(std::path::PathBuf::from("/tmp")),
            source_path: std::path::PathBuf::from("/tmp/sess.jsonl"),
            source_files: vec![SourceFile {
                path: std::path::PathBuf::from("/tmp/sess.jsonl"),
                mtime: 1,
                size: 1,
            }],
            source_fingerprint: "fp".to_string(),
            started_at: Some(1),
            ended_at: Some(2),
            messages: vec![
                Message {
                    idx: 0,
                    role: Role::User,
                    content: "first message".to_string(),
                    timestamp: Some(1),
                    model: None,
                },
                Message {
                    idx: 1,
                    role: Role::Assistant,
                    content: "second message".to_string(),
                    timestamp: Some(2),
                    model: Some("model-x".to_string()),
                },
            ],
        };

        let upsert = storage.upsert_conversation(&conv).unwrap();

        let mut app = App::new();
        app.results = vec![sample_result(upsert.conversation_id)];
        app.selected = 0;

        app.ensure_detail_loaded(&storage);

        assert_eq!(app.detail_loaded_for, Some(upsert.conversation_id));
        assert!(app.detail_conversation.is_some());
        let loaded = app.detail_conversation.unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].content, "first message");
        assert_eq!(loaded.messages[1].content, "second message");
    }

    #[test]
    fn q_quits_app() {
        let mut app = App::new();
        let should_continue = app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!should_continue);
    }

    #[test]
    fn ctrl_c_quits_app() {
        let mut app = App::new();
        let should_continue = app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!should_continue);
    }

    #[test]
    fn page_down_jumps_through_results_history() {
        let mut app = App::new();
        app.focus = Focus::Results;
        app.results = (0..25).map(sample_result).collect();

        app.on_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.selected, PAGE_SCROLL_LINES);

        app.on_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.selected, PAGE_SCROLL_LINES * 2);

        app.on_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.selected, app.results.len() - 1);
    }

    #[test]
    fn page_up_jumps_through_results_history() {
        let mut app = App::new();
        app.focus = Focus::Results;
        app.results = (0..25).map(sample_result).collect();
        app.selected = 24;

        app.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.selected, 14);

        app.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.selected, 4);

        app.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn detail_page_keys_scroll_by_page_step() {
        let mut app = App::new();
        app.focus = Focus::Detail;

        app.on_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.detail_scroll, PAGE_SCROLL_LINES);

        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.detail_scroll, PAGE_SCROLL_LINES + 1);

        app.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.detail_scroll, 1);

        app.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn refresh_finished_bumps_search_generation_and_updates_status() {
        let mut app = App::new();
        let before = app.get_current_search_generation();
        let stats = crate::indexer::IndexStats {
            conversations_inserted: 3,
            conversations_updated: 1,
            time_ms: 42,
            ..Default::default()
        };
        let bump = app.on_refresh_event(RefreshEvent::Finished {
            stats,
            deleted: 0,
            uncertain: 0,
        });
        assert!(bump, "Finished must signal a search re-trigger");
        // Simulate the loop's own trigger_search() call.
        app.trigger_search();
        assert!(app.get_current_search_generation() > before);
        assert!(app.last_index_status.as_deref().unwrap().contains("fresh"));
        assert!(!app.indexing);
    }

    #[test]
    fn refresh_skipped_fresh_marks_status_without_bumping_search() {
        let mut app = App::new();
        let before = app.get_current_search_generation();
        let bump = app.on_refresh_event(RefreshEvent::SkippedFresh);
        assert!(!bump);
        assert_eq!(app.get_current_search_generation(), before);
        assert_eq!(app.last_index_status.as_deref(), Some("index fresh"));
    }

    #[test]
    fn refresh_failed_records_error_status() {
        let mut app = App::new();
        app.on_refresh_event(RefreshEvent::Failed("boom".into()));
        assert!(app.last_index_status.as_deref().unwrap().contains("boom"));
    }
}
