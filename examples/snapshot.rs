//! Export the real Ratatui buffer for repeatable visual QA without a terminal.
use anyhow::{Context, Result};
use ratatui::{
    backend::TestBackend,
    style::{Color, Modifier},
    Terminal,
};
use ratatui_image::picker::Picker;
use std::{
    fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
use zuru::{app::App, config::Config, preview::Preview, ui};

fn rgb(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::White => "#ffffff".into(),
        Color::Black => "#000000".into(),
        _ => "#d8e2f1".into(),
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let directory = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let output = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "artifacts/snapshot.json".into()),
    );
    let filename = args.next().map(|s| s.to_string_lossy().to_string());
    let mut app = App::new(
        directory,
        Config::default(),
        PathBuf::from("zuru.example.toml"),
        Picker::halfblocks(),
    )?;
    let mut terminal = Terminal::new(TestBackend::new(128, 34))?;
    let start = Instant::now();
    let mut selected = filename.is_none();
    loop {
        app.poll();
        if !app.tab().loading && !selected {
            let index = app
                .tab()
                .visible
                .iter()
                .position(|&i| Some(&app.tab().entries[i].name) == filename.as_ref())
                .context("Selected filename not found")?;
            app.tab_mut().selected = index;
            app.request_preview(true);
            selected = true;
        }
        terminal.draw(|f| ui::draw(f, &mut app))?;
        if !app.tab().loading
            && selected
            && !app.preview_pending
            && !matches!(app.preview, Preview::Loading)
        {
            break;
        }
        anyhow::ensure!(
            start.elapsed() < Duration::from_secs(20),
            "Preview timed out"
        );
        thread::sleep(Duration::from_millis(15));
    }
    let buffer = terminal.backend().buffer();
    let mut cells = Vec::new();
    for y in 0..34 {
        for x in 0..128 {
            let cell = &buffer[(x, y)];
            cells.push((
                x,
                y,
                cell.symbol().to_string(),
                rgb(cell.fg),
                rgb(cell.bg),
                cell.modifier.contains(Modifier::BOLD),
            ));
        }
    }
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &output,
        serde_json::to_vec(&serde_json::json!({ "width": 128, "height": 34, "cells": cells }))?,
    )?;
    println!("Wrote {}", output.display());
    Ok(())
}
