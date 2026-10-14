//! Owned preparation/admission boundary shared by human, script and adapter callers.
//! Plans and previews grant no authority: current host policy is checked again at apply.
use crate::{operations::*, ports::LocalCommit, revision::ObservedRevision, session::*};
use editac_core::{
    commands::{
        editor::{EditorCommand, EditorError, PreparedEditorEdit},
        text::TextOrigin,
    },
    document::Document,
    modules::content::Content,
    projection::Snapshot,
};
use editac_markdown::{
    optional::Codecs,
    reconcile::{Conflict, PreparedSourceEdit, SourceBaseline, SourceMode},
};

#[derive(Clone, Copy, Debug)]
pub struct HostPolicy {
    pub writable: bool,
    /// Historical views cannot admit writes or history operations.
    pub history_view: bool,
    pub codecs: Codecs,
    pub references: bool,
    pub max_commands: usize,
    pub max_input_bytes: usize,
}
impl Default for HostPolicy {
    fn default() -> Self {
        Self {
            writable: true,
            history_view: false,
            codecs: Codecs::default(),
            references: false,
            max_commands: 128,
            max_input_bytes: 1_048_576,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Capabilities {
    pub can_prepare: bool,
    pub can_apply: bool,
    pub can_history: bool,
    pub policy: HostPolicy,
}
pub struct ReadSnapshot {
    pub snapshot: Snapshot,
    pub revision: ObservedRevision,
    pub state: SessionState,
    pub capabilities: Capabilities,
}
#[derive(Debug)]
pub enum ServiceError {
    Session(SessionError),
    Engine(EditorError),
    Source(Conflict),
    ReadOnly,
    HistoryView,
    /// Pre-existing history lacks service capability/limit qualification.
    UnqualifiedHistory,
    UnsupportedModule,
    ResourceLimit,
    Operation(OperationError),
    /// Recoverable exact source inputs and the current semantic snapshot.
    SourceConflict {
        conflict: Conflict,
        base: String,
        proposed: String,
        current: Snapshot,
    },
}
impl From<SessionError> for ServiceError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}
impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ServiceError {}

pub struct SourceTarget {
    baseline: SourceBaseline,
    mode: SourceMode,
}
impl SourceTarget {
    pub fn source(&self) -> &str {
        self.baseline.source()
    }
}
#[derive(Clone, Copy, Default)]
struct Requirements {
    commands: usize,
    bytes: usize,
    callouts: bool,
    images: bool,
    tables: bool,
    references: bool,
}
impl Requirements {
    fn include(&mut self, other: Self) {
        self.commands = self.commands.max(other.commands).max(1);
        self.bytes = self.bytes.max(other.bytes);
        self.callouts |= other.callouts;
        self.images |= other.images;
        self.tables |= other.tables;
        self.references |= other.references;
    }
}
pub(crate) enum PreparedPlan {
    Commands(PreparedEditorEdit),
    Source(PreparedSourceEdit),
}
/// Opaque and single-use. The host cannot rewrite its expected revision or staged delta.
pub struct PreparedEdit {
    expected: ObservedRevision,
    plan: PreparedPlan,
    requirements: Requirements,
    preview: Snapshot,
    engine_origin: String,
}
impl PreparedEdit {
    pub fn preview(&self) -> &Snapshot {
        &self.preview
    }
    pub fn expected_revision(&self) -> &ObservedRevision {
        &self.expected
    }
}

pub struct EditService<S: SessionStore> {
    session: Session<S>,
    policy: HostPolicy,
    history_requirements: Requirements,
    history_qualified: bool,
    operations: OperationLog,
}
impl<S: SessionStore> EditService<S> {
    pub fn new(session: Session<S>, policy: HostPolicy) -> Self {
        let history = session.document().history_state();
        let history_qualified = history.undo_groups == 0 && history.redo_groups == 0;
        Self {
            session,
            policy,
            history_requirements: Requirements::default(),
            history_qualified,
            operations: OperationLog::default(),
        }
    }
    /// Read-only access permits anchors, observations and labeled recovery exports.
    pub fn session(&self) -> &Session<S> {
        &self.session
    }
    pub fn set_policy(&mut self, policy: HostPolicy) {
        self.policy = policy;
    }
    pub fn set_permission(&mut self, permission: CachedPermission) {
        self.session.set_permission(permission);
    }
    pub fn recover(&mut self) -> Result<(), SessionError> {
        self.session.recover()
    }
    pub fn capabilities(&self, now: u64) -> Capabilities {
        let allowed = self.admit(now).is_ok();
        let editor = self.session.document().identity().has_editor();
        Capabilities {
            can_prepare: allowed && editor,
            can_apply: allowed && editor,
            can_history: allowed && self.history_admit().is_ok(),
            policy: self.policy,
        }
    }
    pub fn read(&self, now: u64) -> Result<ReadSnapshot, ServiceError> {
        Ok(ReadSnapshot {
            snapshot: self
                .session
                .document()
                .snapshot()
                .map_err(|e| ServiceError::Engine(EditorError::Text(e.into())))?,
            revision: self.session.observed_revision()?,
            state: self.session.state(now),
            capabilities: self.capabilities(now),
        })
    }
    /// Coalesced observation of current semantic state, without mutable-engine callbacks.
    /// Peers remain later events; this API makes no complete event-log promise.
    pub fn observe(
        &self,
        previous: &ObservedRevision,
        now: u64,
    ) -> Result<Option<ReadSnapshot>, ServiceError> {
        let current = self.session.observed_revision()?;
        match previous.compare(&current) {
            Ok(()) => Ok(None),
            Err(crate::revision::RevisionError::Stale) => Ok(Some(self.read(now)?)),
            Err(error) => Err(SessionError::Revision(error).into()),
        }
    }
    /// Local durability only. Missing or retired records return None, never a remote claim.
    pub fn local_receipt(
        &self,
        update: &crate::ports::UpdateId,
    ) -> Result<Option<LocalCommit>, ServiceError> {
        Ok(self.session.local_receipt(update)?)
    }
    fn admit(&self, now: u64) -> Result<(), ServiceError> {
        self.session.admit(now)?;
        if !self.policy.writable {
            return Err(ServiceError::ReadOnly);
        }
        if self.policy.history_view {
            return Err(ServiceError::HistoryView);
        }
        Ok(())
    }
    fn requirements(&self, r: Requirements) -> Result<(), ServiceError> {
        if r.commands > self.policy.max_commands.min(4096)
            || r.bytes > self.policy.max_input_bytes.min(1_048_576)
        {
            return Err(ServiceError::ResourceLimit);
        }
        if (r.callouts && !self.policy.codecs.callouts)
            || (r.images && !self.policy.codecs.images)
            || (r.tables && !self.policy.codecs.tables)
            || (r.references && !self.policy.references)
        {
            return Err(ServiceError::UnsupportedModule);
        }
        Ok(())
    }
    fn history_admit(&self) -> Result<(), ServiceError> {
        if !self.history_qualified {
            return Err(ServiceError::UnqualifiedHistory);
        }
        self.requirements(self.history_requirements)
    }
    fn replica(&self) -> Result<Document, ServiceError> {
        let checkpoint = self.session.document().checkpoint();
        Document::restore(&checkpoint.identity, &checkpoint, &[])
            .map_err(|e| SessionError::Document(e).into())
    }
    pub fn prepare_commands(
        &self,
        expected: &ObservedRevision,
        commands: &[EditorCommand],
        origin: &TextOrigin,
        now: u64,
    ) -> Result<PreparedEdit, ServiceError> {
        self.admit(now)?;
        self.session.compare_revision(expected)?;
        let mut requirements = Requirements {
            commands: commands.len(),
            ..Requirements::default()
        };
        // Include identifiers, anchors and tuples as well as text. Engine bounds remain authoritative.
        if commands.len() > self.policy.max_commands.min(4096) {
            return Err(ServiceError::ResourceLimit);
        }
        for command in commands {
            requirements.bytes = requirements
                .bytes
                .saturating_add(format!("{command:?}").len());
            match command {
                EditorCommand::Content { content, .. } => match content {
                    Content::Image { .. } => requirements.images = true,
                    Content::Table { .. } | Content::RichTable { .. } => requirements.tables = true,
                    Content::Callout { .. } => requirements.callouts = true,
                    Content::Reference { .. } => requirements.references = true,
                    Content::InertExtension { .. } => {}
                },
                EditorCommand::TableCell { .. } => requirements.tables = true,
                _ => {}
            }
        }
        self.requirements(requirements)?;
        let plan = self
            .session
            .document()
            .prepare_editor(commands, origin)
            .map_err(ServiceError::Engine)?;
        let mut replica = self.replica()?;
        let preview_plan = replica
            .prepare_editor(commands, origin)
            .map_err(ServiceError::Engine)?;
        replica
            .commit_editor(preview_plan)
            .map_err(ServiceError::Engine)?;
        let preview = replica
            .snapshot()
            .map_err(|e| ServiceError::Engine(EditorError::Text(e.into())))?;
        Ok(PreparedEdit {
            expected: expected.clone(),
            plan: PreparedPlan::Commands(plan),
            requirements,
            preview,
            engine_origin: origin.as_str().into(),
        })
    }
    /// Capture exact source-to-owner mapping. URI text or whole-document Markdown is never an identity map.
    pub fn capture_source(
        &self,
        source: &str,
        mode: SourceMode,
        owners: &[String],
        now: u64,
    ) -> Result<SourceTarget, ServiceError> {
        self.admit(now)?;
        self.requirements(source_requirements(
            mode,
            source
                .len()
                .saturating_add(owners.iter().map(String::len).sum::<usize>()),
        ))?;
        let current = self.read(now)?.snapshot;
        let baseline = SourceBaseline::capture(self.session.document(), source, mode, owners)
            .map_err(|conflict| ServiceError::SourceConflict {
                conflict,
                base: source.into(),
                proposed: source.into(),
                current,
            })?;
        Ok(SourceTarget { baseline, mode })
    }
    pub fn prepare_source(
        &self,
        target: &SourceTarget,
        expected: &ObservedRevision,
        proposed: &str,
        origin: &TextOrigin,
        now: u64,
    ) -> Result<PreparedEdit, ServiceError> {
        self.admit(now)?;
        self.session.compare_revision(expected)?;
        let requirements = source_requirements(
            target.mode,
            target.source().len().saturating_add(proposed.len()),
        );
        self.requirements(requirements)?;
        let current = self.read(now)?.snapshot;
        let conflict = |error| ServiceError::SourceConflict {
            conflict: error,
            base: target.source().into(),
            proposed: proposed.into(),
            current: current.clone(),
        };
        let plan = target
            .baseline
            .prepare(self.session.document(), proposed, origin)
            .map_err(&conflict)?;
        let mut replica = self.replica()?;
        let preview_plan = target
            .baseline
            .prepare(&replica, proposed, origin)
            .map_err(&conflict)?;
        preview_plan.commit(&mut replica).map_err(&conflict)?;
        let preview = replica
            .snapshot()
            .map_err(|e| ServiceError::Engine(EditorError::Text(e.into())))?;
        Ok(PreparedEdit {
            expected: expected.clone(),
            plan: PreparedPlan::Source(plan),
            requirements,
            preview,
            engine_origin: origin.as_str().into(),
        })
    }
    /// Current policy and expected state are checked under the exclusive endpoint owner.
    pub fn apply(
        &mut self,
        prepared: PreparedEdit,
        now: u64,
    ) -> Result<Option<LocalCommit>, ServiceError> {
        let operation_id = self.operations.next().map_err(ServiceError::Operation)?;
        self.apply_as(
            prepared,
            HostOperation {
                actor: None,
                origin: OperationOrigin::Unattributed,
                request_id: format!("endpoint-{operation_id}"),
            },
            now,
        )
    }
    /// Trusted host metadata is supplied at admission, never inferred from a plan.
    pub fn apply_as(
        &mut self,
        prepared: PreparedEdit,
        host: HostOperation,
        now: u64,
    ) -> Result<Option<LocalCommit>, ServiceError> {
        self.admit(now)?;
        self.requirements(prepared.requirements)?;
        self.session.compare_revision(&prepared.expected)?;
        host.validate().map_err(ServiceError::Operation)?;
        let operation_id = self.operations.next().map_err(ServiceError::Operation)?;
        let before = prepared.expected;
        let result = self
            .session
            .apply_prepared_service(&before, prepared.plan, now);
        if matches!(
            &result,
            Ok(Some(_)) | Err(ServiceError::Session(SessionError::Store(_)))
        ) {
            self.history_requirements.include(prepared.requirements);
        }
        self.record(operation_id, host, prepared.engine_origin, before, &result)?;
        result
    }
    /// Transport/journal origins remain untrusted. Remote integration never joins typing.
    pub fn receive_peer(
        &mut self,
        entry: crate::ports::JournalEntry,
    ) -> Result<LocalCommit, ServiceError> {
        let before = self.session.observed_revision()?;
        let id = self.operations.next().map_err(ServiceError::Operation)?;
        let engine_origin = entry.origin().to_owned();
        let result = self
            .session
            .receive_peer(entry)
            .map(Some)
            .map_err(ServiceError::Session);
        self.record(
            id,
            HostOperation {
                actor: None,
                origin: OperationOrigin::Replay,
                request_id: format!("endpoint-{id}"),
            },
            engine_origin,
            before,
            &result,
        )?;
        result.map(|commit| commit.unwrap())
    }
    /// Oldest-first bounded records. Eviction/reopen loses provenance, never edits.
    pub fn operations(&self) -> &std::collections::VecDeque<OperationRecord> {
        self.operations.records()
    }
    pub fn operation_bytes(&self) -> usize {
        self.operations.bytes()
    }
    fn record(
        &mut self,
        operation_id: u64,
        host: HostOperation,
        engine_origin: String,
        before: ObservedRevision,
        result: &Result<Option<LocalCommit>, ServiceError>,
    ) -> Result<(), ServiceError> {
        if !matches!(
            result,
            Ok(Some(_)) | Err(ServiceError::Session(SessionError::Store(_)))
        ) {
            return Ok(());
        }
        let after = ObservedRevision::observe(self.session.document())
            .map_err(|e| ServiceError::Session(SessionError::Revision(e)))?;
        if before != after {
            let outcome = match result {
                Ok(Some(commit)) => OperationOutcome::LocalCommit(commit.clone()),
                _ => OperationOutcome::RecoveryRequired,
            };
            self.operations.push(OperationRecord {
                operation_id,
                request_id: host.request_id.into_boxed_str().into_string(),
                actor: host.actor,
                origin: host.origin,
                engine_origin,
                before,
                after,
                outcome,
            });
        }
        Ok(())
    }
    pub fn undo(
        &mut self,
        expected: &ObservedRevision,
        now: u64,
    ) -> Result<Option<LocalCommit>, ServiceError> {
        self.admit(now)?;
        self.history_admit()?;
        self.history_operation(expected, now, false)
    }
    pub fn redo(
        &mut self,
        expected: &ObservedRevision,
        now: u64,
    ) -> Result<Option<LocalCommit>, ServiceError> {
        self.admit(now)?;
        self.history_admit()?;
        self.history_operation(expected, now, true)
    }
    fn history_operation(
        &mut self,
        expected: &ObservedRevision,
        now: u64,
        redo: bool,
    ) -> Result<Option<LocalCommit>, ServiceError> {
        let id = self.operations.next().map_err(ServiceError::Operation)?;
        let result = if redo {
            self.session.redo_if_revision(expected, now)
        } else {
            self.session.undo_if_revision(expected, now)
        }
        .map_err(ServiceError::Session);
        self.record(
            id,
            HostOperation {
                actor: None,
                origin: if redo {
                    OperationOrigin::Redo
                } else {
                    OperationOrigin::Undo
                },
                request_id: format!("endpoint-{id}"),
            },
            if redo { "session:redo" } else { "session:undo" }.into(),
            expected.clone(),
            &result,
        )?;
        result
    }
}
fn source_requirements(mode: SourceMode, bytes: usize) -> Requirements {
    let codecs = match mode {
        SourceMode::Markdown(codecs) => codecs,
        SourceMode::Literal => Codecs::default(),
    };
    Requirements {
        bytes,
        callouts: codecs.callouts,
        images: codecs.images,
        tables: codecs.tables,
        ..Requirements::default()
    }
}
