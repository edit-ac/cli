//! Original TUI-1.7: real source admission, shared diff, native store and event/view loop.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use editac_cli::{
    engine::{
        Utf16Offset,
        commands::{editor::EditorCommand, text::TextOrigin},
        document::{CreationPermit, DocumentIdentity},
    },
    markdown::{optional::Codecs, reconcile::SourceMode},
    reconcile::LocalBaseline,
    target::{self, ResolveOptions},
    tui::{
        commands::{Command, CommandError, Commands},
        diff::{self, Review, ReviewError},
        session::{Representation, TerminalSession},
        terminal::{Mode, TerminalIo, TerminalOwner, dispatch_command},
        view::{self, View},
    },
};
use editac_diff::{
    portable::ChangeInput,
    text::{PatchLimits, TextPatch},
};
use editac_session::{
    edit_service::{EditService, HostPolicy},
    ports::*,
    session::{CachedPermission, Session},
    store::NativeStore,
};
use std::{
    collections::VecDeque,
    fs, io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    id: DocumentIdentity,
}
impl Fixture {
    fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("editac-tui-diff-{}-{n}", std::process::id()));
        fs::create_dir(&root).unwrap();
        Self {
            root,
            id: DocumentIdentity::v5(
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
    ) -> (TerminalSession<Arc<NativeStore>>, Arc<NativeStore>) {
        self.local_fixture(source, literal, false)
    }
    fn local_fixture(
        &self,
        source: &str,
        literal: bool,
        opaque: bool,
    ) -> (TerminalSession<Arc<NativeStore>>, Arc<NativeStore>) {
        let path = self
            .root
            .join(if literal { "source.txt" } else { "source.md" });
        fs::write(&path, source).unwrap();
        let target = target::resolve(&path, ResolveOptions::default()).unwrap();
        let (doc, baseline) = LocalBaseline::import(
            target,
            CreationPermit::authorize_new(self.id.clone()).unwrap(),
        )
        .unwrap();
        let doc = if opaque {
            use yrs::{Any, Doc, Map, ReadTxn, StateVector, Transact, updates::decoder::Decode};
            let mut checkpoint = doc.checkpoint();
            let owner = doc
                .snapshot()
                .unwrap()
                .visible
                .iter()
                .find(|n| n.kind == "block")
                .unwrap()
                .id
                .clone();
            let raw = Doc::new();
            raw.transact_mut()
                .apply_update(yrs::Update::decode_v1(&checkpoint.bytes).unwrap())
                .unwrap();
            raw.get_or_insert_map("registry-block-attributes").insert(
                &mut raw.transact_mut(),
                owner,
                Any::Array(
                    vec![
                        Any::from("vendor:unknown"),
                        Any::from(1u32),
                        Any::from("exact opaque payload"),
                    ]
                    .into(),
                ),
            );
            checkpoint.bytes = raw
                .transact()
                .encode_state_as_update_v1(&StateVector::default());
            editac_cli::engine::document::Document::restore(&checkpoint.identity, &checkpoint, &[])
                .unwrap()
        } else {
            doc
        };
        let store = Arc::new(
            NativeStore::create_new(
                self.root.join("session.sqlite"),
                self.id.clone(),
                StoreLimits::default(),
            )
            .unwrap(),
        );
        store
            .publish(0, CheckpointRecord::new(doc.checkpoint(), vec![]).unwrap())
            .unwrap();
        let session = Session::open(store.clone(), &self.id, CachedPermission::LOCAL).unwrap();
        (
            TerminalSession {
                service: EditService::new(session, HostPolicy::default()),
                source: source.into(),
                mode: if literal {
                    SourceMode::Literal
                } else {
                    SourceMode::Markdown(Codecs::default())
                },
                representation: Representation::ExactSource,
                local_baseline: Some(baseline),
            },
            store,
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
}
fn plain(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}
fn route(
    session: &mut TerminalSession<Arc<NativeStore>>,
    commands: Option<&mut Commands>,
    view: &mut View,
    event: &Event,
    now: u64,
    f: &Fixture,
) {
    if !dispatch_command(event, view, session, commands, now, &f.root) {
        if let Some(input) = &mut view.input {
            let _ = input.handle(&mut view.source, event);
        }
    }
}
fn patch(review: &Review) -> TextPatch {
    let wire = match &review.model().input {
        ChangeInput::ExactSource { wire } | ChangeInput::ProjectedMarkdown { wire, .. } => wire,
        _ => panic!("expected text review"),
    };
    TextPatch::decode(wire.as_bytes(), PatchLimits::default()).unwrap()
}
#[test]
fn exact_review_preserves_whitespace_unicode_crlf_and_missing_newline() {
    let f = Fixture::new();
    let before = "context\r\nwide 界 e\u{301} 👩‍💻\r\nlast\t";
    let (session, _) = f.local(before, true);
    let commands = Commands::capture(&session, 0).unwrap();
    let after = "context\r\nwide 界 e\u{301} 🙂\r\nlast\t ";
    let review = commands.review(&session, after, 0).unwrap();
    assert!(matches!(
        review.model().input,
        ChangeInput::ExactSource { .. }
    ));
    assert_eq!(
        patch(&review)
            .apply(before.as_bytes(), PatchLimits::default())
            .unwrap(),
        after.as_bytes()
    );
    let rows = review.page(100).collect::<Vec<_>>().join("\n");
    assert!(rows.contains("-wide 界"));
    assert!(rows.contains("+wide 界"));
    assert!(rows.contains(" context\r"));
    assert!(rows.contains("No newline"));
    assert!(review.label.contains("Exact source"));
    assert!(review.base_label.contains("base revision"));
    assert!(review.proposed_label.contains("uncommitted"));
    assert_eq!(session.source, before);
}
#[test]
fn hosted_review_discloses_serializer_dialect_and_normalization() {
    let f = Fixture::new();
    let (mut session, _) = f.local("before\n", false);
    session.local_baseline = None;
    session.representation = Representation::HostedProjection;
    let commands = Commands::capture(&session, 0).unwrap();
    let review = commands.review(&session, "after\n", 0).unwrap();
    let ChangeInput::ProjectedMarkdown {
        serializer,
        dialect,
        ..
    } = &review.model().input
    else {
        panic!("must label projection")
    };
    assert_eq!(serializer, "editac-markdown-export-v1");
    assert_eq!(dialect, "edit-ac-gfm-v1");
    assert!(review.label.contains("normalize"));
    assert!(!review.label.contains("Exact source"));
    assert_eq!(
        patch(&review)
            .apply(b"before\n", PatchLimits::default())
            .unwrap(),
        b"after\n"
    );
}
#[test]
fn equal_review_is_explicit_and_inert() {
    let f = Fixture::new();
    let (session, store) = f.local("same", true);
    let review = Review::from_session(&session, "same", 0).unwrap();
    assert_eq!(review.page(10).collect::<Vec<_>>(), ["No source changes"]);
    assert!(store.pending(&f.id).unwrap().is_empty());
}
#[test]
fn review_limits_reject_before_mutation_and_export_is_bounded() {
    let f = Fixture::new();
    let (session, store) = f.local("base", true);
    let big = "x".repeat(1_048_577);
    assert_eq!(
        Review::from_session(&session, &big, 0).unwrap_err(),
        ReviewError::Limit
    );
    assert_eq!(
        diff::export_bytes(&session, &big, 0).unwrap_err(),
        ReviewError::Limit
    );
    assert!(store.pending(&f.id).unwrap().is_empty());
}
#[test]
fn changed_candidate_cannot_apply_a_review() {
    let f = Fixture::new();
    let (mut session, store) = f.local("base", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let review = commands.review(&session, "reviewed", 0).unwrap();
    let mut draft = "unreviewed".into();
    assert!(matches!(
        commands.apply_review(&review, &mut session, &mut draft, 0, &f.root),
        Err(CommandError::Review(ReviewError::Stale))
    ));
    assert_eq!(draft, "unreviewed");
    assert_eq!(session.source, "base");
    assert!(store.pending(&f.id).unwrap().is_empty());
}
fn advance(session: &mut TerminalSession<Arc<NativeStore>>, text: &str) {
    let read = session.service.read(0).unwrap();
    let owner = read
        .snapshot
        .visible
        .iter()
        .find(|n| n.kind == "block")
        .unwrap()
        .id
        .clone();
    let len = read
        .snapshot
        .visible
        .iter()
        .find(|n| n.id == owner)
        .unwrap()
        .runs
        .iter()
        .map(|r| r.text.encode_utf16().count())
        .sum::<usize>();
    let prepared = session
        .service
        .prepare_commands(
            &read.revision,
            &[EditorCommand::ReplaceAt {
                owner,
                from: Utf16Offset(0),
                to: Utf16Offset(len as u32),
                text: text.into(),
            }],
            &TextOrigin::new("tui:test").unwrap(),
            0,
        )
        .unwrap();
    session.service.apply(prepared, 0).unwrap();
}
#[test]
fn deletion_only_remote_revision_invalidates_review_without_losing_draft() {
    let f = Fixture::new();
    let (mut session, store) = f.local("base", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let review = commands.review(&session, "draft", 0).unwrap();
    let mut peer = EditService::new(
        Session::open(store.clone(), &f.id, CachedPermission::LOCAL).unwrap(),
        HostPolicy::default(),
    );
    let read = peer.read(0).unwrap();
    let owner = read
        .snapshot
        .visible
        .iter()
        .find(|n| n.kind == "block")
        .unwrap()
        .id
        .clone();
    let prepared = peer
        .prepare_commands(
            &read.revision,
            &[EditorCommand::ReplaceAt {
                owner,
                from: Utf16Offset(1),
                to: Utf16Offset(2),
                text: "".into(),
            }],
            &TextOrigin::new("peer:test").unwrap(),
            0,
        )
        .unwrap();
    let commit = peer.apply(prepared, 0).unwrap().unwrap();
    let entry = store
        .pending(&f.id)
        .unwrap()
        .into_iter()
        .find(|entry| entry.id() == &commit.update)
        .unwrap();
    session.service.receive_peer(entry).unwrap();
    let current = session.service.read(0).unwrap().revision;
    let mut draft = "draft".into();
    assert!(matches!(
        commands.apply_review(&review, &mut session, &mut draft, 0, &f.root),
        Err(CommandError::Review(ReviewError::Stale))
    ));
    assert_eq!(draft, "draft");
    assert_eq!(current, session.service.read(0).unwrap().revision);
    assert_eq!(
        commands.review(&session, &draft, 0).unwrap_err(),
        ReviewError::Stale
    );
}
#[test]
fn validated_review_commits_once_through_native_journal_and_history() {
    let f = Fixture::new();
    let (mut session, store) = f.local("base", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft: String = "reviewed".into();
    let review = commands.review(&session, &draft, 0).unwrap();
    assert!(
        commands
            .apply_review(&review, &mut session, &mut draft, 0, &f.root)
            .unwrap()
            .changed
    );
    assert_eq!(session.source, "reviewed");
    assert_eq!(store.pending(&f.id).unwrap().len(), 1);
    assert!(
        commands
            .apply_review(&review, &mut session, &mut draft, 0, &f.root)
            .is_err()
    );
    commands
        .execute(&mut session, Command::Undo, &mut draft, 0, &f.root)
        .unwrap();
    assert_eq!(draft, "base");
}
#[test]
fn opaque_review_is_honest_and_private_export_retains_original_tuple() {
    let f = Fixture::new();
    let (mut session, store) = f.local_fixture("opaque\n", false, true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let review = commands.review(&session, "replace opaque\n", 0).unwrap();
    assert!(review.limitation.contains("opaque"));
    let revision = session.service.read(0).unwrap().revision;
    let mut draft = "replace opaque\n".into();
    assert!(
        commands
            .apply_review(&review, &mut session, &mut draft, 0, &f.root)
            .is_err()
    );
    assert_eq!(revision, session.service.read(0).unwrap().revision);
    assert!(store.pending(&f.id).unwrap().is_empty());
    let path = diff::export_private(&session, &draft, 0, &f.root).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["source"], draft);
    assert!(
        value["semantic"]
            .as_str()
            .unwrap()
            .contains("exact opaque payload")
    );
    let checkpoint = session.service.session().recovery_export().checkpoint;
    assert_eq!(value["checkpoint"], serde_json::json!(checkpoint.bytes));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
fn readonly_route(event: Event) {
    let f = Fixture::new();
    let (mut session, store) = f.local("base", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut view = View::from_session(&session, 0);
    session.service.set_policy(HostPolicy {
        writable: false,
        ..Default::default()
    });
    let before = session.service.read(0).unwrap().revision;
    route(&mut session, Some(&mut commands), &mut view, &event, 0, &f);
    assert_eq!(view.source, "base");
    assert_eq!(session.source, "base");
    assert_eq!(before, session.service.read(0).unwrap().revision);
    assert!(store.pending(&f.id).unwrap().is_empty());
}
macro_rules! readonly_case {
    ($name:ident,$event:expr) => {
        #[test]
        fn $name() {
            readonly_route($event)
        }
    };
}
readonly_case!(readonly_keyboard_insert, plain(KeyCode::Char('x')));
readonly_case!(readonly_keyboard_delete, plain(KeyCode::Delete));
readonly_case!(readonly_keyboard_backspace, plain(KeyCode::Backspace));
readonly_case!(readonly_keyboard_newline, plain(KeyCode::Enter));
readonly_case!(readonly_bracketed_paste, Event::Paste("injected\n".into()));
readonly_case!(readonly_save_shortcut, key(KeyCode::Char('s')));
readonly_case!(readonly_commit_shortcut, key(KeyCode::Enter));
readonly_case!(readonly_undo_shortcut, key(KeyCode::Char('z')));
readonly_case!(readonly_redo_shortcut, key(KeyCode::Char('y')));
#[test]
fn readonly_direct_commands_reject_dirty_draft_without_touching_file() {
    let f = Fixture::new();
    let (mut session, store) = f.local("base", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    session.service.set_policy(HostPolicy {
        writable: false,
        ..Default::default()
    });
    for command in [Command::Commit, Command::Save, Command::Undo, Command::Redo] {
        let mut draft = "base".into();
        assert!(
            commands
                .execute(&mut session, command, &mut draft, 0, &f.root)
                .is_err()
        );
        assert_eq!(draft, "base");
    }
    assert_eq!(
        fs::read_to_string(f.root.join("source.txt")).unwrap(),
        "base"
    );
    assert!(store.pending(&f.id).unwrap().is_empty());
}
#[test]
fn readonly_review_copy_and_export_work_without_a_mutation_binding() {
    let f = Fixture::new();
    let (mut session, store) = f.local("base", true);
    session.service.set_policy(HostPolicy {
        writable: false,
        ..Default::default()
    });
    assert!(Commands::capture(&session, 0).is_err());
    let mut view = View::from_session(&session, 0);
    route(
        &mut session,
        None,
        &mut view,
        &key(KeyCode::Char('d')),
        0,
        &f,
    );
    assert!(view.review.is_some());
    route(&mut session, None, &mut view, &key(KeyCode::Enter), 0, &f);
    assert!(view.review.is_some());
    route(
        &mut session,
        None,
        &mut view,
        &key(KeyCode::Char('e')),
        0,
        &f,
    );
    assert!(
        view.input_message
            .as_ref()
            .unwrap()
            .contains("private source")
    );
    assert_eq!(
        diff::copy_source(&session, &view.source, 0).unwrap(),
        "base"
    );
    assert!(fs::read_dir(&f.root).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("editac-tui-export")
    }));
    assert!(store.pending(&f.id).unwrap().is_empty());
}
#[test]
fn readonly_review_apply_rechecks_permission_and_history_policy() {
    let f = Fixture::new();
    let (mut session, store) = f.local("base", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let review = commands.review(&session, "reviewed", 0).unwrap();
    let mut draft: String = "reviewed".into();
    session.service.set_policy(HostPolicy {
        history_view: true,
        ..Default::default()
    });
    assert!(
        commands
            .apply_review(&review, &mut session, &mut draft, 0, &f.root)
            .is_err()
    );
    assert_eq!(draft, "reviewed");
    assert!(store.pending(&f.id).unwrap().is_empty());
}
#[test]
fn expired_permission_refreshes_paste_gate_before_input() {
    let f = Fixture::new();
    let (mut session, store) = f.local("base", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut view = View::from_session(&session, 0);
    session.service.set_permission(CachedPermission {
        writable: true,
        expires_at: Some(1),
    });
    route(
        &mut session,
        Some(&mut commands),
        &mut view,
        &Event::Paste("late".into()),
        1,
        &f,
    );
    assert_eq!(view.source, "base");
    assert!(!view.input.unwrap().writable);
    assert!(store.pending(&f.id).unwrap().is_empty());
}
#[test]
fn bounded_navigation_returns_to_editing_and_blocks_review_paste() {
    let f = Fixture::new();
    let (mut session, store) = f.local("a\nb\nc\nd\n", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut view = View::from_session(&session, 0);
    view.source = "A\nB\nC\nD\n".into();
    route(
        &mut session,
        Some(&mut commands),
        &mut view,
        &key(KeyCode::Char('d')),
        0,
        &f,
    );
    let first = view
        .review
        .as_ref()
        .unwrap()
        .page(1)
        .next()
        .unwrap()
        .to_string();
    route(
        &mut session,
        Some(&mut commands),
        &mut view,
        &plain(KeyCode::End),
        0,
        &f,
    );
    assert_ne!(view.review.as_ref().unwrap().page(1).next().unwrap(), first);
    route(
        &mut session,
        Some(&mut commands),
        &mut view,
        &plain(KeyCode::Home),
        0,
        &f,
    );
    assert_eq!(view.review.as_ref().unwrap().page(1).next().unwrap(), first);
    route(
        &mut session,
        Some(&mut commands),
        &mut view,
        &Event::Paste("bad".into()),
        0,
        &f,
    );
    assert_eq!(view.source, "A\nB\nC\nD\n");
    route(
        &mut session,
        Some(&mut commands),
        &mut view,
        &plain(KeyCode::Esc),
        0,
        &f,
    );
    assert!(view.review.is_none());
    route(
        &mut session,
        Some(&mut commands),
        &mut view,
        &plain(KeyCode::Char('x')),
        0,
        &f,
    );
    assert!(view.source.starts_with('x'));
    assert!(store.pending(&f.id).unwrap().is_empty());
}
struct Events {
    events: VecDeque<Event>,
    terminal: ratatui::Terminal<ratatui::backend::TestBackend>,
    frames: Vec<String>,
}
impl TerminalIo for Events {
    fn raw_enabled(&mut self) -> io::Result<bool> {
        Ok(false)
    }
    fn mode(&mut self, _: Mode) -> io::Result<()> {
        Ok(())
    }
    fn size(&mut self) -> io::Result<(u16, u16)> {
        Ok((180, 12))
    }
    fn initialize(&mut self, _: (u16, u16)) -> io::Result<()> {
        Ok(())
    }
    fn resize(&mut self, size: (u16, u16)) -> io::Result<()> {
        self.terminal.backend_mut().resize(size.0, size.1);
        self.terminal.autoresize().unwrap();
        Ok(())
    }
    fn draw(&mut self, v: &View) -> io::Result<()> {
        self.terminal.draw(|f| view::render(f, v)).unwrap();
        self.frames.push(
            self.terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>(),
        );
        Ok(())
    }
    fn read(&mut self, _: Duration) -> io::Result<Option<Event>> {
        self.events
            .pop_front()
            .map(Some)
            .ok_or_else(|| io::Error::other("events exhausted"))
    }
}
#[test]
fn actual_event_owner_renders_review_esc_returns_and_applies_once() {
    let f = Fixture::new();
    let (mut session, store) = f.local("old\n", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut view = View::from_session(&session, 0);
    view.source = "new\n".into();
    let events = Events {
        events: VecDeque::from([
            key(KeyCode::Char('d')),
            Event::Resize(180, 14),
            plain(KeyCode::PageDown),
            plain(KeyCode::Esc),
            key(KeyCode::Char('d')),
            key(KeyCode::Enter),
            plain(KeyCode::Esc),
        ]),
        terminal: ratatui::Terminal::new(ratatui::backend::TestBackend::new(180, 12)).unwrap(),
        frames: vec![],
    };
    let mut owner = TerminalOwner::new(events);
    owner
        .run_with(&mut view, |e, v| {
            dispatch_command(e, v, &mut session, Some(&mut commands), 0, &f.root)
        })
        .unwrap();
    assert!(
        owner
            .backend()
            .frames
            .iter()
            .any(|frame| frame.contains("base revision")
                && frame.contains("-old")
                && frame.contains("+new"))
    );
    assert_eq!(view.source, "new\n");
    assert_eq!(session.source, "new\n");
    assert_eq!(store.pending(&f.id).unwrap().len(), 1);
}
#[test]
fn review_render_escapes_untrusted_terminal_and_directional_controls() {
    let f = Fixture::new();
    let (session, _) = f.local("old", true);
    let mut view = View::from_session(&session, 0);
    view.source = "\x1b]52;secret\x07\u{202e}new".into();
    view.review = Some(Review::from_session(&session, &view.source, 0).unwrap());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(180, 12)).unwrap();
    terminal.draw(|f| view::render(f, &view)).unwrap();
    let cells = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(!cells.contains('\x1b'));
    assert!(!cells.contains('\x07'));
    assert!(!cells.contains('\u{202e}'));
    assert!(cells.contains("\\x1b"));
    assert!(cells.contains("\\u{202e}"));
}
#[test]
fn current_semantic_revision_and_source_hashes_are_separate_labels() {
    let f = Fixture::new();
    let (mut session, _) = f.local("before", true);
    let before = Review::from_session(&session, "after", 0).unwrap();
    advance(&mut session, "remote");
    let after = Review::from_session(&session, "after", 0).unwrap();
    assert_ne!(before.base_label, after.base_label);
    assert_eq!(before.proposed_label, after.proposed_label);
}
