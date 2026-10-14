//! Original TUI-1.5: real importer/native and cached sessions, shared source
//! preparation, selected renderer and injected events. No provider/credentials.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use editac_cli::{
    engine::{Utf16Offset, anchors::BoundaryError, document::DocumentIdentity},
    target::ResolveOptions,
    tui::{
        input::{InputError, InputState, MAX_PASTE_BYTES, SourceInput},
        positions::{SourceMap, byte_to_offset, offset_to_byte},
        session::{open_cached, open_local},
        terminal::{Mode, TerminalIo, TerminalOwner},
        view::{self, View},
    },
};
use editac_session::{
    edit_service::HostPolicy,
    locator::{Catalog, Locator},
    session::CachedPermission,
    store::NativeStore,
};
use ratatui::{Terminal, backend::TestBackend};
use std::{
    collections::VecDeque,
    fs, io,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    identity: DocumentIdentity,
}
impl Fixture {
    fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("editac-tui-input-{}-{n}", std::process::id()));
        fs::create_dir(&root).unwrap();
        Self {
            root,
            identity: DocumentIdentity::v5(
                format!("ac000000-0000-5000-8000-{n:012x}"),
                format!("ac000000-0000-4000-8000-{n:012x}"),
            )
            .unwrap(),
        }
    }
    fn local(
        &self,
        source: &str,
        literal: bool,
    ) -> editac_cli::tui::session::TerminalSession<NativeStore> {
        let path = self
            .root
            .join(if literal { "source.txt" } else { "source.md" });
        fs::write(&path, source).unwrap();
        open_local(
            &path,
            ResolveOptions::default(),
            &self.root.join("session.sqlite"),
            self.identity.clone(),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}
fn shifted(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::SHIFT))
}
fn apply(input: &mut InputState, source: &mut String, code: KeyCode) {
    input.handle(source, &key(code)).unwrap();
}
fn state(source: &str) -> InputState {
    InputState::new(true, source.len())
}

macro_rules! grapheme_case {
    ($name:ident, $source:expr, $units:expr) => {
        #[test]
        fn $name() {
            let f = Fixture::new();
            let session = f.local($source, true);
            let opening = session.service.session().document().checkpoint().bytes;
            let binding = SourceInput::capture(&session, 0).unwrap();
            let mut source: String = session.source.clone();
            let mut input = state(&source);
            apply(&mut input, &mut source, KeyCode::Right);
            assert_eq!(input.caret, Utf16Offset($units));
            assert_eq!(input.anchor, input.caret);
            apply(&mut input, &mut source, KeyCode::Backspace);
            assert_eq!(source, "x");
            assert_eq!(input.caret, Utf16Offset(0));
            let prepared = binding.prepare(&session, &source, 0).unwrap();
            assert_ne!(
                &prepared.preview().visible,
                &session.service.read(0).unwrap().snapshot.visible
            );
            assert_eq!(
                session.service.session().document().checkpoint().bytes,
                opening
            );
            assert_eq!(
                session
                    .service
                    .session()
                    .document()
                    .history_state()
                    .undo_groups,
                0
            );
            assert_eq!(binding.baseline(), $source);
        }
    };
}
grapheme_case!(emoji_surrogate_pair_is_one_caret_stop, "😀x", 2);
grapheme_case!(emoji_zwj_is_one_caret_stop, "👨‍👩‍👧‍👦x", 11);
grapheme_case!(combining_sequence_is_one_caret_stop, "e\u{301}x", 2);
grapheme_case!(wide_character_is_one_caret_stop, "界x", 1);
grapheme_case!(tab_is_one_source_unit, "\tx", 1);
grapheme_case!(crlf_is_one_caret_stop, "\r\nx", 2);
grapheme_case!(bidi_source_is_logical, "אx", 1);

#[test]
fn screen_mapping_separates_bytes_utf16_and_columns() {
    let source = "😀e\u{301}界\tZ\r\nא\u{202e}ב";
    let map = SourceMap::new(source);
    for (offset, row, column) in [
        (0, 0, 0),
        (2, 0, 2),
        (4, 0, 3),
        (5, 0, 5),
        (6, 0, 8),
        (7, 0, 9),
        (9, 1, 0),
        (10, 1, 1),
        (11, 1, 9),
        (12, 1, 10),
    ] {
        assert_eq!(map.locate(Utf16Offset(offset)).unwrap(), (row, column));
        let byte = offset_to_byte(source, Utf16Offset(offset)).unwrap();
        assert_eq!(byte_to_offset(source, byte).unwrap(), Utf16Offset(offset));
    }
    assert_eq!(map.at_column(0, 4), Utf16Offset(4));
    assert_eq!(map.at_column(0, 7), Utf16Offset(5));
    assert!(map.locate(Utf16Offset(3)).is_err());
    assert!(map.locate(Utf16Offset(8)).is_err());
    assert_eq!(SourceMap::new("👨‍👩‍👧‍👦").lines[0].width, 32);
    assert_eq!(SourceMap::new("\u{301}").lines[0].width, 1);
}
#[test]
fn malformed_positions_reject_without_changing_selection() {
    let source = "😀e\u{301}";
    let mut input = state(source);
    assert_eq!(
        offset_to_byte(source, Utf16Offset(1)),
        Err(BoundaryError::SurrogateInterior)
    );
    assert_eq!(
        byte_to_offset(source, 1),
        Err(BoundaryError::InvalidByteBoundary)
    );
    assert!(
        input
            .select(source, Utf16Offset(0), Utf16Offset(3))
            .is_err()
    );
    assert_eq!(input.caret, Utf16Offset(0));
    assert_eq!(input.anchor, Utf16Offset(0));
}
#[test]
fn insertion_snaps_past_new_combining_and_joined_cluster() {
    let mut source: String = "ex".to_owned();
    let mut input = state(&source);
    apply(&mut input, &mut source, KeyCode::Right);
    apply(&mut input, &mut source, KeyCode::Char('\u{301}'));
    assert_eq!(source, "e\u{301}x");
    assert_eq!(input.caret, Utf16Offset(2));
    source = "👨👩x".into();
    input = state(&source);
    apply(&mut input, &mut source, KeyCode::Right);
    input
        .paste_bytes(&mut source, "\u{200d}".as_bytes())
        .unwrap();
    assert_eq!(source, "👨‍👩x");
    assert_eq!(input.caret, Utf16Offset(5));
}
#[test]
fn vertical_movement_keeps_preferred_column_and_home_end() {
    let mut source: String = "a\t界z\nx\na\t界z".into();
    let mut input = state(&source);
    input
        .select(&source, Utf16Offset(3), Utf16Offset(3))
        .unwrap();
    apply(&mut input, &mut source, KeyCode::Down);
    assert_eq!(input.caret, Utf16Offset(6));
    apply(&mut input, &mut source, KeyCode::Down);
    assert_eq!(input.caret, Utf16Offset(10));
    apply(&mut input, &mut source, KeyCode::Home);
    assert_eq!(input.caret, Utf16Offset(7));
    apply(&mut input, &mut source, KeyCode::End);
    assert_eq!(input.caret, Utf16Offset(11));
    apply(&mut input, &mut source, KeyCode::Down);
    assert_eq!(input.caret, Utf16Offset(11));
}
#[test]
fn selection_direction_collapse_replace_and_delete() {
    let mut source: String = "a😀b\r\nc".into();
    let mut input = state(&source);
    apply(&mut input, &mut source, KeyCode::End);
    input.handle(&mut source, &shifted(KeyCode::Left)).unwrap();
    input.handle(&mut source, &shifted(KeyCode::Left)).unwrap();
    assert_eq!(input.anchor, Utf16Offset(4));
    assert_eq!(input.caret, Utf16Offset(1));
    assert_eq!(input.selection(), Utf16Offset(1)..Utf16Offset(4));
    apply(&mut input, &mut source, KeyCode::Right);
    assert_eq!(input.caret, Utf16Offset(4));
    apply(&mut input, &mut source, KeyCode::Delete);
    assert_eq!(source, "a😀bc");
    input
        .select(&source, Utf16Offset(1), Utf16Offset(4))
        .unwrap();
    apply(&mut input, &mut source, KeyCode::Char('X'));
    assert_eq!(source, "aXc");
    input
        .handle(
            &mut source,
            &Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        )
        .unwrap();
    assert_eq!(input.selection(), Utf16Offset(0)..Utf16Offset(3));
    apply(&mut input, &mut source, KeyCode::Backspace);
    assert_eq!(source, "");
}
#[test]
fn multiline_insert_preserves_crlf_and_tabs() {
    let mut source: String = "a\r\nb".into();
    let mut input = state(&source);
    apply(&mut input, &mut source, KeyCode::Right);
    apply(&mut input, &mut source, KeyCode::Enter);
    apply(&mut input, &mut source, KeyCode::Tab);
    assert_eq!(source, "a\r\n\t\r\nb");
    assert_eq!(input.caret, Utf16Offset(4));
    let mut empty = String::new();
    let mut input = state(&empty);
    apply(&mut input, &mut empty, KeyCode::Enter);
    assert_eq!(empty, "\n");
}
#[test]
fn paste_is_literal_in_text_and_markdown_and_preparation_is_inert() {
    for literal in [true, false] {
        let f = Fixture::new();
        let session = f.local("plain\n", literal);
        let binding = SourceInput::capture(&session, 0).unwrap();
        let before = session.service.session().document().checkpoint().bytes;
        let mut source: String = session.source.clone();
        let mut input = state(&source);
        input
            .handle(&mut source, &Event::Paste("# *literal*\r\n\t😀".into()))
            .unwrap();
        assert_eq!(source, "# *literal*\r\n\t😀plain\n");
        let result = binding.prepare(&session, &source, 0);
        if literal {
            let prepared = result.unwrap();
            let text = prepared
                .preview()
                .visible
                .iter()
                .flat_map(|n| n.runs.iter())
                .map(|r| {
                    assert!(r.attributes.is_empty());
                    r.text.as_str()
                })
                .collect::<String>();
            assert_eq!(text, source);
        } else {
            // The qualified source mapper rejects a structural heading/paragraph
            // rewrite rather than guessing identities. The literal draft remains
            // intact; a supported in-paragraph paste still prepares successfully.
            assert!(
                matches!(result, Err(editac_session::edit_service::ServiceError::SourceConflict { conflict, .. })
                if conflict.kind == editac_cli::markdown::reconcile::ConflictKind::AmbiguousMapping)
            );
            assert_eq!(source, "# *literal*\r\n\t😀plain\n");
            source = session.source.clone();
            input = state(&source);
            input
                .handle(&mut source, &Event::Paste("literal 😀 ".into()))
                .unwrap();
            binding.prepare(&session, &source, 0).unwrap();
            assert_eq!(source, "literal 😀 plain\n");
        }
        assert_eq!(
            session.service.session().document().checkpoint().bytes,
            before
        );
        assert_eq!(
            fs::read_to_string(
                f.root
                    .join(if literal { "source.txt" } else { "source.md" })
            )
            .unwrap(),
            "plain\n"
        );
    }
}
#[test]
fn paste_size_and_utf8_limits_are_atomic_and_exact() {
    let mut source: String = "ab".into();
    let mut input = state(&source);
    input
        .select(&source, Utf16Offset(0), Utf16Offset(1))
        .unwrap();
    assert_eq!(
        input.paste_bytes(&mut source, &[0xff]),
        Err(InputError::InvalidUtf8)
    );
    assert_eq!(
        input.paste_bytes(&mut source, &vec![b'x'; MAX_PASTE_BYTES + 1]),
        Err(InputError::TooLarge)
    );
    assert_eq!(source, "ab");
    assert_eq!(input.selection(), Utf16Offset(0)..Utf16Offset(1));
    input
        .paste_bytes(&mut source, &vec![b'x'; MAX_PASTE_BYTES])
        .unwrap();
    assert_eq!(source.len(), MAX_PASTE_BYTES + 1);
    input.max_source_bytes = source.len();
    let before = source.clone();
    assert_eq!(
        input.paste_bytes(&mut source, b"x"),
        Err(InputError::TooLarge)
    );
    assert_eq!(source, before);
}
#[test]
fn readonly_blocks_mutation_but_allows_selection() {
    let mut source: String = "a😀b".into();
    let mut input = InputState::new(false, source.len());
    for event in [
        key(KeyCode::Char('x')),
        key(KeyCode::Enter),
        key(KeyCode::Tab),
        key(KeyCode::Delete),
        key(KeyCode::Backspace),
        Event::Paste("x".into()),
    ] {
        assert_eq!(input.handle(&mut source, &event), Err(InputError::ReadOnly));
        assert_eq!(source, "a😀b");
        assert_eq!(input.caret, Utf16Offset(0));
    }
    input.handle(&mut source, &shifted(KeyCode::Right)).unwrap();
    assert_eq!(input.selection(), Utf16Offset(0)..Utf16Offset(1));
}
#[test]
fn key_release_and_modified_shortcuts_do_not_insert() {
    let mut source: String = "a".into();
    let mut input = state(&source);
    let mut release = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    for event in [
        Event::Key(release),
        Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
    ] {
        assert!(!input.handle(&mut source, &event).unwrap());
        assert_eq!(source, "a");
    }
    let mut repeat = KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT);
    repeat.kind = KeyEventKind::Repeat;
    input.handle(&mut source, &Event::Key(repeat)).unwrap();
    assert_eq!(source, "Xa");
}
#[test]
fn cached_hosted_projection_uses_same_input_and_inert_preparation() {
    use editac_session::{
        locator::{AccessState, CatalogEntry, SourceBinding},
        ports::StoreLimits,
    };
    let f = Fixture::new();
    let local = f.local("# Cached\n\nhello 😀\n", false);
    let identity = local.service.session().document().identity().clone();
    drop(local);
    let locator = Locator::parse(&format!("ac://{}", identity.document_id)).unwrap();
    let mut catalog = Catalog::default();
    catalog
        .insert(
            locator.clone(),
            CatalogEntry {
                identity: identity.clone(),
                source: SourceBinding::Cached,
                access: AccessState::Allowed,
            },
        )
        .unwrap();
    let store = NativeStore::open_existing(
        f.root.join("session.sqlite"),
        identity,
        StoreLimits::default(),
    )
    .unwrap();
    let session = open_cached(
        store,
        &catalog,
        &locator,
        CachedPermission {
            writable: true,
            expires_at: None,
        },
        HostPolicy::default(),
    )
    .unwrap();
    let binding = SourceInput::capture(&session, 0).unwrap();
    let opening = session.service.read(0).unwrap().revision;
    let bytes = session.service.session().document().checkpoint().bytes;
    let mut source: String = session.source.clone();
    let mut input = state(&source);
    input
        .handle(&mut source, &Event::Paste("draft 😀 ".into()))
        .unwrap();
    binding.prepare(&session, &source, 0).unwrap();
    assert!(session.service.observe(&opening, 0).unwrap().is_none());
    assert_eq!(
        session.service.session().document().checkpoint().bytes,
        bytes
    );
    assert!(
        View::from_session(&session, 0)
            .status
            .contains("hosted projection")
    );
}
struct Backend {
    terminal: Terminal<TestBackend>,
    events: VecDeque<Event>,
    modes: Vec<Mode>,
    draws: usize,
}
impl TerminalIo for Backend {
    fn raw_enabled(&mut self) -> io::Result<bool> {
        Ok(false)
    }
    fn mode(&mut self, mode: Mode) -> io::Result<()> {
        self.modes.push(mode);
        Ok(())
    }
    fn size(&mut self) -> io::Result<(u16, u16)> {
        Ok((24, 4))
    }
    fn initialize(&mut self, _: (u16, u16)) -> io::Result<()> {
        Ok(())
    }
    fn resize(&mut self, size: (u16, u16)) -> io::Result<()> {
        self.terminal.backend_mut().resize(size.0, size.1);
        self.terminal.autoresize().unwrap();
        Ok(())
    }
    fn draw(&mut self, view: &View) -> io::Result<()> {
        self.terminal
            .draw(|frame| view::render(frame, view))
            .unwrap();
        self.draws += 1;
        Ok(())
    }
    fn read(&mut self, _: Duration) -> io::Result<Option<Event>> {
        Ok(Some(
            self.events.pop_front().expect("bounded fixture must quit"),
        ))
    }
}
#[test]
fn terminal_owner_wires_focus_selection_paste_resize_and_source_preparation() {
    let f = Fixture::new();
    let session = f.local("a😀b\r\nc", true);
    let binding = SourceInput::capture(&session, 0).unwrap();
    let revision = session.service.read(0).unwrap().revision;
    let mut view = View::from_session(&session, 0);
    let events = vec![
        Event::FocusLost,
        key(KeyCode::Char('!')),
        Event::Paste("ignored".into()),
        Event::FocusGained,
        key(KeyCode::Right),
        shifted(KeyCode::Right),
        Event::Paste("界\t".into()),
        Event::Resize(24, 4),
        key(KeyCode::Esc),
    ];
    let backend = Backend {
        terminal: Terminal::new(TestBackend::new(24, 4)).unwrap(),
        events: events.into(),
        modes: vec![],
        draws: 0,
    };
    let mut owner = TerminalOwner::new(backend);
    owner.run(&mut view).unwrap();
    assert_eq!(view.source, "a界\tb\r\nc");
    assert!(view.draft_changed);
    assert_eq!(view.input.as_ref().unwrap().caret, Utf16Offset(3));
    assert!(owner.backend().draws >= 6);
    assert_eq!(owner.backend().modes.last(), Some(&Mode::Raw(false)));
    binding.prepare(&session, &view.source, 0).unwrap();
    assert!(session.service.observe(&revision, 0).unwrap().is_none());
}
#[test]
fn selection_is_highlighted_and_caret_scrolls_without_clipping_wide_clusters() {
    use ratatui::style::Color;
    let f = Fixture::new();
    let session = f.local("a界\tZ\r\nsecond\nthird", true);
    let mut view = View::from_session(&session, 0);
    view.input
        .as_mut()
        .unwrap()
        .select(&view.source, Utf16Offset(1), Utf16Offset(2))
        .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(10, 4)).unwrap();
    terminal.draw(|f| view::render(f, &view)).unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(1, 0)].symbol(), "界");
    assert_eq!(buffer[(1, 0)].bg, Color::Blue);
    assert_eq!(
        terminal.get_cursor_position().unwrap(),
        ratatui::layout::Position::new(3, 0)
    );
    let end = byte_to_offset(&view.source, view.source.len()).unwrap();
    view.input
        .as_mut()
        .unwrap()
        .select(&view.source, end, end)
        .unwrap();
    terminal.draw(|f| view::render(f, &view)).unwrap();
    assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "s");
    assert_eq!(
        terminal.get_cursor_position().unwrap(),
        ratatui::layout::Position::new(5, 1)
    );
    terminal.backend_mut().resize(3, 3);
    terminal.autoresize().unwrap();
    terminal.draw(|f| view::render(f, &view)).unwrap();
    assert_eq!(
        terminal.get_cursor_position().unwrap(),
        ratatui::layout::Position::new(2, 0)
    );
}
#[test]
fn pasted_controls_are_preserved_in_source_and_escaped_in_frames() {
    let f = Fixture::new();
    let session = f.local("", true);
    let mut view = View::from_session(&session, 0);
    let malicious = "\x1b]52;c;secret\x07\u{202e}א\u{2066}";
    view.input
        .as_mut()
        .unwrap()
        .paste_bytes(&mut view.source, malicious.as_bytes())
        .unwrap();
    assert_eq!(view.source, malicious);
    view.input
        .as_mut()
        .unwrap()
        .select(&view.source, Utf16Offset(0), Utf16Offset(0))
        .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(80, 4)).unwrap();
    terminal.draw(|f| view::render(f, &view)).unwrap();
    let frame = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(frame.contains("\\x1b]52;c;secret\\x07\\u{202e}א\\u{2066}"));
    assert!(!frame.chars().any(char::is_control));
}
