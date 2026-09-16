use crate::{
    archive::{self, ArchiveListing},
    cache::Cache,
    config::Config,
    files::{self, Entry},
    worker::{latest_channel, CancellationToken, LatestSender},
};
use anyhow::{Context, Result};
use image::{imageops::FilterType, DynamicImage, ImageDecoder, ImageReader};
use ratatui::{
    layout::Size,
    style::{Color, Style},
    text::{Line, Span},
};
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::Protocol,
    Resize,
};
use std::{
    collections::hash_map::DefaultHasher,
    fs::{self, File},
    hash::{Hash, Hasher},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, OnceLock,
    },
    thread,
    time::SystemTime,
};
use syntect::{easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet};

#[derive(Clone)]
pub enum Preview {
    Empty,
    Loading,
    Image {
        protocol: Arc<Protocol>,
        width: u32,
        height: u32,
        max_scroll: usize,
        offset: usize,
        zoom: bool,
    },
    Text {
        lines: Arc<Vec<Line<'static>>>,
        truncated: bool,
        language: String,
    },
    Directory(Vec<Entry>),
    Archive(ArchiveListing),
    Metadata {
        entry: Entry,
        note: String,
    },
    Error(String),
}

impl Preview {
    pub fn max_scroll(&self, height: usize) -> usize {
        match self {
            Self::Image { max_scroll, .. } => *max_scroll,
            Self::Text { lines, .. } => lines.len().saturating_sub(height),
            Self::Directory(entries) => entries.len().saturating_sub(height),
            Self::Archive(listing) => listing
                .entries
                .len()
                .saturating_sub(height.saturating_sub(1)),
            _ => 0,
        }
    }
    pub fn title(&self) -> String {
        match self {
            Self::Image {
                width,
                height,
                zoom,
                ..
            } => format!(
                "IMAGE  {width} × {height}  {}",
                if *zoom { "WIDTH" } else { "FIT" }
            ),
            Self::Text {
                language,
                truncated,
                ..
            } => format!(
                "{}{}",
                language.to_uppercase(),
                if *truncated { " · LIMITED" } else { "" }
            ),
            Self::Directory(entries) => format!("DIRECTORY  {} items", entries.len()),
            Self::Archive(listing) => format!(
                "{} ARCHIVE  {} items{}",
                listing.format.label(),
                listing.total_entries,
                if listing.truncated { " · LIMITED" } else { "" }
            ),
            Self::Metadata { .. } => "FILE DETAILS".into(),
            _ => "PREVIEW".into(),
        }
    }
}

pub struct PreviewRequest {
    pub generation: u64,
    pub entry: Entry,
    pub size: Size,
    pub scroll: usize,
    pub zoom: bool,
    pub hidden: bool,
    pub force: bool,
    pub preload: Vec<Entry>,
}
pub struct PreviewResult {
    pub generation: u64,
    pub preview: Preview,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct FileStamp {
    path: PathBuf,
    modified: Option<SystemTime>,
    size: u64,
}

#[derive(Clone, PartialEq, Eq)]
struct PreviewKey {
    file: FileStamp,
    viewport: Option<(u16, u16, bool, usize)>,
}
#[derive(Clone, PartialEq, Eq)]
struct WidthKey {
    file: FileStamp,
    width: u32,
}

struct HighlightAssets {
    syntax: SyntaxSet,
    themes: ThemeSet,
}
static HIGHLIGHTING: OnceLock<HighlightAssets> = OnceLock::new();
static THUMBNAIL_WRITE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default, Clone, Copy)]
pub struct PreviewStats {
    pub image_decodes: usize,
    pub image_encodes: usize,
    pub text_builds: usize,
    pub cache_hits: usize,
    pub disk_cache_hits: usize,
}

struct ThumbnailWrite {
    path: PathBuf,
    image: DynamicImage,
    original_dimensions: (u32, u32),
}

struct CachedThumbnail {
    image: DynamicImage,
    original_dimensions: Option<(u32, u32)>,
}

struct ThumbnailCache {
    directory: PathBuf,
    writer: Sender<ThumbnailWrite>,
}

impl ThumbnailCache {
    fn new(directory: PathBuf, budget: u64) -> Self {
        let (writer, receiver) = mpsc::channel::<ThumbnailWrite>();
        let writer_directory = directory.clone();
        thread::spawn(move || {
            let _ = fs::create_dir_all(&writer_directory);
            trim_thumbnail_cache(&writer_directory, budget);
            while let Ok(write) = receiver.recv() {
                let write_id = THUMBNAIL_WRITE_ID.fetch_add(1, Ordering::Relaxed);
                let temporary = write.path.with_extension(format!("qoi.{write_id}.tmp"));
                let dimensions = thumbnail_dimensions_path(&write.path);
                let temporary_dimensions = dimensions.with_extension(format!("dim.{write_id}.tmp"));
                let image_saved = write
                    .image
                    .save_with_format(&temporary, image::ImageFormat::Qoi)
                    .is_ok();
                let dimensions_saved = fs::write(
                    &temporary_dimensions,
                    encode_thumbnail_dimensions(write.original_dimensions),
                )
                .is_ok();
                if image_saved && dimensions_saved {
                    let _ = fs::remove_file(&write.path);
                    let _ = fs::remove_file(&dimensions);
                    let _ = fs::rename(&temporary, &write.path);
                    let _ = fs::rename(&temporary_dimensions, dimensions);
                } else {
                    let _ = fs::remove_file(&temporary);
                    let _ = fs::remove_file(&temporary_dimensions);
                }
            }
        });
        Self { directory, writer }
    }

    fn path(&self, stamp: &FileStamp, width: u32, height: u32) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        stamp.hash(&mut hasher);
        width.hash(&mut hasher);
        height.hash(&mut hasher);
        self.directory.join(format!("{:016x}.qoi", hasher.finish()))
    }

    fn load(&self, stamp: &FileStamp, width: u32, height: u32) -> Option<CachedThumbnail> {
        let path = self.path(stamp, width, height);
        let decoded = ImageReader::open(&path)
            .ok()
            .and_then(|image| image.with_guessed_format().ok())
            .and_then(|image| image.decode().ok());
        if decoded.is_none() && path.exists() {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(thumbnail_dimensions_path(&path));
        }
        decoded.map(|image| CachedThumbnail {
            image,
            original_dimensions: read_thumbnail_dimensions(&path),
        })
    }

    fn store(
        &self,
        stamp: &FileStamp,
        width: u32,
        height: u32,
        image: &DynamicImage,
        original_dimensions: (u32, u32),
    ) {
        let path = self.path(stamp, width, height);
        if path.exists() {
            return;
        }
        let _ = self.writer.send(ThumbnailWrite {
            path,
            image: image.clone(),
            original_dimensions,
        });
    }

    fn store_dimensions(&self, stamp: &FileStamp, width: u32, height: u32, dimensions: (u32, u32)) {
        let path = thumbnail_dimensions_path(&self.path(stamp, width, height));
        let write_id = THUMBNAIL_WRITE_ID.fetch_add(1, Ordering::Relaxed);
        let temporary = path.with_extension(format!("dim.{write_id}.tmp"));
        if fs::write(&temporary, encode_thumbnail_dimensions(dimensions)).is_ok() {
            let _ = fs::remove_file(&path);
            let _ = fs::rename(&temporary, path);
        } else {
            let _ = fs::remove_file(temporary);
        }
    }

    fn remove(&self, stamp: &FileStamp, width: u32, height: u32) {
        let path = self.path(stamp, width, height);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(thumbnail_dimensions_path(&path));
    }
}

fn thumbnail_dimensions_path(path: &Path) -> PathBuf {
    path.with_extension("qoi.dim")
}

fn encode_thumbnail_dimensions((width, height): (u32, u32)) -> [u8; 8] {
    let mut encoded = [0; 8];
    encoded[..4].copy_from_slice(&width.to_le_bytes());
    encoded[4..].copy_from_slice(&height.to_le_bytes());
    encoded
}

fn read_thumbnail_dimensions(path: &Path) -> Option<(u32, u32)> {
    let encoded: [u8; 8] = fs::read(thumbnail_dimensions_path(path))
        .ok()?
        .try_into()
        .ok()?;
    let width = u32::from_le_bytes(encoded[..4].try_into().ok()?);
    let height = u32::from_le_bytes(encoded[4..].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

pub struct Previewer {
    picker: Picker,
    config: Config,
    decoded: Cache<FileStamp, Arc<DynamicImage>>,
    rendered: Cache<PreviewKey, Preview>,
    scaled: Cache<WidthKey, Arc<DynamicImage>>,
    thumbnails: Option<ThumbnailCache>,
    stats: PreviewStats,
}

impl Previewer {
    pub fn new(picker: Picker, config: Config) -> Self {
        Self::with_thumbnail_cache(picker, config, None)
    }

    pub fn with_thumbnail_cache(
        picker: Picker,
        config: Config,
        cache_dir: Option<PathBuf>,
    ) -> Self {
        // Load the grammar tables on the worker before the first request. This
        // keeps the first text preview from paying a one-time 1–2s startup cost.
        let _ = HIGHLIGHTING.get_or_init(|| HighlightAssets {
            syntax: SyntaxSet::load_defaults_newlines(),
            themes: ThemeSet::load_defaults(),
        });
        let thumbnail_budget = config.thumbnail_cache_mb.saturating_mul(1024 * 1024);
        Self {
            picker,
            config,
            decoded: Cache::new(96 * 1024 * 1024, 8),
            rendered: Cache::new(32 * 1024 * 1024, 32),
            scaled: Cache::new(32 * 1024 * 1024, 8),
            thumbnails: cache_dir
                .filter(|_| thumbnail_budget > 0)
                .map(|directory| ThumbnailCache::new(directory, thumbnail_budget)),
            stats: PreviewStats::default(),
        }
    }

    pub fn load(&mut self, req: &PreviewRequest) -> Preview {
        self.try_load(req, None)
            .unwrap_or_else(|e| Preview::Error(format!("{e:#}")))
    }

    pub fn load_cancellable(
        &mut self,
        req: &PreviewRequest,
        token: &CancellationToken,
    ) -> Option<Preview> {
        let result = self.try_load(req, Some(token));
        if token.is_cancelled() {
            None
        } else {
            Some(result.unwrap_or_else(|e| Preview::Error(format!("{e:#}"))))
        }
    }
    pub fn stats(&self) -> PreviewStats {
        self.stats
    }
    pub fn cache_bytes(&self) -> usize {
        self.decoded.bytes() + self.rendered.bytes() + self.scaled.bytes()
    }

    fn try_load(
        &mut self,
        req: &PreviewRequest,
        token: Option<&CancellationToken>,
    ) -> Result<Preview> {
        check(token)?;
        let entry = &req.entry;
        if req.force {
            self.decoded.retain(|key| key.path != entry.path);
            self.rendered.retain(|key| key.file.path != entry.path);
            self.scaled.retain(|key| key.file.path != entry.path);
        }
        if entry.is_dir {
            return Ok(Preview::Directory(files::read_preview_dir(
                &entry.path,
                req.hidden,
                token,
            )?));
        }
        let meta = fs::metadata(&entry.path)?;
        if !meta.is_file() {
            return Ok(Preview::Metadata {
                entry: entry.clone(),
                note: "Special file · content is not read".into(),
            });
        }
        let stamp = FileStamp {
            path: entry.path.clone(),
            modified: meta.modified().ok(),
            size: meta.len(),
        };
        let text_key = PreviewKey {
            file: stamp.clone(),
            viewport: None,
        };
        let image_key = PreviewKey {
            file: stamp.clone(),
            viewport: Some((
                req.size.width,
                req.size.height,
                req.zoom,
                if req.zoom { req.scroll } else { 0 },
            )),
        };
        if let Some(preview) = self
            .rendered
            .get(&image_key)
            .or_else(|| self.rendered.get(&text_key))
        {
            self.stats.cache_hits += 1;
            return Ok(preview);
        }
        if archive::format(&entry.path).is_some() {
            let listing = archive::listing_with_check(&entry.path, || check(token))?;
            let weight = listing
                .entries
                .iter()
                .map(|entry| entry.path.len() + std::mem::size_of_val(entry))
                .sum::<usize>();
            let preview = Preview::Archive(listing);
            self.rendered.insert(text_key, preview.clone(), weight);
            return Ok(preview);
        }
        let mut head = [0u8; 32];
        let count = File::open(&entry.path)?.read(&mut head)?;
        if (entry.mime.starts_with("image/") && entry.mime != "image/svg+xml")
            || image::guess_format(&head[..count]).is_ok()
        {
            let preview = self.load_image(req, stamp, token)?;
            // Conservative estimate covers native encoded strings and protocol bookkeeping.
            let font = self.picker.font_size();
            let pixels = usize::from(req.size.width)
                * usize::from(req.size.height)
                * usize::from(font.width)
                * usize::from(font.height);
            let weight = if self.picker.protocol_type() == ProtocolType::Halfblocks {
                usize::from(req.size.width) * usize::from(req.size.height) * 48 + 4096
            } else {
                pixels.saturating_mul(16).saturating_add(4096)
            };
            self.rendered.insert(image_key, preview.clone(), weight);
            return Ok(preview);
        }
        let mut bytes = Vec::new();
        File::open(&entry.path)?
            .take(self.config.preview_max_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        let mut truncated = bytes.len() > self.config.preview_max_bytes;
        bytes.truncate(self.config.preview_max_bytes);
        let controls = bytes
            .iter()
            .filter(|&&b| b < 32 && ![9, 10, 13].contains(&b))
            .count();
        let invalid_utf8 = std::str::from_utf8(&bytes)
            .err()
            .is_some_and(|e| e.error_len().is_some() || !truncated);
        if bytes.contains(&0) || controls > bytes.len() / 100 || invalid_utf8 {
            return Ok(Preview::Metadata {
                entry: entry.clone(),
                note: "Binary or non-UTF-8 file · open externally to view".into(),
            });
        }
        let text = String::from_utf8_lossy(&bytes);
        let extension = entry
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        check(token)?;
        self.stats.text_builds += 1;
        let assets = HIGHLIGHTING.get_or_init(|| HighlightAssets {
            syntax: SyntaxSet::load_defaults_newlines(),
            themes: ThemeSet::load_defaults(),
        });
        let syntax = assets
            .syntax
            .find_syntax_by_extension(extension)
            .or_else(|| {
                text.lines()
                    .next()
                    .and_then(|line| assets.syntax.find_syntax_by_first_line(line))
            })
            .unwrap_or_else(|| assets.syntax.find_syntax_plain_text());
        let mut highlighter =
            HighlightLines::new(syntax, &assets.themes.themes["base16-ocean.dark"]);
        let mut lines = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            if index % 8 == 0 {
                check(token)?;
            }
            if index >= self.config.preview_max_lines {
                truncated = true;
                break;
            }
            // Limit work per line for pathological minified files and regex grammars.
            let mut line: String = raw.chars().take(800).collect();
            if raw.chars().nth(800).is_some() {
                line.push('…');
                truncated = true;
            }
            line = files::safe_text(&line.replace('\t', "    "));
            line.push('\n');
            let spans = match highlighter.highlight_line(&line, &assets.syntax) {
                Ok(parts) => parts
                    .into_iter()
                    .map(|(style, part)| {
                        Span::styled(
                            part.trim_end_matches('\n').to_owned(),
                            Style::default().fg(Color::Rgb(
                                style.foreground.r,
                                style.foreground.g,
                                style.foreground.b,
                            )),
                        )
                    })
                    .collect(),
                Err(_) => vec![Span::raw(line.trim_end().to_owned())],
            };
            lines.push(Line::from(spans));
        }
        if lines.is_empty() {
            lines.push(Line::from("(empty file)"));
        }
        check(token)?;
        let weight = lines
            .iter()
            .map(|line| {
                std::mem::size_of::<Line<'static>>()
                    + line
                        .spans
                        .iter()
                        .map(|span| std::mem::size_of::<Span<'static>>() + span.content.len())
                        .sum::<usize>()
            })
            .sum();
        let preview = Preview::Text {
            lines: Arc::new(lines),
            truncated,
            language: syntax.name.clone(),
        };
        self.rendered.insert(text_key, preview.clone(), weight);
        Ok(preview)
    }

    fn load_image(
        &mut self,
        req: &PreviewRequest,
        stamp: FileStamp,
        token: Option<&CancellationToken>,
    ) -> Result<Preview> {
        check(token)?;
        let font = self.picker.font_size();
        let halfblocks = self.picker.protocol_type() == ProtocolType::Halfblocks;
        let (cell_w, cell_h) = if halfblocks {
            (1, 2)
        } else {
            (u32::from(font.width), u32::from(font.height))
        };
        let pixels_w = u32::from(req.size.width.max(1)) * cell_w;
        let pixels_h = u32::from(req.size.height.max(1)) * cell_h;
        if req.force {
            if let Some(cache) = &self.thumbnails {
                cache.remove(&stamp, pixels_w, pixels_h);
            }
        }
        if !req.zoom {
            if let Some(cached) = self
                .thumbnails
                .as_ref()
                .and_then(|cache| cache.load(&stamp, pixels_w, pixels_h))
            {
                check(token)?;
                self.stats.disk_cache_hits += 1;
                let (width, height) = if let Some(dimensions) = cached.original_dimensions {
                    dimensions
                } else {
                    let dimensions = oriented_dimensions(&req.entry.path)?;
                    if let Some(cache) = &self.thumbnails {
                        cache.store_dimensions(&stamp, pixels_w, pixels_h, dimensions);
                    }
                    dimensions
                };
                let protocol = self.encode_image(cached.image, req, halfblocks)?;
                self.stats.image_encodes += 1;
                return Ok(Preview::Image {
                    protocol: Arc::new(protocol),
                    width,
                    height,
                    max_scroll: 0,
                    offset: 0,
                    zoom: false,
                });
            }
        }
        let image = if let Some(image) = self.decoded.get(&stamp) {
            image
        } else {
            let mut reader = ImageReader::open(&req.entry.path)?.with_guessed_format()?;
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(16384);
            limits.max_image_height = Some(16384);
            limits.max_alloc = Some(128 * 1024 * 1024);
            reader.limits(limits);
            let mut decoder = reader.into_decoder()?;
            anyhow::ensure!(
                decoder.total_bytes() <= 128 * 1024 * 1024,
                "Image exceeds 128 MB decoded size limit"
            );
            let orientation = decoder
                .orientation()
                .unwrap_or(image::metadata::Orientation::NoTransforms);
            check(token)?;
            let mut image = DynamicImage::from_decoder(decoder)
                .context("Could not decode image (128 MB / 16384 px limit)")?;
            image.apply_orientation(orientation);
            self.stats.image_decodes += 1;
            let image = Arc::new(image);
            self.decoded
                .insert(stamp.clone(), image.clone(), image.as_bytes().len());
            image
        };
        check(token)?;
        let (width, height) = (image.width(), image.height());
        let mut max_scroll = 0;
        let mut offset = 0;
        let displayed = if req.zoom {
            let scale = pixels_w as f64 / width as f64;
            let full_rows = (height as f64 * scale / f64::from(cell_h)).ceil() as usize;
            max_scroll = full_rows.saturating_sub(req.size.height as usize);
            offset = req.scroll.min(max_scroll);
            let crop_h = ((pixels_h as f64 / scale).ceil() as u32).clamp(1, height);
            let crop_y = ((offset as f64 * f64::from(cell_h) / scale) as u32).min(height - crop_h);
            let scaled_height = (height as f64 * scale).ceil().max(1.0) as u32;
            // Cache the width-scaled strip so panning only crops a small bitmap.
            // Extremely tall strips use the bounded crop-first path instead.
            if u64::from(pixels_w) * u64::from(scaled_height) * 4 <= 16 * 1024 * 1024 {
                let key = WidthKey {
                    file: stamp.clone(),
                    width: pixels_w,
                };
                let strip = if let Some(strip) = self.scaled.get(&key) {
                    strip
                } else {
                    let strip = Arc::new(fit_image(&image, pixels_w, scaled_height));
                    check(token)?;
                    self.scaled
                        .insert(key, strip.clone(), strip.as_bytes().len());
                    strip
                };
                let visible_h = pixels_h.min(strip.height());
                let y = (offset as u32)
                    .saturating_mul(cell_h)
                    .min(strip.height() - visible_h);
                strip.crop_imm(0, y, strip.width(), visible_h)
            } else {
                fit_image(
                    &image.crop_imm(0, crop_y, width, crop_h),
                    pixels_w,
                    pixels_h,
                )
            }
        } else {
            fit_image(&image, pixels_w.min(width), pixels_h.min(height))
        };
        check(token)?;
        if !req.zoom {
            if let Some(cache) = &self.thumbnails {
                cache.store(&stamp, pixels_w, pixels_h, &displayed, (width, height));
            }
        }
        let protocol = self.encode_image(displayed, req, halfblocks)?;
        self.stats.image_encodes += 1;
        check(token)?;
        Ok(Preview::Image {
            protocol: Arc::new(protocol),
            width,
            height,
            max_scroll,
            offset,
            zoom: req.zoom,
        })
    }

    fn encode_image(
        &self,
        displayed: DynamicImage,
        req: &PreviewRequest,
        halfblocks: bool,
    ) -> Result<Protocol> {
        if halfblocks {
            let cells = Size::new(
                displayed.width() as u16,
                displayed.height().div_ceil(2) as u16,
            );
            let bg = match crate::config::color(&self.config.theme.background) {
                Color::Rgb(r, g, b) => image::Rgba([r, g, b, 255]),
                _ => image::Rgba([13, 21, 38, 255]),
            };
            let mut opaque =
                image::RgbaImage::from_pixel(displayed.width(), displayed.height(), bg);
            image::imageops::overlay(&mut opaque, &displayed.to_rgba8(), 0, 0);
            Ok(Protocol::Halfblocks(
                ratatui_image::protocol::halfblocks::Halfblocks::new(
                    DynamicImage::ImageRgba8(opaque),
                    cells,
                )?,
            ))
        } else {
            Ok(self.picker.new_protocol(
                displayed,
                req.size,
                Resize::Fit(Some(FilterType::Triangle)),
            )?)
        }
    }
}

fn trim_thumbnail_cache(directory: &Path, budget: u64) {
    let Ok(read) = fs::read_dir(directory) else {
        return;
    };
    let mut files = read
        .filter_map(Result::ok)
        .filter_map(|entry| {
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "qoi")
            {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| {
                let dimensions_size = fs::metadata(thumbnail_dimensions_path(&entry.path()))
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
                (
                    entry.path(),
                    metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    metadata.len().saturating_add(dimensions_size),
                )
            })
        })
        .collect::<Vec<_>>();
    let mut bytes: u64 = files.iter().map(|(_, _, size)| size).sum();
    if bytes <= budget {
        return;
    }
    files.sort_by_key(|(_, modified, _)| *modified);
    for (path, _, size) in files {
        if bytes <= budget {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            let _ = fs::remove_file(thumbnail_dimensions_path(&path));
            bytes = bytes.saturating_sub(size);
        }
    }
}

fn oriented_dimensions(path: &Path) -> Result<(u32, u32)> {
    let reader = ImageReader::open(path)?.with_guessed_format()?;
    let mut decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    Ok(match orientation {
        image::metadata::Orientation::Rotate90
        | image::metadata::Orientation::Rotate270
        | image::metadata::Orientation::Rotate90FlipH
        | image::metadata::Orientation::Rotate270FlipH => (height, width),
        _ => (width, height),
    })
}

fn check(token: Option<&CancellationToken>) -> Result<()> {
    if let Some(token) = token {
        token.check()?;
    }
    Ok(())
}

fn fit_image(image: &DynamicImage, width: u32, height: u32) -> DynamicImage {
    if image.width() > width.saturating_mul(2) || image.height() > height.saturating_mul(2) {
        image.thumbnail(width.max(1), height.max(1))
    } else {
        image.resize(width.max(1), height.max(1), FilterType::Triangle)
    }
}

pub fn spawn_previewer(
    picker: Picker,
    config: Config,
) -> (LatestSender<PreviewRequest>, Receiver<PreviewResult>) {
    let (tx, rx) = latest_channel::<PreviewRequest>();
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut previewer =
            Previewer::with_thumbnail_cache(picker, config, crate::config::thumbnail_cache_dir());
        while let Some((req, token)) = rx.recv_cancellable() {
            let Some(preview) = previewer.load_cancellable(&req, &token) else {
                continue;
            };
            if result_tx
                .send(PreviewResult {
                    generation: req.generation,
                    preview,
                })
                .is_err()
            {
                break;
            }
            for entry in &req.preload {
                if token.is_cancelled() {
                    break;
                }
                let warm = PreviewRequest {
                    generation: req.generation,
                    entry: entry.clone(),
                    size: req.size,
                    scroll: 0,
                    zoom: false,
                    hidden: req.hidden,
                    force: false,
                    preload: Vec::new(),
                };
                let _ = previewer.load_cancellable(&warm, &token);
            }
        }
    });
    (tx, result_rx)
}
