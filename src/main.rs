use anyhow::{bail, Context, Result};
use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use ratatui_image::picker::{Picker, ProtocolType};
use std::{
    io::{self, IsTerminal, Stdout, Write},
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use zuru::{
    app::{App, ExternalAction},
    config::{self, Config},
    ui,
};

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Restores the user's terminal on errors, normal exits, and panics.
struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        Ok(guard)
    }
    fn suspend(&self) -> Result<()> {
        disable_raw_mode()?;
        execute!(
            io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        )?;
        Ok(())
    }
    fn resume(&self) -> Result<()> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        Ok(())
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("zuru: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let launch_dir = std::env::current_dir()?;
    let mut cwd = launch_dir.clone();
    let mut config_path = config::config_path();
    let mut protocol_override = None;
    let mut chooser_file = None;
    let mut cwd_file = None;
    let mut args = std::env::args_os().skip(1);
    let mut print_config = false;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!("Zuru · three-pane terminal file manager\n\nUsage: zuru [DIRECTORY] [OPTIONS]\n\n  --config PATH          Use a specific TOML configuration\n  --protocol PROTOCOL    auto, halfblocks, kitty, iterm2, or sixel\n  --chooser-file PATH    Write files chosen with Enter to PATH\n  --cwd-file PATH        Write the final directory to PATH on exit\n  --print-config         Print the default TOML configuration\n  --config-path          Print the configuration file location\n  --version              Print version\n\nNavigation: h/j/k/l, arrows, gg/G. Press ? inside the app for all keys.\nRun in a real terminal with a Nerd Font for the best experience.");
                return Ok(());
            }
            Some("--version" | "-V") => {
                println!("zuru {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            Some("--config") => {
                config_path = PathBuf::from(args.next().context("--config requires a path")?)
            }
            Some("--protocol") => {
                protocol_override = Some(
                    args.next()
                        .context("--protocol requires a value")?
                        .to_string_lossy()
                        .to_string(),
                )
            }
            Some("--chooser-file") => {
                chooser_file = Some(output_path(
                    args.next().context("--chooser-file requires a path")?,
                    &launch_dir,
                ));
            }
            Some("--cwd-file") => {
                cwd_file = Some(output_path(
                    args.next().context("--cwd-file requires a path")?,
                    &launch_dir,
                ));
            }
            Some("--print-config") => print_config = true,
            Some("--config-path") => {
                println!("{}", config_path.display());
                return Ok(());
            }
            Some(s) if s.starts_with('-') => bail!("Unknown option: {s}"),
            _ => cwd = PathBuf::from(arg),
        }
    }
    if print_config {
        println!("{}", toml::to_string_pretty(&Config::default())?);
        return Ok(());
    }
    let mut config = Config::load(&config_path)?;
    if let Some(protocol) = protocol_override {
        config.image_protocol = protocol;
        config.validate()?;
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("An interactive terminal is required. Run `cargo run` in Windows Terminal, PowerShell, Kitty, WezTerm, or iTerm2.");
    }
    if let Some(path) = &chooser_file {
        std::fs::write(path, "")
            .with_context(|| format!("Cannot initialize chooser file {}", path.display()))?;
    }
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        previous_hook(info);
    }));
    let guard = TerminalGuard::enter()?;
    let mut picker = if config.image_protocol == "halfblocks" {
        Picker::halfblocks()
    } else {
        Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
    };
    match config.image_protocol.as_str() {
        "kitty" => picker.set_protocol_type(ProtocolType::Kitty),
        "iterm2" => picker.set_protocol_type(ProtocolType::Iterm2),
        "sixel" => picker.set_protocol_type(ProtocolType::Sixel),
        "halfblocks" => picker.set_protocol_type(ProtocolType::Halfblocks),
        _ => {}
    }
    if let ratatui::style::Color::Rgb(r, g, b) = config::color(&config.theme.background) {
        picker.set_background_color(Some(image::Rgba([r, g, b, 255])));
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let mut app = App::new(cwd, config, config_path, picker)?;
    if chooser_file.is_some() {
        app.enable_chooser();
    }
    while !app.should_quit {
        app.poll();
        if app.dirty {
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            app.dirty = false;
        }
        if event::poll(Duration::from_millis(30))? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key),
                Event::Resize(_, _) => {
                    terminal.clear()?;
                    app.dirty = true;
                }
                Event::Paste(text) => app.handle_paste(&text),
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollDown => app.action("down"),
                    MouseEventKind::ScrollUp => app.action("up"),
                    _ => {}
                },
                _ => {}
            }
        }
        if let Some(action) = app.external.take() {
            external(&mut terminal, &guard, &mut app, action)?;
        }
    }
    let final_cwd = app.tab().cwd.clone();
    let chosen = app.chosen.take();
    drop(terminal);
    drop(guard);
    if let Some(path) = cwd_file {
        write_paths(&path, &[final_cwd])?;
    }
    if let (Some(path), Some(chosen)) = (chooser_file, chosen) {
        write_paths(&path, &chosen)?;
    }
    Ok(())
}

fn output_path(path: std::ffi::OsString, launch_dir: &std::path::Path) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        launch_dir.join(path)
    }
}

fn write_paths(path: &std::path::Path, paths: &[PathBuf]) -> Result<()> {
    let mut text = String::new();
    for value in paths {
        let value = output_text(value);
        if value.contains(['\r', '\n']) {
            bail!(
                "Cannot write a path containing a line break to {}",
                path.display()
            );
        }
        text.push_str(&value);
        text.push('\n');
    }
    std::fs::write(path, text).with_context(|| format!("Cannot write {}", path.display()))
}

#[cfg(windows)]
fn output_text(path: &std::path::Path) -> std::borrow::Cow<'_, str> {
    let text = path.to_string_lossy();
    if let Some(path) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}").into()
    } else if let Some(path) = text.strip_prefix(r"\\?\") {
        path.to_owned().into()
    } else {
        text
    }
}

#[cfg(not(windows))]
fn output_text(path: &std::path::Path) -> std::borrow::Cow<'_, str> {
    path.to_string_lossy()
}

fn external(
    terminal: &mut Tui,
    guard: &TerminalGuard,
    app: &mut App,
    action: ExternalAction,
) -> Result<()> {
    let (path, bulk_sources) = match action {
        ExternalAction::Edit(path) => (path, None),
        ExternalAction::BulkRename { plan, sources } => (plan, Some(sources)),
    };
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "notepad".into()
            } else {
                "vi".into()
            }
        });
    let parts = if std::path::Path::new(&editor).is_file() {
        vec![editor]
    } else {
        match shell_words::split(&editor) {
            Ok(parts) => parts,
            Err(e) => {
                if bulk_sources.is_some() {
                    let _ = std::fs::remove_file(&path);
                }
                app.notice(
                    format!("Could not parse EDITOR: {e}; quote paths containing spaces"),
                    true,
                );
                return Ok(());
            }
        }
    };
    if parts.is_empty() {
        if bulk_sources.is_some() {
            let _ = std::fs::remove_file(&path);
        }
        app.notice("EDITOR is empty", true);
        return Ok(());
    }
    guard.suspend()?;
    io::stdout().flush()?;
    let result = Command::new(&parts[0])
        .args(&parts[1..])
        .arg(&path)
        .current_dir(&app.tab().cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    guard.resume()?;
    terminal.clear()?;
    match (result, bulk_sources) {
        (Ok(status), Some(sources)) if status.success() => {
            let edited = std::fs::read_to_string(&path);
            let _ = std::fs::remove_file(&path);
            match edited {
                Ok(edited) => app.review_bulk_rename(sources, &edited),
                Err(error) => app.notice(format!("Could not read rename plan: {error}"), true),
            }
        }
        (Ok(status), Some(_)) => {
            let _ = std::fs::remove_file(&path);
            app.notice(
                format!("Editor exited with {status}; rename cancelled"),
                true,
            );
        }
        (Err(error), Some(_)) => {
            let _ = std::fs::remove_file(&path);
            app.notice(format!("Could not start editor: {error}"), true);
        }
        (Ok(status), None) if status.success() => {
            app.notice("Editor closed", false);
            app.refresh();
        }
        (Ok(status), None) => {
            app.notice(format!("Editor exited with {status}"), true);
            app.refresh();
        }
        (Err(error), None) => {
            app.notice(format!("Could not start editor: {error}"), true);
            app.refresh();
        }
    }
    app.dirty = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integration_paths_are_written_one_per_line() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("paths.txt");
        write_paths(
            &output,
            &[
                directory.path().join("one.txt"),
                directory.path().join("two.txt"),
            ],
        )
        .unwrap();
        let content = std::fs::read_to_string(output).unwrap();
        assert_eq!(content.lines().count(), 2);
        assert!(content.contains("one.txt"));
        assert!(content.contains("two.txt"));
    }

    #[cfg(windows)]
    #[test]
    fn extended_windows_paths_are_made_shell_friendly() {
        assert_eq!(
            output_text(std::path::Path::new(r"\\?\C:\work\file.txt")),
            r"C:\work\file.txt"
        );
        assert_eq!(
            output_text(std::path::Path::new(r"\\?\UNC\server\share\file.txt")),
            r"\\server\share\file.txt"
        );
    }
}
