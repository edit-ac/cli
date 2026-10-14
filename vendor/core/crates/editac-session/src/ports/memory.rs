//! Prepared in-memory adapters. Clones share a serialized owner; no native durability claim.
use super::*;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug)]
pub(crate) struct State {
    pub(crate) journal: Vec<JournalEntry>,
    pub(crate) receipted: Vec<UpdateId>,
    pub(crate) latest: Option<CheckpointRecord>,
    pub(crate) previous: Option<CheckpointRecord>,
    pub(crate) generation: u64,
}
struct Inner {
    state: State,
    fault: Option<InjectedFault>,
    writers: Vec<u32>,
}
#[derive(Clone)]
pub struct MemoryStore {
    identity: DocumentIdentity,
    limits: StoreLimits,
    inner: Arc<Mutex<Inner>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultBoundary {
    BeforeWrite,
    BeforeCommit,
    AfterCommit,
}
#[derive(Clone, Copy, Debug)]
pub struct InjectedFault {
    pub boundary: FaultBoundary,
    pub error: PortError,
}
impl MemoryStore {
    pub fn new(identity: DocumentIdentity, limits: StoreLimits) -> Result<Self, PortError> {
        scope(&identity)?;
        if limits.max_records == 0 || limits.max_record_bytes == 0 || limits.max_total_bytes == 0 {
            return Err(PortError::ResourceLimit);
        }
        Ok(Self {
            identity,
            limits,
            inner: Arc::new(Mutex::new(Inner {
                state: State {
                    journal: vec![],
                    receipted: vec![],
                    latest: None,
                    previous: None,
                    generation: 0,
                },
                writers: vec![],
                fault: None,
            })),
        })
    }
    /// Volatile reservations shared by clones; native persistence keeps its own SQL table.
    pub fn claim_writer(&self, writer: u32) -> Result<(), PortError> {
        let mut inner = self.inner.lock().map_err(|_| PortError::Unavailable)?;
        if writer == 0 {
            return Err(PortError::InvalidRecord);
        }
        if inner.writers.contains(&writer)
            || inner.state.journal.iter().any(|e| e.id().writer == writer)
        {
            return Err(PortError::Conflict);
        }
        if inner.writers.len() >= self.limits.max_records {
            return Err(PortError::ResourceLimit);
        }
        inner.writers.push(writer);
        Ok(())
    }
    /// Native transactions reuse the prepared policy over a detached verified snapshot.
    #[cfg(feature = "native-store")]
    pub(crate) fn from_state(
        identity: DocumentIdentity,
        limits: StoreLimits,
        state: State,
    ) -> Result<Self, PortError> {
        let store = Self::new(identity, limits)?;
        store.limits(&state)?;
        store
            .inner
            .lock()
            .map_err(|_| PortError::Unavailable)?
            .state = state;
        Ok(store)
    }
    #[cfg(feature = "native-store")]
    pub(crate) fn snapshot(&self) -> Result<State, PortError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| PortError::Unavailable)?
            .state
            .clone())
    }
    fn matches(&self, identity: &DocumentIdentity) -> Result<(), PortError> {
        if identity != &self.identity {
            Err(PortError::Incompatible)
        } else {
            Ok(())
        }
    }
    fn limits(&self, state: &State) -> Result<(), PortError> {
        if state.journal.len() > self.limits.max_records {
            return Err(PortError::ResourceLimit);
        }
        let mut total = 0usize;
        for bytes in state.journal.iter().map(|e| e.bytes.len()).chain(
            [state.latest.as_ref(), state.previous.as_ref()]
                .into_iter()
                .flatten()
                .map(|c| c.checkpoint.bytes.len()),
        ) {
            if bytes > self.limits.max_record_bytes {
                return Err(PortError::ResourceLimit);
            }
            total = total.checked_add(bytes).ok_or(PortError::ResourceLimit)?;
        }
        if total > self.limits.max_total_bytes {
            return Err(PortError::Quota);
        }
        Ok(())
    }
    fn transaction<R>(
        &self,
        work: impl FnOnce(&mut State) -> Result<R, PortError>,
    ) -> Result<R, PortError> {
        let mut inner = self.inner.lock().map_err(|_| PortError::Unavailable)?;
        let fault = inner.fault.take();
        if let Some(f) = fault.filter(|f| f.boundary == FaultBoundary::BeforeWrite) {
            return Err(f.error);
        }
        let mut staged = inner.state.clone();
        let result = work(&mut staged)?;
        self.limits(&staged)?;
        if let Some(f) = fault.filter(|f| f.boundary == FaultBoundary::BeforeCommit) {
            return Err(f.error);
        }
        inner.state = staged;
        if fault.is_some_and(|f| f.boundary == FaultBoundary::AfterCommit) {
            return Err(PortError::CommitUnknown);
        }
        Ok(result)
    }
    fn entries(&self, state: &State) -> Result<Vec<JournalEntry>, PortError> {
        for e in &state.journal {
            e.verify(&self.identity)?;
        }
        Ok(state.journal.clone())
    }
}
impl LocalStore for MemoryStore {
    fn capabilities(&self) -> StoreCapabilities {
        StoreCapabilities {
            durability: Durability::Volatile,
            atomic_transactions: true,
            serialized_writers: true,
        }
    }
    fn append(&self, entry: JournalEntry) -> Result<LocalCommit, PortError> {
        self.matches(&entry.id.identity)?;
        entry.verify(&self.identity)?;
        if entry.bytes.len() > self.limits.max_record_bytes {
            return Err(PortError::ResourceLimit);
        }
        self.transaction(|state| {
            if let Some(existing) = state
                .journal
                .iter()
                .find(|e| e.id.writer == entry.id.writer && e.id.sequence == entry.id.sequence)
            {
                if existing != &entry {
                    return Err(PortError::Integrity);
                }
            } else {
                let last = state
                    .journal
                    .iter()
                    .filter(|e| e.id.writer == entry.id.writer)
                    .map(|e| e.id.sequence)
                    .max()
                    .unwrap_or(0);
                if last.checked_add(1) != Some(entry.id.sequence) {
                    return Err(PortError::Sequence);
                }
                state.journal.push(entry.clone());
            }
            Ok(LocalCommit {
                update: entry.id,
                durability: Durability::Volatile,
            })
        })
    }
    fn generation(&self, identity: &DocumentIdentity) -> Result<u64, PortError> {
        self.matches(identity)?;
        Ok(self
            .inner
            .lock()
            .map_err(|_| PortError::Unavailable)?
            .state
            .generation)
    }
    fn publish(
        &self,
        expected: u64,
        checkpoint: CheckpointRecord,
    ) -> Result<PublicationCommit, PortError> {
        checkpoint.verify(&self.identity)?;
        if checkpoint.coverage.len() > self.limits.max_records
            || checkpoint.checkpoint.bytes.len() > self.limits.max_record_bytes
        {
            return Err(PortError::ResourceLimit);
        }
        self.transaction(|state| {
            if expected != state.generation {
                return Err(PortError::Conflict);
            }
            let entries = self.entries(state)?;
            if checkpoint
                .coverage
                .iter()
                .any(|id| !entries.iter().any(|e| &e.id == id))
            {
                return Err(PortError::InvalidRecord);
            }
            let generation = state
                .generation
                .checked_add(1)
                .ok_or(PortError::ResourceLimit)?;
            let previous = [state.latest.as_ref(), state.previous.as_ref()]
                .into_iter()
                .flatten()
                .find(|c| c.verify(&self.identity).is_ok())
                .cloned();
            state.previous = previous;
            state.latest = Some(checkpoint);
            state.generation = generation;
            Ok(PublicationCommit {
                generation,
                durability: Durability::Volatile,
            })
        })
    }
    fn recover(&self, identity: &DocumentIdentity) -> Result<Recovery, PortError> {
        self.matches(identity)?;
        let inner = self.inner.lock().map_err(|_| PortError::Unavailable)?;
        let state = &inner.state;
        let journal = self.entries(state)?;
        let pending = journal
            .iter()
            .filter(|e| !state.receipted.contains(&e.id))
            .cloned()
            .collect();
        let mut rejected = vec![];
        let mut valid = Vec::new();
        for (source, candidate) in [
            (RecoverySource::Latest, &state.latest),
            (RecoverySource::Previous, &state.previous),
        ] {
            if let Some(candidate) = candidate {
                match candidate.verify(identity) {
                    Ok(()) => valid.push((source, candidate.clone())),
                    Err(e) => rejected.push((source, e)),
                }
            }
        }
        let mut valid = valid.into_iter();
        let (source, checkpoint) = valid.next().ok_or(PortError::Integrity)?;
        Ok(Recovery {
            checkpoint,
            fallback: valid.next().map(|(_, point)| point),
            journal,
            pending,
            source,
            rejected,
            generation: state.generation,
        })
    }
    fn pending(&self, identity: &DocumentIdentity) -> Result<Vec<JournalEntry>, PortError> {
        self.matches(identity)?;
        let inner = self.inner.lock().map_err(|_| PortError::Unavailable)?;
        Ok(self
            .entries(&inner.state)?
            .into_iter()
            .filter(|e| !inner.state.receipted.contains(&e.id))
            .collect())
    }
    fn accept_receipt(
        &self,
        negotiated: ReceiptCapability,
        receipt: DurableReceipt,
    ) -> Result<ReceiptResult, PortError> {
        if negotiated != ReceiptCapability::DurableV1
            || receipt.protocol != ReceiptCapability::DurableV1
        {
            return Ok(ReceiptResult::CapabilityUnavailable);
        }
        self.matches(&receipt.update.identity)?;
        self.transaction(|state| {
            let entries = self.entries(state)?;
            if !entries.iter().any(|e| e.id == receipt.update) {
                return Ok(ReceiptResult::UnknownUpdate);
            }
            if !state.receipted.contains(&receipt.update) {
                state.receipted.push(receipt.update);
            }
            Ok(ReceiptResult::RemoteCommitted)
        })
    }
}

/// Prepared transactional failure/corruption adapter using the same conformance boundary.
/// AfterCommit always reports CommitUnknown, regardless of the supplied error label.
#[derive(Clone)]
pub struct FaultInjectedStore(MemoryStore);
impl FaultInjectedStore {
    pub fn new(identity: DocumentIdentity, limits: StoreLimits) -> Result<Self, PortError> {
        MemoryStore::new(identity, limits).map(Self)
    }
    pub fn claim_writer(&self, writer: u32) -> Result<(), PortError> {
        self.0.claim_writer(writer)
    }
    pub fn inject(&self, fault: InjectedFault) -> Result<(), PortError> {
        let mut inner = self.0.inner.lock().map_err(|_| PortError::Unavailable)?;
        if inner.fault.is_some() {
            return Err(PortError::Conflict);
        }
        inner.fault = Some(fault);
        Ok(())
    }
    pub fn corrupt_latest(&self) -> Result<(), PortError> {
        let mut inner = self.0.inner.lock().map_err(|_| PortError::Unavailable)?;
        let record = inner.state.latest.as_mut().ok_or(PortError::Unavailable)?;
        record.checkpoint.bytes[0] ^= 1;
        Ok(())
    }
    pub fn corrupt_journal(&self) -> Result<(), PortError> {
        let mut inner = self.0.inner.lock().map_err(|_| PortError::Unavailable)?;
        inner
            .state
            .journal
            .first_mut()
            .ok_or(PortError::Unavailable)?
            .bytes[0] ^= 1;
        Ok(())
    }
}
impl LocalStore for FaultInjectedStore {
    fn capabilities(&self) -> StoreCapabilities {
        self.0.capabilities()
    }
    fn append(&self, e: JournalEntry) -> Result<LocalCommit, PortError> {
        self.0.append(e)
    }
    fn generation(&self, i: &DocumentIdentity) -> Result<u64, PortError> {
        self.0.generation(i)
    }
    fn publish(&self, g: u64, c: CheckpointRecord) -> Result<PublicationCommit, PortError> {
        self.0.publish(g, c)
    }
    fn recover(&self, i: &DocumentIdentity) -> Result<Recovery, PortError> {
        self.0.recover(i)
    }
    fn pending(&self, i: &DocumentIdentity) -> Result<Vec<JournalEntry>, PortError> {
        self.0.pending(i)
    }
    fn accept_receipt(
        &self,
        n: ReceiptCapability,
        r: DurableReceipt,
    ) -> Result<ReceiptResult, PortError> {
        self.0.accept_receipt(n, r)
    }
}

/// Deterministic loopback frames. No networking, auth negotiation or remote durability.
pub struct PreparedTransport {
    identity: DocumentIdentity,
    state: ConnectionState,
    capabilities: TransportCapabilities,
    queue: VecDeque<TransportFrame>,
    queued_bytes: usize,
}
impl PreparedTransport {
    pub fn new(
        identity: DocumentIdentity,
        state: ConnectionState,
        capabilities: TransportCapabilities,
    ) -> Result<Self, PortError> {
        scope(&identity)?;
        if capabilities.max_frame_bytes == 0
            || capabilities.max_queue_bytes == 0
            || capabilities.max_queue_frames == 0
        {
            return Err(PortError::ResourceLimit);
        }
        Ok(Self {
            identity,
            state,
            capabilities,
            queue: VecDeque::new(),
            queued_bytes: 0,
        })
    }
    /// Host policy controls attachment; does not clear queued frames or produce receipts.
    pub fn set_state(&mut self, state: ConnectionState) {
        self.state = state;
    }
    fn ready(&self) -> Result<(), PortError> {
        match self.state {
            ConnectionState::Authorized => Ok(()),
            ConnectionState::Disconnected => Err(PortError::Unavailable),
            ConnectionState::Unauthorized => Err(PortError::Permission),
            ConnectionState::Incompatible => Err(PortError::Incompatible),
        }
    }
}
impl Transport for PreparedTransport {
    fn state(&self) -> ConnectionState {
        self.state
    }
    fn capabilities(&self) -> TransportCapabilities {
        self.capabilities
    }
    fn send(&mut self, frame: TransportFrame) -> Result<(), PortError> {
        self.ready()?;
        if frame.identity != self.identity {
            return Err(PortError::Incompatible);
        }
        if frame.bytes.is_empty() {
            return Err(PortError::InvalidRecord);
        }
        if frame.bytes.len() > self.capabilities.max_frame_bytes {
            return Err(PortError::ResourceLimit);
        }
        let bytes = self
            .queued_bytes
            .checked_add(frame.bytes.len())
            .ok_or(PortError::Backpressure)?;
        if bytes > self.capabilities.max_queue_bytes
            || self.queue.len() >= self.capabilities.max_queue_frames
        {
            return Err(PortError::Backpressure);
        }
        self.queued_bytes = bytes;
        self.queue.push_back(frame);
        Ok(())
    }
    fn receive(&mut self) -> Result<Option<TransportFrame>, PortError> {
        self.ready()?;
        let frame = self.queue.pop_front();
        if let Some(frame) = &frame {
            self.queued_bytes -= frame.bytes.len();
        }
        Ok(frame)
    }
}
