//! Recoverable presentation over shared source conflicts, never a merge algorithm.
use super::{
    commands::{CommandError, Commands},
    diff,
    input::{InputState, SourceInput},
    session::TerminalSession,
};
use crate::markdown::reconcile::ConflictKind;
use editac_diff::representation::ContentHash;
use editac_session::{
    edit_service::ServiceError, revision::ObservedRevision, session::SessionStore,
};
use std::path::{Path, PathBuf};

pub const MAX_SOURCE_BYTES: usize = 1_048_576;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Mapping(ConflictKind),
    Incompatible,
    Unavailable,
}
impl Reason {
    pub fn from_error(error: &CommandError) -> Self {
        match error {
            CommandError::Service(
                ServiceError::SourceConflict { conflict, .. } | ServiceError::Source(conflict),
            ) => Self::Mapping(conflict.kind),
            _ => Self::Unavailable,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Base,
    Local,
    Current,
    Resolution,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictError {
    Unavailable,
    Limit,
    Stale,
    Unprepared,
    Incompatible,
    Rejected,
    Export,
}
#[derive(Clone)]
pub struct ConflictView {
    pub base: String,
    pub local: String,
    pub current: String,
    pub resolution: String,
    pub reason: Reason,
    pub open: bool,
    pub pane: Pane,
    pub input: InputState,
    pub base_label: String,
    pub current_label: String,
    expected: ObservedRevision,
    prepared: Option<ContentHash>,
    top: usize,
}
// Diagnostics deliberately omit source content.
impl std::fmt::Debug for ConflictView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConflictView")
            .field("reason", &self.reason)
            .field("pane", &self.pane)
            .field("prepared", &self.prepared.is_some())
            .finish()
    }
}
fn label(revision: &ObservedRevision) -> String {
    ContentHash::of(&revision.to_bytes()).hex()
}
impl ConflictView {
    pub fn capture(
        base: &str,
        local: &str,
        current: &str,
        base_revision: &ObservedRevision,
        expected: ObservedRevision,
        reason: Reason,
    ) -> Result<Self, ConflictError> {
        if [base, local, current]
            .iter()
            .any(|s| s.len() > MAX_SOURCE_BYTES)
        {
            return Err(ConflictError::Limit);
        }
        let mut input = InputState::new(true, 0);
        input.max_source_bytes = MAX_SOURCE_BYTES;
        Ok(Self {
            base: base.into(),
            local: local.into(),
            current: current.into(),
            resolution: local.into(),
            reason,
            open: true,
            pane: Pane::Base,
            input,
            base_label: label(base_revision),
            current_label: label(&expected),
            expected,
            prepared: None,
            top: 0,
        })
    }
    /// Another revision keeps the user's proposed resolution, but revokes its
    /// prepared admission. No new current state can be silently approved.
    pub fn advance(
        &mut self,
        local: &str,
        current: &str,
        expected: ObservedRevision,
        reason: Reason,
    ) -> Result<(), ConflictError> {
        if local.len() > MAX_SOURCE_BYTES || current.len() > MAX_SOURCE_BYTES {
            return Err(ConflictError::Limit);
        }
        self.local = local.into();
        self.current = current.into();
        self.reason = reason;
        self.current_label = label(&expected);
        self.expected = expected;
        self.prepared = None;
        self.open = true;
        Ok(())
    }
    pub fn next_pane(&mut self) {
        self.pane = match self.pane {
            Pane::Base => Pane::Local,
            Pane::Local => Pane::Current,
            Pane::Current => Pane::Resolution,
            Pane::Resolution => Pane::Base,
        };
        self.top = 0;
    }
    pub fn choose(&mut self, pane: Pane) {
        self.resolution = match pane {
            Pane::Base => &self.base,
            Pane::Local => &self.local,
            Pane::Current => &self.current,
            Pane::Resolution => &self.resolution,
        }
        .clone();
        self.pane = Pane::Resolution;
        self.input = InputState::new(true, 0);
        self.prepared = None;
        self.top = 0;
    }
    pub fn navigate(&mut self, delta: isize) {
        self.top = self
            .top
            .saturating_add_signed(delta)
            .min(self.source().split('\n').count().saturating_sub(1));
    }
    pub fn source(&self) -> &str {
        match self.pane {
            Pane::Base => &self.base,
            Pane::Local => &self.local,
            Pane::Current => &self.current,
            Pane::Resolution => &self.resolution,
        }
    }
    pub fn page(&self, height: usize) -> impl Iterator<Item = &str> {
        self.source()
            .split('\n')
            .skip(self.top)
            .take(height.min(512))
    }
    pub fn heading(&self) -> String {
        format!(
            "CONFLICT {:?} | {:?} | base/local/current/resolution",
            self.reason, self.pane
        )
    }
    pub fn prepared(&self) -> bool {
        self.prepared == Some(ContentHash::of(self.resolution.as_bytes()))
    }
    pub fn cancel(&mut self) {
        self.open = false;
        self.prepared = None;
    }
    pub fn expected(&self) -> &ObservedRevision {
        &self.expected
    }
    fn validate<S: SessionStore>(
        &self,
        session: &TerminalSession<S>,
        now: u64,
    ) -> Result<(), ConflictError> {
        if self.reason == Reason::Incompatible {
            return Err(ConflictError::Incompatible);
        }
        if self.resolution.len() > MAX_SOURCE_BYTES {
            return Err(ConflictError::Limit);
        }
        let read = session
            .service
            .read(now)
            .map_err(|_| ConflictError::Unavailable)?;
        if read.revision != self.expected {
            return Err(ConflictError::Stale);
        }
        Ok(())
    }
    pub fn prepare<S: SessionStore>(
        &mut self,
        session: &TerminalSession<S>,
        now: u64,
    ) -> Result<(), ConflictError> {
        self.prepared = None;
        self.validate(session, now)?;
        let binding = SourceInput::capture(session, now).map_err(|_| ConflictError::Rejected)?;
        binding
            .prepare(session, &self.resolution, now)
            .map_err(|_| ConflictError::Rejected)?;
        self.prepared = Some(ContentHash::of(self.resolution.as_bytes()));
        Ok(())
    }
    pub fn apply<S: SessionStore>(
        &mut self,
        session: &mut TerminalSession<S>,
        commands: &mut Commands,
        now: u64,
        root: &Path,
    ) -> Result<(), ConflictError> {
        self.validate(session, now)?;
        if !self.prepared() {
            return Err(ConflictError::Unprepared);
        }
        commands
            .resolve(session, &mut self.resolution, &self.expected, now, root)
            .map_err(|_| ConflictError::Rejected)?;
        Ok(())
    }
    pub fn export_private<S: SessionStore>(
        &self,
        session: &TerminalSession<S>,
        now: u64,
        root: &Path,
    ) -> Result<PathBuf, ConflictError> {
        if [&self.base, &self.local, &self.current, &self.resolution]
            .iter()
            .any(|s| s.len() > MAX_SOURCE_BYTES)
        {
            return Err(ConflictError::Limit);
        }
        let mut value: serde_json::Value = if session.service.session().state(now).readiness
            == editac_session::session::Readiness::RecoveryRequired
        {
            // The qualified owner freezes revision admission after a failed local
            // transaction. Its recovery checkpoint still includes full semantic
            // and opaque state; label it as unjournaled, never as a save receipt.
            let recovery = session.service.session().recovery_export();
            if recovery.checkpoint.bytes.len() > 4 * MAX_SOURCE_BYTES
                || recovery
                    .unjournaled
                    .as_ref()
                    .is_some_and(|e| e.bytes().len() > MAX_SOURCE_BYTES)
            {
                return Err(ConflictError::Limit);
            }
            let id = &recovery.checkpoint.identity;
            let unjournaled = recovery.unjournaled.as_ref().map(|e| serde_json::json!({"writer":e.id().writer,"sequence":e.id().sequence,"checksum":e.id().checksum,"origin":e.origin(),"bytes":e.bytes()}));
            serde_json::json!({"version":"editac-tui-recovery-v1","source":self.local,"representation":format!("{:?}",session.representation),"revision":"unavailable during local recovery","last_observed_revision":self.current_label,"semantic_encoding":"complete engine checkpoint (includes unjournaled state)","identity":{"document_id":id.document_id,"epoch":id.epoch,"schema":id.schema,"profile":id.profile,"protocol_version":id.protocol_version,"update_encoding":id.update_encoding,"offsets":id.offsets},"checkpoint":recovery.checkpoint.bytes,"unjournaled":unjournaled,"locally_committed":recovery.locally_committed,"remote_durability":"not asserted"})
        } else {
            let bytes =
                diff::export_bytes(session, &self.local, now).map_err(|_| ConflictError::Export)?;
            serde_json::from_slice(&bytes).map_err(|_| ConflictError::Export)?
        };
        value["conflict"] = serde_json::json!({"version":"editac-tui-conflict-v1", "base":self.base, "local":self.local, "current":self.current, "resolution":self.resolution, "base_revision":self.base_label, "current_revision":self.current_label, "reason":format!("{:?}",self.reason), "unresolved":true});
        let bytes = serde_json::to_vec(&value).map_err(|_| ConflictError::Export)?;
        if bytes.len() > 64 * MAX_SOURCE_BYTES {
            return Err(ConflictError::Limit);
        }
        diff::write_private(&bytes, root).map_err(|_| ConflictError::Export)
    }
}

/// Real and injected terminal events share this modal admission boundary.
pub fn dispatch<S: SessionStore>(
    event: &crossterm::event::Event,
    view: &mut super::view::View,
    session: &mut TerminalSession<S>,
    commands: Option<&mut Commands>,
    now: u64,
    root: &Path,
) -> bool {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    let conflict = view.conflict.as_mut().expect("conflict dispatch");
    if !conflict.open {
        conflict.local = view.source.clone();
    }
    let control = match event {
        Event::Key(k) if k.kind == KeyEventKind::Press && k.modifiers == KeyModifiers::CONTROL => {
            Some(k.code)
        }
        _ => None,
    };
    if let Some(input) = &mut view.input {
        input.writable =
            !conflict.open && commands.is_some() && session.service.capabilities(now).can_apply;
    }
    if control == Some(KeyCode::Char('e')) {
        // Include typing since a cancelled view, without losing its chosen resolution.
        conflict.local = view.source.clone();
        view.input_message = Some(match conflict.export_private(session, now, root) {
            Ok(path) => format!(
                "private base/local/current/resolution recovery export: {}",
                path.display()
            ),
            Err(e) => format!("conflict export rejected: {e:?}; drafts retained"),
        });
        return true;
    }
    if control == Some(KeyCode::Char('t')) {
        conflict.open = true;
        view.review = None;
        return true;
    }
    if !conflict.open {
        if control.is_some() {
            view.input_message =
                Some("unresolved conflict; Ctrl-T resolve or Ctrl-E export".into());
            return true;
        }
        return false;
    }
    let message = match control {
        Some(KeyCode::Char('l')) => {
            conflict.choose(Pane::Local);
            "local source chosen; edit then Ctrl-P prepare".into()
        }
        Some(KeyCode::Char('g')) => {
            conflict.choose(Pane::Current);
            "current source chosen; edit then Ctrl-P prepare".into()
        }
        Some(KeyCode::Char('p')) => match conflict.prepare(session, now) {
            Ok(()) => "resolution prepared; Ctrl-Enter applies (not saved remotely)".into(),
            Err(e) => format!("resolution rejected: {e:?}; all drafts retained"),
        },
        Some(KeyCode::Enter) => {
            let result = commands
                .map(|c| conflict.apply(session, c, now, root))
                .unwrap_or(Err(ConflictError::Unavailable));
            match result {
                Ok(()) => {
                    view.source = session.source.clone();
                    view.conflict = None;
                    view.review = None;
                    view.input = super::view::View::from_session(session, now).input;
                    view.status = super::view::View::from_session(session, now).status;
                    view.draft_changed = false;
                    view.input_message = Some("resolution committed through shared preparation; local/remote status below".into());
                    return true;
                }
                Err(e) => {
                    format!("resolution rejected: {e:?}; recheck current state and prepare again")
                }
            }
        }
        Some(KeyCode::Char('a')) if conflict.pane == Pane::Resolution => {
            let _ = conflict.input.handle(&mut conflict.resolution, event);
            "resolution selection; Ctrl-P prepares current revision".into()
        }
        Some(_) => "conflict: choose/edit, prepare, apply, cancel or export".into(),
        None => {
            if let Event::Key(k) = event {
                if k.kind == KeyEventKind::Release {
                    return true;
                }
                if k.code == KeyCode::Esc {
                    conflict.cancel();
                    view.input_message = Some(
                        "resolution cancelled; draft retained; Ctrl-T reopens, Ctrl-E exports"
                            .into(),
                    );
                    return true;
                }
                if k.code == KeyCode::Tab {
                    conflict.next_pane();
                    return true;
                }
            }
            if conflict.pane == Pane::Resolution {
                conflict.input.writable = commands.is_some()
                    && session.service.capabilities(now).can_apply
                    && conflict.reason != Reason::Incompatible;
                match conflict.input.handle(&mut conflict.resolution, event) {
                    Ok(_) => "resolution draft; Ctrl-P prepares current revision".into(),
                    Err(_) => "resolution input rejected; candidate retained".into(),
                }
            } else {
                if let Event::Key(k) = event {
                    match k.code {
                        KeyCode::Up => conflict.navigate(-1),
                        KeyCode::Down => conflict.navigate(1),
                        KeyCode::PageUp => conflict.navigate(-16),
                        KeyCode::PageDown => conflict.navigate(16),
                        KeyCode::Home => conflict.navigate(isize::MIN),
                        KeyCode::End => conflict.navigate(isize::MAX),
                        _ => (),
                    }
                }
                "source pane is read only; Tab changes pane, Ctrl-L/G chooses resolution".into()
            }
        }
    };
    view.input_message = Some(message);
    true
}

impl ConflictView {
    pub fn render(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
        focused: bool,
    ) {
        use ratatui::{
            layout::Rect,
            style::{Color, Style},
            text::{Line, Span},
            widgets::Paragraph,
        };
        let width = usize::from(area.width);
        let headers = [
            self.heading(),
            format!("Base revision: {} (opening source)", self.base_label),
            format!(
                "Current revision: {} (shared projection; opaque state in export)",
                self.current_label
            ),
            format!(
                "{:?} | {} | no commit/save receipt",
                self.pane,
                if self.prepared() {
                    "prepared (not applied)"
                } else {
                    "unprepared"
                }
            ),
        ];
        let rows = headers
            .iter()
            .take(usize::from(area.height))
            .map(|row| Line::raw(super::view::escape_line(row, width)))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(rows), area);
        let height = area.height.saturating_sub(4);
        if height == 0 {
            return;
        }
        let body = Rect::new(area.x, area.y + 4, area.width, height);
        if self.pane != Pane::Resolution {
            let rows = self
                .page(usize::from(height))
                .map(|row| Line::raw(super::view::escape_line(row, width)))
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(rows), body);
            return;
        }
        let map = super::positions::SourceMap::new(&self.resolution);
        let (row, column) = map.locate(self.input.caret).unwrap_or((0, 0));
        let top = row.saturating_sub(usize::from(height) - 1);
        let left = column.saturating_sub(width.saturating_sub(1));
        let selection = self.input.selection();
        let rows = map
            .lines
            .iter()
            .skip(top)
            .take(usize::from(height))
            .map(|line| {
                let mut spans = vec![];
                for glyph in &line.glyphs {
                    if glyph.column + glyph.width <= left {
                        continue;
                    }
                    if glyph.column < left {
                        spans.push(Span::raw(
                            " ".repeat((glyph.column + glyph.width - left).min(width)),
                        ));
                        continue;
                    }
                    if glyph.column + glyph.width > left.saturating_add(width) {
                        break;
                    }
                    let style = if glyph.start < selection.end && selection.start < glyph.end {
                        Style::default().bg(Color::Blue)
                    } else {
                        Style::default()
                    };
                    spans.push(Span::styled(glyph.text.clone(), style));
                }
                Line::from(spans)
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(rows), body);
        if focused && width > 0 {
            frame.set_cursor_position((
                body.x + u16::try_from(column - left).unwrap_or(0),
                body.y + u16::try_from(row - top).unwrap_or(0),
            ));
        }
    }
}
