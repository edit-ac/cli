//! Original TUI-1.6: real importer/source service/SQLite/file publication and
//! deterministic host receipt boundaries. No provider or ambient credentials.
use editac_cli::{
    engine::document::{CreationPermit, DocumentIdentity},
    files::{self, FailureKind, Filesystem},
    markdown::{optional::Codecs, reconcile::SourceMode},
    reconcile::LocalBaseline,
    target::{self, ResolveOptions},
    tui::{
        commands::{Command, CommandError, Commands},
        session::{Representation, TerminalSession},
        terminal::dispatch_command,
        view::View,
    },
};
use editac_session::{
    edit_service::{EditService, HostPolicy, ServiceError},
    ports::{
        memory::{FaultBoundary, InjectedFault},
        *,
    },
    session::{CachedPermission, LocalState, Readiness, RemoteState, Session},
    store::NativeStore,
};
use std::{
    fs, io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    id: DocumentIdentity,
}
impl Fixture {
    fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("editac-tui-commit-{}-{n}", std::process::id()));
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
                recovery: None,
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
fn execute(
    commands: &mut Commands,
    session: &mut TerminalSession<Arc<NativeStore>>,
    command: Command,
    draft: &mut String,
    f: &Fixture,
) -> Result<editac_cli::tui::commands::CommandResult, CommandError> {
    commands.execute(session, command, draft, 0, &f.root)
}
fn text(session: &TerminalSession<Arc<NativeStore>>) -> String {
    session
        .service
        .read(0)
        .unwrap()
        .snapshot
        .visible
        .iter()
        .filter(|n| n.kind == "block")
        .map(|n| n.runs.iter().map(|r| r.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
#[test]
fn unchanged_source_is_exact_noop_without_journal_or_history() {
    let f = Fixture::new();
    let source = "## Heading\r\n\r\nspelling **bold**\r\n";
    let (mut session, store) = f.local(source, false);
    let before = session.service.session().document().checkpoint().bytes;
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = source.into();
    let result = execute(&mut commands, &mut session, Command::Commit, &mut draft, &f).unwrap();
    assert!(!result.changed);
    assert_eq!(session.source, source);
    assert_eq!(draft, source);
    assert_eq!(
        session.service.session().document().checkpoint().bytes,
        before
    );
    assert!(store.pending(&f.id).unwrap().is_empty());
    assert!(
        !execute(&mut commands, &mut session, Command::Undo, &mut draft, &f)
            .unwrap()
            .changed
    );
}
#[test]
fn prepared_commit_is_once_and_each_source_gesture_is_one_group() {
    let f = Fixture::new();
    let (mut session, store) = f.local("start", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "a👩‍💻\r\nb".into();
    assert!(
        execute(&mut commands, &mut session, Command::Commit, &mut draft, &f)
            .unwrap()
            .changed
    );
    assert!(
        !execute(&mut commands, &mut session, Command::Commit, &mut draft, &f)
            .unwrap()
            .changed
    );
    assert_eq!(store.pending(&f.id).unwrap().len(), 1);
    assert_eq!(text(&session), "a👩‍💻\r\nb");
    draft = "second".into();
    execute(&mut commands, &mut session, Command::Commit, &mut draft, &f).unwrap();
    execute(&mut commands, &mut session, Command::Undo, &mut draft, &f).unwrap();
    assert_eq!(draft, "a👩‍💻\r\nb");
    execute(&mut commands, &mut session, Command::Undo, &mut draft, &f).unwrap();
    assert_eq!(draft, "start");
    assert!(
        !execute(&mut commands, &mut session, Command::Undo, &mut draft, &f)
            .unwrap()
            .changed
    );
    execute(&mut commands, &mut session, Command::Redo, &mut draft, &f).unwrap();
    assert_eq!(draft, "a👩‍💻\r\nb");
    execute(&mut commands, &mut session, Command::Redo, &mut draft, &f).unwrap();
    assert_eq!(text(&session), "second");
}
#[test]
fn markdown_history_preserves_spelling_including_crlf() {
    let f = Fixture::new();
    let source = "spelling **bold**\r\n";
    let (mut session, _) = f.local(source, false);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "edited **bold**\r\n".into();
    execute(&mut commands, &mut session, Command::Commit, &mut draft, &f).unwrap();
    execute(&mut commands, &mut session, Command::Undo, &mut draft, &f).unwrap();
    assert_eq!(draft, source);
    execute(&mut commands, &mut session, Command::Redo, &mut draft, &f).unwrap();
    assert_eq!(draft, "edited **bold**\r\n");
}
#[test]
fn ambiguous_mapping_keeps_draft_source_live_state_and_disk() {
    let f = Fixture::new();
    let (mut session, store) = f.local("# heading\n\nparagraph\n", false);
    let before = session.service.session().document().checkpoint().bytes;
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "plain replacement\n".into();
    assert!(matches!(
        execute(&mut commands, &mut session, Command::Save, &mut draft, &f),
        Err(CommandError::Service(ServiceError::SourceConflict { .. }))
    ));
    assert_eq!(draft, "plain replacement\n");
    assert_eq!(session.source, "# heading\n\nparagraph\n");
    assert_eq!(
        session.service.session().document().checkpoint().bytes,
        before
    );
    assert!(store.pending(&f.id).unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(f.root.join("source.md")).unwrap(),
        session.source
    );
}
#[test]
fn unsupported_source_codec_is_recoverable_without_mutation() {
    let f = Fixture::new();
    let (mut session, store) = f.local("source\n", false);
    session.mode = SourceMode::Markdown(Codecs {
        images: true,
        ..Codecs::default()
    });
    let before = session.service.session().document().checkpoint().bytes;
    assert!(matches!(
        Commands::capture(&session, 0),
        Err(CommandError::Service(ServiceError::UnsupportedModule))
    ));
    assert_eq!(
        session.service.session().document().checkpoint().bytes,
        before
    );
    assert!(store.pending(&f.id).unwrap().is_empty());
    assert_eq!(session.source, "source\n");
}
#[test]
fn readonly_rechecks_commit_save_and_all_history_routes() {
    let f = Fixture::new();
    let (mut session, store) = f.local("source", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "edit".into();
    session.service.set_permission(CachedPermission {
        writable: false,
        expires_at: None,
    });
    for command in [Command::Commit, Command::Save, Command::Undo, Command::Redo] {
        assert!(execute(&mut commands, &mut session, command, &mut draft, &f).is_err());
    }
    assert_eq!(draft, "edit");
    assert_eq!(text(&session), "source");
    assert!(store.pending(&f.id).unwrap().is_empty());
}
#[test]
fn dirty_draft_never_disappears_during_history() {
    let f = Fixture::new();
    let (mut session, _) = f.local("source", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "committed".into();
    execute(&mut commands, &mut session, Command::Commit, &mut draft, &f).unwrap();
    draft = "unsaved draft".into();
    assert!(matches!(
        execute(&mut commands, &mut session, Command::Undo, &mut draft, &f),
        Err(CommandError::DirtyDraft)
    ));
    assert_eq!(draft, "unsaved draft");
    assert_eq!(text(&session), "committed");
}
#[test]
fn save_publishes_exact_bytes_and_refreshes_file_baseline_for_next_save() {
    let f = Fixture::new();
    let (mut session, store) = f.local("original", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "# literal\r\n👩‍💻\n".into();
    let result = execute(&mut commands, &mut session, Command::Save, &mut draft, &f).unwrap();
    assert_eq!(result.file, Some(files::SaveState::Saved));
    assert!(matches!(
        result.state.local,
        LocalState::Committed(Durability::ProcessCrash)
    ));
    assert_eq!(result.state.remote, RemoteState::Unknown);
    assert_eq!(
        fs::read(f.root.join("source.txt")).unwrap(),
        draft.as_bytes()
    );
    draft = "second".into();
    execute(&mut commands, &mut session, Command::Save, &mut draft, &f).unwrap();
    assert_eq!(
        fs::read_to_string(f.root.join("source.txt")).unwrap(),
        draft
    );
    assert_eq!(store.pending(&f.id).unwrap().len(), 2);
    drop(commands);
    drop(session);
    let reopened = Session::open(store.clone(), &f.id, CachedPermission::LOCAL).unwrap();
    assert_eq!(
        reopened
            .document()
            .snapshot()
            .unwrap()
            .visible
            .iter()
            .find(|n| n.kind == "block")
            .unwrap()
            .runs
            .iter()
            .map(|r| r.text.as_str())
            .collect::<String>(),
        "second"
    );
    assert_eq!(reopened.state(0).pending_upload, 2);
}
struct DiskFailure;
impl Filesystem for DiskFailure {
    fn sync_file(&mut self, _: &fs::File) -> io::Result<()> {
        Err(io::Error::other("prepared disk failure"))
    }
}
#[test]
fn file_sync_failure_is_not_saved_and_retains_private_draft() {
    let f = Fixture::new();
    let (mut session, _) = f.local("original", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "new".into();
    let error = commands
        .execute_with(
            &mut session,
            Command::Save,
            &mut draft,
            0,
            &f.root,
            &mut DiskFailure,
        )
        .unwrap_err();
    let CommandError::File(failure) = error else {
        panic!("expected file failure")
    };
    assert_eq!(failure.kind, FailureKind::Io);
    assert_eq!(
        fs::read_to_string(failure.edit.buffer.recovery().buffer()).unwrap(),
        "new"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("source.txt")).unwrap(),
        "original"
    );
    assert_eq!(draft, "new");
    assert_eq!(text(&session), "new");
    let result = execute(&mut commands, &mut session, Command::Save, &mut draft, &f).unwrap();
    assert!(!result.changed);
    assert_eq!(result.file, Some(files::SaveState::Saved));
}
#[test]
fn stale_file_rejects_publication_after_qualified_local_commit() {
    let f = Fixture::new();
    let (mut session, _) = f.local("original", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "local".into();
    fs::write(f.root.join("source.txt"), "external").unwrap();
    assert!(matches!(
        execute(&mut commands, &mut session, Command::Save, &mut draft, &f),
        Err(CommandError::File(_))
    ));
    assert_eq!(
        fs::read_to_string(f.root.join("source.txt")).unwrap(),
        "external"
    );
    assert_eq!(text(&session), "local");
    assert_eq!(draft, "local");
}
fn disk_admission_case(boundary: FaultBoundary) {
    let f = Fixture::new();
    let (mut session, store) = f.local("original", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "new".into();
    store
        .inject_once(InjectedFault {
            boundary,
            error: PortError::Quota,
        })
        .unwrap();
    assert!(execute(&mut commands, &mut session, Command::Commit, &mut draft, &f).is_err());
    assert_eq!(
        session.service.session().state(0).readiness,
        Readiness::RecoveryRequired
    );
    assert_eq!(
        session.service.session().state(0).local,
        LocalState::Unsaved
    );
    assert_eq!(draft, "new");
    assert_eq!(session.source, "original");
    assert!(execute(&mut commands, &mut session, Command::Commit, &mut draft, &f).is_err());
    execute(
        &mut commands,
        &mut session,
        Command::Recover,
        &mut draft,
        &f,
    )
    .unwrap();
    assert_eq!(session.source, "new");
    assert_eq!(store.pending(&f.id).unwrap().len(), 1);
    assert!(
        !execute(&mut commands, &mut session, Command::Commit, &mut draft, &f)
            .unwrap()
            .changed
    );
    execute(&mut commands, &mut session, Command::Undo, &mut draft, &f).unwrap();
    assert_eq!(draft, "original");
}
#[test]
fn journal_before_write_failure_requires_exact_recovery() {
    disk_admission_case(FaultBoundary::BeforeWrite);
}
#[test]
fn journal_lost_ack_does_not_double_commit() {
    disk_admission_case(FaultBoundary::AfterCommit);
}
#[test]
fn history_storage_failure_recovers_the_same_undo_and_redo() {
    let f = Fixture::new();
    let (mut session, store) = f.local("original", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "new".into();
    execute(&mut commands, &mut session, Command::Commit, &mut draft, &f).unwrap();
    for command in [Command::Undo, Command::Redo] {
        store
            .inject_once(InjectedFault {
                boundary: FaultBoundary::BeforeCommit,
                error: PortError::Quota,
            })
            .unwrap();
        assert!(execute(&mut commands, &mut session, command, &mut draft, &f).is_err());
        execute(
            &mut commands,
            &mut session,
            Command::Recover,
            &mut draft,
            &f,
        )
        .unwrap();
        assert_eq!(
            draft,
            if command == Command::Undo {
                "original"
            } else {
                "new"
            }
        );
    }
    assert_eq!(store.pending(&f.id).unwrap().len(), 3);
}
#[test]
fn terminal_command_dispatch_uses_real_commands_and_status() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    let f = Fixture::new();
    let (mut session, _) = f.local("original", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut view = View::from_session(&session, 0);
    view.source = "new".into();
    for (key, expected) in [('s', "new"), ('z', "original"), ('y', "new")] {
        assert!(dispatch_command(
            &Event::Key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL)),
            &mut view,
            &mut session,
            Some(&mut commands),
            0,
            &f.root
        ));
        assert_eq!(view.source, expected);
        assert!(!view.draft_changed);
        assert!(view.status.contains("Committed(ProcessCrash)"));
        assert!(view.status.contains("remote Unknown"));
    }
    let repeat = Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
        KeyEventKind::Repeat,
    ));
    assert!(!dispatch_command(
        &repeat,
        &mut view,
        &mut session,
        Some(&mut commands),
        0,
        &f.root
    ));
}

#[test]
fn recovering_failed_commit_preserves_a_newer_input_draft() {
    let f = Fixture::new();
    let (mut session, store) = f.local("original", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "submitted".into();
    store
        .inject_once(InjectedFault {
            boundary: FaultBoundary::BeforeWrite,
            error: PortError::Quota,
        })
        .unwrap();
    assert!(execute(&mut commands, &mut session, Command::Commit, &mut draft, &f).is_err());
    draft = "submitted plus newer typing".into();
    execute(
        &mut commands,
        &mut session,
        Command::Recover,
        &mut draft,
        &f,
    )
    .unwrap();
    assert_eq!(session.source, "submitted");
    assert_eq!(draft, "submitted plus newer typing");
    assert_eq!(store.pending(&f.id).unwrap().len(), 1);
    execute(&mut commands, &mut session, Command::Commit, &mut draft, &f).unwrap();
    execute(&mut commands, &mut session, Command::Undo, &mut draft, &f).unwrap();
    assert_eq!(draft, "submitted");
}

#[test]
fn opaque_module_rejects_destructive_source_edit_and_retains_checkpoint() {
    let f = Fixture::new();
    let (mut session, store) = f.local_fixture("opaque\n", false, true);
    let before = session.service.session().document().checkpoint().bytes;
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "replace opaque\n".into();
    assert!(execute(&mut commands, &mut session, Command::Save, &mut draft, &f).is_err());
    assert_eq!(session.source, "opaque\n");
    assert_eq!(draft, "replace opaque\n");
    assert_eq!(
        session.service.session().document().checkpoint().bytes,
        before
    );
    assert!(store.pending(&f.id).unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(f.root.join("source.md")).unwrap(),
        "opaque\n"
    );
}

// The host owns transport authentication and receipts; the presenter shares its
// native store. Exercise the actual Session/Transport/receipt persistence path,
// then refresh the presenter through the existing recovery API. No live host.
fn host_session(
    f: &Fixture,
    store: Arc<NativeStore>,
    receipts: ReceiptCapability,
) -> Session<Arc<NativeStore>> {
    let mut host = Session::open(store, &f.id, CachedPermission::LOCAL).unwrap();
    host.attach_transport(
        Box::new(
            editac_session::ports::memory::PreparedTransport::new(
                f.id.clone(),
                ConnectionState::Authorized,
                TransportCapabilities {
                    receipts,
                    max_frame_bytes: 1048576,
                    max_queue_bytes: 1048576,
                    max_queue_frames: 100,
                },
            )
            .unwrap(),
        ),
        100,
        0,
    )
    .unwrap();
    host
}

#[test]
fn local_save_send_and_matching_remote_receipt_have_distinct_statuses() {
    let f = Fixture::new();
    let (mut session, store) = f.local("original", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "saved locally".into();
    let saved = execute(&mut commands, &mut session, Command::Save, &mut draft, &f).unwrap();
    assert_eq!(saved.file, Some(files::SaveState::Saved));
    assert_eq!(saved.state.remote, RemoteState::Unknown);
    assert_eq!(saved.state.pending_upload, 1);
    let update = store.pending(&f.id).unwrap()[0].id().clone();
    let mut host = host_session(&f, store.clone(), ReceiptCapability::DurableV1);
    assert_eq!(host.flush_pending(0).unwrap(), 1);
    assert_eq!(host.state(0).remote, RemoteState::SentDurabilityUnknown);
    assert_eq!(store.pending(&f.id).unwrap().len(), 1);
    let mut unknown = update.clone();
    unknown.sequence += 100;
    assert_eq!(
        host.accept_receipt(
            DurableReceipt {
                protocol: ReceiptCapability::DurableV1,
                update: unknown
            },
            0
        )
        .unwrap(),
        ReceiptResult::UnknownUpdate
    );
    assert_eq!(
        host.accept_receipt(
            DurableReceipt {
                protocol: ReceiptCapability::Unknown,
                update: update.clone()
            },
            0
        )
        .unwrap(),
        ReceiptResult::CapabilityUnavailable
    );
    assert_eq!(host.state(0).pending_upload, 1);
    assert_eq!(
        host.accept_receipt(
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
        host.accept_receipt(
            DurableReceipt {
                protocol: ReceiptCapability::DurableV1,
                update
            },
            0
        )
        .unwrap(),
        ReceiptResult::RemoteCommitted
    );
    draft = "newer unpublished draft".into();
    let refreshed = execute(
        &mut commands,
        &mut session,
        Command::Recover,
        &mut draft,
        &f,
    )
    .unwrap();
    assert_eq!(refreshed.state.remote, RemoteState::RemotelyCommitted);
    assert_eq!(refreshed.state.pending_upload, 0);
    assert_eq!(draft, "newer unpublished draft");
    assert_eq!(text(&session), "saved locally");
    execute(&mut commands, &mut session, Command::Commit, &mut draft, &f).unwrap();
    assert_eq!(
        session.service.session().state(0).remote,
        RemoteState::Unknown
    );
    assert_eq!(session.service.session().state(0).pending_upload, 1);
}

#[test]
fn failed_receipt_persistence_keeps_outbox_until_a_durable_retry() {
    let f = Fixture::new();
    let (mut session, store) = f.local("original", true);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut draft = "committed".into();
    execute(&mut commands, &mut session, Command::Commit, &mut draft, &f).unwrap();
    let mut host = host_session(&f, store.clone(), ReceiptCapability::DurableV1);
    let receipt = DurableReceipt {
        protocol: ReceiptCapability::DurableV1,
        update: store.pending(&f.id).unwrap()[0].id().clone(),
    };
    store
        .inject_once(InjectedFault {
            boundary: FaultBoundary::BeforeCommit,
            error: PortError::Quota,
        })
        .unwrap();
    assert!(host.accept_receipt(receipt.clone(), 0).is_err());
    assert_eq!(store.pending(&f.id).unwrap().len(), 1);
    assert_ne!(host.state(0).remote, RemoteState::RemotelyCommitted);
    assert_eq!(
        session.service.session().state(0).remote,
        RemoteState::Unknown
    );
    host.recover().unwrap();
    assert_eq!(
        host.accept_receipt(receipt, 0).unwrap(),
        ReceiptResult::RemoteCommitted
    );
    let refreshed = execute(
        &mut commands,
        &mut session,
        Command::Recover,
        &mut draft,
        &f,
    )
    .unwrap();
    assert_eq!(refreshed.state.remote, RemoteState::RemotelyCommitted);
    assert_eq!(
        refreshed.state.local,
        LocalState::Committed(Durability::ProcessCrash)
    );
    assert!(store.pending(&f.id).unwrap().is_empty());
}

#[test]
fn unavailable_source_binding_stays_readonly_after_command_dispatch() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let f = Fixture::new();
    let (mut session, _) = f.local("source", true);
    let mut view = View::from_session(&session, 0);
    view.input.as_mut().unwrap().writable = false;
    assert!(dispatch_command(
        &Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        &mut view,
        &mut session,
        None,
        0,
        &f.root,
    ));
    assert!(!view.input.as_ref().unwrap().writable);
    assert_eq!(view.source, "source");
    assert_eq!(session.source, "source");
    assert!(view.input_message.as_ref().unwrap().contains("unavailable"));
}
