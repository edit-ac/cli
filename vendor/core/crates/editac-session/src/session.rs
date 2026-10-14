//! One local replica owner. All mutation and publication passes through this handle.
//! Clocks and authorization come from the host; no socket or credential discovery.
use crate::{
    locator::{Catalog, Connectivity, Locator, LocatorError, ResolutionState},
    ports::{
        memory::{FaultInjectedStore, MemoryStore},
        *,
    },
};
use editac_core::{
    commands::text::{TextCommand, TextError, TextOrigin},
    document::{
        CreationPermit, Document, DocumentError, DocumentIdentity, InitialNode,
        InitializedCheckpoint, ReplayUpdate,
    },
    history::{HistoryConfig, HistoryError},
};

/// Lifecycle-specific writer reservation, alongside the unchanged conformance port.
pub trait SessionStore: LocalStore {
    fn claim_writer(&self, writer: u32) -> Result<(), PortError>;
}
impl<S: SessionStore + ?Sized> LocalStore for std::sync::Arc<S> {
    fn capabilities(&self) -> StoreCapabilities {
        (**self).capabilities()
    }
    fn append(&self, entry: JournalEntry) -> Result<LocalCommit, PortError> {
        (**self).append(entry)
    }
    fn generation(&self, id: &DocumentIdentity) -> Result<u64, PortError> {
        (**self).generation(id)
    }
    fn publish(
        &self,
        generation: u64,
        checkpoint: CheckpointRecord,
    ) -> Result<PublicationCommit, PortError> {
        (**self).publish(generation, checkpoint)
    }
    fn recover(&self, id: &DocumentIdentity) -> Result<Recovery, PortError> {
        (**self).recover(id)
    }
    fn pending(&self, id: &DocumentIdentity) -> Result<Vec<JournalEntry>, PortError> {
        (**self).pending(id)
    }
    fn accept_receipt(
        &self,
        capability: ReceiptCapability,
        receipt: DurableReceipt,
    ) -> Result<ReceiptResult, PortError> {
        (**self).accept_receipt(capability, receipt)
    }
}
impl<S: SessionStore + ?Sized> SessionStore for std::sync::Arc<S> {
    fn claim_writer(&self, writer: u32) -> Result<(), PortError> {
        (**self).claim_writer(writer)
    }
}
impl SessionStore for MemoryStore {
    fn claim_writer(&self, writer: u32) -> Result<(), PortError> {
        MemoryStore::claim_writer(self, writer)
    }
}
impl SessionStore for FaultInjectedStore {
    fn claim_writer(&self, writer: u32) -> Result<(), PortError> {
        FaultInjectedStore::claim_writer(self, writer)
    }
}
#[cfg(feature = "native-store")]
impl SessionStore for crate::store::NativeStore {
    fn claim_writer(&self, writer: u32) -> Result<(), PortError> {
        Self::claim_writer(self, writer)
    }
}

/// Host-granted cached write permission. Expiry is exclusive, in the host's clock domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CachedPermission {
    pub writable: bool,
    pub expires_at: Option<u64>,
}
impl CachedPermission {
    pub const LOCAL: Self = Self {
        writable: true,
        expires_at: None,
    };
    fn state(self, now: u64) -> PermissionState {
        if !self.writable {
            PermissionState::ReadOnly
        } else if self.expires_at.is_some_and(|expiry| now >= expiry) {
            PermissionState::Expired
        } else {
            PermissionState::Writable
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionState {
    Writable,
    ReadOnly,
    Expired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    RecoveryRequired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalState {
    Initialized(Durability),
    Committed(Durability),
    Unsaved,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteState {
    Unknown,
    SentDurabilityUnknown,
    RemotelyCommitted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthState {
    Detached,
    Authorized,
    Expired,
    Unauthorized,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionState {
    pub readiness: Readiness,
    pub local: LocalState,
    pub pending_upload: usize,
    pub remote: RemoteState,
    pub connection: ConnectionState,
    pub auth: AuthState,
    pub permission: PermissionState,
    pub conflict: bool,
    pub recovery_error: Option<PortError>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum SessionError {
    Store(PortError),
    Document(DocumentError),
    Text(TextError),
    History(HistoryError),
    Revision(crate::revision::RevisionError),
    Locator(LocatorError),
    Unavailable,
    Locked,
    Unauthorized,
    Incompatible,
    ReadOnly,
    PermissionExpired,
    RecoveryRequired,
}
impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SessionError {}
impl From<crate::revision::RevisionError> for SessionError {
    fn from(error: crate::revision::RevisionError) -> Self {
        Self::Revision(error)
    }
}
impl From<PortError> for SessionError {
    fn from(e: PortError) -> Self {
        Self::Store(e)
    }
}
impl From<DocumentError> for SessionError {
    fn from(e: DocumentError) -> Self {
        Self::Document(e)
    }
}
impl From<TextError> for SessionError {
    fn from(e: TextError) -> Self {
        Self::Text(e)
    }
}
impl From<HistoryError> for SessionError {
    fn from(e: HistoryError) -> Self {
        Self::History(e)
    }
}
impl From<LocatorError> for SessionError {
    fn from(e: LocatorError) -> Self {
        Self::Locator(e)
    }
}

/// Labeled in-memory salvage, never a durable acknowledgment or publishable checkpoint.
pub struct RecoveryExport {
    pub checkpoint: InitializedCheckpoint,
    pub unjournaled: Option<JournalEntry>,
    pub locally_committed: bool,
}

pub struct Session<S: SessionStore> {
    document: Document,
    store: S,
    permission: CachedPermission,
    next_sequence: u64,
    pending: Vec<JournalEntry>,
    local: LocalState,
    remote: RemoteState,
    conflict: bool,
    recovery_error: Option<PortError>,
    unjournaled: Option<JournalEntry>,
    initialization: Option<CheckpointRecord>,
    transport: Option<Box<dyn Transport>>,
    auth_expiry: Option<u64>,
}
impl<S: SessionStore> Session<S> {
    /// Idempotent against an already initialized store. Never seeds a failed existing restore.
    /// Host explicitly creates/grants the store; URI opening cannot call this constructor.
    pub fn create_local(
        store: S,
        identity: DocumentIdentity,
        initial: &[InitialNode],
    ) -> Result<Self, SessionError> {
        if store.generation(&identity)? != 0 {
            return Self::open(store, &identity, CachedPermission::LOCAL);
        }
        if !store.pending(&identity)?.is_empty() {
            return Err(PortError::Integrity.into());
        }
        let doc = Document::create(CreationPermit::authorize_new(identity)?, initial)?;
        let checkpoint = CheckpointRecord::new(doc.checkpoint(), vec![])?;
        let mut session = Self::assemble(store, doc, CachedPermission::LOCAL, vec![], false)?;
        match session.store.publish(0, checkpoint.clone()) {
            Ok(commit) => session.local = LocalState::Initialized(commit.durability),
            Err(PortError::Conflict) => return Err(PortError::Conflict.into()),
            Err(error) => {
                session.initialization = Some(checkpoint);
                session.freeze(error);
            }
        }
        Ok(session)
    }
    pub fn open(
        store: S,
        identity: &DocumentIdentity,
        permission: CachedPermission,
    ) -> Result<Self, SessionError> {
        let recovery = store.recover(identity)?;
        let doc = restore(identity, &recovery)?;
        let has_receipts = recovery.journal.len() > recovery.pending.len();
        let has_journal = !recovery.journal.is_empty();
        let mut session = Self::assemble(store, doc, permission, recovery.pending, has_receipts)?;
        if has_journal {
            session.local = LocalState::Committed(session.store.capabilities().durability);
        }
        Ok(session)
    }
    /// Resolve locally and restore before any transport exists. Even online uncached hosted
    /// metadata is unavailable to this offline constructor; fetching is an explicit host workflow.
    pub fn open_locator(
        store: S,
        catalog: &Catalog,
        locator: &Locator,
        permission: CachedPermission,
    ) -> Result<Self, SessionError> {
        let resolved = catalog.resolve(locator, Connectivity::Offline)?;
        match resolved.state {
            ResolutionState::Local | ResolutionState::Cached => Self::open(
                store,
                &resolved.entry.ok_or(SessionError::Unavailable)?.identity,
                permission,
            ),
            ResolutionState::Locked => Err(SessionError::Locked),
            ResolutionState::Unauthorized => Err(SessionError::Unauthorized),
            ResolutionState::Incompatible => Err(SessionError::Incompatible),
            _ => Err(SessionError::Unavailable),
        }
    }
    fn assemble(
        store: S,
        mut document: Document,
        permission: CachedPermission,
        pending: Vec<JournalEntry>,
        has_receipts: bool,
    ) -> Result<Self, SessionError> {
        // Resolve a retained writer collision before admitting any edit.
        for attempt in 0..8 {
            match store.claim_writer(document.writer_id()) {
                Ok(()) => break,
                Err(PortError::Conflict) if attempt < 7 => {
                    document = Document::restore(document.identity(), &document.checkpoint(), &[])?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        document.enable_history(HistoryConfig::default())?;
        let durability = store.capabilities().durability;
        let remote = if has_receipts && pending.is_empty() {
            RemoteState::RemotelyCommitted
        } else {
            RemoteState::Unknown
        };
        Ok(Self {
            document,
            store,
            permission,
            next_sequence: 1,
            pending,
            local: LocalState::Initialized(durability),
            remote,
            conflict: false,
            recovery_error: None,
            unjournaled: None,
            initialization: None,
            transport: None,
            auth_expiry: None,
        })
    }
    /// Immutable engine access only: no mutable escape from permission/persistence admission.
    pub fn document(&self) -> &Document {
        &self.document
    }
    pub fn set_permission(&mut self, permission: CachedPermission) {
        self.permission = permission;
    }
    pub fn state(&self, now: u64) -> SessionState {
        SessionState {
            readiness: if self.recovery_error.is_some() {
                Readiness::RecoveryRequired
            } else {
                Readiness::Ready
            },
            local: self.local,
            pending_upload: self.pending.len(),
            remote: self.remote,
            connection: self
                .transport
                .as_ref()
                .map_or(ConnectionState::Disconnected, |t| t.state()),
            auth: self.auth(now),
            permission: self.permission.state(now),
            conflict: self.conflict,
            recovery_error: self.recovery_error,
        }
    }
    fn auth(&self, now: u64) -> AuthState {
        if self.transport.is_none() {
            AuthState::Detached
        } else if self.auth_expiry.is_some_and(|expiry| now >= expiry) {
            AuthState::Expired
        } else if self
            .transport
            .as_ref()
            .is_some_and(|t| t.state() == ConnectionState::Authorized)
        {
            AuthState::Authorized
        } else {
            AuthState::Unauthorized
        }
    }
    fn ready(&self) -> Result<(), SessionError> {
        if self.recovery_error.is_some() {
            Err(SessionError::RecoveryRequired)
        } else {
            Ok(())
        }
    }
    pub(crate) fn admit(&self, now: u64) -> Result<(), SessionError> {
        self.ready()?;
        if self.next_sequence > MAX_SEQUENCE {
            return Err(PortError::Sequence.into());
        }
        match self.permission.state(now) {
            PermissionState::Writable => Ok(()),
            PermissionState::ReadOnly => Err(SessionError::ReadOnly),
            PermissionState::Expired => Err(SessionError::PermissionExpired),
        }
    }
    /// Exact endpoint-observed state, reconstructed from committed CRDT clocks and deletions.
    /// Recovery exports may be read while frozen, but cannot mint an acknowledged revision.
    pub fn observed_revision(&self) -> Result<crate::revision::ObservedRevision, SessionError> {
        self.ready()?;
        Ok(crate::revision::ObservedRevision::observe(&self.document)?)
    }
    /// Exact local transaction lookup, never a remote durability receipt.
    pub fn local_receipt(&self, update: &UpdateId) -> Result<Option<LocalCommit>, SessionError> {
        self.ready()?;
        if update.identity != *self.document.identity() {
            return Err(SessionError::Incompatible);
        }
        let recovered = self.store.recover(self.document.identity())?;
        let retained = recovered.journal.iter().any(|entry| entry.id() == update)
            || recovered.checkpoint.coverage().contains(update);
        Ok(retained.then(|| LocalCommit {
            update: update.clone(),
            durability: self.store.capabilities().durability,
        }))
    }
    pub(crate) fn apply_prepared_service(
        &mut self,
        expected: &crate::revision::ObservedRevision,
        plan: crate::edit_service::PreparedPlan,
        now: u64,
    ) -> Result<Option<LocalCommit>, crate::edit_service::ServiceError> {
        self.admit(now)?;
        self.compare_revision(expected)?;
        // Every service operation is a conservative gesture boundary. Untrusted
        // equal origin strings cannot merge agent/import/replay into human typing.
        self.document.reset_history_group();
        let commit = match plan {
            crate::edit_service::PreparedPlan::Commands(plan) => Some(
                self.document
                    .commit_editor(plan)
                    .map_err(crate::edit_service::ServiceError::Engine)?,
            ),
            crate::edit_service::PreparedPlan::Source(plan) => {
                plan.commit(&mut self.document)
                    .map_err(crate::edit_service::ServiceError::Source)?
                    .commit
            }
        };
        match commit {
            Some(commit) => Ok(self.capture(Some(commit.update), commit.origin.as_str())?),
            None => Ok(None),
        }
    }
    pub(crate) fn compare_revision(
        &self,
        expected: &crate::revision::ObservedRevision,
    ) -> Result<(), SessionError> {
        expected.compare(&self.observed_revision()?)?;
        Ok(())
    }
    /// Compare and prepare/commit under this replica's exclusive mutable owner. No callback,
    /// await or separately mutable engine escapes between the comparison and mutation.
    /// This does not lock an offline peer or implicitly catch up from shared storage.
    pub fn edit_text_if_revision(
        &mut self,
        expected: &crate::revision::ObservedRevision,
        command: &TextCommand,
        origin: &TextOrigin,
        now: u64,
    ) -> Result<Option<LocalCommit>, SessionError> {
        self.admit(now)?;
        self.compare_revision(expected)?;
        self.commit_text(command, origin)
    }
    /// Text semantics are the existing engine commands; full edit-service prepare is CAP-1.8.
    pub fn edit_text(
        &mut self,
        command: &TextCommand,
        origin: &TextOrigin,
        now: u64,
    ) -> Result<Option<LocalCommit>, SessionError> {
        self.admit(now)?;
        self.commit_text(command, origin)
    }
    fn commit_text(
        &mut self,
        command: &TextCommand,
        origin: &TextOrigin,
    ) -> Result<Option<LocalCommit>, SessionError> {
        let plan = self.document.prepare_text(command, origin)?;
        let commit = self.document.commit_text(plan)?;
        self.capture(commit.update, origin.as_str())
    }
    pub fn undo(&mut self, now: u64) -> Result<Option<LocalCommit>, SessionError> {
        self.admit(now)?;
        let update = self.document.undo()?.and_then(|c| c.update);
        self.capture(update, "session:undo")
    }
    pub fn undo_if_revision(
        &mut self,
        expected: &crate::revision::ObservedRevision,
        now: u64,
    ) -> Result<Option<LocalCommit>, SessionError> {
        self.admit(now)?;
        self.compare_revision(expected)?;
        self.undo(now)
    }
    pub fn redo(&mut self, now: u64) -> Result<Option<LocalCommit>, SessionError> {
        self.admit(now)?;
        let update = self.document.redo()?.and_then(|c| c.update);
        self.capture(update, "session:redo")
    }
    pub fn redo_if_revision(
        &mut self,
        expected: &crate::revision::ObservedRevision,
        now: u64,
    ) -> Result<Option<LocalCommit>, SessionError> {
        self.admit(now)?;
        self.compare_revision(expected)?;
        self.redo(now)
    }
    pub(crate) fn capture(
        &mut self,
        update: Option<ReplayUpdate>,
        origin: &str,
    ) -> Result<Option<LocalCommit>, SessionError> {
        let Some(update) = update else {
            return Ok(None);
        };
        let entry = JournalEntry::new(
            update.identity,
            self.document.writer_id(),
            self.next_sequence,
            origin.into(),
            update.bytes,
        )?;
        self.unjournaled = Some(entry.clone());
        match self.store.append(entry.clone()) {
            Ok(commit) => {
                self.next_sequence += 1;
                self.unjournaled = None;
                self.pending.push(entry);
                self.local = LocalState::Committed(commit.durability);
                self.remote = RemoteState::Unknown;
                Ok(Some(commit))
            }
            Err(error) => {
                self.freeze(error);
                Err(error.into())
            }
        }
    }
    fn freeze(&mut self, error: PortError) {
        self.recovery_error = Some(error);
        self.conflict |= error == PortError::Conflict;
        if self.unjournaled.is_some() || self.initialization.is_some() {
            self.local = LocalState::Unsaved;
        }
        self.remote = RemoteState::Unknown;
    }
    pub fn recovery_export(&self) -> RecoveryExport {
        RecoveryExport {
            checkpoint: self.document.checkpoint(),
            unjournaled: self.unjournaled.clone(),
            locally_committed: self.recovery_error.is_none(),
        }
    }
    /// Ordered exact retry before durable replay. Does not need a current write lease:
    /// this settles an already admitted operation, never authorizes a new operation.
    pub fn recover(&mut self) -> Result<(), SessionError> {
        let result = self.recover_inner();
        if let Err(SessionError::Store(error)) = &result {
            self.freeze(*error);
        }
        if result.is_err() && self.recovery_error.is_none() {
            self.freeze(PortError::Integrity);
        }
        result
    }
    fn recover_inner(&mut self) -> Result<(), SessionError> {
        if let Some(checkpoint) = &self.initialization {
            // Unknown acknowledgment resolves through the actual durable state first.
            if self.store.generation(self.document.identity())? == 0 {
                self.store.publish(0, checkpoint.clone())?;
            } else if self
                .store
                .recover(self.document.identity())?
                .checkpoint
                .checkpoint()
                .bytes
                != checkpoint.checkpoint().bytes
            {
                // Another initializer won: never merge two independently seeded documents.
                return Err(PortError::Conflict.into());
            }
        }
        if let Some(entry) = &self.unjournaled {
            self.store.append(entry.clone())?;
        }
        let recovery = self.store.recover(self.document.identity())?;
        let restored = restore(self.document.identity(), &recovery)?;
        // Replay validated durable state into the existing owner, retaining its local undo stack.
        self.document.apply_replay(&ReplayUpdate {
            identity: restored.identity().clone(),
            bytes: restored.checkpoint().bytes,
        })?;
        self.next_sequence = recovery
            .journal
            .iter()
            .filter(|e| e.id().writer == self.document.writer_id())
            .map(|e| e.id().sequence)
            .max()
            .unwrap_or(0)
            + 1;
        self.remote =
            if recovery.journal.len() > recovery.pending.len() && recovery.pending.is_empty() {
                RemoteState::RemotelyCommitted
            } else {
                RemoteState::Unknown
            };
        self.pending = recovery.pending;
        self.local = if recovery.journal.is_empty() {
            LocalState::Initialized(self.store.capabilities().durability)
        } else {
            LocalState::Committed(self.store.capabilities().durability)
        };
        self.unjournaled = None;
        self.initialization = None;
        self.recovery_error = None;
        self.conflict = false;
        Ok(())
    }
    /// Wake peers only after durable/volatile store admission, never from unjournaled edits.
    pub fn notify_local(
        &self,
        channel: &mut impl crate::local_channel::LocalChannel,
    ) -> Result<(), SessionError> {
        self.ready()?;
        channel.notify(self.document.writer_id())?;
        Ok(())
    }
    /// Explicit catch-up is also required on startup/reconnect: sharing a store is not delivery.
    pub fn catch_up_local(&mut self) -> Result<usize, SessionError> {
        self.ready()?;
        let recovery = match self.store.recover(self.document.identity()) {
            Ok(recovery) => recovery,
            Err(error) => {
                self.freeze(error);
                return Err(error.into());
            }
        };
        // Qualify all entries before touching the live owner, preserving history and writer identity.
        let checkpoint = self.document.checkpoint();
        let updates: Vec<_> = recovery
            .journal
            .iter()
            .map(|entry| ReplayUpdate {
                identity: entry.id().identity.clone(),
                bytes: entry.bytes().to_vec(),
            })
            .collect();
        if let Err(error) = Document::restore(self.document.identity(), &checkpoint, &updates) {
            self.freeze(PortError::Integrity);
            return Err(error.into());
        }
        let before = self.document.revision();
        for update in &updates {
            self.document.apply_replay(update)?;
        }
        self.pending = recovery.pending;
        if !recovery.journal.is_empty() {
            self.local = LocalState::Committed(self.store.capabilities().durability);
        }
        if !self.pending.is_empty() {
            self.remote = RemoteState::Unknown;
        }
        Ok((self.document.revision() - before) as usize)
    }
    pub fn receive_local(
        &mut self,
        channel: &mut impl crate::local_channel::LocalChannel,
    ) -> Result<usize, SessionError> {
        self.ready()?;
        let writers = channel.receive()?;
        if writers
            .iter()
            .any(|writer| *writer != self.document.writer_id())
        {
            self.catch_up_local()
        } else {
            Ok(0)
        }
    }
    /// Trusted host transport supplies an exact scoped envelope. No remote receipt is inferred.
    pub fn receive_peer(&mut self, entry: JournalEntry) -> Result<LocalCommit, SessionError> {
        self.ready()?;
        entry.verify(self.document.identity())?;
        let update = ReplayUpdate {
            identity: entry.id().identity.clone(),
            bytes: entry.bytes().to_vec(),
        };
        Document::restore(
            self.document.identity(),
            &self.document.checkpoint(),
            std::slice::from_ref(&update),
        )?;
        // Store first; failure cannot mutate the live replica. Lost ACK freezes admission.
        let commit = match self.store.append(entry) {
            Ok(commit) => commit,
            Err(error) => {
                self.freeze(error);
                return Err(error.into());
            }
        };
        let pending = match self.store.pending(self.document.identity()) {
            Ok(pending) => pending,
            Err(error) => {
                self.freeze(error);
                return Err(error.into());
            }
        };
        self.document.apply_replay(&update)?;
        self.pending = pending;
        self.local = LocalState::Committed(commit.durability);
        self.remote = RemoteState::Unknown;
        Ok(commit)
    }

    pub fn attach_transport(
        &mut self,
        transport: Box<dyn Transport>,
        expires_at: u64,
        now: u64,
    ) -> Result<(), SessionError> {
        self.ready()?;
        if now >= expires_at || transport.state() != ConnectionState::Authorized {
            return Err(SessionError::Unauthorized);
        }
        self.transport = Some(transport);
        self.auth_expiry = Some(expires_at);
        Ok(())
    }
    pub fn detach_transport(&mut self) {
        self.transport = None;
        self.auth_expiry = None;
    }
    fn upload_admission(&self, now: u64) -> Result<(), SessionError> {
        self.ready()?;
        if self.auth(now) != AuthState::Authorized {
            return Err(SessionError::Unauthorized);
        }
        Ok(())
    }
    /// Sends only committed outbox records using the existing y-protocols update frame.
    /// Repetition is safe CRDT delivery. Send success never prunes pending membership.
    pub fn flush_pending(&mut self, now: u64) -> Result<usize, SessionError> {
        self.upload_admission(now)?;
        let transport = self.transport.as_mut().ok_or(SessionError::Unauthorized)?;
        let mut sent = 0;
        for entry in &self.pending {
            let mut bytes = vec![2];
            let mut length = entry.bytes().len();
            loop {
                let part = (length & 127) as u8;
                length >>= 7;
                bytes.push(part | if length > 0 { 128 } else { 0 });
                if length == 0 {
                    break;
                }
            }
            bytes.extend_from_slice(entry.bytes());
            if bytes.len() > transport.capabilities().max_frame_bytes {
                return Err(PortError::ResourceLimit.into());
            }
            transport.send(TransportFrame {
                identity: entry.id().identity.clone(),
                kind: FrameKind::Sync,
                bytes,
            })?;
            sent += 1;
        }
        if sent > 0 {
            self.remote = RemoteState::SentDurabilityUnknown;
        }
        Ok(sent)
    }
    /// Host supplies a receipt obtained from its authenticated application transport.
    pub fn accept_receipt(
        &mut self,
        receipt: DurableReceipt,
        now: u64,
    ) -> Result<ReceiptResult, SessionError> {
        self.upload_admission(now)?;
        let capability = self
            .transport
            .as_ref()
            .ok_or(SessionError::Unauthorized)?
            .capabilities()
            .receipts;
        let id = receipt.update.clone();
        let result = match self.store.accept_receipt(capability, receipt) {
            Ok(result) => result,
            Err(error) => {
                self.freeze(error);
                return Err(error.into());
            }
        };
        if result == ReceiptResult::RemoteCommitted {
            self.pending.retain(|e| e.id() != &id);
            if self.pending.is_empty() {
                self.remote = RemoteState::RemotelyCommitted;
            }
        }
        Ok(result)
    }
}
fn restore(identity: &DocumentIdentity, recovery: &Recovery) -> Result<Document, SessionError> {
    let updates: Vec<_> = recovery
        .journal
        .iter()
        .map(|e| ReplayUpdate {
            identity: e.id().identity.clone(),
            bytes: e.bytes().to_vec(),
        })
        .collect();
    match Document::restore(identity, recovery.checkpoint.checkpoint(), &updates) {
        Ok(doc) => Ok(doc),
        Err(error) => match &recovery.fallback {
            Some(previous) => Ok(Document::restore(
                identity,
                previous.checkpoint(),
                &updates,
            )?),
            None => Err(error.into()),
        },
    }
}
