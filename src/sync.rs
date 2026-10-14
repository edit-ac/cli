//! Persistence-first native adapter for the qualified y-protocols baseline v1
//! and editac-durable-receipt-v1 application port. No daemon or provider mirror.
use crate::{
    auth::{Credential, CredentialStore},
    engine::document::ReplayUpdate,
    hosted::{HostedSession, ServiceConfig},
    status::{self, ConnectionState, Status},
    storage::{DurableReceipt, IoHooks, JournalEntry, NativeIo, RECEIPT_PROTOCOL, StoredIdentity},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, time::Duration};

pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_FRAMES: usize = 32;
pub const MAX_UPLOADS: usize = 32;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportError {
    Unavailable,
    Authorization,
    Revoked,
    Incompatible,
    InvalidProtocol,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Negotiation {
    pub identity: StoredIdentity,
    pub receipt_protocol: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Delivery {
    pub receipt: Option<DurableReceipt>,
}
/// Host owns authenticated connection, timeouts, revocation and server wire I/O.
/// This port uses already-qualified baseline frames and exact application envelopes.
pub trait Transport {
    fn connect(
        &mut self,
        endpoint: &str,
        identity: &StoredIdentity,
        credential: &Credential,
    ) -> Result<Negotiation, TransportError>;
    fn exchange(&mut self, frames: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, TransportError>;
    fn upload(&mut self, entry: &JournalEntry) -> Result<Delivery, TransportError>;
    fn disconnect(&mut self);
}
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub attempts: u8,
    pub initial_backoff_ms: u64,
}
impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            initial_backoff_ms: 50,
        }
    }
}
pub trait Backoff {
    fn wait(&mut self, duration: Duration);
}
pub struct NativeBackoff;
impl Backoff for NativeBackoff {
    fn wait(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    pub status: Status,
    pub attempts: u8,
    pub ignored_receipts: usize,
    pub received_updates: usize,
}

fn invalid() -> TransportError {
    TransportError::InvalidProtocol
}
fn uint(bytes: &[u8], cursor: &mut usize) -> Result<u32, TransportError> {
    let mut result = 0u64;
    for i in 0..5 {
        let byte = *bytes.get(*cursor).ok_or_else(invalid)?;
        *cursor += 1;
        result += u64::from(byte & 127) << (7 * i);
        if result > u32::MAX as u64 {
            return Err(invalid());
        }
        if byte < 128 {
            if i > 0 && byte == 0 {
                return Err(invalid());
            }
            return Ok(result as u32);
        }
    }
    Err(invalid())
}
fn write_uint(mut value: u32, bytes: &mut Vec<u8>) {
    loop {
        let digit = (value & 127) as u8;
        value >>= 7;
        bytes.push(digit | if value > 0 { 128 } else { 0 });
        if value == 0 {
            break;
        }
    }
}
fn vector(bytes: &[u8]) -> Result<(), TransportError> {
    if bytes.len() > 64 * 1024 {
        return Err(invalid());
    }
    let mut cursor = 0;
    let count = uint(bytes, &mut cursor)?;
    if count > 4096 {
        return Err(invalid());
    }
    let mut seen = HashSet::new();
    for _ in 0..count {
        if !seen.insert(uint(bytes, &mut cursor)?) {
            return Err(invalid());
        }
        uint(bytes, &mut cursor)?;
    }
    if cursor != bytes.len() {
        return Err(invalid());
    }
    Ok(())
}
/// Canonical y-protocols 1.0.7 baseline message, with no provider/room envelope.
pub fn frame(kind: u32, payload: &[u8]) -> Result<Vec<u8>, TransportError> {
    if kind > 2 || payload.is_empty() || payload.len() > MAX_FRAME_BYTES - 6 {
        return Err(invalid());
    }
    let mut result = Vec::with_capacity(payload.len() + 6);
    write_uint(kind, &mut result);
    write_uint(payload.len() as u32, &mut result);
    result.extend_from_slice(payload);
    Ok(result)
}
pub fn decode_frame(bytes: &[u8]) -> Result<(u32, &[u8]), TransportError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(invalid());
    }
    let mut cursor = 0;
    let kind = uint(bytes, &mut cursor)?;
    let len = uint(bytes, &mut cursor)? as usize;
    if kind > 2 || len == 0 || len != bytes.len() - cursor {
        return Err(invalid());
    }
    let payload = &bytes[cursor..];
    if kind == 0 {
        vector(payload)?;
    }
    Ok((kind, payload))
}
fn bounded(frames: &[Vec<u8>]) -> Result<(), TransportError> {
    if frames.len() > MAX_FRAMES || frames.iter().map(Vec::len).sum::<usize>() > MAX_FRAME_BYTES {
        return Err(invalid());
    }
    for f in frames {
        decode_frame(f)?;
    }
    Ok(())
}
/// Commit already-prepared engine bytes at the native durability boundary before
/// signaling transport. A failed write keeps the live replica and draft exportable.
pub fn persist_local(
    session: &mut HostedSession,
    update: &ReplayUpdate,
) -> Result<(), crate::storage::StorageError> {
    persist_local_with(session, update, &NativeIo)
}
pub fn persist_local_with(
    session: &mut HostedSession,
    update: &ReplayUpdate,
    hooks: &dyn IoHooks,
) -> Result<(), crate::storage::StorageError> {
    persist(session, update, "editac:local:source", hooks)
}
fn persist(
    session: &mut HostedSession,
    update: &ReplayUpdate,
    origin: &str,
    hooks: &dyn IoHooks,
) -> Result<(), crate::storage::StorageError> {
    let sequence =
        session
            .storage
            .sequence_for_update(&session.recovery.document, update, origin)?;
    session.storage.persist_update_with(
        &session.recovery.document,
        update,
        sequence,
        origin,
        hooks,
    )?;
    Ok(())
}
#[derive(Debug)]
enum AttemptError {
    Transport(TransportError),
    Storage,
    ReceiptStore,
}
impl From<TransportError> for AttemptError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e)
    }
}
/// Retry only the engine's atomic missing-dependency rejection. Other admission
/// failures end the attachment. The queue is bounded across all exchange rounds.
fn receive(
    session: &mut HostedSession,
    incoming: &[Vec<u8>],
    waiting: &mut Vec<ReplayUpdate>,
    report: &mut SyncReport,
    hooks: &dyn IoHooks,
) -> Result<Vec<Vec<u8>>, AttemptError> {
    bounded(incoming)?;
    let mut vectors = Vec::new();
    for bytes in incoming {
        let (kind, payload) = decode_frame(bytes)?;
        if kind == 0 {
            vectors.push(payload);
        } else {
            waiting.push(ReplayUpdate {
                identity: session.recovery.document.identity().clone(),
                bytes: payload.to_vec(),
            });
        }
    }
    if waiting.len() > MAX_FRAMES
        || waiting.iter().map(|u| u.bytes.len()).sum::<usize>() > MAX_FRAME_BYTES
    {
        return Err(invalid().into());
    }
    loop {
        let mut progress = false;
        let mut index = 0;
        while index < waiting.len() {
            let update = &waiting[index];
            match session.apply_received(update) {
                Ok(()) => {
                    // Even a duplicate must cross persistence again after an
                    // uncertain prior publication; state equality is insufficient.
                    if !session
                        .storage
                        .contains_state(&session.recovery.document)
                        .map_err(|_| AttemptError::Storage)?
                    {
                        persist(session, update, "editac:remote:sync", hooks)
                            .map_err(|_| AttemptError::Storage)?;
                        report.received_updates += 1;
                    }
                    waiting.remove(index);
                    progress = true;
                }
                Err(crate::engine::document::DocumentError::IncompleteRestore) => index += 1,
                Err(_) => return Err(invalid().into()),
            }
        }
        if !progress || waiting.is_empty() {
            break;
        }
    }
    // Replies use the state after all currently admissible receives persisted.
    vectors
        .into_iter()
        .map(|vector| {
            let bytes = session
                .recovery
                .document
                .update_v1(vector)
                .map_err(|_| invalid())?;
            Ok(frame(1, &bytes)?)
        })
        .collect()
}
fn attempt(
    session: &mut HostedSession,
    config: &ServiceConfig,
    credential: &Credential,
    transport: &mut dyn Transport,
    report: &mut SyncReport,
    hooks: &dyn IoHooks,
) -> Result<(), AttemptError> {
    if !session
        .storage
        .durability_barrier(&session.recovery.document, hooks)
        .map_err(|_| AttemptError::Storage)?
    {
        return Err(AttemptError::Storage);
    }
    let identity = StoredIdentity::from(session.recovery.document.identity());
    let negotiation = transport.connect(config.endpoint(), &identity, credential)?;
    if negotiation.identity != identity {
        return Err(TransportError::Incompatible.into());
    }
    let receipts = match negotiation.receipt_protocol.as_deref() {
        None => false,
        Some(RECEIPT_PROTOCOL) => true,
        _ => return Err(invalid().into()),
    };
    let mut outgoing = vec![frame(0, &session.recovery.document.state_vector_v1())?];
    let mut waiting = Vec::new();
    for round in 0..MAX_FRAMES {
        bounded(&outgoing)?;
        let incoming = transport.exchange(&outgoing)?;
        outgoing = receive(session, &incoming, &mut waiting, report, hooks)?;
        if outgoing.is_empty() && waiting.is_empty() {
            break;
        }
        if round + 1 == MAX_FRAMES {
            return Err(invalid().into());
        }
    }
    let pending = session
        .storage
        .recover()
        .map_err(|_| AttemptError::Storage)?
        .pending;
    let mut bytes = 0;
    for entry in pending
        .iter()
        .filter(|e| status::uploadable(e))
        .take(MAX_UPLOADS)
    {
        bytes += entry.bytes.len();
        if bytes > MAX_FRAME_BYTES {
            break;
        }
        if receipts {
            let delivery = transport.upload(entry)?;
            if let Some(receipt) = delivery.receipt {
                if !receipt.matches(entry)
                    || !session
                        .storage
                        .accept_receipt_with(&receipt, hooks)
                        .map_err(|_| AttemptError::ReceiptStore)?
                {
                    report.ignored_receipts += 1;
                }
            }
        } else {
            let incoming = transport.exchange(&[frame(2, &entry.bytes)?])?;
            // No-receipt delivery cannot acknowledge uploads. Process its
            // piggybacked baseline replies through the same durability boundary.
            let mut replies = receive(session, &incoming, &mut waiting, report, hooks)?;
            for round in 0..MAX_FRAMES {
                if replies.is_empty() && waiting.is_empty() {
                    break;
                }
                bounded(&replies)?;
                let incoming = transport.exchange(&replies)?;
                replies = receive(session, &incoming, &mut waiting, report, hooks)?;
                if round + 1 == MAX_FRAMES && (!replies.is_empty() || !waiting.is_empty()) {
                    return Err(invalid().into());
                }
            }
        }
    }
    Ok(())
}
pub fn synchronize(
    session: &mut HostedSession,
    config: &ServiceConfig,
    credentials: &mut dyn CredentialStore,
    transport: &mut dyn Transport,
    policy: RetryPolicy,
) -> Result<SyncReport, &'static str> {
    synchronize_with(
        session,
        config,
        credentials,
        transport,
        policy,
        &NativeIo,
        &mut NativeBackoff,
    )
}
pub fn synchronize_with(
    session: &mut HostedSession,
    config: &ServiceConfig,
    credentials: &mut dyn CredentialStore,
    transport: &mut dyn Transport,
    policy: RetryPolicy,
    hooks: &dyn IoHooks,
    backoff: &mut dyn Backoff,
) -> Result<SyncReport, &'static str> {
    if !(1..=5).contains(&policy.attempts) || policy.initial_backoff_ms > 500 {
        return Err("invalid retry bounds");
    }
    if config.identity() != session.recovery.document.identity() {
        return Err("configured identity mismatch");
    }
    let mut report = SyncReport {
        status: status::inspect(session)
            .map_err(|_| "native recovery unavailable; retain cache")?,
        attempts: 0,
        ignored_receipts: 0,
        received_updates: 0,
    };
    // No credentials or connection are touched until local persistence is proven.
    if !report.status.locally_saved {
        report.status.connection = ConnectionState::LocalPersistenceFailure;
        return Ok(report);
    }
    let credential = match credentials.load(config.endpoint()) {
        Ok(Some(c)) => c,
        _ => {
            report.status.connection = ConnectionState::AuthorizationFailure;
            return Ok(report);
        }
    };
    let mut state = ConnectionState::Connected;
    for index in 0..policy.attempts {
        report.attempts += 1;
        let result = attempt(session, config, &credential, transport, &mut report, hooks);
        transport.disconnect();
        match result {
            Ok(()) => break,
            Err(AttemptError::Storage) => {
                state = ConnectionState::LocalPersistenceFailure;
                break;
            }
            Err(AttemptError::ReceiptStore) => {
                state = ConnectionState::ReceiptPersistenceFailure;
                break;
            }
            Err(AttemptError::Transport(TransportError::Unavailable)) => {
                state = ConnectionState::RetryExhausted;
                if index + 1 < policy.attempts {
                    backoff.wait(Duration::from_millis(
                        (policy.initial_backoff_ms * (1 << index)).min(2000),
                    ));
                    state = ConnectionState::Connected;
                }
            }
            Err(AttemptError::Transport(error)) => {
                state = match error {
                    TransportError::Authorization => ConnectionState::AuthorizationFailure,
                    TransportError::Revoked => ConnectionState::Revoked,
                    TransportError::Incompatible => ConnectionState::IncompatibleSchemaEpoch,
                    _ => ConnectionState::InvalidProtocol,
                };
                break;
            }
        }
    }
    report.status =
        status::inspect(session).map_err(|_| "native recovery unavailable; retain cache")?;
    report.status.connection = state;
    if state == ConnectionState::LocalPersistenceFailure {
        report.status.locally_saved = false;
    }
    session.authorized_online = state == ConnectionState::Connected;
    Ok(report)
}
