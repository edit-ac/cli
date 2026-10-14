//! Host lifecycle and presentation recovery. SQLite remains the semantic authority;
//! a sealed private draft is never installed as a replacement document/checkpoint.
use super::{
    commands::{Command, Commands},
    conflict::{ConflictView, Pane, Reason},
    session::{Representation, TerminalSession},
    view::View,
};
use crate::{
    reconcile::LocalBaseline,
    storage::{Draft, IoHooks, NativeIo, NativeStorage, StoredIdentity},
    target::{self, ResolveOptions},
};
use editac_session::{
    edit_service::{EditService, HostPolicy},
    ports::{JournalEntry, StoreLimits, UpdateId},
    revision::ObservedRevision,
    session::{CachedPermission, Readiness, Session, SessionStore},
    store::NativeStore,
};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Checkpoint,
    Quit,
    RecoverableExit,
    CancelExit,
    Escape,
    Interrupt,
    Suspend,
    Resume,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Pass,
    Continue,
    Exit,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    writer: u32,
    sequence: u64,
    checksum: [u8; 32],
    origin: String,
    bytes: Vec<u8>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Conflict {
    base: String,
    local: String,
    current: String,
    resolution: String,
    base_label: String,
    current_label: String,
    pane: String,
    anchor: u32,
    caret: u32,
    top: usize,
    reason: String,
    open: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    committed: String,
    opening: String,
    opening_revision: Vec<u8>,
    draft: String,
    revision: Vec<u8>,
    literal: bool,
    exact: bool,
    target: Option<PathBuf>,
    file_source: Option<String>,
    anchor: u32,
    caret: u32,
    conflict: Option<Conflict>,
    pending: Option<Pending>,
}
fn error(_: impl std::fmt::Debug) -> io::Error {
    io::Error::other("TUI recovery failed; retain store and private draft")
}
fn reason(value: &str) -> io::Result<Reason> {
    use crate::markdown::reconcile::ConflictKind::*;
    for kind in [
        AmbiguousMapping,
        UnsupportedContent,
        Overlap,
        DeletedTarget,
        MovedTarget,
        ResourceLimit,
        IdentityMismatch,
        StalePlan,
        EngineRejected,
    ] {
        if value == format!("Mapping({kind:?})") {
            return Ok(Reason::Mapping(kind));
        }
    }
    match value {
        "Incompatible" => Ok(Reason::Incompatible),
        "Unavailable" => Ok(Reason::Unavailable),
        _ => Err(error("invalid reason")),
    }
}
pub fn recovery_path(store: &Path) -> PathBuf {
    let mut name = store.as_os_str().to_owned();
    name.push(".tui-recovery");
    PathBuf::from(name)
}
pub struct RecoveryOwner {
    storage: NativeStorage,
    _lease: File,
    last: RefCell<Option<Draft>>,
    pub awaiting_exit: bool,
    snapshot_failed: bool,
}
impl RecoveryOwner {
    fn claim(storage: NativeStorage) -> io::Result<Self> {
        let path = storage.root().join("owner.lock");
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err(error("unsafe lease"));
            }
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lease = options.open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let opened = lease.metadata()?;
            let actual = fs::symlink_metadata(&path)?;
            if !actual.is_file()
                || actual.file_type().is_symlink()
                || opened.nlink() != 1
                || opened.mode() & 0o077 != 0
                || (opened.dev(), opened.ino()) != (actual.dev(), actual.ino())
            {
                return Err(error("unsafe lease inode"));
            }
        }
        lease.try_lock().map_err(error)?;
        Ok(Self {
            storage,
            _lease: lease,
            last: RefCell::new(None),
            awaiting_exit: false,
            snapshot_failed: false,
        })
    }
    pub fn for_store(
        store: &Path,
        id: &crate::engine::document::DocumentIdentity,
    ) -> io::Result<Self> {
        Self::claim(NativeStorage::open(recovery_path(store), id.clone()).map_err(error)?)
    }
    pub fn root(&self) -> &Path {
        self.storage.root()
    }
    fn load(&self) -> io::Result<Option<Snapshot>> {
        self.storage
            .read_draft()
            .map_err(error)?
            .map(|record| {
                if record.baseline != "editac-tui-lifecycle-v1" {
                    return Err(error("version"));
                }
                let s: Snapshot = serde_json::from_str(&record.source).map_err(error)?;
                if s.version != 1
                    || [&s.committed, &s.draft]
                        .iter()
                        .any(|s| s.len() > super::conflict::MAX_SOURCE_BYTES)
                {
                    return Err(error("snapshot limit"));
                }
                ObservedRevision::from_bytes(&s.revision).map_err(error)?;
                ObservedRevision::from_bytes(&s.opening_revision).map_err(error)?;
                if s.opening.len() > super::conflict::MAX_SOURCE_BYTES {
                    return Err(error("opening source limit"));
                }
                if let Some(c) = &s.conflict {
                    if [&c.base, &c.local, &c.current, &c.resolution]
                        .iter()
                        .any(|s| s.len() > super::conflict::MAX_SOURCE_BYTES)
                    {
                        return Err(error("conflict limit"));
                    }
                    reason(&c.reason)?;
                }
                Ok(s)
            })
            .transpose()
    }
    pub fn persist<S: SessionStore>(
        &self,
        session: &TerminalSession<S>,
        commands: Option<&Commands>,
        view: &View,
    ) -> io::Result<()> {
        self.persist_with(session, commands, view, &NativeIo)
    }
    pub fn persist_with<S: SessionStore>(
        &self,
        session: &TerminalSession<S>,
        commands: Option<&Commands>,
        view: &View,
        hooks: &dyn IoHooks,
    ) -> io::Result<()> {
        let recovery = session.service.session().recovery_export();
        let revision = session
            .service
            .read(0)
            .ok()
            .map(|r| r.revision)
            .or_else(|| commands.map(|c| c.revision().clone()))
            .ok_or_else(|| error("no observed revision"))?;
        // Catalog aliases may refer to this same local lineage. Retain the
        // original file recovery binding as metadata; a hosted session receives
        // no LocalBaseline and therefore no filesystem publication authority.
        let previous = if session.local_baseline.is_none() {
            self.load()?
        } else {
            None
        };
        let snapshot = Snapshot {
            version: 1,
            committed: commands
                .map_or(session.source.as_str(), |c| {
                    c.recovery_source(&session.source)
                })
                .into(),
            opening: commands
                .map_or(session.source.as_str(), |c| c.baseline())
                .into(),
            opening_revision: commands.map_or_else(
                || revision.to_bytes().to_vec(),
                |c| c.opening_revision().to_bytes().to_vec(),
            ),
            draft: view.source.clone(),
            revision: revision.to_bytes().to_vec(),
            literal: matches!(
                session.mode,
                crate::markdown::reconcile::SourceMode::Literal
            ),
            exact: session.representation == Representation::ExactSource,
            target: session
                .local_baseline
                .as_ref()
                .map(|b| b.target().resolved_path().to_path_buf())
                .or_else(|| previous.as_ref().and_then(|p| p.target.clone())),
            file_source: session
                .local_baseline
                .as_ref()
                .map(|b| b.target().source().into())
                .or_else(|| previous.as_ref().and_then(|p| p.file_source.clone())),
            anchor: view.input.as_ref().map_or(0, |i| i.anchor.0),
            caret: view.input.as_ref().map_or(0, |i| i.caret.0),
            conflict: view.conflict.as_ref().map(|c| Conflict {
                base: c.base.clone(),
                local: c.local.clone(),
                current: c.current.clone(),
                resolution: c.resolution.clone(),
                base_label: c.base_label.clone(),
                current_label: c.current_label.clone(),
                pane: format!("{:?}", c.pane),
                anchor: c.input.anchor.0,
                caret: c.input.caret.0,
                top: c.viewport_top(),
                reason: format!("{:?}", c.reason),
                open: c.open,
            }),
            pending: recovery.unjournaled.map(|e| Pending {
                writer: e.id().writer,
                sequence: e.id().sequence,
                checksum: e.id().checksum,
                origin: e.origin().into(),
                bytes: e.bytes().into(),
            }),
        };
        if [&snapshot.committed, &snapshot.draft]
            .iter()
            .any(|s| s.len() > super::conflict::MAX_SOURCE_BYTES)
        {
            return Err(error("snapshot limit"));
        }
        let draft = Draft {
            baseline: "editac-tui-lifecycle-v1".into(),
            source: serde_json::to_string(&snapshot).map_err(error)?,
        };
        if self.last.borrow().as_ref() == Some(&draft) {
            return Ok(());
        }
        self.storage
            .persist_draft_with(&draft, hooks)
            .map_err(error)?;
        *self.last.borrow_mut() = Some(draft);
        Ok(())
    }
    /// Settle only the exact admitted operation in its original reserved writer lane.
    pub fn settle<S: SessionStore>(&self, store: &S) -> io::Result<()> {
        if let Some(s) = self.load()? {
            if let Some(p) = s.pending {
                let entry = JournalEntry::from_stored(
                    UpdateId {
                        identity: self.storage.identity().clone(),
                        writer: p.writer,
                        sequence: p.sequence,
                        checksum: p.checksum,
                    },
                    p.origin,
                    p.bytes,
                )
                .map_err(error)?;
                let retained = store.recover(self.storage.identity()).map_err(error)?;
                let mut replay = retained
                    .journal
                    .iter()
                    .map(|e| crate::engine::document::ReplayUpdate {
                        identity: e.id().identity.clone(),
                        bytes: e.bytes().to_vec(),
                    })
                    .collect::<Vec<_>>();
                replay.push(crate::engine::document::ReplayUpdate {
                    identity: entry.id().identity.clone(),
                    bytes: entry.bytes().to_vec(),
                });
                let qualify = |checkpoint: &editac_session::ports::CheckpointRecord| {
                    crate::engine::document::Document::restore(
                        self.storage.identity(),
                        checkpoint.checkpoint(),
                        &replay,
                    )
                };
                if qualify(&retained.checkpoint).is_err()
                    && !retained
                        .fallback
                        .as_ref()
                        .is_some_and(|p| qualify(p).is_ok())
                {
                    return Err(error("unjournaled update rejected by shared engine"));
                }
                store.append(entry).map_err(error)?;
            }
        }
        Ok(())
    }
    pub fn restore_source<S: SessionStore>(
        &self,
        session: &mut TerminalSession<S>,
    ) -> io::Result<()> {
        if let Some(s) = self.load()? {
            if StoredIdentity::from(session.service.session().document().identity())
                != StoredIdentity::from(self.storage.identity())
                || s.literal
                    != matches!(
                        session.mode,
                        crate::markdown::reconcile::SourceMode::Literal
                    )
            {
                return Err(error("scope mismatch"));
            }
            let same = session
                .service
                .read(0)
                .map_err(error)?
                .revision
                .to_bytes()
                .as_slice()
                == s.revision;
            if same || s.pending.is_some() {
                // Shared mapping must qualify exact recovered spelling before admission.
                let old = std::mem::replace(&mut session.source, s.committed);
                if super::input::SourceInput::capture(session, 0).is_err() {
                    session.source = old;
                }
            }
        }
        Ok(())
    }
    pub fn restore_view<S: SessionStore>(
        &self,
        session: &TerminalSession<S>,
        view: &mut View,
    ) -> io::Result<()> {
        if let Some(s) = self.load()? {
            let current = session.service.read(0).map_err(error)?.revision;
            let previous = ObservedRevision::from_bytes(&s.revision).map_err(error)?;
            view.source = s.draft;
            view.draft_changed = view.source != session.source;
            if let Some(i) = &mut view.input {
                i.select(
                    &view.source,
                    crate::engine::Utf16Offset(s.anchor),
                    crate::engine::Utf16Offset(s.caret),
                )
                .map_err(error)?;
            }
            if let Some(c) = s.conflict {
                let mut conflict = ConflictView::capture(
                    &c.base,
                    &c.local,
                    &c.current,
                    &previous,
                    current.clone(),
                    reason(&c.reason)?,
                )
                .map_err(error)?;
                conflict.resolution = c.resolution;
                conflict.base_label = c.base_label;
                conflict.current_label = c.current_label;
                conflict.open = c.open;
                let pane = match c.pane.as_str() {
                    "Base" => Pane::Base,
                    "Local" => Pane::Local,
                    "Current" => Pane::Current,
                    "Resolution" => Pane::Resolution,
                    _ => return Err(error("invalid conflict pane")),
                };
                conflict
                    .restore_presentation(
                        pane,
                        crate::engine::Utf16Offset(c.anchor),
                        crate::engine::Utf16Offset(c.caret),
                        c.top,
                    )
                    .map_err(error)?;
                if current != previous {
                    conflict
                        .advance(
                            &conflict.local.clone(),
                            &session.source,
                            current,
                            Reason::Mapping(crate::markdown::reconcile::ConflictKind::StalePlan),
                        )
                        .map_err(error)?;
                }
                view.conflict = Some(conflict);
            } else if view.draft_changed && (current != previous || s.opening != session.source) {
                view.conflict = Some(
                    ConflictView::capture(
                        &s.opening,
                        &view.source,
                        &session.source,
                        &ObservedRevision::from_bytes(&s.opening_revision).map_err(error)?,
                        current,
                        Reason::Mapping(crate::markdown::reconcile::ConflictKind::StalePlan),
                    )
                    .map_err(error)?,
                );
            }
            view.input_message =
                Some("Recovered local draft; remote durability comes from shared receipts".into());
        }
        Ok(())
    }
    pub fn handle<S: SessionStore>(
        &mut self,
        event: Event,
        session: &mut TerminalSession<S>,
        commands: Option<&mut Commands>,
        view: &mut View,
        now: u64,
    ) -> io::Result<Action> {
        self.handle_with(event, session, commands, view, now, &NativeIo)
    }
    pub fn handle_with<S: SessionStore>(
        &mut self,
        event: Event,
        session: &mut TerminalSession<S>,
        mut commands: Option<&mut Commands>,
        view: &mut View,
        now: u64,
        hooks: &dyn IoHooks,
    ) -> io::Result<Action> {
        let event = if event == Event::Escape {
            if self.awaiting_exit {
                Event::CancelExit
            } else if view.review.is_some() || view.conflict.as_ref().is_some_and(|c| c.open) {
                return Ok(Action::Pass);
            } else {
                Event::Quit
            }
        } else {
            event
        };
        if event == Event::CancelExit {
            if !self.awaiting_exit {
                return Ok(Action::Pass);
            }
            self.awaiting_exit = false;
            view.input_message = None;
            return Ok(Action::Continue);
        }
        if let Err(failure) = self.persist_with(session, commands.as_deref(), view, hooks) {
            self.awaiting_exit = true;
            self.snapshot_failed = true;
            if let Some(input) = &mut view.input {
                input.writable = false;
            }
            view.input_message = Some("Draft checkpoint failed; in-memory draft retained. Ctrl-Q retries; Ctrl-E exits only after private recovery is durable; Esc cancels exit.".into());
            return if matches!(event, Event::Interrupt | Event::Suspend) {
                Err(failure)
            } else {
                Ok(Action::Continue)
            };
        }
        if self.snapshot_failed {
            if let Some(input) = &mut view.input {
                input.writable = session.service.capabilities(now).can_apply
                    && commands.is_some()
                    && view.review.is_none()
                    && !view.conflict.as_ref().is_some_and(|c| c.open);
            }
            self.snapshot_failed = false;
        }
        if event == Event::Checkpoint || event == Event::Resume {
            return Ok(Action::Continue);
        }
        if event == Event::RecoverableExit {
            if self.awaiting_exit {
                session.service.detach_transport();
                return Ok(Action::Exit);
            }
            return Ok(Action::Pass);
        }
        let mut failed = false;
        if session.service.session().state(now).readiness == Readiness::RecoveryRequired {
            failed = if let Some(c) = commands.as_deref_mut() {
                c.execute(
                    session,
                    Command::Recover,
                    &mut view.source,
                    now,
                    self.root(),
                )
                .is_err()
            } else {
                session.service.recover().is_err()
            };
        }
        let state = session.service.session().state(now);
        if !failed
            && state.pending_upload > 0
            && state.auth == editac_session::session::AuthState::Authorized
        {
            failed = session.service.flush_pending(now).is_err();
        }
        self.persist_with(session, commands.as_deref(), view, hooks)?;
        view.status = View::from_session(session, now).status;
        if failed && event == Event::Quit {
            self.awaiting_exit = true;
            view.input_message = Some("Flush failed. Ctrl-R retries recovery; Ctrl-Q retries exit; Ctrl-E exits with private recovery; Esc cancels exit. Remote durability not asserted.".into());
            return Ok(Action::Continue);
        }
        if event == Event::Suspend {
            session.service.detach_transport();
            return Ok(Action::Continue);
        }
        session.service.detach_transport();
        Ok(Action::Exit)
    }
    pub fn finish<S: SessionStore>(
        &self,
        session: &mut TerminalSession<S>,
        commands: Option<&Commands>,
        view: &View,
    ) -> io::Result<()> {
        let result = self.persist(session, commands, view);
        session.service.detach_transport();
        if result.is_ok() {
            return result;
        }
        // Terminal I/O may already be unusable. Preserve complete recovery through
        // the qualified export path instead of discarding the in-memory draft.
        let exported = if let Some(conflict) = &view.conflict {
            conflict.export_private(session, 0, &std::env::temp_dir())
        } else {
            let revision = session
                .service
                .read(0)
                .ok()
                .map(|r| r.revision)
                .or_else(|| commands.map(|c| c.revision().clone()));
            revision
                .ok_or(super::conflict::ConflictError::Unavailable)
                .and_then(|revision| {
                    let current = session
                        .project_source()
                        .unwrap_or_else(|_| session.source.clone());
                    ConflictView::capture(
                        commands.map_or(session.source.as_str(), |c| c.baseline()),
                        &view.source,
                        &current,
                        &revision,
                        revision.clone(),
                        Reason::Unavailable,
                    )
                    .and_then(|conflict| conflict.export_private(session, 0, &std::env::temp_dir()))
                })
        };
        match exported {
            Ok(path) => Err(io::Error::other(format!(
                "TUI recovery snapshot failed; private recovery export: {}",
                path.display()
            ))),
            Err(_) => Err(io::Error::other(
                "TUI draft snapshot and emergency export failed; local journal retained, latest in-memory draft is not durable",
            )),
        }
    }
}

pub fn reopen_local(
    path: &Path,
    options: ResolveOptions,
    store_path: &Path,
) -> Result<TerminalSession<NativeStore>, &'static str> {
    let storage = NativeStorage::open_draft(recovery_path(store_path))
        .map_err(|_| "local recovery metadata unavailable; retain store")?;
    let id = storage.identity().clone();
    let recovery =
        RecoveryOwner::claim(storage).map_err(|_| "local recovery already owned or unavailable")?;
    let snapshot = recovery
        .load()
        .map_err(|_| "invalid local recovery")?
        .ok_or("missing local recovery")?;
    let target = target::resolve(path, options).map_err(|_| "local recovery target unavailable")?;
    if snapshot.target.as_deref() != Some(target.resolved_path())
        || snapshot.file_source.as_deref() != Some(target.source())
        || snapshot.literal != (target.format() == target::Format::Text)
    {
        return Err("local recovery target changed; retain private draft and store");
    }
    let store = NativeStore::open_existing(store_path, id.clone(), StoreLimits::default())
        .map_err(|_| "local recovery store unavailable")?;
    recovery
        .settle(&store)
        .map_err(|_| "exact unjournaled recovery failed; retain private draft")?;
    let writable = !matches!(
        target.baseline(),
        target::Baseline::Existing { readonly: true, .. }
    );
    let owner = Session::open(
        store,
        &id,
        CachedPermission {
            writable,
            expires_at: None,
        },
    )
    .map_err(|_| "local session recovery failed")?;
    let mut session = TerminalSession {
        service: EditService::new(owner, HostPolicy::default()),
        recovery: None,
        source: String::new(),
        mode: if snapshot.literal {
            crate::markdown::reconcile::SourceMode::Literal
        } else {
            crate::markdown::reconcile::SourceMode::Markdown(Default::default())
        },
        representation: if snapshot.exact {
            Representation::ExactSource
        } else {
            Representation::HostedProjection
        },
        local_baseline: None,
    };
    session.source = session.project_source()?;
    recovery
        .restore_source(&mut session)
        .map_err(|_| "local source recovery failed")?;
    let owners = session
        .service
        .read(0)
        .map_err(|_| "local recovery snapshot unavailable")?
        .snapshot
        .visible
        .iter()
        .filter(|n| n.kind == "block")
        .map(|n| n.id.clone())
        .collect::<Vec<_>>();
    session.local_baseline = Some(
        LocalBaseline::capture_recovered(
            session.service.session().document(),
            target,
            &session.source,
            &owners,
        )
        .map_err(|_| "local recovered mapping unavailable")?,
    );
    session.recovery = Some(recovery);
    Ok(session)
}

/// Async signal handlers set flags only. Persistence/flush run in the serialized
/// owner; SIGKILL/power loss rely on the last completed native draft/journal barrier.
#[cfg(unix)]
pub struct Signals {
    ids: Vec<signal_hook::SigId>,
    interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    suspend: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
#[cfg(unix)]
impl Signals {
    pub fn new() -> io::Result<Self> {
        use std::sync::{Arc, atomic::AtomicBool};
        let mut owner = Self {
            ids: vec![],
            interrupt: Arc::new(AtomicBool::new(false)),
            suspend: Arc::new(AtomicBool::new(false)),
        };
        for signal in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
        ] {
            owner.ids.push(signal_hook::flag::register(
                signal,
                owner.interrupt.clone(),
            )?);
        }
        owner.ids.push(signal_hook::flag::register(
            signal_hook::consts::SIGTSTP,
            owner.suspend.clone(),
        )?);
        Ok(owner)
    }
    pub fn poll(&self) -> Option<Event> {
        use std::sync::atomic::Ordering;
        if self.interrupt.swap(false, Ordering::SeqCst) {
            Some(Event::Interrupt)
        } else if self.suspend.swap(false, Ordering::SeqCst) {
            Some(Event::Suspend)
        } else {
            None
        }
    }
}
#[cfg(unix)]
impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}
