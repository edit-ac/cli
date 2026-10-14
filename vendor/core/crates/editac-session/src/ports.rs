//! Portable storage and transport boundaries. No database, browser or socket ownership.
//! Memory adapters qualify prepared behavior, never process-crash durability.
use editac_core::document::{DocumentIdentity, InitializedCheckpoint};
use sha2::{Digest, Sha256};
use std::sync::Mutex;

pub mod memory;

/// Browser-compatible exact integer range; adapters must not round receipt sequences.
pub const MAX_SEQUENCE: u64 = (1u64 << 53) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortError {
    InvalidRecord,
    Incompatible,
    Integrity,
    Conflict,
    Sequence,
    Quota,
    Permission,
    Unavailable,
    ResourceLimit,
    Backpressure,
    /// The transaction committed but its acknowledgment was lost. Resolve by recovery/retry.
    CommitUnknown,
}
impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for PortError {}

pub(crate) fn scope(identity: &DocumentIdentity) -> Result<(), PortError> {
    crate::locator::validate_identity(identity).map_err(|_| PortError::Incompatible)
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
fn part(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}
pub(crate) fn hash_scope(hash: &mut Sha256, identity: &DocumentIdentity) {
    for field in [
        &identity.document_id,
        &identity.epoch,
        &identity.schema,
        &identity.profile,
        &identity.offsets,
    ] {
        part(hash, field.as_bytes());
    }
    hash.update(identity.protocol_version.to_be_bytes());
    hash.update(identity.update_encoding.to_be_bytes());
}

/// Exact immutable receipt identity. Typed, not a new wire encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateId {
    pub identity: DocumentIdentity,
    pub writer: u32,
    pub sequence: u64,
    pub checksum: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalEntry {
    id: UpdateId,
    origin: String,
    bytes: Vec<u8>,
}
impl JournalEntry {
    pub fn new(
        identity: DocumentIdentity,
        writer: u32,
        sequence: u64,
        origin: String,
        bytes: Vec<u8>,
    ) -> Result<Self, PortError> {
        scope(&identity)?;
        if writer == 0
            || sequence == 0
            || sequence > MAX_SEQUENCE
            || origin.is_empty()
            || origin.len() > 1024
            || bytes.is_empty()
        {
            return Err(PortError::InvalidRecord);
        }
        Ok(Self {
            id: UpdateId {
                identity,
                writer,
                sequence,
                checksum: digest(&bytes),
            },
            origin,
            bytes,
        })
    }
    /// Read a persisted record without silently rehashing corrupt stored content.
    pub fn from_stored(id: UpdateId, origin: String, bytes: Vec<u8>) -> Result<Self, PortError> {
        let entry = Self::new(id.identity.clone(), id.writer, id.sequence, origin, bytes)?;
        if entry.id != id {
            return Err(PortError::Integrity);
        }
        Ok(entry)
    }
    pub fn id(&self) -> &UpdateId {
        &self.id
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub(crate) fn verify(&self, expected: &DocumentIdentity) -> Result<(), PortError> {
        if &self.id.identity != expected {
            return Err(PortError::Incompatible);
        }
        let rebuilt = Self::new(
            self.id.identity.clone(),
            self.id.writer,
            self.id.sequence,
            self.origin.clone(),
            self.bytes.clone(),
        )?;
        if rebuilt != *self {
            return Err(PortError::Integrity);
        }
        Ok(())
    }
}

/// Engine-supplied coverage is bookkeeping, not a proof of semantic inclusion.
#[derive(Clone, Debug)]
pub struct CheckpointRecord {
    pub(crate) checkpoint: InitializedCheckpoint,
    pub(crate) coverage: Vec<UpdateId>,
    pub(crate) manifest: [u8; 32],
}
impl CheckpointRecord {
    pub fn new(
        checkpoint: InitializedCheckpoint,
        coverage: Vec<UpdateId>,
    ) -> Result<Self, PortError> {
        scope(&checkpoint.identity)?;
        if !checkpoint.initialized || checkpoint.writer_id == 0 || checkpoint.bytes.is_empty() {
            return Err(PortError::InvalidRecord);
        }
        for (i, id) in coverage.iter().enumerate() {
            if id.identity != checkpoint.identity
                || id.writer == 0
                || id.sequence == 0
                || id.sequence > MAX_SEQUENCE
                || coverage[..i].contains(id)
            {
                return Err(PortError::InvalidRecord);
            }
        }
        let mut record = Self {
            checkpoint,
            coverage,
            manifest: [0; 32],
        };
        record.manifest = record.hash();
        Ok(record)
    }
    /// Verify the retained manifest on an adapter read before accepting its envelope.
    pub fn from_stored(
        checkpoint: InitializedCheckpoint,
        coverage: Vec<UpdateId>,
        manifest: [u8; 32],
    ) -> Result<Self, PortError> {
        let record = Self::new(checkpoint, coverage)?;
        if record.manifest != manifest {
            return Err(PortError::Integrity);
        }
        Ok(record)
    }
    fn hash(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        part(&mut hash, b"editac-session-checkpoint-v1");
        hash_scope(&mut hash, &self.checkpoint.identity);
        hash.update([u8::from(self.checkpoint.initialized)]);
        hash.update(self.checkpoint.writer_id.to_be_bytes());
        part(&mut hash, &self.checkpoint.bytes);
        hash.update((self.coverage.len() as u64).to_be_bytes());
        for id in &self.coverage {
            hash_scope(&mut hash, &id.identity);
            hash.update(id.writer.to_be_bytes());
            hash.update(id.sequence.to_be_bytes());
            hash.update(id.checksum);
        }
        hash.finalize().into()
    }
    pub fn checkpoint(&self) -> &InitializedCheckpoint {
        &self.checkpoint
    }
    pub fn coverage(&self) -> &[UpdateId] {
        &self.coverage
    }
    pub fn manifest(&self) -> &[u8; 32] {
        &self.manifest
    }
    pub(crate) fn verify(&self, expected: &DocumentIdentity) -> Result<(), PortError> {
        if &self.checkpoint.identity != expected {
            return Err(PortError::Incompatible);
        }
        if self.hash() != self.manifest {
            return Err(PortError::Integrity);
        }
        Self::new(self.checkpoint.clone(), self.coverage.clone()).map(|_| ())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    Volatile,
    ProcessCrash,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreCapabilities {
    pub durability: Durability,
    pub atomic_transactions: bool,
    pub serialized_writers: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct StoreLimits {
    pub max_records: usize,
    pub max_record_bytes: usize,
    pub max_total_bytes: usize,
}
impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            max_records: 4096,
            max_record_bytes: 16 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalCommit {
    pub update: UpdateId,
    pub durability: Durability,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicationCommit {
    pub generation: u64,
    pub durability: Durability,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoverySource {
    Latest,
    Previous,
}
#[derive(Clone, Debug)]
pub struct Recovery {
    pub checkpoint: CheckpointRecord,
    /// A verified previous envelope for fallback if the engine rejects latest bytes.
    pub fallback: Option<CheckpointRecord>,
    /// All entries, including covered/receipted ones, for idempotent engine replay.
    pub journal: Vec<JournalEntry>,
    pub pending: Vec<JournalEntry>,
    pub source: RecoverySource,
    pub rejected: Vec<(RecoverySource, PortError)>,
    pub generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptCapability {
    Unknown,
    DurableV1,
}
#[derive(Clone, Debug)]
pub struct DurableReceipt {
    pub protocol: ReceiptCapability,
    pub update: UpdateId,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptResult {
    RemoteCommitted,
    CapabilityUnavailable,
    UnknownUpdate,
}

/// Return acknowledgments only after the complete atomic transaction.
/// Implementations must serialize shared writers; checkpoint CAS must never reuse generations.
/// Errors never imply saving. Pending entries are not pruned by publication or sync delivery.
pub trait LocalStore: Send + Sync {
    fn capabilities(&self) -> StoreCapabilities;
    fn append(&self, entry: JournalEntry) -> Result<LocalCommit, PortError>;
    fn generation(&self, identity: &DocumentIdentity) -> Result<u64, PortError>;
    fn publish(
        &self,
        expected_generation: u64,
        checkpoint: CheckpointRecord,
    ) -> Result<PublicationCommit, PortError>;
    fn recover(&self, identity: &DocumentIdentity) -> Result<Recovery, PortError>;
    fn pending(&self, identity: &DocumentIdentity) -> Result<Vec<JournalEntry>, PortError>;
    /// Caller supplies receipts only from its authenticated, negotiated application transport.
    fn accept_receipt(
        &self,
        negotiated: ReceiptCapability,
        receipt: DurableReceipt,
    ) -> Result<ReceiptResult, PortError>;
}

/// One endpoint owner serializes semantic prepare/commit and durable capture.
/// No global lock against as-yet-unseen remote CRDT events is implied.
pub struct SerializedAdmission<T>(Mutex<T>);
impl<T> SerializedAdmission<T> {
    pub fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }
    pub fn with<R>(
        &self,
        work: impl FnOnce(&mut T) -> Result<R, PortError>,
    ) -> Result<R, PortError> {
        let mut guard = self.0.lock().map_err(|_| PortError::Unavailable)?;
        work(&mut guard)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Authorized,
    Unauthorized,
    Incompatible,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Sync,
    Awareness,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportFrame {
    pub identity: DocumentIdentity,
    pub kind: FrameKind,
    pub bytes: Vec<u8>,
}
#[derive(Clone, Copy, Debug)]
pub struct TransportCapabilities {
    pub receipts: ReceiptCapability,
    pub max_frame_bytes: usize,
    pub max_queue_bytes: usize,
    pub max_queue_frames: usize,
}
/// Sending a frame is not remote durability. Awareness can never produce a receipt.
pub trait Transport {
    fn state(&self) -> ConnectionState;
    fn capabilities(&self) -> TransportCapabilities;
    fn send(&mut self, frame: TransportFrame) -> Result<(), PortError>;
    fn receive(&mut self) -> Result<Option<TransportFrame>, PortError>;
}
