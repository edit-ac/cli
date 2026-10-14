//! Document admission and one-time initialization for explicit registry profiles.
//!
//! Metadata and initialization authority live outside the CRDT. The host must grant
//! creation only for a new lineage and persist the resulting checkpoint atomically;
//! an empty state vector is never evidence that an existing document is new.
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use yrs::encoding::read::{Cursor, Read};
use yrs::updates::{
    decoder::{Decode, Decoder, DecoderV1},
    encoder::Encode,
};
use yrs::{Any, Doc, Map, Out, ReadTxn, StateVector, Transact, Transaction, Update, XmlTextPrelim};

pub const SCHEMA: &str = "editac.registry.v1";
pub const PROFILE: &str = "editac.yrs-yjs.v1";
pub const BLOCK_SCHEMA: &str = "editac.registry.blocks.v2";
pub const BLOCK_PROFILE: &str = "editac.yrs-yjs.blocks.v2";
pub const STRUCTURE_SCHEMA: &str = "editac.registry.structure.v3";
pub const STRUCTURE_PROFILE: &str = "editac.yrs-yjs.structure.v3";
pub const EDITOR_SCHEMA: &str = "editac.registry.editor.v4";
pub const EDITOR_PROFILE: &str = "editac.yrs-yjs.editor.v4";
pub const STABLE_EDITOR_SCHEMA: &str = "editac.registry.editor.v5";
pub const STABLE_EDITOR_PROFILE: &str = "editac.yrs-yjs.editor.v5";
pub const BLOCK_ATTRIBUTES: &str = "registry-block-attributes";

/// Atomic owner presentation. Unknown JSON array tuples remain recoverable.
#[derive(Clone, Debug, PartialEq)]
pub enum BlockAttributes {
    Paragraph,
    Heading(u8),
    Code(String),
    Separator,
    Unknown(Any),
}
impl BlockAttributes {
    /// Unknown extension tuples render as a paragraph without executing extensions.
    pub fn presentation(&self) -> Self {
        match self {
            Self::Unknown(_) => Self::Paragraph,
            known => known.clone(),
        }
    }
    pub fn tuple(&self) -> Any {
        match self {
            Self::Paragraph => Any::Array(vec![Any::from("paragraph")].into()),
            Self::Heading(level) => {
                Any::Array(vec![Any::from("heading"), Any::from(*level as u32)].into())
            }
            Self::Code(language) => {
                Any::Array(vec![Any::from("code"), Any::from(language.clone())].into())
            }
            Self::Separator => Any::Array(vec![Any::from("separator")].into()),
            Self::Unknown(value) => value.clone(),
        }
    }
}
fn json_value(value: &Any, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    match value {
        Any::Null | Any::Bool(_) | Any::String(_) => true,
        Any::Number(n) => n.as_f64().is_some_and(f64::is_finite),
        Any::Array(v) => v.iter().all(|x| json_value(x, depth + 1)),
        Any::Map(v) => v.values().all(|x| json_value(x, depth + 1)),
        _ => false,
    }
}
pub(crate) fn parse_block_tuple(value: &Any) -> Result<BlockAttributes, DocumentError> {
    let Any::Array(values) = value else {
        return Err(DocumentError::UnsupportedSchema);
    };
    match values.as_ref() {
        [Any::String(tag)] if tag.as_ref() == "paragraph" => Ok(BlockAttributes::Paragraph),
        [Any::String(tag), Any::Number(n)]
            if tag.as_ref() == "heading" && n.as_i64().is_some_and(|n| (1..=6).contains(&n)) =>
        {
            Ok(BlockAttributes::Heading(n.as_i64().unwrap() as u8))
        }
        [Any::String(tag), ..]
            if tag.as_ref() != "paragraph" && tag.as_ref() != "heading" && json_value(value, 0) =>
        {
            Ok(BlockAttributes::Unknown(value.clone()))
        }
        _ => Err(DocumentError::UnsupportedSchema),
    }
}
pub(crate) fn parse_editor_tuple(value: &Any) -> Result<BlockAttributes, DocumentError> {
    let Any::Array(values) = value else {
        return Err(DocumentError::UnsupportedSchema);
    };
    match values.as_ref() {
        [Any::String(tag)] if tag.as_ref() == "separator" => Ok(BlockAttributes::Separator),
        [Any::String(tag), Any::String(language)]
            if tag.as_ref() == "code"
                && language.len() <= 64
                && language
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+._-".contains(&b)) =>
        {
            Ok(BlockAttributes::Code(language.to_string()))
        }
        [Any::String(tag), ..] if matches!(tag.as_ref(), "code" | "separator") => {
            Err(DocumentError::UnsupportedSchema)
        }
        _ => parse_block_tuple(value),
    }
}
pub const ROOTS: [&str; 5] = [
    "registry-parents",
    "registry-texts",
    "registry-placements",
    "delete-probe-tombstones",
    "probe-split-join",
];
/// Bound the encoded input before decoding. Broader resource qualification is a
/// separate gate; this initialization API is not an untrusted transport port.
pub const MAX_RESTORE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentIdentity {
    pub profile: String,
    pub schema: String,
    pub protocol_version: u32,
    pub update_encoding: u32,
    pub offsets: String,
    pub document_id: String,
    pub epoch: String,
}
impl DocumentIdentity {
    /// IDs are independently assigned canonical lowercase UUIDs by the host.
    pub fn v1(
        document_id: impl Into<String>,
        epoch: impl Into<String>,
    ) -> Result<Self, DocumentError> {
        let identity = Self {
            profile: PROFILE.into(),
            schema: SCHEMA.into(),
            protocol_version: 1,
            update_encoding: 1,
            offsets: "utf16".into(),
            document_id: document_id.into(),
            epoch: epoch.into(),
        };
        identity.validate()?;
        Ok(identity)
    }
    pub fn v2(
        document_id: impl Into<String>,
        epoch: impl Into<String>,
    ) -> Result<Self, DocumentError> {
        let mut identity = Self::v1(document_id, epoch)?;
        identity.profile = BLOCK_PROFILE.into();
        identity.schema = BLOCK_SCHEMA.into();
        Ok(identity)
    }
    pub fn v3(
        document_id: impl Into<String>,
        epoch: impl Into<String>,
    ) -> Result<Self, DocumentError> {
        let mut identity = Self::v1(document_id, epoch)?;
        identity.profile = STRUCTURE_PROFILE.into();
        identity.schema = STRUCTURE_SCHEMA.into();
        Ok(identity)
    }
    pub fn v4(
        document_id: impl Into<String>,
        epoch: impl Into<String>,
    ) -> Result<Self, DocumentError> {
        let mut identity = Self::v1(document_id, epoch)?;
        identity.profile = EDITOR_PROFILE.into();
        identity.schema = EDITOR_SCHEMA.into();
        Ok(identity)
    }
    /// Select the qualified witnessed profile explicitly; never migrate restored data.
    pub fn v5(
        document_id: impl Into<String>,
        epoch: impl Into<String>,
    ) -> Result<Self, DocumentError> {
        let mut identity = Self::v1(document_id, epoch)?;
        identity.profile = STABLE_EDITOR_PROFILE.into();
        identity.schema = STABLE_EDITOR_SCHEMA.into();
        Ok(identity)
    }
    pub fn has_stable_presentation(&self) -> bool {
        self.profile == STABLE_EDITOR_PROFILE && self.schema == STABLE_EDITOR_SCHEMA
    }
    pub fn has_editor(&self) -> bool {
        (self.profile == EDITOR_PROFILE && self.schema == EDITOR_SCHEMA)
            || self.has_stable_presentation()
    }
    pub fn has_structure(&self) -> bool {
        (self.profile == STRUCTURE_PROFILE && self.schema == STRUCTURE_SCHEMA) || self.has_editor()
    }
    pub fn has_blocks(&self) -> bool {
        (self.profile == BLOCK_PROFILE && self.schema == BLOCK_SCHEMA) || self.has_structure()
    }
    fn validate(&self) -> Result<(), DocumentError> {
        if !canonical_uuid(&self.document_id) || !canonical_uuid(&self.epoch) {
            return Err(DocumentError::InvalidMetadata);
        }
        if !((self.profile == PROFILE && self.schema == SCHEMA) || self.has_blocks())
            || self.protocol_version != 1
            || self.update_encoding != 1
            || self.offsets != "utf16"
        {
            return Err(DocumentError::UnsupportedProfile);
        }
        Ok(())
    }
    fn admit(&self, incoming: &Self) -> Result<(), DocumentError> {
        self.validate()?;
        incoming.validate()?;
        if self.profile != incoming.profile || self.schema != incoming.schema {
            return Err(DocumentError::UnsupportedProfile);
        }
        if self.document_id != incoming.document_id {
            return Err(DocumentError::WrongDocument);
        }
        if self.epoch != incoming.epoch {
            return Err(DocumentError::WrongEpoch);
        }
        Ok(())
    }
}
fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocumentError {
    InvalidMetadata,
    UnsupportedProfile,
    WrongDocument,
    WrongEpoch,
    MissingInitialization,
    AlreadyInitialized,
    InvalidInitialContent,
    InvalidUpdate,
    IncompleteRestore,
    UnsupportedSchema,
    ResourceLimit,
    WriterUnavailable,
}
impl std::fmt::Display for DocumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for DocumentError {}

/// Non-cloneable authority for the one initializer. The host must also enforce
/// this across processes/durable storage. This process rejects repeat grants.
#[derive(Debug)]
pub struct CreationPermit {
    identity: DocumentIdentity,
    committed: bool,
}
// false reserves an in-flight creator; true means authoritative existing state.
static CREATED: OnceLock<Mutex<HashMap<(String, String), bool>>> = OnceLock::new();
impl CreationPermit {
    pub fn authorize_new(identity: DocumentIdentity) -> Result<Self, DocumentError> {
        identity.validate()?;
        let mut created = CREATED
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| DocumentError::WriterUnavailable)?;
        let key = (identity.document_id.clone(), identity.epoch.clone());
        if created.contains_key(&key) {
            return Err(DocumentError::AlreadyInitialized);
        }
        created.insert(key, false);
        Ok(Self {
            identity,
            committed: false,
        })
    }
}
impl Drop for CreationPermit {
    fn drop(&mut self) {
        if !self.committed {
            if let Ok(mut created) = CREATED.get_or_init(Default::default).lock() {
                let key = (
                    self.identity.document_id.clone(),
                    self.identity.epoch.clone(),
                );
                if created.get(&key) == Some(&false) {
                    created.remove(&key);
                }
            }
        }
    }
}

/// Ordinary initial nodes use the schema's writer-local ID allocation. Pair
/// fixtures are restored from the qualified initializer, never independently seeded.
#[derive(Clone, Debug)]
pub struct InitialNode {
    pub kind: String,
    pub text: String,
}
#[derive(Clone, Debug)]
pub struct ReplayUpdate {
    pub identity: DocumentIdentity,
    pub bytes: Vec<u8>,
}
/// Store this envelope even when `bytes` encodes an empty document. Initialization
/// cannot be inferred from bytes. Writer identity is provenance, never restored.
#[derive(Clone, Debug)]
pub struct InitializedCheckpoint {
    pub identity: DocumentIdentity,
    pub initialized: bool,
    pub writer_id: u32,
    pub bytes: Vec<u8>,
}

static LIVE_WRITERS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
#[derive(Debug)]
struct WriterLease(u32);
impl WriterLease {
    fn acquire(id: u32) -> Result<Option<Self>, DocumentError> {
        let mut live = LIVE_WRITERS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| DocumentError::WriterUnavailable)?;
        Ok(live.insert(id).then_some(Self(id)))
    }
}
impl Drop for WriterLease {
    fn drop(&mut self) {
        if let Ok(mut live) = LIVE_WRITERS.get_or_init(Default::default).lock() {
            live.remove(&self.0);
        }
    }
}

/// One authoritative Yrs replica. No public mutable Yrs handle or clone is exposed.
#[derive(Debug)]
pub struct Document {
    pub(crate) doc: Doc,
    projection: crate::projection::ProjectionIndex,
    identity: DocumentIdentity,
    _writer: WriterLease,
    next_node_counter: u64,
    pub(crate) next_structure_counter: u32,
    pub(crate) revision: u64,
    pub(crate) history: Option<crate::history::History>,
    pub(crate) patches: crate::patches::PatchState,
}
impl Document {
    pub fn create(
        mut permit: CreationPermit,
        nodes: &[InitialNode],
    ) -> Result<Self, DocumentError> {
        if permit.identity.has_structure() {
            if nodes.iter().any(|n| n.kind != "block") {
                return Err(DocumentError::InvalidInitialContent);
            }
            if nodes.len() > 4096
                || nodes.iter().any(|n| n.text.encode_utf16().count() > 262144)
                || nodes.iter().map(|n| n.text.len()).sum::<usize>() > 1048576
            {
                return Err(DocumentError::ResourceLimit);
            }
        }
        let size = nodes
            .iter()
            .try_fold(0usize, |n, node| {
                n.checked_add(node.text.len())
                    .and_then(|n| n.checked_add(node.kind.len()))
            })
            .ok_or(DocumentError::ResourceLimit)?;
        if size > MAX_RESTORE_BYTES || nodes.len() > 100_000 {
            return Err(DocumentError::ResourceLimit);
        }
        if nodes.iter().any(|n| n.kind.is_empty()) {
            return Err(DocumentError::InvalidInitialContent);
        }
        let key = (
            permit.identity.document_id.clone(),
            permit.identity.epoch.clone(),
        );
        let mut created = CREATED
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| DocumentError::WriterUnavailable)?;
        if created.get(&key) != Some(&false) {
            return Err(DocumentError::AlreadyInitialized);
        }
        let (doc, writer) = fresh_replica(&HashSet::new(), &permit.identity)?;
        if permit.identity.has_blocks() {
            doc.get_or_insert_map(BLOCK_ATTRIBUTES);
        }
        {
            let kinds = doc.get_or_insert_map(ROOTS[0]);
            let texts = doc.get_or_insert_map(ROOTS[1]);
            let placements = doc.get_or_insert_map(ROOTS[2]);
            let mut txn = doc.transact_mut_with("initialize");
            for (counter, node) in nodes.iter().enumerate() {
                let id = if permit.identity.has_structure() {
                    format!("n:{}:{}", doc.client_id().get(), counter + 1)
                } else {
                    format!("n-{:x}-{:x}", doc.client_id().get(), counter)
                };
                if permit.identity.has_structure() {
                    txn.get_map(crate::ownership::ROOTS[6]).unwrap().insert(
                        &mut txn,
                        id.clone(),
                        true,
                    );
                }
                kinds.insert(&mut txn, id.clone(), node.kind.as_str());
                texts.insert(&mut txn, id.clone(), XmlTextPrelim::new(node.text.as_str()));
                placements.insert(
                    &mut txn,
                    id.clone(),
                    Any::Array(vec![Any::from("@root"), Any::from(counter as u32)].into()),
                );
                if permit.identity.has_blocks() && node.kind == "block" {
                    txn.get_map(BLOCK_ATTRIBUTES).unwrap().insert(
                        &mut txn,
                        id,
                        BlockAttributes::Paragraph.tuple(),
                    );
                }
            }
        }
        if permit.identity.has_structure() {
            crate::editor::project(&doc, permit.identity.has_stable_presentation())
                .map_err(ownership_error)?;
        }
        // Ensure a document accepted by creation can also pass its restore bound.
        if doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default())
            .len()
            > MAX_RESTORE_BYTES
        {
            return Err(DocumentError::ResourceLimit);
        }
        created.insert(key, true);
        permit.committed = true;
        Ok(Self {
            projection: crate::projection::ProjectionIndex::with_identity(&doc, &permit.identity),
            doc,
            identity: permit.identity.clone(),
            _writer: writer,
            next_node_counter: nodes.len() as u64,
            next_structure_counter: 1,
            revision: 0,
            history: None,
            patches: Default::default(),
        })
    }

    /// Replay on a disposable replica; publish a live document only after all
    /// identity checks, decoding, dependency integration and root checks succeed.
    /// Failure leaves the borrowed checkpoint/journal available for recovery.
    pub fn restore(
        expected: &DocumentIdentity,
        checkpoint: &InitializedCheckpoint,
        journal: &[ReplayUpdate],
    ) -> Result<Self, DocumentError> {
        expected.admit(&checkpoint.identity)?;
        if !checkpoint.initialized {
            return Err(DocumentError::MissingInitialization);
        }
        let mut total = checkpoint.bytes.len();
        for update in journal {
            expected.admit(&update.identity)?;
            total = total
                .checked_add(update.bytes.len())
                .ok_or(DocumentError::ResourceLimit)?;
        }
        if total > MAX_RESTORE_BYTES {
            return Err(DocumentError::ResourceLimit);
        }
        let mut decoded = Vec::with_capacity(journal.len() + 1);
        for bytes in std::iter::once(&checkpoint.bytes).chain(journal.iter().map(|u| &u.bytes)) {
            decoded.push(if expected.has_structure() {
                crate::ownership::decode(bytes).map_err(ownership_error)?
            } else {
                Update::decode_v1(bytes).map_err(|_| DocumentError::InvalidUpdate)?
            });
        }
        // Staging has no editing lease and never performs local writes.
        let staging = crate::new_document();
        register_profile_roots(&staging, expected);
        if expected.has_blocks() {
            staging.get_or_insert_map(BLOCK_ATTRIBUTES);
        }
        {
            let mut txn = staging.transact_mut_with("restore");
            // A journal is one bounded causal corpus. Integrate its merged
            // structs/delete set together: incremental out-of-order application
            // can leave pending history deletions in Yrs even when every missing
            // struct is present later in this same journal. Genuine missing
            // dependencies still fail the pending-state check below.
            txn.apply_update(Update::merge_updates(decoded))
                .map_err(|_| DocumentError::InvalidUpdate)?;
        }
        if expected.has_structure() {
            let baseline = crate::new_document();
            register_profile_roots(&baseline, expected);
            baseline
                .transact_mut_with("restore:baseline")
                .apply_update(crate::ownership::decode(&checkpoint.bytes).map_err(ownership_error)?)
                .map_err(|_| DocumentError::InvalidUpdate)?;
            crate::editor::project(&baseline, expected.has_stable_presentation())
                .map_err(ownership_error)?;
            validate_immutable_structure(&baseline, &staging, expected.has_stable_presentation())?;
        }
        let txn = staging.transact();
        if txn.store().pending_update().is_some() || txn.store().pending_ds().is_some() {
            return Err(DocumentError::IncompleteRestore);
        }
        if expected.has_structure() {
            crate::editor::project(&staging, expected.has_stable_presentation())
                .map_err(ownership_error)?;
        } else {
            validate_roots(&txn, expected.has_blocks())?;
        }
        // The v2 profile qualifies the inherited ownership descriptor as well as
        // its tuples. Never publish/admit a split with corrupt binary anchors.
        if expected.has_blocks() {
            crate::projection::ProjectionIndex::with_identity(&staging, expected)
                .read()
                .map_err(|_| DocumentError::UnsupportedSchema)?;
        }
        let mut excluded: HashSet<u32> = txn
            .state_vector()
            .iter()
            .map(|(id, _)| id.get() as u32)
            .collect();
        excluded.insert(checkpoint.writer_id);
        let bytes = txn.encode_state_as_update_v1(&StateVector::default());
        drop(txn);
        let (doc, writer) = fresh_replica(&excluded, expected)?;
        if expected.has_blocks() {
            doc.get_or_insert_map(BLOCK_ATTRIBUTES);
        }
        doc.transact_mut_with("restore")
            .apply_update(Update::decode_v1(&bytes).map_err(|_| DocumentError::InvalidUpdate)?)
            .map_err(|_| DocumentError::InvalidUpdate)?;
        CREATED
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| DocumentError::WriterUnavailable)?
            .insert((expected.document_id.clone(), expected.epoch.clone()), true);
        Ok(Self {
            projection: crate::projection::ProjectionIndex::with_identity(&doc, expected),
            doc,
            identity: expected.clone(),
            _writer: writer,
            next_node_counter: 0,
            next_structure_counter: 1,
            revision: 0,
            history: None,
            patches: Default::default(),
        })
    }
    /// Qualify incoming bytes against a disposable combined checkpoint first.
    /// The caller retains the borrowed rejected envelope/bytes for recovery.
    pub fn apply_replay(&mut self, update: &ReplayUpdate) -> Result<(), DocumentError> {
        self.identity.admit(&update.identity)?;
        let checkpoint = self.checkpoint();
        let candidate = Self::restore(&self.identity, &checkpoint, std::slice::from_ref(update))?;
        if candidate.checkpoint().bytes == checkpoint.bytes {
            return Ok(());
        }
        if self.identity.has_structure() {
            validate_immutable_structure(
                &self.doc,
                &candidate.doc,
                self.identity.has_stable_presentation(),
            )?;
        }
        drop(candidate);
        let next_revision = self
            .revision
            .checked_add(1)
            .ok_or(DocumentError::ResourceLimit)?;
        let decoded = Update::decode_v1(&update.bytes).map_err(|_| DocumentError::InvalidUpdate)?;
        let patch_before = self
            .patch_before()
            .map_err(|_| DocumentError::UnsupportedSchema)?;
        self.doc
            .transact_mut_with("remote")
            .apply_update(decoded)
            .expect("validated combined update");
        self.revision = next_revision;
        self.reset_history_group();
        self.patch_finish(
            patch_before,
            crate::patches::PatchOperation::Remote,
            "remote",
            Some(update.clone()),
        );
        Ok(())
    }

    pub(crate) fn general_view(&self) -> crate::ownership::Result<crate::ownership::View> {
        crate::editor::project(&self.doc, self.identity.has_stable_presentation())
    }
    /// Exercise the complete mutation on an isolated replica, then validate replay
    /// and resource bounds. No temporary writer is admitted to the live document.
    pub(crate) fn validate_general_mutation(
        &self,
        mutate: impl FnOnce(&mut yrs::TransactionMut<'_>),
    ) -> Result<(), DocumentError> {
        let excluded: HashSet<_> = self
            .read()
            .state_vector()
            .iter()
            .map(|(id, _)| id.get() as u32)
            .chain([self.writer_id()])
            .collect();
        let stage = (0..128)
            .map(|_| crate::new_document())
            .find(|d| !excluded.contains(&(d.client_id().get() as u32)))
            .ok_or(DocumentError::WriterUnavailable)?;
        register_profile_roots(&stage, &self.identity);
        let bytes = self.checkpoint().bytes;
        stage
            .transact_mut_with("prepare:stage")
            .apply_update(crate::ownership::decode(&bytes).map_err(ownership_error)?)
            .map_err(|_| DocumentError::InvalidUpdate)?;
        {
            let mut txn = stage.transact_mut_with("prepare:stage");
            mutate(&mut txn);
        }
        if stage
            .transact()
            .encode_state_as_update_v1(&StateVector::default())
            .len()
            > MAX_RESTORE_BYTES
        {
            return Err(DocumentError::ResourceLimit);
        }
        crate::editor::project(&stage, self.identity.has_stable_presentation())
            .map_err(ownership_error)?;
        validate_immutable_structure(&self.doc, &stage, self.identity.has_stable_presentation())
    }
    pub fn snapshot(
        &self,
    ) -> Result<crate::projection::Snapshot, crate::projection::ProjectionError> {
        self.projection.read()
    }
    pub(crate) fn inspect(
        &self,
    ) -> Result<crate::projection::Snapshot, crate::projection::ProjectionError> {
        self.projection.inspect()
    }
    pub(crate) fn write(&mut self, origin: &str) -> yrs::TransactionMut<'_> {
        self.doc.transact_mut_with(origin)
    }
    /// Local observation token; not a globally comparable CRDT timestamp.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn identity(&self) -> &DocumentIdentity {
        &self.identity
    }
    pub fn is_initialized(&self) -> bool {
        true
    }
    pub fn writer_id(&self) -> u32 {
        self._writer.0
    }
    pub fn next_node_counter(&self) -> u64 {
        self.next_node_counter
    }
    /// Read-only view for subsequent engine projection; callers cannot write roots.
    pub(crate) fn read(&self) -> Transaction<'_> {
        self.doc.transact()
    }
    /// Owned v1 clock summary for sync. It is not an exact revision token.
    pub fn state_vector_v1(&self) -> Vec<u8> {
        self.read().state_vector().encode_v1()
    }
    /// Encode missing structs and the delete set without mutating the replica.
    /// Reject malformed/trailing state-vector bytes before encoding.
    pub fn update_v1(&self, state_vector: &[u8]) -> Result<Vec<u8>, DocumentError> {
        if state_vector.len() > MAX_RESTORE_BYTES {
            return Err(DocumentError::ResourceLimit);
        }
        let mut decoder = DecoderV1::new(Cursor::new(state_vector));
        let count: u32 = decoder
            .read_var()
            .map_err(|_| DocumentError::InvalidUpdate)?;
        // Every entry requires at least a client and clock byte. Bound the
        // attacker-controlled allocation before the library reserves the map.
        if count as usize > state_vector.len() / 2 {
            return Err(DocumentError::InvalidUpdate);
        }
        let mut decoder = DecoderV1::new(Cursor::new(state_vector));
        let vector = StateVector::decode(&mut decoder).map_err(|_| DocumentError::InvalidUpdate)?;
        if !decoder
            .read_to_end()
            .map_err(|_| DocumentError::InvalidUpdate)?
            .is_empty()
        {
            return Err(DocumentError::InvalidUpdate);
        }
        Ok(self.read().encode_state_as_update_v1(&vector))
    }
    pub fn checkpoint(&self) -> InitializedCheckpoint {
        InitializedCheckpoint {
            identity: self.identity.clone(),
            initialized: true,
            writer_id: self.writer_id(),
            bytes: self
                .read()
                .encode_state_as_update_v1(&StateVector::default()),
        }
    }
}
fn register_roots(doc: &Doc) {
    for root in ROOTS {
        doc.get_or_insert_map(root);
    }
}
fn fresh_replica(
    excluded: &HashSet<u32>,
    identity: &DocumentIdentity,
) -> Result<(Doc, WriterLease), DocumentError> {
    for _ in 0..128 {
        let doc = crate::new_document();
        let id = doc.client_id().get() as u32;
        if excluded.contains(&id) {
            continue;
        }
        if let Some(lease) = WriterLease::acquire(id)? {
            register_profile_roots(&doc, identity);
            return Ok((doc, lease));
        }
    }
    Err(DocumentError::WriterUnavailable)
}
fn validate_roots(txn: &Transaction<'_>, blocks: bool) -> Result<(), DocumentError> {
    for (name, value) in txn.root_refs() {
        if !(ROOTS.contains(&name) || (blocks && name == BLOCK_ATTRIBUTES))
            || !matches!(value, Out::YMap(map) if map.as_ref().len() == 0)
        {
            return Err(DocumentError::UnsupportedSchema);
        }
    }
    let kinds = txn
        .get_map(ROOTS[0])
        .ok_or(DocumentError::UnsupportedSchema)?;
    let texts = txn
        .get_map(ROOTS[1])
        .ok_or(DocumentError::UnsupportedSchema)?;
    for (id, kind) in kinds.iter(txn) {
        if !valid_node_id(id)
            || !matches!(kind, Out::Any(Any::String(_)))
            || !matches!(texts.get(txn, id), Some(Out::YXmlText(_)))
        {
            return Err(DocumentError::UnsupportedSchema);
        }
    }
    if texts.iter(txn).any(|(id, _)| !kinds.contains_key(txn, id)) {
        return Err(DocumentError::UnsupportedSchema);
    }
    let tombstones = txn
        .get_map(ROOTS[3])
        .ok_or(DocumentError::UnsupportedSchema)?;
    if tombstones
        .iter(txn)
        // The reserved split owner has no registry text of its own, but its
        // tombstone is part of the frozen ownership/visibility contract.
        .any(|(id, v)| {
            (!valid_node_id(id) && id != "block-split") || !matches!(v, Out::Any(Any::Bool(true)))
        })
    {
        return Err(DocumentError::UnsupportedSchema);
    }
    // Placements deliberately retain malformed values for the frozen read fallback.
    let layout = txn
        .get_map(ROOTS[4])
        .ok_or(DocumentError::UnsupportedSchema)?;
    for (key, value) in layout.iter(txn) {
        let valid = if let Out::Any(Any::Array(values)) = value {
            matches!(values.as_ref(), [Any::String(mode)] if mode.as_ref() == "original" || mode.as_ref() == "join")
                || matches!(values.as_ref(), [Any::String(mode), Any::Buffer(_)] if mode.as_ref() == "split")
        } else {
            false
        };
        if key != "pair"
            || !valid
            || !kinds.contains_key(txn, "block-1")
            || !kinds.contains_key(txn, "block-2")
        {
            return Err(DocumentError::UnsupportedSchema);
        }
    }
    if blocks {
        let attributes = txn
            .get_map(BLOCK_ATTRIBUTES)
            .ok_or(DocumentError::UnsupportedSchema)?;
        for (id, value) in attributes.iter(txn) {
            let ordinary = matches!(kinds.get(txn, id), Some(Out::Any(Any::String(kind))) if kind.as_ref() == "block");
            let derived = id == "block-split" && ["block-1", "block-2"].iter().all(|id| matches!(kinds.get(txn, id), Some(Out::Any(Any::String(kind))) if kind.as_ref() == "block"));
            if !ordinary && !derived {
                return Err(DocumentError::UnsupportedSchema);
            }
            let Out::Any(value) = value else {
                return Err(DocumentError::UnsupportedSchema);
            };
            parse_block_tuple(&value)?;
        }
        if layout.contains_key(txn, "pair") && !["block-1", "block-2"].iter().all(|id| matches!(kinds.get(txn, id), Some(Out::Any(Any::String(kind))) if kind.as_ref() == "block")) {
            return Err(DocumentError::UnsupportedSchema);
        }
    }
    Ok(())
}
pub(crate) fn valid_node_id(id: &str) -> bool {
    id != "block-split"
        && (1..=64).contains(&id.len())
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn register_profile_roots(doc: &Doc, identity: &DocumentIdentity) {
    if identity.has_editor() {
        for root in crate::editor_mapping::ROOTS {
            doc.get_or_insert_map(root);
        }
    } else if identity.has_structure() {
        for root in crate::ownership::ROOTS {
            doc.get_or_insert_map(root);
        }
    } else {
        register_roots(doc);
    }
    if identity.has_blocks() {
        doc.get_or_insert_map(BLOCK_ATTRIBUTES);
    }
}
pub(crate) fn ownership_error(error: &str) -> DocumentError {
    match error {
        "resource-limit" => DocumentError::ResourceLimit,
        "missing-binary-dependency" => DocumentError::IncompleteRestore,
        "invalid-bytes" => DocumentError::InvalidUpdate,
        _ => DocumentError::UnsupportedSchema,
    }
}
pub(crate) fn validate_immutable_structure(
    before: &Doc,
    after: &Doc,
    stable: bool,
) -> Result<(), DocumentError> {
    let a = before.transact();
    let b = after.transact();
    for root in if a.get_map(crate::editor_mapping::ROOTS[9]).is_some() {
        vec![0, 2, 3, 5, 6]
    } else {
        vec![0, 3, 5, 6]
    } {
        let old = a.get_map(crate::ownership::ROOTS[root]).unwrap();
        let new = b.get_map(crate::ownership::ROOTS[root]).unwrap();
        for (key, value) in old.iter(&a) {
            if new.get(&b, key) != Some(value) {
                return Err(DocumentError::UnsupportedSchema);
            }
        }
    }
    let old = crate::editor::project(before, stable).map_err(ownership_error)?;
    let new = crate::editor::project(after, stable).map_err(ownership_error)?;
    for (key, value) in old.sources {
        let Any::Map(v) = value else { unreachable!() };
        let Some(Any::Map(w)) = new.sources.get(&key) else {
            return Err(DocumentError::UnsupportedSchema);
        };
        if v["identity"] != w["identity"] {
            return Err(DocumentError::UnsupportedSchema);
        }
    }
    Ok(())
}
