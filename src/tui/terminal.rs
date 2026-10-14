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
        self.start()?;
        let result = self.run_inner(view, &mut command, &mut tick);
        let cleanup = self.shutdown();
        result.and(cleanup)
    }
    fn run_inner(
        &mut self,
        view: &mut View,
        command: &mut impl FnMut(&Event, &mut View) -> bool,
        tick: &mut impl FnMut(&mut View) -> bool,
    ) -> io::Result<()> {
        self.io.draw(view)?;
        loop {
            if tick(view) {
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
                        && ((key.code == KeyCode::Esc
                            && view.review.is_none()
                            && !view.conflict.as_ref().is_some_and(|c| c.open))
                            || (matches!(key.code, KeyCode::Char('q' | 'c'))
                                && key.modifiers == KeyModifiers::CONTROL)) =>
                {
                    return Ok(());
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
}
impl TerminalIo for CrosstermIo {
    fn raw_enabled(&mut self) -> io::Result<bool> {
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
    fn read(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if event::poll(timeout)? {
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
    let recovery_root = session
        .local_baseline
        .as_ref()
        .and_then(|b| b.target().resolved_path().parent())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let mut commands = super::commands::Commands::capture(session, now).ok();
    let mut view = View::from_session(session, now);
    if commands.is_none() {
        if let Some(input) = &mut view.input {
            input.writable = false;
        }
        view.input_message = Some("source preparation unavailable; read only".into());
    }
    let collaboration = super::collaboration::Collaboration::capture(session, now)
        .map_err(|_| io::Error::other("change subscription unavailable"))?;
    let shared = std::cell::RefCell::new((session, &mut commands, collaboration));
    let clock = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(u64::MAX, |d| d.as_secs())
    };
    TerminalOwner::new(CrosstermIo::default()).run_with_tick(
        &mut view,
        |event, view| {
            let mut state = shared.borrow_mut();
            let (session, commands, _) = &mut *state;
            dispatch_command(
                event,
                view,
                session,
                commands.as_mut(),
                clock(),
                &recovery_root,
            )
        },
        |view| {
            let mut state = shared.borrow_mut();
            let (session, commands, collaboration) = &mut *state;
            match collaboration.tick(session, commands.as_mut(), view, clock()) {
                Ok(changed) => changed,
                Err(_) => {
                    let message = "remote update unavailable; retain draft and queued work";
                    let status = View::from_session(session, clock()).status;
                    let changed =
                        view.input_message.as_deref() != Some(message) || view.status != status;
                    view.status = status;
                    if let Some(input) = &mut view.input {
                        input.writable = false;
                    }
                    view.input_message = Some(message.into());
                    changed
                }
            }
        },
    )
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
