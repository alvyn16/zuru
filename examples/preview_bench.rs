//! Repeatable end-to-end preview timings; run with two images and a text file.
use anyhow::{ensure, Context, Result};
use ratatui::layout::Size;
use ratatui_image::picker::Picker;
use std::{
    path::{Path, PathBuf},
    time::Instant,
};
use zuru::{
    config::Config,
    files::Entry,
    preview::{Preview, PreviewRequest, Previewer},
};

fn measure(previewer: &mut Previewer, path: &Path, zoom: bool, scroll: usize) -> Result<f64> {
    let request = PreviewRequest {
        generation: 1,
        entry: Entry::read(path.to_path_buf())?,
        size: Size::new(56, 28),
        scroll,
        zoom,
        hidden: false,
        force: false,
        preload: Vec::new(),
    };
    let started = Instant::now();
    let result = previewer.load(&request);
    if let Preview::Error(error) = result {
        anyhow::bail!("{error}");
    }
    Ok(started.elapsed().as_secs_f64() * 1000.0)
}

fn main() -> Result<()> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    ensure!(
        args.len() == 3,
        "Usage: preview_bench IMAGE_A IMAGE_B TEXT_FILE"
    );
    let started = Instant::now();
    let mut previewer = Previewer::new(Picker::halfblocks(), Config::default());
    let mut values = serde_json::Map::new();
    values.insert(
        "initialize_ms".into(),
        started.elapsed().as_secs_f64().mul_add(1000.0, 0.0).into(),
    );
    for (label, path, zoom, scroll) in [
        ("image_cold_ms", &args[0], false, 0),
        ("image_repeat_ms", &args[0], false, 0),
        ("image_b_ms", &args[1], false, 0),
        ("image_revisit_ms", &args[0], false, 0),
        ("image_zoom_ms", &args[0], true, 0),
        ("image_pan_ms", &args[0], true, 3),
        ("text_cold_ms", &args[2], false, 0),
        ("text_repeat_ms", &args[2], false, 0),
    ] {
        values.insert(
            label.into(),
            measure(&mut previewer, path, zoom, scroll)
                .with_context(|| label.to_string())?
                .into(),
        );
    }
    println!("{}", serde_json::to_string_pretty(&values)?);
    Ok(())
}
