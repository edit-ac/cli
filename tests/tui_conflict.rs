//! TUI-1.9 canonical: actual native journal, shared mapper, independent second
//! replica, prepared transport, real command/event/view path. No provider traffic.
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
        collaboration::Collaboration,
        commands::{Command, Commands},
        conflict::{ConflictError, ConflictView, MAX_SOURCE_BYTES, Pane, Reason},
        session::{Representation, TerminalSession},
        terminal::{Mode, TerminalIo, TerminalOwner, dispatch_command},
        view::View,
    },
};
use editac_session::{
    edit_service::{EditService, HostPolicy},
    ports::{
        memory::{FaultBoundary, InjectedFault},
        *,
    },
    session::{CachedPermission, Session},
    store::NativeStore,
};
use std::{
    fs, io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Harness {
    root: PathBuf,
    id: DocumentIdentity,
    store: Arc<NativeStore>,
    peer_store: Arc<NativeStore>,
    peer: EditService<Arc<NativeStore>>,
    session: TerminalSession<Arc<NativeStore>>,
    commands: Commands,
    collaboration: Collaboration,
    view: View,
}
impl Harness {
    fn new(source: &str, literal: bool) -> Self {
        Self::new_with_unknown(source, literal, false)
    }
    fn new_with_unknown(source: &str, literal: bool, unknown: bool) -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("editac-tui-conflict-{}-{n}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let id = DocumentIdentity::v5(
            format!("ad000000-0000-5000-8000-{n:012x}"),
            format!("ad000000-0000-4000-8000-{n:012x}"),
        )
        .unwrap();
        let path = root.join(if literal { "source.txt" } else { "source.md" });
        fs::write(&path, source).unwrap();
        let (doc, baseline) = LocalBaseline::import(
            target::resolve(&path, ResolveOptions::default()).unwrap(),
            CreationPermit::authorize_new(id.clone()).unwrap(),
        )
        .unwrap();
        let doc = if unknown {
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
        let make = |name| {
            let store = Arc::new(
                NativeStore::create_new(root.join(name), id.clone(), StoreLimits::default())
                    .unwrap(),
            );
            store
                .publish(0, CheckpointRecord::new(doc.checkpoint(), vec![]).unwrap())
                .unwrap();
            store
        };
        let store = make("local.sqlite");
        let peer_store = make("peer.sqlite");
        let session = TerminalSession {
            service: EditService::new(
                Session::open(store.clone(), &id, CachedPermission::LOCAL).unwrap(),
                HostPolicy::default(),
            ),
            source: source.into(),
            mode: if literal {
                SourceMode::Literal
            } else {
                SourceMode::Markdown(Codecs::default())
            },
            representation: Representation::ExactSource,
            local_baseline: Some(baseline),
        };
        let commands = Commands::capture(&session, 0).unwrap();
        let collaboration = Collaboration::capture(&session, 0).unwrap();
        let view = View::from_session(&session, 0);
        // The granted host admits this prepared peer writer to its native journal.
        // This is the store's existing host policy, not authority derived from a URI.
        let peer = EditService::new(
            Session::open(peer_store.clone(), &id, CachedPermission::LOCAL).unwrap(),
            HostPolicy::default(),
        );
        store
            .claim_writer(peer.session().document().writer_id())
            .unwrap();
        Self {
            root,
            id,
            store,
            peer_store,
            peer,
            session,
            commands,
            collaboration,
            view,
        }
    }
    fn edit(&mut self, block: usize, from: u32, to: u32, text: &str) -> JournalEntry {
        let read = self.peer.read(0).unwrap();
        let owner = read
            .snapshot
            .visible
            .iter()
            .filter(|n| n.kind == "block")
            .nth(block)
            .unwrap()
            .id
            .clone();
        let p = self
            .peer
            .prepare_commands(
                &read.revision,
                &[EditorCommand::ReplaceAt {
                    owner,
                    from: Utf16Offset(from),
                    to: Utf16Offset(to),
                    text: text.into(),
                }],
                &TextOrigin::new("peer:remote").unwrap(),
                0,
            )
            .unwrap();
        let c = self.peer.apply(p, 0).unwrap().unwrap();
        self.peer_store
            .pending(&self.id)
            .unwrap()
            .into_iter()
            .find(|e| e.id() == &c.update)
            .unwrap()
    }
    fn enqueue(&mut self, e: JournalEntry) {
        self.collaboration.enqueue(&self.session, e).unwrap();
    }
    fn tick(&mut self) -> bool {
        self.collaboration
            .tick(
                &mut self.session,
                Some(&mut self.commands),
                &mut self.view,
                0,
            )
            .unwrap()
    }
    fn command(&mut self, c: Command) {
        self.commands
            .execute(&mut self.session, c, &mut self.view.source, 0, &self.root)
            .unwrap();
    }
    fn text(&self) -> String {
        self.session
            .service
            .session()
            .document()
            .snapshot()
            .unwrap()
            .visible
            .iter()
            .filter(|n| n.kind == "block")
            .map(|n| n.runs.iter().map(|r| r.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("|")
    }
    fn transport(&mut self, expires: u64) {
        self.session
            .service
            .attach_transport(
                Box::new(
                    editac_session::ports::memory::PreparedTransport::new(
                        self.id.clone(),
                        ConnectionState::Authorized,
                        TransportCapabilities {
                            receipts: ReceiptCapability::DurableV1,
                            max_frame_bytes: 1048576,
                            max_queue_bytes: 1048576,
                            max_queue_frames: 100,
                        },
                    )
                    .unwrap(),
                ),
                expires,
                0,
            )
            .unwrap();
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use editac_cli::markdown::reconcile::ConflictKind;
fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}
impl Harness {
    fn dispatch(&mut self, event: Event) -> bool {
        dispatch_command(
            &event,
            &mut self.view,
            &mut self.session,
            Some(&mut self.commands),
            0,
            &self.root,
        )
    }
    fn ctrl(&mut self, code: KeyCode) {
        assert!(self.dispatch(key(code, KeyModifiers::CONTROL)));
    }
    fn overlap(literal: bool) -> Self {
        let mut h = Self::new(if literal { "base" } else { "base\n" }, literal);
        h.view.source = if literal { "local" } else { "local\n" }.into();
        let entry = h.edit(0, 0, 4, "remote");
        h.enqueue(entry);
        h.tick();
        h
    }
    fn remote_commands(&mut self, commands: &[EditorCommand]) -> JournalEntry {
        let read = self.peer.read(0).unwrap();
        let p = self
            .peer
            .prepare_commands(
                &read.revision,
                commands,
                &TextOrigin::new("peer:remote").unwrap(),
                0,
            )
            .unwrap();
        let c = self.peer.apply(p, 0).unwrap().unwrap();
        self.peer_store
            .pending(&self.id)
            .unwrap()
            .into_iter()
            .find(|e| e.id() == &c.update)
            .unwrap()
    }
}
fn frame(view: &View, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| editac_cli::tui::view::render(f, view))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect::<String>()
}
#[test]
fn overlap_opens_labeled_three_way_without_partial_mutation() {
    let h = Harness::overlap(true);
    let c = h.view.conflict.as_ref().unwrap();
    assert_eq!(
        (&*c.base, &*c.local, &*c.current),
        ("base", "local", "remote")
    );
    assert_eq!(c.reason, Reason::Mapping(ConflictKind::Overlap));
    assert_eq!(h.text(), "remote");
    assert_eq!(h.store.pending(&h.id).unwrap().len(), 1); // remote entry only
    let screen = frame(&h.view, 100, 15);
    for label in [
        "CONFLICT",
        "base/local/current/resolution",
        "Base revision:",
        "Current revision:",
        "unprepared",
        "base",
    ] {
        assert!(screen.contains(label), "{label}");
    }
}
#[test]
fn every_pane_is_keyboard_accessible_and_exact_sources_remain_retained() {
    let mut h = Harness::overlap(true);
    for (pane, text) in [
        (Pane::Base, "base"),
        (Pane::Local, "local"),
        (Pane::Current, "remote"),
        (Pane::Resolution, "local"),
    ] {
        let c = h.view.conflict.as_ref().unwrap();
        assert_eq!(c.pane, pane);
        assert!(frame(&h.view, 100, 15).contains(text));
        assert!(h.dispatch(key(KeyCode::Tab, KeyModifiers::NONE)));
    }
    assert_eq!(h.view.source, "local");
    assert_eq!(h.text(), "remote");
}
#[test]
fn literal_explicit_resolution_is_prepared_then_committed_once() {
    let mut h = Harness::overlap(true);
    let revision = h.session.service.read(0).unwrap().revision;
    h.ctrl(KeyCode::Char('l'));
    assert!(h.dispatch(key(KeyCode::Char('a'), KeyModifiers::CONTROL)));
    assert!(h.dispatch(Event::Paste("resolved 👩‍💻\r\n\t# *literal*".into())));
    h.ctrl(KeyCode::Char('p'));
    assert!(h.view.conflict.as_ref().unwrap().prepared());
    assert_eq!(h.session.service.read(0).unwrap().revision, revision);
    assert_eq!(h.store.pending(&h.id).unwrap().len(), 1);
    h.ctrl(KeyCode::Enter);
    assert!(h.view.conflict.is_none());
    assert_eq!(h.view.source, "resolved 👩‍💻\r\n\t# *literal*");
    assert_eq!(h.text(), h.view.source);
    assert_eq!(h.store.pending(&h.id).unwrap().len(), 2);
    h.ctrl(KeyCode::Enter);
    assert_eq!(h.store.pending(&h.id).unwrap().len(), 2);
    h.command(Command::Undo);
    assert_eq!(h.text(), "remote");
    h.command(Command::Redo);
    assert_eq!(h.text(), "resolved 👩‍💻\r\n\t# *literal*");
}
#[test]
fn markdown_resolution_preserves_exact_spelling_and_shared_semantics() {
    let mut h = Harness::overlap(false);
    h.ctrl(KeyCode::Char('l'));
    h.view.conflict.as_mut().unwrap().resolution = "resolved **strong**\n".into();
    h.ctrl(KeyCode::Char('p'));
    assert!(h.view.conflict.as_ref().unwrap().prepared());
    h.ctrl(KeyCode::Enter);
    assert!(h.view.conflict.is_none());
    assert_eq!(h.view.source, "resolved **strong**\n");
    assert_eq!(h.text(), "resolved strong");
    let snapshot = h.session.service.read(0).unwrap().snapshot;
    assert!(
        snapshot
            .visible
            .iter()
            .flat_map(|n| &n.runs)
            .any(|r| r.text == "strong" && !r.attributes.is_empty())
    );
}
#[test]
fn selecting_current_can_close_conflict_as_qualified_noop() {
    let mut h = Harness::overlap(true);
    let revision = h.session.service.read(0).unwrap().revision;
    h.ctrl(KeyCode::Char('g'));
    h.ctrl(KeyCode::Char('p'));
    h.ctrl(KeyCode::Enter);
    assert!(h.view.conflict.is_none());
    assert_eq!(h.view.source, "remote");
    assert_eq!(h.session.service.read(0).unwrap().revision, revision);
    assert_eq!(h.store.pending(&h.id).unwrap().len(), 1);
    h.tick();
    assert!(h.collaboration.draft.is_none());
}
#[test]
fn apply_without_preparation_and_changed_candidate_are_inert() {
    let mut h = Harness::overlap(true);
    let revision = h.session.service.read(0).unwrap().revision;
    h.ctrl(KeyCode::Enter);
    assert!(
        h.view
            .input_message
            .as_ref()
            .unwrap()
            .contains("Unprepared")
    );
    h.ctrl(KeyCode::Char('p'));
    h.view.conflict.as_mut().unwrap().resolution.push('!');
    h.ctrl(KeyCode::Enter);
    assert!(
        h.view
            .input_message
            .as_ref()
            .unwrap()
            .contains("Unprepared")
    );
    assert_eq!(h.session.service.read(0).unwrap().revision, revision);
    assert_eq!(h.text(), "remote");
}
#[test]
fn remote_change_between_prepare_and_tick_rejects_apply() {
    let mut h = Harness::overlap(true);
    h.ctrl(KeyCode::Char('p'));
    let entry = h.edit(0, 0, 0, "new ");
    h.session.service.receive_peer(entry).unwrap();
    let revision = h.session.service.read(0).unwrap().revision;
    h.ctrl(KeyCode::Enter);
    assert!(h.view.input_message.as_ref().unwrap().contains("Stale"));
    assert_eq!(h.text(), "new remote");
    assert_eq!(h.session.service.read(0).unwrap().revision, revision);
    assert_eq!(h.view.conflict.as_ref().unwrap().resolution, "local");
}
#[test]
fn repeated_advancement_invalidates_resolution_but_keeps_opening_base_and_candidate() {
    let mut h = Harness::overlap(true);
    h.ctrl(KeyCode::Char('p'));
    for text in ["first ", "second "] {
        let entry = h.edit(0, 0, 0, text);
        h.enqueue(entry);
        h.tick();
    }
    let c = h.view.conflict.as_ref().unwrap();
    assert_eq!(
        (&*c.base, &*c.local, &*c.current, &*c.resolution),
        ("base", "local", "second first remote", "local")
    );
    assert!(!c.prepared());
    h.ctrl(KeyCode::Enter);
    assert_eq!(h.text(), "second first remote");
    h.ctrl(KeyCode::Char('g'));
    h.view
        .conflict
        .as_mut()
        .unwrap()
        .resolution
        .push_str(" resolved");
    h.ctrl(KeyCode::Char('p'));
    h.ctrl(KeyCode::Enter);
    assert_eq!(h.text(), "second first remote resolved");
}
#[test]
fn cancelled_view_retains_drafts_and_exports_private_full_recovery() {
    let mut h = Harness::overlap(true);
    h.ctrl(KeyCode::Char('p'));
    assert!(h.dispatch(key(KeyCode::Esc, KeyModifiers::NONE)));
    assert!(!h.view.conflict.as_ref().unwrap().open);
    assert!(!h.view.conflict.as_ref().unwrap().prepared());
    assert_eq!(h.view.source, "local");
    h.ctrl(KeyCode::Char('s'));
    assert_eq!(h.text(), "remote");
    let c = h.view.conflict.as_ref().unwrap();
    let path = c.export_private(&h.session, 0, &h.root).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for (key, text) in [
        ("base", "base"),
        ("local", "local"),
        ("current", "remote"),
        ("resolution", "local"),
    ] {
        assert_eq!(value["conflict"][key], text);
    }
    assert_eq!(value["conflict"]["unresolved"], true);
    assert_eq!(value["remote_durability"], "not asserted");
    assert_eq!(
        value["checkpoint"].as_array().unwrap().len(),
        h.session
            .service
            .session()
            .recovery_export()
            .checkpoint
            .bytes
            .len()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    h.ctrl(KeyCode::Char('t'));
    assert!(h.view.conflict.as_ref().unwrap().open);
}
#[test]
fn export_command_includes_conflict_after_cancel_and_later_typing() {
    let mut h = Harness::overlap(true);
    h.dispatch(key(KeyCode::Esc, KeyModifiers::NONE));
    h.view.source = "later local".into();
    h.tick();
    h.ctrl(KeyCode::Char('e'));
    let path = fs::read_dir(&h.root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|s| s == "json"))
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(value["conflict"]["local"], "later local");
    assert_eq!(value["conflict"]["base"], "base");
    h.ctrl(KeyCode::Char('t'));
    h.ctrl(KeyCode::Char('l'));
    assert_eq!(h.view.conflict.as_ref().unwrap().resolution, "later local");
}
#[test]
fn removed_target_retains_triple_and_refuses_unqualified_rebinding() {
    let mut h = Harness::new("one\n\ntwo\n", false);
    h.view.source = "local one\n\ntwo\n".into();
    let owner = h
        .peer
        .read(0)
        .unwrap()
        .snapshot
        .visible
        .iter()
        .find(|n| n.kind == "block")
        .unwrap()
        .id
        .clone();
    let entry = h.remote_commands(&[
        EditorCommand::ReplaceAt {
            owner: owner.clone(),
            from: Utf16Offset(0),
            to: Utf16Offset(3),
            text: String::new(),
        },
        EditorCommand::Enable {
            target: owner,
            enabled: false,
        },
    ]);
    h.enqueue(entry);
    h.tick();
    let c = h.view.conflict.as_ref().unwrap();
    assert_eq!(c.reason, Reason::Mapping(ConflictKind::DeletedTarget));
    assert_eq!(c.base, "one\n\ntwo\n");
    assert_eq!(c.local, "local one\n\ntwo\n");
    assert!(!c.current.contains("one"));
    h.ctrl(KeyCode::Char('p'));
    assert!(!h.view.conflict.as_ref().unwrap().prepared());
    // The qualified engine retains empty target slots after disable/delete.
    // Do not guess a smaller ownership mapping or destroy that recovery state.
    assert_eq!(h.text(), "||two");
    let revision = h.session.service.read(0).unwrap().revision;
    h.ctrl(KeyCode::Char('g'));
    h.ctrl(KeyCode::Char('p'));
    assert!(!h.view.conflict.as_ref().unwrap().prepared());
    h.ctrl(KeyCode::Enter);
    assert_eq!(h.session.service.read(0).unwrap().revision, revision);
    let c = h.view.conflict.as_ref().unwrap();
    let path = c.export_private(&h.session, 0, &h.root).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(value["conflict"]["current"], c.current);
    assert_eq!(value["conflict"]["base"], "one\n\ntwo\n");
}
#[test]
fn ambiguous_structural_mapping_opens_conflict_without_remote_or_mutation() {
    let mut h = Harness::new("base\n", false);
    let revision = h.session.service.read(0).unwrap().revision;
    h.view.source = "# *literal*\r\n\t😀plain\n".into();
    h.ctrl(KeyCode::Enter);
    let c = h.view.conflict.as_ref().unwrap();
    assert_eq!(c.reason, Reason::Mapping(ConflictKind::AmbiguousMapping));
    assert_eq!(c.base, "base\n");
    assert_eq!(c.local, h.view.source);
    assert_eq!(h.session.service.read(0).unwrap().revision, revision);
    h.ctrl(KeyCode::Char('p'));
    assert!(!h.view.conflict.as_ref().unwrap().prepared());
    h.ctrl(KeyCode::Char('g'));
    h.view.conflict.as_mut().unwrap().resolution = "fixed\n".into();
    h.ctrl(KeyCode::Char('p'));
    h.ctrl(KeyCode::Enter);
    assert_eq!(h.text(), "fixed");
}
#[test]
fn unknown_content_rejects_resolution_and_export_keeps_opaque_semantics() {
    let mut h = Harness::new_with_unknown("opaque\n", false, true);
    let revision = h.session.service.read(0).unwrap().revision;
    h.view.source = "replace opaque\n".into();
    h.ctrl(KeyCode::Enter);
    let c = h.view.conflict.as_ref().unwrap();
    assert_eq!(c.reason, Reason::Mapping(ConflictKind::UnsupportedContent));
    h.ctrl(KeyCode::Char('p'));
    h.ctrl(KeyCode::Enter);
    assert_eq!(h.session.service.read(0).unwrap().revision, revision);
    let c = h.view.conflict.as_ref().unwrap();
    let path = c.export_private(&h.session, 0, &h.root).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert!(
        value["semantic"]
            .as_str()
            .unwrap()
            .contains("exact opaque payload")
    );
    assert_eq!(value["conflict"]["base"], "opaque\n");
    assert_eq!(value["conflict"]["local"], "replace opaque\n");
    assert_eq!(h.text(), "opaque");
    assert!(h.store.pending(&h.id).unwrap().is_empty());
}
#[test]
fn incompatible_epoch_is_rejected_and_never_offers_same_epoch_resolution() {
    let mut h = Harness::new("base", true);
    h.view.source = "local".into();
    let mut other = Harness::new("other epoch", true);
    let entry = other.edit(0, 0, 0, "remote ");
    let rejected = h
        .collaboration
        .enqueue(&h.session, entry.clone())
        .unwrap_err();
    assert_eq!(rejected.1, entry);
    let revision = h.session.service.read(0).unwrap().revision;
    h.tick();
    let c = h.view.conflict.as_ref().unwrap();
    assert_eq!(c.reason, Reason::Incompatible);
    assert_eq!(
        (&*c.base, &*c.local, &*c.current),
        ("base", "local", "base")
    );
    h.ctrl(KeyCode::Char('p'));
    assert!(
        h.view
            .input_message
            .as_ref()
            .unwrap()
            .contains("Incompatible")
    );
    h.ctrl(KeyCode::Enter);
    assert_eq!(h.session.service.read(0).unwrap().revision, revision);
    assert_eq!(h.text(), "base");
    assert_eq!(h.collaboration.pending(), (0, 0));
    assert!(
        h.view
            .conflict
            .as_ref()
            .unwrap()
            .export_private(&h.session, 0, &h.root)
            .is_ok()
    );
}
#[test]
fn readonly_revocation_after_preparation_blocks_all_resolution_mutation() {
    let mut h = Harness::overlap(true);
    h.ctrl(KeyCode::Char('p'));
    let revision = h.session.service.read(0).unwrap().revision;
    h.session.service.set_permission(CachedPermission {
        writable: false,
        expires_at: None,
    });
    h.ctrl(KeyCode::Enter);
    h.ctrl(KeyCode::Char('l'));
    h.dispatch(Event::Paste("unauthorized".into()));
    assert_eq!(h.view.conflict.as_ref().unwrap().resolution, "local");
    h.ctrl(KeyCode::Char('p'));
    assert!(!h.view.conflict.as_ref().unwrap().prepared());
    assert_eq!(h.session.service.read(0).unwrap().revision, revision);
    assert!(
        h.view
            .conflict
            .as_ref()
            .unwrap()
            .export_private(&h.session, 0, &h.root)
            .is_ok()
    );
}
#[test]
fn source_panes_and_history_shortcuts_cannot_mutate_conflict_draft_or_replica() {
    let mut h = Harness::overlap(true);
    let revision = h.session.service.read(0).unwrap().revision;
    for event in [
        Event::Paste("bad".into()),
        key(KeyCode::Delete, KeyModifiers::NONE),
        key(KeyCode::Char('s'), KeyModifiers::CONTROL),
        key(KeyCode::Char('z'), KeyModifiers::CONTROL),
        key(KeyCode::Char('y'), KeyModifiers::CONTROL),
        key(KeyCode::Char('d'), KeyModifiers::CONTROL),
    ] {
        assert!(h.dispatch(event));
    }
    assert_eq!(h.view.source, "local");
    assert_eq!(h.text(), "remote");
    assert_eq!(h.session.service.read(0).unwrap().revision, revision);
}
#[test]
fn failed_resolution_persistence_retains_all_recovery_candidates() {
    let mut h = Harness::overlap(true);
    h.ctrl(KeyCode::Char('p'));
    h.store
        .inject_once(InjectedFault {
            boundary: FaultBoundary::BeforeWrite,
            error: PortError::Quota,
        })
        .unwrap();
    h.ctrl(KeyCode::Enter);
    let c = h.view.conflict.as_ref().unwrap();
    assert_eq!(
        (&*c.base, &*c.local, &*c.current, &*c.resolution),
        ("base", "local", "remote", "local")
    );
    let path = c.export_private(&h.session, 0, &h.root).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(value["locally_committed"], false);
    assert!(value["unjournaled"]["bytes"].as_array().is_some());
    assert_eq!(h.store.pending(&h.id).unwrap().len(), 1); // unjournaled resolution cannot be called locally saved
    assert!(h.view.input_message.as_ref().unwrap().contains("Rejected"));
}
#[test]
fn bounded_sources_and_paste_reject_without_losing_candidate() {
    let mut h = Harness::overlap(true);
    let revision = h.session.service.read(0).unwrap().revision;
    assert!(matches!(
        ConflictView::capture(
            &"x".repeat(MAX_SOURCE_BYTES + 1),
            "local",
            "remote",
            &revision,
            revision.clone(),
            Reason::Unavailable
        ),
        Err(ConflictError::Limit)
    ));
    h.ctrl(KeyCode::Char('l'));
    h.dispatch(Event::Paste(
        "x".repeat(editac_cli::tui::input::MAX_PASTE_BYTES + 1),
    ));
    assert_eq!(h.view.conflict.as_ref().unwrap().resolution, "local");
    h.view.conflict.as_mut().unwrap().resolution = "x".repeat(MAX_SOURCE_BYTES + 1);
    h.ctrl(KeyCode::Char('p'));
    assert!(h.view.input_message.as_ref().unwrap().contains("Limit"));
    assert_eq!(h.text(), "remote");
}
#[test]
fn conflict_frames_escape_controls_and_bound_tiny_and_long_views() {
    let mut h = Harness::overlap(true);
    let c = h.view.conflict.as_mut().unwrap();
    c.base = "\x1b]52;c;attack\x07\u{202e} text\n".repeat(500);
    c.pane = Pane::Base;
    let screen = frame(&h.view, 100, 12);
    assert!(!screen.contains('\x1b'));
    assert!(!screen.contains('\x07'));
    assert!(!screen.contains('\u{202e}'));
    assert!(screen.contains("\\x1b"));
    for (w, hgt) in [(1, 1), (1, 2), (3, 4)] {
        let screen = frame(&h.view, w, hgt);
        assert_eq!(screen.chars().count(), usize::from(w * hgt));
    }
    h.dispatch(key(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(h.view.conflict.as_ref().unwrap().page(1000).count(), 1);
    assert!(!format!("{:?}", h.view.conflict.as_ref().unwrap()).contains("attack"));
}
struct ScriptIo {
    events: std::collections::VecDeque<Event>,
    modes: Vec<Mode>,
    frames: Vec<String>,
}
impl TerminalIo for ScriptIo {
    fn raw_enabled(&mut self) -> io::Result<bool> {
        Ok(false)
    }
    fn mode(&mut self, m: Mode) -> io::Result<()> {
        self.modes.push(m);
        Ok(())
    }
    fn size(&mut self) -> io::Result<(u16, u16)> {
        Ok((100, 15))
    }
    fn initialize(&mut self, _: (u16, u16)) -> io::Result<()> {
        Ok(())
    }
    fn resize(&mut self, _: (u16, u16)) -> io::Result<()> {
        Ok(())
    }
    fn draw(&mut self, v: &View) -> io::Result<()> {
        self.frames.push(frame(v, 100, 15));
        Ok(())
    }
    fn read(&mut self, _: Duration) -> io::Result<Option<Event>> {
        self.events
            .pop_front()
            .map(Some)
            .ok_or_else(|| io::Error::other("script exhausted"))
    }
}
#[test]
fn terminal_escape_cancels_conflict_then_quits_and_restores_modes() {
    let mut h = Harness::overlap(true);
    let io = ScriptIo {
        events: [
            key(KeyCode::Esc, KeyModifiers::NONE),
            key(KeyCode::Char('e'), KeyModifiers::CONTROL),
            key(KeyCode::Esc, KeyModifiers::NONE),
        ]
        .into(),
        modes: vec![],
        frames: vec![],
    };
    let mut owner = TerminalOwner::new(io);
    owner
        .run_with(&mut h.view, |event, view| {
            dispatch_command(
                event,
                view,
                &mut h.session,
                Some(&mut h.commands),
                0,
                &h.root,
            )
        })
        .unwrap();
    assert_eq!(owner.backend().modes.len(), 10);
    assert!(owner.backend().frames.len() >= 3);
    assert_eq!(h.view.source, "local");
    assert!(!h.view.conflict.as_ref().unwrap().open);
    assert_eq!(h.text(), "remote");
}

#[test]
fn resolution_terminal_loop_commits_then_observes_without_reopening_conflict() {
    let mut h = Harness::overlap(true);
    h.transport(100);
    let events = [
        key(KeyCode::Char('l'), KeyModifiers::CONTROL),
        key(KeyCode::Char('a'), KeyModifiers::CONTROL),
        Event::Paste("terminal resolution".into()),
        key(KeyCode::Char('p'), KeyModifiers::CONTROL),
        key(KeyCode::Enter, KeyModifiers::CONTROL),
        key(KeyCode::Esc, KeyModifiers::NONE),
    ];
    let mut owner = TerminalOwner::new(ScriptIo {
        events: events.into(),
        modes: vec![],
        frames: vec![],
    });
    let shared = std::cell::RefCell::new((&mut h.session, &mut h.commands, &mut h.collaboration));
    owner
        .run_with_tick(
            &mut h.view,
            |event, view| {
                let mut state = shared.borrow_mut();
                let (session, commands, _) = &mut *state;
                dispatch_command(event, view, session, Some(commands), 0, &h.root)
            },
            |view| {
                let mut state = shared.borrow_mut();
                let (session, commands, collaboration) = &mut *state;
                collaboration
                    .tick(session, Some(commands), view, 0)
                    .unwrap()
            },
        )
        .unwrap();
    drop(shared);
    assert!(h.view.conflict.is_none());
    assert!(h.collaboration.draft.is_none());
    assert_eq!(h.view.source, "terminal resolution");
    assert_eq!(h.text(), "terminal resolution");
    assert_eq!(owner.backend().modes.len(), 10);
    assert!(
        owner
            .backend()
            .frames
            .iter()
            .any(|s| s.contains("prepared (not applied)"))
    );
    assert!(!h.view.status.contains("RemotelyCommitted"));
    assert_eq!(h.store.pending(&h.id).unwrap().len(), 2);
}
#[test]
fn cancelled_standalone_mapping_conflict_reopens_with_latest_source() {
    let mut h = Harness::new("base\n", false);
    h.view.source = "# heading\n\nother\n".into();
    h.ctrl(KeyCode::Enter);
    assert!(h.view.conflict.is_some());
    h.dispatch(key(KeyCode::Esc, KeyModifiers::NONE));
    h.view.source = "latest paragraph\n".into();
    h.ctrl(KeyCode::Char('t'));
    h.ctrl(KeyCode::Char('l'));
    assert_eq!(
        h.view.conflict.as_ref().unwrap().resolution,
        "latest paragraph\n"
    );
    h.ctrl(KeyCode::Char('p'));
    h.ctrl(KeyCode::Enter);
    assert_eq!(h.text(), "latest paragraph");
}
#[test]
fn resolution_caret_and_selection_use_unicode_screen_mapping() {
    let mut h = Harness::overlap(true);
    h.ctrl(KeyCode::Char('l'));
    h.dispatch(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    h.dispatch(Event::Paste("😀\t👩‍💻\r\ne\u{301}界".into()));
    let c = h.view.conflict.as_ref().unwrap();
    assert_eq!(
        c.input.caret,
        Utf16Offset(c.resolution.encode_utf16().count() as u32)
    );
    assert_eq!(c.input.anchor, c.input.caret);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 12)).unwrap();
    terminal
        .draw(|f| editac_cli::tui::view::render(f, &h.view))
        .unwrap();
    assert_eq!(
        terminal.get_cursor_position().unwrap(),
        ratatui::layout::Position::new(3, 5)
    );
    h.dispatch(key(KeyCode::Left, KeyModifiers::SHIFT));
    assert_ne!(
        h.view.conflict.as_ref().unwrap().input.anchor,
        h.view.conflict.as_ref().unwrap().input.caret
    );
    h.dispatch(Event::Paste("end".into()));
    assert_eq!(
        h.view.conflict.as_ref().unwrap().resolution,
        "😀\t👩‍💻\r\ne\u{301}end"
    );
}

#[test]
fn retained_conflict_tracks_remote_revision_even_after_source_returns_to_clean() {
    let mut h = Harness::overlap(true);
    h.ctrl(KeyCode::Char('p'));
    h.dispatch(key(KeyCode::Esc, KeyModifiers::NONE));
    h.view.source = h.session.source.clone();
    let entry = h.edit(0, 0, 0, "next ");
    h.enqueue(entry);
    h.tick();
    let c = h.view.conflict.as_ref().unwrap();
    assert!(c.open);
    assert!(!c.prepared());
    assert_eq!(c.base, "base");
    assert_eq!(c.resolution, "local");
    assert_eq!(c.current, "next remote");
    assert_eq!(c.expected(), &h.session.service.read(0).unwrap().revision);
    h.ctrl(KeyCode::Char('p'));
    assert!(h.view.conflict.as_ref().unwrap().prepared());
    h.ctrl(KeyCode::Enter);
    assert_eq!(h.text(), "local");
}
