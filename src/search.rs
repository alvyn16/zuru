use crate::{
    files::Entry,
    worker::{latest_channel, CancellationToken, LatestSender},
};
use anyhow::{Context, Result};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
};

const NAME_RESULT_LIMIT: usize = 10_000;
const CONTENT_RESULT_LIMIT: usize = 5_000;
const CONTENT_READ_LIMIT: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchKind {
    Name,
    Contents,
}

impl SearchKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "NAME",
            Self::Contents => "TEXT",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchView {
    pub root: PathBuf,
    pub query: String,
    pub kind: SearchKind,
    pub scanned: usize,
    pub truncated: bool,
}

pub struct SearchRequest {
    pub generation: u64,
    pub root: PathBuf,
    pub query: String,
    pub kind: SearchKind,
    pub hidden: bool,
}

pub struct SearchResult {
    pub generation: u64,
    pub root: PathBuf,
    pub query: String,
    pub kind: SearchKind,
    pub entries: std::result::Result<Vec<Entry>, String>,
    pub scanned: usize,
    pub truncated: bool,
}

pub fn spawn_searcher() -> (LatestSender<SearchRequest>, Receiver<SearchResult>) {
    let (tx, rx) = latest_channel::<SearchRequest>();
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Some((request, token)) = rx.recv_cancellable() {
            let outcome = search(&request, &token);
            if token.is_cancelled() {
                continue;
            }
            let (entries, scanned, truncated) = match outcome {
                Ok(found) => (Ok(found.entries), found.scanned, found.truncated),
                Err(error) => (Err(format!("{error:#}")), 0, false),
            };
            if result_tx
                .send(SearchResult {
                    generation: request.generation,
                    root: request.root,
                    query: request.query,
                    kind: request.kind,
                    entries,
                    scanned,
                    truncated,
                })
                .is_err()
            {
                break;
            }
        }
    });
    (tx, result_rx)
}

struct Found {
    entries: Vec<Entry>,
    scanned: usize,
    truncated: bool,
}

fn search(request: &SearchRequest, token: &CancellationToken) -> Result<Found> {
    let query = request.query.to_lowercase();
    anyhow::ensure!(!query.is_empty(), "Enter something to search for");
    std::fs::read_dir(&request.root)
        .with_context(|| format!("Cannot search {}", request.root.display()))?;
    let limit = match request.kind {
        SearchKind::Name => NAME_RESULT_LIMIT,
        SearchKind::Contents => CONTENT_RESULT_LIMIT,
    };
    let mut entries = Vec::new();
    let mut scanned = 0;
    let mut truncated = false;
    let mut builder = ignore::WalkBuilder::new(&request.root);
    builder
        .hidden(!request.hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false);
    for child in builder.build() {
        token.check()?;
        let Ok(child) = child else {
            continue;
        };
        if child.depth() == 0 {
            continue;
        }
        scanned += 1;
        let matches = match request.kind {
            SearchKind::Name => child
                .file_name()
                .to_string_lossy()
                .to_lowercase()
                .contains(&query),
            SearchKind::Contents => {
                child.file_type().is_some_and(|kind| kind.is_file())
                    && content_matches(child.path(), &query, token)
            }
        };
        if matches {
            if let Ok(entry) = Entry::read(child.path().to_path_buf()) {
                entries.push(entry);
            }
            if entries.len() >= limit {
                truncated = true;
                break;
            }
        }
    }
    entries.sort_by_cached_key(|entry| {
        entry
            .path
            .strip_prefix(&request.root)
            .unwrap_or(&entry.path)
            .to_string_lossy()
            .to_lowercase()
    });
    Ok(Found {
        entries,
        scanned,
        truncated,
    })
}

fn content_matches(path: &Path, query: &str, token: &CancellationToken) -> bool {
    let mut bytes = Vec::new();
    let result = File::open(path).and_then(|file| {
        file.take(CONTENT_READ_LIMIT)
            .read_to_end(&mut bytes)
            .map(|_| ())
    });
    if result.is_err() || token.is_cancelled() || bytes.contains(&0) {
        return false;
    }
    let controls = bytes
        .iter()
        .filter(|&&byte| byte < 32 && ![9, 10, 13].contains(&byte))
        .count();
    if controls > bytes.len() / 100 {
        return false;
    }
    String::from_utf8_lossy(&bytes)
        .to_lowercase()
        .contains(query)
}
