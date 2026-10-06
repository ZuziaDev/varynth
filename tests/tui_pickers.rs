//! Picker navigation bounds, fuzzy filtering and effort level order.

use varynth::composer::{PickItem, Picker, PickerKind};
use varynth::runtime::Runtime;

fn picker(n: usize) -> Picker {
    Picker {
        kind: PickerKind::Models,
        items: (0..n)
            .map(|i| PickItem {
                id: format!("m{i}"),
                label: format!("model {i}"),
                detail: String::new(),
            })
            .collect(),
        sel: 0,
        query: String::new(),
    }
}

#[test]
fn empty_picker_stays_at_zero_and_selects_nothing() {
    let mut p = picker(0);
    p.up();
    assert_eq!(p.sel, 0);
    p.down();
    assert_eq!(p.sel, 0);
    assert!(p.selected().is_none());
}

#[test]
fn up_stops_at_the_first_item() {
    let mut p = picker(3);
    p.up();
    p.up();
    assert_eq!(p.sel, 0);
    assert_eq!(p.selected().unwrap().id, "m0");
}

#[test]
fn down_stops_at_the_last_item() {
    let mut p = picker(3);
    for _ in 0..10 {
        p.down();
    }
    assert_eq!(p.sel, 2);
    assert_eq!(p.selected().unwrap().id, "m2");
    p.up();
    assert_eq!(p.selected().unwrap().id, "m1");
}

#[test]
fn single_item_picker_does_not_move() {
    let mut p = picker(1);
    p.down();
    assert_eq!(p.sel, 0);
    p.up();
    assert_eq!(p.sel, 0);
    assert_eq!(p.selected().unwrap().id, "m0");
}

#[test]
fn effort_levels_are_ordered_low_to_ultra() {
    assert_eq!(
        Runtime::EFFORT_LEVELS,
        ["low", "medium", "high", "xhigh", "max", "ultra"]
    );
}

#[test]
fn query_narrows_the_list_and_backspace_restores_it() {
    let mut p = picker(4);
    p.type_char('2');
    assert_eq!(p.visible().len(), 1, "'2' matches only 'model 2'");
    assert_eq!(p.selected().unwrap().id, "m2");
    p.down();
    assert_eq!(p.sel, 0, "selection stays within the filtered list");
    p.backspace();
    assert_eq!(p.visible().len(), 4);
    assert_eq!(p.selected().unwrap().id, "m0");
}

#[test]
fn up_and_down_respect_the_filtered_list_bounds() {
    let mut p = picker(5);
    p.type_char('4');
    for _ in 0..10 {
        p.down();
    }
    assert_eq!(p.sel, 0, "one match: nowhere to move");
    p.query.clear();
    p.sel = 0;
    for _ in 0..10 {
        p.down();
    }
    assert_eq!(p.sel, 4, "full list bounds restore after clearing");
}

#[test]
fn pin_content_is_capped_and_sent_instead_of_only_a_path() {
    use varynth::composer::{read_jail_pin, scan_jail_files, Composer, PIN_MAX_CHARS};
    use varynth::sandbox::{Jail, SandboxMode};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/one.rs"), "fn test() {}\n".repeat(900)).unwrap();
    let jail = Jail::new(
        dir.path().into(),
        Vec::new(),
        SandboxMode::WorkspaceWrite,
        Vec::new(),
    );
    let candidates = scan_jail_files(&jail);
    assert!(candidates.iter().any(|path| path == "src/one.rs"));
    assert!(candidates.iter().any(|path| path == "src/"));
    let content = read_jail_pin(&jail, "src/one.rs").unwrap();
    assert_eq!(content.chars().count(), PIN_MAX_CHARS);
    let mut composer = Composer::default();
    composer.insert_str("#src/one.rs");
    composer.accept_file_pin("src/one.rs", &content);
    let message = composer.submit().0;
    assert!(message.contains("[#pin src/one.rs]"));
    assert!(message.contains("fn test() {}"));
    assert_eq!(read_jail_pin(&jail, "src/").unwrap(), "src/one.rs");
}

#[test]
fn pins_reject_outside_paths_even_with_full_access() {
    use varynth::composer::read_jail_pin;
    use varynth::sandbox::{Jail, SandboxMode};
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("secret.txt");
    std::fs::write(&path, "not a pin").unwrap();
    let jail = Jail::new(
        workspace.path().into(),
        Vec::new(),
        SandboxMode::DangerFullAccess,
        Vec::new(),
    );
    assert!(read_jail_pin(&jail, &path.display().to_string()).is_err());
}

#[test]
fn paste_edits_are_preserved_verbatim_when_submitted() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use varynth::composer::{Composer, TextEditor};
    let mut composer = Composer::default();
    composer.paste("a\nb\nc\nd\ne\nf");
    let mut editor = TextEditor::new(composer.pastes()[0].text.clone());
    editor.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    editor.insert(" changed");
    editor.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    editor.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    editor.insert("new line");
    let edited = editor.text.clone();
    assert!(composer.replace_paste(0, editor.text));
    let message = composer.submit().0;
    assert!(message.contains(&edited));
    assert!(message.contains("changed"));
}

#[test]
fn prefix_no_match_leaves_typing_and_submit_available() {
    use crossterm::event::KeyCode;
    use varynth::composer::Composer;
    let mut composer = Composer::default();
    composer.insert_str("#missing");
    assert!(!composer.prefix_key(KeyCode::Enter));
    composer.insert_str(" more text");
    assert_eq!(composer.submit().0, "#missing more text");
}

#[test]
fn model_and_prefix_render_at_small_viewports() {
    use ratatui::{backend::TestBackend, Terminal};
    use varynth::composer::{render_picker, render_prefix, Composer};
    for (width, height) in [(24, 8), (80, 24)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut composer = Composer::default();
        composer.insert_str("$does-not-exist");
        terminal
            .draw(|frame| render_prefix(frame, frame.area(), &composer))
            .unwrap();
        let text = terminal.backend().to_string();
        assert!(text.contains("no match"));
        let p = picker(3);
        terminal
            .draw(|frame| render_picker(frame, frame.area(), &p))
            .unwrap();
        assert!(terminal.backend().to_string().contains("model"));
    }
}

#[test]
fn composer_clear_removes_cards_queue_and_draft() {
    use varynth::composer::Composer;
    let mut composer = Composer::default();
    composer.insert_str("queued");
    composer.enqueue();
    composer.paste("a\nb\nc\nd\ne\nf");
    composer.insert_str("draft");
    composer.clear();
    assert!(composer.is_empty());
    assert_eq!(composer.queued(), 0);
    assert!(!composer.history_prev());
}
