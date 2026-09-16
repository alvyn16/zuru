use crate::{
    config::{color, Config},
    files::safe_text,
};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const PAGES: [&str; 5] = ["Start", "Files", "Preview", "Places", "Commands"];

pub fn keys(config: &Config, action: &str) -> String {
    let bindings = config.bindings();
    let Some(keys) = bindings.get(action).filter(|keys| !keys.is_empty()) else {
        return "Unassigned".into();
    };
    keys.iter()
        .map(|key| {
            key.split(' ')
                .map(|token| {
                    if let Some(s) = token.strip_prefix("ctrl-") {
                        return format!("Ctrl+{s}");
                    }
                    if let Some(s) = token.strip_prefix("alt-") {
                        return format!("Alt+{s}");
                    }
                    match token {
                        "up" => "↑".into(),
                        "down" => "↓".into(),
                        "left" => "←".into(),
                        "right" => "→".into(),
                        "space" => "Space".into(),
                        "enter" => "Enter".into(),
                        "esc" => "Esc".into(),
                        "backspace" => "Backspace".into(),
                        "pageup" => "Page Up".into(),
                        "pagedown" => "Page Down".into(),
                        "home" => "Home".into(),
                        "end" => "End".into(),
                        "tab" => "Tab".into(),
                        "backtab" => "Shift+Tab".into(),
                        "delete" => "Delete".into(),
                        "f1" => "F1".into(),
                        s if s.len() == 1 && s.chars().all(char::is_uppercase) => {
                            format!("Shift+{}", s.to_lowercase())
                        }
                        s => safe_text(s),
                    }
                })
                .collect::<Vec<_>>()
                .join(" then ")
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Wrap before scrolling, so narrow terminals can still reach every instruction.
pub fn lines(config: &Config, page: usize, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut guide = Guide {
        config,
        width,
        lines: Vec::new(),
    };
    match page {
        0 => {
            guide.title("GETTING AROUND");
            guide.note("Middle: your files. Left: the parent folder. Right: a preview of the highlighted item.");
            guide.space();
            for (action, text) in [
                ("down", "Move down one item"),
                ("up", "Move up one item"),
                ("enter", "Enter a folder or open a file"),
                ("parent", "Go back to the parent folder"),
                ("first", "Jump to the first item"),
                ("last", "Jump to the last item"),
                ("page_down", "Move down one page"),
                ("page_up", "Move up one page"),
                ("hidden", "Show or hide hidden files"),
                (
                    "sort",
                    "Choose name, extension, size, or modified-time order",
                ),
                ("refresh", "Refresh the folder"),
                ("quit", "Quit Zuru"),
            ] {
                guide.action(action, text);
            }
            guide.space();
            guide.note(
                "'then' means press keys in order. Shift+j means hold Shift while pressing j.",
            );
        }
        1 => {
            guide.title("COPY, MOVE & RENAME");
            guide.note("Actions use your selected items. With no selection, they use the highlighted item.");
            guide.space();
            for (action, text) in [
                ("copy", "Copy items; nothing moves yet"),
                ("cut", "Mark items to move"),
                ("paste", "Paste into the folder you are viewing"),
                ("rename", "Rename the highlighted item"),
                (
                    "bulk_rename",
                    "Edit selected filenames together, then review before applying",
                ),
                ("trash", "Send items to trash; asks you to confirm"),
                ("undo", "Review recent file changes and undo the newest one"),
                (
                    "create_archive",
                    "Create a ZIP, TAR, or TAR.GZ from the selected items",
                ),
                ("extract", "Extract selected archives into new folders"),
                ("tasks", "Show progress, recent results, and cancellation"),
            ] {
                guide.action(action, text);
            }
            guide.space();
            guide.title("SELECT MORE THAN ONE ITEM");
            for (action, text) in [
                ("toggle", "Select or deselect an item, then move down"),
                ("visual", "Start a range; move up/down to extend it"),
                ("select_all", "Select all visible items"),
                ("escape", "Clear your selection"),
            ] {
                guide.action(action, text);
            }
            guide.space();
            guide.note(&format!(
                "Copy example: press {} on an item, enter the destination folder, then press {}.",
                keys(config, "copy"),
                keys(config, "paste")
            ));
            guide.note("Zuru never replaces an existing item without asking. Trashed items can be restored from your OS trash.");
            guide.note("If a destination exists, choose Skip, Keep Both, or Replace. Replace sends the old destination to trash first.");
            guide.note("Undo covers rename, move, copy, create, extract, and trash when your operating system supports restoring trash items. Open Undo History and press Enter to undo the newest change.");
            guide.note("Bulk rename opens one filename per line in your editor. Keep the line count unchanged; after closing the editor, Zuru shows every change and blocks duplicates or existing names.");
        }
        2 => {
            guide.title("LOOK INSIDE A FILE");
            guide.note(
                "Highlight an image, music file, video, code file, text file, folder, or archive to preview it automatically.",
            );
            guide.space();
            for (action, text) in [
                ("preview_down", "Scroll the preview down"),
                ("preview_up", "Scroll the preview up"),
                ("zoom", "Images: toggle fit-to-pane / full-width view"),
                ("open", "Open in the system's default app"),
                ("edit", "Open in your configured text editor"),
            ] {
                guide.action(action, text);
            }
            guide.space();
            guide.title("MUSIC & VIDEO");
            guide.note("Music shows track details and embedded album art. Video shows a still frame and media details when ffmpeg and ffprobe are installed. Press Enter to play in your default media app. Use preview scrolling if the details do not fit.");
            guide.space();
            guide.title("ARCHIVES");
            guide.note("ZIP, TAR, TAR.GZ, and TGZ files show their contents without extraction. Archive creation and extraction run in the task worker, so you can keep navigating.");
            guide.space();
            guide.title("SCROLLING IMAGES");
            guide.note(&format!("Press {} to fill the pane's width. If the image is taller than the pane, use {} and {} to look further down or up.", keys(config, "zoom"), keys(config, "preview_down"), keys(config, "preview_up")));
            guide.space();
            guide.note("The file list and its preview scroll separately. The bottom percentage shows your position in the preview.");
        }
        3 => {
            guide.title("FIND FILES & SAVE PLACES");
            for (action, text) in [
                ("filter", "Type part of a filename to filter this folder"),
                (
                    "search_name",
                    "Search filenames in this folder and all subfolders",
                ),
                (
                    "search_contents",
                    "Search text inside files in all subfolders",
                ),
                ("escape", "Clear a filter or leave recursive search results"),
                ("bookmark", "Save this folder under a name"),
                ("bookmarks", "Choose a saved folder and press Enter"),
                ("home_dir", "Jump to your home folder"),
                ("downloads", "Jump to Downloads"),
            ] {
                guide.action(action, text);
            }
            guide.space();
            guide.title("WORK IN SEVERAL FOLDERS");
            for (action, text) in [
                ("new_tab", "Open another tab in this folder"),
                ("next_tab", "Switch to the next tab"),
                ("prev_tab", "Switch to the previous tab"),
                ("close_tab", "Close this tab"),
            ] {
                guide.action(action, text);
            }
            guide.row("1 ... 9".into(), "Switch directly to a numbered tab");
            guide.space();
            guide.note("Filtering: Enter keeps the results. Esc while typing restores the previous filter.");
        }
        _ => {
            guide.title("CREATE FILES & JUMP TO A PATH");
            guide.action(
                "command",
                "Open the command line, then type a command below",
            );
            guide.space();
            for (command, description) in [
                ("cd PATH", "Go to a folder, for example: cd ~/Downloads"),
                ("find NAME", "Search filenames recursively"),
                ("grep TEXT", "Search inside files recursively"),
                (
                    "mkdir NAME",
                    "Create a folder, for example: mkdir New folder",
                ),
                (
                    "touch NAME",
                    "Create an empty file, for example: touch notes.txt",
                ),
                (
                    "bookmark NAME",
                    "Save this folder, for example: bookmark work",
                ),
                (
                    "archive NAME.zip",
                    "Archive the selected or highlighted items",
                ),
                ("extract", "Extract selected archives into this folder"),
                ("undo", "Open the undo history"),
                ("reload", "Apply changes from your TOML configuration"),
                ("help", "Open this guide"),
                ("quit", "Exit Zuru"),
            ] {
                guide.row(command.into(), description);
            }
            guide.space();
            guide.title("WHILE TYPING OR RENAMING");
            guide.row("Enter".into(), "Accept the name or run the command");
            guide.row("Esc".into(), "Cancel and return to your files");
            guide.row("Backspace".into(), "Delete the last character");
            guide.row("Ctrl+u".into(), "Clear the whole input");
            guide.space();
            guide.title("SHELL INTEGRATION");
            guide.row(
                "--chooser-file PATH".into(),
                "Choose files with Enter and write their paths to a file",
            );
            guide.row(
                "--cwd-file PATH".into(),
                "Write Zuru's final folder so a shell wrapper can cd there",
            );
            guide.space();
            guide.note("Paths can contain spaces. ~ stands for your home folder. These commands do not run a shell.");
        }
    }
    guide.lines
}

struct Guide<'a> {
    config: &'a Config,
    width: usize,
    lines: Vec<Line<'static>>,
}
impl Guide<'_> {
    fn space(&mut self) {
        self.lines.push(Line::default());
    }
    fn title(&mut self, text: &str) {
        for text in wrap(text, self.width) {
            self.lines.push(Line::from(Span::styled(
                text,
                Style::default().fg(color(&self.config.theme.accent)).bold(),
            )));
        }
    }
    fn note(&mut self, text: &str) {
        for text in wrap(text, self.width) {
            self.lines.push(Line::from(Span::styled(
                text,
                Style::default().fg(color(&self.config.theme.muted)),
            )));
        }
    }
    fn action(&mut self, action: &str, description: &str) {
        self.row(keys(self.config, action), description);
    }
    fn row(&mut self, key: String, description: &str) {
        let key_style = Style::default().fg(color(&self.config.theme.accent)).bold();
        let column = (self.width / 3).min(24);
        if key.width() <= column && self.width >= 54 {
            for (index, text) in wrap(description, self.width.saturating_sub(column + 2))
                .into_iter()
                .enumerate()
            {
                let label = if index == 0 {
                    format!("{}{}  ", key, " ".repeat(column - key.width()))
                } else {
                    " ".repeat(column + 2)
                };
                self.lines.push(Line::from(vec![
                    Span::styled(label, key_style),
                    Span::raw(text),
                ]));
            }
        } else {
            for text in wrap(&key, self.width) {
                self.lines.push(Line::from(Span::styled(text, key_style)));
            }
            for text in wrap(description, self.width.saturating_sub(2).max(1)) {
                self.lines.push(Line::from(format!("  {text}")));
            }
        }
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.width() + 1 + word.width() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        for c in word.chars() {
            if line.width() + c.width().unwrap_or(0) > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            line.push(c);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}
