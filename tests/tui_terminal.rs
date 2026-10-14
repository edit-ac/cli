//! Deterministic selected-renderer and injected OS-boundary qualification.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use editac_cli::tui::{
    terminal::{Mode, TerminalIo, TerminalOwner, bounded_size},
    view::{View, escape_line},
};
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend, TestBackend},
    buffer::Buffer,
};
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Eq)]
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
    calls: Vec<Mode>,
    frames: Vec<Buffer>,
}
struct Fixture {
    trace: Arc<Mutex<Trace>>,
    terminal: Terminal<TestBackend>,
    events: VecDeque<Option<Event>>,
    fail: Option<&'static str>,
    fail_mode: Option<Mode>,
}
impl Fixture {
    fn new(events: Vec<Option<Event>>) -> Self {
        Self {
            trace: Arc::default(),
            terminal: Terminal::new(TestBackend::new(64, 5)).unwrap(),
            events: events.into(),
            fail: None,
            fail_mode: None,
        }
    }
    fn check(&mut self, step: &'static str) -> io::Result<()> {
        if self.fail == Some(step) {
            self.fail = None;
            Err(io::Error::other(format!("prepared {step} failure")))
        } else {
            Ok(())
        }
    }
}
impl TerminalIo for Fixture {
    fn raw_enabled(&mut self) -> io::Result<bool> {
        self.check("raw query")?;
        Ok(self.trace.lock().unwrap().modes.raw)
    }
    fn mode(&mut self, mode: Mode) -> io::Result<()> {
        let mut trace = self.trace.lock().unwrap();
        trace.calls.push(mode);
        match mode {
            Mode::Raw(v) => trace.modes.raw = v,
            Mode::Alternate(v) => trace.modes.alternate = v,
            Mode::Paste(v) => trace.modes.paste = v,
            Mode::Focus(v) => trace.modes.focus = v,
            Mode::CursorVisible(v) => trace.modes.cursor = v,
        }
        // Fail AFTER side effect to prove attempted operations are rolled back.
        if self.fail_mode == Some(mode) {
            self.fail_mode = None;
            Err(io::Error::other("prepared partial mode failure"))
        } else {
            Ok(())
        }
    }
    fn size(&mut self) -> io::Result<(u16, u16)> {
        self.check("size")?;
        Ok((64, 5))
    }
    fn initialize(&mut self, _: (u16, u16)) -> io::Result<()> {
        self.check("initialize")
    }
    fn resize(&mut self, size: (u16, u16)) -> io::Result<()> {
        self.check("resize")?;
        self.terminal.backend_mut().resize(size.0, size.1);
        self.terminal.autoresize().unwrap();
        Ok(())
    }
    fn draw(&mut self, view: &View) -> io::Result<()> {
        self.terminal
            .draw(|frame| editac_cli::tui::view::render(frame, view))
            .unwrap();
        self.trace
            .lock()
            .unwrap()
            .frames
            .push(self.terminal.backend().buffer().clone());
        self.check("draw")
    }
    fn read(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        assert_eq!(timeout, Duration::from_millis(100));
        self.check("read")?;
        self.events
            .pop_front()
            .ok_or_else(|| io::Error::other("fixture events exhausted"))
    }
}
fn view() -> View {
    View {
        source: "# source\nsecond line\nthird".into(),
        status: "exact source | local initialized | remote unknown".into(),
        focused: true,
    }
}
fn key(code: KeyCode, modifiers: KeyModifiers) -> Option<Event> {
    Some(Event::Key(KeyEvent::new(code, modifiers)))
}
fn quit() -> Option<Event> {
    key(KeyCode::Esc, KeyModifiers::NONE)
}
fn restored(owner: &TerminalOwner<Fixture>) {
    assert_eq!(
        owner.backend().trace.lock().unwrap().modes,
        Modes::default()
    );
}

#[test]
fn fixed_frame_has_source_status_and_help_snapshot() {
    let f = Fixture::new(vec![quit()]);
    let mut owner = TerminalOwner::new(f);
    owner.run(&mut view()).unwrap();
    restored(&owner);
    let trace = owner.backend().trace.lock().unwrap();
    let expected = [
        "# source",
        "second line",
        "third",
        "focused | exact source | local initialized | remote unknown",
        "Esc / Ctrl-Q: quit | source shell; editing pending",
    ];
    let padded: Vec<_> = expected.iter().map(|s| format!("{s:<64}")).collect();
    assert_eq!(trace.frames, [Buffer::with_lines(padded)]);
    assert_eq!(
        trace.calls,
        [
            Mode::Raw(true),
            Mode::Alternate(true),
            Mode::Paste(true),
            Mode::Focus(true),
            Mode::CursorVisible(false),
            Mode::CursorVisible(true),
            Mode::Focus(false),
            Mode::Paste(false),
            Mode::Alternate(false),
            Mode::Raw(false)
        ]
    );
}
#[test]
fn resize_focus_and_repeated_events_are_deterministic() {
    let f = Fixture::new(vec![
        None,
        Some(Event::FocusLost),
        quit(),
        Some(Event::FocusLost),
        Some(Event::Resize(32, 4)),
        Some(Event::Resize(32, 4)),
        Some(Event::FocusGained),
        quit(),
    ]);
    let mut owner = TerminalOwner::new(f);
    let mut v = view();
    owner.run(&mut v).unwrap();
    restored(&owner);
    let trace = owner.backend().trace.lock().unwrap();
    assert_eq!(trace.frames.len(), 4);
    assert_eq!(trace.frames[2].area.width, 32);
    assert_eq!(trace.frames[2].area.height, 4);
    assert!(
        trace.frames[2]
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .contains("unfocused")
    );
    assert!(v.focused);
    assert_eq!(v.source, view().source);
}
#[test]
fn release_repeat_paste_and_mouse_do_not_mutate_or_quit() {
    let f = Fixture::new(vec![
        Some(Event::Key(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ))),
        Some(Event::Key(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ))),
        Some(Event::Paste("untrusted\x1b[31m".into())),
        key(KeyCode::Char('x'), KeyModifiers::NONE),
        key(KeyCode::Char('q'), KeyModifiers::CONTROL),
    ]);
    let mut owner = TerminalOwner::new(f);
    let mut v = view();
    owner.run(&mut v).unwrap();
    restored(&owner);
    assert_eq!(owner.backend().trace.lock().unwrap().frames.len(), 1);
    assert_eq!(v.source, view().source);
}
#[test]
fn control_c_is_a_normal_raw_mode_exit() {
    let mut owner = TerminalOwner::new(Fixture::new(vec![key(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )]));
    owner.run(&mut view()).unwrap();
    restored(&owner);
}
#[test]
fn malicious_source_and_status_never_reach_writer_as_controls() {
    let mut v = view();
    v.source = "safe\x1b]52;c;fixture\x07\r\t\x7f\u{009b}[31m\u{202e}".into();
    v.status = "\x1b[31mstatus\x00".into();
    let mut owner = TerminalOwner::new(Fixture::new(vec![quit()]));
    owner.run(&mut v).unwrap();
    let trace = owner.backend().trace.lock().unwrap();
    let buffer = &trace.frames[0];
    for cell in &buffer.content {
        assert!(cell.symbol().chars().all(|c| !c.is_control()));
    }
    let symbols = buffer
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(symbols.contains("\\x1b]52;c;fixture\\x07\\r\\t\\x7f\\x9b[31m\\u{202e}"));
    assert!(symbols.contains("\\x1b[31mstatus\\x00"));
    // Real selected backend receives sanitized cells. Its own CSI commands are
    // expected; the document's OSC clipboard command must never be emitted.
    let mut bytes = Vec::new();
    let mut backend = CrosstermBackend::new(&mut bytes);
    backend
        .draw(
            buffer
                .content
                .iter()
                .enumerate()
                .map(|(i, cell)| ((i % 64) as u16, (i / 64) as u16, cell)),
        )
        .unwrap();
    Backend::flush(&mut backend).unwrap();
    assert!(!bytes.windows(2).any(|w| w == b"\x1b]"));
    assert!(bytes.windows(4).any(|w| w == b"\\x1b"));
}
#[test]
fn every_control_byte_and_directional_control_is_visible() {
    for c in (0..=0x9f)
        .filter_map(char::from_u32)
        .filter(|c| c.is_control())
    {
        let escaped = escape_line(&c.to_string(), 1);
        assert!(escaped.starts_with('\\'));
        assert!(!escaped.chars().any(char::is_control));
    }
    assert_eq!(
        escape_line("a\u{202e}b\u{2066}c", 5),
        "a\\u{202e}b\\u{2066}c"
    );
    assert_eq!(escape_line("abcdef", 3), "abc");
}
#[test]
fn tiny_and_extreme_resize_are_bounded() {
    assert_eq!(bounded_size((0, 0)), (1, 1));
    assert_eq!(bounded_size((u16::MAX, u16::MAX)), (512, 256));
    let f = Fixture::new(vec![
        Some(Event::Resize(1, 1)),
        Some(Event::Resize(2, 2)),
        quit(),
    ]);
    let mut owner = TerminalOwner::new(f);
    owner.run(&mut view()).unwrap();
    restored(&owner);
    let trace = owner.backend().trace.lock().unwrap();
    assert_eq!(trace.frames[1], Buffer::with_lines(["f"]));
    assert_eq!(trace.frames[2], Buffer::with_lines(["fo", "Es"]));
}
#[test]
fn every_partial_startup_mode_failure_restores_terminal() {
    for mode in [
        Mode::Raw(true),
        Mode::Alternate(true),
        Mode::Paste(true),
        Mode::Focus(true),
        Mode::CursorVisible(false),
    ] {
        let mut f = Fixture::new(vec![]);
        f.fail_mode = Some(mode);
        let mut owner = TerminalOwner::new(f);
        assert!(owner.run(&mut view()).is_err());
        restored(&owner);
        let calls = owner.backend().trace.lock().unwrap().calls.clone();
        owner.shutdown().unwrap();
        assert_eq!(owner.backend().trace.lock().unwrap().calls, calls);
    }
}
#[test]
fn raw_query_size_and_renderer_initialization_failures_restore_terminal() {
    for step in ["raw query", "size", "initialize"] {
        let mut f = Fixture::new(vec![]);
        f.fail = Some(step);
        let mut owner = TerminalOwner::new(f);
        assert!(owner.run(&mut view()).is_err());
        restored(&owner);
    }
}
#[test]
fn draw_read_and_resize_errors_restore_terminal() {
    for step in ["draw", "read", "resize"] {
        let mut f = Fixture::new(vec![Some(Event::Resize(20, 4)), quit()]);
        f.fail = Some(step);
        let mut owner = TerminalOwner::new(f);
        assert!(owner.run(&mut view()).is_err());
        restored(&owner);
    }
}
#[test]
fn teardown_failure_attempts_remaining_modes_and_retries_only_failure() {
    let mut f = Fixture::new(vec![quit()]);
    f.fail_mode = Some(Mode::CursorVisible(true));
    let mut owner = TerminalOwner::new(f);
    assert!(owner.run(&mut view()).is_err());
    restored(&owner);
    let count = owner.backend().trace.lock().unwrap().calls.len();
    owner.shutdown().unwrap();
    assert_eq!(owner.backend().trace.lock().unwrap().calls.len(), count + 1);
    owner.shutdown().unwrap();
    assert_eq!(owner.backend().trace.lock().unwrap().calls.len(), count + 1);
}
#[test]
fn drop_after_start_is_idempotent_and_preserves_preexisting_raw_mode() {
    let f = Fixture::new(vec![]);
    let trace = f.trace.clone();
    trace.lock().unwrap().modes.raw = true;
    {
        let mut owner = TerminalOwner::new(f);
        owner.start().unwrap();
        assert!(owner.start().is_err());
    }
    let mut expected = Modes::default();
    expected.raw = true;
    assert_eq!(trace.lock().unwrap().modes, expected);
}
#[test]
fn unwinding_restores_owned_terminal() {
    let f = Fixture::new(vec![]);
    let trace = f.trace.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut owner = TerminalOwner::new(f);
        owner.start().unwrap();
        panic!("prepared unwinding");
    }));
    assert!(result.is_err());
    assert_eq!(trace.lock().unwrap().modes, Modes::default());
}

#[test]
fn real_shared_session_drives_source_permission_and_durability_labels() {
    use editac_cli::{
        engine::document::DocumentIdentity, target::ResolveOptions, tui::session::open_local,
    };
    let root = std::env::temp_dir().join(format!(
        "editac-tui-terminal-session-{}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    let _cleanup = Cleanup(root.clone());
    let path = root.join("source.md");
    let source = "# Exact spelling\r\n\r\n*retain* 🦀\n";
    std::fs::write(&path, source).unwrap();
    let identity = DocumentIdentity::v5(
        "ac000000-0000-5000-8000-000000000015",
        "ac000000-0000-4000-8000-000000000014",
    )
    .unwrap();
    let session = open_local(
        &path,
        ResolveOptions::default(),
        &root.join("cache.sqlite"),
        identity.clone(),
    )
    .unwrap();
    let mut v = View::from_session(&session, 0);
    assert_eq!(v.source, source);
    assert!(
        v.status
            .contains("exact source | Writable | local Initialized")
    );
    assert!(
        v.status
            .contains("remote Unknown | pending 0 | write capability")
    );
    let opening_state = session.service.session().state(0);
    let mut owner = TerminalOwner::new(Fixture::new(vec![
        Some(Event::FocusLost),
        Some(Event::Resize(40, 6)),
        Some(Event::FocusGained),
        quit(),
    ]));
    owner.run(&mut v).unwrap();
    restored(&owner);
    assert_eq!(session.source, source);
    assert_eq!(session.service.session().document().identity(), &identity);
    assert_eq!(session.service.session().state(0), opening_state);
    drop(session);
}
