use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::DateTime;
use chrono::Utc;
use codex_core::Cursor;
use codex_core::INTERACTIVE_SESSION_SOURCES;
use codex_core::RolloutRecorder;
use codex_core::ThreadItem;
use codex_core::ThreadSortKey;
use codex_core::ThreadsPage;
use codex_core::find_thread_labels_by_ids;
use codex_core::find_thread_names_by_ids;
use codex_core::path_utils;
use codex_core::read_session_meta_line;
use codex_core::util::format_fork_thread_name;
use color_eyre::eyre::Result;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Stylize as _;
use ratatui::text::Line;
use ratatui::text::Span;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;
use unicode_width::UnicodeWidthStr;

use crate::diff_render::display_path_for;
use crate::key_hint;
use crate::text_formatting::truncate_text;
use crate::tui::FrameRequester;
use crate::tui::Tui;
use crate::tui::TuiEvent;
use codex_protocol::ThreadId;

const PAGE_SIZE: usize = 25;
const LOAD_NEAR_THRESHOLD: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSelection {
    StartFresh,
    Resume(PathBuf),
    Manage(SessionManagementAction),
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionManagementActionKind {
    Archive,
    DeletePermanent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionManagementAction {
    pub kind: SessionManagementActionKind,
    pub path: PathBuf,
    pub thread_id: Option<ThreadId>,
    pub is_current_session: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionView {
    Active,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPickerExit {
    Close,
}

#[derive(Clone)]
struct PageLoadRequest {
    codex_home: PathBuf,
    cursor: Option<Cursor>,
    request_token: usize,
    search_token: Option<usize>,
    default_provider: String,
    sort_key: ThreadSortKey,
    view: SessionView,
}

type PageLoader = Arc<dyn Fn(PageLoadRequest) + Send + Sync>;

enum BackgroundEvent {
    PageLoaded {
        request_token: usize,
        search_token: Option<usize>,
        page: std::io::Result<ThreadsPage>,
    },
}

/// Interactive session picker that lists recorded rollout files with simple
/// search and pagination.
///
/// The picker displays sessions in a table with timestamp columns (created/updated),
/// git branch, working directory, and conversation preview. Users can toggle
/// between sorting by creation time and last-updated time using the Tab key.
///
/// Sessions are loaded on-demand via cursor-based pagination. The backend
/// `RolloutRecorder::list_threads` returns pages ordered by the selected sort key,
/// and the picker deduplicates across pages to handle overlapping windows when
/// new sessions appear during pagination.
///
/// Filtering happens in two layers:
/// 1. Provider and source filtering at the backend (only interactive CLI sessions
///    for the current model provider).
/// 2. Working-directory filtering at the picker (unless `--all` is passed).
pub async fn run_sessions_picker(
    tui: &mut Tui,
    codex_home: &Path,
    default_provider: &str,
    show_all: bool,
    initial_view: SessionView,
    exit_behavior: SessionPickerExit,
    current_session_path: Option<PathBuf>,
) -> Result<SessionSelection> {
    run_session_picker(
        tui,
        codex_home,
        default_provider,
        show_all,
        initial_view,
        exit_behavior,
        current_session_path,
    )
    .await
}

async fn run_session_picker(
    tui: &mut Tui,
    codex_home: &Path,
    default_provider: &str,
    show_all: bool,
    initial_view: SessionView,
    exit_behavior: SessionPickerExit,
    current_session_path: Option<PathBuf>,
) -> Result<SessionSelection> {
    let alt = AltScreenGuard::enter(tui);
    let (bg_tx, bg_rx) = mpsc::unbounded_channel();

    let default_provider = default_provider.to_string();
    let filter_cwd = if show_all {
        None
    } else {
        std::env::current_dir().ok()
    };

    let loader_tx = bg_tx.clone();
    let page_loader: PageLoader = Arc::new(move |request: PageLoadRequest| {
        let tx = loader_tx.clone();
        tokio::spawn(async move {
            let provider_filter = vec![request.default_provider.clone()];
            let page = match request.view {
                SessionView::Active => {
                    RolloutRecorder::list_threads(
                        &request.codex_home,
                        PAGE_SIZE,
                        request.cursor.as_ref(),
                        request.sort_key,
                        INTERACTIVE_SESSION_SOURCES,
                        Some(provider_filter.as_slice()),
                        request.default_provider.as_str(),
                    )
                    .await
                }
                SessionView::Archived => {
                    RolloutRecorder::list_archived_threads(
                        &request.codex_home,
                        PAGE_SIZE,
                        request.cursor.as_ref(),
                        request.sort_key,
                        INTERACTIVE_SESSION_SOURCES,
                        Some(provider_filter.as_slice()),
                        request.default_provider.as_str(),
                    )
                    .await
                }
            };
            let _ = tx.send(BackgroundEvent::PageLoaded {
                request_token: request.request_token,
                search_token: request.search_token,
                page,
            });
        });
    });

    let mut state = PickerState::new(
        codex_home.to_path_buf(),
        alt.tui.frame_requester(),
        page_loader,
        default_provider.clone(),
        show_all,
        filter_cwd,
        initial_view,
        exit_behavior,
        current_session_path,
    );
    state.start_initial_load();
    state.request_frame();

    let mut tui_events = alt.tui.event_stream().fuse();
    let mut background_events = UnboundedReceiverStream::new(bg_rx).fuse();

    loop {
        tokio::select! {
            Some(ev) = tui_events.next() => {
                match ev {
                    TuiEvent::Key(key) => {
                        if matches!(key.kind, KeyEventKind::Release) {
                            continue;
                        }
                        if let Some(sel) = state.handle_key(key).await? {
                            return Ok(sel);
                        }
                    }
                    TuiEvent::Draw => {
                        if let Ok(size) = alt.tui.terminal.size() {
                            let list_height = size.height.saturating_sub(4) as usize;
                            state.update_view_rows(list_height);
                            state.ensure_minimum_rows_for_view(list_height);
                        }
                        draw_picker(alt.tui, &state)?;
                    }
                    _ => {}
                }
            }
            Some(event) = background_events.next() => {
                state.handle_background_event(event).await?;
            }
            else => break,
        }
    }

    // Fallback – treat as cancel/new
    Ok(SessionSelection::StartFresh)
}

/// Returns the human-readable column header for the given sort key.
fn sort_key_label(sort_key: ThreadSortKey) -> &'static str {
    match sort_key {
        ThreadSortKey::CreatedAt => "Created at",
        ThreadSortKey::UpdatedAt => "Updated at",
    }
}

/// RAII guard that ensures we leave the alt-screen on scope exit.
struct AltScreenGuard<'a> {
    tui: &'a mut Tui,
}

impl<'a> AltScreenGuard<'a> {
    fn enter(tui: &'a mut Tui) -> Self {
        let _ = tui.enter_alt_screen();
        Self { tui }
    }
}

impl Drop for AltScreenGuard<'_> {
    fn drop(&mut self) {
        let _ = self.tui.leave_alt_screen();
    }
}

struct PickerState {
    codex_home: PathBuf,
    requester: FrameRequester,
    pagination: PaginationState,
    all_rows: Vec<Row>,
    filtered_rows: Vec<Row>,
    seen_paths: HashSet<PathBuf>,
    selected: usize,
    scroll_top: usize,
    query: String,
    search_state: SearchState,
    next_request_token: usize,
    next_search_token: usize,
    page_loader: PageLoader,
    view_rows: Option<usize>,
    default_provider: String,
    show_all: bool,
    filter_cwd: Option<PathBuf>,
    view: SessionView,
    exit_behavior: SessionPickerExit,
    current_session_path: Option<PathBuf>,
    sort_key: ThreadSortKey,
    thread_name_cache: HashMap<ThreadId, Option<String>>,
    thread_label_cache: HashMap<ThreadId, Option<String>>,
    fork_parent_id_cache: HashMap<PathBuf, Option<ThreadId>>,
    pending_management_confirmation: Option<PendingManagementConfirmation>,
}

#[derive(Clone, Debug)]
struct PendingManagementConfirmation {
    action: SessionManagementAction,
}

struct PaginationState {
    next_cursor: Option<Cursor>,
    num_scanned_files: usize,
    reached_scan_cap: bool,
    loading: LoadingState,
}

#[derive(Clone, Copy, Debug)]
enum LoadingState {
    Idle,
    Pending(PendingLoad),
}

#[derive(Clone, Copy, Debug)]
struct PendingLoad {
    request_token: usize,
    search_token: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
enum SearchState {
    Idle,
    Active { token: usize },
}

enum LoadTrigger {
    Scroll,
    Search { token: usize },
}

impl LoadingState {
    fn is_pending(&self) -> bool {
        matches!(self, LoadingState::Pending(_))
    }
}

impl SearchState {
    fn active_token(&self) -> Option<usize> {
        match self {
            SearchState::Idle => None,
            SearchState::Active { token } => Some(*token),
        }
    }

    fn is_active(&self) -> bool {
        self.active_token().is_some()
    }
}

#[derive(Clone)]
struct Row {
    path: PathBuf,
    preview: String,
    thread_id: Option<ThreadId>,
    thread_name: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    cwd: Option<PathBuf>,
    git_branch: Option<String>,
}

impl Row {
    fn display_preview(&self) -> &str {
        self.thread_name.as_deref().unwrap_or(&self.preview)
    }

    fn matches_query(&self, query: &str) -> bool {
        if self.preview.to_lowercase().contains(query) {
            return true;
        }
        if let Some(thread_name) = self.thread_name.as_ref()
            && thread_name.to_lowercase().contains(query)
        {
            return true;
        }
        false
    }
}

impl PickerState {
    fn new(
        codex_home: PathBuf,
        requester: FrameRequester,
        page_loader: PageLoader,
        default_provider: String,
        show_all: bool,
        filter_cwd: Option<PathBuf>,
        view: SessionView,
        exit_behavior: SessionPickerExit,
        current_session_path: Option<PathBuf>,
    ) -> Self {
        Self {
            codex_home,
            requester,
            pagination: PaginationState {
                next_cursor: None,
                num_scanned_files: 0,
                reached_scan_cap: false,
                loading: LoadingState::Idle,
            },
            all_rows: Vec::new(),
            filtered_rows: Vec::new(),
            seen_paths: HashSet::new(),
            selected: 0,
            scroll_top: 0,
            query: String::new(),
            search_state: SearchState::Idle,
            next_request_token: 0,
            next_search_token: 0,
            page_loader,
            view_rows: None,
            default_provider,
            show_all,
            filter_cwd,
            view,
            exit_behavior,
            current_session_path,
            sort_key: ThreadSortKey::CreatedAt,
            thread_name_cache: HashMap::new(),
            thread_label_cache: HashMap::new(),
            fork_parent_id_cache: HashMap::new(),
            pending_management_confirmation: None,
        }
    }

    fn request_frame(&self) {
        self.requester.schedule_frame();
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<Option<SessionSelection>> {
        if let Some(pending) = self.pending_management_confirmation.clone() {
            match key.code {
                KeyCode::Esc => {
                    self.pending_management_confirmation = None;
                    self.request_frame();
                }
                KeyCode::Enter => {
                    self.pending_management_confirmation = None;
                    return Ok(Some(SessionSelection::Manage(pending.action)));
                }
                KeyCode::Char('c')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    return Ok(Some(SessionSelection::Exit));
                }
                _ => {}
            }
            return Ok(None);
        }

        match key.code {
            KeyCode::Esc => {
                let exit = match self.exit_behavior {
                    SessionPickerExit::Close => SessionSelection::Exit,
                };
                return Ok(Some(exit));
            }
            KeyCode::Char('c')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                return Ok(Some(SessionSelection::Exit));
            }
            KeyCode::Enter => {
                if let Some(row) = self.filtered_rows.get(self.selected) {
                    if self
                        .current_session_path
                        .as_ref()
                        .is_some_and(|path| paths_match(path, &row.path))
                    {
                        let exit = match self.exit_behavior {
                            SessionPickerExit::Close => SessionSelection::Exit,
                        };
                        return Ok(Some(exit));
                    }
                    return Ok(Some(SessionSelection::Resume(row.path.clone())));
                }
            }
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                    self.ensure_selected_visible();
                }
                self.request_frame();
            }
            KeyCode::Down => {
                if self.selected + 1 < self.filtered_rows.len() {
                    self.selected += 1;
                    self.ensure_selected_visible();
                }
                self.maybe_load_more_for_scroll();
                self.request_frame();
            }
            KeyCode::PageUp => {
                let step = self.view_rows.unwrap_or(10).max(1);
                if self.selected > 0 {
                    self.selected = self.selected.saturating_sub(step);
                    self.ensure_selected_visible();
                    self.request_frame();
                }
            }
            KeyCode::PageDown => {
                if !self.filtered_rows.is_empty() {
                    let step = self.view_rows.unwrap_or(10).max(1);
                    let max_index = self.filtered_rows.len().saturating_sub(1);
                    self.selected = (self.selected + step).min(max_index);
                    self.ensure_selected_visible();
                    self.maybe_load_more_for_scroll();
                    self.request_frame();
                }
            }
            KeyCode::Tab => {
                self.toggle_sort_key();
                self.request_frame();
            }
            KeyCode::Char('a')
                if self.query.is_empty()
                    && !key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL)
                    && !key.modifiers.contains(crossterm::event::KeyModifiers::ALT) =>
            {
                self.toggle_view();
            }
            KeyCode::Char('o')
                if self.query.is_empty()
                    && !key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL)
                    && !key.modifiers.contains(crossterm::event::KeyModifiers::ALT) =>
            {
                self.toggle_show_all();
                self.request_frame();
            }
            KeyCode::Char('d')
                if self.query.is_empty()
                    && !key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL)
                    && !key.modifiers.contains(crossterm::event::KeyModifiers::ALT) =>
            {
                self.begin_management_confirmation_for_selected();
            }
            KeyCode::Backspace => {
                let mut new_query = self.query.clone();
                new_query.pop();
                self.set_query(new_query);
            }
            KeyCode::Char(c) => {
                // basic text input for search
                if !key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
                    && !key.modifiers.contains(crossterm::event::KeyModifiers::ALT)
                {
                    let mut new_query = self.query.clone();
                    new_query.push(c);
                    self.set_query(new_query);
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn start_initial_load(&mut self) {
        self.reset_pagination();
        self.all_rows.clear();
        self.filtered_rows.clear();
        self.seen_paths.clear();
        self.selected = 0;

        let search_token = if self.query.is_empty() {
            self.search_state = SearchState::Idle;
            None
        } else {
            let token = self.allocate_search_token();
            self.search_state = SearchState::Active { token };
            Some(token)
        };

        let request_token = self.allocate_request_token();
        self.pagination.loading = LoadingState::Pending(PendingLoad {
            request_token,
            search_token,
        });
        self.request_frame();

        (self.page_loader)(PageLoadRequest {
            codex_home: self.codex_home.clone(),
            cursor: None,
            request_token,
            search_token,
            default_provider: self.default_provider.clone(),
            sort_key: self.sort_key,
            view: self.view,
        });
    }

    async fn handle_background_event(&mut self, event: BackgroundEvent) -> Result<()> {
        match event {
            BackgroundEvent::PageLoaded {
                request_token,
                search_token,
                page,
            } => {
                let pending = match self.pagination.loading {
                    LoadingState::Pending(pending) => pending,
                    LoadingState::Idle => return Ok(()),
                };
                if pending.request_token != request_token {
                    return Ok(());
                }
                self.pagination.loading = LoadingState::Idle;
                let page = page.map_err(color_eyre::Report::from)?;
                self.ingest_page(page);
                self.update_thread_names().await;
                let completed_token = pending.search_token.or(search_token);
                self.continue_search_if_token_matches(completed_token);
            }
        }
        Ok(())
    }

    fn reset_pagination(&mut self) {
        self.pagination.next_cursor = None;
        self.pagination.num_scanned_files = 0;
        self.pagination.reached_scan_cap = false;
        self.pagination.loading = LoadingState::Idle;
    }

    fn ingest_page(&mut self, page: ThreadsPage) {
        if let Some(cursor) = page.next_cursor.clone() {
            self.pagination.next_cursor = Some(cursor);
        } else {
            self.pagination.next_cursor = None;
        }
        self.pagination.num_scanned_files = self
            .pagination
            .num_scanned_files
            .saturating_add(page.num_scanned_files);
        if page.reached_scan_cap {
            self.pagination.reached_scan_cap = true;
        }

        let rows = rows_from_items(page.items);
        for row in rows {
            if self.seen_paths.insert(row.path.clone()) {
                self.all_rows.push(row);
            }
        }

        self.apply_filter();
    }

    async fn update_thread_names(&mut self) {
        let mut missing_ids = HashSet::new();
        for row in &self.all_rows {
            let Some(thread_id) = row_thread_id(row) else {
                continue;
            };
            if self.thread_name_cache.contains_key(&thread_id) {
                continue;
            }
            missing_ids.insert(thread_id);
        }

        if !missing_ids.is_empty() {
            let names = find_thread_names_by_ids(&self.codex_home, &missing_ids)
                .await
                .unwrap_or_default();
            for thread_id in missing_ids {
                let thread_name = names.get(&thread_id).cloned();
                self.thread_name_cache.insert(thread_id, thread_name);
            }
        }

        let mut paths_to_check_for_forks = Vec::new();
        for row in &self.all_rows {
            if self.fork_parent_id_cache.contains_key(&row.path) {
                continue;
            }
            paths_to_check_for_forks.push(row.path.clone());
        }
        for path in paths_to_check_for_forks {
            let parent_id = read_session_meta_line(path.as_path())
                .await
                .ok()
                .and_then(|meta| meta.meta.forked_from_id);
            self.fork_parent_id_cache.insert(path, parent_id);
        }

        let mut missing_parent_ids = HashSet::new();
        for row in &self.all_rows {
            let Some(thread_id) = row_thread_id(row) else {
                continue;
            };
            if self
                .thread_name_cache
                .get(&thread_id)
                .cloned()
                .flatten()
                .is_some()
            {
                continue;
            }
            let Some(parent_id) = self.fork_parent_id_cache.get(&row.path).cloned().flatten()
            else {
                continue;
            };
            if self.thread_label_cache.contains_key(&parent_id) {
                continue;
            }
            missing_parent_ids.insert(parent_id);
        }

        if !missing_parent_ids.is_empty() {
            let labels = find_thread_labels_by_ids(&self.codex_home, &missing_parent_ids)
                .await
                .unwrap_or_default();
            for parent_id in missing_parent_ids {
                let label = labels.get(&parent_id).cloned();
                self.thread_label_cache.insert(parent_id, label);
            }
        }

        let mut fork_rows_by_parent: HashMap<ThreadId, Vec<usize>> = HashMap::new();
        for (index, row) in self.all_rows.iter().enumerate() {
            let Some(thread_id) = row_thread_id(row) else {
                continue;
            };
            if self
                .thread_name_cache
                .get(&thread_id)
                .cloned()
                .flatten()
                .is_some()
            {
                continue;
            }

            let Some(parent_id) = self.fork_parent_id_cache.get(&row.path).cloned().flatten()
            else {
                continue;
            };
            fork_rows_by_parent
                .entry(parent_id)
                .or_default()
                .push(index);
        }

        let mut fork_numbers_by_index: HashMap<usize, usize> = HashMap::new();
        for row_indexes in fork_rows_by_parent.values_mut() {
            row_indexes.sort_by(|left, right| {
                compare_fork_rows(&self.all_rows[*left], &self.all_rows[*right])
            });
            for (offset, row_index) in row_indexes.iter().enumerate() {
                fork_numbers_by_index.insert(*row_index, offset + 1);
            }
        }

        let mut updated = false;
        for (row_index, row) in self.all_rows.iter_mut().enumerate() {
            let explicit_thread_name = row_thread_id(row)
                .and_then(|thread_id| self.thread_name_cache.get(&thread_id).cloned().flatten());
            let next_thread_name = if explicit_thread_name.is_some() {
                explicit_thread_name
            } else if let Some(parent_id) =
                self.fork_parent_id_cache.get(&row.path).cloned().flatten()
            {
                let parent_label = self
                    .thread_label_cache
                    .get(&parent_id)
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| "Untitled".to_string());
                let fork_number = fork_numbers_by_index.get(&row_index).copied().unwrap_or(1);
                format_fork_thread_name(fork_number, &parent_label)
            } else {
                None
            };
            if row.thread_name == next_thread_name {
                continue;
            }
            row.thread_name = next_thread_name;
            updated = true;
        }

        if updated {
            self.apply_filter();
        }
    }

    fn apply_filter(&mut self) {
        let base_iter = self
            .all_rows
            .iter()
            .filter(|row| self.row_matches_filter(row));
        if self.query.is_empty() {
            self.filtered_rows = base_iter.cloned().collect();
        } else {
            let q = self.query.to_lowercase();
            self.filtered_rows = base_iter.filter(|r| r.matches_query(&q)).cloned().collect();
        }
        if self.selected >= self.filtered_rows.len() {
            self.selected = self.filtered_rows.len().saturating_sub(1);
        }
        if self.filtered_rows.is_empty() {
            self.scroll_top = 0;
        }
        self.ensure_selected_visible();
        self.request_frame();
    }

    fn row_matches_filter(&self, row: &Row) -> bool {
        if self.show_all {
            return true;
        }
        let Some(filter_cwd) = self.filter_cwd.as_ref() else {
            return true;
        };
        let Some(row_cwd) = row.cwd.as_ref() else {
            return false;
        };
        paths_match(row_cwd, filter_cwd)
    }

    fn set_query(&mut self, new_query: String) {
        if self.query == new_query {
            return;
        }
        self.query = new_query;
        self.selected = 0;
        self.apply_filter();
        if self.query.is_empty() {
            self.search_state = SearchState::Idle;
            return;
        }
        if !self.filtered_rows.is_empty() {
            self.search_state = SearchState::Idle;
            return;
        }
        if self.pagination.reached_scan_cap || self.pagination.next_cursor.is_none() {
            self.search_state = SearchState::Idle;
            return;
        }
        let token = self.allocate_search_token();
        self.search_state = SearchState::Active { token };
        self.load_more_if_needed(LoadTrigger::Search { token });
    }

    fn continue_search_if_needed(&mut self) {
        let Some(token) = self.search_state.active_token() else {
            return;
        };
        if !self.filtered_rows.is_empty() {
            self.search_state = SearchState::Idle;
            return;
        }
        if self.pagination.reached_scan_cap || self.pagination.next_cursor.is_none() {
            self.search_state = SearchState::Idle;
            return;
        }
        self.load_more_if_needed(LoadTrigger::Search { token });
    }

    fn continue_search_if_token_matches(&mut self, completed_token: Option<usize>) {
        let Some(active) = self.search_state.active_token() else {
            return;
        };
        if let Some(token) = completed_token
            && token != active
        {
            return;
        }
        self.continue_search_if_needed();
    }

    fn ensure_selected_visible(&mut self) {
        if self.filtered_rows.is_empty() {
            self.scroll_top = 0;
            return;
        }
        let capacity = self.view_rows.unwrap_or(self.filtered_rows.len()).max(1);

        if self.selected < self.scroll_top {
            self.scroll_top = self.selected;
        } else {
            let last_visible = self.scroll_top.saturating_add(capacity - 1);
            if self.selected > last_visible {
                self.scroll_top = self.selected.saturating_sub(capacity - 1);
            }
        }

        let max_start = self.filtered_rows.len().saturating_sub(capacity);
        if self.scroll_top > max_start {
            self.scroll_top = max_start;
        }
    }

    fn ensure_minimum_rows_for_view(&mut self, minimum_rows: usize) {
        if minimum_rows == 0 {
            return;
        }
        if self.filtered_rows.len() >= minimum_rows {
            return;
        }
        if self.pagination.loading.is_pending() || self.pagination.next_cursor.is_none() {
            return;
        }
        if let Some(token) = self.search_state.active_token() {
            self.load_more_if_needed(LoadTrigger::Search { token });
        } else {
            self.load_more_if_needed(LoadTrigger::Scroll);
        }
    }

    fn update_view_rows(&mut self, rows: usize) {
        self.view_rows = if rows == 0 { None } else { Some(rows) };
        self.ensure_selected_visible();
    }

    fn maybe_load_more_for_scroll(&mut self) {
        if self.pagination.loading.is_pending() {
            return;
        }
        if self.pagination.next_cursor.is_none() {
            return;
        }
        if self.filtered_rows.is_empty() {
            return;
        }
        let remaining = self.filtered_rows.len().saturating_sub(self.selected + 1);
        if remaining <= LOAD_NEAR_THRESHOLD {
            self.load_more_if_needed(LoadTrigger::Scroll);
        }
    }

    fn load_more_if_needed(&mut self, trigger: LoadTrigger) {
        if self.pagination.loading.is_pending() {
            return;
        }
        let Some(cursor) = self.pagination.next_cursor.clone() else {
            return;
        };
        let request_token = self.allocate_request_token();
        let search_token = match trigger {
            LoadTrigger::Scroll => None,
            LoadTrigger::Search { token } => Some(token),
        };
        self.pagination.loading = LoadingState::Pending(PendingLoad {
            request_token,
            search_token,
        });
        self.request_frame();

        (self.page_loader)(PageLoadRequest {
            codex_home: self.codex_home.clone(),
            cursor: Some(cursor),
            request_token,
            search_token,
            default_provider: self.default_provider.clone(),
            sort_key: self.sort_key,
            view: self.view,
        });
    }

    fn allocate_request_token(&mut self) -> usize {
        let token = self.next_request_token;
        self.next_request_token = self.next_request_token.wrapping_add(1);
        token
    }

    fn allocate_search_token(&mut self) -> usize {
        let token = self.next_search_token;
        self.next_search_token = self.next_search_token.wrapping_add(1);
        token
    }

    fn toggle_view(&mut self) {
        self.view = match self.view {
            SessionView::Active => SessionView::Archived,
            SessionView::Archived => SessionView::Active,
        };
        self.start_initial_load();
    }

    fn toggle_show_all(&mut self) {
        self.show_all = !self.show_all;
        self.filter_cwd = if self.show_all {
            None
        } else {
            std::env::current_dir().ok()
        };
        self.apply_filter();
    }

    /// Cycles the sort order between creation time and last-updated time.
    ///
    /// Triggers a full reload because the backend must re-sort all sessions.
    /// The existing `all_rows` are cleared and pagination restarts from the
    /// beginning with the new sort key.
    fn toggle_sort_key(&mut self) {
        self.sort_key = match self.sort_key {
            ThreadSortKey::CreatedAt => ThreadSortKey::UpdatedAt,
            ThreadSortKey::UpdatedAt => ThreadSortKey::CreatedAt,
        };
        self.start_initial_load();
    }

    fn begin_management_confirmation_for_selected(&mut self) {
        let Some(row) = self.filtered_rows.get(self.selected) else {
            return;
        };

        let is_current_session = self
            .current_session_path
            .as_ref()
            .is_some_and(|path| paths_match(path, &row.path));
        let kind = match self.view {
            SessionView::Active => SessionManagementActionKind::Archive,
            SessionView::Archived => SessionManagementActionKind::DeletePermanent,
        };
        let action = SessionManagementAction {
            kind,
            path: row.path.clone(),
            thread_id: row_thread_id(row),
            is_current_session,
        };
        self.pending_management_confirmation = Some(PendingManagementConfirmation { action });
        self.request_frame();
    }

    fn confirmation_hint_line(&self) -> Option<Line<'static>> {
        let pending = self.pending_management_confirmation.as_ref()?;
        let action = pending.action.kind;
        let warning = if pending.action.is_current_session {
            match action {
                SessionManagementActionKind::Archive => {
                    "This is the currently open chat. Archiving it will end this session."
                }
                SessionManagementActionKind::DeletePermanent => {
                    "This is the currently open chat. Permanent delete will end this session."
                }
            }
        } else {
            match action {
                SessionManagementActionKind::Archive => "Archive this chat?",
                SessionManagementActionKind::DeletePermanent => {
                    "Delete this archived chat permanently? This cannot be undone."
                }
            }
        };

        Some(
            vec![
                warning.bold(),
                "  ".into(),
                key_hint::plain(KeyCode::Enter).into(),
                " confirm ".dim(),
                key_hint::plain(KeyCode::Esc).into(),
                " cancel".dim(),
            ]
            .into(),
        )
    }
}

fn rows_from_items(items: Vec<ThreadItem>) -> Vec<Row> {
    items.into_iter().map(|item| head_to_row(&item)).collect()
}

fn head_to_row(item: &ThreadItem) -> Row {
    let created_at = item.created_at.as_deref().and_then(parse_timestamp_str);
    let updated_at = item
        .updated_at
        .as_deref()
        .and_then(parse_timestamp_str)
        .or(created_at);

    let preview = item
        .first_user_message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| String::from("(no message yet)"));

    Row {
        path: item.path.clone(),
        preview,
        thread_id: thread_id_from_rollout_path(item.path.as_path()).or(item.thread_id),
        thread_name: None,
        created_at,
        updated_at,
        cwd: item.cwd.clone(),
        git_branch: item.git_branch.clone(),
    }
}

fn row_thread_id(row: &Row) -> Option<ThreadId> {
    thread_id_from_rollout_path(row.path.as_path()).or(row.thread_id)
}

fn thread_id_from_rollout_path(path: &Path) -> Option<ThreadId> {
    let file_name = path.file_name()?.to_str()?;
    let stem = file_name.strip_suffix(".jsonl")?;
    if !stem.starts_with("rollout-") || stem.len() < 36 {
        return None;
    }
    let uuid_text = &stem[stem.len().saturating_sub(36)..];
    ThreadId::from_string(uuid_text).ok()
}

fn compare_fork_rows(left: &Row, right: &Row) -> Ordering {
    left.created_at
        .cmp(&right.created_at)
        .then_with(|| left.updated_at.cmp(&right.updated_at))
        .then_with(|| left.path.cmp(&right.path))
}

fn paths_match(a: &Path, b: &Path) -> bool {
    if let (Ok(ca), Ok(cb)) = (
        path_utils::normalize_for_path_comparison(a),
        path_utils::normalize_for_path_comparison(b),
    ) {
        return ca == cb;
    }
    a == b
}

fn parse_timestamp_str(ts: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
}

fn draw_picker(tui: &mut Tui, state: &PickerState) -> std::io::Result<()> {
    // Render full-screen overlay
    let height = tui.terminal.size()?.height;
    tui.draw(height, |frame| {
        let area = frame.area();
        let [header, search, columns, list, hint] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(area.height.saturating_sub(4)),
            Constraint::Length(1),
        ])
        .areas(area);

        // Header
        let mut header_spans: Vec<Span<'static>> = vec!["Sessions".bold().cyan()];
        if state.view == SessionView::Archived {
            header_spans.push(" ".into());
            header_spans.push("(archived)".dim());
        }
        header_spans.push("  ".into());
        header_spans.push("Sort:".dim());
        header_spans.push(" ".into());
        header_spans.push(sort_key_label(state.sort_key).magenta());
        let header_line: Line = header_spans.into();
        frame.render_widget_ref(header_line, header);

        // Search line
        let q = if state.query.is_empty() {
            "Type to search".dim().to_string()
        } else {
            format!("Search: {}", state.query)
        };
        frame.render_widget_ref(Line::from(q), search);

        let metrics = calculate_column_metrics(&state.filtered_rows, state.show_all);

        // Column headers and list
        render_column_headers(frame, columns, &metrics, state.sort_key);
        render_list(frame, list, state, &metrics);

        // Hint line
        let action_label = "switch";
        let toggle_archived = match state.view {
            SessionView::Active => "archived",
            SessionView::Archived => "active",
        };
        let toggle_scope = if state.show_all { "scoped" } else { "all" };
        let hint_line: Line = if let Some(confirm_line) = state.confirmation_hint_line() {
            confirm_line
        } else {
            vec![
                key_hint::plain(KeyCode::Enter).into(),
                format!(" to {action_label} ").dim(),
                "    ".dim(),
                key_hint::plain(KeyCode::Esc).into(),
                " to start new ".dim(),
                "    ".dim(),
                key_hint::ctrl(KeyCode::Char('c')).into(),
                " to quit ".dim(),
                "    ".dim(),
                key_hint::plain(KeyCode::Tab).into(),
                " to toggle sort ".dim(),
                "    ".dim(),
                key_hint::plain(KeyCode::Char('a')).into(),
                format!(" {toggle_archived} ").dim(),
                "    ".dim(),
                key_hint::plain(KeyCode::Char('o')).into(),
                format!(" {toggle_scope} ").dim(),
                "    ".dim(),
                key_hint::plain(KeyCode::Char('d')).into(),
                " manage ".dim(),
                "    ".dim(),
                key_hint::plain(KeyCode::Up).into(),
                "/".dim(),
                key_hint::plain(KeyCode::Down).into(),
                " to browse".dim(),
            ]
            .into()
        };
        frame.render_widget_ref(hint_line, hint);
    })
}

fn render_list(
    frame: &mut crate::custom_terminal::Frame,
    area: Rect,
    state: &PickerState,
    metrics: &ColumnMetrics,
) {
    if area.height == 0 {
        return;
    }

    let rows = &state.filtered_rows;
    if rows.is_empty() {
        let message = render_empty_state_line(state);
        frame.render_widget_ref(message, area);
        return;
    }

    let capacity = area.height as usize;
    let start = state.scroll_top.min(rows.len().saturating_sub(1));
    let end = rows.len().min(start + capacity);
    let labels = &metrics.labels;
    let mut y = area.y;

    let visibility = column_visibility(area.width, metrics, state.sort_key);
    let max_created_width = metrics.max_created_width;
    let max_updated_width = metrics.max_updated_width;
    let max_branch_width = metrics.max_branch_width;
    let max_cwd_width = metrics.max_cwd_width;

    for (idx, (row, (created_label, updated_label, branch_label, cwd_label))) in rows[start..end]
        .iter()
        .zip(labels[start..end].iter())
        .enumerate()
    {
        let is_sel = start + idx == state.selected;
        let marker = if is_sel { "> ".bold() } else { "  ".into() };
        let marker_width = 2usize;
        let created_span = if visibility.show_created {
            Some(Span::from(format!("{created_label:<max_created_width$}")).dim())
        } else {
            None
        };
        let updated_span = if visibility.show_updated {
            Some(Span::from(format!("{updated_label:<max_updated_width$}")).dim())
        } else {
            None
        };
        let branch_span = if !visibility.show_branch {
            None
        } else if branch_label.is_empty() {
            Some(
                Span::from(format!(
                    "{empty:<width$}",
                    empty = "-",
                    width = max_branch_width
                ))
                .dim(),
            )
        } else {
            Some(Span::from(format!("{branch_label:<max_branch_width$}")).cyan())
        };
        let cwd_span = if !visibility.show_cwd {
            None
        } else if cwd_label.is_empty() {
            Some(
                Span::from(format!(
                    "{empty:<width$}",
                    empty = "-",
                    width = max_cwd_width
                ))
                .dim(),
            )
        } else {
            Some(Span::from(format!("{cwd_label:<max_cwd_width$}")).dim())
        };

        let mut preview_width = area.width as usize;
        preview_width = preview_width.saturating_sub(marker_width);
        if visibility.show_created {
            preview_width = preview_width.saturating_sub(max_created_width + 2);
        }
        if visibility.show_updated {
            preview_width = preview_width.saturating_sub(max_updated_width + 2);
        }
        if visibility.show_branch {
            preview_width = preview_width.saturating_sub(max_branch_width + 2);
        }
        if visibility.show_cwd {
            preview_width = preview_width.saturating_sub(max_cwd_width + 2);
        }
        let add_leading_gap = !visibility.show_created
            && !visibility.show_updated
            && !visibility.show_branch
            && !visibility.show_cwd;
        if add_leading_gap {
            preview_width = preview_width.saturating_sub(2);
        }
        let preview = truncate_text(row.display_preview(), preview_width);
        let mut spans: Vec<Span> = vec![marker];
        if let Some(created) = created_span {
            spans.push(created);
            spans.push("  ".into());
        }
        if let Some(updated) = updated_span {
            spans.push(updated);
            spans.push("  ".into());
        }
        if let Some(branch) = branch_span {
            spans.push(branch);
            spans.push("  ".into());
        }
        if let Some(cwd) = cwd_span {
            spans.push(cwd);
            spans.push("  ".into());
        }
        if add_leading_gap {
            spans.push("  ".into());
        }
        spans.push(preview.into());

        let line: Line = spans.into();
        let rect = Rect::new(area.x, y, area.width, 1);
        frame.render_widget_ref(line, rect);
        y = y.saturating_add(1);
    }

    if state.pagination.loading.is_pending() && y < area.y.saturating_add(area.height) {
        let loading_text = match state.view {
            SessionView::Active => "Loading older sessions…",
            SessionView::Archived => "Loading archived sessions…",
        };
        let loading_line: Line = vec!["  ".into(), loading_text.italic().dim()].into();
        let rect = Rect::new(area.x, y, area.width, 1);
        frame.render_widget_ref(loading_line, rect);
    }
}

fn render_empty_state_line(state: &PickerState) -> Line<'static> {
    if !state.query.is_empty() {
        if state.search_state.is_active()
            || (state.pagination.loading.is_pending() && state.pagination.next_cursor.is_some())
        {
            return vec!["Searching…".italic().dim()].into();
        }
        if state.pagination.reached_scan_cap {
            let noun = match state.view {
                SessionView::Active => "sessions",
                SessionView::Archived => "archived sessions",
            };
            let msg = format!(
                "Search scanned first {} {noun}; more may exist",
                state.pagination.num_scanned_files,
            );
            return vec![Span::from(msg).italic().dim()].into();
        }
        return vec!["No results for your search".italic().dim()].into();
    }

    if state.all_rows.is_empty() && state.pagination.num_scanned_files == 0 {
        return match state.view {
            SessionView::Active => vec!["No sessions yet".italic().dim()].into(),
            SessionView::Archived => vec!["No archived sessions yet".italic().dim()].into(),
        };
    }

    if state.pagination.loading.is_pending() {
        return match state.view {
            SessionView::Active => vec!["Loading older sessions…".italic().dim()].into(),
            SessionView::Archived => vec!["Loading archived sessions…".italic().dim()].into(),
        };
    }

    match state.view {
        SessionView::Active => vec!["No sessions yet".italic().dim()].into(),
        SessionView::Archived => vec!["No archived sessions yet".italic().dim()].into(),
    }
}

fn human_time_ago(ts: DateTime<Utc>) -> String {
    let now = Utc::now();
    let delta = now - ts;
    let secs = delta.num_seconds();
    if secs < 60 {
        let n = secs.max(0);
        if n == 1 {
            format!("{n} second ago")
        } else {
            format!("{n} seconds ago")
        }
    } else if secs < 60 * 60 {
        let m = secs / 60;
        if m == 1 {
            format!("{m} minute ago")
        } else {
            format!("{m} minutes ago")
        }
    } else if secs < 60 * 60 * 24 {
        let h = secs / 3600;
        if h == 1 {
            format!("{h} hour ago")
        } else {
            format!("{h} hours ago")
        }
    } else {
        let d = secs / (60 * 60 * 24);
        if d == 1 {
            format!("{d} day ago")
        } else {
            format!("{d} days ago")
        }
    }
}

fn format_updated_label(row: &Row) -> String {
    match (row.updated_at, row.created_at) {
        (Some(updated), _) => human_time_ago(updated),
        (None, Some(created)) => human_time_ago(created),
        (None, None) => "-".to_string(),
    }
}

fn format_created_label(row: &Row) -> String {
    match row.created_at {
        Some(created) => human_time_ago(created),
        None => "-".to_string(),
    }
}

fn render_column_headers(
    frame: &mut crate::custom_terminal::Frame,
    area: Rect,
    metrics: &ColumnMetrics,
    sort_key: ThreadSortKey,
) {
    if area.height == 0 {
        return;
    }

    let mut spans: Vec<Span> = vec!["  ".into()];
    let visibility = column_visibility(area.width, metrics, sort_key);
    if visibility.show_created {
        let label = format!(
            "{text:<width$}",
            text = "Created at",
            width = metrics.max_created_width
        );
        spans.push(Span::from(label).bold());
        spans.push("  ".into());
    }
    if visibility.show_updated {
        let label = format!(
            "{text:<width$}",
            text = "Updated at",
            width = metrics.max_updated_width
        );
        spans.push(Span::from(label).bold());
        spans.push("  ".into());
    }
    if visibility.show_branch {
        let label = format!(
            "{text:<width$}",
            text = "Branch",
            width = metrics.max_branch_width
        );
        spans.push(Span::from(label).bold());
        spans.push("  ".into());
    }
    if visibility.show_cwd {
        let label = format!(
            "{text:<width$}",
            text = "CWD",
            width = metrics.max_cwd_width
        );
        spans.push(Span::from(label).bold());
        spans.push("  ".into());
    }
    spans.push("Conversation".bold());
    frame.render_widget_ref(Line::from(spans), area);
}

/// Pre-computed column widths and formatted labels for all visible rows.
///
/// Widths are measured in Unicode display width (not byte length) so columns
/// align correctly when labels contain non-ASCII characters.
struct ColumnMetrics {
    max_created_width: usize,
    max_updated_width: usize,
    max_branch_width: usize,
    max_cwd_width: usize,
    /// (created_label, updated_label, branch_label, cwd_label) per row.
    labels: Vec<(String, String, String, String)>,
}

/// Determines which columns to render given available terminal width.
///
/// When the terminal is narrow, only one timestamp column is shown (whichever
/// matches the current sort key). Branch and CWD are hidden if their max
/// widths are zero (no data to show).
#[derive(Debug, PartialEq, Eq)]
struct ColumnVisibility {
    show_created: bool,
    show_updated: bool,
    show_branch: bool,
    show_cwd: bool,
}

fn calculate_column_metrics(rows: &[Row], include_cwd: bool) -> ColumnMetrics {
    fn right_elide(s: &str, max: usize) -> String {
        if s.chars().count() <= max {
            return s.to_string();
        }
        if max <= 1 {
            return "…".to_string();
        }
        let tail_len = max - 1;
        let tail: String = s
            .chars()
            .rev()
            .take(tail_len)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("…{tail}")
    }

    let mut labels: Vec<(String, String, String, String)> = Vec::with_capacity(rows.len());
    let mut max_created_width = UnicodeWidthStr::width("Created at");
    let mut max_updated_width = UnicodeWidthStr::width("Updated at");
    let mut max_branch_width = UnicodeWidthStr::width("Branch");
    let mut max_cwd_width = if include_cwd {
        UnicodeWidthStr::width("CWD")
    } else {
        0
    };

    for row in rows {
        let created = format_created_label(row);
        let updated = format_updated_label(row);
        let branch_raw = row.git_branch.clone().unwrap_or_default();
        let branch = right_elide(&branch_raw, 24);
        let cwd = if include_cwd {
            let cwd_raw = row
                .cwd
                .as_ref()
                .map(|p| display_path_for(p, std::path::Path::new("/")))
                .unwrap_or_default();
            right_elide(&cwd_raw, 24)
        } else {
            String::new()
        };
        max_created_width = max_created_width.max(UnicodeWidthStr::width(created.as_str()));
        max_updated_width = max_updated_width.max(UnicodeWidthStr::width(updated.as_str()));
        max_branch_width = max_branch_width.max(UnicodeWidthStr::width(branch.as_str()));
        max_cwd_width = max_cwd_width.max(UnicodeWidthStr::width(cwd.as_str()));
        labels.push((created, updated, branch, cwd));
    }

    ColumnMetrics {
        max_created_width,
        max_updated_width,
        max_branch_width,
        max_cwd_width,
        labels,
    }
}

/// Computes which columns fit in the available width.
///
/// The algorithm reserves at least `MIN_PREVIEW_WIDTH` characters for the
/// conversation preview. If both timestamp columns don't fit, only the one
/// matching the current sort key is shown.
fn column_visibility(
    area_width: u16,
    metrics: &ColumnMetrics,
    sort_key: ThreadSortKey,
) -> ColumnVisibility {
    const MIN_PREVIEW_WIDTH: usize = 10;

    let show_branch = metrics.max_branch_width > 0;
    let show_cwd = metrics.max_cwd_width > 0;

    // Calculate remaining width after all optional columns.
    let mut preview_width = area_width as usize;
    preview_width = preview_width.saturating_sub(2); // marker
    if metrics.max_created_width > 0 {
        preview_width = preview_width.saturating_sub(metrics.max_created_width + 2);
    }
    if metrics.max_updated_width > 0 {
        preview_width = preview_width.saturating_sub(metrics.max_updated_width + 2);
    }
    if show_branch {
        preview_width = preview_width.saturating_sub(metrics.max_branch_width + 2);
    }
    if show_cwd {
        preview_width = preview_width.saturating_sub(metrics.max_cwd_width + 2);
    }

    // If preview would be too narrow, hide the non-active timestamp column.
    let show_both = preview_width >= MIN_PREVIEW_WIDTH;
    let show_created = if show_both {
        metrics.max_created_width > 0
    } else {
        sort_key == ThreadSortKey::CreatedAt
    };
    let show_updated = if show_both {
        metrics.max_updated_width > 0
    } else {
        sort_key == ThreadSortKey::UpdatedAt
    };

    ColumnVisibility {
        show_created,
        show_updated,
        show_branch,
        show_cwd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use chrono::Utc;
    use crossterm::event::KeyModifiers;
    use pretty_assertions::assert_eq;
    use tokio::sync::broadcast;

    fn make_state(
        view: SessionView,
        row_path: PathBuf,
        thread_id: Option<ThreadId>,
        current_session_path: Option<PathBuf>,
    ) -> PickerState {
        let (draw_tx, _draw_rx) = broadcast::channel(1);
        let requester = FrameRequester::new(draw_tx);
        let loader: PageLoader = Arc::new(|_request| {});
        let mut state = PickerState::new(
            PathBuf::from("/tmp/codex-home"),
            requester,
            loader,
            "openai".to_string(),
            true,
            None,
            view,
            SessionPickerExit::Close,
            current_session_path,
        );
        let row = Row {
            path: row_path,
            preview: "preview".to_string(),
            thread_id,
            thread_name: None,
            created_at: None,
            updated_at: None,
            cwd: None,
            git_branch: None,
        };
        state.all_rows = vec![row.clone()];
        state.filtered_rows = vec![row];
        state
    }

    #[tokio::test]
    async fn d_then_enter_emits_archive_management_action_in_active_view() {
        let thread_id = ThreadId::new();
        let row_path = PathBuf::from("/tmp/codex-home/sessions/rollout-a.jsonl");
        let mut state = make_state(SessionView::Active, row_path.clone(), Some(thread_id), None);

        let first = state
            .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .await
            .expect("key handling should succeed");
        assert_eq!(first, None);
        assert!(state.pending_management_confirmation.is_some());

        let second = state
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("key handling should succeed");
        assert_eq!(
            second,
            Some(SessionSelection::Manage(SessionManagementAction {
                kind: SessionManagementActionKind::Archive,
                path: row_path,
                thread_id: Some(thread_id),
                is_current_session: false,
            }))
        );
    }

    #[tokio::test]
    async fn d_then_enter_emits_permanent_delete_management_action_in_archived_view() {
        let thread_id = ThreadId::new();
        let row_path = PathBuf::from("/tmp/codex-home/archived_sessions/rollout-a.jsonl");
        let mut state = make_state(
            SessionView::Archived,
            row_path.clone(),
            Some(thread_id),
            None,
        );

        let first = state
            .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .await
            .expect("key handling should succeed");
        assert_eq!(first, None);
        assert!(state.pending_management_confirmation.is_some());

        let second = state
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("key handling should succeed");
        assert_eq!(
            second,
            Some(SessionSelection::Manage(SessionManagementAction {
                kind: SessionManagementActionKind::DeletePermanent,
                path: row_path,
                thread_id: Some(thread_id),
                is_current_session: false,
            }))
        );
    }

    #[tokio::test]
    async fn current_session_delete_sets_current_session_flag() {
        let thread_id = ThreadId::new();
        let row_path = PathBuf::from("/tmp/codex-home/archived_sessions/rollout-a.jsonl");
        let mut state = make_state(
            SessionView::Archived,
            row_path.clone(),
            Some(thread_id),
            Some(row_path),
        );

        let first = state
            .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .await
            .expect("key handling should succeed");
        assert_eq!(first, None);

        let pending = state
            .pending_management_confirmation
            .as_ref()
            .expect("pending confirmation should be set");
        assert!(pending.action.is_current_session);
        assert!(
            state.confirmation_hint_line().is_some(),
            "expected confirmation hint line to be visible"
        );
    }

    #[tokio::test]
    async fn management_action_uses_thread_id_from_rollout_path() {
        let expected_id = ThreadId::new();
        let wrong_id = ThreadId::new();
        let row_path = PathBuf::from(format!(
            "/tmp/codex-home/sessions/rollout-2025-01-01T00-00-00-{expected_id}.jsonl"
        ));
        let mut state = make_state(SessionView::Active, row_path.clone(), Some(wrong_id), None);

        let first = state
            .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .await
            .expect("key handling should succeed");
        assert_eq!(first, None);

        let second = state
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("key handling should succeed");
        assert_eq!(
            second,
            Some(SessionSelection::Manage(SessionManagementAction {
                kind: SessionManagementActionKind::Archive,
                path: row_path,
                thread_id: Some(expected_id),
                is_current_session: false,
            }))
        );
    }

    #[tokio::test]
    async fn update_thread_names_numbers_multiple_untitled_forks() {
        let (draw_tx, _draw_rx) = broadcast::channel(1);
        let requester = FrameRequester::new(draw_tx);
        let loader: PageLoader = Arc::new(|_request| {});
        let mut state = PickerState::new(
            PathBuf::from("/tmp/codex-home"),
            requester,
            loader,
            "openai".to_string(),
            true,
            None,
            SessionView::Active,
            SessionPickerExit::Close,
            None,
        );

        let parent_id = ThreadId::new();
        let path_a = PathBuf::from(format!(
            "/tmp/codex-home/sessions/rollout-2025-01-01T00-00-00-{}.jsonl",
            ThreadId::new()
        ));
        let path_b = PathBuf::from(format!(
            "/tmp/codex-home/sessions/rollout-2025-01-01T00-00-00-{}.jsonl",
            ThreadId::new()
        ));
        let path_c = PathBuf::from(format!(
            "/tmp/codex-home/sessions/rollout-2025-01-01T00-00-00-{}.jsonl",
            ThreadId::new()
        ));

        let row = |path: PathBuf, preview: &str, created_at: DateTime<Utc>| Row {
            path,
            preview: preview.to_string(),
            thread_id: None,
            thread_name: None,
            created_at: Some(created_at),
            updated_at: None,
            cwd: None,
            git_branch: None,
        };
        state.all_rows = vec![
            row(
                path_c.clone(),
                "third",
                DateTime::parse_from_rfc3339("2025-01-03T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            row(
                path_a.clone(),
                "first",
                DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            row(
                path_b.clone(),
                "second",
                DateTime::parse_from_rfc3339("2025-01-02T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
        ];
        state.filtered_rows = state.all_rows.clone();
        state.fork_parent_id_cache.insert(path_a, Some(parent_id));
        state.fork_parent_id_cache.insert(path_b, Some(parent_id));
        state.fork_parent_id_cache.insert(path_c, Some(parent_id));
        state
            .thread_label_cache
            .insert(parent_id, Some("Parent thread title".to_string()));

        state.update_thread_names().await;

        let names: Vec<String> = state
            .all_rows
            .iter()
            .map(Row::display_preview)
            .map(str::to_string)
            .collect();
        assert_eq!(
            names,
            vec![
                "Fork#3 Parent thread title".to_string(),
                "Fork#1 Parent thread title".to_string(),
                "Fork#2 Parent thread title".to_string(),
            ]
        );
    }
}
