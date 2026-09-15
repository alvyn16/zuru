use crate::worker::{latest_channel, CancellationToken, LatestSender};
use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
    time::SystemTime,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub permissions: String,
    pub mime: String,
}

impl Entry {
    pub fn read(path: PathBuf) -> Result<Self> {
        let link = fs::symlink_metadata(&path)?;
        let meta = if link.is_symlink() {
            fs::metadata(&path).unwrap_or_else(|_| link.clone())
        } else {
            link.clone()
        };
        Ok(Self {
            name: safe_text(
                &path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy(),
            ),
            is_dir: meta.is_dir(),
            is_symlink: link.is_symlink(),
            size: meta.len(),
            modified: meta.modified().ok(),
            permissions: permissions(&meta, link.is_symlink()),
            mime: mime_guess::from_path(&path)
                .first_or_octet_stream()
                .to_string(),
            path,
        })
    }

    pub fn icon(&self) -> &'static str {
        if self.is_symlink {
            return "";
        }
        if self.is_dir {
            return "";
        }
        if self.mime.starts_with("image/") {
            return "";
        }
        let ext = self
            .path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "rs" => "",
            "js" | "jsx" | "ts" | "tsx" => "",
            "py" => "",
            "go" => "",
            "html" | "css" | "scss" | "vue" | "svelte" => "",
            "json" | "toml" | "yaml" | "yml" | "xml" => "",
            "zip" | "tar" | "gz" | "7z" | "rar" | "xz" | "bz2" => "",
            "md" => "",
            "pdf" => "",
            "mp3" | "flac" | "wav" | "ogg" => "",
            "mp4" | "mkv" | "mov" => "",
            "exe" | "dll" | "so" => "",
            "sh" | "ps1" | "bat" => "",
            _ => "",
        }
    }
}

/// Never let control characters from filenames/content become terminal escape sequences.
pub fn safe_text(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '�' } else { c })
        .collect()
}

pub fn read_dir(path: &Path, hidden: bool) -> Result<Vec<Entry>> {
    read_directory(path, hidden, None)
}

fn read_directory(
    path: &Path,
    hidden: bool,
    token: Option<&CancellationToken>,
) -> Result<Vec<Entry>> {
    // Open explicitly so inaccessible directories report an error, not an empty listing.
    fs::read_dir(path).with_context(|| format!("Cannot read {}", path.display()))?;
    let mut entries = Vec::new();
    for child in jwalk::WalkDir::new(path)
        .skip_hidden(false)
        .follow_links(false)
        .max_depth(1)
        .min_depth(1)
    {
        if let Some(token) = token {
            token.check()?;
        }
        let child = child?;
        if !hidden && child.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if let Ok(entry) = Entry::read(child.path()) {
            entries.push(entry);
        }
    }
    sort_entries(&mut entries);
    Ok(entries)
}

fn sort_entries(entries: &mut [Entry]) {
    // Compute folded names once rather than allocate strings at every comparison.
    entries
        .sort_by_cached_key(|entry| (!entry.is_dir, entry.name.to_lowercase(), entry.name.clone()));
}

/// A directory preview only needs names and types, not a stat call per child.
pub fn read_preview_dir(
    path: &Path,
    hidden: bool,
    token: Option<&CancellationToken>,
) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for child in fs::read_dir(path).with_context(|| format!("Cannot read {}", path.display()))? {
        if let Some(token) = token {
            token.check()?;
        }
        let child = child?;
        if !hidden && child.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let kind = child.file_type()?;
        let path = child.path();
        entries.push(Entry {
            name: safe_text(&child.file_name().to_string_lossy()),
            is_dir: kind.is_dir(),
            is_symlink: kind.is_symlink(),
            size: 0,
            modified: None,
            permissions: String::new(),
            mime: mime_guess::from_path(&path)
                .first_or_octet_stream()
                .to_string(),
            path,
        });
    }
    sort_entries(&mut entries);
    Ok(entries)
}

#[cfg(unix)]
fn permissions(meta: &fs::Metadata, link: bool) -> String {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode();
    let mut s = String::from(if link {
        "l"
    } else if meta.is_dir() {
        "d"
    } else {
        "-"
    });
    for (i, c) in "rwxrwxrwx".chars().enumerate() {
        s.push(if mode & (1 << (8 - i)) != 0 { c } else { '-' });
    }
    for (bit, index, yes, no) in [
        (0o4000, 3, 's', 'S'),
        (0o2000, 6, 's', 'S'),
        (0o1000, 9, 't', 'T'),
    ] {
        if mode & bit != 0 {
            let old = s.as_bytes()[index];
            s.replace_range(
                index..index + 1,
                &if old == b'x' { yes } else { no }.to_string(),
            );
        }
    }
    s
}

#[cfg(not(unix))]
fn permissions(meta: &fs::Metadata, link: bool) -> String {
    // Windows has ACLs; this is a readable approximation, never an ACL claim.
    format!(
        "{}{}",
        if link {
            "l"
        } else if meta.is_dir() {
            "d"
        } else {
            "-"
        },
        if meta.permissions().readonly() {
            "r--r--r--"
        } else {
            "rw-rw-rw-"
        }
    )
}

pub fn human_size(size: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

pub struct DirectoryRequest {
    pub generation: u64,
    pub path: PathBuf,
    pub hidden: bool,
}
pub struct DirectoryResult {
    pub generation: u64,
    pub path: PathBuf,
    pub entries: Result<Vec<Entry>>,
    pub parent: Vec<Entry>,
}

pub fn spawn_reader() -> (LatestSender<DirectoryRequest>, Receiver<DirectoryResult>) {
    let (tx, rx) = latest_channel::<DirectoryRequest>();
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Some((req, token)) = rx.recv_cancellable() {
            let entries = read_directory(&req.path, req.hidden, Some(&token));
            if token.is_cancelled() {
                continue;
            }
            let parent = req
                .path
                .parent()
                .and_then(|p| read_preview_dir(p, req.hidden, Some(&token)).ok())
                .unwrap_or_default();
            if token.is_cancelled() {
                continue;
            }
            if result_tx
                .send(DirectoryResult {
                    generation: req.generation,
                    path: req.path,
                    entries,
                    parent,
                })
                .is_err()
            {
                break;
            }
        }
    });
    (tx, result_rx)
}
