use crate::{files::safe_text, worker::CancellationToken};
use lofty::{
    config::ParseOptions,
    file::{AudioFile, TaggedFileExt},
    picture::PictureType,
    prelude::Accessor,
    probe::Probe,
};
use serde_json::Value;
use std::{
    fs::{self, File},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAX_ARTWORK_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    Video,
}

impl MediaKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Audio => "AUDIO",
            Self::Video => "VIDEO",
        }
    }
}

pub struct MediaData {
    pub kind: MediaKind,
    pub details: Vec<(String, String)>,
    pub artwork: Option<Vec<u8>>,
}

pub fn kind(path: &Path, mime: &str) -> Option<MediaKind> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // MIME databases also use .ts for MPEG transport streams. Preserve code previews.
    if extension == "ts" {
        return None;
    }
    if mime.starts_with("audio/") {
        return Some(MediaKind::Audio);
    }
    if mime.starts_with("video/") {
        return Some(MediaKind::Video);
    }
    if matches!(
        extension.as_str(),
        "aac"
            | "aif"
            | "aiff"
            | "ape"
            | "flac"
            | "m4a"
            | "mp3"
            | "mpc"
            | "oga"
            | "ogg"
            | "opus"
            | "wav"
            | "wma"
    ) {
        Some(MediaKind::Audio)
    } else if matches!(
        extension.as_str(),
        "avi"
            | "flv"
            | "m2ts"
            | "m4v"
            | "mkv"
            | "mov"
            | "mp4"
            | "mpeg"
            | "mpg"
            | "mts"
            | "webm"
            | "wmv"
    ) {
        Some(MediaKind::Video)
    } else {
        None
    }
}

pub fn load(
    path: &Path,
    kind: MediaKind,
    include_artwork: bool,
    token: Option<&CancellationToken>,
) -> MediaData {
    match kind {
        MediaKind::Audio => audio(path, include_artwork),
        MediaKind::Video => video(path, include_artwork, token),
    }
}

fn audio(path: &Path, include_artwork: bool) -> MediaData {
    let mut data = MediaData {
        kind: MediaKind::Audio,
        details: vec![("Format".into(), extension_label(path))],
        artwork: None,
    };
    let options = ParseOptions::new().read_cover_art(include_artwork);
    let Some(probe) = Probe::open(path)
        .ok()
        .and_then(|probe| probe.guess_file_type().ok())
    else {
        finish_details(&mut data.details);
        return data;
    };
    let Ok(tagged) = probe.options(options).read() else {
        finish_details(&mut data.details);
        return data;
    };
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    if let Some(tag) = tag {
        push_text(&mut data.details, "Title", tag.title().as_deref());
        push_text(&mut data.details, "Artist", tag.artist().as_deref());
        push_text(&mut data.details, "Album", tag.album().as_deref());
        if include_artwork {
            data.artwork = tag
                .pictures()
                .iter()
                .find(|picture| picture.pic_type() == PictureType::CoverFront)
                .or_else(|| tag.pictures().first())
                .map(|picture| picture.data())
                .filter(|bytes| bytes.len() <= MAX_ARTWORK_BYTES)
                .map(ToOwned::to_owned);
        }
    }
    let properties = tagged.properties();
    if !properties.duration().is_zero() {
        data.details.push((
            "Duration".into(),
            format_duration(properties.duration().as_secs_f64()),
        ));
    }
    let mut quality = Vec::new();
    if let Some(rate) = properties.sample_rate().filter(|rate| *rate > 0) {
        quality.push(format!("{:.1} kHz", f64::from(rate) / 1000.0));
    }
    if let Some(bitrate) = properties.audio_bitrate().filter(|rate| *rate > 0) {
        quality.push(format!("{bitrate} kbps"));
    }
    if let Some(channels) = properties.channels().filter(|channels| *channels > 0) {
        quality.push(match channels {
            1 => "mono".into(),
            2 => "stereo".into(),
            count => format!("{count} channels"),
        });
    }
    if !quality.is_empty() {
        data.details.push(("Audio".into(), quality.join(" · ")));
    }
    finish_details(&mut data.details);
    data
}

fn video(path: &Path, include_artwork: bool, token: Option<&CancellationToken>) -> MediaData {
    let probe = ffprobe(path, token);
    let mut details = video_details(path, probe.as_ref());
    let artwork = include_artwork.then(|| video_frame(path, token)).flatten();
    if include_artwork && artwork.is_none() {
        details.push(("Preview".into(), "Frame unavailable · Enter to play".into()));
    }
    finish_details(&mut details);
    MediaData {
        kind: MediaKind::Video,
        details,
        artwork,
    }
}

fn video_details(path: &Path, probe: Option<&Value>) -> Vec<(String, String)> {
    let mut details = vec![("Format".into(), extension_label(path))];
    let Some(probe) = probe else {
        return details;
    };
    let tags = probe.pointer("/format/tags");
    for (label, name) in [("Title", "title"), ("Artist", "artist"), ("Album", "album")] {
        if let Some(value) = tags.and_then(|tags| tag_value(tags, name)) {
            details.push((label.into(), safe_text(value)));
        }
    }
    if let Some(seconds) = probe
        .pointer("/format/duration")
        .and_then(Value::as_str)
        .and_then(|duration| duration.parse::<f64>().ok())
        .filter(|duration| duration.is_finite() && *duration > 0.0)
    {
        details.push(("Duration".into(), format_duration(seconds)));
    }
    if let Some(streams) = probe.get("streams").and_then(Value::as_array) {
        if let Some(video) = streams
            .iter()
            .find(|stream| stream.get("codec_type").and_then(Value::as_str) == Some("video"))
        {
            let width = video.get("width").and_then(Value::as_u64);
            let height = video.get("height").and_then(Value::as_u64);
            if let (Some(width), Some(height)) = (width, height) {
                details.push(("Resolution".into(), format!("{width} × {height}")));
            }
            if let Some(codec) = video.get("codec_name").and_then(Value::as_str) {
                details.push(("Video".into(), safe_text(codec)));
            }
        }
        if let Some(audio) = streams
            .iter()
            .find(|stream| stream.get("codec_type").and_then(Value::as_str) == Some("audio"))
        {
            if let Some(codec) = audio.get("codec_name").and_then(Value::as_str) {
                details.push(("Audio".into(), safe_text(codec)));
            }
        }
    }
    details
}

fn tag_value<'a>(tags: &'a Value, name: &str) -> Option<&'a str> {
    tags.get(name)
        .or_else(|| tags.get(name.to_ascii_uppercase()))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn ffprobe(path: &Path, token: Option<&CancellationToken>) -> Option<Value> {
    let temporary = tempfile::Builder::new()
        .prefix("zuru-probe-")
        .tempdir()
        .ok()?;
    let output_path = temporary.path().join("metadata.json");
    let output_file = File::create(&output_path).ok()?;
    let mut command = Command::new("ffprobe");
    hide_window(&mut command);
    command
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_entries",
            "format=duration:format_tags=title,artist,album:stream=codec_type,codec_name,width,height",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output_file))
        .stderr(Stdio::null());
    let probe = run_bounded(&mut command, token)
        .then(|| read_limited(&output_path, 64 * 1024))
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    drop(command);
    let _ = fs::remove_file(output_path);
    probe
}

fn video_frame(path: &Path, token: Option<&CancellationToken>) -> Option<Vec<u8>> {
    if token.is_some_and(CancellationToken::is_cancelled) {
        return None;
    }
    let temporary = tempfile::Builder::new()
        .prefix("zuru-video-")
        .tempdir()
        .ok()?;
    let output = temporary.path().join("frame.png");
    let mut command = Command::new("ffmpeg");
    hide_window(&mut command);
    command
        .args(["-v", "error", "-ss", "0.25", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-vf",
            "scale=1280:720:force_original_aspect_ratio=decrease",
            "-y",
        ])
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let bytes = run_bounded(&mut command, token)
        .then(|| read_limited(&output, MAX_ARTWORK_BYTES))
        .flatten();
    let _ = fs::remove_file(output);
    bytes
}

fn run_bounded(command: &mut Command, token: Option<&CancellationToken>) -> bool {
    if token.is_some_and(CancellationToken::is_cancelled) {
        return false;
    }
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let started = Instant::now();
    loop {
        if token.is_some_and(CancellationToken::is_cancelled)
            || started.elapsed() >= Duration::from_secs(5)
        {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn read_limited(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let metadata = fs::metadata(path).ok()?;
    (metadata.len() <= limit as u64)
        .then(|| fs::read(path).ok())
        .flatten()
}

fn push_text(details: &mut Vec<(String, String)>, label: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        details.push((label.into(), safe_text(value)));
    }
}

fn finish_details(details: &mut Vec<(String, String)>) {
    details.push(("Play".into(), "Enter or o".into()));
}

fn extension_label(path: &Path) -> String {
    path.extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| !extension.is_empty())
        .map(str::to_ascii_uppercase)
        .unwrap_or_else(|| "Media".into())
}

pub fn format_duration(seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(windows)]
fn hide_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_window(_: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_types_and_durations_are_readable() {
        assert_eq!(
            kind(Path::new("song.flac"), "application/octet-stream"),
            Some(MediaKind::Audio)
        );
        assert_eq!(
            kind(Path::new("clip.webm"), "application/octet-stream"),
            Some(MediaKind::Video)
        );
        assert_eq!(
            kind(
                Path::new("code.ts"),
                mime_guess::from_path("code.ts")
                    .first_or_octet_stream()
                    .as_ref()
            ),
            None
        );
        assert_eq!(format_duration(65.2), "1:05");
        assert_eq!(format_duration(3661.0), "1:01:01");
    }

    #[test]
    fn ffprobe_json_becomes_video_details() {
        let probe: Value = serde_json::from_str(
            r#"{"streams":[{"codec_name":"h264","codec_type":"video","width":1920,"height":1080},{"codec_name":"aac","codec_type":"audio"}],"format":{"duration":"65.2","tags":{"title":"Demo"}}}"#,
        )
        .unwrap();
        let details = video_details(Path::new("demo.mp4"), Some(&probe));
        assert!(details.contains(&("Title".into(), "Demo".into())));
        assert!(details.contains(&("Duration".into(), "1:05".into())));
        assert!(details.contains(&("Resolution".into(), "1920 × 1080".into())));
        assert!(details.contains(&("Video".into(), "h264".into())));
        assert!(details.contains(&("Audio".into(), "aac".into())));
    }
}
