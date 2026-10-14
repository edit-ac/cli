//! TUI-1.10 canonical shared native-store/command/event/lifecycle integration.
//! Prepared transport only; no credentials, network or provider calls.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use editac_cli::{
    engine::document::{CreationPermit, DocumentIdentity},
    markdown::reconcile::{ConflictKind, SourceMode},
    reconcile::LocalBaseline,
    storage::{Boundary, IoHooks, RecordKind},
    target::{self, ResolveOptions},
    tui::{
        commands::{Command, Commands},
        conflict::{ConflictView, Pane, Reason},
        lifecycle::{self, Event as Life, RecoveryOwner},
        session::{Representation, Request, TerminalSession},
        terminal::{Mode, TerminalIo, TerminalOwner, run_session},
        view::View,
    },
};
use editac_session::{
    edit_service::{EditService, HostPolicy},
    ports::{
        memory::{FaultBoundary, InjectedFault},
        *,
    },
    session::{AuthState, CachedPermission, Readiness, RemoteState, Session},
    store::NativeStore,
};
use std::{
    collections::VecDeque,
    fs, io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Harness {
    root: PathBuf,
    id: DocumentIdentity,
    store: Option<Arc<NativeStore>>,
    session: Option<TerminalSession<Arc<NativeStore>>>,
}
impl Harness {
    fn new(source: &str, literal: bool) -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::var_os("TUI_RECOVERY_EVIDENCE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        fs::create_dir_all(&base).unwrap();
        let root = base.join(format!("tui-recovery-{}-{n}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let id = DocumentIdentity::v5(
            format!("ae000000-0000-5000-8000-{n:012x}"),
            format!("ae000000-0000-4000-8000-{n:012x}"),
        )
        .unwrap();
        let path = root.join(if literal { "source.txt" } else { "source.md" });
        fs::write(&path, source).unwrap();
        let (doc, baseline) = LocalBaseline::import(
            target::resolve(&path, ResolveOptions::default()).unwrap(),
            CreationPermit::authorize_new(id.clone()).unwrap(),
        )
        .unwrap();
        let store = Arc::new(
            NativeStore::create_new(
                root.join("state.sqlite"),
                id.clone(),
                StoreLimits::default(),
            )
            .unwrap(),
        );
        store
            .publish(0, CheckpointRecord::new(doc.checkpoint(), vec![]).unwrap())
            .unwrap();
        let mut session = TerminalSession {
            service: EditService::new(
                Session::open(store.clone(), &id, CachedPermission::LOCAL).unwrap(),
                HostPolicy::default(),
            ),
            source: source.into(),
            mode: if literal {
                SourceMode::Literal
            } else {
                SourceMode::Markdown(Default::default())
            },
            representation: Representation::ExactSource,
            local_baseline: Some(baseline),
            recovery: None,
        };
        let recovery = RecoveryOwner::for_store(store.path(), &id).unwrap();
        recovery
            .persist(&session, None, &View::from_session(&session, 0))
            .unwrap();
        session.recovery = Some(recovery);
        Self {
            root,
            id,
            store: Some(store),
            session: Some(session),
        }
    }
    fn session(&mut self) -> &mut TerminalSession<Arc<NativeStore>> {
        self.session.as_mut().unwrap()
    }
    fn save_snapshot(&mut self, commands: &Commands, view: &View) {
        let s = self.session();
        s.recovery
            .as_ref()
            .unwrap()
            .persist(s, Some(commands), view)
            .unwrap();
    }
    fn restart(&mut self) -> TerminalSession<NativeStore> {
        let literal = matches!(self.session().mode, SourceMode::Literal);
        self.session.take();
        self.store.take();
        let args = [
            "tui".into(),
            self.root
                .join(if literal { "source.txt" } else { "source.md" })
                .into_os_string(),
            "--store".into(),
            self.root.join("state.sqlite").into_os_string(),
        ];
        Request::parse(&args).unwrap().open().unwrap()
    }
    fn edit(&mut self, source: &str) -> (Commands, View) {
        let root = self.root.clone();
        let s = self.session();
        let mut c = Commands::capture(s, 0).unwrap();
        let mut v = View::from_session(s, 0);
        v.source = source.into();
        c.execute(s, Command::Commit, &mut v.source, 0, &root)
            .unwrap();
        (c, v)
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.session.take();
        self.store.take();
        if std::env::var_os("TUI_RECOVERY_EVIDENCE_ROOT").is_none() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}
fn quit() -> Event {
    key(KeyCode::Char('q'), KeyModifiers::CONTROL)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Modes {
    raw: bool,
    alternate: bool,
    paste: bool,
    focus: bool,
    cursor: bool,
}
impl Default for Modes {
    fn default() -> Self {
        Self {
            raw: false,
            alternate: false,
            paste: false,
            focus: false,
            cursor: true,
        }
    }
}
#[derive(Default)]
struct Trace {
    modes: Modes,
    frames: Vec<View>,
    disposals: usize,
    suspends: usize,
    reads: usize,
    draws: usize,
    starts: usize,
    mode_failure: Option<Mode>,
}
struct Io {
    trace: Arc<Mutex<Trace>>,
    events: VecDeque<Event>,
    lifecycle: VecDeque<Life>,
    fail: &'static str,
}
impl Io {
    fn new(events: Vec<Event>, fail: &'static str) -> Self {
        Self {
            trace: Default::default(),
            events: events.into(),
            lifecycle: VecDeque::new(),
            fail,
        }
    }
}
impl TerminalIo for Io {
    fn raw_enabled(&mut self) -> io::Result<bool> {
        if self.fail == "raw" {
            return Err(io::Error::other("raw"));
        }
        Ok(self.trace.lock().unwrap().modes.raw)
    }
    fn mode(&mut self, mode: Mode) -> io::Result<()> {
        let mut t = self.trace.lock().unwrap();
        match mode {
            Mode::Raw(v) => t.modes.raw = v,
            Mode::Alternate(v) => t.modes.alternate = v,
            Mode::Paste(v) => t.modes.paste = v,
            Mode::Focus(v) => t.modes.focus = v,
            Mode::CursorVisible(v) => t.modes.cursor = v,
        };
        if t.mode_failure == Some(mode) {
            t.mode_failure = None;
            return Err(io::Error::other("mode"));
        }
        Ok(())
    }
    fn size(&mut self) -> io::Result<(u16, u16)> {
        if self.fail == "size" {
            return Err(io::Error::other("size"));
        }
        Ok((60, 12))
    }
    fn initialize(&mut self, _: (u16, u16)) -> io::Result<()> {
        let mut trace = self.trace.lock().unwrap();
        trace.starts += 1;
        if self.fail == "init" || (self.fail == "resume_init" && trace.starts > 1) {
            return Err(io::Error::other("init"));
        }
        Ok(())
    }
    fn resize(&mut self, _: (u16, u16)) -> io::Result<()> {
        Ok(())
    }
    fn draw(&mut self, v: &View) -> io::Result<()> {
        let mut t = self.trace.lock().unwrap();
        t.draws += 1;
        t.frames.push(v.clone());
        if self.fail == "draw" || (self.fail == "draw_after" && t.draws > 1) {
            return Err(io::Error::other("draw"));
        }
        Ok(())
    }
    fn read(&mut self, _: Duration) -> io::Result<Option<Event>> {
        self.trace.lock().unwrap().reads += 1;
        if let Some(e) = self.events.pop_front() {
            return Ok(Some(e));
        }
        if self.fail == "read" {
            return Err(io::Error::other("read"));
        }
        Err(io::Error::other("fixture exhausted"))
    }
    fn lifecycle(&mut self) -> io::Result<Option<Life>> {
        Ok(self.lifecycle.pop_front())
    }
    fn suspend(&mut self) -> io::Result<()> {
        let mut t = self.trace.lock().unwrap();
        assert_eq!(t.modes, Modes::default());
        t.suspends += 1;
        if self.fail == "suspend" {
            return Err(io::Error::other("suspend"));
        }
        Ok(())
    }
    fn dispose(&mut self) {
        self.trace.lock().unwrap().disposals += 1;
    }
}
fn run(
    h: &mut Harness,
    events: Vec<Event>,
    failure: &'static str,
) -> (io::Result<()>, Arc<Mutex<Trace>>) {
    let backend = Io::new(events, failure);
    let trace = backend.trace.clone();
    let mut terminal = TerminalOwner::new(backend);
    let result = run_session(h.session(), 0, &mut terminal, || 0);
    terminal.shutdown().unwrap();
    terminal.shutdown().unwrap();
    assert_eq!(trace.lock().unwrap().modes, Modes::default());
    (result, trace)
}
fn view(s: &TerminalSession<NativeStore>) -> View {
    let mut v = View::from_session(s, 0);
    s.recovery
        .as_ref()
        .unwrap()
        .restore_view(s, &mut v)
        .unwrap();
    v
}

#[test]
fn normal_quit_recovers_dirty_unicode_and_selection() {
    let mut h = Harness::new("literal\r\n", true);
    let (result, trace) = run(
        &mut h,
        vec![Event::Paste("👩‍💻é界\tאב\r\n".into()), quit()],
        "",
    );
    result.unwrap();
    let expected = trace.lock().unwrap().frames.last().unwrap().source.clone();
    let s = h.restart();
    let v = view(&s);
    assert_eq!(v.source, expected);
    assert_eq!(s.source, "literal\r\n");
    assert!(v.draft_changed);
    assert!(v.input.unwrap().caret.0 > 0);
    assert_eq!(s.service.session().state(0).remote, RemoteState::Unknown);
}
#[test]
fn escape_quit_restores_terminal_without_mutation() {
    let mut h = Harness::new("body", true);
    run(&mut h, vec![key(KeyCode::Esc, KeyModifiers::NONE)], "")
        .0
        .unwrap();
    let s = h.restart();
    assert_eq!(view(&s).source, "body");
}
#[test]
fn control_c_interrupt_recovers_draft() {
    let mut h = Harness::new("body", true);
    run(
        &mut h,
        vec![
            Event::Paste("draft".into()),
            key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ],
        "",
    )
    .0
    .unwrap();
    let s = h.restart();
    assert_eq!(view(&s).source, "draftbody");
}
macro_rules! io_failure {
    ($name:ident,$failure:literal) => {
        #[test]
        fn $name() {
            let mut h = Harness::new("body", true);
            let result = run(&mut h, vec![Event::Paste("draft".into())], $failure).0;
            assert!(result.is_err());
            let s = h.restart();
            let expected = if matches!($failure, "read" | "draw_after") {
                "draftbody"
            } else {
                "body"
            };
            assert_eq!(view(&s).source, expected);
            assert_eq!(s.service.session().state(0).auth, AuthState::Detached);
        }
    };
}
io_failure!(startup_raw_failure, "raw");
io_failure!(startup_size_failure, "size");
io_failure!(startup_initialization_failure, "init");
io_failure!(initial_draw_failure, "draw");
io_failure!(draw_failure_after_input, "draw_after");
io_failure!(read_failure_after_input, "read");
#[test]
fn startup_mode_partial_failure_restores_every_inverse() {
    let mut h = Harness::new("body", true);
    let io = Io::new(vec![quit()], "");
    let trace = io.trace.clone();
    trace.lock().unwrap().mode_failure = Some(Mode::Alternate(true));
    let mut terminal = TerminalOwner::new(io);
    assert!(run_session(h.session(), 0, &mut terminal, || 0).is_err());
    assert_eq!(trace.lock().unwrap().modes, Modes::default());
    let s = h.restart();
    assert_eq!(s.source, "body");
}
#[test]
fn cleanup_failure_retries_idempotently() {
    let mut h = Harness::new("body", true);
    let io = Io::new(vec![quit()], "");
    let trace = io.trace.clone();
    trace.lock().unwrap().mode_failure = Some(Mode::Alternate(false));
    let mut terminal = TerminalOwner::new(io);
    assert!(run_session(h.session(), 0, &mut terminal, || 0).is_err());
    terminal.shutdown().unwrap();
    terminal.shutdown().unwrap();
    assert_eq!(trace.lock().unwrap().modes, Modes::default());
}
#[test]
fn commit_restart_retains_exact_markdown_and_pending_outbox() {
    let mut h = Harness::new("# Heading\r\n\r\n- item\r\n", false);
    let (c, v) = h.edit("## Heading\r\n\r\n- item changed\r\n");
    let revision = h.session().service.read(0).unwrap().revision;
    let pending = h.store.as_ref().unwrap().pending(&h.id).unwrap();
    assert!(!pending.is_empty());
    h.save_snapshot(&c, &v);
    let s = h.restart();
    assert_eq!(s.source, v.source);
    assert_eq!(s.service.read(0).unwrap().revision, revision);
    assert_eq!(s.service.session().state(0).pending_upload, pending.len());
    assert_eq!(s.service.session().state(0).remote, RemoteState::Unknown);
    assert_eq!(view(&s).source, v.source);
}
#[test]
fn save_restart_updates_guarded_file_baseline() {
    let mut h = Harness::new("body", true);
    let root = h.root.clone();
    let s = h.session();
    let mut c = Commands::capture(s, 0).unwrap();
    let mut v = View::from_session(s, 0);
    v.source = "saved spelling\r\n".into();
    c.execute(s, Command::Save, &mut v.source, 0, &root)
        .unwrap();
    h.save_snapshot(&c, &v);
    assert_eq!(
        fs::read_to_string(h.root.join("source.txt")).unwrap(),
        v.source
    );
    let s = h.restart();
    assert_eq!(s.source, v.source);
}
#[test]
fn no_op_restart_keeps_source_spelling() {
    let mut h = Harness::new("# Heading\r\n\r\n", false);
    let (c, v) = h.edit("# Heading\r\n\r\n");
    assert_eq!(h.session().service.session().state(0).pending_upload, 0);
    h.save_snapshot(&c, &v);
    let s = h.restart();
    assert_eq!(s.source, "# Heading\r\n\r\n");
}
#[test]
fn local_restart_refuses_another_or_changed_file() {
    let mut h = Harness::new("body", true);
    h.session.take();
    fs::write(h.root.join("source.txt"), "external change").unwrap();
    assert!(
        lifecycle::reopen_local(
            &h.root.join("source.txt"),
            ResolveOptions::default(),
            &h.root.join("state.sqlite")
        )
        .is_err()
    );
    let other = h.root.join("other.txt");
    fs::write(&other, "body").unwrap();
    assert!(
        lifecycle::reopen_local(
            &other,
            ResolveOptions::default(),
            &h.root.join("state.sqlite")
        )
        .is_err()
    );
    assert!(h.root.join("state.sqlite.tui-recovery/draft.json").exists());
}
#[test]
fn one_recovery_owner_prevents_concurrent_snapshot_replacement() {
    let h = Harness::new("body", true);
    assert!(RecoveryOwner::for_store(&h.root.join("state.sqlite"), &h.id).is_err());
}
#[test]
fn corrupt_recovery_never_reinitializes_store() {
    let mut h = Harness::new("body", true);
    h.session.take();
    let path = h.root.join("state.sqlite.tui-recovery/draft.json");
    fs::write(&path, "corrupt").unwrap();
    assert!(
        lifecycle::reopen_local(
            &h.root.join("source.txt"),
            ResolveOptions::default(),
            &h.root.join("state.sqlite")
        )
        .is_err()
    );
    assert_eq!(h.store.as_ref().unwrap().pending(&h.id).unwrap().len(), 0);
    assert_eq!(fs::read_to_string(path).unwrap(), "corrupt");
}

fn conflict_case(cancelled: bool) {
    let mut h = Harness::new("base", true);
    let c = Commands::capture(h.session(), 0).unwrap();
    let mut v = View::from_session(h.session(), 0);
    v.source = "local draft".into();
    let r = c.revision().clone();
    let mut conflict = ConflictView::capture(
        "base",
        "local draft",
        "base",
        &r,
        r.clone(),
        Reason::Mapping(ConflictKind::DeletedTarget),
    )
    .unwrap();
    conflict.choose(Pane::Resolution);
    conflict.resolution = "selected 👩‍💻 resolution".into();
    if cancelled {
        conflict.cancel();
    }
    v.conflict = Some(conflict);
    h.save_snapshot(&c, &v);
    let s = h.restart();
    let recovered = view(&s);
    let conflict = recovered.conflict.unwrap();
    assert_eq!(conflict.base, "base");
    assert_eq!(conflict.local, "local draft");
    assert_eq!(conflict.current, "base");
    assert_eq!(conflict.resolution, "selected 👩‍💻 resolution");
    assert_eq!(conflict.open, !cancelled);
    assert!(!conflict.prepared());
    assert_eq!(
        conflict.reason,
        Reason::Mapping(ConflictKind::DeletedTarget)
    );
    assert_eq!(recovered.source, "local draft");
}
#[test]
fn unresolved_conflict_restart_retains_all_sources() {
    conflict_case(false)
}
#[test]
fn cancelled_conflict_restart_retains_resolution() {
    conflict_case(true)
}
#[test]
fn newer_remote_revision_on_restart_reopens_stale_draft_conflict() {
    let mut h = Harness::new("base", true);
    let (c, mut v) = h.edit("current");
    v.source = "dirty candidate".into();
    h.save_snapshot(&c, &v);
    // Actual independent replica and journal; committed remote update enters the same live owner.
    let checkpoint = h.session().service.session().recovery_export().checkpoint;
    let peer_store = Arc::new(
        NativeStore::create_new(
            h.root.join("peer.sqlite"),
            h.id.clone(),
            StoreLimits::default(),
        )
        .unwrap(),
    );
    peer_store
        .publish(0, CheckpointRecord::new(checkpoint, vec![]).unwrap())
        .unwrap();
    let mut peer = EditService::new(
        Session::open(peer_store.clone(), &h.id, CachedPermission::LOCAL).unwrap(),
        HostPolicy::default(),
    );
    let read = peer.read(0).unwrap();
    let owners = read
        .snapshot
        .visible
        .iter()
        .filter(|n| n.kind == "block")
        .map(|n| n.id.clone())
        .collect::<Vec<_>>();
    let binding = peer
        .capture_source("current", SourceMode::Literal, &owners, 0)
        .unwrap();
    let prepared = peer
        .prepare_source(
            &binding,
            &read.revision,
            "remote current",
            &editac_cli::engine::commands::text::TextOrigin::new("peer").unwrap(),
            0,
        )
        .unwrap();
    peer.apply(prepared, 0).unwrap();
    h.store
        .as_ref()
        .unwrap()
        .claim_writer(peer.session().document().writer_id())
        .unwrap();
    let entry = peer_store.pending(&h.id).unwrap().pop().unwrap();
    h.session().service.receive_peer(entry).unwrap();
    let s = h.restart();
    let recovered = view(&s);
    assert_eq!(recovered.source, "dirty candidate");
    let conflict = recovered.conflict.unwrap();
    assert_eq!(conflict.base, "current");
    assert_eq!(conflict.current, "remote current");
    assert!(!conflict.prepared());
}

struct TransportProbe {
    fail: bool,
    drops: Arc<AtomicUsize>,
    sends: Arc<AtomicUsize>,
}
impl Drop for TransportProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Transport for TransportProbe {
    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities {
            max_frame_bytes: 1_048_576,
            max_queue_bytes: 1_048_576,
            max_queue_frames: 64,
            receipts: ReceiptCapability::Unknown,
        }
    }
    fn state(&self) -> ConnectionState {
        ConnectionState::Authorized
    }
    fn receive(&mut self) -> Result<Option<TransportFrame>, PortError> {
        Ok(None)
    }
    fn send(&mut self, _: TransportFrame) -> Result<(), PortError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            Err(PortError::Unavailable)
        } else {
            Ok(())
        }
    }
}
fn attach(h: &mut Harness, fail: bool) -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let drops = Arc::new(AtomicUsize::new(0));
    let sends = Arc::new(AtomicUsize::new(0));
    h.session()
        .service
        .attach_transport(
            Box::new(TransportProbe {
                fail,
                drops: drops.clone(),
                sends: sends.clone(),
            }),
            100,
            0,
        )
        .unwrap();
    (drops, sends)
}
#[test]
fn successful_flush_disposes_transport_and_retains_unreceipted_outbox() {
    let mut h = Harness::new("base", true);
    let (c, v) = h.edit("changed");
    h.save_snapshot(&c, &v);
    let (drops, sends) = attach(&mut h, false);
    run(&mut h, vec![quit()], "").0.unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(sends.load(Ordering::SeqCst) > 0);
    assert_eq!(
        h.session().service.session().state(0).remote,
        RemoteState::SentDurabilityUnknown
    );
    let s = h.restart();
    assert_eq!(s.service.session().state(0).pending_upload, 1);
    assert_eq!(s.service.session().state(0).remote, RemoteState::Unknown);
}
#[test]
fn failed_flush_requires_explicit_recoverable_exit() {
    let mut h = Harness::new("base", true);
    let (c, v) = h.edit("changed");
    h.save_snapshot(&c, &v);
    let (drops, sends) = attach(&mut h, true);
    let (result, trace) = run(
        &mut h,
        vec![quit(), key(KeyCode::Char('e'), KeyModifiers::CONTROL)],
        "",
    );
    result.unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(sends.load(Ordering::SeqCst), 1);
    assert!(trace.lock().unwrap().frames.iter().any(|v| {
        v.input_message
            .as_ref()
            .is_some_and(|m| m.contains("Flush failed"))
    }));
    let s = h.restart();
    assert_eq!(s.source, "changed");
    assert_eq!(s.service.session().state(0).pending_upload, 1);
}
#[test]
fn failed_flush_escape_cancels_and_retry_is_explicit() {
    let mut h = Harness::new("base", true);
    let (c, v) = h.edit("changed");
    h.save_snapshot(&c, &v);
    let (_, sends) = attach(&mut h, true);
    let (result, trace) = run(
        &mut h,
        vec![
            quit(),
            key(KeyCode::Esc, KeyModifiers::NONE),
            quit(),
            key(KeyCode::Char('e'), KeyModifiers::CONTROL),
        ],
        "",
    );
    result.unwrap();
    assert_eq!(sends.load(Ordering::SeqCst), 2);
    assert!(
        trace
            .lock()
            .unwrap()
            .frames
            .iter()
            .any(|v| v.input_message.is_none())
    );
}
#[test]
fn interrupt_after_failed_flush_preserves_recovery_and_detaches() {
    let mut h = Harness::new("base", true);
    let (c, v) = h.edit("changed");
    h.save_snapshot(&c, &v);
    let (drops, _) = attach(&mut h, true);
    run(
        &mut h,
        vec![key(KeyCode::Char('c'), KeyModifiers::CONTROL)],
        "",
    )
    .0
    .unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let s = h.restart();
    assert_eq!(s.source, "changed");
    assert_eq!(s.service.session().state(0).pending_upload, 1);
}
#[test]
fn startup_failure_disposes_attached_transport() {
    let mut h = Harness::new("base", true);
    let (drops, _) = attach(&mut h, false);
    assert!(run(&mut h, vec![], "init").0.is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
fn pending_restart(boundary: FaultBoundary) {
    let mut h = Harness::new("base", true);
    let mut c = Commands::capture(h.session(), 0).unwrap();
    let mut v = View::from_session(h.session(), 0);
    v.source = "admitted 👩‍💻".into();
    h.store
        .as_ref()
        .unwrap()
        .inject_once(InjectedFault {
            boundary,
            error: PortError::CommitUnknown,
        })
        .unwrap();
    let root = h.root.clone();
    assert!(
        c.execute(h.session(), Command::Commit, &mut v.source, 0, &root)
            .is_err()
    );
    assert_eq!(
        h.session().service.session().state(0).readiness,
        Readiness::RecoveryRequired
    );
    h.save_snapshot(&c, &v);
    let s = h.restart();
    assert_eq!(s.source, "admitted 👩‍💻");
    assert_eq!(s.service.session().state(0).pending_upload, 1);
    assert_eq!(s.service.session().state(0).remote, RemoteState::Unknown);
    assert_eq!(view(&s).source, "admitted 👩‍💻");
}
#[test]
fn restart_settles_exact_update_before_append_boundary() {
    pending_restart(FaultBoundary::BeforeWrite)
}
#[test]
fn restart_settles_exact_update_after_append_boundary() {
    pending_restart(FaultBoundary::AfterCommit)
}
struct Fault(Boundary);
impl IoHooks for Fault {
    fn boundary(&self, record: RecordKind, boundary: Boundary) -> io::Result<()> {
        if record == RecordKind::Draft && boundary == self.0 {
            Err(io::Error::other("draft boundary"))
        } else {
            Ok(())
        }
    }
}
fn draft_boundary(boundary: Boundary, published: bool) {
    let mut h = Harness::new("base", true);
    let c = Commands::capture(h.session(), 0).unwrap();
    let mut v = View::from_session(h.session(), 0);
    v.source = "new draft".into();
    let s = h.session();
    assert!(
        s.recovery
            .as_ref()
            .unwrap()
            .persist_with(s, Some(&c), &v, &Fault(boundary))
            .is_err()
    );
    let s = h.restart();
    assert_eq!(
        view(&s).source,
        if published { "new draft" } else { "base" }
    );
    assert_eq!(s.source, "base");
}
#[test]
fn draft_failure_before_write_retains_previous() {
    draft_boundary(Boundary::BeforeWrite, false)
}
#[test]
fn draft_failure_after_file_sync_retains_previous() {
    draft_boundary(Boundary::AfterFileSync, false)
}
#[test]
fn draft_failure_after_publish_is_recovered_without_save_claim() {
    draft_boundary(Boundary::AfterPublish, true)
}
#[test]
fn draft_failure_after_directory_sync_is_recovered() {
    draft_boundary(Boundary::AfterDirectorySync, true)
}
#[test]
fn suspend_resume_restores_before_suspend_and_restarts_modes() {
    let mut h = Harness::new("base", true);
    let mut io = Io::new(vec![Event::Paste("draft".into()), quit()], "");
    io.lifecycle.push_back(Life::Suspend);
    let trace = io.trace.clone();
    let mut terminal = TerminalOwner::new(io);
    run_session(h.session(), 0, &mut terminal, || 0).unwrap();
    assert_eq!(trace.lock().unwrap().suspends, 1);
    assert_eq!(trace.lock().unwrap().modes, Modes::default());
    let s = h.restart();
    assert_eq!(view(&s).source, "draftbase");
}
#[test]
fn suspend_failure_keeps_terminal_restored_and_draft() {
    let mut h = Harness::new("base", true);
    let mut io = Io::new(vec![], "suspend");
    io.lifecycle.push_back(Life::Suspend);
    let trace = io.trace.clone();
    let mut terminal = TerminalOwner::new(io);
    assert!(run_session(h.session(), 0, &mut terminal, || 0).is_err());
    assert_eq!(trace.lock().unwrap().modes, Modes::default());
    let s = h.restart();
    assert_eq!(s.source, "base");
}
#[test]
fn readonly_recovery_keeps_draft_but_rejects_mutation() {
    let mut h = Harness::new("base", true);
    h.session().service.set_permission(CachedPermission {
        writable: false,
        expires_at: None,
    });
    run(
        &mut h,
        vec![
            Event::Paste("forbidden".into()),
            key(KeyCode::Char('s'), KeyModifiers::CONTROL),
            quit(),
        ],
        "",
    )
    .0
    .unwrap();
    let s = h.restart();
    assert_eq!(s.source, "base");
    assert_eq!(view(&s).source, "base");
    assert_eq!(s.service.session().state(0).pending_upload, 0);
}

#[cfg(unix)]
struct SignalIo {
    inner: Io,
    signals: lifecycle::Signals,
    marker: PathBuf,
    resumed: bool,
}
#[cfg(unix)]
impl TerminalIo for SignalIo {
    fn raw_enabled(&mut self) -> io::Result<bool> {
        self.inner.raw_enabled()
    }
    fn mode(&mut self, m: Mode) -> io::Result<()> {
        self.inner.mode(m)
    }
    fn size(&mut self) -> io::Result<(u16, u16)> {
        self.inner.size()
    }
    fn initialize(&mut self, s: (u16, u16)) -> io::Result<()> {
        self.inner.initialize(s)?;
        fs::write(&self.marker, "ready")
    }
    fn resize(&mut self, s: (u16, u16)) -> io::Result<()> {
        self.inner.resize(s)
    }
    fn draw(&mut self, v: &View) -> io::Result<()> {
        self.inner.draw(v)
    }
    fn read(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if self.resumed {
            Ok(Some(quit()))
        } else {
            std::thread::sleep(timeout);
            Ok(None)
        }
    }
    fn lifecycle(&mut self) -> io::Result<Option<Life>> {
        Ok(self.signals.poll())
    }
    fn suspend(&mut self) -> io::Result<()> {
        self.inner.suspend()?;
        fs::write(self.marker.with_extension("suspended"), "terminal restored")?;
        signal_hook::low_level::raise(signal_hook::consts::SIGSTOP)?;
        self.resumed = true;
        Ok(())
    }
    fn dispose(&mut self) {
        self.inner.dispose();
    }
}
#[cfg(unix)]
#[test]
fn signal_child() {
    let Some(case) = std::env::var_os("TUI_RECOVERY_SIGNAL_CASE") else {
        let signals = lifecycle::Signals::new().unwrap();
        assert_eq!(signals.poll(), None);
        drop(signals);
        return;
    };
    let mut h = Harness::new("signal fixture", true);
    let marker = PathBuf::from(std::env::var_os("TUI_RECOVERY_SIGNAL_MARKER").unwrap());
    if case == "kill" {
        let (c, mut v) = h.edit("committed signal fixture");
        v.source = "dirty interrupted draft".into();
        let mut conflict = ConflictView::capture(
            "signal fixture",
            &v.source,
            "committed signal fixture",
            c.opening_revision(),
            c.revision().clone(),
            Reason::Mapping(ConflictKind::Overlap),
        )
        .unwrap();
        conflict.choose(Pane::Resolution);
        conflict.resolution = "selected unresolved resolution".into();
        v.conflict = Some(conflict);
        h.save_snapshot(&c, &v);
        fs::write(marker.with_extension("receipt"), serde_json::to_vec(&serde_json::json!({"root":h.root,"store":h.root.join("state.sqlite"),"target":h.root.join("source.txt")})).unwrap()).unwrap();
    }
    let inner = Io::new(vec![], "");
    let trace = inner.trace.clone();
    let io = SignalIo {
        inner,
        signals: lifecycle::Signals::new().unwrap(),
        marker,
        resumed: false,
    };
    let mut owner = TerminalOwner::new(io);
    run_session(h.session(), 0, &mut owner, || 0).unwrap();
    assert_eq!(trace.lock().unwrap().modes, Modes::default());
    assert_eq!(
        trace.lock().unwrap().suspends,
        if case == "suspend" { 1 } else { 0 }
    );
    let s = h.restart();
    assert_eq!(s.source, "signal fixture");
}
#[cfg(unix)]
fn signal_process(case: &str, signal: &str) {
    use std::{
        process::{Command, Stdio},
        time::Instant,
    };
    let base = std::env::var_os("TUI_RECOVERY_EVIDENCE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let root = base.join(format!("signal-{case}-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let marker = root.join("ready");
    let _ = fs::remove_file(&marker);
    let _ = fs::remove_file(marker.with_extension("suspended"));
    let stdout = fs::File::create(root.join("child.log")).unwrap();
    let stderr = stdout.try_clone().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "signal_child", "--nocapture"])
        .env("TUI_RECOVERY_SIGNAL_CASE", case)
        .env("TUI_RECOVERY_SIGNAL_MARKER", &marker)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap();
    let outcome = (|| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            if child.try_wait().unwrap().is_some() || Instant::now() > deadline {
                return Err("signal child not ready");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            Command::new("/bin/kill")
                .args([signal, &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        if case == "suspend" {
            loop {
                let status = Command::new("/bin/ps")
                    .args(["-o", "state=", "-p", &child.id().to_string()])
                    .output()
                    .unwrap();
                if String::from_utf8_lossy(&status.stdout).contains('T') {
                    break;
                }
                if Instant::now() > deadline {
                    return Err("child never suspended");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                fs::read_to_string(marker.with_extension("suspended")).unwrap(),
                "terminal restored"
            );
            assert!(
                Command::new("/bin/kill")
                    .args(["-CONT", &child.id().to_string()])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                if case == "kill" {
                    use std::os::unix::process::ExitStatusExt;
                    assert_eq!(status.signal(), Some(9));
                    let receipt: serde_json::Value = serde_json::from_slice(
                        &fs::read(marker.with_extension("receipt")).unwrap(),
                    )
                    .unwrap();
                    let target = PathBuf::from(receipt["target"].as_str().unwrap());
                    let store = PathBuf::from(receipt["store"].as_str().unwrap());
                    let session =
                        lifecycle::reopen_local(&target, ResolveOptions::default(), &store)
                            .unwrap();
                    assert_eq!(session.source, "committed signal fixture");
                    assert_eq!(session.service.session().state(0).pending_upload, 1);
                    assert_eq!(
                        session.service.session().state(0).remote,
                        RemoteState::Unknown
                    );
                    let recovered = view(&session);
                    assert_eq!(recovered.source, "dirty interrupted draft");
                    assert_eq!(
                        recovered.conflict.as_ref().unwrap().resolution,
                        "selected unresolved resolution"
                    );
                    assert!(!recovered.conflict.as_ref().unwrap().prepared());
                    drop(session);
                    if std::env::var_os("TUI_RECOVERY_EVIDENCE_ROOT").is_none() {
                        fs::remove_dir_all(receipt["root"].as_str().unwrap()).unwrap();
                    }
                    return Ok(());
                }
                return if status.success() {
                    Ok(())
                } else {
                    Err("signal child failed")
                };
            }
            if Instant::now() > deadline {
                return Err("signal child did not exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    if outcome.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    outcome.unwrap();
    if std::env::var_os("TUI_RECOVERY_EVIDENCE_ROOT").is_none() {
        fs::remove_dir_all(root).unwrap();
    }
}
#[cfg(unix)]
#[test]
fn actual_sigint_reaches_lifecycle_owner() {
    signal_process("interrupt", "-INT")
}
#[cfg(unix)]
#[test]
fn actual_sigtstp_sigcont_restore_and_resume() {
    signal_process("suspend", "-TSTP")
}

#[test]
fn checkpoint_failure_keeps_owner_and_requires_durable_recovery_before_exit() {
    let mut h = Harness::new("base", true);
    let mut recovery = h.session().recovery.take().unwrap();
    let mut c = Commands::capture(h.session(), 0).unwrap();
    let mut v = View::from_session(h.session(), 0);
    v.source = "draft not yet durable".into();
    assert_eq!(
        recovery
            .handle_with(
                Life::Checkpoint,
                h.session(),
                Some(&mut c),
                &mut v,
                0,
                &Fault(Boundary::BeforeWrite)
            )
            .unwrap(),
        lifecycle::Action::Continue
    );
    assert!(recovery.awaiting_exit);
    assert!(!v.input.as_ref().unwrap().writable);
    assert!(
        v.input_message
            .as_ref()
            .unwrap()
            .contains("in-memory draft retained")
    );
    assert_eq!(
        recovery
            .handle_with(
                Life::RecoverableExit,
                h.session(),
                Some(&mut c),
                &mut v,
                0,
                &Fault(Boundary::BeforeWrite)
            )
            .unwrap(),
        lifecycle::Action::Continue
    );
    assert_eq!(
        recovery
            .handle(Life::RecoverableExit, h.session(), Some(&mut c), &mut v, 0)
            .unwrap(),
        lifecycle::Action::Exit
    );
    h.session().recovery = Some(recovery);
    let s = h.restart();
    assert_eq!(view(&s).source, "draft not yet durable");
    assert_eq!(s.source, "base");
}
#[test]
fn cached_hosted_restart_uses_actual_catalog_and_native_cache() {
    let mut h = Harness::new("# Cached\n", false);
    let root = h.root.clone();
    let id = h.id.clone();
    // The same native checkpoint is a host-granted cache. Release the exact-source
    // presentation lease and give the cache a fresh hosted presentation sidecar.
    h.session.take();
    fs::remove_dir_all(lifecycle::recovery_path(&root.join("state.sqlite"))).unwrap();
    let catalog = root.join("catalog.json");
    fs::write(&catalog,serde_json::to_vec(&serde_json::json!({"authorities":[],"entries":[{"locator":format!("ac://{}",id.document_id),"identity":editac_cli::storage::StoredIdentity::from(&id),"store":root.join("state.sqlite"),"access":"allowed","writable":true}]})).unwrap()).unwrap();
    let args = [
        "tui".into(),
        format!("ac://{}", id.document_id).into(),
        "--catalog".into(),
        catalog.into_os_string(),
    ];
    let request = Request::parse(&args).unwrap();
    let mut session = request.open().unwrap();
    assert_eq!(session.representation, Representation::HostedProjection);
    let mut commands = Commands::capture(&session, 0).unwrap();
    let mut v = View::from_session(&session, 0);
    v.source = "# Cached edit\n".into();
    commands
        .execute(&mut session, Command::Commit, &mut v.source, 0, &root)
        .unwrap();
    v.source = "# unsaved cached draft\n".into();
    session
        .recovery
        .as_ref()
        .unwrap()
        .persist(&session, Some(&commands), &v)
        .unwrap();
    drop(session);
    h.store.take();
    let session = request.open().unwrap();
    assert_eq!(session.source, "# Cached edit\n");
    assert_eq!(view(&session).source, "# unsaved cached draft\n");
    assert_eq!(session.service.session().state(0).pending_upload, 1);
    assert_eq!(session.service.session().state(0).auth, AuthState::Detached);
    assert_eq!(
        session.service.session().state(0).remote,
        RemoteState::Unknown
    );
}

#[test]
fn unusable_terminal_and_failed_snapshot_export_complete_private_recovery() {
    let mut h = Harness::new("base", true);
    let mut c = Commands::capture(h.session(), 0).unwrap();
    let mut v = View::from_session(h.session(), 0);
    v.source = "latest recoverable draft".into();
    let draft = h.root.join("state.sqlite.tui-recovery/draft.json");
    fs::remove_file(&draft).unwrap();
    fs::create_dir(&draft).unwrap();
    let recovery = h.session().recovery.take().unwrap();
    let error = recovery.finish(h.session(), Some(&mut c), &v).unwrap_err();
    let message = error.to_string();
    let path = PathBuf::from(
        message
            .strip_prefix("TUI recovery snapshot failed; private recovery export: ")
            .unwrap(),
    );
    let bytes = fs::read(&path).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["conflict"]["local"], "latest recoverable draft");
    assert_eq!(value["remote_durability"], "not asserted");
    assert!(value["checkpoint"].as_array().unwrap().len() > 0);
    fs::write(h.root.join("emergency-export.json"), bytes).unwrap();
    fs::remove_file(path).unwrap();
    h.session().recovery = Some(recovery);
    assert_eq!(
        h.session().service.session().state(0).auth,
        AuthState::Detached
    );
}
#[test]
fn acknowledged_remote_durability_is_recovered_only_from_exact_native_receipt() {
    let mut h = Harness::new("base", true);
    let (c, v) = h.edit("changed");
    h.save_snapshot(&c, &v);
    struct Receipted;
    impl Transport for Receipted {
        fn capabilities(&self) -> TransportCapabilities {
            TransportCapabilities {
                receipts: ReceiptCapability::DurableV1,
                max_frame_bytes: 1_048_576,
                max_queue_bytes: 1_048_576,
                max_queue_frames: 64,
            }
        }
        fn state(&self) -> ConnectionState {
            ConnectionState::Authorized
        }
        fn send(&mut self, _: TransportFrame) -> Result<(), PortError> {
            Ok(())
        }
        fn receive(&mut self) -> Result<Option<TransportFrame>, PortError> {
            Ok(None)
        }
    }
    h.session()
        .service
        .attach_transport(Box::new(Receipted), 100, 0)
        .unwrap();
    let entry = h
        .store
        .as_ref()
        .unwrap()
        .pending(&h.id)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        h.session()
            .service
            .accept_receipt(
                DurableReceipt {
                    protocol: ReceiptCapability::DurableV1,
                    update: entry.id().clone()
                },
                0
            )
            .unwrap(),
        ReceiptResult::RemoteCommitted
    );
    h.save_snapshot(&c, &v);
    let s = h.restart();
    assert_eq!(
        s.service.session().state(0).remote,
        RemoteState::RemotelyCommitted
    );
    assert_eq!(s.service.session().state(0).pending_upload, 0);
    assert_eq!(s.service.session().state(0).auth, AuthState::Detached);
}

#[test]
fn disjoint_dirty_mapping_restart_keeps_base_and_requires_explicit_resolution() {
    use editac_cli::tui::collaboration::Collaboration;
    let mut h = Harness::new("# one\n\nsecond\n", false);
    let mut c = Commands::capture(h.session(), 0).unwrap();
    let opening = c.opening_revision().clone();
    let mut collaboration = Collaboration::capture(h.session(), 0).unwrap();
    let mut v = View::from_session(h.session(), 0);
    v.source = "# local one\n\nsecond\n".into();
    let checkpoint = h.session().service.session().recovery_export().checkpoint;
    let peer_store = Arc::new(
        NativeStore::create_new(
            h.root.join("peer.sqlite"),
            h.id.clone(),
            StoreLimits::default(),
        )
        .unwrap(),
    );
    peer_store
        .publish(0, CheckpointRecord::new(checkpoint, vec![]).unwrap())
        .unwrap();
    let mut peer = EditService::new(
        Session::open(peer_store.clone(), &h.id, CachedPermission::LOCAL).unwrap(),
        HostPolicy::default(),
    );
    let read = peer.read(0).unwrap();
    let owners = read
        .snapshot
        .visible
        .iter()
        .filter(|n| n.kind == "block")
        .map(|n| n.id.clone())
        .collect::<Vec<_>>();
    let binding = peer
        .capture_source(
            "# one\n\nsecond\n",
            SourceMode::Markdown(Default::default()),
            &owners,
            0,
        )
        .unwrap();
    let prepared = peer
        .prepare_source(
            &binding,
            &read.revision,
            "# one\n\nsecond remote\n",
            &editac_cli::engine::commands::text::TextOrigin::new("peer").unwrap(),
            0,
        )
        .unwrap();
    peer.apply(prepared, 0).unwrap();
    h.store
        .as_ref()
        .unwrap()
        .claim_writer(peer.session().document().writer_id())
        .unwrap();
    let entry = peer_store.pending(&h.id).unwrap().pop().unwrap();
    collaboration.enqueue(h.session(), entry).unwrap();
    collaboration
        .tick(h.session(), Some(&mut c), &mut v, 0)
        .unwrap();
    assert!(v.conflict.is_none());
    assert_ne!(v.source, h.session().source);
    assert_eq!(c.opening_revision(), &opening);
    assert_eq!(h.session().representation, Representation::HostedProjection);
    h.save_snapshot(&c, &v);
    let mut session = h.restart();
    let mut recovered = view(&session);
    let conflict = recovered.conflict.as_ref().unwrap();
    assert_eq!(conflict.base, "# one\n\nsecond\n");
    assert_eq!(conflict.local, "# local one\n\nsecond\n");
    assert_eq!(conflict.current, session.source);
    assert!(conflict.current.contains("second remote"));
    assert!(!conflict.prepared());
    let mut commands = Commands::capture(&session, 0).unwrap();
    let revision = session.service.read(0).unwrap().revision;
    editac_cli::tui::terminal::dispatch_command(
        &key(KeyCode::Enter, KeyModifiers::CONTROL),
        &mut recovered,
        &mut session,
        Some(&mut commands),
        0,
        &h.root,
    );
    assert_eq!(session.service.read(0).unwrap().revision, revision);
    assert!(recovered.conflict.is_some());
}

#[cfg(unix)]
#[test]
fn forced_termination_recovers_native_outbox_and_unresolved_draft() {
    signal_process("kill", "-KILL")
}

#[cfg(unix)]
#[test]
fn actual_sigterm_reaches_lifecycle_owner() {
    signal_process("terminate", "-TERM")
}
#[cfg(unix)]
#[test]
fn actual_sighup_reaches_lifecycle_owner() {
    signal_process("hangup", "-HUP")
}
#[test]
fn suspend_disposes_transport_without_claiming_receipt_or_implicit_reattach() {
    let mut h = Harness::new("base", true);
    let (c, v) = h.edit("changed");
    h.save_snapshot(&c, &v);
    let (drops, sends) = attach(&mut h, false);
    let mut io = Io::new(vec![quit()], "");
    io.lifecycle.push_back(Life::Suspend);
    let mut owner = TerminalOwner::new(io);
    run_session(h.session(), 0, &mut owner, || 0).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(sends.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.session().service.session().state(0).auth,
        AuthState::Detached
    );
    assert_eq!(h.session().service.session().state(0).pending_upload, 1);
    let s = h.restart();
    assert_eq!(s.source, "changed");
    assert_eq!(s.service.session().state(0).remote, RemoteState::Unknown);
}
#[test]
fn resume_initialization_failure_restores_modes_and_retains_recovery() {
    let mut h = Harness::new("base", true);
    let mut io = Io::new(vec![], "resume_init");
    io.lifecycle.push_back(Life::Suspend);
    let trace = io.trace.clone();
    let mut owner = TerminalOwner::new(io);
    assert!(run_session(h.session(), 0, &mut owner, || 0).is_err());
    assert_eq!(trace.lock().unwrap().starts, 2);
    assert_eq!(trace.lock().unwrap().modes, Modes::default());
    let s = h.restart();
    assert_eq!(view(&s).source, "base");
}
