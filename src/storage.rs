//! Native Unix recovery over the pinned shared engine and journal identity contract.
//!
//! One private, caller-selected directory holds a single document lineage. Advisory
//! locks serialize cooperating processes, not hostile/non-cooperating writers.
//! Checkpoint bytes and exact coverage share one checksummed atomic record; the
//! previous valid record and every journal entry remain available. The journal is
//! also the unpruned outbox: checkpoint publication never implies a remote receipt.
//! Drafts are separate source projections and are never replayed as CRDT updates.
//! Success requires file sync, atomic rename and directory sync. Post-publication
//! errors are explicitly uncertain. Tested process-crash durability on local Unix
//! filesystems is not a hardware/power-loss or network-filesystem guarantee.
use crate::engine::document::{
    Document, DocumentIdentity, InitializedCheckpoint, MAX_RESTORE_BYTES, ReplayUpdate,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use yrs::{
    Any, ClientID, ID, Update,
    encoding::write::Write as WireWrite,
    updates::{
        decoder::Decode,
        encoder::{Encoder, EncoderV1},
    },
};

const MAX_RECORD_BYTES: u64 = (MAX_RESTORE_BYTES as u64) * 8;
const MAX_ENTRIES: usize = 4096;
const MAX_SEQUENCE: u64 = 9_007_199_254_740_991; // shared JS safe integer boundary

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    Journal,
    Latest,
    Previous,
    Draft,
    Receipt,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    BeforeWrite,
    AfterWrite,
    AfterFileSync,
    BeforePublish,
    AfterPublish,
    AfterDirectorySync,
}
/// Deterministic I/O failure/process-interruption seam; all underlying I/O is native.
pub trait IoHooks {
    fn boundary(&self, _record: RecordKind, _boundary: Boundary) -> io::Result<()> {
        Ok(())
    }
}
pub struct NativeIo;
impl IoHooks for NativeIo {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Io,
    Busy,
    Invalid,
    /// Persisted document/profile/schema/epoch differs from the requested lineage.
    IncompatibleIdentity,
    Missing,
    UnsupportedPlatform,
    PublicationUncertain,
}
#[derive(Debug)]
pub struct StorageError {
    pub kind: FailureKind,
    pub detail: String,
}
impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "storage {:?}: {} (retain recovery files)",
            self.kind, self.detail
        )
    }
}
impl std::error::Error for StorageError {}
fn invalid(detail: impl Into<String>) -> StorageError {
    StorageError {
        kind: FailureKind::Invalid,
        detail: detail.into(),
    }
}
fn io_error(e: io::Error) -> StorageError {
    StorageError {
        kind: FailureKind::Io,
        detail: e.to_string(),
    }
}
fn engine_error(e: impl std::fmt::Display) -> StorageError {
    invalid(e.to_string())
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Comparison-only v1 encoder. Persistence always retains the original bytes.
/// Yrs Update equality compares item IDs, not contents; raw v1 encoding can
/// reorder nested map keys. Canonicalize only unordered Any/JSON maps while
/// retaining every struct, content value and deletion range.
struct StateEncoder(EncoderV1);
impl WireWrite for StateEncoder {
    fn write_all(&mut self, bytes: &[u8]) {
        self.0.write_all(bytes);
    }
}
fn write_ordered_any(encoder: &mut impl WireWrite, value: &Any) {
    match value {
        Any::Map(map) => {
            encoder.write_u8(118);
            encoder.write_var(map.len() as u64);
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            for key in keys {
                encoder.write_string(key);
                write_ordered_any(encoder, &map[key]);
            }
        }
        Any::Array(values) => {
            encoder.write_u8(117);
            encoder.write_var(values.len() as u64);
            for value in values.iter() {
                write_ordered_any(encoder, value);
            }
        }
        _ => value.encode(encoder),
    }
}
impl Encoder for StateEncoder {
    fn to_vec(self) -> Vec<u8> {
        self.0.to_vec()
    }
    fn reset_ds_cur_val(&mut self) {
        self.0.reset_ds_cur_val();
    }
    fn write_ds_clock(&mut self, clock: u32) {
        self.0.write_ds_clock(clock);
    }
    fn write_ds_len(&mut self, len: u32) {
        self.0.write_ds_len(len);
    }
    fn write_left_id(&mut self, id: &ID) {
        self.0.write_left_id(id);
    }
    fn write_right_id(&mut self, id: &ID) {
        self.0.write_right_id(id);
    }
    fn write_client(&mut self, client: ClientID) {
        self.0.write_client(client);
    }
    fn write_info(&mut self, info: u8) {
        self.0.write_info(info);
    }
    fn write_parent_info(&mut self, key: bool) {
        self.0.write_parent_info(key);
    }
    fn write_type_ref(&mut self, info: u8) {
        self.0.write_type_ref(info);
    }
    fn write_len(&mut self, len: u32) {
        self.0.write_len(len);
    }
    fn write_any(&mut self, value: &Any) {
        write_ordered_any(&mut self.0, value);
    }
    fn write_json(&mut self, value: &Any) {
        let mut json = String::new();
        value.to_json(&mut json);
        // Any's JSON writer emits valid JSON. serde_json's default map is sorted.
        let value: serde_json::Value = serde_json::from_str(&json).expect("Yrs Any JSON");
        self.0
            .write_string(&serde_json::to_string(&value).expect("JSON value"));
    }
    fn write_key(&mut self, key: &str) {
        self.0.write_key(key);
    }
}
fn same_state(a: &[u8], b: &[u8]) -> Result<bool, StorageError> {
    fn canonical(bytes: &[u8]) -> Result<Vec<u8>, StorageError> {
        use yrs::{Doc, OffsetKind, Options, ReadTxn, StateVector, Transact};
        let update = Update::decode_v1(bytes).map_err(engine_error)?;
        // Restore legitimately coalesces structs and garbage-collects deleted
        // payloads. Normalize both full checkpoints in isolated codec staging,
        // never in an authoritative engine replica or through Markdown/JSON.
        let staging = Doc::with_options(Options {
            offset_kind: OffsetKind::Utf16,
            ..Options::default()
        });
        staging
            .transact_mut()
            .apply_update(update)
            .map_err(engine_error)?;
        let txn = staging.transact();
        if txn.store().pending_update().is_some() || txn.store().pending_ds().is_some() {
            return Err(invalid("incomplete checkpoint comparison"));
        }
        let mut encoder = StateEncoder(EncoderV1::new());
        txn.encode_state_as_update(&StateVector::default(), &mut encoder);
        Ok(encoder.to_vec())
    }
    Ok(canonical(a)? == canonical(b)?)
}

/// Field order and camelCase spelling match the shared journal identity contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredIdentity {
    pub profile: String,
    pub schema: String,
    pub document_id: String,
    pub epoch: String,
    pub protocol_version: u32,
    pub update_encoding: u32,
    pub offsets: String,
}
impl From<&DocumentIdentity> for StoredIdentity {
    fn from(i: &DocumentIdentity) -> Self {
        Self {
            profile: i.profile.clone(),
            schema: i.schema.clone(),
            document_id: i.document_id.clone(),
            epoch: i.epoch.clone(),
            protocol_version: i.protocol_version,
            update_encoding: i.update_encoding,
            offsets: i.offsets.clone(),
        }
    }
}
impl StoredIdentity {
    pub fn key(&self) -> String {
        serde_json::to_string(&(
            &self.profile,
            &self.schema,
            &self.document_id,
            &self.epoch,
            self.protocol_version,
            self.update_encoding,
            &self.offsets,
        ))
        .expect("identity serialization")
    }
    fn admit(&self, expected: &DocumentIdentity) -> Result<(), StorageError> {
        if self != &Self::from(expected) {
            return Err(StorageError {
                kind: FailureKind::IncompatibleIdentity,
                detail: "document/schema/epoch identity mismatch".into(),
            });
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JournalEntry {
    pub identity: StoredIdentity,
    pub writer_id: u32,
    pub sequence: u64,
    pub origin: String,
    pub bytes: Vec<u8>,
    pub checksum: String,
    pub update_id: String,
}
impl JournalEntry {
    fn new(
        document: &Document,
        update: &ReplayUpdate,
        sequence: u64,
        origin: &str,
    ) -> Result<Self, StorageError> {
        if document.identity() != &update.identity
            || document.writer_id() == 0
            || sequence == 0
            || sequence > MAX_SEQUENCE
            || origin.is_empty()
            || origin.len() > 1024
            || update.bytes.is_empty()
            || update.bytes.len() > MAX_RESTORE_BYTES
        {
            return Err(invalid("invalid update identity/sequence/origin/size"));
        }
        let identity = StoredIdentity::from(document.identity());
        let checksum = hash(&update.bytes);
        let update_id =
            serde_json::to_string(&(identity.key(), document.writer_id(), sequence, &checksum))
                .map_err(engine_error)?;
        Ok(Self {
            identity,
            writer_id: document.writer_id(),
            sequence,
            origin: origin.into(),
            bytes: update.bytes.clone(),
            checksum,
            update_id,
        })
    }
    fn validate(&self, expected: &DocumentIdentity) -> Result<(), StorageError> {
        self.identity.admit(expected)?;
        if self.writer_id == 0
            || self.sequence == 0
            || self.sequence > MAX_SEQUENCE
            || self.origin.is_empty()
            || self.origin.len() > 1024
            || self.bytes.is_empty()
            || self.bytes.len() > MAX_RESTORE_BYTES
            || self.checksum != hash(&self.bytes)
            || self.update_id
                != serde_json::to_string(&(
                    self.identity.key(),
                    self.writer_id,
                    self.sequence,
                    &self.checksum,
                ))
                .map_err(engine_error)?
        {
            return Err(invalid("corrupt journal checksum/receipt identity"));
        }
        Ok(())
    }
    fn replay(&self, identity: &DocumentIdentity) -> ReplayUpdate {
        ReplayUpdate {
            identity: identity.clone(),
            bytes: self.bytes.clone(),
        }
    }
    fn filename(&self) -> String {
        format!("journal-{:010}-{:016}.json", self.writer_id, self.sequence)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CheckpointRecord {
    identity: StoredIdentity,
    initialized: bool,
    writer_id: u32,
    bytes: Vec<u8>,
    coverage: Vec<String>,
}
impl CheckpointRecord {
    fn new(document: &Document, coverage: &[String]) -> Self {
        let c = document.checkpoint();
        Self {
            identity: StoredIdentity::from(&c.identity),
            initialized: c.initialized,
            writer_id: c.writer_id,
            bytes: c.bytes,
            coverage: coverage.to_vec(),
        }
    }
    fn checkpoint(
        &self,
        expected: &DocumentIdentity,
        entries: &[JournalEntry],
    ) -> Result<InitializedCheckpoint, StorageError> {
        self.identity.admit(expected)?;
        let mut unique = HashSet::new();
        if !self.initialized
            || self.writer_id == 0
            || self.bytes.is_empty()
            || self.bytes.len() > MAX_RESTORE_BYTES
            || self
                .coverage
                .iter()
                .any(|id| !unique.insert(id) || !entries.iter().any(|e| &e.update_id == id))
        {
            return Err(invalid("invalid initialization/coverage/size"));
        }
        let c = InitializedCheckpoint {
            identity: expected.clone(),
            initialized: true,
            writer_id: self.writer_id,
            bytes: self.bytes.clone(),
        };
        let covered: Vec<_> = entries
            .iter()
            .filter(|e| unique.contains(&e.update_id))
            .map(|e| e.replay(expected))
            .collect();
        let restored = Document::restore(expected, &c, &covered).map_err(engine_error)?;
        if !same_state(&restored.checkpoint().bytes, &c.bytes)? {
            return Err(invalid("coverage includes updates absent from checkpoint"));
        }
        Ok(c)
    }
}
/// Qualified application receipt; baseline frames/state vectors never create one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableReceipt {
    pub protocol: String,
    pub state: String,
    pub identity: StoredIdentity,
    pub writer_id: u32,
    pub sequence: u64,
    pub checksum: String,
    pub update_id: String,
}
pub const RECEIPT_PROTOCOL: &str = "editac-durable-receipt-v1";
impl DurableReceipt {
    pub fn matches(&self, entry: &JournalEntry) -> bool {
        self.protocol == RECEIPT_PROTOCOL
            && self.state == "durable"
            && self.identity == entry.identity
            && self.writer_id == entry.writer_id
            && self.sequence == entry.sequence
            && self.checksum == entry.checksum
            && self.update_id == entry.update_id
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReceiptRecord {
    identity: StoredIdentity,
    receipts: Vec<DurableReceipt>,
}

/// Retained exact source, independent of semantic admission and journal coverage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Draft {
    pub baseline: String,
    pub source: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDraft {
    identity: StoredIdentity,
    draft: Draft,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sealed {
    version: u32,
    payload: serde_json::Value,
    checksum: String,
}
fn encode<T: Serialize>(record: &T) -> Result<Vec<u8>, StorageError> {
    let payload = serde_json::to_value(record).map_err(engine_error)?;
    let checksum = hash(&serde_json::to_vec(&payload).map_err(engine_error)?);
    let bytes = serde_json::to_vec(&Sealed {
        version: 1,
        payload,
        checksum,
    })
    .map_err(engine_error)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(invalid("record size limit"));
    }
    Ok(bytes)
}
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StorageError> {
    let sealed: Sealed = serde_json::from_slice(bytes).map_err(engine_error)?;
    if sealed.version != 1
        || sealed.checksum != hash(&serde_json::to_vec(&sealed.payload).map_err(engine_error)?)
    {
        return Err(invalid("unsupported version or corrupt record"));
    }
    serde_json::from_value(sealed.payload).map_err(engine_error)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryPoint {
    Latest,
    Previous,
}
#[derive(Debug)]
pub struct Recovery {
    pub document: Document,
    pub point: RecoveryPoint,
    pub pending: Vec<JournalEntry>,
    pub tail: Vec<JournalEntry>,
    pub coverage: Vec<String>,
    pub draft: Option<Draft>,
    pub rejected_latest: Option<String>,
    pub rejected_draft: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCommit {
    pub update_id: String,
    pub durability: &'static str,
}

/// Explicit unlock releases the shared open-file-description lock even when a
/// concurrent fork temporarily retains an inherited descriptor before exec.
struct StorageLock(File);
impl Drop for StorageLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

pub struct NativeStorage {
    root: PathBuf,
    identity: DocumentIdentity,
    #[cfg(unix)]
    inode: (u64, u64),
}
impl NativeStorage {
    pub fn open(root: impl AsRef<Path>, identity: DocumentIdentity) -> Result<Self, StorageError> {
        // Validate with the shared metadata constructors, never a creation permit:
        // opening persisted state must not reserve/reinitialize its lineage.
        let valid = [
            DocumentIdentity::v1,
            DocumentIdentity::v2,
            DocumentIdentity::v3,
            DocumentIdentity::v4,
            DocumentIdentity::v5,
        ]
        .into_iter()
        .any(|constructor| {
            constructor(identity.document_id.clone(), identity.epoch.clone())
                .is_ok_and(|candidate| candidate == identity)
        });
        if !valid {
            return Err(invalid("missing or unsupported document identity"));
        }
        #[cfg(not(unix))]
        {
            let _ = root;
            return Err(StorageError {
                kind: FailureKind::UnsupportedPlatform,
                detail: "native storage is qualified only on Unix".into(),
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, MetadataExt};
            let root = root.as_ref();
            if !root.try_exists().map_err(io_error)? {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(root)
                    .map_err(io_error)?;
                File::open(root.parent().ok_or_else(|| invalid("missing parent"))?)
                    .map_err(io_error)?
                    .sync_all()
                    .map_err(io_error)?;
            }
            let m = fs::symlink_metadata(root).map_err(io_error)?;
            if !m.is_dir() || m.file_type().is_symlink() || m.mode() & 0o077 != 0 {
                return Err(invalid(
                    "storage must be a private regular directory (0700)",
                ));
            }
            Ok(Self {
                root: fs::canonicalize(root).map_err(io_error)?,
                identity,
                inode: (m.dev(), m.ino()),
            })
        }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    fn lock(&self) -> Result<StorageLock, StorageError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let m = fs::symlink_metadata(&self.root).map_err(io_error)?;
            if !m.is_dir() || (m.dev(), m.ino()) != self.inode || m.mode() & 0o077 != 0 {
                return Err(invalid("storage directory changed"));
            }
            let path = self.root.join("lock");
            if let Ok(m) = fs::symlink_metadata(&path) {
                if !m.is_file() || m.nlink() != 1 || m.mode() & 0o077 != 0 {
                    return Err(invalid("unsafe lock inode"));
                }
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
                .map_err(io_error)?;
            let a = file.metadata().map_err(io_error)?;
            let b = fs::symlink_metadata(path).map_err(io_error)?;
            if !b.is_file() || (a.dev(), a.ino()) != (b.dev(), b.ino()) {
                return Err(invalid("lock replaced"));
            }
            file.try_lock().map_err(|e| match e {
                fs::TryLockError::WouldBlock => StorageError {
                    kind: FailureKind::Busy,
                    detail: "another cooperating writer holds storage".into(),
                },
                fs::TryLockError::Error(e) => io_error(e),
            })?;
            Ok(StorageLock(file))
        }
        #[cfg(not(unix))]
        {
            Err(StorageError {
                kind: FailureKind::UnsupportedPlatform,
                detail: "Unix only".into(),
            })
        }
    }
    fn read<T: DeserializeOwned>(&self, name: &str) -> Result<T, StorageError> {
        let path = self.root.join(name);
        let m = fs::symlink_metadata(&path).map_err(io_error)?;
        if !m.is_file() || m.len() > MAX_RECORD_BYTES {
            return Err(invalid("unsafe/oversized record"));
        }
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(io_error)?
            .take(MAX_RECORD_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(invalid("record size limit"));
        }
        decode(&bytes)
    }
    fn entries(&self) -> Result<Vec<JournalEntry>, StorageError> {
        let mut entries = Vec::new();
        let mut total = 0;
        for path in fs::read_dir(&self.root).map_err(io_error)? {
            let name = path
                .map_err(io_error)?
                .file_name()
                .into_string()
                .map_err(|_| invalid("invalid filename"))?;
            if !name.starts_with("journal-") {
                continue;
            }
            if entries.len() >= MAX_ENTRIES {
                return Err(invalid("journal entry count limit"));
            }
            let entry: JournalEntry = self.read(&name)?;
            entry.validate(&self.identity)?;
            if name != entry.filename() {
                return Err(invalid("journal key mismatch"));
            }
            total += entry.bytes.len();
            if total > MAX_RESTORE_BYTES {
                return Err(invalid(
                    "journal byte limit; retain data for explicit compaction",
                ));
            }
            entries.push(entry);
        }
        entries.sort_by_key(|e| (e.writer_id, e.sequence));
        Ok(entries)
    }
    fn recovery(&self, entries: Vec<JournalEntry>) -> Result<Recovery, StorageError> {
        let replay: Vec<_> = entries.iter().map(|e| e.replay(&self.identity)).collect();
        let mut rejected = None;
        let mut incompatible = false;
        for (name, point) in [
            ("latest.json", RecoveryPoint::Latest),
            ("previous.json", RecoveryPoint::Previous),
        ] {
            let attempt = (|| {
                let r: CheckpointRecord = self.read(name)?;
                let c = r.checkpoint(&self.identity, &entries)?;
                let document =
                    Document::restore(&self.identity, &c, &replay).map_err(engine_error)?;
                Ok::<_, StorageError>((r, document))
            })();
            match attempt {
                Ok((r, document)) => {
                    return Ok(Recovery {
                        document,
                        point,
                        tail: entries
                            .iter()
                            .filter(|e| !r.coverage.contains(&e.update_id))
                            .cloned()
                            .collect(),
                        pending: self.unreceipted(&entries)?,
                        coverage: r.coverage,
                        draft: self.draft_unlocked().ok().flatten(),
                        rejected_latest: rejected,
                        rejected_draft: self.draft_unlocked().err().map(|e| e.to_string()),
                    });
                }
                Err(e) => {
                    incompatible |= e.kind == FailureKind::IncompatibleIdentity;
                    if point == RecoveryPoint::Latest {
                        rejected = Some(e.to_string());
                    }
                }
            }
        }
        Err(StorageError {
            kind: if incompatible {
                FailureKind::IncompatibleIdentity
            } else {
                FailureKind::Missing
            },
            detail: format!(
                "no valid recovery point; latest: {}",
                rejected.unwrap_or_default()
            ),
        })
    }
    fn receipts(&self, entries: &[JournalEntry]) -> Result<Vec<DurableReceipt>, StorageError> {
        if !self
            .root
            .join("receipts.json")
            .try_exists()
            .map_err(io_error)?
        {
            return Ok(vec![]);
        }
        let record: ReceiptRecord = self.read("receipts.json")?;
        record.identity.admit(&self.identity)?;
        let mut unique = HashSet::new();
        if record.receipts.len() > MAX_ENTRIES
            || record
                .receipts
                .iter()
                .any(|r| !unique.insert(&r.update_id) || !entries.iter().any(|e| r.matches(e)))
        {
            return Err(invalid("stored receipt identity mismatch"));
        }
        Ok(record.receipts)
    }
    fn unreceipted(&self, entries: &[JournalEntry]) -> Result<Vec<JournalEntry>, StorageError> {
        let receipts = self.receipts(entries)?;
        Ok(entries
            .iter()
            .filter(|e| !receipts.iter().any(|r| r.matches(e)))
            .cloned()
            .collect())
    }
    /// Never delete recovery bytes. Exact admitted receipts alone change outbox membership.
    pub fn accept_receipt(&self, receipt: &DurableReceipt) -> Result<bool, StorageError> {
        self.accept_receipt_with(receipt, &NativeIo)
    }
    pub fn accept_receipt_with(
        &self,
        receipt: &DurableReceipt,
        hooks: &dyn IoHooks,
    ) -> Result<bool, StorageError> {
        let _lock = self.lock()?;
        let entries = self.entries()?;
        let mut receipts = self.receipts(&entries)?;
        if !entries.iter().any(|e| receipt.matches(e)) {
            return Ok(false);
        }
        if !receipts.iter().any(|r| r == receipt) {
            receipts.push(receipt.clone());
        }
        self.publish(
            "receipts.json",
            &ReceiptRecord {
                identity: StoredIdentity::from(&self.identity),
                receipts,
            },
            RecordKind::Receipt,
            hooks,
        )?;
        Ok(true)
    }
    /// Verify the exact current replica is durable before any transport can attach.
    pub fn contains_state(&self, document: &Document) -> Result<bool, StorageError> {
        let _lock = self.lock()?;
        if document.identity() != &self.identity {
            return Ok(false);
        }
        let recovered = self.recovery(self.entries()?)?;
        same_state(
            &recovered.document.checkpoint().bytes,
            &document.checkpoint().bytes,
        )
    }
    /// A retry after uncertain publication reuses immutable identity and bytes.
    pub fn sequence_for_update(
        &self,
        document: &Document,
        update: &ReplayUpdate,
        origin: &str,
    ) -> Result<u64, StorageError> {
        let _lock = self.lock()?;
        let entries = self.entries()?;
        if let Some(entry) = entries.iter().find(|e| {
            e.writer_id == document.writer_id()
                && e.identity == StoredIdentity::from(&update.identity)
                && e.origin == origin
                && e.bytes == update.bytes
        }) {
            return Ok(entry.sequence);
        }
        Ok(entries
            .iter()
            .filter(|e| e.writer_id == document.writer_id())
            .map(|e| e.sequence)
            .max()
            .unwrap_or(0)
            + 1)
    }
    /// Recovery can find a publication whose prior caller never observed a
    /// directory-sync acknowledgment. Cross the native boundary explicitly;
    /// file existence alone is not an acknowledgment for status/attachment.
    pub fn durability_barrier(
        &self,
        document: &Document,
        hooks: &dyn IoHooks,
    ) -> Result<bool, StorageError> {
        let _lock = self.lock()?;
        if document.identity() != &self.identity {
            return Ok(false);
        }
        let recovery = self.recovery(self.entries()?)?;
        if !same_state(
            &recovery.document.checkpoint().bytes,
            &document.checkpoint().bytes,
        )? {
            return Ok(false);
        }
        for item in fs::read_dir(&self.root).map_err(io_error)? {
            let path = item.map_err(io_error)?.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| invalid("invalid filename"))?;
            if name.starts_with("journal-")
                || matches!(name, "latest.json" | "previous.json" | "receipts.json")
            {
                if !fs::symlink_metadata(&path).map_err(io_error)?.is_file() {
                    return Err(invalid("unsafe durability record"));
                }
                File::open(path)
                    .and_then(|f| f.sync_all())
                    .map_err(io_error)?;
            }
        }
        hooks
            .boundary(RecordKind::Latest, Boundary::AfterFileSync)
            .map_err(io_error)?;
        File::open(&self.root)
            .and_then(|f| f.sync_all())
            .map_err(io_error)?;
        hooks
            .boundary(RecordKind::Latest, Boundary::AfterDirectorySync)
            .map_err(io_error)?;
        Ok(true)
    }
    pub fn committed_receipts(&self) -> Result<usize, StorageError> {
        let _lock = self.lock()?;
        Ok(self.receipts(&self.entries()?)?.len())
    }

    pub fn recover(&self) -> Result<Recovery, StorageError> {
        let _lock = self.lock()?;
        self.recovery(self.entries()?)
    }
    fn publish<T: Serialize>(
        &self,
        name: &str,
        record: &T,
        kind: RecordKind,
        hooks: &dyn IoHooks,
    ) -> Result<(), StorageError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let bytes = encode(record)?;
        let destination = self.root.join(name);
        if let Ok(m) = fs::symlink_metadata(&destination) {
            if !m.is_file() {
                return Err(invalid("unsafe publication target"));
            }
        }
        let temp = self.root.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut published = false;
        let result = (|| {
            hooks.boundary(kind, Boundary::BeforeWrite)?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(&bytes)?;
            hooks.boundary(kind, Boundary::AfterWrite)?;
            file.sync_all()?;
            hooks.boundary(kind, Boundary::AfterFileSync)?;
            hooks.boundary(kind, Boundary::BeforePublish)?;
            fs::rename(&temp, &destination)?;
            published = true;
            hooks.boundary(kind, Boundary::AfterPublish)?;
            File::open(&self.root)?.sync_all()?;
            hooks.boundary(kind, Boundary::AfterDirectorySync)?;
            Ok::<_, io::Error>(())
        })();
        // Interrupted processes can leave private temporaries; recovery ignores them.
        if !published {
            let _ = fs::remove_file(temp);
        }
        result.map_err(|e| StorageError {
            kind: if published {
                FailureKind::PublicationUncertain
            } else {
                FailureKind::Io
            },
            detail: e.to_string(),
        })
    }
    pub fn initialize(&self, document: &Document) -> Result<(), StorageError> {
        self.initialize_with(document, &NativeIo)
    }
    pub fn initialize_with(
        &self,
        document: &Document,
        hooks: &dyn IoHooks,
    ) -> Result<(), StorageError> {
        let _lock = self.lock()?;
        if self
            .root
            .join("latest.json")
            .try_exists()
            .map_err(io_error)?
            || self
                .root
                .join("previous.json")
                .try_exists()
                .map_err(io_error)?
            || !self.entries()?.is_empty()
        {
            return Err(invalid("existing lineage cannot be reinitialized"));
        }
        let r = CheckpointRecord::new(document, &[]);
        r.checkpoint(&self.identity, &[])?;
        self.publish("previous.json", &r, RecordKind::Previous, hooks)?;
        self.publish("latest.json", &r, RecordKind::Latest, hooks)
    }
    pub fn persist_update(
        &self,
        document: &Document,
        update: &ReplayUpdate,
        sequence: u64,
        origin: &str,
    ) -> Result<LocalCommit, StorageError> {
        self.persist_update_with(document, update, sequence, origin, &NativeIo)
    }
    pub fn persist_update_with(
        &self,
        document: &Document,
        update: &ReplayUpdate,
        sequence: u64,
        origin: &str,
        hooks: &dyn IoHooks,
    ) -> Result<LocalCommit, StorageError> {
        let _lock = self.lock()?;
        let entry = JournalEntry::new(document, update, sequence, origin)?;
        entry.validate(&self.identity)?;
        // Verify bytes actually belong to the caller's committed engine state.
        let c = document.checkpoint();
        let restored = Document::restore(&self.identity, &c, std::slice::from_ref(update))
            .map_err(engine_error)?;
        if !same_state(&restored.checkpoint().bytes, &c.bytes)? {
            return Err(invalid("update is not committed in caller's replica"));
        }
        let mut entries = self.entries()?;
        let recovery = self.recovery(entries.clone())?;
        if let Some(existing) = entries
            .iter()
            .find(|e| e.writer_id == entry.writer_id && e.sequence == entry.sequence)
        {
            if existing != &entry {
                return Err(invalid("immutable journal key collision"));
            }
        } else {
            if entries
                .iter()
                .any(|e| e.writer_id == entry.writer_id && e.sequence >= entry.sequence)
            {
                return Err(invalid("nonmonotonic writer sequence"));
            }
            if entries.len() >= MAX_ENTRIES
                || entries.iter().map(|e| e.bytes.len()).sum::<usize>() + entry.bytes.len()
                    > MAX_RESTORE_BYTES
            {
                return Err(invalid("journal capacity; no acknowledgment"));
            }
            entries.push(entry.clone());
            let updates: Vec<_> = entries.iter().map(|e| e.replay(&self.identity)).collect();
            Document::restore(&self.identity, &recovery.document.checkpoint(), &updates)
                .map_err(engine_error)?;
        }
        // Re-publish exact duplicates so a retry after uncertain publication must
        // cross the durability boundary anew before acknowledging.
        self.publish(&entry.filename(), &entry, RecordKind::Journal, hooks)?;
        Ok(LocalCommit {
            update_id: entry.update_id,
            durability: "file-sync+atomic-rename+directory-sync",
        })
    }
    pub fn publish_checkpoint(
        &self,
        document: &Document,
        coverage: &[String],
    ) -> Result<(), StorageError> {
        self.publish_checkpoint_with(document, coverage, &NativeIo)
    }
    pub fn publish_checkpoint_with(
        &self,
        document: &Document,
        coverage: &[String],
        hooks: &dyn IoHooks,
    ) -> Result<(), StorageError> {
        let _lock = self.lock()?;
        let entries = self.entries()?;
        let recovery = self.recovery(entries.clone())?;
        let r = CheckpointRecord::new(document, coverage);
        let c = r.checkpoint(&self.identity, &entries)?;
        let replay: Vec<_> = entries.iter().map(|e| e.replay(&self.identity)).collect();
        // Never publish a stale or foreign checkpoint that drops committed state.
        let restored = Document::restore(&self.identity, &c, &replay).map_err(engine_error)?;
        if !same_state(&restored.checkpoint().bytes, &c.bytes)?
            || !same_state(&recovery.document.checkpoint().bytes, &c.bytes)?
        {
            return Err(invalid("checkpoint differs from recovered committed state"));
        }
        if recovery.point == RecoveryPoint::Latest {
            let prior: CheckpointRecord = self.read("latest.json")?;
            self.publish("previous.json", &prior, RecordKind::Previous, hooks)?;
        }
        self.publish("latest.json", &r, RecordKind::Latest, hooks)
    }
    fn draft_unlocked(&self) -> Result<Option<Draft>, StorageError> {
        if !self
            .root
            .join("draft.json")
            .try_exists()
            .map_err(io_error)?
        {
            return Ok(None);
        }
        let d: StoredDraft = self.read("draft.json")?;
        d.identity.admit(&self.identity)?;
        Self::admit_draft(&d.draft)?;
        Ok(Some(d.draft))
    }
    fn admit_draft(d: &Draft) -> Result<(), StorageError> {
        if d.baseline.len() + d.source.len() > MAX_RESTORE_BYTES {
            return Err(invalid("draft size limit"));
        }
        Ok(())
    }
    /// Available even if committed recovery is corrupt; no implicit deletion.
    pub fn read_draft(&self) -> Result<Option<Draft>, StorageError> {
        let _lock = self.lock()?;
        self.draft_unlocked()
    }
    pub fn persist_draft(&self, draft: &Draft) -> Result<(), StorageError> {
        self.persist_draft_with(draft, &NativeIo)
    }
    pub fn persist_draft_with(
        &self,
        draft: &Draft,
        hooks: &dyn IoHooks,
    ) -> Result<(), StorageError> {
        let _lock = self.lock()?;
        Self::admit_draft(draft)?;
        self.publish(
            "draft.json",
            &StoredDraft {
                identity: StoredIdentity::from(&self.identity),
                draft: draft.clone(),
            },
            RecordKind::Draft,
            hooks,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::{Doc, ReadTxn, StateVector, Text, Transact};

    #[test]
    fn nested_map_encoding_order_preserves_state_but_changed_data_does_not() {
        use std::{collections::HashMap, sync::Arc};
        use yrs::{Any, Map, encoding::write::Write};
        fn map_bytes(order: [&str; 2]) -> Vec<u8> {
            let mut bytes = vec![118, 2]; // lib0 Any map tag and entry count
            for key in order {
                bytes.write_string(key);
                Any::String(format!("{key}-value").into()).encode(&mut bytes);
            }
            bytes
        }
        let doc = Doc::with_client_id(7);
        let map = doc.get_or_insert_map("metadata");
        map.insert(
            &mut doc.transact_mut(),
            "value",
            Any::Map(Arc::new(HashMap::from([
                ("alpha".into(), Any::String("alpha-value".into())),
                ("omega".into(), Any::String("omega-value".into())),
            ]))),
        );
        let encoded = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let forward = map_bytes(["alpha", "omega"]);
        let reverse = map_bytes(["omega", "alpha"]);
        let start = encoded
            .windows(forward.len())
            .position(|w| w == forward || w == reverse)
            .unwrap();
        let mut a = encoded.clone();
        let mut b = encoded;
        a[start..start + forward.len()].copy_from_slice(&forward);
        b[start..start + reverse.len()].copy_from_slice(&reverse);
        assert_ne!(a, b); // deterministic encoding variation, not random retries
        assert!(same_state(&a, &b).unwrap());
        b[start + 10] = b'X'; // alter retained unknown data without changing clocks
        assert!(!same_state(&a, &b).unwrap());
    }

    #[test]
    fn deletion_only_state_cannot_equal_its_older_checkpoint() {
        let doc = Doc::with_client_id(7);
        let text = doc.get_or_insert_text("text");
        text.insert(&mut doc.transact_mut(), 0, "abc");
        let before = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let clocks = doc.transact().state_vector();
        text.remove_range(&mut doc.transact_mut(), 1, 1);
        let txn = doc.transact();
        let after = txn.encode_state_as_update_v1(&StateVector::default());
        assert_eq!(txn.state_vector(), clocks);
        assert!(!same_state(&before, &after).unwrap());
    }
}
