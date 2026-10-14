//! One synchronous owner for modes, renderer and bounded per-event work.
use super::view::{self, View};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
        Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, layout::Rect};
use std::{
    io::{self, Stdout},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Raw(bool),
    Alternate(bool),
    Paste(bool),
    Focus(bool),
    CursorVisible(bool),
}
impl Mode {
    fn inverse(self, initial_raw: bool) -> Self {
        match self {
            Self::Raw(_) => Self::Raw(initial_raw),
            Self::Alternate(v) => Self::Alternate(!v),
            Self::Paste(v) => Self::Paste(!v),
            Self::Focus(v) => Self::Focus(!v),
            Self::CursorVisible(v) => Self::CursorVisible(!v),
        }
    }
}

/// Inject the OS boundary, retaining the same guard/event/view path in tests.
/// Mode calls can partially succeed; the guard records rollback before each call.
pub trait TerminalIo {
    fn raw_enabled(&mut self) -> io::Result<bool>;
    fn mode(&mut self, mode: Mode) -> io::Result<()>;
    fn size(&mut self) -> io::Result<(u16, u16)>;
    fn initialize(&mut self, size: (u16, u16)) -> io::Result<()>;
    fn resize(&mut self, size: (u16, u16)) -> io::Result<()>;
    fn draw(&mut self, view: &View) -> io::Result<()>;
    fn read(&mut self, timeout: Duration) -> io::Result<Option<Event>>;
    fn lifecycle(&mut self) -> io::Result<Option<super::lifecycle::Event>> {
        Ok(None)
    }
    fn suspend(&mut self) -> io::Result<()> {
        Err(io::Error::other("suspend unsupported by this terminal"))
    }
    fn dispose(&mut self) {}
}

pub struct TerminalOwner<T: TerminalIo> {
    io: T,
    rollback: Vec<Mode>,
    size: (u16, u16),
}
impl<T: TerminalIo> TerminalOwner<T> {
    pub fn new(io: T) -> Self {
        Self {
            io,
            rollback: Vec::new(),
            size: (0, 0),
        }
    }
    pub fn backend(&self) -> &T {
        &self.io
    }
    pub fn start(&mut self) -> io::Result<()> {
        if !self.rollback.is_empty() {
            return Err(io::Error::other("terminal already started"));
        }
        let result = self.start_inner();
        if result.is_err() {
            let _ = self.shutdown();
        }
        result
    }
    fn start_inner(&mut self) -> io::Result<()> {
        let initial_raw = self.io.raw_enabled()?;
        for mode in [
            Mode::Raw(true),
            Mode::Alternate(true),
            Mode::Paste(true),
            Mode::Focus(true),
            Mode::CursorVisible(false),
        ] {
            self.rollback.push(mode.inverse(initial_raw));
            self.io.mode(mode)?;
        }
        self.size = bounded_size(self.io.size()?);
        self.io.initialize(self.size)
    }
    /// All inverses are attempted even when one fails. Successful operations are
    /// removed; failed inverses are retained for an idempotent retry/Drop.
    pub fn shutdown(&mut self) -> io::Result<()> {
        let mut first = None;
        let mut pending = Vec::new();
        while let Some(mode) = self.rollback.pop() {
            if let Err(error) = self.io.mode(mode) {
                if first.is_none() {
                    first = Some(error);
                }
                pending.push(mode);
            }
        }
        pending.reverse();
        self.rollback = pending;
        if self.rollback.is_empty() {
            self.io.dispose();
        }
        first.map_or(Ok(()), Err)
    }
    pub fn run(&mut self, view: &mut View) -> io::Result<()> {
        self.run_with(view, |_, _| false)
    }
    /// Command callback runs under the same event/terminal owner, before input.
    pub fn run_with(
        &mut self,
        view: &mut View,
        mut command: impl FnMut(&Event, &mut View) -> bool,
    ) -> io::Result<()> {
        self.run_with_tick(view, &mut command, |_| false)
    }
    pub fn run_with_tick(
        &mut self,
        view: &mut View,
        mut command: impl FnMut(&Event, &mut View) -> bool,
        mut tick: impl FnMut(&mut View) -> bool,
    ) -> io::Result<()> {
        self.run_managed(view, &mut command, &mut tick, |event, view| {
            if event == super::lifecycle::Event::RecoverableExit
                || (event == super::lifecycle::Event::Escape
                    && (view.review.is_some() || view.conflict.as_ref().is_some_and(|c| c.open)))
            {
                return Ok(super::lifecycle::Action::Pass);
            }
            Ok(
                if matches!(
                    event,
                    super::lifecycle::Event::Quit
                        | super::lifecycle::Event::Escape
                        | super::lifecycle::Event::Interrupt
                ) {
                    super::lifecycle::Action::Exit
                } else {
                    super::lifecycle::Action::Continue
                },
            )
        })
    }
    pub fn run_managed(
        &mut self,
        view: &mut View,
        mut command: impl FnMut(&Event, &mut View) -> bool,
        mut tick: impl FnMut(&mut View) -> bool,
        mut lifecycle: impl FnMut(
            super::lifecycle::Event,
            &mut View,
        ) -> io::Result<super::lifecycle::Action>,
    ) -> io::Result<()> {
        lifecycle(super::lifecycle::Event::Checkpoint, view)?;
        self.start()?;
        let result = self.run_inner(view, &mut command, &mut tick, &mut lifecycle);
        let cleanup = self.shutdown();
        result.and(cleanup)
    }
    fn run_inner(
        &mut self,
        view: &mut View,
        command: &mut impl FnMut(&Event, &mut View) -> bool,
        tick: &mut impl FnMut(&mut View) -> bool,
        lifecycle: &mut impl FnMut(
            super::lifecycle::Event,
            &mut View,
        ) -> io::Result<super::lifecycle::Action>,
    ) -> io::Result<()> {
        self.io.draw(view)?;
        loop {
            if let Some(event) = self.io.lifecycle()? {
                if lifecycle(event, view)? == super::lifecycle::Action::Exit {
                    return Ok(());
                }
                if event == super::lifecycle::Event::Suspend {
                    self.shutdown()?;
                    self.io.suspend()?;
                    self.start()?;
                    lifecycle(super::lifecycle::Event::Resume, view)?;
                }
                self.io.draw(view)?;
            }
            if tick(view) {
                lifecycle(super::lifecycle::Event::Checkpoint, view)?;
                self.io.draw(view)?;
            }
            let Some(event) = self.io.read(Duration::from_millis(100))? else {
                continue;
            };
            let redraw = match event {
                Event::Resize(width, height) => {
                    let size = bounded_size((width, height));
                    if size == self.size {
                        false
                    } else {
                        self.io.resize(size)?;
                        self.size = size;
                        true
                    }
                }
                Event::FocusLost => {
                    let changed = view.focused;
                    view.focused = false;
                    changed
                }
                Event::FocusGained => {
                    let changed = !view.focused;
                    view.focused = true;
                    changed
                }
                Event::Key(key)
                    if view.focused
                        && key.kind == KeyEventKind::Press
                        && ((key.code == KeyCode::Esc)
                            || (matches!(key.code, KeyCode::Char('q' | 'c'))
                                && key.modifiers == KeyModifiers::CONTROL)) =>
                {
                    let intent = if key.code == KeyCode::Esc {
                        super::lifecycle::Event::Escape
                    } else if key.code == KeyCode::Char('c') {
                        super::lifecycle::Event::Interrupt
                    } else {
                        super::lifecycle::Event::Quit
                    };
                    match lifecycle(intent, view)? {
                        super::lifecycle::Action::Exit => return Ok(()),
                        super::lifecycle::Action::Pass => command(&Event::Key(key), view),
                        super::lifecycle::Action::Continue => true,
                    }
                }
                Event::Key(key)
                    if view.focused
                        && key.kind == KeyEventKind::Press
                        && key.code == KeyCode::Char('e')
                        && key.modifiers == KeyModifiers::CONTROL =>
                {
                    match lifecycle(super::lifecycle::Event::RecoverableExit, view)? {
                        super::lifecycle::Action::Exit => return Ok(()),
                        super::lifecycle::Action::Pass => command(&Event::Key(key), view),
                        super::lifecycle::Action::Continue => true,
                    }
                }
                event if view.focused && command(&event, view) => true,
                event if view.focused => {
                    if let Some(input) = &mut view.input {
                        let before = view.source.clone();
                        match input.handle(&mut view.source, &event) {
                            Ok(changed) => {
                                view.draft_changed |= before != view.source;
                                if changed {
                                    view.input_message = None;
                                }
                                changed
                            }
                            Err(error) => {
                                view.input_message = Some(format!("input rejected: {error:?}"));
                                true
                            }
                        }
                    } else {
                        false
                    }
                }
                _ => false,
            };
            if redraw {
                lifecycle(super::lifecycle::Event::Checkpoint, view)?;
                self.io.draw(view)?;
            }
        }
    }
}
impl<T: TerminalIo> Drop for TerminalOwner<T> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
/// Cap allocations/draw work even for malicious or impossible resize events.
pub fn bounded_size((width, height): (u16, u16)) -> (u16, u16) {
    (width.clamp(1, 512), height.clamp(1, 256))
}

#[derive(Default)]
pub struct CrosstermIo {
    terminal: Option<Terminal<CrosstermBackend<Stdout>>>,
    #[cfg(unix)]
    signals: Option<super::lifecycle::Signals>,
}
impl TerminalIo for CrosstermIo {
    fn raw_enabled(&mut self) -> io::Result<bool> {
        #[cfg(unix)]
        if self.signals.is_none() {
            self.signals = Some(super::lifecycle::Signals::new()?);
        }
        terminal::is_raw_mode_enabled()
    }
    fn mode(&mut self, mode: Mode) -> io::Result<()> {
        let mut out = io::stdout();
        match mode {
            Mode::Raw(true) => terminal::enable_raw_mode(),
            Mode::Raw(false) => terminal::disable_raw_mode(),
            Mode::Alternate(true) => execute!(out, EnterAlternateScreen),
            Mode::Alternate(false) => execute!(out, LeaveAlternateScreen),
            Mode::Paste(true) => execute!(out, EnableBracketedPaste),
            Mode::Paste(false) => execute!(out, DisableBracketedPaste),
            Mode::Focus(true) => execute!(out, EnableFocusChange),
            Mode::Focus(false) => execute!(out, DisableFocusChange),
            Mode::CursorVisible(true) => execute!(out, Show),
            Mode::CursorVisible(false) => execute!(out, Hide),
        }
    }
    fn size(&mut self) -> io::Result<(u16, u16)> {
        terminal::size()
    }
    fn initialize(&mut self, (width, height): (u16, u16)) -> io::Result<()> {
        self.terminal = Some(Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, width, height)),
            },
        )?);
        self.terminal.as_mut().unwrap().clear()
    }
    fn resize(&mut self, (width, height): (u16, u16)) -> io::Result<()> {
        self.terminal
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal unavailable"))?
            .resize(Rect::new(0, 0, width, height))
    }
    fn draw(&mut self, view: &View) -> io::Result<()> {
        self.terminal
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal unavailable"))?
            .draw(|frame| view::render(frame, view))
            .map(|_| ())
    }
    fn lifecycle(&mut self) -> io::Result<Option<super::lifecycle::Event>> {
        #[cfg(unix)]
        {
            return Ok(self.signals.as_ref().and_then(|s| s.poll()));
        }
        #[cfg(not(unix))]
        {
            Ok(None)
        }
    }
    fn suspend(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            signal_hook::low_level::raise(signal_hook::consts::SIGSTOP)
        }
        #[cfg(not(unix))]
        {
            Err(io::Error::other("native suspend unsupported"))
        }
    }
    fn dispose(&mut self) {
        self.terminal = None;
        #[cfg(unix)]
        {
            self.signals = None;
        }
    }
    fn read(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        let ready = match event::poll(timeout) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => false,
            result => result?,
        };
        if ready {
            event::read().map(Some)
        } else {
            Ok(None)
        }
    }
}

pub fn run<S: editac_session::session::SessionStore>(
    session: &mut super::session::TerminalSession<S>,
    now: u64,
) -> io::Result<()> {
    run_with_adapter(session, now, None)
}
pub fn run_with_adapter<S: editac_session::session::SessionStore>(
    session: &mut super::session::TerminalSession<S>,
    now: u64,
    adapter: Option<std::path::PathBuf>,
) -> io::Result<()> {
    let clock = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(u64::MAX, |d| d.as_secs())
    };
    let adapter = adapter
        .map(crate::host_adapter::HostAdapter::new)
        .transpose()
        .map_err(io::Error::other)?;
    let mut acknowledged = vec![];
    let result = run_session_with_delivery(
        session,
        now,
        &mut TerminalOwner::new(CrosstermIo::default()),
        clock,
        |session, collaboration, finalizing| {
            let Some(adapter) = &adapter else {
                return Ok(());
            };
            collaboration.enable_host_delivery();
            for id in collaboration.take_admitted() {
                if !acknowledged.contains(&id) {
                    acknowledged.push(id);
                }
            }
            let pending = collaboration.pending();
            let entries = adapter
                .terminal_updates(
                    session.service.session().document().identity(),
                    &acknowledged,
                    &collaboration.queued_ids(),
                    if finalizing {
                        0
                    } else {
                        super::collaboration::MAX_PENDING - pending.0
                    },
                    if finalizing {
                        0
                    } else {
                        super::collaboration::MAX_PENDING_BYTES - pending.1
                    },
                )
                .map_err(|_| {
                    io::Error::other(
                        "host delivery unavailable; retained drafts and pending journal",
                    )
                })?;
            acknowledged.clear();
            for entry in entries {
                collaboration
                    .enqueue(session, entry)
                    .map_err(|_| io::Error::other("host envelope rejected; retained draft"))?;
            }
            Ok(())
        },
    );
    if let Some(adapter) = &adapter {
        let detached = adapter
            .terminal_detach(session.service.session().document().identity())
            .map_err(|_| io::Error::other("host detach unavailable; local state retained"));
        return result.and(detached);
    }
    result
}
/// Canonical command/collaboration/lifecycle orchestration used by the binary and tests.
pub fn run_session<S: editac_session::session::SessionStore, T: TerminalIo>(
    session: &mut super::session::TerminalSession<S>,
    now: u64,
    terminal: &mut TerminalOwner<T>,
    clock: impl Fn() -> u64,
) -> io::Result<()> {
    run_session_with_delivery(session, now, terminal, clock, |_, _, _| Ok(()))
}
fn run_session_with_delivery<S: editac_session::session::SessionStore, T: TerminalIo>(
    session: &mut super::session::TerminalSession<S>,
    now: u64,
    terminal: &mut TerminalOwner<T>,
    clock: impl Fn() -> u64,
    mut delivery: impl FnMut(
        &super::session::TerminalSession<S>,
        &mut super::collaboration::Collaboration,
        bool,
    ) -> io::Result<()>,
) -> io::Result<()> {
    let mut recovery = session
        .recovery
        .take()
        .ok_or_else(|| io::Error::other("private draft recovery owner required"))?;
    let result = (|| {
        let root = recovery.root().to_path_buf();
        let mut commands = super::commands::Commands::capture(session, now).ok();
        let mut view = View::from_session(session, now);
        recovery.restore_view(session, &mut view)?;
        if commands.is_none() {
            if let Some(input) = &mut view.input {
                input.writable = false;
            }
            view.input_message = Some("source preparation unavailable; read only".into());
        }
        let collaboration = super::collaboration::Collaboration::capture(session, now)
            .map_err(|_| io::Error::other("change subscription unavailable"))?;
        let shared = std::cell::RefCell::new((&mut *session, &mut commands, collaboration));
        let result = terminal.run_managed(
            &mut view,
            |event, view| {
                let mut state = shared.borrow_mut();
                let (session, commands, _) = &mut *state;
                dispatch_command(event, view, session, commands.as_mut(), clock(), &root)
            },
            |view| {
                let mut state = shared.borrow_mut();
                let (session, commands, collaboration) = &mut *state;
                let delivery_error = delivery(session, collaboration, false)
                    .err()
                    .map(|e| e.to_string());
                match collaboration.tick(session, commands.as_mut(), view, clock()) {
                    Ok(changed) => {
                        if let Some(message) = delivery_error {
                            let message_changed = view.input_message.as_ref() != Some(&message);
                            view.input_message = Some(message);
                            changed || message_changed
                        } else {
                            changed
                        }
                    }
                    Err(_) => {
                        let message = "remote update unavailable; retain draft and queued work";
                        let status = View::from_session(session, clock()).status;
                        let changed =
                            view.input_message.as_deref() != Some(message) || view.status != status;
                        view.status = status;
                        if let Some(i) = &mut view.input {
                            i.writable = false;
                        }
                        view.input_message = Some(message.into());
                        changed
                    }
                }
            },
            |event, view| {
                let mut state = shared.borrow_mut();
                let (session, commands, _) = &mut *state;
                recovery.handle(event, session, commands.as_mut(), view, clock())
            },
        );
        let mut state = shared.borrow_mut();
        let (session, commands, collaboration) = &mut *state;
        let delivered = delivery(session, collaboration, true);
        let finished = recovery.finish(session, commands.as_ref(), &view);
        finished.and(result).and(delivered)
    })();
    // Includes failures during view/observer acquisition, before terminal startup.
    session.service.detach_transport();
    let cleanup = terminal.shutdown();
    session.recovery = Some(recovery);
    result.and(cleanup)
}

/// Shared by the real terminal loop and deterministic injected-event checks.
/// Ctrl-S saves, Ctrl-Enter commits, Ctrl-Z/Y use shared history, Ctrl-R recovers.
pub fn dispatch_command<S: editac_session::session::SessionStore>(
    event: &Event,
    view: &mut View,
    session: &mut super::session::TerminalSession<S>,
    commands: Option<&mut super::commands::Commands>,
    now: u64,
    recovery_root: &std::path::Path,
) -> bool {
    use super::commands::{Command, CommandError};
    if view.conflict.is_some() {
        return super::conflict::dispatch(event, view, session, commands, now, recovery_root);
    }
    // The cached input flag never grants permission after revocation/lease expiry.
    let available = commands.is_some();
    if let Some(input) = &mut view.input {
        input.writable =
            available && view.review.is_none() && session.service.capabilities(now).can_apply;
    }
    if matches!(event, Event::Key(k) if k.kind == KeyEventKind::Press && k.modifiers == KeyModifiers::CONTROL && k.code == KeyCode::Char('e'))
    {
        view.input_message = Some(
            match super::diff::export_private(session, &view.source, now, recovery_root) {
                Ok(path) => format!(
                    "private source/semantic recovery export: {}",
                    path.display()
                ),
                Err(error) => format!("export rejected: {error:?}; retain source"),
            },
        );
        return true;
    }
    if matches!(event, Event::Key(k) if k.kind == KeyEventKind::Press && k.modifiers == KeyModifiers::CONTROL && k.code == KeyCode::Char('d'))
    {
        if view.review.is_some() {
            view.review = None;
        } else {
            let review = match commands.as_ref() {
                Some(commands) => commands.review(session, &view.source, now),
                None => super::diff::Review::from_session(session, &view.source, now),
            };
            match review {
                Ok(review) => {
                    view.review = Some(review);
                    view.input_message = None;
                }
                Err(error) => {
                    view.input_message = Some(format!("review rejected: {error:?}; retain draft"))
                }
            }
        }
        return true;
    }
    if view.review.is_some() {
        let mut apply = false;
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                if key.code == KeyCode::Esc {
                    view.review = None;
                } else if key.modifiers == KeyModifiers::NONE {
                    let review = view.review.as_mut().expect("review exists");
                    match key.code {
                        KeyCode::Up => review.navigate(-1),
                        KeyCode::Down => review.navigate(1),
                        KeyCode::PageUp => review.navigate(-16),
                        KeyCode::PageDown => review.navigate(16),
                        KeyCode::Home => review.navigate(isize::MIN),
                        KeyCode::End => review.navigate(isize::MAX),
                        _ => {
                            view.input_message =
                                Some("review is read only; Esc returns to source".into())
                        }
                    }
                } else if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Enter {
                    apply = true;
                } else {
                    view.input_message = Some("review is read only; Esc returns to source".into());
                }
            }
        }
        if apply {
            let result = commands.map(|commands| {
                commands.apply_review(
                    view.review.as_ref().expect("review exists"),
                    session,
                    &mut view.source,
                    now,
                    recovery_root,
                )
            });
            view.input_message = Some(match result {
                Some(Ok(_)) => {
                    view.review = None;
                    "review committed through shared preparation".into()
                }
                Some(Err(_)) => {
                    "review rejected; stale/unsupported/readonly; retain draft and review again"
                        .into()
                }
                None => "review apply unavailable; read only".into(),
            });
            view.status = View::from_session(session, now).status;
            view.draft_changed = view.source != session.source;
        }
        return true; // Paste and all other editing/history paths remain inert in review.
    }
    let Event::Key(key) = event else {
        return false;
    };
    if key.kind != KeyEventKind::Press || key.modifiers != KeyModifiers::CONTROL {
        return false;
    }
    let command = match key.code {
        KeyCode::Char('s') => Command::Save,
        KeyCode::Enter => Command::Commit,
        KeyCode::Char('z') => Command::Undo,
        KeyCode::Char('y') => Command::Redo,
        KeyCode::Char('r') => Command::Recover,
        _ => return false,
    };
    let commands_available = commands.is_some();
    let opening = commands
        .as_ref()
        .map(|c| (c.baseline().to_owned(), c.revision().clone()));
    let result = commands
        .map(|commands| commands.execute(session, command, &mut view.source, now, recovery_root));
    let refreshed = View::from_session(session, now);
    view.status = refreshed.status;
    view.draft_changed = view.source != session.source;
    if let (
        Some(Err(CommandError::Service(
            editac_session::edit_service::ServiceError::SourceConflict { conflict, .. },
        ))),
        Some((base, base_revision)),
    ) = (&result, opening)
    {
        if let (Ok(current), Ok(read)) = (session.project_source(), session.service.read(now)) {
            view.conflict = super::conflict::ConflictView::capture(
                &base,
                &view.source,
                &current,
                &base_revision,
                read.revision,
                super::conflict::Reason::Mapping(conflict.kind),
            )
            .ok();
        }
    }
    if let Some(input) = &mut view.input {
        input.writable = commands_available && session.service.capabilities(now).can_apply;
    }
    view.input_message = Some(match result {
        Some(Ok(result)) => {
            if matches!(command, Command::Undo | Command::Redo | Command::Recover) {
                view.input = refreshed.input; // History restores source; caret returns to start.
            }
            format!(
                "{command:?}: {} | file {:?}",
                if result.changed {
                    "committed locally"
                } else {
                    "no semantic change"
                },
                result.file
            )
        }
        Some(Err(error)) => match error {
            CommandError::Service(_) => {
                "command rejected; retain draft; check session/recovery status".into()
            }
            CommandError::DirtyDraft => "commit or save the draft before history".into(),
            CommandError::File(failure) => format!(
                "file publication {:?}; private draft retained",
                failure.kind
            ),
            CommandError::Preserve(_) => "cannot preserve private draft; source retained".into(),
            CommandError::Review(_) => "review rejected; retain draft".into(),
            CommandError::Refresh => {
                "source binding refresh failed; retain source and reopen".into()
            }
        },
        None => "command unavailable; read only".into(),
    });
    true
}
