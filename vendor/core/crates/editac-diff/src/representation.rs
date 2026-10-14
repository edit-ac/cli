//! Version 1 review representation contract. Source hashes cover unmodified bytes;
//! semantic hashes cover canonical JSON (sorted object keys) prefixed by its version.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const SEMANTIC_ENCODING: &str = "editac-semantic-json-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentHash(pub [u8; 32]);
impl ContentHash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
    pub fn hex(self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// Equality scope, not a causal order or attribution claim. The observed token is
/// the session's opaque version-1 token, including deletion state, never a vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionScope {
    pub document: String,
    pub epoch: String,
    pub schema: String,
    pub module_profile: String,
    pub observed_token: [u8; 65],
}
impl RevisionScope {
    fn validate(&self) -> Result<(), RepresentationError> {
        if self.observed_token[0] != 1
            || [
                &self.document,
                &self.epoch,
                &self.schema,
                &self.module_profile,
            ]
            .iter()
            .any(|s| s.is_empty() || s.len() > 256)
        {
            return Err(RepresentationError::InvalidScope);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Representation {
    ExactSourceUtf8,
    ProjectedMarkdown { serializer: String, dialect: String },
    SemanticV1,
}
impl Representation {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ExactSourceUtf8 => "Exact source (UTF-8 bytes)",
            Self::ProjectedMarkdown { .. } => "Projected Markdown (normalized serialization)",
            Self::SemanticV1 => "Semantic snapshot (content, formatting and structure)",
        }
    }
}

/// JSON values retain opaque attributes/tuples; object keys are canonicalized
/// recursively before hashing. Node order and run order remain significant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticRun {
    pub text: String,
    pub marks: Value,
}
/// UTF-16 source ranges and integrated shared-type identity, not terminal columns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticSpan {
    pub source: String,
    pub identity: String,
    pub start: u32,
    pub end: u32,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticNode {
    pub id: String,
    pub kind: String,
    pub parent: String,
    pub order: u32,
    pub spans: Vec<SemanticSpan>,
    pub runs: Vec<SemanticRun>,
    pub attributes: Value,
    pub editor: Value,
    pub opaque: Value,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticSnapshot {
    pub visible: Vec<SemanticNode>,
    pub recovery: Vec<SemanticNode>,
    pub retained_structure: Value,
}

/// Typed provenance must be supplied by the owning source/serializer boundary.
/// Even a valid UTF-8 transport buffer is explicitly rejected as a review input.
pub enum RevisionInput<'a> {
    ExactSource(&'a [u8]),
    ProjectedMarkdown {
        bytes: &'a [u8],
        serializer: &'a str,
        dialect: &'a str,
    },
    Semantic(&'a SemanticSnapshot),
    CrdtUpdate(&'a [u8]),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepresentationLimits {
    pub max_bytes: usize,
    pub max_nodes: usize,
    pub max_depth: usize,
}
impl Default for RepresentationLimits {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            max_nodes: 10_000,
            max_depth: 64,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepresentationError {
    InvalidScope,
    InvalidProfile,
    InvalidUtf8,
    BinarySource,
    InternalTransport,
    ResourceLimit,
    InvalidSemantic,
    IncompatibleScope,
    IncompatibleRepresentation,
}
impl std::fmt::Display for RepresentationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for RepresentationError {}

/// Immutable owned data: only read access is exposed. No raw source appears in
/// Debug logs. Profile/scope equality is checked separately from content hashes.
#[derive(Clone)]
pub struct ImmutableRevision {
    scope: RevisionScope,
    representation: Representation,
    bytes: Vec<u8>,
    hash: ContentHash,
}
impl std::fmt::Debug for ImmutableRevision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImmutableRevision")
            .field("representation", &self.representation)
            .field("byte_len", &self.bytes.len())
            .field("hash", &self.hash)
            .finish()
    }
}
impl ImmutableRevision {
    pub fn capture(
        scope: RevisionScope,
        input: RevisionInput<'_>,
        limits: RepresentationLimits,
    ) -> Result<Self, RepresentationError> {
        scope.validate()?;
        let (representation, bytes) = match input {
            RevisionInput::CrdtUpdate(_) => return Err(RepresentationError::InternalTransport),
            RevisionInput::ExactSource(bytes) => {
                validate_text(bytes, limits)?;
                (Representation::ExactSourceUtf8, bytes.to_vec())
            }
            RevisionInput::ProjectedMarkdown {
                bytes,
                serializer,
                dialect,
            } => {
                if [serializer, dialect]
                    .iter()
                    .any(|s| s.is_empty() || s.len() > 256)
                {
                    return Err(RepresentationError::InvalidProfile);
                }
                validate_text(bytes, limits)?;
                (
                    Representation::ProjectedMarkdown {
                        serializer: serializer.into(),
                        dialect: dialect.into(),
                    },
                    bytes.to_vec(),
                )
            }
            RevisionInput::Semantic(snapshot) => {
                if snapshot
                    .visible
                    .len()
                    .saturating_add(snapshot.recovery.len())
                    > limits.max_nodes
                {
                    return Err(RepresentationError::ResourceLimit);
                }
                let mut ids = BTreeSet::new();
                for node in snapshot.visible.iter().chain(&snapshot.recovery) {
                    if node.id.is_empty()
                        || node.kind.is_empty()
                        || node.parent.is_empty()
                        || !ids.insert(&node.id)
                        || node.spans.iter().any(|span| {
                            span.start > span.end
                                || span.source.is_empty()
                                || span.identity.is_empty()
                        })
                        || !node.runs.iter().all(|r| r.marks.is_object())
                    {
                        return Err(RepresentationError::InvalidSemantic);
                    }
                }
                // Validate borrowed fields before cloning/encoding a possibly huge input.
                let mut budget = limits.max_bytes;
                charge(SEMANTIC_ENCODING.len() + 1, &mut budget)?;
                for node in snapshot.visible.iter().chain(&snapshot.recovery) {
                    charge(64, &mut budget)?;
                    for text in [&node.id, &node.kind, &node.parent] {
                        charge(text.len(), &mut budget)?;
                    }
                    for span in &node.spans {
                        charge(32, &mut budget)?;
                        charge(
                            span.source
                                .len()
                                .saturating_add(span.identity.len())
                                .saturating_add(8),
                            &mut budget,
                        )?;
                    }
                    for run in &node.runs {
                        charge(16, &mut budget)?;
                        charge(run.text.len(), &mut budget)?;
                        bound_value(&run.marks, 0, limits, &mut budget)?;
                    }
                    bound_value(&node.attributes, 0, limits, &mut budget)?;
                    bound_value(&node.editor, 0, limits, &mut budget)?;
                    bound_value(&node.opaque, 0, limits, &mut budget)?;
                }
                bound_value(&snapshot.retained_structure, 0, limits, &mut budget)?;
                let mut value = serde_json::to_value(snapshot)
                    .map_err(|_| RepresentationError::InvalidSemantic)?;
                canonicalize(&mut value);
                let mut bytes = SEMANTIC_ENCODING.as_bytes().to_vec();
                bytes.push(b'\n');
                serde_json::to_writer(
                    &mut LimitedWriter {
                        bytes: &mut bytes,
                        limit: limits.max_bytes,
                    },
                    &value,
                )
                .map_err(|_| RepresentationError::ResourceLimit)?;
                if bytes.len() > limits.max_bytes {
                    return Err(RepresentationError::ResourceLimit);
                }
                (Representation::SemanticV1, bytes)
            }
        };
        let hash = ContentHash::of(&bytes);
        Ok(Self {
            scope,
            representation,
            bytes,
            hash,
        })
    }
    pub fn scope(&self) -> &RevisionScope {
        &self.scope
    }
    pub fn representation(&self) -> &Representation {
        &self.representation
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn hash(&self) -> ContentHash {
        self.hash
    }
    /// Establish comparison compatibility; equality does not identify an author.
    pub fn content_changed(&self, other: &Self) -> Result<bool, RepresentationError> {
        if self.scope.document != other.scope.document
            || self.scope.epoch != other.scope.epoch
            || self.scope.schema != other.scope.schema
            || self.scope.module_profile != other.scope.module_profile
        {
            return Err(RepresentationError::IncompatibleScope);
        }
        if self.representation != other.representation {
            return Err(RepresentationError::IncompatibleRepresentation);
        }
        Ok(self.hash != other.hash)
    }
}
fn validate_text(bytes: &[u8], limits: RepresentationLimits) -> Result<(), RepresentationError> {
    if bytes.len() > limits.max_bytes {
        return Err(RepresentationError::ResourceLimit);
    }
    std::str::from_utf8(bytes).map_err(|_| RepresentationError::InvalidUtf8)?;
    if bytes.contains(&0) {
        return Err(RepresentationError::BinarySource);
    }
    Ok(())
}
struct LimitedWriter<'a> {
    bytes: &'a mut Vec<u8>,
    limit: usize,
}
impl std::io::Write for LimitedWriter<'_> {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        if input.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("semantic output limit"));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn charge(bytes: usize, budget: &mut usize) -> Result<(), RepresentationError> {
    *budget = budget
        .checked_sub(bytes)
        .ok_or(RepresentationError::ResourceLimit)?;
    Ok(())
}
fn bound_value(
    value: &Value,
    depth: usize,
    limits: RepresentationLimits,
    budget: &mut usize,
) -> Result<(), RepresentationError> {
    if depth > limits.max_depth {
        return Err(RepresentationError::ResourceLimit);
    }
    charge(1, budget)?;
    match value {
        Value::String(s) => charge(s.len(), budget)?,
        Value::Array(items) => {
            for v in items {
                bound_value(v, depth + 1, limits, budget)?;
            }
        }
        Value::Object(items) => {
            for (k, v) in items {
                charge(k.len(), budget)?;
                bound_value(v, depth + 1, limits, budget)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn canonicalize(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(canonicalize),
        Value::Object(items) => {
            let sorted: std::collections::BTreeMap<_, _> =
                std::mem::take(items).into_iter().collect();
            for (key, mut value) in sorted {
                canonicalize(&mut value);
                items.insert(key, value);
            }
        }
        _ => {}
    }
}

/// Policy contract only; DIF-1.2 implements immutable storage, eviction and reads.
/// Both count and total encoded bytes must be bounded; zero means retain nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub max_revisions: usize,
    pub max_total_bytes: usize,
}
impl RetentionPolicy {
    pub fn permits(&self, revisions: usize, total_bytes: usize) -> bool {
        revisions <= self.max_revisions && total_bytes <= self.max_total_bytes
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnavailableReason {
    Missing,
    Pruned,
    KeyUnavailable,
    IncompatibleSchema,
}
/// Absence carries no current-replica fallback; locked/migrated history is explicit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryAvailability<T> {
    Available(T),
    Unavailable(UnavailableReason),
}
