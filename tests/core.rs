use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, layout::Size, Terminal};
use ratatui_image::picker::Picker;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use zuru::{
    app::{App, ExternalAction, Mode, RenameStatus, SortMode},
    archive,
    config::{self, Config},
    files::{self, Entry},
    media::MediaKind,
    operations::{
        self, Conflict, ConflictChoice, Operation, OperationEvent, OperationResult, TaskProgress,
        UndoAction,
    },
    preview::{Preview, PreviewRequest, Previewer},
    search::SearchKind,
    ui,
};

struct TestArchiveProgress;

impl archive::Progress for TestArchiveProgress {
    fn check(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn current(&mut self, _path: &Path) {}

    fn add_bytes(&mut self, _bytes: u64) {}
}

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("alpha")).unwrap();
    fs::create_dir(dir.path().join("beta")).unwrap();
    fs::write(
        dir.path().join("code.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )
    .unwrap();
    fs::write(dir.path().join("notes.txt"), "plain text\n").unwrap();
    fs::write(dir.path().join(".hidden"), "secret").unwrap();
    dir
}

fn app(path: &Path) -> App {
    App::new(
        path.into(),
        Config::default(),
        path.join("config.toml"),
        Picker::halfblocks(),
    )
    .unwrap()
}

fn wait_until(app: &mut App, ready: impl Fn(&App) -> bool) {
    let start = Instant::now();
    loop {
        app.poll();
        if ready(app) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(12),
            "Timed out waiting for worker"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn loaded(app: &mut App) {
    wait_until(app, |a| {
        !a.tab().loading && !a.preview_pending && !matches!(a.preview, Preview::Loading)
    });
}

fn key(app: &mut App, c: char) {
    app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
}

#[test]
fn directory_order_hidden_and_metadata() {
    let dir = fixture();
    let entries = files::read_dir(dir.path(), false).unwrap();
    assert_eq!(
        entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        ["alpha", "beta", "code.rs", "notes.txt"]
    );
    assert!(entries[0].is_dir);
    assert_eq!(entries[2].icon(), "");
    assert_eq!(entries[2].permissions.len(), 10);
    assert_eq!(files::read_dir(dir.path(), true).unwrap().len(), 5);
    assert!(files::read_dir(&dir.path().join("missing"), true).is_err());
}

#[test]
fn copy_tree_and_reject_collisions_or_descendants() {
    let dir = fixture();
    let source = dir.path().join("alpha");
    fs::create_dir(source.join("nested")).unwrap();
    fs::write(source.join("nested/hello.txt"), "hello").unwrap();
    let destination = dir.path().join("beta");
    let target = operations::copy_into(&source, &destination).unwrap();
    assert_eq!(
        fs::read_to_string(target.join("nested/hello.txt")).unwrap(),
        "hello"
    );
    fs::write(target.join("nested/hello.txt"), "keep me").unwrap();
    assert!(operations::copy_into(&source, &destination).is_err());
    assert_eq!(
        fs::read_to_string(target.join("nested/hello.txt")).unwrap(),
        "keep me"
    );
    assert!(operations::copy_into(&source, &source.join("nested")).is_err());
    assert!(!source.join("nested/alpha").exists());
}

#[test]
fn file_copy_never_overwrites_and_rename_validates_names() {
    let dir = fixture();
    let source = dir.path().join("notes.txt");
    assert!(operations::copy_into(&source, dir.path()).is_err());
    assert!(operations::execute(Operation::Rename {
        source: source.clone(),
        name: "code.rs".into()
    })
    .is_err());
    for name in ["", ".", "..", "../escape", "sub/name", "sub\\name"] {
        assert!(operations::validate_name(name).is_err());
    }
    operations::execute(Operation::Rename {
        source: source.clone(),
        name: "a long 日本語 name.txt".into(),
    })
    .unwrap();
    assert!(!source.exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("a long 日本語 name.txt")).unwrap(),
        "plain text\n"
    );
    assert!(operations::execute(Operation::Touch(dir.path().join("code.rs"))).is_err());
}

#[test]
fn archives_create_list_preview_and_extract_all_supported_formats() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("project");
    fs::create_dir_all(source.join("nested")).unwrap();
    fs::write(source.join("readme.txt"), "zuru archive\n").unwrap();
    fs::write(source.join("nested/data.bin"), [1, 2, 3, 4]).unwrap();

    for extension in ["zip", "tar", "tar.gz", "tgz"] {
        let archive_path = dir.path().join(format!("bundle-{extension}.{extension}"));
        archive::create(
            &archive_path,
            std::slice::from_ref(&source),
            &mut TestArchiveProgress,
        )
        .unwrap();
        let listing = archive::listing(&archive_path).unwrap();
        assert_eq!(listing.total_entries, 4);
        assert_eq!(listing.total_bytes, 17);
        assert!(listing
            .entries
            .iter()
            .any(|entry| entry.path == "project/readme.txt"));
        assert!(listing
            .entries
            .iter()
            .any(|entry| entry.path == "project/nested/data.bin"));

        let output = dir.path().join(format!("output-{extension}"));
        archive::extract(&archive_path, &output, &mut TestArchiveProgress).unwrap();
        assert_eq!(
            fs::read_to_string(output.join("project/readme.txt")).unwrap(),
            "zuru archive\n"
        );
        assert_eq!(
            fs::read(output.join("project/nested/data.bin")).unwrap(),
            [1, 2, 3, 4]
        );

        let mut previewer = Previewer::new(Picker::halfblocks(), Config::default());
        let preview = previewer.load(&request(archive_path));
        assert!(matches!(preview, Preview::Archive(_)));
    }
}

#[test]
fn archive_extraction_rejects_parent_path_escape() {
    let dir = tempfile::tempdir().unwrap();
    let archive_path = dir.path().join("unsafe.zip");
    let mut writer = zip::ZipWriter::new(fs::File::create(&archive_path).unwrap());
    writer
        .start_file("../escape.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"must stay contained").unwrap();
    writer.finish().unwrap();

    let output = dir.path().join("unpacked");
    let error = archive::extract(&archive_path, &output, &mut TestArchiveProgress).unwrap_err();
    assert!(error.to_string().contains("Extraction incomplete"));
    assert!(!dir.path().join("escape.txt").exists());
}

#[test]
fn app_records_and_applies_rename_undo() {
    let dir = fixture();
    let original = dir.path().join("notes.txt");
    let renamed = dir.path().join("journal.txt");
    let mut app = app(dir.path());
    loaded(&mut app);
    app.action("last");
    app.action("rename");
    app.input = "journal.txt".into();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    wait_until(&mut app, |app| !app.busy);
    assert!(!original.exists());
    assert!(renamed.exists());
    assert_eq!(app.undo_history.len(), 1);

    app.action("undo");
    assert!(matches!(app.mode, Mode::UndoHistory));
    let mut terminal = Terminal::new(TestBackend::new(90, 25)).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(rendered.contains("UNDO HISTORY"));
    assert!(rendered.contains("rename journal.txt"));

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    wait_until(&mut app, |app| !app.busy);
    assert!(original.exists());
    assert!(!renamed.exists());
    assert!(app.undo_history.is_empty());
}

#[test]
fn undo_refuses_to_remove_a_created_folder_after_it_gains_contents() {
    let dir = tempfile::tempdir().unwrap();
    let created = dir.path().join("created");
    fs::create_dir(&created).unwrap();
    fs::write(created.join("keep.txt"), "keep").unwrap();
    let error = operations::execute(Operation::Undo(UndoAction::Filesystem {
        label: "folder creation".into(),
        remove: vec![created.clone()],
        restore: Vec::new(),
        restore_renames: Vec::new(),
        remove_nonempty_directories: false,
    }))
    .unwrap_err();
    assert!(error.to_string().contains("no longer empty"));
    assert_eq!(
        fs::read_to_string(created.join("keep.txt")).unwrap(),
        "keep"
    );
}

#[test]
fn chooser_enters_folders_and_returns_only_files() {
    let dir = fixture();
    let mut app = app(dir.path());
    loaded(&mut app);
    app.enable_chooser();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    loaded(&mut app);
    assert!(app.tab().cwd.ends_with("alpha"));
    assert!(app.chosen.is_none());
    assert!(!app.should_quit);

    app.action("parent");
    loaded(&mut app);
    app.action("last");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.should_quit);
    let chosen = app.chosen.as_ref().unwrap();
    assert_eq!(chosen.len(), 1);
    assert_eq!(chosen[0].file_name().unwrap(), "notes.txt");
    assert!(chosen[0].is_absolute());
    assert!(chosen[0].exists());
}

#[test]
fn bulk_copy_reports_partial_failure_without_clobbering() {
    let dir = fixture();
    let dest = dir.path().join("alpha");
    fs::write(dest.join("code.rs"), "keep").unwrap();
    let result = operations::execute(Operation::Paste {
        sources: vec![dir.path().join("code.rs"), dir.path().join("notes.txt")],
        destination: dest.clone(),
        cut: false,
    });
    assert!(result.unwrap_err().to_string().contains("Copied 1/2"));
    assert_eq!(fs::read_to_string(dest.join("code.rs")).unwrap(), "keep");
    assert!(dest.join("notes.txt").exists());
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed_during_recursive_copy() {
    use std::os::unix::fs::symlink;
    let dir = fixture();
    symlink(dir.path().join("notes.txt"), dir.path().join("alpha/link")).unwrap();
    assert!(operations::copy_into(&dir.path().join("alpha"), &dir.path().join("beta")).is_err());
    assert!(!dir.path().join("beta/alpha").exists());
    assert!(
        operations::copy_into(&dir.path().join("alpha/link"), &dir.path().join("beta")).is_err()
    );
}

#[test]
fn config_defaults_partial_theme_and_key_validation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    assert_eq!(Config::load(&path).unwrap().theme.background, "#0d1526");
    fs::write(
        &path,
        "[theme]\nselection = '#123456'\n[keys]\nup = ['w', 'up']\ntasks = []\n",
    )
    .unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.theme.selection, "#123456");
    assert_eq!(config.theme.background, "#0d1526");
    fs::write(&path, "[keys]\nup = ['j']").unwrap();
    assert!(Config::load(&path)
        .unwrap_err()
        .to_string()
        .contains("both"));
    fs::write(&path, "[theme]\nselection = 'blue'").unwrap();
    assert!(Config::load(&path).is_err());
    assert_eq!(ui::tail("hello日本語", 6), "…本語");
    assert_eq!(files::safe_text("hello\x1b[2J\n"), "hello�[2J�");
}

#[test]
fn navigation_sequences_restore_cursor_and_tabs_keep_context() {
    let dir = fixture();
    let mut app = app(dir.path());
    loaded(&mut app);
    let original = app.tab().cwd.clone();
    key(&mut app, 'j');
    assert_eq!(app.tab().current().unwrap().name, "beta");
    key(&mut app, 'l');
    loaded(&mut app);
    assert!(app.tab().cwd.ends_with("beta"));
    key(&mut app, 'h');
    loaded(&mut app);
    assert_eq!(app.tab().current().unwrap().name, "beta");
    key(&mut app, 'g');
    key(&mut app, 'g');
    assert_eq!(app.tab().selected, 0);
    key(&mut app, 'G');
    assert_eq!(app.tab().current().unwrap().name, "notes.txt");
    key(&mut app, 't');
    loaded(&mut app);
    assert_eq!(app.tabs.len(), 2);
    app.navigate(dir.path().join("alpha"));
    loaded(&mut app);
    key(&mut app, '1');
    loaded(&mut app);
    assert_eq!(app.tab().cwd, original);
    assert_eq!(app.tab().current().unwrap().name, "notes.txt");
    key(&mut app, '2');
    loaded(&mut app);
    assert!(app.tab().cwd.ends_with("alpha"));
}

#[test]
fn sort_controls_apply_immediately_and_are_remembered_per_tab() {
    let dir = fixture();
    let mut app = app(dir.path());
    loaded(&mut app);

    key(&mut app, ',');
    assert!(matches!(app.mode, Mode::Sort));
    let mut terminal = Terminal::new(TestBackend::new(90, 26)).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(rendered.contains("SORT THIS TAB"));
    assert!(rendered.contains("Keep folders first"));
    key(&mut app, 's');
    assert_eq!(app.tab().sort, SortMode::Size);
    assert_eq!(
        app.tab()
            .entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["notes.txt", "code.rs"]
    );
    key(&mut app, 'r');
    assert!(app.tab().sort_reverse);
    assert!(app.tab().entries[..2].iter().all(|entry| entry.is_dir));
    assert_eq!(
        app.tab()
            .entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["code.rs", "notes.txt"]
    );
    key(&mut app, ',');
    key(&mut app, 't');
    loaded(&mut app);
    assert_eq!(app.tab().sort, SortMode::Size);
    assert!(app.tab().sort_reverse);

    key(&mut app, ',');
    key(&mut app, 'e');
    key(&mut app, 'd');
    assert_eq!(app.tab().sort, SortMode::Extension);
    assert!(!app.tab().directories_first);
    key(&mut app, 'm');
    assert_eq!(app.tab().sort, SortMode::Modified);
    key(&mut app, 'e');
    key(&mut app, ',');
    key(&mut app, '1');
    loaded(&mut app);
    assert_eq!(app.tab().sort, SortMode::Size);
    assert!(app.tab().sort_reverse);
    assert!(app.tab().directories_first);
}

#[test]
fn bulk_rename_plan_and_review_explain_collisions_before_apply() {
    let dir = fixture();
    let mut app = app(dir.path());
    loaded(&mut app);
    app.action("last");
    app.action("bulk_rename");
    let (plan, sources) = match app.external.take().expect("editor action") {
        ExternalAction::BulkRename { plan, sources } => (plan, sources),
        ExternalAction::Edit(_) => panic!("expected bulk rename plan"),
    };
    assert_eq!(
        sources[0].file_name().unwrap().to_string_lossy(),
        "notes.txt"
    );
    assert_eq!(fs::read_to_string(&plan).unwrap(), "notes.txt\n");
    fs::remove_file(plan).unwrap();

    app.review_bulk_rename(sources.clone(), "code.rs\n");
    let Mode::BulkRename(review) = &app.mode else {
        panic!("expected review")
    };
    assert!(matches!(
        review.changes[0].status,
        RenameStatus::Collision(_)
    ));
    key(&mut app, 'a');
    assert!(matches!(app.mode, Mode::BulkRename(_)));
    assert!(app.message.as_ref().is_some_and(|message| message.error));

    app.review_bulk_rename(sources, "journal.txt\n");
    let Mode::BulkRename(review) = &app.mode else {
        panic!("expected review")
    };
    assert!(matches!(review.changes[0].status, RenameStatus::Ready));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(rendered.contains("BULK RENAME REVIEW"));
    assert!(rendered.contains("notes.txt"));
    assert!(rendered.contains("journal.txt"));
    assert!(rendered.contains("0 problem(s)"));
    key(&mut app, 'a');
    wait_until(&mut app, |app| !app.busy);
    assert!(dir.path().join("journal.txt").exists());
    assert!(!dir.path().join("notes.txt").exists());
}

#[test]
fn bulk_rename_handles_swaps_without_clobbering() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.txt");
    let second = dir.path().join("second.txt");
    fs::write(&first, "FIRST").unwrap();
    fs::write(&second, "SECOND").unwrap();
    let message = operations::execute(Operation::BulkRename(vec![
        (first.clone(), second.clone()),
        (second.clone(), first.clone()),
    ]))
    .unwrap();
    assert_eq!(message, "Renamed 2 item(s)");
    assert_eq!(fs::read_to_string(first).unwrap(), "SECOND");
    assert_eq!(fs::read_to_string(second).unwrap(), "FIRST");
    assert!(fs::read_dir(dir.path()).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".zuru-rename-")));
}

#[test]
fn fuzzy_filter_visual_selection_and_configured_keys() {
    let dir = fixture();
    let mut app = app(dir.path());
    loaded(&mut app);
    key(&mut app, 'v');
    key(&mut app, 'j');
    key(&mut app, 'j');
    assert_eq!(app.targets().len(), 3);
    key(&mut app, 'k');
    assert_eq!(app.targets().len(), 2);
    app.action("escape");
    assert!(app.tab().marked.is_empty());
    key(&mut app, '/');
    app.handle_paste("cdrs");
    assert_eq!(app.tab().visible.len(), 1);
    assert_eq!(app.tab().current().unwrap().name, "code.rs");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.tab().filter, "cdrs");
    app.action("escape");
    assert_eq!(app.tab().visible.len(), 4);
    let path = dir.path().join("config.toml");
    fs::write(&path, "[keys]\ndown = ['w']\ntasks = []").unwrap();
    app.command("reload");
    loaded(&mut app);
    app.action("first");
    key(&mut app, 'w');
    assert_eq!(app.tab().selected, 1);
    key(&mut app, 'j');
    assert_eq!(app.tab().selected, 1);
}

#[test]
fn latest_directory_result_wins_after_rapid_navigation() {
    let dir = fixture();
    let mut app = app(dir.path());
    app.navigate(dir.path().join("alpha"));
    app.navigate(dir.path().join("beta"));
    app.navigate(dir.path().to_path_buf());
    loaded(&mut app);
    assert_eq!(app.tab().entries.len(), 4);
    // Going up highlights the child we just left, even if its read was superseded.
    assert_eq!(app.tab().current().unwrap().name, "beta");
}

#[test]
fn watcher_refreshes_external_changes_preserving_selection() {
    let dir = fixture();
    let mut app = app(dir.path());
    loaded(&mut app);
    app.action("last");
    fs::write(dir.path().join("external.txt"), "new").unwrap();
    wait_until(&mut app, |a| {
        a.tab().entries.iter().any(|e| e.name == "external.txt")
    });
    assert_eq!(app.tab().current().unwrap().name, "notes.txt");
}

#[test]
fn preview_folder_watches_and_text_scroll_survives_unrelated_changes() {
    let dir = fixture();
    let mut app = app(dir.path());
    loaded(&mut app);
    fs::write(dir.path().join("alpha/new.txt"), "arrived").unwrap();
    wait_until(
        &mut app,
        |a| matches!(&a.preview, Preview::Directory(entries) if entries.iter().any(|e| e.name == "new.txt")),
    );
    let long = (1..=200).map(|n| format!("line {n}\n")).collect::<String>();
    fs::write(dir.path().join("notes.txt"), long).unwrap();
    app.action("last");
    loaded(&mut app);
    app.scroll_preview(12);
    assert_eq!(app.preview_scroll, 12);
    fs::write(dir.path().join("something.txt"), "new").unwrap();
    wait_until(&mut app, |a| {
        a.tab().entries.iter().any(|e| e.name == "something.txt") && !a.preview_pending
    });
    assert_eq!(app.tab().current().unwrap().name, "notes.txt");
    assert_eq!(app.preview_scroll, 12);
}

#[test]
fn selected_file_write_refreshes_preview_even_when_listing_is_unchanged() {
    let dir = fixture();
    let mut app = app(dir.path());
    loaded(&mut app);
    app.action("last");
    loaded(&mut app);
    fs::write(dir.path().join("notes.txt"), "changed-preview\n").unwrap();
    wait_until(
        &mut app,
        |a| matches!(&a.preview, Preview::Text { lines, .. } if lines.first().is_some_and(|line| line.to_string().contains("changed-preview"))),
    );
    assert_eq!(app.tab().current().unwrap().name, "notes.txt");
}

#[test]
fn preview_cache_reuses_decoded_and_rendered_values_and_force_invalidates() {
    let dir = fixture();
    let path = dir.path().join("small.png");
    image::RgbImage::from_fn(80, 50, |x, y| image::Rgb([x as u8, y as u8, 90]))
        .save(&path)
        .unwrap();
    let mut previewer = Previewer::new(Picker::halfblocks(), Config::default());
    let mut req = request(path.clone());
    let _ = previewer.load(&req);
    let first = previewer.stats();
    let _ = previewer.load(&req);
    let second = previewer.stats();
    assert_eq!(first.image_decodes, 1);
    assert_eq!(second.image_decodes, 1);
    assert!(second.cache_hits > first.cache_hits);
    req.force = true;
    let _ = previewer.load(&req);
    assert_eq!(previewer.stats().image_decodes, 2);
}

#[test]
fn persistent_thumbnail_cache_avoids_a_second_full_decode() {
    let dir = fixture();
    let cache = dir.path().join("thumb-cache");
    let path = dir.path().join("cached.png");
    image::RgbImage::from_fn(320, 180, |x, y| image::Rgb([x as u8, y as u8, 120]))
        .save(&path)
        .unwrap();
    let request = request(path);
    let mut first = Previewer::with_thumbnail_cache(
        Picker::halfblocks(),
        Config::default(),
        Some(cache.clone()),
    );
    assert!(matches!(first.load(&request), Preview::Image { .. }));
    let started = Instant::now();
    while fs::read_dir(&cache)
        .map(|files| {
            let extensions = files
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    entry
                        .path()
                        .extension()
                        .map(|extension| extension.to_owned())
                })
                .collect::<Vec<_>>();
            !extensions.iter().any(|extension| extension == "qoi")
                || !extensions.iter().any(|extension| extension == "dim")
        })
        .unwrap_or(true)
    {
        assert!(started.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(10));
    }
    let mut second =
        Previewer::with_thumbnail_cache(Picker::halfblocks(), Config::default(), Some(cache));
    assert!(matches!(
        second.load(&request),
        Preview::Image {
            width: 320,
            height: 180,
            ..
        }
    ));
    assert_eq!(second.stats().image_decodes, 0);
    assert_eq!(second.stats().disk_cache_hits, 1);
}

#[test]
fn recursive_name_and_content_search_restore_the_original_folder() {
    let dir = fixture();
    fs::create_dir_all(dir.path().join("alpha/nested")).unwrap();
    fs::write(
        dir.path().join("alpha/nested/roadmap.md"),
        "A Unique Search Needle lives here.",
    )
    .unwrap();
    let mut app = app(dir.path());
    loaded(&mut app);
    app.start_search(SearchKind::Name, "ROADMAP".into());
    loaded(&mut app);
    assert_eq!(app.tab().entries.len(), 1);
    assert!(app.tab().current().unwrap().path.ends_with("roadmap.md"));
    let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(rendered.contains("alpha/nested/roadmap.md"));
    app.action("escape");
    loaded(&mut app);
    assert!(app.tab().search.is_none());
    assert_eq!(app.tab().entries.len(), 4);
    app.start_search(SearchKind::Contents, "unique search needle".into());
    loaded(&mut app);
    assert_eq!(app.tab().entries.len(), 1);
    assert!(app.tab().current().unwrap().path.ends_with("roadmap.md"));
    app.action("escape");
    loaded(&mut app);
    fs::write(dir.path().join(".gitignore"), "/generated/\n").unwrap();
    fs::create_dir_all(dir.path().join("generated")).unwrap();
    fs::write(dir.path().join("generated/ignored-hit.txt"), "ignored").unwrap();
    app.start_search(SearchKind::Name, "ignored-hit".into());
    loaded(&mut app);
    assert!(app.tab().entries.is_empty());
}

#[test]
fn operation_worker_reports_conflicts_progress_and_cancellation() {
    let dir = fixture();
    let destination = dir.path().join("alpha");
    fs::write(destination.join("notes.txt"), "existing").unwrap();
    let (controller, events) = operations::spawn_worker();
    let task_id = controller
        .submit(Operation::Paste {
            sources: vec![dir.path().join("notes.txt")],
            destination: destination.clone(),
            cut: false,
        })
        .unwrap();
    let mut saw_progress = false;
    loop {
        match events.recv_timeout(Duration::from_secs(5)).unwrap() {
            OperationEvent::Started(_) | OperationEvent::Progress(_) => saw_progress = true,
            OperationEvent::Conflict(conflict) => {
                assert_eq!(conflict.task_id, task_id);
                controller
                    .resolve(task_id, ConflictChoice::KeepBoth)
                    .unwrap();
            }
            OperationEvent::Finished(result) => {
                assert!(!result.error);
                assert!(!result.cancelled);
                break;
            }
        }
    }
    assert!(saw_progress);
    assert_eq!(
        fs::read_to_string(destination.join("notes (copy).txt")).unwrap(),
        "plain text\n"
    );
    assert_eq!(
        fs::read_to_string(destination.join("notes.txt")).unwrap(),
        "existing"
    );

    let skipped = dir.path().join("skip.txt");
    fs::write(&skipped, "stay").unwrap();
    let skip_id = controller
        .submit(Operation::Paste {
            sources: vec![skipped.clone()],
            destination: dir.path().to_path_buf(),
            cut: true,
        })
        .unwrap();
    loop {
        match events.recv_timeout(Duration::from_secs(5)).unwrap() {
            OperationEvent::Conflict(_) => {
                controller.resolve(skip_id, ConflictChoice::Skip).unwrap()
            }
            OperationEvent::Finished(result) => {
                assert!(result.moved_sources.is_none());
                break;
            }
            _ => {}
        }
    }
    assert!(skipped.exists());

    let cancel_id = controller
        .submit(Operation::Paste {
            sources: vec![dir.path().join("code.rs")],
            destination: dir.path().join("beta"),
            cut: false,
        })
        .unwrap();
    controller.cancel(cancel_id);
    loop {
        if let OperationEvent::Finished(result) =
            events.recv_timeout(Duration::from_secs(5)).unwrap()
        {
            assert!(result.cancelled);
            break;
        }
    }
    assert!(!dir.path().join("beta/code.rs").exists());
}

#[test]
fn task_panel_renders_active_progress_and_history() {
    let dir = fixture();
    let mut app = app(dir.path());
    app.mode = Mode::Tasks;
    app.busy = true;
    app.task = Some(TaskProgress {
        task_id: 7,
        label: "Copying files".into(),
        current: Some(dir.path().join("notes.txt")),
        completed_items: 2,
        total_items: 4,
        bytes_done: 512,
        total_bytes: 1024,
        bytes_per_second: 256,
    });
    app.task_history.push_front(OperationResult {
        task_id: 6,
        message: "Copied 2 item(s)".into(),
        error: false,
        cancelled: false,
        refresh: false,
        moved_sources: None,
        undo: None,
    });
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(rendered.contains("TASKS"));
    assert!(rendered.contains("Copying files"));
    assert!(rendered.contains("50%"));
    assert!(rendered.contains("cancel task"));
}

#[test]
fn help_pages_are_plain_language_and_page_navigation_is_bounded() {
    let config = Config::default();
    for page in 0..zuru::help::PAGES.len() {
        let lines = zuru::help::lines(&config, page, 72);
        assert!(!lines.is_empty());
        let text: String = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("preview_down"));
        assert!(lines.len() >= 4);
    }
    let dir = fixture();
    let mut app = app(dir.path());
    app.mode = Mode::Help { page: 0, scroll: 0 };
    key(&mut app, '5');
    assert!(matches!(app.mode, Mode::Help { page: 4, scroll: 0 }));
    key(&mut app, 'k');
    assert!(matches!(app.mode, Mode::Help { page: 4, scroll: 0 }));
    key(&mut app, 'j');
    assert!(matches!(app.mode, Mode::Help { page: 4, .. }));
}

#[test]
fn help_overlay_renders_title_body_and_navigation_footer() {
    let dir = fixture();
    let mut app = app(dir.path());
    app.mode = Mode::Help { page: 0, scroll: 0 };
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("ZURU HELP"));
    assert!(text.contains("GETTING AROUND"));
    assert!(text.contains("change page"));
}

#[test]
fn large_directory_renders_last_item_and_scrolls_back_home() {
    let dir = fixture();
    let mut app = app(dir.path());
    loaded(&mut app);
    let template = app.tab().entries[2].clone();
    let entries = (0..10_000)
        .map(|i| {
            let mut entry = template.clone();
            entry.name = format!("file-{i:05}.rs");
            entry.path = dir.path().join(&entry.name);
            entry
        })
        .collect();
    app.tab_mut().entries = entries;
    app.tab_mut().visible = (0..10_000).collect();
    app.tab_mut().selected = 9999;
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("file-09999.rs"));
    assert!(!text.contains("file-00000.rs"));
    app.tab_mut().selected = 0;
    terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("file-00000.rs"));
    assert!(!text.contains("file-09999.rs"));
}

fn request(path: PathBuf) -> PreviewRequest {
    PreviewRequest {
        generation: 1,
        entry: Entry::read(path).unwrap(),
        size: Size::new(32, 12),
        scroll: 0,
        zoom: false,
        hidden: false,
        force: false,
        preload: Vec::new(),
    }
}

#[test]
fn preview_routes_code_binary_directory_and_truncated_text() {
    let dir = fixture();
    let config = Config {
        preview_max_lines: 2,
        ..Config::default()
    };
    let mut previewer = Previewer::new(Picker::halfblocks(), config);
    match previewer.load(&request(dir.path().join("code.rs"))) {
        Preview::Text {
            lines,
            truncated,
            language,
        } => {
            assert_eq!(language, "Rust");
            assert_eq!(lines.len(), 2);
            assert!(truncated);
            assert!(lines[0].spans.len() > 1);
        }
        _ => panic!("Expected highlighted code"),
    }
    fs::write(dir.path().join("binary.bin"), [0, 1, 2, 3, 0xff]).unwrap();
    assert!(matches!(
        previewer.load(&request(dir.path().join("binary.bin"))),
        Preview::Metadata { .. }
    ));
    assert!(matches!(
        previewer.load(&request(dir.path().join("alpha"))),
        Preview::Directory(_)
    ));
    fs::write(dir.path().join("bad.png"), b"not an image").unwrap();
    assert!(matches!(
        previewer.load(&request(dir.path().join("bad.png"))),
        Preview::Error(_)
    ));
}

#[test]
fn audio_preview_reads_duration_and_renders_playback_guidance() {
    let dir = fixture();
    let path = dir.path().join("song.wav");
    let data_size = 16_000u32;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&8_000u32.to_le_bytes());
    wav.extend_from_slice(&16_000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());
    wav.resize(44 + data_size as usize, 0);
    fs::write(&path, wav).unwrap();
    assert_eq!(Entry::read(path.clone()).unwrap().icon(), "");
    let mut previewer = Previewer::new(Picker::halfblocks(), Config::default());
    let preview = previewer.load(&request(path));
    match &preview {
        Preview::Media {
            kind,
            details,
            artwork,
        } => {
            assert_eq!(*kind, MediaKind::Audio);
            assert!(details.contains(&("Duration".into(), "0:01".into())));
            assert!(details
                .iter()
                .any(|(label, value)| label == "Audio" && value.contains("8.0 kHz")));
            assert!(artwork.is_none());
        }
        _ => panic!("Expected audio preview"),
    }
    let mut app = app(dir.path());
    loaded(&mut app);
    app.preview = preview;
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(rendered.contains("AUDIO PREVIEW"));
    assert!(rendered.contains("Duration"));
    assert!(rendered.contains("Enter or o"));
}

#[test]
fn image_fallback_decodes_fits_and_scrolls_tall_images() {
    let dir = fixture();
    let path = dir.path().join("portrait.png");
    image::RgbImage::from_fn(120, 900, |x, y| image::Rgb([x as u8, (y % 256) as u8, 180]))
        .save(&path)
        .unwrap();
    let mut previewer = Previewer::new(Picker::halfblocks(), Config::default());
    let mut req = request(path);
    match previewer.load(&req) {
        Preview::Image {
            protocol,
            width,
            height,
            max_scroll,
            ..
        } => {
            assert_eq!((width, height), (120, 900));
            assert!(protocol.size().height <= 12);
            assert_eq!(max_scroll, 0);
        }
        _ => panic!("Expected image preview"),
    }
    req.zoom = true;
    req.scroll = usize::MAX;
    match previewer.load(&req) {
        Preview::Image {
            protocol,
            max_scroll,
            offset,
            ..
        } => {
            assert!(max_scroll > 0);
            assert_eq!(offset, max_scroll);
            assert!(protocol.size().height <= 12);
        }
        _ => panic!("Expected scrollable image"),
    }
}

#[test]
fn layout_has_three_panes_and_full_width_selection_and_survives_resize() {
    let dir = fixture();
    let mut app = app(dir.path());
    loaded(&mut app);
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
    for label in ["PARENT", "CURRENT", "DIRECTORY", "NORMAL", "1/4"] {
        assert!(text.contains(label), "Missing {label}");
    }
    let row = 3;
    let selected_bg = config::color(&app.config.theme.selection);
    let selected_cells = (0..120)
        .filter(|&x| buffer[(x, row)].bg == selected_bg)
        .count();
    assert!(
        selected_cells >= 40,
        "Selection spans only {selected_cells} cells"
    );
    for (width, height) in [(1, 1), (30, 5), (48, 8), (80, 24), (200, 50)] {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        for mode in [
            Mode::Normal,
            Mode::Tasks,
            Mode::Conflict(Conflict {
                task_id: 1,
                source: dir.path().join("notes.txt"),
                target: dir.path().join("alpha/notes.txt"),
                is_dir: false,
            }),
            Mode::Help { page: 0, scroll: 0 },
            Mode::Bookmarks { selected: 0 },
            Mode::ConfirmTrash(app.targets()),
            Mode::UndoHistory,
        ] {
            app.mode = mode;
            term.draw(|f| ui::draw(f, &mut app)).unwrap();
        }
    }
}

#[test]
fn bookmarks_persist_and_invalid_navigation_is_recoverable() {
    let dir = fixture();
    let mut app = app(dir.path());
    loaded(&mut app);
    app.command("bookmark work");
    let saved = Config::default().bookmarks(&app.config_path);
    assert_eq!(saved.get("work"), Some(&app.tab().cwd));
    app.navigate(dir.path().join("missing"));
    loaded(&mut app);
    assert!(app.tab().error.is_some());
    app.action("parent");
    loaded(&mut app);
    assert!(app.tab().error.is_none());
}

#[test]
fn trash_key_opens_confirmation_and_busy_work_prevents_exit() {
    let dir = fixture();
    let mut app = app(dir.path());
    loaded(&mut app);
    app.action("last");
    key(&mut app, 'd');
    assert!(matches!(app.mode, Mode::ConfirmTrash(_)));
    key(&mut app, 'n');
    assert!(matches!(app.mode, Mode::Normal));
    assert!(dir.path().join("notes.txt").exists());
    app.busy = true;
    app.action("quit");
    assert!(!app.should_quit);
}

#[test]
#[ignore = "Exercises the real OS trash with disposable fixtures"]
fn os_trash_and_move_work_with_disposable_files() {
    let dir = fixture();
    let source = dir.path().join("notes.txt");
    operations::execute(Operation::Paste {
        sources: vec![source.clone()],
        destination: dir.path().join("alpha"),
        cut: true,
    })
    .unwrap();
    assert!(!source.exists());
    let moved = dir.path().join("alpha/notes.txt");
    assert_eq!(fs::read_to_string(&moved).unwrap(), "plain text\n");
    let (controller, events) = operations::spawn_worker();
    controller
        .submit(Operation::Trash(vec![moved.clone()]))
        .unwrap();
    let undo = loop {
        if let OperationEvent::Finished(result) =
            events.recv_timeout(Duration::from_secs(5)).unwrap()
        {
            assert!(!result.error);
            break result
                .undo
                .expect("trash should produce an OS restore record");
        }
    };
    assert!(!moved.exists());
    controller.submit(Operation::Undo(undo)).unwrap();
    loop {
        if let OperationEvent::Finished(result) =
            events.recv_timeout(Duration::from_secs(5)).unwrap()
        {
            assert!(!result.error, "{}", result.message);
            break;
        }
    }
    assert_eq!(fs::read_to_string(&moved).unwrap(), "plain text\n");

    let replacement = dir.path().join("replacement.txt");
    let existing = dir.path().join("alpha/replacement.txt");
    fs::write(&replacement, "new").unwrap();
    fs::write(&existing, "old").unwrap();
    let task_id = controller
        .submit(Operation::Paste {
            sources: vec![replacement],
            destination: dir.path().join("alpha"),
            cut: false,
        })
        .unwrap();
    loop {
        match events.recv_timeout(Duration::from_secs(5)).unwrap() {
            OperationEvent::Conflict(_) => controller
                .resolve(task_id, ConflictChoice::Replace)
                .unwrap(),
            OperationEvent::Finished(result) => {
                assert!(!result.error);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(fs::read_to_string(existing).unwrap(), "new");

    let app_trash = dir.path().join("app-trash-folder ");
    #[cfg(windows)]
    let app_trash = PathBuf::from(format!(r"\\?\{}", app_trash.display()));
    fs::create_dir(&app_trash).unwrap();
    fs::write(app_trash.join("inside.txt"), "trash through the d key").unwrap();
    let mut app = app(dir.path());
    loaded(&mut app);
    let entry_index = app
        .tab()
        .entries
        .iter()
        .position(|entry| entry.name == "app-trash-folder ")
        .unwrap();
    app.tab_mut().selected = app
        .tab()
        .visible
        .iter()
        .position(|index| *index == entry_index)
        .unwrap();
    key(&mut app, 'd');
    assert!(matches!(app.mode, Mode::ConfirmTrash(_)));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    wait_until(&mut app, |app| !app.busy);
    assert!(
        !app_trash.exists(),
        "{}",
        app.message.as_ref().unwrap().text
    );
    assert_eq!(app.undo_history.len(), 1);
    app.action("undo");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    wait_until(&mut app, |app| !app.busy);
    assert!(app_trash.exists());
    assert_eq!(
        fs::read_to_string(app_trash.join("inside.txt")).unwrap(),
        "trash through the d key"
    );
    fs::remove_dir_all(&app_trash).unwrap();
}
