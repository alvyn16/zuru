use crate::archive;
use anyhow::{bail, Context, Result};
use std::{
    collections::HashSet,
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use walkdir::WalkDir;

static TRASH_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub enum Operation {
    Paste {
        sources: Vec<PathBuf>,
        destination: PathBuf,
        cut: bool,
    },
    Trash(Vec<PathBuf>),
    Rename {
        source: PathBuf,
        name: String,
    },
    BulkRename(Vec<(PathBuf, PathBuf)>),
    CreateArchive {
        sources: Vec<PathBuf>,
        destination: PathBuf,
    },
    ExtractArchives {
        archives: Vec<PathBuf>,
        destination: PathBuf,
    },
    Undo(UndoAction),
    Mkdir(PathBuf),
    Touch(PathBuf),
    Open(PathBuf),
}

#[derive(Clone, Debug)]
pub enum UndoAction {
    Rename {
        label: String,
        changes: Vec<(PathBuf, PathBuf)>,
    },
    Filesystem {
        label: String,
        remove: Vec<PathBuf>,
        restore: Vec<trash::TrashItem>,
        restore_renames: Vec<(PathBuf, PathBuf)>,
        remove_nonempty_directories: bool,
    },
}

impl UndoAction {
    pub fn label(&self) -> &str {
        match self {
            Self::Rename { label, .. } | Self::Filesystem { label, .. } => label,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OperationResult {
    pub task_id: u64,
    pub message: String,
    pub error: bool,
    pub cancelled: bool,
    pub refresh: bool,
    pub moved_sources: Option<Vec<PathBuf>>,
    pub undo: Option<UndoAction>,
}

#[derive(Clone, Debug)]
pub struct TaskProgress {
    pub task_id: u64,
    pub label: String,
    pub current: Option<PathBuf>,
    pub completed_items: usize,
    pub total_items: usize,
    pub bytes_done: u64,
    pub total_bytes: u64,
    pub bytes_per_second: u64,
}

impl TaskProgress {
    pub fn percent(&self) -> u16 {
        self.bytes_done
            .saturating_mul(100)
            .checked_div(self.total_bytes)
            .or_else(|| {
                self.completed_items
                    .saturating_mul(100)
                    .checked_div(self.total_items)
                    .map(|percent| percent as u64)
            })
            .unwrap_or(0)
            .min(100) as u16
    }
}

#[derive(Clone, Debug)]
pub struct Conflict {
    pub task_id: u64,
    pub source: PathBuf,
    pub target: PathBuf,
    pub is_dir: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictChoice {
    Skip,
    KeepBoth,
    Replace,
    SkipAll,
    KeepBothAll,
    ReplaceAll,
    Cancel,
}

#[derive(Clone, Debug)]
pub enum OperationEvent {
    Started(TaskProgress),
    Progress(TaskProgress),
    Conflict(Conflict),
    Finished(OperationResult),
}

struct TaskRequest {
    task_id: u64,
    operation: Operation,
}

#[derive(Clone)]
pub struct OperationController {
    tasks: Sender<TaskRequest>,
    conflicts: Sender<(u64, ConflictChoice)>,
    cancelled: Arc<AtomicU64>,
    next_id: Arc<AtomicU64>,
}

impl OperationController {
    pub fn submit(&self, operation: Operation) -> Result<u64> {
        let task_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.tasks
            .send(TaskRequest { task_id, operation })
            .context("Operation worker is unavailable")?;
        Ok(task_id)
    }

    pub fn cancel(&self, task_id: u64) {
        self.cancelled.store(task_id, Ordering::Release);
    }

    pub fn resolve(&self, task_id: u64, choice: ConflictChoice) -> Result<()> {
        self.conflicts
            .send((task_id, choice))
            .context("Operation worker is unavailable")
    }
}

pub fn spawn_worker() -> (OperationController, Receiver<OperationEvent>) {
    let (task_tx, task_rx) = mpsc::channel::<TaskRequest>();
    let (conflict_tx, conflict_rx) = mpsc::channel::<(u64, ConflictChoice)>();
    let (event_tx, event_rx) = mpsc::channel();
    let cancelled = Arc::new(AtomicU64::new(0));
    let controller = OperationController {
        tasks: task_tx,
        conflicts: conflict_tx,
        cancelled: cancelled.clone(),
        next_id: Arc::new(AtomicU64::new(1)),
    };
    thread::spawn(move || {
        while let Ok(request) = task_rx.recv() {
            let operation = request.operation;
            let refresh = !matches!(&operation, Operation::Open(_));
            let moved_sources = match &operation {
                Operation::Paste {
                    sources, cut: true, ..
                } => Some(sources.clone()),
                _ => None,
            };
            let mut tracker = Tracker::new(
                request.task_id,
                operation_label(&operation),
                event_tx.clone(),
                cancelled.clone(),
            );
            tracker.started();
            let result = execute_tracked(operation, &mut tracker, &conflict_rx);
            let moved_sources = moved_sources.filter(|_| !tracker.skipped_items);
            let report = match result {
                Ok(message) => OperationResult {
                    task_id: request.task_id,
                    message,
                    error: false,
                    cancelled: false,
                    refresh,
                    moved_sources,
                    undo: tracker.undo.clone(),
                },
                Err(error) => OperationResult {
                    task_id: request.task_id,
                    message: if error.downcast_ref::<Cancelled>().is_some() {
                        "Operation cancelled; completed changes were kept".into()
                    } else {
                        format!("{error:#}")
                    },
                    error: error.downcast_ref::<Cancelled>().is_none(),
                    cancelled: error.downcast_ref::<Cancelled>().is_some(),
                    refresh,
                    moved_sources: None,
                    undo: tracker.undo.clone(),
                },
            };
            if event_tx.send(OperationEvent::Finished(report)).is_err() {
                break;
            }
        }
    });
    (controller, event_rx)
}

#[derive(Debug)]
struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Operation cancelled")
    }
}

impl Error for Cancelled {}

struct Tracker {
    progress: TaskProgress,
    events: Sender<OperationEvent>,
    cancelled: Arc<AtomicU64>,
    last_update: Instant,
    started_at: Instant,
    skipped_items: bool,
    undo: Option<UndoAction>,
}

impl Tracker {
    fn new(
        task_id: u64,
        label: String,
        events: Sender<OperationEvent>,
        cancelled: Arc<AtomicU64>,
    ) -> Self {
        Self {
            progress: TaskProgress {
                task_id,
                label,
                current: None,
                completed_items: 0,
                total_items: 0,
                bytes_done: 0,
                total_bytes: 0,
                bytes_per_second: 0,
            },
            events,
            cancelled,
            last_update: Instant::now(),
            started_at: Instant::now(),
            skipped_items: false,
            undo: None,
        }
    }

    fn started(&mut self) {
        let _ = self
            .events
            .send(OperationEvent::Started(self.progress.clone()));
    }

    fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) == self.progress.task_id {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    fn set_totals(&mut self, items: usize, bytes: u64) {
        self.progress.total_items = items;
        self.progress.total_bytes = bytes;
        self.emit(true);
    }

    fn current(&mut self, path: &Path) {
        self.progress.current = Some(path.to_path_buf());
        self.emit(false);
    }

    fn add_bytes(&mut self, bytes: u64) {
        self.progress.bytes_done = self.progress.bytes_done.saturating_add(bytes);
        self.emit(false);
    }

    fn complete_item(&mut self, path: &Path) {
        self.progress.current = Some(path.to_path_buf());
        self.progress.completed_items = self.progress.completed_items.saturating_add(1);
        self.emit(false);
    }

    fn skip(&mut self, work: Work) {
        self.skipped_items = true;
        self.progress.completed_items = self.progress.completed_items.saturating_add(work.items);
        self.progress.bytes_done = self.progress.bytes_done.saturating_add(work.bytes);
        self.emit(true);
    }

    fn emit(&mut self, force: bool) {
        if force || self.last_update.elapsed() >= Duration::from_millis(50) {
            let elapsed = self.started_at.elapsed().as_secs_f64();
            if elapsed > 0.05 {
                self.progress.bytes_per_second = (self.progress.bytes_done as f64 / elapsed) as u64;
            }
            let _ = self
                .events
                .send(OperationEvent::Progress(self.progress.clone()));
            self.last_update = Instant::now();
        }
    }
}

impl archive::Progress for Tracker {
    fn check(&self) -> Result<()> {
        Tracker::check(self)
    }

    fn current(&mut self, path: &Path) {
        Tracker::current(self, path);
    }

    fn add_bytes(&mut self, bytes: u64) {
        Tracker::add_bytes(self, bytes);
    }
}

#[derive(Clone, Copy, Default)]
struct Work {
    items: usize,
    bytes: u64,
}

impl std::ops::AddAssign for Work {
    fn add_assign(&mut self, other: Self) {
        self.items = self.items.saturating_add(other.items);
        self.bytes = self.bytes.saturating_add(other.bytes);
    }
}

fn operation_label(operation: &Operation) -> String {
    match operation {
        Operation::Paste { cut: true, .. } => "Moving files".into(),
        Operation::Paste { .. } => "Copying files".into(),
        Operation::Trash(_) => "Sending files to trash".into(),
        Operation::Rename { .. } => "Renaming file".into(),
        Operation::BulkRename(_) => "Renaming files".into(),
        Operation::CreateArchive { .. } => "Creating archive".into(),
        Operation::ExtractArchives { .. } => "Extracting archives".into(),
        Operation::Undo(action) => format!("Undoing {}", action.label()),
        Operation::Mkdir(_) => "Creating folder".into(),
        Operation::Touch(_) => "Creating file".into(),
        Operation::Open(_) => "Opening file".into(),
    }
}

fn execute_tracked(
    operation: Operation,
    tracker: &mut Tracker,
    conflicts: &Receiver<(u64, ConflictChoice)>,
) -> Result<String> {
    match operation {
        Operation::Open(path) => {
            tracker.set_totals(1, 0);
            tracker.check()?;
            tracker.current(&path);
            open::that(&path).with_context(|| format!("Could not open {}", path.display()))?;
            tracker.complete_item(&path);
            Ok(format!(
                "Opened {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ))
        }
        Operation::Paste {
            sources,
            destination,
            cut,
        } => paste_tracked(sources, destination, cut, tracker, conflicts),
        Operation::Trash(paths) => {
            let trash_before = trash_before();
            let works = paths
                .iter()
                .map(|path| measure_path(path, tracker))
                .collect::<Result<Vec<_>>>()?;
            let total = works
                .iter()
                .copied()
                .fold(Work::default(), |mut sum, work| {
                    sum += work;
                    sum
                });
            tracker.set_totals(total.items, total.bytes);
            let mut completed = 0;
            let mut errors = Vec::new();
            let mut trashed = Vec::new();
            let mut restore_renames = Vec::new();
            for (path, work) in paths.iter().zip(works) {
                tracker.check()?;
                tracker.current(path);
                match trash_delete(path) {
                    Ok(result) => {
                        trashed.push(result.path);
                        if let Some(rename) = result.restore_rename {
                            restore_renames.push(rename);
                        }
                        completed += 1;
                        tracker.skip(work);
                    }
                    Err(error) => errors.push(format!("{}: {error}", path.display())),
                }
            }
            tracker.undo =
                captured_undo("trash", Vec::new(), &trashed, restore_renames, trash_before);
            if !errors.is_empty() {
                bail!(
                    "Trashed {completed}/{} items. {}",
                    paths.len(),
                    errors.join("; ")
                );
            }
            Ok(format!("Sent {completed} item(s) to trash"))
        }
        Operation::Rename { source, name } => {
            tracker.set_totals(1, 0);
            tracker.check()?;
            validate_name(&name)?;
            let parent = source.parent().context("Cannot rename a filesystem root")?;
            let target = parent.join(name);
            if source == target {
                tracker.complete_item(&source);
                return Ok("Name unchanged".into());
            }
            ensure_absent(&target)?;
            tracker.current(&source);
            fs::rename(&source, &target)
                .with_context(|| format!("Could not rename {}", source.display()))?;
            tracker.complete_item(&target);
            tracker.undo = Some(UndoAction::Rename {
                label: format!(
                    "rename {}",
                    target.file_name().unwrap_or_default().to_string_lossy()
                ),
                changes: vec![(target.clone(), source)],
            });
            Ok(format!(
                "Renamed to {}",
                target.file_name().unwrap_or_default().to_string_lossy()
            ))
        }
        Operation::BulkRename(changes) => {
            let inverse = changes
                .iter()
                .map(|(source, target)| (target.clone(), source.clone()))
                .collect();
            let message = bulk_rename(changes, Some(tracker))?;
            tracker.undo = Some(UndoAction::Rename {
                label: "bulk rename".into(),
                changes: inverse,
            });
            Ok(message)
        }
        Operation::CreateArchive {
            sources,
            destination,
        } => {
            let bytes = sources.iter().try_fold(0u64, |total, source| {
                Ok::<_, anyhow::Error>(total.saturating_add(measure_path(source, tracker)?.bytes))
            })?;
            tracker.set_totals(1, bytes);
            tracker.current(&destination);
            archive::create(&destination, &sources, tracker)?;
            tracker.complete_item(&destination);
            tracker.undo = Some(UndoAction::Filesystem {
                label: "archive creation".into(),
                remove: vec![destination.clone()],
                restore: Vec::new(),
                restore_renames: Vec::new(),
                remove_nonempty_directories: true,
            });
            Ok(format!("Created archive {}", destination.display()))
        }
        Operation::ExtractArchives {
            archives,
            destination,
        } => {
            let outputs = archives
                .iter()
                .map(|path| archive::extraction_directory(path, &destination))
                .collect::<Result<Vec<_>>>()?;
            let mut unique = HashSet::new();
            for output in &outputs {
                if !unique.insert(rename_path_key(output)) {
                    bail!("Selected archives would extract to the same folder");
                }
                ensure_absent(output)?;
            }
            tracker.set_totals(archives.len(), 0);
            let mut completed = Vec::new();
            for (path, output) in archives.iter().zip(outputs) {
                tracker.check()?;
                tracker.current(path);
                completed.push(output.clone());
                if let Err(error) = archive::extract(path, &output, tracker) {
                    tracker.undo = Some(UndoAction::Filesystem {
                        label: "archive extraction".into(),
                        remove: completed,
                        restore: Vec::new(),
                        restore_renames: Vec::new(),
                        remove_nonempty_directories: true,
                    });
                    return Err(error);
                }
                tracker.complete_item(path);
            }
            tracker.undo = Some(UndoAction::Filesystem {
                label: "archive extraction".into(),
                remove: completed,
                restore: Vec::new(),
                restore_renames: Vec::new(),
                remove_nonempty_directories: true,
            });
            Ok(format!("Extracted {} archive(s)", archives.len()))
        }
        Operation::Undo(action) => undo(action, tracker),
        Operation::Mkdir(path) => {
            tracker.set_totals(1, 0);
            tracker.check()?;
            tracker.current(&path);
            fs::create_dir(&path)
                .with_context(|| format!("Could not create {}", path.display()))?;
            tracker.complete_item(&path);
            tracker.undo = Some(UndoAction::Filesystem {
                label: "folder creation".into(),
                remove: vec![path.clone()],
                restore: Vec::new(),
                restore_renames: Vec::new(),
                remove_nonempty_directories: false,
            });
            Ok(format!("Created folder {}", path.display()))
        }
        Operation::Touch(path) => {
            tracker.set_totals(1, 0);
            tracker.check()?;
            tracker.current(&path);
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            tracker.complete_item(&path);
            tracker.undo = Some(UndoAction::Filesystem {
                label: "file creation".into(),
                remove: vec![path.clone()],
                restore: Vec::new(),
                restore_renames: Vec::new(),
                remove_nonempty_directories: true,
            });
            Ok(format!("Created file {}", path.display()))
        }
    }
}

#[derive(Clone, Copy)]
enum ConflictPolicy {
    Skip,
    KeepBoth,
    Replace,
}

fn paste_tracked(
    sources: Vec<PathBuf>,
    destination: PathBuf,
    cut: bool,
    tracker: &mut Tracker,
    conflicts: &Receiver<(u64, ConflictChoice)>,
) -> Result<String> {
    let trash_before = trash_before();
    let destination =
        fs::canonicalize(&destination).context("Cannot access destination directory")?;
    let sources = sources
        .into_iter()
        .map(|source| absolute_preserving_link(&source))
        .collect::<Result<Vec<_>>>()?;
    let works = sources
        .iter()
        .map(|source| measure_path(source, tracker))
        .collect::<Result<Vec<_>>>()?;
    let total = works
        .iter()
        .copied()
        .fold(Work::default(), |mut sum, work| {
            sum += work;
            sum
        });
    tracker.set_totals(total.items, total.bytes);
    let mut policy = None;
    let mut completed = 0;
    let mut errors = Vec::new();
    let mut created = Vec::new();
    let mut trashed = Vec::new();
    let mut restore_renames = Vec::new();
    for (source, work) in sources.iter().zip(works) {
        tracker.check()?;
        let result = (|| {
            let meta = fs::symlink_metadata(source)?;
            if meta.is_symlink() {
                bail!("Copy/move of symbolic links is not supported in v1");
            }
            if !meta.is_dir() && !meta.is_file() {
                bail!("Special files cannot be copied");
            }
            if meta.is_dir() && destination.starts_with(source) {
                bail!("Cannot copy a directory into itself or one of its descendants");
            }
            let mut target = destination.join(
                source
                    .file_name()
                    .context("Cannot copy a filesystem root")?,
            );
            if path_exists(&target)? {
                match resolve_conflict(
                    Conflict {
                        task_id: tracker.progress.task_id,
                        source: source.clone(),
                        target: target.clone(),
                        is_dir: meta.is_dir(),
                    },
                    &mut policy,
                    tracker,
                    conflicts,
                )? {
                    ConflictPolicy::Skip => {
                        tracker.skip(work);
                        return Ok(false);
                    }
                    ConflictPolicy::KeepBoth => {
                        target = unique_target(&target, meta.is_dir())?;
                    }
                    ConflictPolicy::Replace => {
                        if source == &target {
                            bail!("Cannot replace an item with itself; choose Keep Both or Skip");
                        }
                        tracker.current(&target);
                        let result = trash_delete(&target).with_context(|| {
                            format!("Could not send existing {} to trash", target.display())
                        })?;
                        trashed.push(result.path);
                        if let Some(rename) = result.restore_rename {
                            restore_renames.push(rename);
                        }
                    }
                }
            }
            copy_to(source, &target, tracker)?;
            created.push(target.clone());
            tracker.check()?;
            if cut {
                let result = trash_delete(source).with_context(|| {
                    format!(
                        "Copy succeeded, but could not trash original {}; both copies were kept",
                        source.display()
                    )
                })?;
                trashed.push(result.path);
                if let Some(rename) = result.restore_rename {
                    restore_renames.push(rename);
                }
            }
            Ok(true)
        })();
        match result {
            Ok(true) => completed += 1,
            Ok(false) => {}
            Err(error) if error.downcast_ref::<Cancelled>().is_some() => return Err(error),
            Err(error) => errors.push(format!("{}: {error:#}", source.display())),
        }
    }
    let verb = if cut { "Moved" } else { "Copied" };
    tracker.undo = if trashed.is_empty() {
        (!created.is_empty()).then(|| UndoAction::Filesystem {
            label: verb.to_ascii_lowercase(),
            remove: created,
            restore: Vec::new(),
            restore_renames: Vec::new(),
            remove_nonempty_directories: true,
        })
    } else {
        captured_undo(
            &verb.to_ascii_lowercase(),
            created,
            &trashed,
            restore_renames,
            trash_before,
        )
    };
    if !errors.is_empty() {
        bail!(
            "{verb} {completed}/{} items. {}",
            sources.len(),
            errors.join("; ")
        );
    }
    Ok(format!("{verb} {completed} item(s)"))
}

fn undo(action: UndoAction, tracker: &mut Tracker) -> Result<String> {
    let label = action.label().to_owned();
    match action {
        UndoAction::Rename { changes, .. } => {
            bulk_rename(changes, Some(tracker))?;
        }
        UndoAction::Filesystem {
            remove,
            restore,
            restore_renames,
            remove_nonempty_directories,
            ..
        } => {
            tracker.set_totals(remove.len().saturating_add(restore.len()), 0);
            for path in remove.iter().rev() {
                tracker.check()?;
                tracker.current(path);
                let metadata = match fs::symlink_metadata(path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        tracker.complete_item(path);
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
                if metadata.is_dir()
                    && !remove_nonempty_directories
                    && fs::read_dir(path)?.next().is_some()
                {
                    bail!("Cannot undo {label}: {} is no longer empty", path.display());
                }
                let _ = trash_delete(path).with_context(|| {
                    format!("Could not remove {} while undoing", path.display())
                })?;
                tracker.complete_item(path);
            }
            for (_, destination) in &restore_renames {
                ensure_absent(destination)?;
            }
            let restored_paths = restore
                .iter()
                .map(|item| item.original_path())
                .collect::<Vec<_>>();
            restore_trash(restore)?;
            for (restored, destination) in &restore_renames {
                fs::rename(restored, destination).with_context(|| {
                    format!(
                        "Restored {}, but could not restore its original filename {}",
                        restored.display(),
                        destination.display()
                    )
                })?;
            }
            for path in restored_paths {
                let completed = restore_renames
                    .iter()
                    .find(|(restored, _)| rename_path_key(restored) == rename_path_key(&path))
                    .map(|(_, destination)| destination)
                    .unwrap_or(&path);
                tracker.complete_item(completed);
            }
        }
    }
    Ok(format!("Undid {label}"))
}

fn captured_undo(
    label: &str,
    remove: Vec<PathBuf>,
    trashed_paths: &[PathBuf],
    restore_renames: Vec<(PathBuf, PathBuf)>,
    before: Option<HashSet<std::ffi::OsString>>,
) -> Option<UndoAction> {
    let restore = captured_trash(trashed_paths, before)?;
    (restore.len() == trashed_paths.len()).then(|| UndoAction::Filesystem {
        label: label.into(),
        remove,
        restore,
        restore_renames,
        remove_nonempty_directories: true,
    })
}

struct TrashedPath {
    path: PathBuf,
    restore_rename: Option<(PathBuf, PathBuf)>,
}

/// Windows' shell Recycle Bin API does not reliably accept the extended-length
/// prefix returned by `std::fs::canonicalize`. It cannot address a basename
/// ending in a space or dot at all, so stage that rare case under a safe name.
fn trash_delete(path: &Path) -> Result<TrashedPath> {
    #[cfg(windows)]
    if path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with([' ', '.']))
    {
        let parent = path.parent().context("Cannot trash a filesystem root")?;
        let staged = loop {
            let candidate = parent.join(format!(
                ".zuru-trash-{}-{}",
                std::process::id(),
                TRASH_TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            if !path_exists(&candidate)? {
                break candidate;
            }
        };
        fs::rename(path, &staged).with_context(|| {
            format!(
                "Could not prepare the Windows-incompatible name {} for trash",
                path.display()
            )
        })?;
        if let Err(error) = trash::delete(trash_compatible_path(&staged)) {
            if let Err(rollback) = fs::rename(&staged, path) {
                bail!(
                    "Could not send {} to trash: {error}. Its temporary name is {} because restoring the original name also failed: {rollback}",
                    path.display(),
                    staged.display()
                );
            }
            return Err(error.into());
        }
        return Ok(TrashedPath {
            path: staged.clone(),
            restore_rename: Some((staged, path.to_path_buf())),
        });
    }

    trash::delete(trash_compatible_path(path))?;
    Ok(TrashedPath {
        path: path.to_path_buf(),
        restore_rename: None,
    })
}

#[cfg(windows)]
fn trash_compatible_path(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(path) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{path}"))
    } else if let Some(path) = text.strip_prefix(r"\\?\") {
        PathBuf::from(path)
    } else {
        path.to_path_buf()
    }
}

#[cfg(not(windows))]
fn trash_compatible_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(any(
    target_os = "windows",
    all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )
))]
fn trash_before() -> Option<HashSet<std::ffi::OsString>> {
    trash::os_limited::list()
        .ok()
        .map(|items| items.into_iter().map(|item| item.id).collect())
}

#[cfg(not(any(
    target_os = "windows",
    all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )
)))]
fn trash_before() -> Option<HashSet<std::ffi::OsString>> {
    None
}

#[cfg(any(
    target_os = "windows",
    all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )
))]
fn captured_trash(
    paths: &[PathBuf],
    before: Option<HashSet<std::ffi::OsString>>,
) -> Option<Vec<trash::TrashItem>> {
    let before = before?;
    let expected: HashSet<_> = paths.iter().map(|path| trash_path_key(path)).collect();
    trash::os_limited::list().ok().map(|items| {
        items
            .into_iter()
            .filter(|item| {
                !before.contains(&item.id)
                    && expected.contains(&trash_path_key(&item.original_path()))
            })
            .collect()
    })
}

#[cfg(not(any(
    target_os = "windows",
    all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )
)))]
fn captured_trash(
    _paths: &[PathBuf],
    _before: Option<HashSet<std::ffi::OsString>>,
) -> Option<Vec<trash::TrashItem>> {
    None
}

#[cfg(any(
    target_os = "windows",
    all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )
))]
fn restore_trash(items: Vec<trash::TrashItem>) -> Result<()> {
    trash::os_limited::restore_all(items).context("Could not restore item(s) from trash")
}

#[cfg(not(any(
    target_os = "windows",
    all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )
)))]
fn restore_trash(items: Vec<trash::TrashItem>) -> Result<()> {
    if items.is_empty() {
        Ok(())
    } else {
        bail!("Restoring items from trash is unavailable on this operating system")
    }
}

#[cfg(windows)]
fn trash_path_key(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches("\\\\?\\")
        .replace('\\', "/")
        .to_lowercase()
}

#[cfg(not(windows))]
fn trash_path_key(path: &Path) -> PathBuf {
    path.to_path_buf()
}

fn resolve_conflict(
    conflict: Conflict,
    policy: &mut Option<ConflictPolicy>,
    tracker: &Tracker,
    conflicts: &Receiver<(u64, ConflictChoice)>,
) -> Result<ConflictPolicy> {
    if let Some(policy) = policy {
        return Ok(*policy);
    }
    tracker
        .events
        .send(OperationEvent::Conflict(conflict.clone()))
        .context("Application closed during conflict resolution")?;
    loop {
        tracker.check()?;
        match conflicts.recv_timeout(Duration::from_millis(100)) {
            Ok((task_id, choice)) if task_id == conflict.task_id => {
                let selected = match choice {
                    ConflictChoice::Skip => ConflictPolicy::Skip,
                    ConflictChoice::KeepBoth => ConflictPolicy::KeepBoth,
                    ConflictChoice::Replace => ConflictPolicy::Replace,
                    ConflictChoice::SkipAll => {
                        *policy = Some(ConflictPolicy::Skip);
                        ConflictPolicy::Skip
                    }
                    ConflictChoice::KeepBothAll => {
                        *policy = Some(ConflictPolicy::KeepBoth);
                        ConflictPolicy::KeepBoth
                    }
                    ConflictChoice::ReplaceAll => {
                        *policy = Some(ConflictPolicy::Replace);
                        ConflictPolicy::Replace
                    }
                    ConflictChoice::Cancel => return Err(Cancelled.into()),
                };
                return Ok(selected);
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(Cancelled.into()),
        }
    }
}

fn measure_path(path: &Path, tracker: &Tracker) -> Result<Work> {
    tracker.check()?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.is_symlink() {
        return Ok(Work {
            items: 1,
            bytes: metadata.len(),
        });
    }
    let mut work = Work::default();
    for entry in WalkDir::new(path).follow_links(false) {
        tracker.check()?;
        let entry = entry?;
        work.items = work.items.saturating_add(1);
        if entry.file_type().is_file() {
            work.bytes = work.bytes.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(work)
}

fn copy_to(source: &Path, target: &Path, tracker: &mut Tracker) -> Result<()> {
    ensure_absent(target)?;
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_file() {
        return copy_file_tracked(source, target, tracker);
    }
    let paths = WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for entry in &paths {
        tracker.check()?;
        if !entry.file_type().is_dir() && !entry.file_type().is_file() {
            bail!(
                "Tree contains a link or special file: {}",
                entry.path().display()
            );
        }
    }
    fs::create_dir(target)?;
    tracker.complete_item(source);
    for entry in paths.into_iter().skip(1) {
        tracker.check()?;
        let destination = target.join(entry.path().strip_prefix(source)?);
        if entry.file_type().is_dir() {
            fs::create_dir(&destination).with_context(|| {
                format!(
                    "Copy incomplete; source is intact, partial destination kept at {}",
                    target.display()
                )
            })?;
            tracker.complete_item(entry.path());
        } else {
            copy_file_tracked(entry.path(), &destination, tracker).with_context(|| {
                format!(
                    "Copy incomplete; source is intact, partial destination kept at {}",
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}

fn copy_file_tracked(source: &Path, target: &Path, tracker: &mut Tracker) -> Result<()> {
    tracker.check()?;
    tracker.current(source);
    let mut input = fs::File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    let mut buffer = vec![0; 256 * 1024];
    loop {
        tracker.check()?;
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        tracker.add_bytes(read as u64);
    }
    output.sync_all()?;
    fs::set_permissions(target, input.metadata()?.permissions())?;
    tracker.complete_item(source);
    Ok(())
}

fn unique_target(target: &Path, directory: bool) -> Result<PathBuf> {
    let parent = target.parent().context("Destination has no parent")?;
    let name = target
        .file_name()
        .context("Destination has no filename")?
        .to_string_lossy();
    let (stem, extension) = if directory {
        (name.into_owned(), None)
    } else {
        (
            target
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            target
                .extension()
                .map(|extension| extension.to_string_lossy().into_owned()),
        )
    };
    for number in 1..=10_000 {
        let suffix = if number == 1 {
            " (copy)".into()
        } else {
            format!(" (copy {number})")
        };
        let filename = match &extension {
            Some(extension) => format!("{stem}{suffix}.{extension}"),
            None => format!("{stem}{suffix}"),
        };
        let candidate = parent.join(filename);
        if !path_exists(&candidate)? {
            return Ok(candidate);
        }
    }
    bail!("Could not find an available Keep Both filename")
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

struct NoProgress;

impl archive::Progress for NoProgress {
    fn check(&self) -> Result<()> {
        Ok(())
    }

    fn current(&mut self, _path: &Path) {}

    fn add_bytes(&mut self, _bytes: u64) {}
}

pub fn execute(operation: Operation) -> Result<String> {
    match operation {
        Operation::Open(path) => {
            open::that(&path).with_context(|| format!("Could not open {}", path.display()))?;
            Ok(format!(
                "Opened {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ))
        }
        Operation::Paste {
            sources,
            destination,
            cut,
        } => {
            let mut completed = 0;
            let mut errors = Vec::new();
            for source in &sources {
                let result = copy_into(source, &destination).and_then(|_| {
                    if cut {
                        trash_delete(source).with_context(|| format!("Copy succeeded, but could not trash original {}; both copies were kept", source.display()))?;
                    }
                    Ok(())
                });
                match result {
                    Ok(_) => completed += 1,
                    Err(e) => errors.push(format!("{}: {e:#}", source.display())),
                }
            }
            let verb = if cut { "Moved" } else { "Copied" };
            if !errors.is_empty() {
                bail!(
                    "{verb} {completed}/{} items. {}",
                    sources.len(),
                    errors.join("; ")
                );
            }
            Ok(format!("{verb} {completed} item(s)"))
        }
        Operation::Trash(paths) => {
            let mut completed = 0;
            let mut errors = Vec::new();
            for path in &paths {
                match trash_delete(path) {
                    Ok(_) => completed += 1,
                    Err(e) => errors.push(format!("{}: {e}", path.display())),
                }
            }
            if !errors.is_empty() {
                bail!(
                    "Trashed {completed}/{} items. {}",
                    paths.len(),
                    errors.join("; ")
                );
            }
            Ok(format!("Sent {completed} item(s) to trash"))
        }
        Operation::Rename { source, name } => {
            validate_name(&name)?;
            let parent = source.parent().context("Cannot rename a filesystem root")?;
            let target = parent.join(name);
            if source == target {
                return Ok("Name unchanged".into());
            }
            ensure_absent(&target)?;
            fs::rename(&source, &target)
                .with_context(|| format!("Could not rename {}", source.display()))?;
            Ok(format!(
                "Renamed to {}",
                target.file_name().unwrap_or_default().to_string_lossy()
            ))
        }
        Operation::BulkRename(changes) => bulk_rename(changes, None),
        Operation::CreateArchive {
            sources,
            destination,
        } => {
            archive::create(&destination, &sources, &mut NoProgress)?;
            Ok(format!("Created archive {}", destination.display()))
        }
        Operation::ExtractArchives {
            archives,
            destination,
        } => {
            for path in &archives {
                let output = archive::extraction_directory(path, &destination)?;
                archive::extract(path, &output, &mut NoProgress)?;
            }
            Ok(format!("Extracted {} archive(s)", archives.len()))
        }
        Operation::Undo(action) => undo_untracked(action),
        Operation::Mkdir(path) => {
            fs::create_dir(&path)
                .with_context(|| format!("Could not create {}", path.display()))?;
            Ok(format!("Created folder {}", path.display()))
        }
        Operation::Touch(path) => {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            Ok(format!("Created file {}", path.display()))
        }
    }
}

fn undo_untracked(action: UndoAction) -> Result<String> {
    let label = action.label().to_owned();
    match action {
        UndoAction::Rename { changes, .. } => {
            bulk_rename(changes, None)?;
        }
        UndoAction::Filesystem {
            remove,
            restore,
            restore_renames,
            remove_nonempty_directories,
            ..
        } => {
            for path in remove.iter().rev() {
                let metadata = match fs::symlink_metadata(path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if metadata.is_dir()
                    && !remove_nonempty_directories
                    && fs::read_dir(path)?.next().is_some()
                {
                    bail!("Cannot undo {label}: {} is no longer empty", path.display());
                }
                let _ = trash_delete(path)?;
            }
            for (_, destination) in &restore_renames {
                ensure_absent(destination)?;
            }
            restore_trash(restore)?;
            for (restored, destination) in restore_renames {
                fs::rename(&restored, &destination).with_context(|| {
                    format!(
                        "Restored {}, but could not restore its original filename {}",
                        restored.display(),
                        destination.display()
                    )
                })?;
            }
        }
    }
    Ok(format!("Undid {label}"))
}

pub fn validate_name(name: &str) -> Result<()> {
    let mut components = Path::new(name).components();
    if name.is_empty()
        || name.contains(['/', '\\', '\0'])
        || name.chars().any(char::is_control)
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        bail!("Enter a single filename (no path separators, . or ..)");
    }
    #[cfg(windows)]
    if name.contains([':', '*', '?', '"', '<', '>', '|']) || name.ends_with([' ', '.']) {
        bail!("That name contains characters Windows does not allow");
    }
    Ok(())
}

#[derive(Debug)]
struct StagedRename {
    source: PathBuf,
    temporary: PathBuf,
    target: PathBuf,
}

static RENAME_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn bulk_rename(
    changes: Vec<(PathBuf, PathBuf)>,
    mut tracker: Option<&mut Tracker>,
) -> Result<String> {
    let changes: Vec<_> = changes
        .into_iter()
        .filter(|(source, target)| source != target)
        .collect();
    if changes.is_empty() {
        return Ok("No filenames changed".into());
    }
    if let Some(tracker) = tracker.as_deref_mut() {
        tracker.set_totals(changes.len(), 0);
    }

    let mut source_keys = HashSet::with_capacity(changes.len());
    let mut target_keys = HashSet::with_capacity(changes.len());
    for (source, target) in &changes {
        if source.parent() != target.parent() {
            bail!("Bulk rename cannot move items between folders");
        }
        let target_name = target
            .file_name()
            .and_then(|name| name.to_str())
            .context("Bulk rename requires filenames that can be represented as text")?;
        validate_name(target_name)?;
        fs::symlink_metadata(source)
            .with_context(|| format!("Cannot access {}", source.display()))?;
        if !source_keys.insert(rename_path_key(source)) {
            bail!("The rename plan contains the same source more than once");
        }
        if !target_keys.insert(rename_path_key(target)) {
            bail!("Two items cannot be renamed to the same filename");
        }
    }
    for (_, target) in &changes {
        match fs::symlink_metadata(target) {
            Ok(_) if !source_keys.contains(&rename_path_key(target)) => {
                bail!("Destination already exists: {}", target.display())
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot check {}", target.display()))
            }
        }
    }

    let batch = RENAME_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let mut staged = Vec::with_capacity(changes.len());
    for (index, (source, target)) in changes.into_iter().enumerate() {
        if let Some(tracker) = tracker.as_deref_mut() {
            if let Err(error) = tracker.check() {
                let rollback = rollback_bulk_rename(&staged, 0);
                return bulk_rename_failure(error, rollback);
            }
            tracker.current(&source);
        }
        let parent = source.parent().context("Cannot rename a filesystem root")?;
        let temporary = (0..10_000)
            .map(|attempt| {
                parent.join(format!(
                    ".zuru-rename-{}-{batch}-{index}-{attempt}",
                    std::process::id()
                ))
            })
            .find(|candidate| {
                !source_keys.contains(&rename_path_key(candidate))
                    && !target_keys.contains(&rename_path_key(candidate))
                    && matches!(path_exists(candidate), Ok(false))
            });
        let Some(temporary) = temporary else {
            let rollback = rollback_bulk_rename(&staged, 0);
            return bulk_rename_failure(
                anyhow::anyhow!("Could not reserve a temporary filename for bulk rename"),
                rollback,
            );
        };
        if let Err(error) = fs::rename(&source, &temporary) {
            let rollback = rollback_bulk_rename(&staged, 0);
            return bulk_rename_failure(
                anyhow::Error::new(error).context(format!("Could not stage {}", source.display())),
                rollback,
            );
        }
        staged.push(StagedRename {
            source,
            temporary,
            target,
        });
    }

    let mut finalized = 0;
    while finalized < staged.len() {
        if let Some(tracker) = tracker.as_deref_mut() {
            if let Err(error) = tracker.check() {
                let rollback = rollback_bulk_rename(&staged, finalized);
                return bulk_rename_failure(error, rollback);
            }
            tracker.current(&staged[finalized].source);
        }
        let item = &staged[finalized];
        match path_exists(&item.target) {
            Ok(false) => {}
            Ok(true) => {
                let rollback = rollback_bulk_rename(&staged, finalized);
                return bulk_rename_failure(
                    anyhow::anyhow!(
                        "Destination appeared while renaming: {}",
                        item.target.display()
                    ),
                    rollback,
                );
            }
            Err(error) => {
                let rollback = rollback_bulk_rename(&staged, finalized);
                return bulk_rename_failure(
                    error.context(format!("Cannot check {}", item.target.display())),
                    rollback,
                );
            }
        }
        if let Err(error) = fs::rename(&item.temporary, &item.target) {
            let rollback = rollback_bulk_rename(&staged, finalized);
            return bulk_rename_failure(
                anyhow::Error::new(error).context(format!(
                    "Could not rename {} to {}",
                    item.source.display(),
                    item.target.display()
                )),
                rollback,
            );
        }
        finalized += 1;
        if let Some(tracker) = tracker.as_deref_mut() {
            tracker.complete_item(&item.target);
        }
    }
    Ok(format!("Renamed {} item(s)", staged.len()))
}

fn rollback_bulk_rename(staged: &[StagedRename], finalized: usize) -> Vec<String> {
    let mut errors = Vec::new();
    for item in staged.iter().take(finalized) {
        match path_exists(&item.temporary) {
            Ok(false) => {
                if let Err(error) = fs::rename(&item.target, &item.temporary) {
                    errors.push(format!("{}: {error}", item.target.display()));
                }
            }
            Ok(true) => errors.push(format!(
                "temporary rollback path is occupied: {}",
                item.temporary.display()
            )),
            Err(error) => errors.push(format!("{}: {error}", item.temporary.display())),
        }
    }
    for item in staged.iter().rev() {
        if fs::symlink_metadata(&item.temporary).is_ok() {
            match path_exists(&item.source) {
                Ok(false) => {
                    if let Err(error) = fs::rename(&item.temporary, &item.source) {
                        errors.push(format!("{}: {error}", item.source.display()));
                    }
                }
                Ok(true) => errors.push(format!(
                    "original path is occupied; recovery copy remains at {}",
                    item.temporary.display()
                )),
                Err(error) => errors.push(format!("{}: {error}", item.source.display())),
            }
        }
    }
    errors
}

fn bulk_rename_failure(error: anyhow::Error, rollback: Vec<String>) -> Result<String> {
    if rollback.is_empty() {
        Err(error.context("Bulk rename was rolled back"))
    } else {
        Err(error.context(format!(
            "Bulk rename rollback was incomplete: {}",
            rollback.join("; ")
        )))
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

fn ensure_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!(
            "Destination exists; nothing was overwritten: {}",
            path.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn copy_into(source: &Path, destination: &Path) -> Result<PathBuf> {
    let source = absolute_preserving_link(source)?;
    let destination =
        fs::canonicalize(destination).context("Cannot access destination directory")?;
    let target = destination.join(
        source
            .file_name()
            .context("Cannot copy a filesystem root")?,
    );
    ensure_absent(&target)?;
    let meta = fs::symlink_metadata(&source)?;
    if meta.is_symlink() {
        bail!("Copy/move of symbolic links is not supported in v1");
    }
    if !meta.is_dir() && !meta.is_file() {
        bail!("Special files cannot be copied");
    }
    if meta.is_dir() && destination.starts_with(&source) {
        bail!("Cannot copy a directory into itself or one of its descendants");
    }
    if meta.is_file() {
        copy_file(&source, &target)?;
    } else {
        // Preflight the entire tree before creating anything. Never follow links outside it.
        let paths: Vec<_> = WalkDir::new(&source)
            .follow_links(false)
            .into_iter()
            .collect::<std::result::Result<_, _>>()?;
        for entry in &paths {
            if !entry.file_type().is_dir() && !entry.file_type().is_file() {
                bail!(
                    "Tree contains a link or special file: {}",
                    entry.path().display()
                );
            }
        }
        fs::create_dir(&target)?;
        for entry in paths.into_iter().skip(1) {
            let path = target.join(entry.path().strip_prefix(&source)?);
            let result = if entry.file_type().is_dir() {
                fs::create_dir(&path).map_err(anyhow::Error::from)
            } else {
                copy_file(entry.path(), &path)
            };
            result.with_context(|| {
                format!(
                    "Copy incomplete; source is intact, partial destination kept at {}",
                    target.display()
                )
            })?;
        }
    }
    Ok(target)
}

fn absolute_preserving_link(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = fs::canonicalize(absolute.parent().context("Missing parent")?)?;
    Ok(parent.join(absolute.file_name().context("Missing filename")?))
}

fn copy_file(source: &Path, target: &Path) -> Result<()> {
    let mut input = fs::File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    io::copy(&mut input, &mut output).with_context(|| {
        format!(
            "Copy incomplete; source is intact, partial file kept at {}",
            target.display()
        )
    })?;
    output.sync_all()?;
    fs::set_permissions(target, input.metadata()?.permissions())?;
    Ok(())
}
