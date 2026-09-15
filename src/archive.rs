use anyhow::{bail, Context, Result};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use walkdir::WalkDir;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

const PREVIEW_LIMIT: usize = 2_000;
const ENTRY_LIMIT: usize = 100_000;
const EXTRACTED_SIZE_LIMIT: u64 = 16 * 1024 * 1024 * 1024;
static ARCHIVE_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    Tar,
    TarGz,
}

impl ArchiveFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Zip => "ZIP",
            Self::Tar => "TAR",
            Self::TarGz => "TAR.GZ",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct ArchiveListing {
    pub format: ArchiveFormat,
    pub entries: Vec<ArchiveEntry>,
    pub total_entries: usize,
    pub total_bytes: u64,
    pub truncated: bool,
}

pub trait Progress {
    fn check(&self) -> Result<()>;
    fn current(&mut self, path: &Path);
    fn add_bytes(&mut self, bytes: u64);
}

pub fn format(path: &Path) -> Option<ArchiveFormat> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Some(ArchiveFormat::TarGz)
    } else if name.ends_with(".tar") {
        Some(ArchiveFormat::Tar)
    } else if name.ends_with(".zip") {
        Some(ArchiveFormat::Zip)
    } else {
        None
    }
}

pub fn listing(path: &Path) -> Result<ArchiveListing> {
    listing_with_check(path, || Ok(()))
}

pub fn listing_with_check(
    path: &Path,
    mut check: impl FnMut() -> Result<()>,
) -> Result<ArchiveListing> {
    check()?;
    match format(path).context("Supported archives are .zip, .tar, .tar.gz, and .tgz")? {
        ArchiveFormat::Zip => list_zip(path, &mut check),
        ArchiveFormat::Tar => list_tar(path, ArchiveFormat::Tar, File::open(path)?, &mut check),
        ArchiveFormat::TarGz => list_tar(
            path,
            ArchiveFormat::TarGz,
            GzDecoder::new(File::open(path)?),
            &mut check,
        ),
    }
}

fn list_zip(path: &Path, check: &mut impl FnMut() -> Result<()>) -> Result<ArchiveListing> {
    let mut archive = ZipArchive::new(File::open(path)?)
        .with_context(|| format!("Could not read ZIP archive {}", path.display()))?;
    let total_entries = archive.len();
    let mut total_bytes = 0u64;
    let mut entries = Vec::with_capacity(total_entries.min(PREVIEW_LIMIT));
    for index in 0..total_entries.min(ENTRY_LIMIT) {
        check()?;
        let file = archive.by_index(index)?;
        total_bytes = total_bytes.saturating_add(file.size());
        if entries.len() < PREVIEW_LIMIT {
            entries.push(ArchiveEntry {
                path: crate::files::safe_text(file.name()),
                is_dir: file.is_dir(),
                size: file.size(),
            });
        }
    }
    Ok(ArchiveListing {
        format: ArchiveFormat::Zip,
        entries,
        total_entries,
        total_bytes,
        truncated: total_entries > PREVIEW_LIMIT || total_entries > ENTRY_LIMIT,
    })
}

fn list_tar<R: Read>(
    path: &Path,
    format: ArchiveFormat,
    reader: R,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<ArchiveListing> {
    let mut archive = tar::Archive::new(reader);
    let mut entries = Vec::new();
    let mut total_entries = 0usize;
    let mut total_bytes = 0u64;
    let mut truncated = false;
    for entry in archive
        .entries()
        .with_context(|| format!("Could not read TAR archive {}", path.display()))?
    {
        check()?;
        if total_entries >= ENTRY_LIMIT {
            truncated = true;
            break;
        }
        let entry = entry?;
        let entry_path = entry.path()?;
        total_entries += 1;
        total_bytes = total_bytes.saturating_add(entry.size());
        if entries.len() < PREVIEW_LIMIT {
            entries.push(ArchiveEntry {
                path: crate::files::safe_text(&entry_path.to_string_lossy().replace('\\', "/")),
                is_dir: entry.header().entry_type().is_dir(),
                size: entry.size(),
            });
        } else {
            truncated = true;
        }
    }
    Ok(ArchiveListing {
        format,
        entries,
        total_entries,
        total_bytes,
        truncated,
    })
}

pub fn create(path: &Path, sources: &[PathBuf], progress: &mut impl Progress) -> Result<()> {
    let archive_format =
        format(path).context("Archive name must end in .zip, .tar, .tar.gz, or .tgz")?;
    ensure_absent(path)?;
    let entries = collect_sources(sources, progress)?;
    let parent = path.parent().context("Archive destination has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".zuru-archive-{}-{}.tmp",
        std::process::id(),
        ARCHIVE_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let result = match archive_format {
        ArchiveFormat::Zip => write_zip(&temporary, &entries, progress),
        ArchiveFormat::Tar => {
            let file = create_new(&temporary)?;
            write_tar(file, &entries, progress).map(|_| ())
        }
        ArchiveFormat::TarGz => {
            let file = create_new(&temporary)?;
            let encoder = GzEncoder::new(file, Compression::default());
            let encoder = write_tar(encoder, &entries, progress)?;
            encoder.finish()?;
            Ok(())
        }
    };
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error).context("Archive creation failed; the incomplete archive was removed");
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("Could not save archive to {}", path.display()));
    }
    Ok(())
}

#[derive(Clone)]
struct SourceEntry {
    source: PathBuf,
    archive_path: String,
    is_dir: bool,
}

fn collect_sources(sources: &[PathBuf], progress: &mut impl Progress) -> Result<Vec<SourceEntry>> {
    if sources.is_empty() {
        bail!("Select at least one item to archive");
    }
    let mut entries = Vec::new();
    let mut names = HashSet::new();
    for source in sources {
        progress.check()?;
        let source = fs::canonicalize(source)
            .with_context(|| format!("Cannot access {}", source.display()))?;
        let parent = source
            .parent()
            .context("Cannot archive a filesystem root")?;
        for entry in WalkDir::new(&source).follow_links(false) {
            progress.check()?;
            let entry = entry?;
            if entry.file_type().is_symlink() {
                bail!("Archives containing symbolic links are not supported");
            }
            if !entry.file_type().is_dir() && !entry.file_type().is_file() {
                bail!("Archives containing special files are not supported");
            }
            let relative = entry.path().strip_prefix(parent)?;
            let archive_path = portable_archive_path(relative)?;
            if !names.insert(archive_path.to_lowercase()) {
                bail!("Selected items produce the same archive path: {archive_path}");
            }
            entries.push(SourceEntry {
                source: entry.path().to_path_buf(),
                archive_path,
                is_dir: entry.file_type().is_dir(),
            });
            if entries.len() > ENTRY_LIMIT {
                bail!("Archive contains more than {ENTRY_LIMIT} entries");
            }
        }
    }
    entries.sort_by(|a, b| a.archive_path.cmp(&b.archive_path));
    Ok(entries)
}

fn portable_archive_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            bail!("Archive paths must be relative");
        };
        let part = part
            .to_str()
            .context("ZIP creation requires filenames that can be represented as UTF-8")?;
        if part.chars().any(char::is_control) {
            bail!("Archive paths cannot contain control characters");
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

fn write_zip(path: &Path, entries: &[SourceEntry], progress: &mut impl Progress) -> Result<()> {
    let output = create_new(path)?;
    let mut writer = ZipWriter::new(output);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let directory_options = SimpleFileOptions::default().unix_permissions(0o755);
    for entry in entries {
        progress.check()?;
        progress.current(&entry.source);
        if entry.is_dir {
            writer.add_directory(format!("{}/", entry.archive_path), directory_options)?;
        } else {
            writer.start_file(&entry.archive_path, options)?;
            copy_progress(File::open(&entry.source)?, &mut writer, progress)?;
        }
    }
    writer.finish()?.sync_all()?;
    Ok(())
}

fn write_tar<W: Write>(
    writer: W,
    entries: &[SourceEntry],
    progress: &mut impl Progress,
) -> Result<W> {
    let mut archive = tar::Builder::new(writer);
    for entry in entries {
        progress.check()?;
        progress.current(&entry.source);
        if entry.is_dir {
            archive.append_dir(&entry.archive_path, &entry.source)?;
        } else {
            let metadata = fs::metadata(&entry.source)?;
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&metadata);
            header.set_cksum();
            let file = File::open(&entry.source)?;
            let mut reader = ProgressReader {
                inner: file,
                progress,
            };
            archive.append_data(&mut header, &entry.archive_path, &mut reader)?;
        }
    }
    archive.finish()?;
    Ok(archive.into_inner()?)
}

pub fn extraction_directory(path: &Path, parent: &Path) -> Result<PathBuf> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("Archive filename cannot be represented as text")?;
    let lower = filename.to_ascii_lowercase();
    let suffix = [".tar.gz", ".tgz", ".tar", ".zip"]
        .into_iter()
        .find(|suffix| lower.ends_with(suffix))
        .context("Unsupported archive type")?;
    let stem = &filename[..filename.len() - suffix.len()];
    let stem = if stem.is_empty() { "archive" } else { stem };
    Ok(parent.join(stem))
}

pub fn extract(path: &Path, destination: &Path, progress: &mut impl Progress) -> Result<()> {
    ensure_absent(destination)?;
    fs::create_dir(destination)?;
    let result = match format(path).context("Unsupported archive type")? {
        ArchiveFormat::Zip => extract_zip(path, destination, progress),
        ArchiveFormat::Tar => extract_tar(File::open(path)?, destination, progress),
        ArchiveFormat::TarGz => {
            extract_tar(GzDecoder::new(File::open(path)?), destination, progress)
        }
    };
    if let Err(error) = result {
        return Err(error).with_context(|| {
            format!(
                "Extraction incomplete; partial files were kept at {}",
                destination.display()
            )
        });
    }
    Ok(())
}

fn extract_zip(path: &Path, destination: &Path, progress: &mut impl Progress) -> Result<()> {
    let mut archive = ZipArchive::new(File::open(path)?)?;
    if archive.len() > ENTRY_LIMIT {
        bail!("Archive contains more than {ENTRY_LIMIT} entries");
    }
    let mut extracted = 0u64;
    for index in 0..archive.len() {
        progress.check()?;
        let mut entry = archive.by_index(index)?;
        let relative = entry
            .enclosed_name()
            .context("Archive contains an unsafe path")?
            .to_path_buf();
        validate_extracted_path(&relative)?;
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            bail!("Archive contains a symbolic link, which Zuru will not extract");
        }
        extracted = extracted.saturating_add(entry.size());
        if extracted > EXTRACTED_SIZE_LIMIT {
            bail!("Archive expands beyond the 16 GB safety limit");
        }
        let target = destination.join(relative);
        progress.current(&target);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = create_new(&target)?;
            copy_progress(&mut entry, &mut output, progress)?;
        }
    }
    Ok(())
}

fn extract_tar<R: Read>(reader: R, destination: &Path, progress: &mut impl Progress) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    let mut count = 0usize;
    let mut extracted = 0u64;
    for entry in archive.entries()? {
        progress.check()?;
        count += 1;
        if count > ENTRY_LIMIT {
            bail!("Archive contains more than {ENTRY_LIMIT} entries");
        }
        let mut entry = entry?;
        let relative = entry.path()?.to_path_buf();
        validate_extracted_path(&relative)?;
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            bail!("Archive contains a link or special file, which Zuru will not extract");
        }
        extracted = extracted.saturating_add(entry.size());
        if extracted > EXTRACTED_SIZE_LIMIT {
            bail!("Archive expands beyond the 16 GB safety limit");
        }
        let target = destination.join(relative);
        progress.current(&target);
        if kind.is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = create_new(&target)?;
            copy_progress(&mut entry, &mut output, progress)?;
        }
    }
    Ok(())
}

fn validate_extracted_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        bail!("Archive contains an unsafe path: {}", path.display());
    }
    Ok(())
}

struct ProgressReader<'a, R, P> {
    inner: R,
    progress: &'a mut P,
}

impl<R: Read, P: Progress> Read for ProgressReader<'_, R, P> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.progress
            .check()
            .map_err(|error| io::Error::other(format!("{error:#}")))?;
        let read = self.inner.read(buffer)?;
        self.progress.add_bytes(read as u64);
        Ok(read)
    }
}

fn copy_progress(
    mut input: impl Read,
    mut output: impl Write,
    progress: &mut impl Progress,
) -> Result<u64> {
    let mut buffer = vec![0; 256 * 1024];
    let mut total = 0u64;
    loop {
        progress.check()?;
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        progress.add_bytes(read as u64);
        total = total.saturating_add(read as u64);
    }
    Ok(total)
}

fn create_new(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("Could not create {}", path.display()))
}

fn ensure_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("Destination already exists: {}", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
