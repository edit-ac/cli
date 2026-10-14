//! TUI-1.8 canonical: actual native journal, shared mapper, independent second
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
        collaboration::{Collaboration, MAX_PENDING, MAX_PER_TICK},
        commands::{Command, Commands},
        input::InputState,
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
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("editac-tui-remote-{}-{n}", std::process::id()));
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
#[test]
fn clean_literal_refresh_preserves_backward_unicode_selection() {
    let mut h = Harness::new("ab👩‍💻e\u{301}界\tz\r\nend", true);
    let old = h.view.source.clone();
    let a = editac_cli::tui::positions::byte_to_offset(&old, old.find('z').unwrap() + 1).unwrap();
    let c = editac_cli::tui::positions::byte_to_offset(&old, old.find('界').unwrap()).unwrap();
    h.view.input.as_mut().unwrap().select(&old, a, c).unwrap();
    let e = h.edit(0, 0, 0, "前");
    h.enqueue(e);
    assert!(h.tick());
    assert_eq!(h.view.source, format!("前{old}"));
    assert_eq!(h.view.input.as_ref().unwrap().anchor, Utf16Offset(a.0 + 1));
    assert_eq!(h.view.input.as_ref().unwrap().caret, Utf16Offset(c.0 + 1));
    assert!(!h.view.draft_changed);
    assert!(h.collaboration.draft.is_none());
}
#[test]
fn deleted_selection_collapses_to_valid_grapheme() {
    let mut h = Harness::new("abcd", true);
    h.view
        .input
        .as_mut()
        .unwrap()
        .select("abcd", Utf16Offset(1), Utf16Offset(3))
        .unwrap();
    let e = h.edit(0, 0, 4, "👩‍💻");
    h.enqueue(e);
    h.tick();
    assert_eq!(h.view.input.as_ref().unwrap().caret, Utf16Offset(5));
    assert_eq!(h.view.input.as_ref().unwrap().anchor, Utf16Offset(5));
}
#[test]
fn clean_markdown_refresh_labels_projection_and_rebinds() {
    let mut h = Harness::new("one\n\ntwo\n", false);
    let e = h.edit(1, 0, 0, "remote ");
    h.enqueue(e);
    h.tick();
    assert!(h.view.source.contains("remote two"));
    assert_eq!(h.session.representation, Representation::HostedProjection);
    h.view.source = h.view.source.replace("one", "local one");
    h.command(Command::Commit);
    assert_eq!(h.text(), "local one|remote two");
}
#[test]
fn dirty_disjoint_blocks_prepare_without_mutation_then_explicitly_commit() {
    let mut h = Harness::new("one\n\ntwo\n", false);
    let base = h.view.source.clone();
    h.view.source = "local one\n\ntwo\n".into();
    let e = h.edit(1, 0, 0, "remote ");
    h.enqueue(e);
    h.tick();
    let d = h.collaboration.draft.as_ref().unwrap();
    assert_eq!(d.base, base);
    assert_eq!(d.local, "local one\n\ntwo\n");
    assert!(d.current.contains("remote two"));
    assert!(d.reconciled);
    assert_eq!(h.text(), "one|remote two");
    h.command(Command::Commit);
    assert_eq!(h.text(), "local one|remote two");
    assert!(h.view.source.contains("remote two"));
    assert!(h.view.source.contains("local one"));
    h.tick();
    assert!(h.collaboration.draft.is_none());
}
#[test]
fn dirty_disjoint_literal_ranges_use_shared_anchors() {
    let mut h = Harness::new("alpha beta", true);
    h.view.source = "ALPHA beta".into();
    let e = h.edit(0, 6, 10, "BETA");
    h.enqueue(e);
    h.tick();
    assert!(h.collaboration.draft.as_ref().unwrap().reconciled);
    h.command(Command::Commit);
    assert_eq!(h.text(), "ALPHA BETA");
    assert_eq!(h.view.source, "ALPHA BETA");
}
#[test]
fn overlap_retains_triple_and_rejects_stale_whole_source() {
    let mut h = Harness::new("base", true);
    h.view.source = "local".into();
    let e = h.edit(0, 0, 4, "remote");
    h.enqueue(e);
    h.tick();
    let d = h.collaboration.draft.as_ref().unwrap();
    assert_eq!(
        (&*d.base, &*d.local, &*d.current),
        ("base", "local", "remote")
    );
    assert!(!d.reconciled);
    let rev = h.session.service.read(0).unwrap().revision;
    assert!(
        h.commands
            .execute(
                &mut h.session,
                Command::Commit,
                &mut h.view.source,
                0,
                &h.root
            )
            .is_err()
    );
    assert_eq!(h.view.source, "local");
    assert_eq!(h.session.service.read(0).unwrap().revision, rev);
}
#[test]
fn another_remote_revision_invalidates_prepared_disjoint_admission() {
    let mut h = Harness::new("alpha beta", true);
    h.view.source = "ALPHA beta".into();
    let e = h.edit(0, 6, 10, "BETA");
    h.enqueue(e);
    h.tick();
    let e = h.edit(0, 0, 5, "REMOTE");
    h.session.service.receive_peer(e).unwrap();
    assert!(
        h.commands
            .execute(
                &mut h.session,
                Command::Commit,
                &mut h.view.source,
                0,
                &h.root
            )
            .is_err()
    );
    assert_eq!(h.view.source, "ALPHA beta");
    h.tick();
    assert!(!h.collaboration.draft.as_ref().unwrap().reconciled);
}
#[test]
fn repeated_advancement_preserves_original_base_and_newer_typing() {
    let mut h = Harness::new("one\n\ntwo\n", false);
    h.view.source = "local one\n\ntwo\n".into();
    let e = h.edit(1, 0, 0, "r1 ");
    h.enqueue(e);
    h.tick();
    h.view.source = "new local one\n\ntwo\n".into();
    let e = h.edit(1, 0, 0, "r2 ");
    h.enqueue(e);
    h.tick();
    let d = h.collaboration.draft.as_ref().unwrap();
    assert_eq!(d.base, "one\n\ntwo\n");
    assert_eq!(d.local, "new local one\n\ntwo\n");
    assert!(d.current.contains("r2 r1 two"));
    h.command(Command::Commit);
    assert_eq!(h.text(), "new local one|r2 r1 two");
}
#[test]
fn duplicates_do_not_replace_replica_revision_or_history() {
    let mut h = Harness::new("base", true);
    let ptr = h.session.service.session().document() as *const _;
    let e = h.edit(0, 0, 0, "remote ");
    h.enqueue(e.clone());
    h.enqueue(e.clone());
    assert_eq!(h.collaboration.pending().0, 1);
    h.tick();
    let rev = h.session.service.read(0).unwrap().revision;
    h.enqueue(e);
    assert!(!h.tick());
    assert_eq!(h.session.service.read(0).unwrap().revision, rev);
    assert_eq!(ptr, h.session.service.session().document() as *const _);
    assert_eq!(
        h.session
            .service
            .session()
            .document()
            .history_state()
            .undo_groups,
        0
    );
}
#[test]
fn reordered_updates_eventually_integrate_on_same_replica() {
    let mut h = Harness::new("base", true);
    let ptr = h.session.service.session().document() as *const _;
    let a = h.edit(0, 0, 0, "A");
    let b = h.edit(0, 0, 0, "B");
    h.enqueue(b.clone());
    h.tick();
    h.enqueue(a);
    h.enqueue(b);
    h.tick();
    assert_eq!(h.text(), "BAbase");
    assert_eq!(ptr, h.session.service.session().document() as *const _);
}
#[test]
fn bounded_queue_returns_envelope_and_drains_fixed_work() {
    let mut h = Harness::new("base", true);
    let mut entries = vec![];
    for _ in 0..MAX_PENDING + 1 {
        entries.push(h.edit(0, 0, 0, "x"));
    }
    for e in entries.iter().take(MAX_PENDING) {
        h.enqueue(e.clone());
    }
    let err = h
        .collaboration
        .enqueue(&h.session, entries[MAX_PENDING].clone())
        .unwrap_err();
    assert_eq!(err.1.id(), entries[MAX_PENDING].id());
    assert_eq!(h.collaboration.pending().0, MAX_PENDING);
    h.tick();
    assert_eq!(h.collaboration.pending().0, MAX_PENDING - MAX_PER_TICK);
    while h.collaboration.pending().0 > 0 {
        h.tick();
    }
    h.enqueue(err.1);
    h.tick();
    assert_eq!(h.text(), format!("{}base", "x".repeat(MAX_PENDING + 1)));
    assert_eq!(h.collaboration.pending(), (0, 0));
}
#[test]
fn wrong_epoch_is_rejected_before_queue_or_replica_mutation() {
    let mut h = Harness::new("base", true);
    let mut other = Harness::new("other", true);
    let e = other.edit(0, 0, 0, "bad");
    assert!(h.collaboration.enqueue(&h.session, e).is_err());
    assert_eq!(h.collaboration.pending(), (0, 0));
    assert_eq!(h.text(), "base");
}
#[test]
fn local_durability_failure_retains_queue_and_dirty_candidate() {
    let mut h = Harness::new("base", true);
    h.view.source = "local".into();
    let e = h.edit(0, 0, 0, "remote ");
    h.enqueue(e);
    h.store
        .inject_once(InjectedFault {
            boundary: FaultBoundary::BeforeWrite,
            error: PortError::Quota,
        })
        .unwrap();
    assert!(
        h.collaboration
            .tick(&mut h.session, Some(&mut h.commands), &mut h.view, 0)
            .is_err()
    );
    assert_eq!(h.collaboration.pending().0, 1);
    assert_eq!(h.text(), "base");
    assert_eq!(h.view.source, "local");
    h.session.service.recover().unwrap();
    h.tick();
    assert_eq!(h.collaboration.pending().0, 0);
    assert_eq!(h.view.source, "local");
}
#[test]
fn remote_edits_survive_local_undo_redo() {
    let mut h = Harness::new("alpha beta", true);
    h.view.source = "ALPHA beta".into();
    h.command(Command::Commit);
    let e = h.edit(0, 6, 10, "BETA");
    h.enqueue(e);
    h.tick();
    h.command(Command::Undo);
    assert_eq!(h.text(), "alpha BETA");
    assert_eq!(h.view.source, "alpha BETA");
    h.command(Command::Redo);
    assert_eq!(h.text(), "ALPHA BETA");
    assert_eq!(h.view.source, "ALPHA BETA");
}
#[test]
fn delayed_receipts_detach_reconnect_keep_pending_and_live_replica() {
    let mut h = Harness::new("base", true);
    let ptr = h.session.service.session().document() as *const _;
    h.view.source = "local".into();
    h.command(Command::Commit);
    let update = h.store.pending(&h.id).unwrap()[0].id().clone();
    h.transport(100);
    assert_eq!(h.session.service.flush_pending(0).unwrap(), 1);
    h.tick();
    assert!(h.view.status.contains("SentDurabilityUnknown"));
    h.session.service.detach_transport();
    h.view.source = "new local".into();
    let before = h.view.source.clone();
    assert!(
        h.session
            .service
            .accept_receipt(
                DurableReceipt {
                    protocol: ReceiptCapability::DurableV1,
                    update: update.clone()
                },
                0
            )
            .is_err()
    );
    h.tick();
    assert_eq!(h.view.source, before);
    assert_eq!(h.store.pending(&h.id).unwrap().len(), 1);
    h.transport(100);
    assert_eq!(
        h.session
            .service
            .accept_receipt(
                DurableReceipt {
                    protocol: ReceiptCapability::DurableV1,
                    update: update.clone()
                },
                0
            )
            .unwrap(),
        ReceiptResult::RemoteCommitted
    );
    assert_eq!(
        h.session
            .service
            .accept_receipt(
                DurableReceipt {
                    protocol: ReceiptCapability::DurableV1,
                    update
                },
                0
            )
            .unwrap(),
        ReceiptResult::RemoteCommitted
    );
    h.tick();
    assert!(h.view.status.contains("RemotelyCommitted"));
    assert_eq!(h.view.source, "new local");
    assert_eq!(ptr, h.session.service.session().document() as *const _);
}
#[test]
fn expired_transport_cannot_imply_remote_durability() {
    let mut h = Harness::new("base", true);
    h.transport(1);
    h.view.source = "local".into();
    h.command(Command::Commit);
    assert!(h.session.service.flush_pending(1).is_err());
    assert_eq!(h.store.pending(&h.id).unwrap().len(), 1);
}
#[test]
fn readonly_remote_refresh_still_rejects_typing_and_commit() {
    let mut h = Harness::new("base", true);
    h.session.service.set_permission(CachedPermission {
        writable: false,
        expires_at: None,
    });
    let e = h.edit(0, 0, 0, "remote ");
    h.enqueue(e); // refresh binding requires write capability; presentation still advances safely.
    assert!(
        h.collaboration
            .tick(&mut h.session, None, &mut h.view, 0)
            .unwrap()
    );
    assert_eq!(h.view.source, "remote base");
    assert!(!h.view.input.as_ref().unwrap().writable);
    h.view.source = "bad".into();
    assert!(
        h.commands
            .execute(
                &mut h.session,
                Command::Commit,
                &mut h.view.source,
                0,
                &h.root
            )
            .is_err()
    );
}
struct Io {
    reads: usize,
    draws: usize,
    modes: Vec<Mode>,
}
impl TerminalIo for Io {
    fn raw_enabled(&mut self) -> io::Result<bool> {
        Ok(false)
    }
    fn mode(&mut self, m: Mode) -> io::Result<()> {
        self.modes.push(m);
        Ok(())
    }
    fn size(&mut self) -> io::Result<(u16, u16)> {
        Ok((40, 8))
    }
    fn initialize(&mut self, _: (u16, u16)) -> io::Result<()> {
        Ok(())
    }
    fn resize(&mut self, _: (u16, u16)) -> io::Result<()> {
        Ok(())
    }
    fn draw(&mut self, v: &View) -> io::Result<()> {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 8)).unwrap();
        t.draw(|f| editac_cli::tui::view::render(f, v)).unwrap();
        self.draws += 1;
        Ok(())
    }
    fn read(&mut self, _: Duration) -> io::Result<Option<crossterm::event::Event>> {
        self.reads += 1;
        if self.reads == 1 {
            Ok(None)
        } else {
            Ok(Some(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Esc,
                    crossterm::event::KeyModifiers::NONE,
                ),
            )))
        }
    }
}
#[test]
fn idle_event_loop_observes_remote_without_a_key_and_tears_down() {
    let mut h = Harness::new("base", true);
    let e = h.edit(0, 0, 0, "remote ");
    h.enqueue(e);
    let mut terminal = TerminalOwner::new(Io {
        reads: 0,
        draws: 0,
        modes: vec![],
    });
    terminal
        .run_with_tick(
            &mut h.view,
            |_, _| false,
            |view| {
                h.collaboration
                    .tick(&mut h.session, Some(&mut h.commands), view, 0)
                    .unwrap()
            },
        )
        .unwrap();
    assert_eq!(h.view.source, "remote base");
    assert!(terminal.backend().draws >= 2);
    assert_eq!(terminal.backend().modes.len(), 10);
}
#[test]
fn stale_review_rejects_after_remote_tick() {
    let mut h = Harness::new("base", true);
    let review = h.commands.review(&h.session, "local", 0).unwrap();
    h.view.source = "local".into();
    let e = h.edit(0, 0, 0, "remote ");
    h.enqueue(e);
    h.tick();
    assert!(
        h.commands
            .apply_review(&review, &mut h.session, &mut h.view.source, 0, &h.root)
            .is_err()
    );
    assert_eq!(h.text(), "remote base");
    assert_eq!(h.view.source, "local");
}
#[test]
fn source_command_dispatch_commits_reconciled_draft_once() {
    let mut h = Harness::new("alpha beta", true);
    h.view.source = "ALPHA beta".into();
    let e = h.edit(0, 6, 10, "BETA");
    h.enqueue(e);
    h.tick();
    let key = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert!(dispatch_command(
        &key,
        &mut h.view,
        &mut h.session,
        Some(&mut h.commands),
        0,
        &h.root
    ));
    assert_eq!(h.text(), "ALPHA BETA");
    assert_eq!(h.view.source, "ALPHA BETA");
    let count = h.store.pending(&h.id).unwrap().len();
    assert!(dispatch_command(
        &key,
        &mut h.view,
        &mut h.session,
        Some(&mut h.commands),
        0,
        &h.root
    ));
    assert_eq!(h.store.pending(&h.id).unwrap().len(), count);
}
#[test]
fn presentation_mapping_does_not_split_crlf_combining_or_zwj() {
    let old = "a\r\ne\u{301}👩‍💻界";
    let mut input = InputState::new(true, old.len());
    input.select(old, Utf16Offset(3), Utf16Offset(10)).unwrap();
    editac_cli::tui::collaboration::refresh_selection(&mut input, old, &format!("x{old}"));
    assert_eq!(input.anchor, Utf16Offset(4));
    assert_eq!(input.caret, Utf16Offset(11));
}
#[test]
fn local_commit_observation_keeps_exact_markdown_spelling() {
    let mut h = Harness::new("one\n\ntwo\n", false);
    h.view.source = "ONE\n\ntwo\n".into();
    h.command(Command::Commit);
    let exact = h.view.source.clone();
    h.tick();
    assert_eq!(h.view.source, exact);
    assert_eq!(h.session.representation, Representation::ExactSource);
}
#[test]
fn dirty_typing_after_reconciliation_revalidates_candidate() {
    let mut h = Harness::new("alpha beta", true);
    h.view.source = "ALPHA beta".into();
    let e = h.edit(0, 6, 10, "BETA");
    h.enqueue(e);
    h.tick();
    h.view.source = "ALPHA mine".into();
    h.tick();
    assert!(!h.collaboration.draft.as_ref().unwrap().reconciled);
    assert!(
        h.commands
            .execute(
                &mut h.session,
                Command::Commit,
                &mut h.view.source,
                0,
                &h.root
            )
            .is_err()
    );
    assert_eq!(h.view.source, "ALPHA mine");
    assert_eq!(h.text(), "alpha BETA");
}
#[test]
fn remote_lost_ack_retains_envelope_and_retry_is_idempotent() {
    let mut h = Harness::new("base", true);
    h.view.source = "local".into();
    let e = h.edit(0, 0, 0, "remote ");
    h.enqueue(e);
    h.store
        .inject_once(InjectedFault {
            boundary: FaultBoundary::AfterCommit,
            error: PortError::Unavailable,
        })
        .unwrap();
    assert!(
        h.collaboration
            .tick(&mut h.session, Some(&mut h.commands), &mut h.view, 0)
            .is_err()
    );
    assert_eq!(h.collaboration.pending().0, 1);
    assert_eq!(h.view.source, "local");
    h.session.service.recover().unwrap();
    h.tick();
    assert_eq!(h.text(), "remote base");
    assert_eq!(h.collaboration.pending(), (0, 0));
    assert_eq!(h.view.source, "local");
    assert_eq!(h.store.pending(&h.id).unwrap().len(), 1);
}
#[test]
fn failed_reconciled_commit_recovers_merged_source_without_stale_submission() {
    let mut h = Harness::new("alpha beta", true);
    h.view.source = "ALPHA beta".into();
    let e = h.edit(0, 6, 10, "BETA");
    h.enqueue(e);
    h.tick();
    h.store
        .inject_once(InjectedFault {
            boundary: FaultBoundary::BeforeWrite,
            error: PortError::Quota,
        })
        .unwrap();
    assert!(
        h.commands
            .execute(
                &mut h.session,
                Command::Commit,
                &mut h.view.source,
                0,
                &h.root
            )
            .is_err()
    );
    assert_eq!(h.view.source, "ALPHA beta");
    h.command(Command::Recover);
    assert_eq!(h.view.source, "ALPHA BETA");
    assert_eq!(h.text(), "ALPHA BETA");
    h.tick();
    h.command(Command::Undo);
    assert_eq!(h.text(), "alpha BETA");
}
#[test]
fn wrong_delayed_receipt_never_discards_pending_records() {
    let mut h = Harness::new("base", true);
    h.view.source = "local".into();
    h.command(Command::Commit);
    h.transport(100);
    let mut update = h.store.pending(&h.id).unwrap()[0].id().clone();
    update.sequence += 10;
    assert_eq!(
        h.session
            .service
            .accept_receipt(
                DurableReceipt {
                    protocol: ReceiptCapability::DurableV1,
                    update
                },
                0
            )
            .unwrap(),
        ReceiptResult::UnknownUpdate
    );
    h.tick();
    assert_eq!(h.store.pending(&h.id).unwrap().len(), 1);
    assert!(!h.view.status.contains("RemotelyCommitted"));
}
#[test]
fn offline_dirty_draft_and_remote_queue_survive_transport_reconnect() {
    let mut h = Harness::new("alpha beta", true);
    h.transport(100);
    h.session.service.detach_transport();
    h.view.source = "ALPHA beta".into();
    let e = h.edit(0, 6, 10, "BETA");
    h.enqueue(e);
    let ptr = h.session.service.session().document() as *const _;
    h.transport(100);
    h.tick();
    assert_eq!(h.view.source, "ALPHA beta");
    assert!(h.collaboration.draft.as_ref().unwrap().reconciled);
    h.command(Command::Commit);
    assert_eq!(h.text(), "ALPHA BETA");
    assert_eq!(ptr, h.session.service.session().document() as *const _);
    assert_eq!(
        h.session
            .service
            .session()
            .document()
            .history_state()
            .undo_groups,
        1
    );
}
#[test]
fn selection_inside_unchanged_middle_survives_disjoint_remote_updates() {
    let mut h = Harness::new("first middle last", true);
    h.view
        .input
        .as_mut()
        .unwrap()
        .select("first middle last", Utf16Offset(6), Utf16Offset(12))
        .unwrap();
    let a = h.edit(0, 0, 5, "FIRSTLONG");
    let b = h.edit(0, 17, 21, "LASTLONG");
    h.enqueue(a);
    h.enqueue(b);
    h.tick();
    assert_eq!(h.text(), "FIRSTLONG middle LASTLONG");
    assert_eq!(h.view.input.as_ref().unwrap().anchor, Utf16Offset(10));
    assert_eq!(h.view.input.as_ref().unwrap().caret, Utf16Offset(16));
}
#[test]
fn queue_byte_limit_returns_oversized_envelope_without_loss() {
    let mut h = Harness::new("base", true);
    let e = JournalEntry::new(
        h.id.clone(),
        99,
        1,
        "host:prepared".into(),
        vec![0; editac_cli::tui::collaboration::MAX_PENDING_BYTES + 1],
    )
    .unwrap();
    let expected = e.id().clone();
    let (_, returned) = h.collaboration.enqueue(&h.session, e).unwrap_err();
    assert_eq!(returned.id(), &expected);
    assert_eq!(h.collaboration.pending(), (0, 0));
    assert_eq!(h.text(), "base");
}

#[test]
fn deleted_caret_collapses_to_replacement_end_while_surviving_anchor_stays_mapped() {
    let mut h = Harness::new("ab cd ef", true);
    h.view
        .input
        .as_mut()
        .unwrap()
        .select("ab cd ef", Utf16Offset(1), Utf16Offset(4))
        .unwrap();
    let e = h.edit(0, 3, 5, "👩‍💻");
    h.enqueue(e);
    h.tick();
    let input = h.view.input.as_ref().unwrap();
    assert_eq!(input.anchor, Utf16Offset(1));
    assert_eq!(input.caret, Utf16Offset(8));
    assert_eq!(h.view.source, "ab 👩‍💻 ef");
}

#[test]
fn conflicting_queued_update_returns_ownership_without_discarding_original() {
    let mut h = Harness::new("base", true);
    let entry = h.edit(0, 0, 0, "remote ");
    h.enqueue(entry.clone());
    for (origin, bytes) in [
        ("different-origin".to_string(), entry.bytes().to_vec()),
        (entry.origin().to_string(), vec![1, 2, 3]),
    ] {
        let conflicting = JournalEntry::new(
            entry.id().identity.clone(),
            entry.id().writer,
            entry.id().sequence,
            origin,
            bytes,
        )
        .unwrap();
        let expected = conflicting.clone();
        let (error, returned) = h
            .collaboration
            .enqueue(&h.session, conflicting)
            .unwrap_err();
        assert!(matches!(
            error,
            editac_cli::tui::collaboration::CollaborationError::ConflictingUpdate
        ));
        assert_eq!(returned, expected);
        assert_eq!(h.collaboration.pending().0, 1);
        assert_eq!(h.text(), "base");
    }
    h.tick();
    assert_eq!(h.text(), "remote base");
    assert_eq!(h.collaboration.pending(), (0, 0));
}

#[test]
fn reversed_backlog_larger_than_tick_budget_eventually_drains_on_same_replica() {
    let mut h = Harness::new("base", true);
    let replica = h.session.service.session().document() as *const _;
    let count = MAX_PER_TICK * 2 + 3;
    let entries: Vec<_> = (0..count).map(|_| h.edit(0, 0, 0, "x")).collect();
    for entry in entries.into_iter().rev() {
        h.enqueue(entry);
    }
    // Each attempt budget is bounded; later ticks must still reach predecessors.
    for _ in 0..count * count {
        if h.collaboration.pending().0 == 0 {
            break;
        }
        h.tick();
    }
    assert_eq!(h.collaboration.pending(), (0, 0));
    assert_eq!(h.text(), format!("{}base", "x".repeat(count)));
    assert_eq!(h.session.service.session().document() as *const _, replica);
    assert_eq!(
        h.session
            .service
            .session()
            .document()
            .history_state()
            .undo_groups,
        0
    );
}
