//! Coalesced shared-session observation; this owner holds drafts and bounded UI work,
//! never another semantic replica. Host adapters enqueue authenticated scoped envelopes.
//! They retain the existing store's writer-reservation admission policy; enqueue cannot
//! authorize a remote writer or discover a host/credentials.
use super::{
    commands::Commands,
    input::InputState,
    positions,
    session::{Representation, TerminalSession},
    view::View,
};
use editac_core::Utf16Offset;
use editac_session::{
    edit_service::ServiceError, ports::JournalEntry, revision::ObservedRevision,
    session::SessionStore,
};
use std::collections::VecDeque;

pub const MAX_PENDING: usize = 64;
pub const MAX_PENDING_BYTES: usize = 1_048_576;
pub const MAX_PER_TICK: usize = 8;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    pub base: String,
    pub local: String,
    pub current: String,
    pub reconciled: bool,
}
#[derive(Debug)]
pub enum CollaborationError {
    Service(ServiceError),
    QueueFull,
    ConflictingUpdate,
    Incompatible,
    Projection,
    Binding,
}
pub struct Collaboration {
    observed: ObservedRevision,
    pending: VecDeque<JournalEntry>,
    bytes: usize,
    pub draft: Option<Draft>,
    incompatible: bool,
}
impl Collaboration {
    pub fn capture<S: SessionStore>(
        session: &TerminalSession<S>,
        now: u64,
    ) -> Result<Self, CollaborationError> {
        Ok(Self {
            observed: session
                .service
                .read(now)
                .map_err(CollaborationError::Service)?
                .revision,
            pending: VecDeque::new(),
            bytes: 0,
            draft: None,
            incompatible: false,
        })
    }
    pub fn pending(&self) -> (usize, usize) {
        (self.pending.len(), self.bytes)
    }
    /// Backpressure returns ownership to the caller; it must retain/retry the envelope.
    pub fn enqueue<S: SessionStore>(
        &mut self,
        session: &TerminalSession<S>,
        entry: JournalEntry,
    ) -> Result<(), (CollaborationError, JournalEntry)> {
        if &entry.id().identity != session.service.session().document().identity() {
            self.incompatible = true;
            return Err((CollaborationError::Incompatible, entry));
        }
        if let Some(queued) = self
            .pending
            .iter()
            .find(|e| e.id().writer == entry.id().writer && e.id().sequence == entry.id().sequence)
        {
            // An update key alone cannot establish idempotence: the shared journal
            // requires identical bytes and origin as well. Return conflicting work
            // to the adapter rather than silently dropping it before admission.
            return if queued == &entry {
                Ok(())
            } else {
                Err((CollaborationError::ConflictingUpdate, entry))
            };
        }
        if self.pending.len() >= MAX_PENDING
            || self.bytes.saturating_add(entry.bytes().len()) > MAX_PENDING_BYTES
        {
            return Err((CollaborationError::QueueFull, entry));
        }
        self.bytes += entry.bytes().len();
        self.pending.push_back(entry);
        Ok(())
    }
    /// At most eight journal integrations and one projection/observation per frame.
    /// Failed durable admission retains the queued update and the user's draft.
    pub fn tick<S: SessionStore>(
        &mut self,
        session: &mut TerminalSession<S>,
        mut commands: Option<&mut Commands>,
        view: &mut View,
        now: u64,
    ) -> Result<bool, CollaborationError> {
        if self.incompatible && view.conflict.is_none() {
            let current = session
                .project_source()
                .map_err(|_| CollaborationError::Projection)?;
            let revision = session
                .service
                .read(now)
                .map_err(CollaborationError::Service)?
                .revision;
            view.conflict = Some(
                super::conflict::ConflictView::capture(
                    &session.source,
                    &view.source,
                    &current,
                    &self.observed,
                    revision,
                    super::conflict::Reason::Incompatible,
                )
                .map_err(|_| CollaborationError::Projection)?,
            );
            self.draft = Some(Draft {
                base: session.source.clone(),
                local: view.source.clone(),
                current,
                reconciled: false,
            });
            view.review = None;
            view.input_message = Some(
                "incompatible identity/epoch; rejected update retained by adapter; export recovery"
                    .into(),
            );
            return Ok(true);
        }
        let clean_anchors = if matches!(
            session.mode,
            crate::markdown::reconcile::SourceMode::Literal
        ) && view.source == session.source
            && session.project_source().ok().as_ref() == Some(&view.source)
        {
            view.input.as_ref().and_then(|input| {
                let doc = session.service.session().document();
                let snapshot = doc.snapshot().ok()?;
                let owner = snapshot
                    .visible
                    .iter()
                    .find(|n| n.kind == "block")?
                    .id
                    .as_str();
                doc.selection(
                    (owner, input.anchor, editac_core::anchors::Bias::Right),
                    (owner, input.caret, editac_core::anchors::Bias::Right),
                )
                .ok()
            })
        } else {
            None
        };
        let mut writable_binding = commands.is_some();
        let mut stalled = 0;
        for _ in 0..MAX_PER_TICK {
            let Some(entry) = self.pending.front() else {
                break;
            };
            match session.service.receive_peer(entry.clone()) {
                Ok(_) => (),
                Err(ServiceError::Session(editac_session::session::SessionError::Document(
                    crate::engine::document::DocumentError::IncompleteRestore,
                ))) => {
                    let waiting = self.pending.pop_front().expect("front exists");
                    self.pending.push_back(waiting);
                    stalled += 1;
                    if stalled >= self.pending.len() {
                        break;
                    }
                    continue;
                }
                Err(error) => return Err(CollaborationError::Service(error)),
            }
            stalled = 0;
            let entry = self.pending.pop_front().expect("front exists");
            self.bytes -= entry.bytes().len();
        }
        // Draft text can change between ticks. Keep the retained recovery candidate current.
        if let Some(draft) = &mut self.draft {
            if draft.local != view.source {
                draft.local = view.source.clone();
                if let Some(conflict) = &mut view.conflict {
                    conflict.local = view.source.clone();
                }
                draft.reconciled = commands
                    .as_deref_mut()
                    .is_some_and(|c| c.reconcile(session, &view.source, now).is_ok());
            }
        }
        let status = View::from_session(session, now).status;
        let status_changed = view.status != status;
        view.status = status;
        let Some(read) = session
            .service
            .observe(&self.observed, now)
            .map_err(CollaborationError::Service)?
        else {
            if view.source == session.source && view.conflict.is_none() {
                self.draft = None;
            }
            return Ok(status_changed);
        };
        // Our explicit command already rebound this revision and retained exact spelling.
        // It is not a remote refresh; don't normalize a locally committed source.
        if commands
            .as_ref()
            .is_some_and(|c| c.revision() == &read.revision)
            && view.source == session.source
        {
            self.observed = read.revision;
            self.draft = None;
            view.conflict = None;
            return Ok(status_changed);
        }
        let current = session
            .project_source()
            .map_err(|_| CollaborationError::Projection)?;
        if current.len() > MAX_PENDING_BYTES {
            return Err(CollaborationError::Projection);
        }
        let dirty = view.source != session.source;
        if dirty {
            let base = self
                .draft
                .as_ref()
                .map(|d| d.base.clone())
                .unwrap_or_else(|| session.source.clone());
            let result = commands
                .as_deref_mut()
                .map(|c| c.reconcile(session, &view.source, now));
            let reconciled = matches!(result, Some(Ok(()))) && !self.incompatible;
            let reason = if self.incompatible {
                super::conflict::Reason::Incompatible
            } else {
                match &result {
                    Some(Err(e)) => super::conflict::Reason::from_error(e),
                    _ => super::conflict::Reason::Unavailable,
                }
            };
            if let Some(conflict) = &mut view.conflict {
                conflict
                    .advance(&view.source, &current, read.revision.clone(), reason)
                    .map_err(|_| CollaborationError::Projection)?;
            } else if !reconciled {
                view.conflict = Some(
                    super::conflict::ConflictView::capture(
                        &base,
                        &view.source,
                        &current,
                        &self.observed,
                        read.revision.clone(),
                        reason,
                    )
                    .map_err(|_| CollaborationError::Projection)?,
                );
                view.review = None;
            }
            self.draft = Some(Draft {
                base,
                local: view.source.clone(),
                current: current.clone(),
                reconciled,
            });
            view.input_message = Some(
                if reconciled {
                    "remote change: disjoint draft prepared; explicit commit required"
                } else {
                    "remote conflict: base/local/current retained; export draft before resolution"
                }
                .into(),
            );
        } else {
            if let Some(conflict) = &mut view.conflict {
                conflict
                    .advance(
                        &view.source,
                        &current,
                        read.revision.clone(),
                        conflict.reason,
                    )
                    .map_err(|_| CollaborationError::Projection)?;
            }
            if let Some(input) = &mut view.input {
                let retained = refresh_selection(input, &view.source, &current);
                if let Some(selection) = &clean_anchors {
                    use editac_core::anchors::Resolution;
                    if let Ok(resolved) = session
                        .service
                        .session()
                        .document()
                        .resolve_selection(selection)
                    {
                        if let (Resolution::Position(a), Resolution::Position(c)) =
                            (resolved.anchor, resolved.focus)
                        {
                            let align = |o| {
                                positions::offset_to_byte(&current, o)
                                    .ok()
                                    .and_then(|b| positions::after_insert(&current, b).ok())
                            };
                            if let (Some(a), Some(c)) = (align(a.offset), align(c.offset)) {
                                // Deleted text uses the documented replacement-end collapse.
                                // Surviving text follows the shared CRDT anchors.
                                let _ = input.select(
                                    &current,
                                    if retained.0 { a } else { input.anchor },
                                    if retained.1 { c } else { input.caret },
                                );
                            }
                        }
                    }
                }
            }
            view.source = current.clone();
            self.draft = None;
            view.input_message = Some("remote source refreshed; selection mapped".into());
        }
        session.source = current;
        if matches!(
            session.mode,
            crate::markdown::reconcile::SourceMode::Markdown(_)
        ) {
            session.representation = Representation::HostedProjection;
        }
        if let Some(c) = commands {
            c.remote_advanced();
            if !dirty && c.refresh(session, now).is_err() {
                writable_binding = false;
            }
        }
        if let Some(input) = &mut view.input {
            input.writable = writable_binding
                && session.service.capabilities(now).can_apply
                && view.review.is_none();
            input.writable &= !view.conflict.as_ref().is_some_and(|c| c.open);
        }
        view.draft_changed = view.source != session.source;
        self.observed = read.revision;
        Ok(true)
    }
}
/// Presentation mapping only: unchanged prefix/suffix retain exact source positions;
/// replaced/deleted positions collapse to the replacement's end, on a grapheme boundary.
/// This never supplies semantic edit anchors to the engine.
pub fn refresh_selection(input: &mut InputState, old: &str, new: &str) -> (bool, bool) {
    let prefix = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    let suffix = old[prefix..]
        .chars()
        .rev()
        .zip(new[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    use editac_diff::text::{LineKind, PatchLimits, word_changes};
    // Reuse the qualified bounded source diff to preserve unchanged interior runs,
    // including selections between several disjoint updates. These are UI offsets.
    let limits = PatchLimits {
        max_source_bytes: MAX_PENDING_BYTES,
        max_output_bytes: MAX_PENDING_BYTES,
        max_lines: 16_384,
        max_comparison_cells: 131_072,
        ..PatchLimits::default()
    };
    if let Ok(changes) = word_changes(old, new, limits) {
        let map = |offset| {
            let byte = positions::offset_to_byte(old, offset).unwrap_or(old.len());
            if byte < prefix {
                return (
                    positions::after_insert(new, byte).unwrap_or(Utf16Offset(0)),
                    true,
                );
            }
            if byte >= old.len() - suffix {
                return (
                    positions::after_insert(new, new.len() - (old.len() - byte))
                        .unwrap_or(Utf16Offset(0)),
                    true,
                );
            }
            let (mut before, mut after) = (0, 0);
            for change in &changes {
                match change.kind {
                    LineKind::Context => {
                        if byte < before + change.text.len() {
                            return (
                                positions::after_insert(new, after + byte.saturating_sub(before))
                                    .unwrap_or(Utf16Offset(0)),
                                byte >= before,
                            );
                        }
                        before += change.text.len();
                        after += change.text.len();
                    }
                    LineKind::Delete => before += change.text.len(),
                    LineKind::Insert => after += change.text.len(),
                }
            }
            (
                positions::after_insert(new, new.len()).unwrap_or(Utf16Offset(0)),
                false,
            )
        };
        let (anchor, a_retained) = map(input.anchor);
        let (caret, c_retained) = map(input.caret);
        let _ = input.select(new, anchor, caret);
        return (a_retained, c_retained);
    }

    let map = |offset: Utf16Offset| {
        let byte = positions::offset_to_byte(old, offset).unwrap_or(old.len());
        let target = if byte < prefix {
            byte
        } else if byte >= old.len() - suffix {
            new.len() - (old.len() - byte)
        } else {
            new.len() - suffix
        };
        positions::after_insert(new, target).unwrap_or(Utf16Offset(0))
    };
    let anchor = map(input.anchor);
    let caret = map(input.caret);
    let _ = input.select(new, anchor, caret);
    (false, false)
}
