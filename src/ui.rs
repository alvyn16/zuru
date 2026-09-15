use crate::{
    app::{App, InputKind, Mode, RenameStatus},
    config::color,
    files::{human_size, safe_text, Entry},
    preview::Preview,
};
use chrono::{DateTime, Local};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph},
    Frame,
};
use ratatui_image::Image;
use unicode_width::UnicodeWidthStr;

fn style(fg: Color, bg: Color) -> Style {
    Style::default().fg(fg).bg(bg)
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let theme = app.config.theme.clone();
    let bg = color(&theme.background);
    let fg = color(&theme.foreground);
    frame.render_widget(Block::new().style(style(fg, bg)), area);
    if area.width < 48 || area.height < 8 {
        frame.render_widget(
            Paragraph::new("ZURU\nEnlarge the terminal to at least 48 × 8.\nq to quit")
                .style(style(fg, bg)),
            area,
        );
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);
    header(frame, app, rows[0]);
    let panes = Layout::horizontal([
        Constraint::Percentage(20),
        Constraint::Percentage(35),
        Constraint::Percentage(45),
    ])
    .split(rows[1]);
    app.list_height = panes[1].height.saturating_sub(2).max(1) as usize;
    let preview_area = Rect::new(
        panes[2].x + 1,
        panes[2].y + 2,
        panes[2].width.saturating_sub(2),
        panes[2].height.saturating_sub(3),
    );
    app.set_preview_size(Size::new(preview_area.width, preview_area.height));
    parent_pane(frame, app, panes[0]);
    current_pane(frame, app, panes[1]);
    preview_pane(frame, app, panes[2], preview_area);
    feedback(frame, app, rows[2]);
    status(frame, app, rows[3]);
    overlay(frame, app, area);
}

fn header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let t = &app.config.theme;
    let base = style(color(&t.accent), color(&t.panel));
    frame.render_widget(Block::new().style(base), area);
    let badges: Vec<Span<'static>> = app
        .tabs
        .iter()
        .enumerate()
        .flat_map(|(i, _)| {
            let active = i == app.active;
            vec![
                Span::styled(
                    format!(" {} ", i + 1),
                    style(
                        if active {
                            color(&t.background)
                        } else {
                            color(&t.muted)
                        },
                        if active {
                            color(&t.accent)
                        } else {
                            color(&t.slate)
                        },
                    )
                    .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ", base),
            ]
        })
        .collect();
    let badge_width = (app.tabs.len() * 4).min(area.width as usize / 3) as u16;
    let path_area = Rect::new(area.x, area.y, area.width.saturating_sub(badge_width), 1);
    let path = App::displayed_path(&app.tab().cwd);
    let available = path_area.width.saturating_sub(4) as usize;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  ", base),
            Span::styled(tail(&path, available), base.add_modifier(Modifier::BOLD)),
        ])),
        path_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(badges)).alignment(Alignment::Right),
        Rect::new(area.right() - badge_width, area.y, badge_width, 1),
    );
}

fn pane_heading(frame: &mut Frame<'_>, app: &App, area: Rect, label: String, border: bool) {
    let t = &app.config.theme;
    if border {
        frame.render_widget(
            Block::new()
                .borders(Borders::RIGHT)
                .border_style(Style::default().fg(color(&t.border))),
            area,
        );
    }
    let title = Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1);
    frame.render_widget(
        Paragraph::new(label).style(Style::default().fg(color(&t.muted))),
        title,
    );
}

fn parent_pane(frame: &mut Frame<'_>, app: &App, area: Rect) {
    pane_heading(frame, app, area, "PARENT".into(), true);
    let t = &app.config.theme;
    let tab = app.tab();
    let inner = Rect::new(
        area.x,
        area.y + 2,
        area.width.saturating_sub(1),
        area.height.saturating_sub(2),
    );
    if tab.parent.is_empty() {
        frame.render_widget(
            Paragraph::new(if tab.loading {
                "  Reading…"
            } else {
                "  filesystem root"
            })
            .style(Style::default().fg(color(&t.muted))),
            inner,
        );
        return;
    }
    let selected = tab.parent.iter().position(|e| e.path == tab.cwd);
    let offset = selected
        .unwrap_or(0)
        .saturating_sub(inner.height as usize / 2);
    let items: Vec<_> = tab
        .parent
        .iter()
        .skip(offset)
        .take(inner.height as usize)
        .map(|entry| {
            ListItem::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    format!("{} ", entry.icon()),
                    Style::default().fg(color(&t.accent)),
                ),
                Span::styled(
                    entry.name.clone(),
                    Style::default().fg(color(if entry.is_dir { &t.accent } else { &t.muted })),
                ),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(selected.map(|i| i - offset));
    frame.render_stateful_widget(
        List::new(items).highlight_style(style(color(&t.accent), color(&t.slate)).bold()),
        inner,
        &mut state,
    );
}

fn current_pane(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let tab = app.tab();
    let count = if tab.filter.is_empty() {
        format!("{}", tab.entries.len())
    } else {
        format!("{}/{}", tab.visible.len(), tab.entries.len())
    };
    let title = if let Some(search) = &tab.search {
        format!(
            "SEARCH {}  '{}'  {count}{}",
            search.kind.label(),
            safe_text(&search.query),
            if tab.loading { "  ·" } else { "" }
        )
    } else {
        format!(
            "CURRENT {count} · {} {}{}{}",
            tab.sort.label(),
            if tab.sort_reverse { "↓" } else { "↑" },
            if tab.directories_first {
                " · DIRS"
            } else {
                ""
            },
            if tab.loading { "  ·" } else { "" }
        )
    };
    pane_heading(frame, app, area, title, true);
    let t = app.config.theme.clone();
    let inner = Rect::new(
        area.x,
        area.y + 2,
        area.width.saturating_sub(1),
        area.height.saturating_sub(2),
    );
    if tab.visible.is_empty() {
        let text = if tab.loading {
            if tab.search.is_some() {
                "  Searching this folder and its subfolders…".into()
            } else {
                "  Reading directory…".into()
            }
        } else if let Some(e) = &tab.error {
            format!("  {e}\n\n  h to go up · R to retry")
        } else if tab.search.is_some() {
            "  No recursive matches\n\n  Esc returns to the folder".into()
        } else if !tab.filter.is_empty() {
            "  No matches\n  Esc clears the filter".into()
        } else {
            "  This folder is empty\n\n  :mkdir NAME\n  :touch NAME".into()
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(color(&t.muted)))
                .wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
        return;
    }
    // Build only the visible window; rendering a large directory stays O(screen height).
    let height = inner.height.max(1) as usize;
    let offset = tab
        .list_state
        .offset()
        .min(tab.selected)
        .max(tab.selected.saturating_sub(height - 1));
    let items: Vec<_> = tab
        .visible
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(index, &i)| {
            let entry = &tab.entries[i];
            let selected = index == tab.selected;
            let marked = tab.marked.contains(&entry.path);
            let text_color = if selected {
                Color::White
            } else if entry.is_dir {
                color(&t.accent)
            } else {
                color(&t.foreground)
            };
            let marker = if marked { "▎" } else { " " };
            let label = tab
                .search
                .as_ref()
                .and_then(|search| entry.path.strip_prefix(&search.root).ok())
                .map(|path| safe_text(&path.display().to_string().replace('\\', "/")))
                .unwrap_or_else(|| entry.name.clone());
            let filename = format!("{}{}", label, if entry.is_symlink { " ↗" } else { "" });
            ListItem::new(Line::from(vec![
                Span::styled(marker, Style::default().fg(color(&t.select))),
                Span::styled(
                    format!("{} ", entry.icon()),
                    Style::default().fg(if selected {
                        Color::White
                    } else {
                        color(&t.accent)
                    }),
                ),
                Span::styled(filename, Style::default().fg(text_color)),
            ]))
        })
        .collect();
    let selected = app.tab().selected;
    app.tab_mut().list_state.select(Some(selected));
    *app.tab_mut().list_state.offset_mut() = offset;
    let mut visible_state = ListState::default().with_selected(Some(selected - offset));
    frame.render_stateful_widget(
        List::new(items).highlight_style(style(Color::White, color(&t.selection)).bold()),
        inner,
        &mut visible_state,
    );
}

fn preview_pane(frame: &mut Frame<'_>, app: &App, area: Rect, inner: Rect) {
    pane_heading(frame, app, area, app.preview.title(), false);
    let t = &app.config.theme;
    let muted = Style::default().fg(color(&t.muted));
    match &app.preview {
        Preview::Empty => frame.render_widget(
            Paragraph::new("\nSelect a file to preview")
                .style(muted)
                .alignment(Alignment::Center),
            inner,
        ),
        Preview::Loading => {
            frame.render_widget(Paragraph::new("\nLoading preview…").style(muted), inner)
        }
        Preview::Error(error) => frame.render_widget(
            Paragraph::new(format!("Preview unavailable\n\n{}", safe_text(error)))
                .style(muted)
                .wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        ),
        Preview::Image { protocol, .. } => {
            // Graphics protocols can paint over popups; suppress them while a popup is open.
            if matches!(
                app.mode,
                Mode::Help { .. }
                    | Mode::Bookmarks { .. }
                    | Mode::ConfirmTrash(_)
                    | Mode::Tasks
                    | Mode::Conflict(_)
                    | Mode::Sort
                    | Mode::BulkRename(_)
                    | Mode::UndoHistory
            ) {
                return;
            }
            let size = protocol.size();
            let image_area = Rect::new(
                inner.x + inner.width.saturating_sub(size.width) / 2,
                inner.y,
                inner.width.min(size.width),
                inner.height.min(size.height),
            );
            frame.render_widget(Image::new(protocol), image_area);
        }
        Preview::Text { lines, .. } => {
            let shown: Vec<_> = lines
                .iter()
                .enumerate()
                .skip(app.preview_scroll)
                .take(inner.height as usize)
                .map(|(i, line)| {
                    let mut spans = vec![Span::styled(format!("{:>4}  ", i + 1), muted)];
                    spans.extend(line.spans.clone());
                    Line::from(spans)
                })
                .collect();
            frame.render_widget(Paragraph::new(shown), inner);
        }
        Preview::Directory(entries) => {
            if entries.is_empty() {
                frame.render_widget(Paragraph::new("Empty folder").style(muted), inner);
            }
            let lines: Vec<_> = entries
                .iter()
                .skip(app.preview_scroll)
                .take(inner.height as usize)
                .map(|entry| {
                    Line::from(vec![
                        Span::styled(
                            format!("{}  ", entry.icon()),
                            Style::default().fg(color(&t.accent)),
                        ),
                        Span::styled(
                            entry.name.clone(),
                            Style::default().fg(color(if entry.is_dir {
                                &t.accent
                            } else {
                                &t.foreground
                            })),
                        ),
                    ])
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), inner);
        }
        Preview::Archive(listing) => {
            let header = format!(
                "{} entries · {} unpacked{}",
                listing.total_entries,
                human_size(listing.total_bytes),
                if listing.truncated {
                    " · preview limited"
                } else {
                    ""
                }
            );
            let mut lines = vec![Line::from(Span::styled(
                header,
                Style::default().fg(color(&t.muted)),
            ))];
            lines.extend(
                listing
                    .entries
                    .iter()
                    .skip(app.preview_scroll)
                    .take(inner.height.saturating_sub(1) as usize)
                    .map(|entry| {
                        Line::from(vec![
                            Span::styled(
                                if entry.is_dir { "  " } else { "  " },
                                Style::default().fg(color(&t.accent)),
                            ),
                            Span::styled(
                                entry.path.clone(),
                                Style::default().fg(color(&t.foreground)),
                            ),
                            Span::styled(
                                if entry.is_dir {
                                    String::new()
                                } else {
                                    format!("  {}", human_size(entry.size))
                                },
                                Style::default().fg(color(&t.muted)),
                            ),
                        ])
                    }),
            );
            frame.render_widget(Paragraph::new(lines), inner);
        }
        Preview::Metadata { entry, note } => metadata(frame, app, entry, note, inner),
    }
    let max = app.preview.max_scroll(inner.height as usize);
    if max > 0 && area.height > 3 {
        let bar_height = inner.height;
        let thumb =
            ((app.preview_scroll as f64 / max as f64) * bar_height.saturating_sub(1) as f64) as u16;
        for row in 0..bar_height {
            frame.render_widget(
                Paragraph::new(if row == thumb { "┃" } else { "│" }).style(
                    Style::default().fg(color(if row == thumb { &t.accent } else { &t.border })),
                ),
                Rect::new(area.right() - 1, inner.y + row, 1, 1),
            );
        }
    }
}

fn metadata(frame: &mut Frame<'_>, app: &App, entry: &Entry, note: &str, area: Rect) {
    let t = &app.config.theme;
    let modified = entry
        .modified
        .map(|time| {
            DateTime::<Local>::from(time)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "Unavailable".into());
    let lines = vec![
        Line::default(),
        Line::from(Span::styled(
            format!("{}  {}", entry.icon(), entry.name),
            Style::default().fg(color(&t.accent)).bold(),
        )),
        Line::default(),
        Line::from(format!("Size       {}", human_size(entry.size))),
        Line::from(format!("Type       {}", entry.mime)),
        Line::from(format!("Modified   {modified}")),
        Line::from(format!("Access     {}", entry.permissions)),
        Line::default(),
        Line::from(Span::styled(
            note.to_owned(),
            Style::default().fg(color(&t.muted)),
        )),
        Line::default(),
        Line::from(Span::styled(
            "o open externally  ·  e edit",
            Style::default().fg(color(&t.muted)),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
        area,
    );
}

fn feedback(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let t = &app.config.theme;
    let base = style(color(&t.muted), color(&t.background));
    let line = match &app.mode {
        Mode::Input(kind) => {
            let prefix = match kind {
                InputKind::Filter => " / ",
                InputKind::SearchName => " find name › ",
                InputKind::SearchContents => " find text › ",
                InputKind::Command => " : ",
                InputKind::Rename(_) => " rename › ",
                InputKind::Bookmark => " bookmark › ",
                InputKind::Archive(_) => " create archive › ",
            };
            let input = tail(
                &safe_text(&app.input),
                area.width.saturating_sub(prefix.width() as u16 + 1) as usize,
            );
            let cursor =
                (prefix.width() + input.width()).min(area.width.saturating_sub(1) as usize) as u16;
            frame.set_cursor_position((area.x + cursor, area.y));
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(color(&t.command))),
                Span::styled(input, Style::default().fg(color(&t.foreground))),
            ])
        }
        _ if app.busy => {
            let progress = app.task.as_ref().map(|task| task.percent()).unwrap_or(0);
            Line::from(Span::styled(
                format!(" ◌ Task {progress}% · w details/cancel · navigation remains available"),
                Style::default().fg(color(&t.command)),
            ))
        }
        _ if app.message.is_some() => {
            let m = app.message.as_ref().unwrap();
            Line::from(Span::styled(
                format!(" {} {}", if m.error { "!" } else { "✓" }, m.text),
                Style::default().fg(color(if m.error { &t.danger } else { &t.normal })),
            ))
        }
        _ if !app.pending_keys.is_empty() => Line::from(format!(" {} …", app.pending_keys)),
        _ if !app.tab().filter.is_empty() => Line::from(format!(
            " / {}  ·  {} matches  ·  Esc clear",
            app.tab().filter,
            app.tab().visible.len()
        )),
        _ if app.tab().search.is_some() => {
            let search = app.tab().search.as_ref().expect("search checked above");
            Line::from(format!(
                " {} results · {} paths checked{} · Esc returns to folder",
                app.tab().visible.len(),
                search.scanned,
                if search.truncated {
                    " · result limit reached"
                } else {
                    ""
                }
            ))
        }
        _ if app.chooser => Line::from(Span::styled(
            format!(
                " CHOOSER · {} selected · Space toggles · Enter returns files · q cancels",
                app.tab().marked.len()
            ),
            Style::default().fg(color(&t.accent)),
        )),
        _ if !app.tab().marked.is_empty() => Line::from(Span::styled(
            format!(
                " {} selected  ·  B bulk rename  y copy  x cut  d trash  Esc clear",
                app.tab().marked.len()
            ),
            Style::default().fg(color(&t.select)),
        )),
        _ => Line::from(
            " h j k l navigate   , sort   / search   u undo   X extract   C archive   ? help",
        ),
    };
    frame.render_widget(Paragraph::new(line).style(base), area);
}

fn status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let t = &app.config.theme;
    let background = color(&t.background);
    let fg = color(&t.foreground);
    let selected = app.tab().current();
    let mode = if app.chooser && matches!(app.mode, Mode::Normal) {
        "CHOOSE"
    } else if !app.tab().marked.is_empty() && matches!(app.mode, Mode::Normal) {
        "SELECT"
    } else if app.tab().search.is_some() && matches!(app.mode, Mode::Normal) {
        "SEARCH"
    } else {
        app.mode.label()
    };
    let mode_color = color(match mode {
        "SELECT" => &t.select,
        "RENAME" | "BULK" | "SORT" | "UNDO" | "COMMAND" | "SEARCH" | "TASKS" => &t.command,
        "CHOOSE" => &t.accent,
        "TRASH" | "CONFLICT" => &t.danger,
        _ => &t.normal,
    });
    let size = selected
        .map(|e| {
            if e.is_dir {
                "DIR".into()
            } else {
                human_size(e.size)
            }
        })
        .unwrap_or_else(|| "—".into());
    let perms = selected
        .map(|e| e.permissions.as_str())
        .unwrap_or("----------");
    let max = app.preview.max_scroll(app.preview_size.height as usize);
    let percent = (app.preview_scroll * 100).checked_div(max).unwrap_or(100);
    let count = format!(
        "{}/{}",
        if app.tab().visible.is_empty() {
            0
        } else {
            app.tab().selected + 1
        },
        app.tab().visible.len()
    );
    let segments = [
        (format!(" {mode} "), background, mode_color),
        (format!(" {size} "), fg, color(&t.slate)),
        (format!(" {perms} "), color(&t.muted), color(&t.panel)),
        (format!(" {percent}% "), fg, color(&t.slate)),
        (format!(" {count} "), background, color(&t.accent)),
    ];
    let mut spans = Vec::new();
    let mut width = 0;
    for (i, (label, text_color, block_color)) in segments.iter().enumerate() {
        width += label.width() + 1;
        spans.push(Span::styled(
            label.clone(),
            style(*text_color, *block_color).bold(),
        ));
        let next_color = segments.get(i + 1).map(|s| s.2).unwrap_or(color(&t.panel));
        spans.push(Span::styled("", style(*block_color, next_color)));
    }
    let name = selected.map(|e| e.name.as_str()).unwrap_or("zuru");
    let available = (area.width as usize).saturating_sub(width);
    let trailing = tail(name, available.saturating_sub(2));
    let padding = available.saturating_sub(trailing.width() + 1);
    spans.push(Span::styled(
        format!("{}{trailing} ", " ".repeat(padding)),
        style(fg, color(&t.panel)),
    ));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(style(fg, color(&t.panel))),
        area,
    );
}

fn overlay(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    if matches!(app.mode, Mode::Help { .. }) {
        help_overlay(frame, app, area);
        return;
    }
    if matches!(app.mode, Mode::Tasks) {
        task_overlay(frame, app, area);
        return;
    }
    if matches!(app.mode, Mode::Conflict(_)) {
        conflict_overlay(frame, app, area);
        return;
    }
    if matches!(app.mode, Mode::Sort) {
        sort_overlay(frame, app, area);
        return;
    }
    if matches!(app.mode, Mode::BulkRename(_)) {
        bulk_rename_overlay(frame, app, area);
        return;
    }
    if matches!(app.mode, Mode::UndoHistory) {
        undo_overlay(frame, app, area);
        return;
    }
    let t = &app.config.theme;
    let (title, lines, desired_height, scroll) = match &app.mode {
        Mode::ConfirmTrash(paths) => {
            let mut lines = vec![
                Line::from(format!("Send {} item(s) to the OS trash?", paths.len())),
                Line::default(),
            ];
            lines.extend(paths.iter().take(5).map(|p| {
                Line::from(format!(
                    "  {}",
                    safe_text(&p.file_name().unwrap_or_default().to_string_lossy())
                ))
            }));
            if paths.len() > 5 {
                lines.push(Line::from(format!("  … and {} more", paths.len() - 5)));
            }
            lines.extend([
                Line::default(),
                Line::from(Span::styled(
                    "Enter / y / d  send to trash     n / Esc  cancel",
                    Style::default().fg(color(&t.danger)).bold(),
                )),
            ]);
            (" MOVE TO TRASH ".to_string(), lines, 14, 0)
        }
        Mode::Bookmarks { selected } => {
            let mut lines = vec![
                Line::from("Enter to jump · m saves the current folder · Esc closes"),
                Line::default(),
            ];
            lines.extend(app.bookmarks.iter().enumerate().map(|(i, (name, path))| {
                Line::from(Span::styled(
                    format!(
                        " {}  {:<14} {}",
                        if i == *selected { "›" } else { " " },
                        name,
                        App::displayed_path(path)
                    ),
                    if i == *selected {
                        style(Color::White, color(&t.selection))
                    } else {
                        Style::default().fg(color(&t.foreground))
                    },
                ))
            }));
            (" BOOKMARKS ".into(), lines, 20, selected.saturating_sub(10))
        }
        _ => return,
    };
    let width = area.width.saturating_sub(6).min(84);
    let height = desired_height.min(area.height.saturating_sub(2));
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(title)
        .border_style(Style::default().fg(color(&t.accent)))
        .style(style(color(&t.foreground), color(&t.panel)));
    let content = block.inner(popup);
    frame.render_widget(block, popup);
    let inner = Rect::new(
        content.x + 1,
        content.y,
        content.width.saturating_sub(2),
        content.height,
    );
    let max = lines.len().saturating_sub(inner.height as usize);
    frame.render_widget(
        Paragraph::new(lines).scroll((scroll.min(max) as u16, 0)),
        inner,
    );
}

fn undo_overlay(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let t = &app.config.theme;
    let width = area.width.saturating_sub(4).min(76);
    let height = area.height.saturating_sub(2).min(22);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" UNDO HISTORY ")
        .border_style(Style::default().fg(color(&t.accent)))
        .style(style(color(&t.foreground), color(&t.panel)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let sections = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Zuru can undo the newest completed operation."),
            Line::from(Span::styled(
                "Trash restoration is available where the operating system exposes it.",
                Style::default().fg(color(&t.muted)),
            )),
        ]),
        sections[0],
    );
    let lines = if app.undo_history.is_empty() {
        vec![Line::from(Span::styled(
            "\n  Nothing to undo yet",
            Style::default().fg(color(&t.muted)),
        ))]
    } else {
        app.undo_history
            .iter()
            .take(sections[1].height as usize)
            .enumerate()
            .map(|(index, action)| {
                Line::from(Span::styled(
                    format!(" {} {}", if index == 0 { "›" } else { " " }, action.label()),
                    if index == 0 {
                        style(Color::White, color(&t.selection)).bold()
                    } else {
                        Style::default().fg(color(&t.muted))
                    },
                ))
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), sections[1]);
    frame.render_widget(
        Paragraph::new(if app.undo_history.is_empty() {
            " Esc / u close"
        } else {
            " Enter undo newest    Esc / u close"
        })
        .style(style(color(&t.muted), color(&t.slate))),
        sections[2],
    );
}

fn sort_overlay(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let t = &app.config.theme;
    let tab = app.tab();
    let width = area.width.saturating_sub(4).min(62);
    let height = area.height.saturating_sub(2).min(15);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" SORT THIS TAB ")
        .border_style(Style::default().fg(color(&t.accent)))
        .style(style(color(&t.foreground), color(&t.panel)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let active = |mode| if tab.sort == mode { "●" } else { "○" };
    let lines = vec![
        Line::from("Choose how items are ordered. Changes apply immediately."),
        Line::default(),
        Line::from(format!(" n  {}  Name", active(crate::app::SortMode::Name))),
        Line::from(format!(
            " e  {}  File extension",
            active(crate::app::SortMode::Extension)
        )),
        Line::from(format!(
            " s  {}  File size",
            active(crate::app::SortMode::Size)
        )),
        Line::from(format!(
            " m  {}  Modified time",
            active(crate::app::SortMode::Modified)
        )),
        Line::default(),
        Line::from(format!(
            " r  Reverse order       {}",
            if tab.sort_reverse { "ON" } else { "OFF" }
        )),
        Line::from(format!(
            " d  Keep folders first  {}",
            if tab.directories_first { "ON" } else { "OFF" }
        )),
        Line::default(),
        Line::from(Span::styled(
            "Esc / , closes · this tab remembers its choices",
            Style::default().fg(color(&t.muted)),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
        Rect::new(
            inner.x + 1,
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        ),
    );
}

fn bulk_rename_overlay(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let t = app.config.theme.clone();
    let Mode::BulkRename(review) = &mut app.mode else {
        return;
    };
    let width = area.width.saturating_sub(2).min(104);
    let height = area.height.saturating_sub(2).min(30);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" BULK RENAME REVIEW ")
        .border_style(Style::default().fg(color(if review.has_errors() {
            &t.danger
        } else {
            &t.accent
        })))
        .style(style(color(&t.foreground), color(&t.panel)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let sections = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let changed = review
        .changes
        .iter()
        .filter(|change| matches!(change.status, RenameStatus::Ready))
        .count();
    let problems = review
        .changes
        .iter()
        .filter(|change| change.status.problem().is_some())
        .count();
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "{changed} change(s) · {problems} problem(s) · review every old → new name"
            )),
            Line::from(Span::styled(
                "Problems must be fixed in the editor before Zuru can apply the batch.",
                Style::default().fg(color(&t.muted)),
            )),
        ]),
        sections[0],
    );
    let visible = sections[1].height.max(1) as usize;
    let max_scroll = review.changes.len().saturating_sub(visible);
    review.scroll = review.scroll.min(max_scroll);
    let lines = review
        .changes
        .iter()
        .skip(review.scroll)
        .take(visible)
        .map(|change| {
            let (icon, line_color, suffix) = match &change.status {
                RenameStatus::Ready => ("✓", color(&t.normal), String::new()),
                RenameStatus::Unchanged => ("=", color(&t.muted), " — unchanged".into()),
                RenameStatus::Invalid(message) | RenameStatus::Collision(message) => {
                    ("!", color(&t.danger), format!(" — {message}"))
                }
            };
            let old_name = safe_text(&change.old_name);
            let new_name = safe_text(&change.new_name);
            let suffix = safe_text(&suffix);
            Line::from(Span::styled(
                tail(
                    &format!(" {icon} {old_name} → {new_name}{suffix}"),
                    sections[1].width.saturating_sub(1) as usize,
                ),
                Style::default().fg(line_color),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), sections[1]);
    let footer = if review.has_errors() {
        " e edit again    j/k scroll    Esc cancel"
    } else {
        " Enter / a apply    e edit again    j/k scroll    Esc cancel"
    };
    frame.render_widget(
        Paragraph::new(footer).style(style(color(&t.muted), color(&t.slate))),
        sections[2],
    );
}

fn task_overlay(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let t = &app.config.theme;
    let width = area.width.saturating_sub(4).min(88);
    let height = area.height.saturating_sub(2).min(22);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" TASKS ")
        .border_style(Style::default().fg(color(&t.accent)))
        .style(style(color(&t.foreground), color(&t.panel)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let sections = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);
    if let Some(task) = &app.task {
        let item = format!(
            "{}/{} items · {} / {} · {}/s",
            task.completed_items,
            task.total_items,
            human_size(task.bytes_done),
            human_size(task.total_bytes),
            human_size(task.bytes_per_second)
        );
        let path = task
            .current
            .as_deref()
            .map(App::displayed_path)
            .unwrap_or_else(|| "Preparing…".into());
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    task.label.clone(),
                    Style::default().fg(color(&t.accent)).bold(),
                )),
                Line::from(item),
                Line::from(Span::styled(
                    tail(&path, sections[0].width.saturating_sub(1) as usize),
                    Style::default().fg(color(&t.muted)),
                )),
            ]),
            sections[0],
        );
        frame.render_widget(
            Gauge::default()
                .ratio(f64::from(task.percent()) / 100.0)
                .label(format!("{}%", task.percent()))
                .gauge_style(style(color(&t.background), color(&t.accent)).bold()),
            Rect::new(
                sections[0].x,
                sections[0].bottom().saturating_sub(1),
                sections[0].width,
                1,
            ),
        );
    } else {
        frame.render_widget(
            Paragraph::new("No active task").style(Style::default().fg(color(&t.muted))),
            sections[0],
        );
    }
    frame.render_widget(
        Paragraph::new("RECENT").style(Style::default().fg(color(&t.muted)).bold()),
        sections[1],
    );
    let history = app
        .task_history
        .iter()
        .take(sections[2].height as usize)
        .map(|result| {
            let icon = if result.cancelled {
                "■"
            } else if result.error {
                "!"
            } else {
                "✓"
            };
            Line::from(Span::styled(
                format!(" {icon} {}", result.message),
                Style::default().fg(color(if result.error {
                    &t.danger
                } else if result.cancelled {
                    &t.command
                } else {
                    &t.normal
                })),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(history), sections[2]);
    frame.render_widget(
        Paragraph::new(if app.busy {
            " c cancel task    Esc / w close"
        } else {
            " Esc / w close"
        })
        .style(style(color(&t.muted), color(&t.slate))),
        sections[3],
    );
}

fn conflict_overlay(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Mode::Conflict(conflict) = &app.mode else {
        return;
    };
    let t = &app.config.theme;
    let width = area.width.saturating_sub(4).min(92);
    let height = area.height.saturating_sub(2).min(16);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" FILE ALREADY EXISTS ")
        .border_style(Style::default().fg(color(&t.command)))
        .style(style(color(&t.foreground), color(&t.panel)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let source = App::displayed_path(&conflict.source);
    let target = App::displayed_path(&conflict.target);
    let lines = vec![
        Line::from(format!(
            "Zuru found a {} with the same name.",
            if conflict.is_dir { "folder" } else { "file" }
        )),
        Line::default(),
        Line::from(Span::styled(
            format!(
                "From: {}",
                tail(&source, inner.width.saturating_sub(7) as usize)
            ),
            Style::default().fg(color(&t.muted)),
        )),
        Line::from(Span::styled(
            format!(
                "To:   {}",
                tail(&target, inner.width.saturating_sub(7) as usize)
            ),
            Style::default().fg(color(&t.muted)),
        )),
        Line::default(),
        Line::from("s Skip       k Keep Both       r Replace (existing goes to trash)"),
        Line::from("S Skip All   K Keep All        R Replace All"),
        Line::default(),
        Line::from(Span::styled(
            "Esc / c cancels the task",
            Style::default().fg(color(&t.danger)),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
        Rect::new(
            inner.x + 1,
            inner.y + 1,
            inner.width.saturating_sub(2),
            inner.height.saturating_sub(2),
        ),
    );
}

fn help_overlay(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Mode::Help { page, scroll } = app.mode else {
        return;
    };
    let t = &app.config.theme;
    let width = area.width.saturating_sub(2).min(100);
    let height = area.height.saturating_sub(2).min(36);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let block = Block::bordered()
        .title(" ZURU HELP ")
        .border_style(Style::default().fg(color(&t.accent)))
        .style(style(color(&t.foreground), color(&t.panel)));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let short = ["Start", "Files", "View", "Places", "Cmds"];
    let labels = if inner.width < 55 {
        &short
    } else {
        &crate::help::PAGES
    };
    let tabs = labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            Span::styled(
                format!(" {} {} ", index + 1, label),
                if index == page {
                    style(color(&t.background), color(&t.accent)).bold()
                } else {
                    style(color(&t.muted), color(&t.slate))
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Line::from(tabs)), areas[0]);
    let body = Rect::new(
        areas[2].x + 1,
        areas[2].y,
        areas[2].width.saturating_sub(2),
        areas[2].height,
    );
    let lines = crate::help::lines(&app.config, page, body.width as usize);
    app.help_view_height = body.height.max(1) as usize;
    app.help_scroll_max = lines.len().saturating_sub(app.help_view_height);
    let scroll = scroll.min(app.help_scroll_max);
    if let Mode::Help { scroll: actual, .. } = &mut app.mode {
        *actual = scroll;
    }
    let more = if app.help_scroll_max == 0 {
        String::new()
    } else {
        format!("  {}/{}", scroll + 1, app.help_scroll_max + 1)
    };
    let footer = if inner.width < 60 {
        format!(" Tab page · ↑/↓ scroll · Esc close{more}")
    } else {
        format!(" Tab / ← → change page    ↑ ↓ / PgDn scroll    Esc close{more}")
    };
    frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), body);
    frame.render_widget(
        Paragraph::new(footer).style(style(color(&t.muted), color(&t.slate))),
        areas[3],
    );
}

pub fn tail(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 1;
    for c in text.chars().rev() {
        let n = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + n > width {
            break;
        }
        result.push(c);
        used += n;
    }
    format!("…{}", result.chars().rev().collect::<String>())
}
