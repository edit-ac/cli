//! Optional native SQLite WAL adapter. One host-selected local file per document scope.
//! Transactions reuse the portable policy; this module owns only persistence/identity leases.
use crate::ports::{
    memory::{FaultBoundary, InjectedFault, MemoryStore, State},
    *,
};
use editac_core::document::{DocumentIdentity, InitializedCheckpoint};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const APPLICATION_ID: i64 = 0x45414353;
const VERSION: i64 = 1;

/// Observations are diagnostics, never save acknowledgments. Hooks have no ambient configuration.
/// Invoked synchronously under the store lock; observers must not re-enter the adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionBoundary {
    BeforeWrite,
    BeforeCommit,
    AfterCommit,
}
pub type TransactionObserver = Arc<dyn Fn(TransactionBoundary) + Send + Sync>;
struct Inner {
    connection: Connection,
    fault: Option<InjectedFault>,
}
pub struct NativeStore {
    identity: DocumentIdentity,
    limits: StoreLimits,
    path: PathBuf,
    inner: Mutex<Inner>,
    observer: Option<TransactionObserver>,
}
fn sql_error(error: rusqlite::Error) -> PortError {
    use rusqlite::ErrorCode::*;
    match error {
        rusqlite::Error::SqliteFailure(e, _) => match e.code {
            DiskFull => PortError::Quota,
            PermissionDenied | ReadOnly => PortError::Permission,
            DatabaseBusy | DatabaseLocked => PortError::Conflict,
            DatabaseCorrupt | NotADatabase => PortError::Integrity,
            TooBig => PortError::ResourceLimit,
            _ => PortError::Unavailable,
        },
        _ => PortError::Integrity,
    }
}
fn io_error(error: std::io::Error) -> PortError {
    match error.kind() {
        std::io::ErrorKind::AlreadyExists => PortError::Conflict,
        std::io::ErrorKind::PermissionDenied => PortError::Permission,
        _ => PortError::Unavailable,
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    document_id: String,
    epoch: String,
    schema: String,
    profile: String,
    offsets: String,
    protocol_version: u32,
    update_encoding: u32,
}
impl From<&DocumentIdentity> for Scope {
    fn from(i: &DocumentIdentity) -> Self {
        Self {
            document_id: i.document_id.clone(),
            epoch: i.epoch.clone(),
            schema: i.schema.clone(),
            profile: i.profile.clone(),
            offsets: i.offsets.clone(),
            protocol_version: i.protocol_version,
            update_encoding: i.update_encoding,
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Id {
    writer: u32,
    sequence: u64,
    checksum: [u8; 32],
}
impl Id {
    fn encode(id: &UpdateId) -> Self {
        Self {
            writer: id.writer,
            sequence: id.sequence,
            checksum: id.checksum,
        }
    }
    fn decode(self, identity: &DocumentIdentity) -> UpdateId {
        UpdateId {
            identity: identity.clone(),
            writer: self.writer,
            sequence: self.sequence,
            checksum: self.checksum,
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: Id,
    origin: String,
    bytes: Vec<u8>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Point {
    initialized: bool,
    writer: u32,
    bytes: Vec<u8>,
    coverage: Vec<Id>,
    manifest: [u8; 32],
}
impl Point {
    fn encode(c: &CheckpointRecord) -> Self {
        Self {
            initialized: c.checkpoint().initialized,
            writer: c.checkpoint().writer_id,
            bytes: c.checkpoint().bytes.clone(),
            coverage: c.coverage().iter().map(Id::encode).collect(),
            manifest: *c.manifest(),
        }
    }
    fn decode(self, i: &DocumentIdentity) -> CheckpointRecord {
        // Retain the stored manifest. Recovery verifies candidates independently so a damaged
        // latest checkpoint cannot hide a valid previous envelope or invent an empty document.
        CheckpointRecord {
            checkpoint: InitializedCheckpoint {
                identity: i.clone(),
                initialized: self.initialized,
                writer_id: self.writer,
                bytes: self.bytes,
            },
            coverage: self.coverage.into_iter().map(|id| id.decode(i)).collect(),
            manifest: self.manifest,
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    generation: u64,
    journal: Vec<Entry>,
    receipted: Vec<Id>,
    latest: Option<Point>,
    previous: Option<Point>,
}
impl Snapshot {
    fn encode(s: &State) -> Self {
        Self {
            generation: s.generation,
            journal: s
                .journal
                .iter()
                .map(|e| Entry {
                    id: Id::encode(e.id()),
                    origin: e.origin().into(),
                    bytes: e.bytes().to_vec(),
                })
                .collect(),
            receipted: s.receipted.iter().map(Id::encode).collect(),
            latest: s.latest.as_ref().map(Point::encode),
            previous: s.previous.as_ref().map(Point::encode),
        }
    }
    fn decode(self, i: &DocumentIdentity, limits: StoreLimits) -> Result<State, PortError> {
        if self.generation > i64::MAX as u64
            || self.journal.len() > limits.max_records
            || self.receipted.len() > limits.max_records
        {
            return Err(PortError::ResourceLimit);
        }
        for point in [self.latest.as_ref(), self.previous.as_ref()]
            .into_iter()
            .flatten()
        {
            if point.coverage.len() > limits.max_records {
                return Err(PortError::ResourceLimit);
            }
        }
        if (self.generation == 0) != self.latest.is_none() {
            return Err(PortError::Integrity);
        }
        let mut journal = Vec::new();
        let mut sequences = BTreeMap::new();
        for e in self.journal {
            let entry = JournalEntry::from_stored(e.id.decode(i), e.origin, e.bytes)?;
            let last = sequences.entry(entry.id().writer).or_insert(0u64);
            if last.checked_add(1) != Some(entry.id().sequence) {
                return Err(PortError::Integrity);
            }
            *last = entry.id().sequence;
            journal.push(entry);
        }
        let mut receipted = Vec::new();
        for id in self.receipted.into_iter().map(|id| id.decode(i)) {
            if receipted.contains(&id) || !journal.iter().any(|e| e.id() == &id) {
                return Err(PortError::Integrity);
            }
            receipted.push(id);
        }
        Ok(State {
            generation: self.generation,
            journal,
            receipted,
            latest: self.latest.map(|p| p.decode(i)),
            previous: self.previous.map(|p| p.decode(i)),
        })
    }
}
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, PortError> {
    serde_json::to_vec(value).map_err(|_| PortError::Integrity)
}
impl NativeStore {
    /// Explicit creation only. Never replaces an existing file or creates parent directories.
    /// The host grants/validates this absolute local path and keeps its -wal/-shm sidecars together.
    pub fn create_new(
        path: impl AsRef<Path>,
        identity: DocumentIdentity,
        limits: StoreLimits,
    ) -> Result<Self, PortError> {
        Self::create_new_observed(path, identity, limits, None)
    }
    pub fn create_new_observed(
        path: impl AsRef<Path>,
        identity: DocumentIdentity,
        limits: StoreLimits,
        observer: Option<TransactionObserver>,
    ) -> Result<Self, PortError> {
        scope(&identity)?;
        MemoryStore::new(identity.clone(), limits)?;
        Self::path_check(path.as_ref())?;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path.as_ref())
            .map_err(io_error)?;
        let store = Self::connect(path.as_ref(), identity, limits, observer)?;
        {
            let mut inner = store.inner.lock().map_err(|_| PortError::Unavailable)?;
            let tx = inner
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            tx.execute_batch("CREATE TABLE state (id INTEGER PRIMARY KEY CHECK(id=1), scope BLOB NOT NULL, body BLOB NOT NULL); CREATE TABLE writers (writer INTEGER PRIMARY KEY CHECK(writer>0 AND writer<=4294967295));").map_err(sql_error)?;
            let memory = MemoryStore::new(store.identity.clone(), limits)?;
            tx.execute(
                "INSERT INTO state VALUES(1,?1,?2)",
                params![
                    encode(&Scope::from(&store.identity))?,
                    encode(&Snapshot::encode(&memory.snapshot()?))?
                ],
            )
            .map_err(sql_error)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)
                .map_err(sql_error)?;
            tx.pragma_update(None, "user_version", VERSION)
                .map_err(sql_error)?;
            tx.commit().map_err(|_| PortError::CommitUnknown)?;
        }
        Ok(store)
    }
    /// Existing opens never create a file/schema/checkpoint and reject another scope/version.
    pub fn open_existing(
        path: impl AsRef<Path>,
        identity: DocumentIdentity,
        limits: StoreLimits,
    ) -> Result<Self, PortError> {
        Self::open_existing_observed(path, identity, limits, None)
    }
    pub fn open_existing_observed(
        path: impl AsRef<Path>,
        identity: DocumentIdentity,
        limits: StoreLimits,
        observer: Option<TransactionObserver>,
    ) -> Result<Self, PortError> {
        scope(&identity)?;
        MemoryStore::new(identity.clone(), limits)?;
        Self::path_check(path.as_ref())?;
        // Inspect before setting WAL; incompatible files are not migrated or reconfigured.
        let connection =
            Connection::open_with_flags(path.as_ref(), OpenFlags::SQLITE_OPEN_READ_WRITE)
                .map_err(sql_error)?;
        let app: i64 = connection
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(sql_error)?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(sql_error)?;
        if app != APPLICATION_ID || version != VERSION {
            return Err(PortError::Incompatible);
        }
        let stored: Vec<u8> = connection
            .query_row("SELECT scope FROM state WHERE id=1", [], |r| r.get(0))
            .map_err(sql_error)?;
        if stored != encode(&Scope::from(&identity))? {
            return Err(PortError::Incompatible);
        }
        drop(connection);
        let store = Self::connect(path.as_ref(), identity, limits, observer)?;
        store.read(|_| Ok(()))?;
        Ok(store)
    }
    fn path_check(path: &Path) -> Result<(), PortError> {
        if !path.is_absolute()
            || path.file_name().is_none()
            || path.to_string_lossy().contains('\0')
        {
            return Err(PortError::InvalidRecord);
        }
        Ok(())
    }
    fn connect(
        path: &Path,
        identity: DocumentIdentity,
        limits: StoreLimits,
        observer: Option<TransactionObserver>,
    ) -> Result<Self, PortError> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(sql_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(sql_error)?;
        let mode: String = connection
            .pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))
            .map_err(sql_error)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(PortError::Unavailable);
        }
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sql_error)?;
        connection
            .pragma_update(None, "fullfsync", true)
            .map_err(sql_error)?;
        connection
            .pragma_update(None, "checkpoint_fullfsync", true)
            .map_err(sql_error)?;
        let sync: i64 = connection
            .pragma_query_value(None, "synchronous", |r| r.get(0))
            .map_err(sql_error)?;
        if sync != 2 {
            return Err(PortError::Unavailable);
        }
        Ok(Self {
            identity,
            limits,
            path: path.into(),
            inner: Mutex::new(Inner {
                connection,
                fault: None,
            }),
            observer,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    fn matches(&self, i: &DocumentIdentity) -> Result<(), PortError> {
        if i == &self.identity {
            Ok(())
        } else {
            Err(PortError::Incompatible)
        }
    }
    fn load(&self, connection: &Connection) -> Result<MemoryStore, PortError> {
        let (stored, size): (Vec<u8>, i64) = connection
            .query_row("SELECT scope,length(body) FROM state WHERE id=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(sql_error)?;
        if stored != encode(&Scope::from(&self.identity))? {
            return Err(PortError::Incompatible);
        }
        // JSON byte arrays use at most six bytes per payload byte. Bound reads before allocation.
        let cap = self
            .limits
            .max_total_bytes
            .checked_mul(6)
            .and_then(|v| {
                self.limits
                    .max_records
                    .checked_mul(4096)
                    .and_then(|m| v.checked_add(m))
            })
            .and_then(|v| v.checked_add(16384))
            .ok_or(PortError::ResourceLimit)?;
        if size < 0 || size as u64 > cap as u64 {
            return Err(PortError::ResourceLimit);
        }
        let body: Vec<u8> = connection
            .query_row("SELECT body FROM state WHERE id=1", [], |r| r.get(0))
            .map_err(sql_error)?;
        let snapshot: Snapshot = serde_json::from_slice(&body).map_err(|_| PortError::Integrity)?;
        let state = snapshot.decode(&self.identity, self.limits)?;
        for entry in &state.journal {
            if connection
                .query_row(
                    "SELECT writer FROM writers WHERE writer=?1",
                    [entry.id().writer],
                    |r| r.get::<_, u32>(0),
                )
                .optional()
                .map_err(sql_error)?
                .is_none()
            {
                return Err(PortError::Integrity);
            }
        }
        MemoryStore::from_state(self.identity.clone(), self.limits, state)
    }
    fn read<R>(
        &self,
        work: impl FnOnce(&MemoryStore) -> Result<R, PortError>,
    ) -> Result<R, PortError> {
        let mut inner = self.inner.lock().map_err(|_| PortError::Unavailable)?;
        let tx = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(sql_error)?;
        let result = work(&self.load(&tx)?)?;
        tx.commit().map_err(sql_error)?;
        Ok(result)
    }
    fn observe(&self, b: TransactionBoundary) {
        if let Some(observer) = &self.observer {
            observer(b);
        }
    }
    fn mutate<R>(
        &self,
        work: impl FnOnce(&Connection, &MemoryStore) -> Result<R, PortError>,
    ) -> Result<R, PortError> {
        let mut inner = self.inner.lock().map_err(|_| PortError::Unavailable)?;
        let fault = inner.fault.take();
        let tx = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let memory = self.load(&tx)?;
        self.observe(TransactionBoundary::BeforeWrite);
        if let Some(f) = fault.filter(|f| f.boundary == FaultBoundary::BeforeWrite) {
            return Err(f.error);
        }
        let result = work(&tx, &memory)?;
        tx.execute(
            "UPDATE state SET body=?1 WHERE id=1",
            [encode(&Snapshot::encode(&memory.snapshot()?))?],
        )
        .map_err(sql_error)?;
        self.observe(TransactionBoundary::BeforeCommit);
        if let Some(f) = fault.filter(|f| f.boundary == FaultBoundary::BeforeCommit) {
            return Err(f.error);
        }
        tx.commit().map_err(|_| PortError::CommitUnknown)?;
        self.observe(TransactionBoundary::AfterCommit);
        if fault.is_some_and(|f| f.boundary == FaultBoundary::AfterCommit) {
            return Err(PortError::CommitUnknown);
        }
        Ok(result)
    }
    /// Reserve the real engine replica's nonzero writer ID once, before its first local append.
    /// Reservations survive process death and are never reused/released. Restored replicas use
    /// fresh engine IDs. A collision rejects, so the host must restore a fresh replica and retry.
    pub fn claim_writer(&self, writer: u32) -> Result<(), PortError> {
        if writer == 0 {
            return Err(PortError::InvalidRecord);
        }
        self.mutate(|tx, _| {
            let count: i64 = tx
                .query_row("SELECT count(*) FROM writers", [], |r| r.get(0))
                .map_err(sql_error)?;
            if tx
                .query_row(
                    "SELECT writer FROM writers WHERE writer=?1",
                    [writer],
                    |r| r.get::<_, u32>(0),
                )
                .optional()
                .map_err(sql_error)?
                .is_some()
            {
                return Err(PortError::Conflict);
            }
            if count < 0 || count as u64 >= self.limits.max_records as u64 {
                return Err(PortError::ResourceLimit);
            }
            tx.execute("INSERT INTO writers VALUES(?1)", [writer])
                .map_err(sql_error)?;
            Ok(())
        })
    }
    /// Explicit deterministic native transaction failure injection; never enabled by environment.
    pub fn inject_once(&self, fault: InjectedFault) -> Result<(), PortError> {
        let mut inner = self.inner.lock().map_err(|_| PortError::Unavailable)?;
        if inner.fault.is_some() {
            return Err(PortError::Conflict);
        }
        inner.fault = Some(fault);
        Ok(())
    }
    /// Host storage quota in SQLite pages, also permitting real SQLITE_FULL qualification.
    pub fn set_max_pages(&self, pages: u32) -> Result<u32, PortError> {
        if pages == 0 {
            return Err(PortError::InvalidRecord);
        }
        self.inner
            .lock()
            .map_err(|_| PortError::Unavailable)?
            .connection
            .pragma_update_and_check(None, "max_page_count", pages, |r| r.get(0))
            .map_err(sql_error)
    }
    pub fn page_count(&self) -> Result<u32, PortError> {
        self.inner
            .lock()
            .map_err(|_| PortError::Unavailable)?
            .connection
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .map_err(sql_error)
    }
}
impl LocalStore for NativeStore {
    fn capabilities(&self) -> StoreCapabilities {
        StoreCapabilities {
            durability: Durability::ProcessCrash,
            atomic_transactions: true,
            serialized_writers: true,
        }
    }
    fn append(&self, entry: JournalEntry) -> Result<LocalCommit, PortError> {
        self.matches(&entry.id().identity)?;
        self.mutate(|tx, memory| {
            if tx
                .query_row(
                    "SELECT writer FROM writers WHERE writer=?1",
                    [entry.id().writer],
                    |r| r.get::<_, u32>(0),
                )
                .optional()
                .map_err(sql_error)?
                .is_none()
            {
                return Err(PortError::Permission);
            }
            let mut commit = memory.append(entry)?;
            commit.durability = Durability::ProcessCrash;
            Ok(commit)
        })
    }
    fn generation(&self, i: &DocumentIdentity) -> Result<u64, PortError> {
        self.matches(i)?;
        self.read(|s| s.generation(i))
    }
    fn publish(&self, g: u64, c: CheckpointRecord) -> Result<PublicationCommit, PortError> {
        self.matches(&c.checkpoint().identity)?;
        self.mutate(|_, s| {
            let mut commit = s.publish(g, c)?;
            if commit.generation > i64::MAX as u64 {
                return Err(PortError::ResourceLimit);
            }
            commit.durability = Durability::ProcessCrash;
            Ok(commit)
        })
    }
    fn recover(&self, i: &DocumentIdentity) -> Result<Recovery, PortError> {
        self.matches(i)?;
        self.read(|s| s.recover(i))
    }
    fn pending(&self, i: &DocumentIdentity) -> Result<Vec<JournalEntry>, PortError> {
        self.matches(i)?;
        self.read(|s| s.pending(i))
    }
    fn accept_receipt(
        &self,
        n: ReceiptCapability,
        r: DurableReceipt,
    ) -> Result<ReceiptResult, PortError> {
        if n != ReceiptCapability::DurableV1 || r.protocol != ReceiptCapability::DurableV1 {
            return Ok(ReceiptResult::CapabilityUnavailable);
        }
        self.matches(&r.update.identity)?;
        self.mutate(|_, s| s.accept_receipt(n, r))
    }
}
