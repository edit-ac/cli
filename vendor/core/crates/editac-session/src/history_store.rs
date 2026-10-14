//! On-demand immutable review history. Not an undo stack, encrypted archive, or live replica.
//! No edit/receive path calls this module. Host callbacks own representation provenance.
use crate::{
    revision::ObservedRevision,
    session::{Session, SessionError, SessionStore},
};
use editac_core::document::Document;
use editac_diff::representation::{
    ContentHash, HistoryAvailability, ImmutableRevision, Representation, RepresentationError,
    RetentionPolicy, RevisionScope, UnavailableReason,
};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_STORE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryKey {
    namespace: String,
    instance: u64,
    sequence: u64,
}

/// All bindings must come from the capture receipt, not the current replica.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRequest {
    pub key: HistoryKey,
    pub scope: RevisionScope,
    pub representation: Representation,
    pub hash: ContentHash,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HistoryError {
    InvalidLabel,
    ResourceLimit,
    SequenceExhausted,
    ScopeMismatch,
    RepresentationMismatch,
    HashMismatch,
    Session(SessionError),
    Representation(RepresentationError),
}
impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for HistoryError {}

/// Metadata is descriptive; a name is never an authenticated author claim.
/// Key profiles describe host access generations, not cryptographic implementation.
pub struct HistoricalRevision {
    name: String,
    required_key_profile: Option<String>,
    revision: ImmutableRevision,
    request: HistoryRequest,
    accounted_bytes: usize,
}
impl HistoricalRevision {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn revision(&self) -> &ImmutableRevision {
        &self.revision
    }
    pub fn request(&self) -> &HistoryRequest {
        &self.request
    }
    pub fn required_key_profile(&self) -> Option<&str> {
        self.required_key_profile.as_deref()
    }
}
impl std::fmt::Debug for HistoricalRevision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoricalRevision")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

/// FIFO bound covers payload and variable retained metadata; entry overhead is count-bounded.
/// Reads borrow storage, and there is no separate cache or growing eviction ledger.
pub struct HistoryStore {
    namespace: String,
    instance: u64,
    policy: RetentionPolicy,
    issued: u64,
    total_bytes: usize,
    entries: VecDeque<HistoricalRevision>,
}
impl HistoryStore {
    pub fn new(namespace: &str, policy: RetentionPolicy) -> Result<Self, HistoryError> {
        label(namespace)?;
        let instance = NEXT_STORE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| HistoryError::SequenceExhausted)?;
        Ok(Self {
            namespace: namespace.into(),
            instance,
            policy,
            issued: 0,
            total_bytes: 0,
            entries: VecDeque::new(),
        })
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Store an already-materialized immutable revision; rejected captures do not evict history.
    pub fn capture(
        &mut self,
        name: &str,
        revision: ImmutableRevision,
        required_key_profile: Option<&str>,
    ) -> Result<HistoryRequest, HistoryError> {
        label(name)?;
        if let Some(profile) = required_key_profile {
            label(profile)?;
        }
        let scope = revision.scope();
        // Scope/profile are held twice: by the immutable revision and capture receipt.
        let profile_bytes = match revision.representation() {
            Representation::ProjectedMarkdown {
                serializer,
                dialect,
            } => serializer.len().saturating_add(dialect.len()),
            _ => 0,
        };
        let metadata = scope
            .document
            .len()
            .saturating_add(scope.epoch.len())
            .saturating_add(scope.schema.len())
            .saturating_add(scope.module_profile.len())
            .saturating_add(profile_bytes)
            .saturating_mul(2);
        let bytes = revision
            .bytes()
            .len()
            .saturating_add(metadata)
            .saturating_add(name.len())
            .saturating_add(required_key_profile.map_or(0, str::len))
            .saturating_add(self.namespace.len());
        if self.policy.max_revisions == 0 || bytes > self.policy.max_total_bytes {
            return Err(HistoryError::ResourceLimit);
        }
        let sequence = self
            .issued
            .checked_add(1)
            .ok_or(HistoryError::SequenceExhausted)?;
        let request = HistoryRequest {
            key: HistoryKey {
                namespace: self.namespace.clone(),
                instance: self.instance,
                sequence,
            },
            scope: scope.clone(),
            representation: revision.representation().clone(),
            hash: revision.hash(),
        };
        while self.entries.len() >= self.policy.max_revisions
            || bytes > self.policy.max_total_bytes.saturating_sub(self.total_bytes)
        {
            self.evict_oldest();
        }
        self.entries.push_back(HistoricalRevision {
            name: name.into(),
            required_key_profile: required_key_profile.map(str::to_owned),
            revision,
            request: request.clone(),
            accounted_bytes: bytes,
        });
        self.total_bytes += bytes;
        self.issued = sequence;
        Ok(request)
    }

    /// Explicit capture against an acknowledged session state. Callback sees only &Document;
    /// historical materialization cannot rewind or mutate it. Stale requests do not run it.
    pub fn capture_session<S: SessionStore>(
        &mut self,
        session: &Session<S>,
        expected: &ObservedRevision,
        name: &str,
        required_key_profile: Option<&str>,
        materialize: impl FnOnce(
            &Document,
            RevisionScope,
        ) -> Result<ImmutableRevision, RepresentationError>,
    ) -> Result<HistoryRequest, HistoryError> {
        label(name)?;
        if let Some(profile) = required_key_profile {
            label(profile)?;
        }
        session
            .compare_revision(expected)
            .map_err(HistoryError::Session)?;
        let document = session.document();
        let id = document.identity();
        let scope = RevisionScope {
            document: id.document_id.clone(),
            epoch: id.epoch.clone(),
            schema: id.schema.clone(),
            module_profile: id.profile.clone(),
            observed_token: expected.to_bytes(),
        };
        let revision =
            materialize(document, scope.clone()).map_err(HistoryError::Representation)?;
        if revision.scope() != &scope {
            return Err(HistoryError::ScopeMismatch);
        }
        self.capture(name, revision, required_key_profile)
    }

    pub fn read<'a>(
        &'a self,
        request: &HistoryRequest,
        available_key_profile: Option<&str>,
    ) -> Result<HistoryAvailability<&'a HistoricalRevision>, HistoryError> {
        if request.key.namespace != self.namespace
            || request.key.instance != self.instance
            || request.key.sequence == 0
            || request.key.sequence > self.issued
        {
            return Ok(HistoryAvailability::Unavailable(UnavailableReason::Missing));
        }
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.request.key == request.key)
        else {
            return Ok(HistoryAvailability::Unavailable(UnavailableReason::Pruned));
        };
        if entry.required_key_profile.is_some()
            && entry.required_key_profile.as_deref() != available_key_profile
        {
            return Ok(HistoryAvailability::Unavailable(
                UnavailableReason::KeyUnavailable,
            ));
        }
        let stored = &entry.request;
        if stored.scope.schema != request.scope.schema
            || stored.scope.module_profile != request.scope.module_profile
        {
            return Ok(HistoryAvailability::Unavailable(
                UnavailableReason::IncompatibleSchema,
            ));
        }
        if stored.scope != request.scope {
            return Err(HistoryError::ScopeMismatch);
        }
        if stored.representation != request.representation {
            return Err(HistoryError::RepresentationMismatch);
        }
        if stored.hash != request.hash {
            return Err(HistoryError::HashMismatch);
        }
        Ok(HistoryAvailability::Available(entry))
    }

    /// Tightening retention immediately prunes oldest captures, without rewriting them.
    pub fn set_retention(&mut self, policy: RetentionPolicy) {
        self.policy = policy;
        while !self.policy.permits(self.len(), self.total_bytes) {
            self.evict_oldest();
        }
    }
    fn evict_oldest(&mut self) {
        if let Some(entry) = self.entries.pop_front() {
            self.total_bytes -= entry.accounted_bytes;
        }
    }
}
fn label(value: &str) -> Result<(), HistoryError> {
    if value.is_empty() || value.len() > 256 {
        Err(HistoryError::InvalidLabel)
    } else {
        Ok(())
    }
}
