use crate::{
    archive,
    config::{self, Config},
    files::{self, DirectoryRequest, DirectoryResult, Entry},
    operations::{
        self, Conflict, ConflictChoice, Operation, OperationController, OperationEvent,
        OperationResult, TaskProgress, UndoAction,
    },
    preview::{self, Preview, PreviewRequest, PreviewResult},
    search::{self, SearchKind, SearchRequest, SearchResult, SearchView},
    worker::LatestSender,
};
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{layout::Size, widgets::ListState};
use ratatui_image::picker::Picker;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant, SystemTime},
};

static BULK_RENAME_PLAN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    Name,
    Extension,
    Size,
    Modified,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "NAME",
            Self::Extension => "EXTENSION",
            Self::Size => "SIZE",
            Self::Modified => "MODIFIED",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameStatus {
    Ready,
    Unchanged,
    Invalid(String),
    Collision(String),
}

impl RenameStatus {
    pub fn problem(&self) -> Option<&str> {
        match self {
            Self::Invalid(message) | Self::Collision(message) => Some(message),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RenameChange {
    pub source: PathBuf,
    pub target: PathBuf,
    pub old_name: String,
    pub new_name: String,
    pub status: RenameStatus,
}

#[derive(Clone, Debug)]
pub struct BulkRenameReview {
    pub changes: Vec<RenameChange>,
    pub scroll: usize,
}

impl BulkRenameReview {
    pub fn has_errors(&self) -> bool {
        self.changes
            .iter()
            .any(|change| change.status.problem().is_some())
    }

    pub fn ready_changes(&self) -> Vec<(PathBuf, PathBuf)> {
        self.changes
            .iter()
            .filter(|change| matches!(change.status, RenameStatus::Ready))
            .map(|change| (change.source.clone(), change.target.clone()))
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SortValue {
    Name(String, String),
    Extension(String, String, String),
    Size(u64, String, String),
    Modified(SystemTime, String, String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ListingSortKey {
    directory_rank: bool,
    value: SortValue,
    reverse: bool,
}

impl Ord for ListingSortKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.directory_rank
            .cmp(&other.directory_rank)
            .then_with(|| {
                if self.reverse {
                    other.value.cmp(&self.value)
                } else {
                    self.value.cmp(&other.value)
                }
            })
    }
}

impl PartialOrd for ListingSortKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub struct Tab {
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub parent: Vec<Entry>,
    pub visible: Vec<usize>,
    pub selected: usize,
    pub list_state: ListState,
    pub marked: HashSet<PathBuf>,
    pub filter: String,
    pub loading: bool,
    pub error: Option<String>,
    pub search: Option<SearchView>,
    pub sort: SortMode,
    pub sort_reverse: bool,
    pub directories_first: bool,
    preferred: Option<PathBuf>,
    history: HashMap<PathBuf, PathBuf>,
}

impl Tab {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            entries: Vec::new(),
            parent: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            list_state: ListState::default(),
            marked: HashSet::new(),
            filter: String::new(),
            loading: true,
            error: None,
            search: None,
            sort: SortMode::Name,
            sort_reverse: false,
            directories_first: true,
            preferred: None,
            history: HashMap::new(),
        }
    }
    pub fn current(&self) -> Option<&Entry> {
        self.visible
            .get(self.selected)
            .and_then(|&i| self.entries.get(i))
    }
    pub fn refilter(&mut self) {
        let keep = self.current().map(|e| e.path.clone());
        if self.filter.is_empty() {
            self.visible = (0..self.entries.len()).collect();
        } else {
            let matcher = SkimMatcherV2::default().ignore_case();
            let mut matches: Vec<_> = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(i, e)| {
                    matcher
                        .fuzzy_match(&e.name, &self.filter)
                        .map(|score| (i, score))
                })
                .collect();
            matches.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            self.visible = matches.into_iter().map(|(i, _)| i).collect();
        }
        self.selected = keep
            .and_then(|p| self.visible.iter().position(|&i| self.entries[i].path == p))
            .unwrap_or(self.selected)
            .min(self.visible.len().saturating_sub(1));
    }

    pub fn resort(&mut self) {
        let selected = self.current().map(|entry| entry.path.clone());
        let mode = self.sort;
        let reverse = self.sort_reverse;
        let directories_first = self.directories_first;
        self.entries.sort_by_cached_key(|entry| {
            let name = entry.name.to_lowercase();
            let path = entry.path.to_string_lossy().to_lowercase();
            let value = match mode {
                SortMode::Name => SortValue::Name(name, path),
                SortMode::Extension => SortValue::Extension(
                    entry
                        .path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .unwrap_or("")
                        .to_lowercase(),
                    name,
                    path,
                ),
                SortMode::Size => SortValue::Size(entry.size, name, path),
                SortMode::Modified => SortValue::Modified(
                    entry.modified.unwrap_or(SystemTime::UNIX_EPOCH),
                    name,
                    path,
                ),
            };
            ListingSortKey {
                directory_rank: directories_first && !entry.is_dir,
                value,
                reverse,
            }
        });
        self.visible.clear();
        self.selected = 0;
        self.refilter();
        if let Some(path) = selected {
            if let Some(index) = self
                .visible
                .iter()
                .position(|&index| self.entries[index].path == path)
            {
                self.selected = index;
            }
        }
    }
}

#[derive(Clone)]
pub enum InputKind {
    Filter,
    SearchName,
    SearchContents,
    Command,
    Rename(PathBuf),
    Bookmark,
    Archive(Vec<PathBuf>),
}
pub enum Mode {
    Normal,
    Visual { anchor: usize },
    Input(InputKind),
    ConfirmTrash(Vec<PathBuf>),
    Tasks,
    Sort,
    BulkRename(BulkRenameReview),
    UndoHistory,
    Conflict(Conflict),
    Bookmarks { selected: usize },
    Help { page: usize, scroll: usize },
}

impl Mode {
    pub fn label(&self) -> &str {
        match self {
            Self::Visual { .. } => "SELECT",
            Self::Input(InputKind::Rename(_)) => "RENAME",
            Self::Input(InputKind::Archive(_)) => "ARCHIVE",
            Self::Input(InputKind::SearchName | InputKind::SearchContents) => "SEARCH",
            Self::Input(_) => "COMMAND",
            Self::ConfirmTrash(_) => "TRASH",
            Self::Tasks => "TASKS",
            Self::Sort => "SORT",
            Self::BulkRename(_) => "BULK",
            Self::UndoHistory => "UNDO",
            Self::Conflict(_) => "CONFLICT",
            _ => "NORMAL",
        }
    }
}

#[derive(Clone)]
pub struct Clipboard {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}
pub struct Message {
    pub text: String,
    pub error: bool,
    pub until: Instant,
}
pub enum ExternalAction {
    Edit(PathBuf),
    BulkRename {
        plan: PathBuf,
        sources: Vec<PathBuf>,
    },
}

pub struct App {
    pub config: Config,
    pub config_path: PathBuf,
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub mode: Mode,
    pub input: String,
    pub preview: Preview,
    pub preview_pending: bool,
    pub preview_scroll: usize,
    pub preview_size: Size,
    pub zoom: bool,
    pub clipboard: Option<Clipboard>,
    pub message: Option<Message>,
    pub busy: bool,
    pub task: Option<TaskProgress>,
    pub task_history: VecDeque<OperationResult>,
    pub undo_history: VecDeque<UndoAction>,
    pub should_quit: bool,
    pub external: Option<ExternalAction>,
    pub chooser: bool,
    pub chosen: Option<Vec<PathBuf>>,
    pub bookmarks: BTreeMap<String, PathBuf>,
    pub protocol_name: String,
    pub dirty: bool,
    pub pending_keys: String,
    pub list_height: usize,
    pub help_scroll_max: usize,
    pub help_view_height: usize,
    filter_backup: String,
    pending_since: Instant,
    bindings: BTreeMap<String, Vec<String>>,
    directory_generation: u64,
    preview_generation: u64,
    search_generation: u64,
    directory_tx: LatestSender<DirectoryRequest>,
    directory_rx: Receiver<DirectoryResult>,
    preview_tx: LatestSender<PreviewRequest>,
    preview_rx: Receiver<PreviewResult>,
    search_tx: LatestSender<SearchRequest>,
    search_rx: Receiver<SearchResult>,
    operation: OperationController,
    operation_rx: Receiver<OperationEvent>,
    task_id: Option<u64>,
    undo_in_flight: Option<UndoAction>,
    watcher: Option<RecommendedWatcher>,
    watch_rx: Receiver<notify::Result<notify::Event>>,
    watched: Vec<PathBuf>,
    refresh_at: Option<Instant>,
    refresh_preview_at: Option<Instant>,
    force_preview_refresh: bool,
    picker: Picker,
}

impl App {
    pub fn new(cwd: PathBuf, config: Config, config_path: PathBuf, picker: Picker) -> Result<Self> {
        let cwd = std::fs::canonicalize(cwd).context("Cannot access starting directory")?;
        if !cwd.is_dir() {
            anyhow::bail!("Starting path must be a directory");
        }
        let (directory_tx, directory_rx) = files::spawn_reader();
        let (preview_tx, preview_rx) = preview::spawn_previewer(picker.clone(), config.clone());
        let (search_tx, search_rx) = search::spawn_searcher();
        let (operation, operation_rx) = operations::spawn_worker();
        let (watch_tx, watch_rx) = mpsc::channel();
        let watcher_result = notify::recommended_watcher(move |event| {
            let _ = watch_tx.send(event);
        });
        let watcher_error = watcher_result.as_ref().err().map(ToString::to_string);
        let mut app = Self {
            bookmarks: config.bookmarks(&config_path),
            bindings: config.bindings(),
            config,
            config_path,
            tabs: vec![Tab::new(cwd)],
            active: 0,
            mode: Mode::Normal,
            input: String::new(),
            preview: Preview::Empty,
            preview_pending: false,
            preview_scroll: 0,
            preview_size: Size::new(40, 20),
            zoom: false,
            clipboard: None,
            message: None,
            busy: false,
            task: None,
            task_history: VecDeque::new(),
            undo_history: VecDeque::new(),
            should_quit: false,
            external: None,
            chooser: false,
            chosen: None,
            protocol_name: format!("{:?}", picker.protocol_type()).to_lowercase(),
            dirty: true,
            pending_keys: String::new(),
            list_height: 20,
            help_scroll_max: 0,
            help_view_height: 10,
            filter_backup: String::new(),
            pending_since: Instant::now(),
            directory_generation: 0,
            preview_generation: 0,
            search_generation: 0,
            directory_tx,
            directory_rx,
            preview_tx,
            preview_rx,
            search_tx,
            search_rx,
            operation,
            operation_rx,
            task_id: None,
            undo_in_flight: None,
            watcher: watcher_result.ok(),
            watch_rx,
            watched: Vec::new(),
            refresh_at: None,
            refresh_preview_at: None,
            force_preview_refresh: false,
            picker,
        };
        app.refresh();
        app.update_watches();
        if let Some(e) = watcher_error {
            app.notice(
                format!("Live refresh unavailable: {e}. Press R to refresh."),
                true,
            );
        }
        Ok(app)
    }

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }
    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }
    pub fn notice(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some(Message {
            text: files::safe_text(&text.into()),
            error,
            until: Instant::now() + Duration::from_secs(if error { 15 } else { 5 }),
        });
        self.dirty = true;
    }

    pub fn enable_chooser(&mut self) {
        self.chooser = true;
        self.notice("Chooser mode · select files and press Enter", false);
    }

    pub fn refresh(&mut self) {
        if let Some(search) = self.tab().search.clone() {
            self.start_search(search.kind, search.query);
            return;
        }
        self.directory_generation += 1;
        let req = DirectoryRequest {
            generation: self.directory_generation,
            path: self.tab().cwd.clone(),
            hidden: self.config.show_hidden,
        };
        let tab = self.tab_mut();
        if let Some(entry) = tab.current() {
            tab.preferred = Some(entry.path.clone());
        }
        tab.loading = true;
        self.directory_tx.send(req);
        self.dirty = true;
    }

    pub fn start_search(&mut self, kind: SearchKind, query: String) {
        let query = query.trim().to_owned();
        if query.is_empty() {
            self.notice("Enter something to search for", true);
            return;
        }
        self.search_generation += 1;
        let request = SearchRequest {
            generation: self.search_generation,
            root: self.tab().cwd.clone(),
            query: query.clone(),
            kind,
            hidden: self.config.show_hidden,
        };
        self.preview_tx.cancel();
        self.preview_generation += 1;
        self.preview_pending = false;
        self.preview = Preview::Empty;
        let tab = self.tab_mut();
        tab.search = Some(SearchView {
            root: request.root.clone(),
            query,
            kind,
            scanned: 0,
            truncated: false,
        });
        tab.entries.clear();
        tab.visible.clear();
        tab.selected = 0;
        tab.marked.clear();
        tab.filter.clear();
        tab.loading = true;
        tab.error = None;
        self.search_tx.send(request);
        self.dirty = true;
    }

    fn exit_search(&mut self) {
        self.search_tx.cancel();
        self.tab_mut().search = None;
        self.tab_mut().filter.clear();
        self.refresh();
    }

    pub fn request_preview(&mut self, reset: bool) {
        self.preview_generation += 1;
        if reset {
            self.preview_scroll = 0;
            self.preview = Preview::Loading;
        }
        let preload = self.preview_preload_entries();
        if let Some(entry) = self.tab().current().cloned() {
            self.preview_pending = true;
            self.preview_tx.send(PreviewRequest {
                generation: self.preview_generation,
                entry,
                size: self.preview_size,
                scroll: self.preview_scroll,
                zoom: self.zoom,
                hidden: self.config.show_hidden,
                force: std::mem::take(&mut self.force_preview_refresh),
                preload,
            });
        } else {
            self.preview_tx.cancel();
            self.preview = Preview::Empty;
            self.preview_pending = false;
        }
        if reset {
            self.update_watches();
        }
        self.dirty = true;
    }

    fn preview_preload_entries(&self) -> Vec<Entry> {
        let tab = self.tab();
        let selected = tab.selected;
        let mut preload = Vec::new();
        for distance in 1..=4 {
            for visible in [
                selected.checked_add(distance),
                selected.checked_sub(distance),
            ]
            .into_iter()
            .flatten()
            {
                let Some(entry) = tab
                    .visible
                    .get(visible)
                    .and_then(|&index| tab.entries.get(index))
                else {
                    continue;
                };
                if !entry.is_dir && crate::media::kind(&entry.path, &entry.mime).is_none() {
                    preload.push(entry.clone());
                    if preload.len() == 4 {
                        return preload;
                    }
                }
            }
        }
        preload
    }

    pub fn set_preview_size(&mut self, size: Size) {
        let size = Size::new(size.width.max(1), size.height.max(1));
        if self.preview_size != size {
            self.preview_size = size;
            if matches!(
                self.preview,
                Preview::Image { .. } | Preview::Media { .. } | Preview::Loading
            ) {
                self.request_preview(false);
            } else {
                self.preview_scroll = self
                    .preview_scroll
                    .min(self.preview.max_scroll(size.height as usize));
            }
            self.dirty = true;
        }
    }

    pub fn poll(&mut self) {
        while let Ok(result) = self.directory_rx.try_recv() {
            if result.generation != self.directory_generation || result.path != self.tab().cwd {
                continue;
            }
            let previous = self.tab().current().cloned();
            let previous_path = previous.as_ref().map(|e| e.path.clone());
            let tab = self.tab_mut();
            tab.loading = false;
            tab.parent = result.parent;
            match result.entries {
                Ok(entries) => {
                    tab.entries = entries;
                    tab.error = None;
                    tab.visible.clear();
                    tab.resort();
                    if let Some(path) = previous_path.clone().or(tab.preferred.take()) {
                        if let Some(index) = tab
                            .visible
                            .iter()
                            .position(|&i| tab.entries[i].path == path)
                        {
                            tab.selected = index;
                        }
                    }
                    let existing: HashSet<_> = tab.entries.iter().map(|e| &e.path).collect();
                    tab.marked.retain(|p| existing.contains(p));
                }
                Err(e) => {
                    tab.entries.clear();
                    tab.visible.clear();
                    tab.selected = 0;
                    tab.error = Some(format!("{e:#}"));
                }
            }
            let current = self.tab().current();
            if previous.as_ref() != current || self.force_preview_refresh {
                let reset = previous_path.as_ref() != current.map(|e| &e.path);
                self.request_preview(reset);
                self.force_preview_refresh = false;
            }
        }
        while let Ok(result) = self.search_rx.try_recv() {
            let current = self.tab().search.as_ref();
            if result.generation != self.search_generation
                || result.root != self.tab().cwd
                || !current.is_some_and(|search| {
                    search.kind == result.kind && search.query == result.query
                })
            {
                continue;
            }
            let tab = self.tab_mut();
            tab.loading = false;
            if let Some(search) = &mut tab.search {
                search.scanned = result.scanned;
                search.truncated = result.truncated;
            }
            match result.entries {
                Ok(entries) => {
                    tab.entries = entries;
                    tab.error = None;
                    tab.selected = 0;
                    tab.visible.clear();
                    tab.resort();
                }
                Err(error) => {
                    tab.entries.clear();
                    tab.visible.clear();
                    tab.selected = 0;
                    tab.error = Some(error);
                }
            }
            self.request_preview(true);
        }
        while let Ok(result) = self.preview_rx.try_recv() {
            if result.generation != self.preview_generation {
                continue;
            }
            self.preview = result.preview;
            self.preview_pending = false;
            self.preview_scroll = match &self.preview {
                Preview::Image { offset, .. } => *offset,
                other => self
                    .preview_scroll
                    .min(other.max_scroll(self.preview_size.height as usize)),
            };
            self.dirty = true;
        }
        while let Ok(event) = self.operation_rx.try_recv() {
            match event {
                OperationEvent::Started(progress) | OperationEvent::Progress(progress) => {
                    self.task_id = Some(progress.task_id);
                    self.task = Some(progress);
                    self.busy = true;
                    self.dirty = true;
                }
                OperationEvent::Conflict(conflict) => {
                    self.mode = Mode::Conflict(conflict);
                    self.dirty = true;
                }
                OperationEvent::Finished(result) => {
                    self.busy = false;
                    self.task_id = None;
                    self.task = None;
                    if let Some(action) = self.undo_in_flight.take() {
                        if result.error || result.cancelled {
                            self.undo_history.push_front(action);
                        }
                    }
                    if let Some(action) = result.undo.clone() {
                        self.undo_history.push_front(action);
                        self.undo_history.truncate(20);
                    }
                    if self.clipboard.as_ref().is_some_and(|clipboard| {
                        clipboard.cut && result.moved_sources.as_ref() == Some(&clipboard.paths)
                    }) {
                        self.clipboard = None;
                    }
                    if matches!(self.mode, Mode::Conflict(_)) {
                        self.mode = Mode::Tasks;
                    }
                    self.task_history.push_front(result.clone());
                    self.task_history.truncate(8);
                    let refresh = result.refresh;
                    self.notice(result.message, result.error);
                    if refresh {
                        self.refresh();
                    }
                }
            }
        }
        while let Ok(event) = self.watch_rx.try_recv() {
            match event {
                Ok(event) if !matches!(event.kind, notify::EventKind::Access(_)) => {
                    let (listing, preview) =
                        relevant_change(&event, &self.tab().cwd, self.tab().current());
                    let deadline = Instant::now() + Duration::from_millis(150);
                    if listing {
                        self.refresh_at.get_or_insert(deadline);
                    }
                    if preview {
                        // A watcher event is stronger evidence than the file
                        // stamp alone. Some filesystems keep coarse mtimes,
                        // so invalidate the selected preview cache entry.
                        self.force_preview_refresh = true;
                        self.refresh_preview_at.get_or_insert(deadline);
                    }
                }
                Err(e) => self.notice(format!("Watcher: {e}"), true),
                _ => {}
            }
        }
        if self.refresh_at.is_some_and(|time| Instant::now() >= time) {
            self.refresh_at = None;
            self.refresh();
        }
        if self
            .refresh_preview_at
            .is_some_and(|time| Instant::now() >= time)
        {
            self.refresh_preview_at = None;
            self.request_preview(false);
        }
        if self
            .message
            .as_ref()
            .is_some_and(|m| Instant::now() >= m.until)
        {
            self.message = None;
            self.dirty = true;
        }
        if !self.pending_keys.is_empty()
            && self.pending_since.elapsed() > Duration::from_millis(900)
        {
            self.pending_keys.clear();
            self.dirty = true;
        }
    }

    fn update_watches(&mut self) {
        let cwd = self.tab().cwd.clone();
        let mut paths = vec![cwd.clone()];
        if let Some(parent) = cwd.parent() {
            paths.push(parent.to_path_buf());
        }
        if let Some(entry) = self.tab().current().filter(|e| e.is_dir) {
            paths.push(entry.path.clone());
        }
        if paths == self.watched {
            return;
        }
        let mut errors = Vec::new();
        if let Some(watcher) = &mut self.watcher {
            for path in self.watched.drain(..) {
                let _ = watcher.unwatch(&path);
            }
            for path in paths {
                match watcher.watch(&path, RecursiveMode::NonRecursive) {
                    Ok(()) => self.watched.push(path),
                    Err(e) => errors.push(e.to_string()),
                }
            }
        }
        if !errors.is_empty() {
            self.notice(format!("Live refresh: {}", errors.join("; ")), true);
        }
    }

    pub fn navigate(&mut self, path: PathBuf) {
        self.preview_tx.cancel();
        self.search_tx.cancel();
        self.preview_pending = false;
        self.refresh_at = None;
        self.refresh_preview_at = None;
        let tab = self.tab_mut();
        if let Some(entry) = tab.current() {
            tab.history.insert(tab.cwd.clone(), entry.path.clone());
        }
        let previous = tab.cwd.clone();
        tab.preferred = if previous.parent() == Some(path.as_path()) {
            Some(previous)
        } else {
            tab.history.get(&path).cloned()
        };
        tab.cwd = path;
        tab.entries.clear();
        tab.visible.clear();
        tab.parent.clear();
        tab.selected = 0;
        tab.list_state = ListState::default();
        tab.marked.clear();
        tab.filter.clear();
        tab.error = None;
        tab.search = None;
        self.mode = Mode::Normal;
        self.preview = Preview::Empty;
        self.preview_generation += 1;
        self.refresh();
        self.update_watches();
    }

    fn move_cursor(&mut self, delta: isize) {
        let tab = self.tab_mut();
        let previous = tab.selected;
        tab.selected = tab
            .selected
            .saturating_add_signed(delta)
            .min(tab.visible.len().saturating_sub(1));
        if previous != tab.selected {
            if let Mode::Visual { anchor } = self.mode {
                let tab = self.tab_mut();
                tab.marked.clear();
                let start = anchor.min(tab.selected).min(tab.visible.len());
                let end = anchor
                    .max(tab.selected)
                    .saturating_add(1)
                    .min(tab.visible.len());
                for &i in &tab.visible[start..end] {
                    tab.marked.insert(tab.entries[i].path.clone());
                }
            }
            self.request_preview(true);
        }
    }

    pub fn targets(&self) -> Vec<PathBuf> {
        let tab = self.tab();
        if tab.marked.is_empty() {
            tab.current()
                .map(|e| vec![e.path.clone()])
                .unwrap_or_default()
        } else {
            let mut paths = Vec::with_capacity(tab.marked.len());
            let mut seen = HashSet::with_capacity(tab.marked.len());
            for entry in tab
                .visible
                .iter()
                .filter_map(|&index| tab.entries.get(index))
                .chain(tab.entries.iter())
            {
                if tab.marked.contains(&entry.path) && seen.insert(entry.path.clone()) {
                    paths.push(entry.path.clone());
                }
            }
            paths
        }
    }

    fn choose_files(&mut self) {
        if self.busy {
            self.notice("Wait for the active operation before choosing files", true);
            return;
        }
        let targets: HashSet<_> = self.targets().into_iter().collect();
        let chosen = self
            .tab()
            .entries
            .iter()
            .filter(|entry| !entry.is_dir && targets.contains(&entry.path))
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        if chosen.is_empty() {
            self.notice("Select at least one file; folders cannot be returned", true);
            return;
        }
        self.chosen = Some(chosen);
        self.should_quit = true;
    }

    fn set_sort(&mut self, mode: SortMode) {
        self.tab_mut().sort = mode;
        self.tab_mut().resort();
        self.request_preview(false);
    }

    fn prepare_bulk_rename(&mut self, sources: Vec<PathBuf>, names: Vec<String>) {
        if sources.is_empty() {
            self.notice("Select at least one item to rename", true);
            return;
        }
        if sources.len() != names.len() {
            self.notice(
                format!(
                    "Bulk rename needs exactly {} line(s); the editor returned {}",
                    sources.len(),
                    names.len()
                ),
                true,
            );
            return;
        }
        let plan = std::env::temp_dir().join(format!(
            "zuru-bulk-rename-{}-{}.txt",
            std::process::id(),
            BULK_RENAME_PLAN_ID.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        let mut contents = names.join("\n");
        contents.push('\n');
        match fs::write(&plan, contents) {
            Ok(()) => {
                self.external = Some(ExternalAction::BulkRename { plan, sources });
                self.notice(
                    "Edit one filename per line, then save and close the editor",
                    false,
                );
            }
            Err(error) => self.notice(format!("Could not create rename plan: {error}"), true),
        }
    }

    fn start_bulk_rename(&mut self) {
        let sources = self.targets();
        let mut names = Vec::with_capacity(sources.len());
        for source in &sources {
            let Some(name) = source.file_name().and_then(|name| name.to_str()) else {
                self.notice(
                    "Bulk rename requires filenames that can be represented as text",
                    true,
                );
                return;
            };
            if name.chars().any(char::is_control) {
                self.notice(
                    "Bulk rename cannot edit filenames containing control characters",
                    true,
                );
                return;
            }
            names.push(name.to_owned());
        }
        self.prepare_bulk_rename(sources, names);
    }

    pub fn review_bulk_rename(&mut self, sources: Vec<PathBuf>, edited: &str) {
        let normalized = edited.replace("\r\n", "\n");
        let mut names: Vec<String> = normalized.split('\n').map(str::to_owned).collect();
        if normalized.ends_with('\n') && names.last().is_some_and(String::is_empty) {
            names.pop();
        }
        if names.len() != sources.len() {
            self.notice(
                format!(
                    "Bulk rename needs exactly {} line(s); the editor returned {}. Press B to try again.",
                    sources.len(),
                    names.len()
                ),
                true,
            );
            return;
        }

        let source_keys: HashSet<_> = sources.iter().map(|path| rename_path_key(path)).collect();
        let mut changes = sources
            .into_iter()
            .zip(names)
            .map(|(source, new_name)| {
                let old_name = source
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_owned();
                let parent = source.parent().unwrap_or(Path::new(""));
                let target = parent.join(&new_name);
                let status = match operations::validate_name(&new_name) {
                    Err(error) => RenameStatus::Invalid(format!("{error:#}")),
                    Ok(()) if source == target => RenameStatus::Unchanged,
                    Ok(()) => RenameStatus::Ready,
                };
                RenameChange {
                    source,
                    target,
                    old_name,
                    new_name,
                    status,
                }
            })
            .collect::<Vec<_>>();

        let mut target_counts = HashMap::new();
        for change in &changes {
            if change.status.problem().is_none() {
                *target_counts
                    .entry(rename_path_key(&change.target))
                    .or_insert(0usize) += 1;
            }
        }
        for change in &mut changes {
            if change.status.problem().is_some() {
                continue;
            }
            let key = rename_path_key(&change.target);
            if target_counts.get(&key).copied().unwrap_or(0) > 1 {
                change.status = RenameStatus::Collision("another line uses this name".into());
            } else if !source_keys.contains(&key) {
                match fs::symlink_metadata(&change.target) {
                    Ok(_) => {
                        change.status =
                            RenameStatus::Collision("an item with this name already exists".into())
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        change.status = RenameStatus::Collision(format!(
                            "cannot verify this destination: {error}"
                        ))
                    }
                }
            }
        }
        self.mode = Mode::BulkRename(BulkRenameReview { changes, scroll: 0 });
        self.dirty = true;
    }

    fn submit(&mut self, operation: Operation) {
        if self.busy {
            self.notice("An operation is still running", true);
            return;
        }
        let undoing = match &operation {
            Operation::Undo(action) => Some(action.clone()),
            _ => None,
        };
        match self.operation.submit(operation) {
            Ok(task_id) => {
                if let Some(action) = undoing {
                    self.undo_history.pop_front();
                    self.undo_in_flight = Some(action);
                }
                self.task_id = Some(task_id);
                self.busy = true;
                self.mode = Mode::Normal;
                self.notice("Task started · w shows progress", false);
            }
            Err(error) => self.notice(format!("{error:#}"), true),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.dirty = true;
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.busy {
                self.notice("Wait for the active operation before quitting", true);
            } else {
                self.should_quit = true;
            }
            return;
        }
        match &mut self.mode {
            Mode::Input(_) => {
                self.handle_input(key);
                return;
            }
            Mode::ConfirmTrash(paths) => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Char('y' | 'd')) {
                    let paths = paths.clone();
                    self.submit(Operation::Trash(paths));
                } else if matches!(key.code, KeyCode::Esc | KeyCode::Char('n')) {
                    self.mode = Mode::Normal;
                }
                return;
            }
            Mode::Tasks => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q' | 'w') => self.mode = Mode::Normal,
                    KeyCode::Char('c') => {
                        if let Some(task_id) = self.task_id {
                            self.operation.cancel(task_id);
                            self.notice("Cancellation requested", false);
                        }
                    }
                    _ => {}
                }
                return;
            }
            Mode::UndoHistory => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q' | 'u') => self.mode = Mode::Normal,
                    KeyCode::Enter => {
                        if let Some(action) = self.undo_history.front().cloned() {
                            self.submit(Operation::Undo(action));
                        } else {
                            self.mode = Mode::Normal;
                            self.notice("There is nothing to undo", false);
                        }
                    }
                    _ => {}
                }
                return;
            }
            Mode::Sort => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q' | ',') => self.mode = Mode::Normal,
                    KeyCode::Char('n') => self.set_sort(SortMode::Name),
                    KeyCode::Char('e') => self.set_sort(SortMode::Extension),
                    KeyCode::Char('s') => self.set_sort(SortMode::Size),
                    KeyCode::Char('m') => self.set_sort(SortMode::Modified),
                    KeyCode::Char('r') => {
                        let reverse = !self.tab().sort_reverse;
                        self.tab_mut().sort_reverse = reverse;
                        self.tab_mut().resort();
                    }
                    KeyCode::Char('d') => {
                        let directories_first = !self.tab().directories_first;
                        self.tab_mut().directories_first = directories_first;
                        self.tab_mut().resort();
                    }
                    _ => {}
                }
                return;
            }
            Mode::BulkRename(review) => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
                    KeyCode::Down | KeyCode::Char('j') => {
                        review.scroll = review
                            .scroll
                            .saturating_add(1)
                            .min(review.changes.len().saturating_sub(1));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        review.scroll = review.scroll.saturating_sub(1)
                    }
                    KeyCode::PageDown => {
                        review.scroll = review
                            .scroll
                            .saturating_add(8)
                            .min(review.changes.len().saturating_sub(1));
                    }
                    KeyCode::PageUp => review.scroll = review.scroll.saturating_sub(8),
                    KeyCode::Home => review.scroll = 0,
                    KeyCode::End => review.scroll = review.changes.len().saturating_sub(1),
                    KeyCode::Char('e') => {
                        let sources = review
                            .changes
                            .iter()
                            .map(|change| change.source.clone())
                            .collect();
                        let names = review
                            .changes
                            .iter()
                            .map(|change| change.new_name.clone())
                            .collect();
                        self.prepare_bulk_rename(sources, names);
                    }
                    KeyCode::Enter | KeyCode::Char('a') => {
                        if review.has_errors() {
                            self.notice("Fix the marked rename problems before applying", true);
                        } else {
                            let changes = review.ready_changes();
                            if changes.is_empty() {
                                self.mode = Mode::Normal;
                                self.notice("No filenames changed", false);
                            } else {
                                self.tab_mut().marked.clear();
                                self.submit(Operation::BulkRename(changes));
                            }
                        }
                    }
                    _ => {}
                }
                return;
            }
            Mode::Conflict(conflict) => {
                let choice = match key.code {
                    KeyCode::Char('s') => Some(ConflictChoice::Skip),
                    KeyCode::Char('k') => Some(ConflictChoice::KeepBoth),
                    KeyCode::Char('r') => Some(ConflictChoice::Replace),
                    KeyCode::Char('S') => Some(ConflictChoice::SkipAll),
                    KeyCode::Char('K') => Some(ConflictChoice::KeepBothAll),
                    KeyCode::Char('R') => Some(ConflictChoice::ReplaceAll),
                    KeyCode::Esc | KeyCode::Char('c') => Some(ConflictChoice::Cancel),
                    _ => None,
                };
                if let Some(choice) = choice {
                    let task_id = conflict.task_id;
                    if let Err(error) = self.operation.resolve(task_id, choice) {
                        self.notice(format!("{error:#}"), true);
                    }
                    self.mode = Mode::Tasks;
                }
                return;
            }
            Mode::Help { page, scroll } => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q' | '?') | KeyCode::F(1) => {
                        self.mode = Mode::Normal
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *scroll = scroll.saturating_add(1).min(self.help_scroll_max)
                    }
                    KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown => {
                        *scroll = scroll
                            .saturating_add(self.help_view_height)
                            .min(self.help_scroll_max)
                    }
                    KeyCode::PageUp => *scroll = scroll.saturating_sub(self.help_view_height),
                    KeyCode::Home => *scroll = 0,
                    KeyCode::End => *scroll = self.help_scroll_max,
                    KeyCode::Tab | KeyCode::Right => {
                        *page = (*page + 1) % crate::help::PAGES.len();
                        *scroll = 0;
                    }
                    KeyCode::BackTab | KeyCode::Left => {
                        *page = (*page + crate::help::PAGES.len() - 1) % crate::help::PAGES.len();
                        *scroll = 0;
                    }
                    KeyCode::Char(c @ '1'..='5') => {
                        *page = c as usize - '1' as usize;
                        *scroll = 0;
                    }
                    _ => {}
                }
                return;
            }
            Mode::Bookmarks { selected } => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q' | 'b') => self.mode = Mode::Normal,
                    KeyCode::Down | KeyCode::Char('j') => {
                        *selected = (*selected + 1).min(self.bookmarks.len().saturating_sub(1))
                    }
                    KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                    KeyCode::Char('m') => {
                        self.input.clear();
                        self.mode = Mode::Input(InputKind::Bookmark);
                    }
                    KeyCode::Enter | KeyCode::Char('l') => {
                        if let Some(path) = self.bookmarks.values().nth(*selected).cloned() {
                            self.navigate(path);
                        }
                    }
                    _ => {}
                }
                return;
            }
            _ => {}
        }
        let name = config::key_name(key);
        if name.is_empty() {
            return;
        }
        let combined = if self.pending_keys.is_empty() {
            name.clone()
        } else {
            format!("{} {name}", self.pending_keys)
        };
        self.pending_keys.clear();
        let mut action = None;
        for candidate in [&combined, &name] {
            if let Some((found, _)) = self
                .bindings
                .iter()
                .find(|(_, keys)| keys.contains(candidate))
            {
                action = Some(found.clone());
                break;
            }
            if self
                .bindings
                .values()
                .flatten()
                .any(|k| k.starts_with(&format!("{candidate} ")))
            {
                self.pending_keys = candidate.clone();
                self.pending_since = Instant::now();
                return;
            }
        }
        if let Some(action) = action {
            self.action(&action);
        } else if let Ok(number) = name.parse::<usize>() {
            if (1..=self.tabs.len()).contains(&number) {
                self.switch_tab(number - 1);
            }
        }
    }

    pub fn action(&mut self, action: &str) {
        match action {
            "quit" => {
                if self.busy {
                    self.notice("A task is active · press w, then c to cancel", true);
                } else {
                    self.should_quit = true;
                }
            }
            "up" => self.move_cursor(-1),
            "down" => self.move_cursor(1),
            "first" => self.move_cursor(-(self.tab().selected as isize)),
            "last" => self.move_cursor(self.tab().visible.len() as isize),
            "page_up" => self.move_cursor(-(self.list_height as isize)),
            "page_down" => self.move_cursor(self.list_height as isize),
            "parent" => {
                if self.tab().search.is_some() {
                    self.exit_search();
                } else if let Some(p) = self.tab().cwd.parent() {
                    self.navigate(p.to_path_buf());
                }
            }
            "enter" | "open" | "edit" => {
                if let Some(entry) = self.tab().current().cloned() {
                    if entry.is_dir {
                        self.navigate(entry.path);
                    } else if action == "enter" && self.chooser {
                        self.choose_files();
                    } else {
                        if action == "edit" {
                            self.external = Some(ExternalAction::Edit(entry.path));
                        } else {
                            self.submit(Operation::Open(entry.path));
                        }
                    }
                }
            }
            "copy" | "cut" => {
                let paths = self.targets();
                if !paths.is_empty() {
                    self.notice(
                        format!(
                            "{} {} item(s) · p to paste",
                            if action == "cut" { "Cut" } else { "Yanked" },
                            paths.len()
                        ),
                        false,
                    );
                    self.clipboard = Some(Clipboard {
                        paths,
                        cut: action == "cut",
                    });
                    self.mode = Mode::Normal;
                    self.tab_mut().marked.clear();
                }
            }
            "paste" => {
                if let Some(clip) = self.clipboard.clone() {
                    self.submit(Operation::Paste {
                        sources: clip.paths,
                        destination: self.tab().cwd.clone(),
                        cut: clip.cut,
                    });
                } else {
                    self.notice("Clipboard is empty · y to copy, x to cut", false);
                }
            }
            "trash" => {
                let targets = self.targets();
                if !targets.is_empty() {
                    self.mode = Mode::ConfirmTrash(targets);
                } else if self.tab().loading {
                    self.notice(
                        "The folder is still loading; try d again in a moment",
                        false,
                    );
                } else {
                    self.notice("There is nothing here to send to trash", false);
                }
            }
            "rename" => {
                if let Some(entry) = self.tab().current().cloned() {
                    self.input = entry
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into();
                    self.mode = Mode::Input(InputKind::Rename(entry.path));
                }
            }
            "bulk_rename" => self.start_bulk_rename(),
            "create_archive" => {
                let sources = self.targets();
                if !sources.is_empty() {
                    self.input = if sources.len() == 1 {
                        sources[0]
                            .file_stem()
                            .and_then(|name| name.to_str())
                            .filter(|name| !name.is_empty())
                            .map(|name| format!("{name}.zip"))
                            .unwrap_or_else(|| "archive.zip".into())
                    } else {
                        "archive.zip".into()
                    };
                    self.mode = Mode::Input(InputKind::Archive(sources));
                }
            }
            "extract" => {
                let archives = self.targets();
                if archives.is_empty() {
                    self.notice("Select at least one archive to extract", true);
                } else if let Some(path) =
                    archives.iter().find(|path| archive::format(path).is_none())
                {
                    self.notice(format!("Unsupported archive: {}", path.display()), true);
                } else {
                    self.submit(Operation::ExtractArchives {
                        archives,
                        destination: self.tab().cwd.clone(),
                    });
                }
            }
            "sort" => self.mode = Mode::Sort,
            "visual" => {
                if matches!(self.mode, Mode::Visual { .. }) {
                    self.mode = Mode::Normal;
                } else {
                    let anchor = self.tab().selected;
                    self.mode = Mode::Visual { anchor };
                    if let Some(entry) = self.tab().current() {
                        let path = entry.path.clone();
                        self.tab_mut().marked.insert(path);
                    }
                }
            }
            "toggle" => {
                if let Some(entry) = self.tab().current() {
                    let path = entry.path.clone();
                    if !self.tab_mut().marked.remove(&path) {
                        self.tab_mut().marked.insert(path);
                    }
                }
                self.mode = Mode::Normal;
                self.move_cursor(1);
            }
            "select_all" => {
                let paths = self
                    .tab()
                    .visible
                    .iter()
                    .map(|&i| self.tab().entries[i].path.clone())
                    .collect();
                self.tab_mut().marked = paths;
            }
            "filter" => {
                self.filter_backup = self.tab().filter.clone();
                self.input = self.tab().filter.clone();
                self.mode = Mode::Input(InputKind::Filter);
            }
            "search_name" => {
                self.input.clear();
                self.mode = Mode::Input(InputKind::SearchName);
            }
            "search_contents" => {
                self.input.clear();
                self.mode = Mode::Input(InputKind::SearchContents);
            }
            "command" => {
                self.input.clear();
                self.mode = Mode::Input(InputKind::Command);
            }
            "bookmark" => {
                self.input.clear();
                self.mode = Mode::Input(InputKind::Bookmark);
            }
            "bookmarks" => self.mode = Mode::Bookmarks { selected: 0 },
            "tasks" => {
                self.mode = if matches!(self.mode, Mode::Tasks) {
                    Mode::Normal
                } else {
                    Mode::Tasks
                }
            }
            "undo" => {
                self.mode = if matches!(self.mode, Mode::UndoHistory) {
                    Mode::Normal
                } else {
                    Mode::UndoHistory
                }
            }
            "home_dir" => {
                if let Some(dirs) = directories::UserDirs::new() {
                    self.navigate(dirs.home_dir().into());
                }
            }
            "downloads" => {
                if let Some(path) = self.bookmarks.get("downloads").cloned() {
                    self.navigate(path);
                }
            }
            "hidden" => {
                self.config.show_hidden = !self.config.show_hidden;
                if self.tab().current().is_some_and(|entry| entry.is_dir) {
                    self.force_preview_refresh = true;
                }
                self.refresh();
            }
            "refresh" => {
                self.force_preview_refresh = true;
                self.refresh();
            }
            "preview_down" => self.scroll_preview(3),
            "preview_up" => self.scroll_preview(-3),
            "zoom" => {
                self.zoom = !self.zoom;
                self.preview_scroll = 0;
                self.request_preview(false);
            }
            "new_tab" => {
                let current = self.tab();
                let (cwd, sort, sort_reverse, directories_first) = (
                    current.cwd.clone(),
                    current.sort,
                    current.sort_reverse,
                    current.directories_first,
                );
                let mut tab = Tab::new(cwd);
                tab.sort = sort;
                tab.sort_reverse = sort_reverse;
                tab.directories_first = directories_first;
                self.tabs.push(tab);
                self.switch_tab(self.tabs.len() - 1);
            }
            "next_tab" => self.switch_tab((self.active + 1) % self.tabs.len()),
            "prev_tab" => self.switch_tab((self.active + self.tabs.len() - 1) % self.tabs.len()),
            "close_tab" => {
                if self.tabs.len() > 1 {
                    self.tabs.remove(self.active);
                    self.active = self.active.min(self.tabs.len() - 1);
                    self.switch_tab(self.active);
                } else {
                    self.notice("This is the last tab · q to quit", false);
                }
            }
            "help" => self.mode = Mode::Help { page: 0, scroll: 0 },
            "escape" => {
                self.mode = Mode::Normal;
                self.tab_mut().marked.clear();
                if !self.tab().filter.is_empty() {
                    self.tab_mut().filter.clear();
                    self.tab_mut().refilter();
                    self.request_preview(true);
                } else if self.tab().search.is_some() {
                    self.exit_search();
                }
                self.message = None;
            }
            _ => {}
        }
        self.dirty = true;
    }

    pub fn scroll_preview(&mut self, delta: isize) {
        let max = self.preview.max_scroll(self.preview_size.height as usize);
        let next = self.preview_scroll.saturating_add_signed(delta).min(max);
        if next != self.preview_scroll {
            self.preview_scroll = next;
            if matches!(self.preview, Preview::Image { .. }) {
                self.request_preview(false);
            }
            self.dirty = true;
        }
    }

    fn switch_tab(&mut self, index: usize) {
        self.preview_tx.cancel();
        self.refresh_at = None;
        self.refresh_preview_at = None;
        self.active = index;
        self.mode = Mode::Normal;
        self.preview = Preview::Empty;
        self.preview_generation += 1;
        self.refresh();
        self.request_preview(true);
        self.update_watches();
    }

    pub fn handle_paste(&mut self, text: &str) {
        if matches!(self.mode, Mode::Input(_)) {
            self.input.extend(
                text.chars()
                    .filter(|c| !c.is_control())
                    .take(4096usize.saturating_sub(self.input.chars().count())),
            );
            self.filter_changed();
            self.dirty = true;
        }
    }

    fn handle_input(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            if matches!(self.mode, Mode::Input(InputKind::Filter)) {
                self.tab_mut().filter = self.filter_backup.clone();
                self.tab_mut().refilter();
                self.request_preview(true);
            }
            self.mode = Mode::Normal;
            return;
        }
        if key.code == KeyCode::Enter {
            let mode = std::mem::replace(&mut self.mode, Mode::Normal);
            let input = self.input.clone();
            match mode {
                Mode::Input(InputKind::Rename(source)) => self.submit(Operation::Rename {
                    source,
                    name: input,
                }),
                Mode::Input(InputKind::SearchName) => self.start_search(SearchKind::Name, input),
                Mode::Input(InputKind::SearchContents) => {
                    self.start_search(SearchKind::Contents, input)
                }
                Mode::Input(InputKind::Command) => self.command(&input),
                Mode::Input(InputKind::Bookmark) => self.save_bookmark(&input),
                Mode::Input(InputKind::Archive(sources)) => {
                    let name = input.trim();
                    if let Err(error) = operations::validate_name(name) {
                        self.notice(format!("{error:#}"), true);
                    } else if archive::format(Path::new(name)).is_none() {
                        self.notice(
                            "Archive name must end in .zip, .tar, .tar.gz, or .tgz",
                            true,
                        );
                    } else {
                        self.submit(Operation::CreateArchive {
                            sources,
                            destination: self.tab().cwd.join(name),
                        });
                    }
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.clear()
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && self.input.chars().count() < 4096 =>
            {
                self.input.push(c)
            }
            _ => {}
        }
        self.filter_changed();
    }

    fn filter_changed(&mut self) {
        if matches!(self.mode, Mode::Input(InputKind::Filter)) {
            self.tab_mut().filter = self.input.clone();
            self.tab_mut().selected = 0;
            self.tab_mut().visible.clear();
            self.tab_mut().refilter();
            self.request_preview(true);
        }
    }

    fn save_bookmark(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.notice("Bookmark name cannot be empty", true);
            return;
        }
        let mut updated = self.bookmarks.clone();
        updated.insert(name.into(), self.tab().cwd.clone());
        match config::save_bookmarks(&self.config_path, &updated) {
            Ok(()) => {
                self.bookmarks = updated;
                self.notice(format!("Saved bookmark '{name}' · b to jump"), false);
            }
            Err(e) => self.notice(format!("Could not save bookmark: {e}"), true),
        }
    }

    pub fn command(&mut self, input: &str) {
        let (command, arg) = input.trim().split_once(' ').unwrap_or((input.trim(), ""));
        let arg = arg.trim().trim_matches('"');
        match command {
            "q" | "quit" => self.action("quit"),
            "cd" if !arg.is_empty() => self.navigate(config::expand_path(arg, &self.tab().cwd)),
            "mkdir" if !arg.is_empty() => {
                self.submit(Operation::Mkdir(config::expand_path(arg, &self.tab().cwd)))
            }
            "touch" if !arg.is_empty() => {
                self.submit(Operation::Touch(config::expand_path(arg, &self.tab().cwd)))
            }
            "bookmark" if !arg.is_empty() => self.save_bookmark(arg),
            "find" if !arg.is_empty() => self.start_search(SearchKind::Name, arg.into()),
            "grep" if !arg.is_empty() => self.start_search(SearchKind::Contents, arg.into()),
            "archive" if !arg.is_empty() => {
                let sources = self.targets();
                if let Err(error) = operations::validate_name(arg) {
                    self.notice(format!("{error:#}"), true);
                } else if archive::format(Path::new(arg)).is_none() {
                    self.notice(
                        "Archive name must end in .zip, .tar, .tar.gz, or .tgz",
                        true,
                    );
                } else if sources.is_empty() {
                    self.notice("Select at least one item to archive", true);
                } else {
                    self.submit(Operation::CreateArchive {
                        sources,
                        destination: self.tab().cwd.join(arg),
                    });
                }
            }
            "extract" => self.action("extract"),
            "undo" => self.action("undo"),
            "reload" => match Config::load(&self.config_path) {
                Ok(config) => {
                    self.bindings = config.bindings();
                    self.bookmarks = config.bookmarks(&self.config_path);
                    self.config = config;
                    if let ratatui::style::Color::Rgb(r, g, b) =
                        config::color(&self.config.theme.background)
                    {
                        self.picker
                            .set_background_color(Some(image::Rgba([r, g, b, 255])));
                    }
                    let (tx, rx) =
                        preview::spawn_previewer(self.picker.clone(), self.config.clone());
                    self.preview_tx = tx;
                    self.preview_rx = rx;
                    self.request_preview(true);
                    self.refresh();
                    self.notice(
                        "Config reloaded · protocol changes take effect on restart",
                        false,
                    );
                }
                Err(e) => self.notice(format!("{e:#}"), true),
            },
            "help" => self.mode = Mode::Help { page: 0, scroll: 0 },
            "" => {}
            _ => self.notice(
                "Commands: cd PATH · find NAME · grep TEXT · mkdir NAME · touch NAME · archive NAME.zip · extract · undo · bookmark NAME · reload · quit",
                true,
            ),
        }
    }

    pub fn displayed_path(path: &Path) -> String {
        files::safe_text(
            &path
                .display()
                .to_string()
                .trim_start_matches("\\\\?\\")
                .replace('\\', "/"),
        )
    }
    pub fn preview_requests(&self) -> u64 {
        self.preview_generation
    }
}

#[cfg(windows)]
fn rename_path_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

#[cfg(not(windows))]
fn rename_path_key(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// A sibling file being written should not regenerate the selected preview.
fn relevant_change(event: &notify::Event, cwd: &Path, selected: Option<&Entry>) -> (bool, bool) {
    let mut listing = event.paths.is_empty();
    let mut preview = false;
    let structural = matches!(
        event.kind,
        notify::EventKind::Any
            | notify::EventKind::Other
            | notify::EventKind::Create(_)
            | notify::EventKind::Remove(_)
            | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
    );
    for path in &event.paths {
        if path == cwd || path.parent() == Some(cwd) {
            listing = true;
        }
        if selected.is_some_and(|entry| path == &entry.path)
            && matches!(event.kind, notify::EventKind::Modify(_))
        {
            // A write to the highlighted file may leave size/mtime unchanged
            // on coarse timestamp filesystems. Request its preview directly.
            preview = true;
        }
        // Parent context displays names/types only, so ignore sibling data writes.
        if structural && path.parent() == cwd.parent() {
            listing = true;
        }
        if selected.is_some_and(|entry| entry.is_dir && path.parent() == Some(entry.path.as_path()))
        {
            preview = true;
        }
    }
    (listing, preview)
}
