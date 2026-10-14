//! Endpoint-local observed revisions, not global locks or remote durability receipts.
//!
//! A complete integrated Yrs state is identified by immutable struct clocks plus the
//! complete deletion set, within one immutable document lineage. Hash both in canonical
//! order: state vectors alone miss deletions, and raw update bytes have unordered maps.
//! Checkpoint/journal replay persists both components; fresh writer IDs, checkpoint
//! publication generations and outbox receipts do not change the observed revision.
use editac_core::document::{Document, DocumentError, MAX_RESTORE_BYTES};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Versioned opaque bytes suitable for a host's prepared proposal or persisted receipt.
/// Tokens are equality validators, never credentials, globally ordered clocks or signatures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedRevision {
    scope: [u8; 32],
    state: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevisionError {
    InvalidToken,
    InvalidObservedState,
    ResourceLimit,
    Document(DocumentError),
    ScopeMismatch,
    Stale,
}
impl std::fmt::Display for RevisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for RevisionError {}

impl ObservedRevision {
    pub const ENCODED_LEN: usize = 65;

    pub fn to_bytes(&self) -> [u8; Self::ENCODED_LEN] {
        let mut bytes = [0; Self::ENCODED_LEN];
        bytes[0] = 1;
        bytes[1..33].copy_from_slice(&self.scope);
        bytes[33..].copy_from_slice(&self.state);
        bytes
    }

    /// Strict fixed-length/version framing. Comparison still requires the owning session;
    /// parsing an untrusted token does not authorize a mutation or verify an author.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RevisionError> {
        if bytes.len() != Self::ENCODED_LEN || bytes[0] != 1 {
            return Err(RevisionError::InvalidToken);
        }
        let mut scope = [0; 32];
        let mut state = [0; 32];
        scope.copy_from_slice(&bytes[1..33]);
        state.copy_from_slice(&bytes[33..]);
        Ok(Self { scope, state })
    }

    pub(crate) fn observe(document: &Document) -> Result<Self, RevisionError> {
        let vector = document.state_vector_v1();
        // Diff against these exact local clocks: no structs are missing, but the engine
        // emits its full delete set even when there have been deletion-only changes.
        // Only engine-generated v1 buffers enter this bounded decoder, never peer bytes.
        let deletions = document
            .update_v1(&vector)
            .map_err(RevisionError::Document)?;
        let mut scope = Sha256::new();
        scope.update(b"editac-observed-revision-scope-v1");
        crate::ports::hash_scope(&mut scope, document.identity());
        Ok(Self {
            scope: scope.finalize().into(),
            state: state_digest(&vector, &deletions)?,
        })
    }

    pub(crate) fn compare(&self, current: &Self) -> Result<(), RevisionError> {
        if self.scope != current.scope {
            Err(RevisionError::ScopeMismatch)
        } else if self.state != current.state {
            Err(RevisionError::Stale)
        } else {
            Ok(())
        }
    }
}

fn state_digest(vector: &[u8], deletions: &[u8]) -> Result<[u8; 32], RevisionError> {
    let mut reader = Reader::new(vector)?;
    let mut clocks = BTreeMap::new();
    let count = reader.count(2)?;
    for _ in 0..count {
        let client = reader.var()?;
        let clock = reader.clock()?;
        if clock == 0 || clocks.insert(client, clock).is_some() {
            return Err(RevisionError::InvalidObservedState);
        }
    }
    reader.finish()?;
    let mut reader = Reader::new(deletions)?;
    if reader.var()? != 0 {
        // This must be a diff from the same immutable local vector, with zero structs.
        return Err(RevisionError::InvalidObservedState);
    }
    let count = reader.count(2)?;
    let mut deletes = BTreeMap::new();
    for _ in 0..count {
        let client = reader.var()?;
        let count = reader.count(2)?;
        let mut ranges = Vec::new();
        for _ in 0..count {
            let start = reader.clock()?;
            let len = reader.clock()?;
            let end = start
                .checked_add(len)
                .ok_or(RevisionError::InvalidObservedState)?;
            if len == 0
                || end
                    > *clocks
                        .get(&client)
                        .ok_or(RevisionError::InvalidObservedState)?
            {
                return Err(RevisionError::InvalidObservedState);
            }
            ranges.push((start, end));
        }
        if ranges.is_empty() || deletes.contains_key(&client) {
            return Err(RevisionError::InvalidObservedState);
        }
        ranges.sort_unstable();
        // Canonical set representation, independent of range fragmentation or ordering.
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for (start, end) in ranges {
            if let Some(last) = merged.last_mut().filter(|last| start <= last.1) {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        deletes.insert(client, merged);
    }
    reader.finish()?;
    let mut hash = Sha256::new();
    hash.update(b"editac-observed-revision-state-v1");
    hash.update((clocks.len() as u64).to_be_bytes());
    for (client, clock) in clocks {
        hash.update(client.to_be_bytes());
        hash.update(clock.to_be_bytes());
    }
    hash.update((deletes.len() as u64).to_be_bytes());
    for (client, ranges) in deletes {
        hash.update(client.to_be_bytes());
        hash.update((ranges.len() as u64).to_be_bytes());
        for (start, end) in ranges {
            hash.update(start.to_be_bytes());
            hash.update(end.to_be_bytes());
        }
    }
    Ok(hash.finalize().into())
}

/// The public engine's v1 vector and delete-only diff use unsigned base-128 varints.
/// No decoding/allocation of semantic structs, Any values or arbitrary peer updates.
struct Reader<'a> {
    bytes: &'a [u8],
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self, RevisionError> {
        if bytes.len() > MAX_RESTORE_BYTES {
            return Err(RevisionError::ResourceLimit);
        }
        Ok(Self { bytes })
    }
    fn var(&mut self) -> Result<u64, RevisionError> {
        let mut value = 0;
        for shift in (0..70).step_by(7) {
            let (&byte, rest) = self
                .bytes
                .split_first()
                .ok_or(RevisionError::InvalidObservedState)?;
            self.bytes = rest;
            if shift == 63 && byte > 1 {
                return Err(RevisionError::InvalidObservedState);
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(RevisionError::InvalidObservedState)
    }
    fn clock(&mut self) -> Result<u32, RevisionError> {
        self.var()?
            .try_into()
            .map_err(|_| RevisionError::InvalidObservedState)
    }
    fn count(&mut self, min_bytes: usize) -> Result<usize, RevisionError> {
        let count: usize = self
            .var()?
            .try_into()
            .map_err(|_| RevisionError::InvalidObservedState)?;
        if count > self.bytes.len() / min_bytes {
            return Err(RevisionError::InvalidObservedState);
        }
        Ok(count)
    }
    fn finish(self) -> Result<(), RevisionError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(RevisionError::InvalidObservedState)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_clocks_and_delete_ranges_ignore_map_order_and_fragmentation() {
        let vector_a = [2, 1, 10, 2, 20];
        let vector_b = [2, 2, 20, 1, 10];
        let deletes_a = [0, 2, 1, 1, 2, 4, 2, 1, 10, 2];
        let deletes_b = [0, 2, 2, 1, 10, 2, 1, 2, 4, 2, 2, 2];
        assert_eq!(
            state_digest(&vector_a, &deletes_a),
            state_digest(&vector_b, &deletes_b)
        );
        assert!(state_digest(&vector_a, &deletes_a).is_ok());
        // Deleting a new range changes state even with identical clocks.
        assert_ne!(
            state_digest(&vector_a, &deletes_a),
            state_digest(&vector_a, &[0, 1, 1, 1, 2, 5])
        );
    }

    #[test]
    fn unexpected_structs_trailing_bytes_overflow_and_incomplete_sets_reject() {
        for deletes in [
            &[1][..],
            &[0, 0, 0],
            &[0, 1, 1, 1, 8, 3],
            &[0, 1, 1, 1, 0xff],
        ] {
            assert_eq!(
                state_digest(&[1, 1, 10], deletes),
                Err(RevisionError::InvalidObservedState)
            );
        }
        assert_eq!(
            state_digest(&[1, 1, 0xff, 0xff, 0xff, 0xff, 0x10], &[0, 0]),
            Err(RevisionError::InvalidObservedState)
        );
    }
}
