use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use directories::{ProjectDirs, UserDirs};
use ratatui::style::Color;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub show_hidden: bool,
    pub image_protocol: String,
    pub preview_max_bytes: usize,
    pub preview_max_lines: usize,
    pub thumbnail_cache_mb: u64,
    pub theme: Theme,
    pub keys: BTreeMap<String, Vec<String>>,
    pub bookmarks: BTreeMap<String, PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub background: String,
    pub panel: String,
    pub foreground: String,
    pub muted: String,
    pub accent: String,
    pub selection: String,
    pub border: String,
    pub normal: String,
    pub select: String,
    pub command: String,
    pub danger: String,
    pub slate: String,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: "#0d1526".into(),
            panel: "#111d33".into(),
            foreground: "#d8e2f1".into(),
            muted: "#7689a6".into(),
            accent: "#7dcfff".into(),
            selection: "#2b5fb8".into(),
            border: "#293b57".into(),
            normal: "#9ece6a".into(),
            select: "#bb9af7".into(),
            command: "#e0af68".into(),
            danger: "#f7768e".into(),
            slate: "#253550".into(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            show_hidden: false,
            image_protocol: "auto".into(),
            preview_max_bytes: 512 * 1024,
            preview_max_lines: 4000,
            thumbnail_cache_mb: 256,
            theme: Theme::default(),
            keys: BTreeMap::new(),
            bookmarks: BTreeMap::new(),
        }
    }
}

pub fn color(s: &str) -> Color {
    let value = u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0xd8e2f1);
    Color::Rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config = match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("Invalid config: {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.into()),
        };
        Self::validate(&config)?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        let theme = toml::Value::try_from(&self.theme)?;
        for (name, value) in theme.as_table().context("Invalid theme")? {
            let s = value.as_str().context("Theme colors must be strings")?;
            if s.len() != 7 || !s.starts_with('#') || u32::from_str_radix(&s[1..], 16).is_err() {
                bail!("theme.{name} must be a #RRGGBB color");
            }
        }
        if !["auto", "halfblocks", "kitty", "iterm2", "sixel"]
            .contains(&self.image_protocol.as_str())
        {
            bail!("image_protocol must be auto, halfblocks, kitty, iterm2, or sixel");
        }
        if !(1024..=8 * 1024 * 1024).contains(&self.preview_max_bytes)
            || !(1..=20_000).contains(&self.preview_max_lines)
        {
            bail!("Preview limits: 1024–8388608 bytes and 1–20000 lines");
        }
        if self.thumbnail_cache_mb > 4096 {
            bail!("thumbnail_cache_mb must be between 0 and 4096");
        }
        let mut bindings = BTreeMap::new();
        for (action, keys) in self.bindings() {
            if !default_bindings().contains_key(&action) {
                bail!("Unknown key action: {action}");
            }
            for key in keys {
                if !valid_key(&key) {
                    bail!("Invalid key binding: {key}");
                }
                if let Some(other) = bindings.insert(key.clone(), action.clone()) {
                    bail!("Key {key} is assigned to both {other} and {action}; override both actions to remap it");
                }
            }
        }
        Ok(())
    }

    pub fn bindings(&self) -> BTreeMap<String, Vec<String>> {
        let mut keys = default_bindings();
        keys.extend(self.keys.clone());
        keys
    }

    pub fn bookmarks(&self, path: &Path) -> BTreeMap<String, PathBuf> {
        let mut result = BTreeMap::new();
        if let Some(dirs) = UserDirs::new() {
            result.insert("home".into(), dirs.home_dir().to_path_buf());
            if let Some(p) = dirs.download_dir() {
                result.insert("downloads".into(), p.to_path_buf());
            }
            if let Some(p) = dirs.document_dir() {
                result.insert("documents".into(), p.to_path_buf());
            }
        }
        result.extend(self.bookmarks.clone());
        if let Ok(text) = fs::read_to_string(bookmark_path(path)) {
            if let Ok(saved) = toml::from_str::<BTreeMap<String, PathBuf>>(&text) {
                result.extend(saved);
            }
        }
        result
    }
}

pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ZURU_CONFIG") {
        return path.into();
    }
    ProjectDirs::from("", "", "zuru")
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("zuru.toml"))
}

pub fn thumbnail_cache_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "zuru").map(|d| d.cache_dir().join("thumbnails"))
}

pub fn bookmark_path(config: &Path) -> PathBuf {
    config.with_file_name("bookmarks.toml")
}

pub fn save_bookmarks(config: &Path, bookmarks: &BTreeMap<String, PathBuf>) -> Result<()> {
    let path = bookmark_path(config);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, toml::to_string_pretty(bookmarks)?)?;
    Ok(())
}

pub fn expand_path(text: &str, cwd: &Path) -> PathBuf {
    let text = text.trim();
    let path = if text == "~" || text.starts_with("~/") || text.starts_with("~\\") {
        UserDirs::new()
            .map(|u| u.home_dir().join(text.get(2..).unwrap_or("")))
            .unwrap_or_else(|| PathBuf::from(text))
    } else {
        PathBuf::from(text)
    };
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

pub fn default_bindings() -> BTreeMap<String, Vec<String>> {
    [
        ("quit", &["q", "ctrl-c"][..]),
        ("up", &["k", "up"]),
        ("down", &["j", "down"]),
        ("parent", &["h", "left", "backspace"]),
        ("enter", &["l", "right", "enter"]),
        ("first", &["g g", "home"]),
        ("last", &["G", "end"]),
        ("page_up", &["pageup", "ctrl-u"]),
        ("page_down", &["pagedown", "ctrl-d"]),
        ("open", &["o"]),
        ("edit", &["e"]),
        ("copy", &["y"]),
        ("cut", &["x"]),
        ("paste", &["p"]),
        ("trash", &["d", "delete"]),
        ("rename", &["r"]),
        ("bulk_rename", &["B"]),
        ("create_archive", &["C"]),
        ("extract", &["X"]),
        ("undo", &["u"]),
        ("sort", &[","]),
        ("visual", &["v"]),
        ("toggle", &["space"]),
        ("select_all", &["a"]),
        ("filter", &["/"]),
        ("search_name", &["s"]),
        ("search_contents", &["S"]),
        ("command", &[":"]),
        ("hidden", &["."]),
        ("refresh", &["R", "ctrl-r"]),
        ("preview_down", &["J", "ctrl-j"]),
        ("preview_up", &["K", "ctrl-k"]),
        ("zoom", &["z"]),
        ("bookmarks", &["b"]),
        ("bookmark", &["m"]),
        ("tasks", &["w"]),
        ("home_dir", &["g h"]),
        ("downloads", &["g d"]),
        ("new_tab", &["t"]),
        ("next_tab", &["tab"]),
        ("prev_tab", &["backtab"]),
        ("close_tab", &["ctrl-w"]),
        ("help", &["?", "f1"]),
        ("escape", &["esc"]),
    ]
    .into_iter()
    .map(|(action, keys)| (action.into(), keys.iter().map(|s| (*s).into()).collect()))
    .collect()
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.split(' ').all(|token| {
            let bare = token
                .strip_prefix("ctrl-")
                .or_else(|| token.strip_prefix("alt-"))
                .unwrap_or(token);
            bare.chars().count() == 1
                || [
                    "up",
                    "down",
                    "left",
                    "right",
                    "enter",
                    "backspace",
                    "home",
                    "end",
                    "pageup",
                    "pagedown",
                    "delete",
                    "space",
                    "tab",
                    "backtab",
                    "esc",
                    "f1",
                ]
                .contains(&bare)
        })
}

pub fn key_name(key: KeyEvent) -> String {
    let name = match key.code {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "backtab".into(),
        KeyCode::F(1) => "f1".into(),
        _ => return String::new(),
    };
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        format!("ctrl-{name}")
    } else if key.modifiers.contains(KeyModifiers::ALT) {
        format!("alt-{name}")
    } else {
        name
    }
}
