//! Source commands delegate semantic history and durability to the shared owner.
//! Source spellings are presentation history, never a second semantic replica.
use super::{input::SourceInput, session::TerminalSession};
use crate::{
    editor::ChangedBuffer,
    files,
    reconcile::{LocalBaseline, ReconciledEdit},
    target::{self, Format, ResolveOptions},
};
use editac_session::{
    edit_service::ServiceError,
    session::{SessionState, SessionStore},
};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Commit,
    Save,
    Undo,
    Redo,
    Recover,
}
#[derive(Debug)]
pub enum CommandError {
    Service(ServiceError),
    DirtyDraft,
    Preserve(crate::editor::EditorError),
    File(files::SaveFailure),
    Refresh,
    Review(super::diff::ReviewError),
}
impl From<ServiceError> for CommandError {
    fn from(e: ServiceError) -> Self {
        Self::Service(e)
    }
}
#[derive(Debug)]
pub struct CommandResult {
    pub changed: bool,
    pub file: Option<files::SaveState>,
    pub state: SessionState,
}
#[derive(Clone)]
struct Spelling {
    before: String,
    after: String,
}
enum Pending {
    Commit(Spelling),
    Undo(Spelling),
    Redo(Spelling),
}
pub struct Commands {
    binding: SourceInput,
    undo: Vec<Spelling>,
    redo: Vec<Spelling>,
    pending: Option<Pending>,
    projected_history: bool,
}
impl Commands {
    pub fn capture<S: SessionStore>(
        session: &TerminalSession<S>,
        now: u64,
    ) -> Result<Self, CommandError> {
        Ok(Self {
            binding: SourceInput::capture(session, now)?,
            undo: vec![],
            redo: vec![],
            pending: None,
            projected_history: false,
        })
    }
    pub fn review<S: SessionStore>(
        &self,
        session: &TerminalSession<S>,
        draft: &str,
        now: u64,
    ) -> Result<super::diff::Review, super::diff::ReviewError> {
        super::diff::Review::capture(
            session,
            self.binding.baseline(),
            self.binding.revision(),
            draft,
            now,
        )
    }
    pub fn apply_review<S: SessionStore>(
        &mut self,
        review: &super::diff::Review,
        session: &mut TerminalSession<S>,
        draft: &mut String,
        now: u64,
        root: &Path,
    ) -> Result<CommandResult, CommandError> {
        review
            .validate(session, draft, now)
            .map_err(CommandError::Review)?;
        self.execute(session, Command::Commit, draft, now, root)
    }
    pub fn revision(&self) -> &editac_session::revision::ObservedRevision {
        self.binding.revision()
    }
    pub fn remote_advanced(&mut self) {
        self.projected_history = true;
    }
    pub fn reconcile<S: SessionStore>(
        &mut self,
        session: &TerminalSession<S>,
        draft: &str,
        now: u64,
    ) -> Result<(), CommandError> {
        self.binding.reconcile(session, draft, now)?;
        Ok(())
    }
    pub fn refresh<S: SessionStore>(
        &mut self,
        session: &TerminalSession<S>,
        now: u64,
    ) -> Result<(), CommandError> {
        self.rebind(session, now)
    }
    fn bound_history(&mut self) {
        while self.undo.len() > 100
            || self
                .undo
                .iter()
                .chain(&self.redo)
                .map(|s| s.before.len() + s.after.len())
                .sum::<usize>()
                > 8 * 1024 * 1024
        {
            if self.undo.is_empty() {
                self.redo.remove(0);
            } else {
                self.undo.remove(0);
            }
        }
    }
    fn rebind<S: SessionStore>(
        &mut self,
        session: &TerminalSession<S>,
        now: u64,
    ) -> Result<(), CommandError> {
        self.binding = SourceInput::capture(session, now)?;
        Ok(())
    }
    fn commit<S: SessionStore>(
        &mut self,
        session: &mut TerminalSession<S>,
        draft: &str,
        now: u64,
    ) -> Result<bool, CommandError> {
        let prepared = self.binding.prepare(session, draft, now)?;
        let mut spelling = Spelling {
            before: session.source.clone(),
            after: draft.into(),
        };
        let committed = match session.service.apply(prepared, now) {
            Ok(result) => result.is_some(),
            Err(error) => {
                if matches!(
                    error,
                    ServiceError::Session(editac_session::session::SessionError::Store(_))
                ) {
                    self.pending = Some(Pending::Commit(spelling));
                }
                return Err(error.into());
            }
        };
        if self.binding.baseline() != session.source {
            spelling.after = session
                .project_source()
                .map_err(|_| CommandError::Refresh)?;
        }
        if committed {
            self.undo.push(spelling);
            self.redo.clear();
        } else if session.source != draft {
            // Spelling-only acceptance belongs to the current semantic history state.
            if let Some(last) = self.undo.last_mut() {
                last.after = draft.into();
            }
            if let Some(last) = self.redo.last_mut() {
                last.before = draft.into();
            }
        }
        self.bound_history();
        session.source = if self.binding.baseline() != session.source {
            session
                .project_source()
                .map_err(|_| CommandError::Refresh)?
        } else {
            draft.into()
        };
        self.rebind(session, now)?;
        Ok(committed)
    }
    pub fn execute<S: SessionStore>(
        &mut self,
        session: &mut TerminalSession<S>,
        command: Command,
        draft: &mut String,
        now: u64,
        recovery_root: &Path,
    ) -> Result<CommandResult, CommandError> {
        self.execute_with(
            session,
            command,
            draft,
            now,
            recovery_root,
            &mut files::NativeFilesystem,
        )
    }
    pub fn execute_with<S: SessionStore>(
        &mut self,
        session: &mut TerminalSession<S>,
        command: Command,
        draft: &mut String,
        now: u64,
        recovery_root: &Path,
        filesystem: &mut impl files::Filesystem,
    ) -> Result<CommandResult, CommandError> {
        let mut file = None;
        let changed = match command {
            Command::Commit | Command::Save => {
                let changed = self.commit(session, draft, now)?;
                *draft = session.source.clone();
                if command == Command::Save {
                    if let Some(baseline) = &session.local_baseline {
                        let target = baseline.target().clone();
                        let buffer = ChangedBuffer::preserve(
                            target.clone(),
                            session.source.clone(),
                            recovery_root,
                        )
                        .map_err(CommandError::Preserve)?;
                        // The service has already qualified and durably captured this source.
                        // Do not perform a second semantic commit to publish its exact bytes.
                        let saved = files::save_with(
                            ReconciledEdit {
                                buffer,
                                commit: None,
                            },
                            None,
                            false,
                            filesystem,
                        )
                        .map_err(CommandError::File)?;
                        file = Some(saved.state);
                        let refreshed = target::resolve(
                            target.requested_path(),
                            ResolveOptions {
                                format: Some(match target.format() {
                                    Format::Markdown => Format::Markdown,
                                    Format::Text => Format::Text,
                                }),
                                ..ResolveOptions::default()
                            },
                        )
                        .map_err(|_| CommandError::Refresh)?;
                        if refreshed.source() != session.source {
                            return Err(CommandError::Refresh);
                        }
                        let owners = session
                            .service
                            .read(now)?
                            .snapshot
                            .visible
                            .iter()
                            .filter(|n| n.kind == "block")
                            .map(|n| n.id.clone())
                            .collect::<Vec<_>>();
                        session.local_baseline = Some(
                            LocalBaseline::capture(
                                session.service.session().document(),
                                refreshed,
                                &owners,
                            )
                            .map_err(|_| CommandError::Refresh)?,
                        );
                        // Only success permits cleanup. Publication failures retain these files.
                        saved
                            .edit
                            .buffer
                            .into_parts()
                            .2
                            .discard()
                            .map_err(|_| CommandError::Refresh)?;
                    }
                }
                changed
            }
            Command::Undo | Command::Redo => {
                if *draft != session.source {
                    return Err(CommandError::DirtyDraft);
                }
                let redo = command == Command::Redo;
                // Never mutate semantic history when exact spelling evidence was evicted.
                if (if redo { &self.redo } else { &self.undo }).is_empty() {
                    if !session.service.capabilities(now).can_history {
                        return Err(ServiceError::ReadOnly.into());
                    }
                    self.binding.prepare(session, draft, now)?;
                    return Ok(CommandResult {
                        changed: false,
                        file: None,
                        state: session.service.session().state(now),
                    });
                }
                let result = if redo {
                    session.service.redo(self.binding.revision(), now)
                } else {
                    session.service.undo(self.binding.revision(), now)
                };
                let spelling = if redo {
                    self.redo.last()
                } else {
                    self.undo.last()
                }
                .cloned();
                let changed = match result {
                    Ok(result) => result.is_some(),
                    Err(error) => {
                        if matches!(
                            error,
                            ServiceError::Session(editac_session::session::SessionError::Store(_))
                        ) {
                            if let Some(s) = spelling {
                                self.pending = Some(if redo {
                                    Pending::Redo(s)
                                } else {
                                    Pending::Undo(s)
                                });
                            }
                        }
                        return Err(error.into());
                    }
                };
                if changed {
                    let spelling = spelling.ok_or(CommandError::Refresh)?;
                    if redo {
                        self.redo.pop();
                        session.source = spelling.after.clone();
                        self.undo.push(spelling);
                    } else {
                        self.undo.pop();
                        session.source = spelling.before.clone();
                        self.redo.push(spelling);
                    }
                    if self.projected_history {
                        session.source = session
                            .project_source()
                            .map_err(|_| CommandError::Refresh)?;
                    }
                    *draft = session.source.clone();
                }
                self.rebind(session, now)?;
                changed
            }
            Command::Recover => {
                session.service.recover().map_err(ServiceError::from)?;
                let changed = self.pending.is_some();
                if let Some(pending) = self.pending.take() {
                    // The input owner may have accepted more typing after failure.
                    // Recover the committed spelling without replacing that newer draft.
                    let submitted = match &pending {
                        Pending::Commit(s) => &s.after,
                        Pending::Undo(s) => &s.after,
                        Pending::Redo(s) => &s.before,
                    };
                    let restore_draft = draft == submitted;
                    match pending {
                        Pending::Commit(s) => {
                            session.source = s.after.clone();
                            self.undo.push(s);
                            self.redo.clear();
                        }
                        Pending::Undo(s) => {
                            session.source = s.before.clone();
                            self.undo.pop();
                            self.redo.push(s);
                        }
                        Pending::Redo(s) => {
                            session.source = s.after.clone();
                            self.redo.pop();
                            self.undo.push(s);
                        }
                    }
                    if self.projected_history {
                        session.source = session
                            .project_source()
                            .map_err(|_| CommandError::Refresh)?;
                    }
                    if restore_draft {
                        *draft = session.source.clone();
                    }
                }
                self.bound_history();
                self.rebind(session, now)?;
                changed
            }
        };
        Ok(CommandResult {
            changed,
            file,
            state: session.service.session().state(now),
        })
    }
}
