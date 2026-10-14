//! Temporary source input only. The existing service owns prepared semantics;
//! keyboard/paste never calls apply, history or storage.
use super::{
    positions::{self, SourceMap},
    session::TerminalSession,
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use editac_core::{
    Utf16Offset,
    anchors::{self, BoundaryError, Direction},
    commands::text::TextOrigin,
};
use editac_session::{
    edit_service::{PreparedEdit, ServiceError, SourceTarget},
    revision::ObservedRevision,
    session::SessionStore,
};
use std::ops::Range;

pub const MAX_PASTE_BYTES: usize = 262_144;
#[derive(Debug, PartialEq, Eq)]
pub enum InputError {
    Boundary(BoundaryError),
    ReadOnly,
    TooLarge,
    InvalidUtf8,
}
impl From<BoundaryError> for InputError {
    fn from(e: BoundaryError) -> Self {
        Self::Boundary(e)
    }
}
#[derive(Clone, Debug)]
pub struct InputState {
    pub caret: Utf16Offset,
    pub anchor: Utf16Offset,
    pub writable: bool,
    pub max_source_bytes: usize,
    preferred_column: Option<usize>,
}
impl InputState {
    pub fn new(writable: bool, base_bytes: usize) -> Self {
        Self {
            caret: Utf16Offset(0),
            anchor: Utf16Offset(0),
            writable,
            max_source_bytes: 1_048_576usize.saturating_sub(base_bytes),
            preferred_column: None,
        }
    }
    pub fn selection(&self) -> Range<Utf16Offset> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }
    pub fn select(
        &mut self,
        source: &str,
        anchor: Utf16Offset,
        caret: Utf16Offset,
    ) -> Result<(), InputError> {
        let map = SourceMap::new(source);
        map.locate(anchor)?;
        map.locate(caret)?;
        self.anchor = anchor;
        self.caret = caret;
        self.preferred_column = None;
        Ok(())
    }
    pub fn paste_bytes(&mut self, source: &mut String, bytes: &[u8]) -> Result<(), InputError> {
        if bytes.len() > MAX_PASTE_BYTES {
            return Err(InputError::TooLarge);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| InputError::InvalidUtf8)?;
        self.replace(source, text)
    }
    fn replace(&mut self, source: &mut String, text: &str) -> Result<(), InputError> {
        if !self.writable {
            return Err(InputError::ReadOnly);
        }
        let selected = self.selection();
        let map = SourceMap::new(source);
        map.locate(selected.start)?;
        map.locate(selected.end)?;
        let range = anchors::validate_range(source, selected)?;
        let length = source
            .len()
            .saturating_sub(range.len())
            .saturating_add(text.len());
        if length > self.max_source_bytes {
            return Err(InputError::TooLarge);
        }
        let end = range.start + text.len();
        source.replace_range(range, text);
        self.caret = positions::after_insert(source, end)?;
        self.anchor = self.caret;
        self.preferred_column = None;
        Ok(())
    }
    pub fn handle(&mut self, source: &mut String, event: &Event) -> Result<bool, InputError> {
        if let Event::Paste(text) = event {
            self.paste_bytes(source, text.as_bytes())?;
            return Ok(true);
        }
        let Event::Key(key) = event else {
            return Ok(false);
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(false);
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let plain = (key.modifiers - KeyModifiers::SHIFT).is_empty();
        if key.code == KeyCode::Char('a') && key.modifiers == KeyModifiers::CONTROL {
            self.anchor = Utf16Offset(0);
            self.caret = positions::byte_to_offset(source, source.len())?;
            self.preferred_column = None;
            return Ok(true);
        }
        if !plain {
            return Ok(false);
        }
        let direction = match key.code {
            KeyCode::Left => Some(Direction::Backward),
            KeyCode::Right => Some(Direction::Forward),
            _ => None,
        };
        if let Some(direction) = direction {
            let next = if !shift && self.anchor != self.caret {
                Some(match direction {
                    Direction::Backward => self.selection().start,
                    Direction::Forward => self.selection().end,
                })
            } else {
                anchors::move_grapheme(source, self.caret, direction)?
            };
            if let Some(next) = next {
                self.caret = next;
            }
            if !shift {
                self.anchor = self.caret;
            }
            self.preferred_column = None;
            return Ok(true);
        }
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End => {
                let map = SourceMap::new(source);
                let (row, column) = map.locate(self.caret)?;
                let column = *self.preferred_column.get_or_insert(column);
                self.caret = match key.code {
                    KeyCode::Home => {
                        self.preferred_column = None;
                        map.lines[row].start
                    }
                    KeyCode::End => {
                        self.preferred_column = None;
                        map.lines[row].end
                    }
                    KeyCode::Up => map.at_column(row.saturating_sub(1), column),
                    _ => map.at_column((row + 1).min(map.lines.len() - 1), column),
                };
                if !shift {
                    self.anchor = self.caret;
                }
            }
            KeyCode::Backspace | KeyCode::Delete => {
                if !self.writable {
                    return Err(InputError::ReadOnly);
                }
                let old = (self.anchor, self.caret);
                if self.anchor == self.caret {
                    let dir = if key.code == KeyCode::Backspace {
                        Direction::Backward
                    } else {
                        Direction::Forward
                    };
                    if let Some(range) = anchors::deletion_range(source, self.caret, dir)? {
                        self.anchor = range.start;
                        self.caret = range.end;
                    }
                }
                if let Err(e) = self.replace(source, "") {
                    (self.anchor, self.caret) = old;
                    return Err(e);
                }
            }
            KeyCode::Enter => {
                let newline = if source.contains("\r\n") {
                    "\r\n"
                } else {
                    "\n"
                };
                self.replace(source, newline)?;
            }
            KeyCode::Tab => self.replace(source, "\t")?,
            KeyCode::Char(c) if !c.is_control() => self.replace(source, &c.to_string())?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}
/// One qualified source baseline and revision, retained separately from the
/// temporary draft. Preparation is explicit and cannot mutate the live replica.
pub struct SourceInput {
    target: SourceTarget,
    expected: ObservedRevision,
    origin: TextOrigin,
}
impl SourceInput {
    pub fn capture<S: SessionStore>(
        session: &TerminalSession<S>,
        now: u64,
    ) -> Result<Self, ServiceError> {
        let read = session.service.read(now)?;
        let owners = read
            .snapshot
            .visible
            .iter()
            .filter(|node| node.kind == "block")
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        let target = session
            .service
            .capture_source(&session.source, session.mode, &owners, now)?;
        Ok(Self {
            target,
            expected: read.revision,
            origin: TextOrigin::new("tui:source").expect("static valid origin"),
        })
    }
    pub fn prepare<S: SessionStore>(
        &self,
        session: &TerminalSession<S>,
        draft: &str,
        now: u64,
    ) -> Result<PreparedEdit, ServiceError> {
        session
            .service
            .prepare_source(&self.target, &self.expected, draft, &self.origin, now)
    }
    pub fn revision(&self) -> &ObservedRevision {
        &self.expected
    }
    pub fn baseline(&self) -> &str {
        self.target.source()
    }
}
