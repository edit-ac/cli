#![allow(dead_code)]
#[path = "hosted.rs"]
mod hosted;
use editac_cli::{
    engine::{
        Utf16Offset,
        commands::{editor::EditorCommand, text::TextOrigin},
        document::{Document, ReplayUpdate},
    },
    hosted::HostedSession,
    storage::{DurableReceipt, NativeStorage, RECEIPT_PROTOCOL, StoredIdentity},
    sync::{self, Backoff, Delivery, Negotiation, RetryPolicy, Transport, TransportError},
};

use std::{collections::VecDeque, path::PathBuf, time::Duration};
pub struct Harness {
    pub host: hosted::Harness,
    pub session: HostedSession,
    pub transport: Service,
    pub clock: Clock,
}
impl Harness {
    pub fn new() -> Self {
        let mut host = hosted::Harness::with_source("alpha\n\nbeta\n");
        let opened = host.open(false).unwrap();
        drop(opened);
        let session = host.open(true).unwrap();
        let peer = Document::restore(&host.identity, &host.service.checkpoint, &[]).unwrap();
        let transport = Service {
            peer,
            root: host.cache(),
            connect_failures: VecDeque::new(),
            upload_failures: VecDeque::new(),
            receipt_mode: ReceiptMode::Exact,
            connects: 0,
            disconnects: 0,
            uploads: vec![],
            exchanges: 0,
            handshake: false,
            negotiation_identity: None,
            capability: Some(RECEIPT_PROTOCOL.into()),
            extra_frames: vec![],
            later_frames: VecDeque::new(),
            receipt_override: None,
        };
        Self {
            host,
            session,
            transport,
            clock: Clock::default(),
        }
    }
    pub fn edit(&mut self, source: &str) -> ReplayUpdate {
        let baseline = self.session.source_baseline().unwrap();
        let plan = self.session.prepare_source(&baseline, source).unwrap();
        let commit = plan.commit(&mut self.session.recovery.document).unwrap();
        commit.commit.unwrap().update
    }
    pub fn local(&mut self, source: &str) {
        let update = self.edit(source);
        sync::persist_local(&mut self.session, &update).unwrap();
    }
    pub fn run(&mut self) -> sync::SyncReport {
        sync::synchronize_with(
            &mut self.session,
            &self.host.config,
            &mut self.host.credentials,
            &mut self.transport,
            RetryPolicy::default(),
            &editac_cli::storage::NativeIo,
            &mut self.clock,
        )
        .unwrap()
    }
    pub fn reopened(&mut self) -> HostedSession {
        self.host.open(true).unwrap()
    }
}
#[derive(Default)]
pub struct Clock(pub Vec<Duration>);
impl Backoff for Clock {
    fn wait(&mut self, duration: Duration) {
        self.0.push(duration);
    }
}
#[derive(Clone, Copy)]
pub enum ReceiptMode {
    Exact,
    Missing,
    WrongDocument,
    WrongEpoch,
    WrongSchema,
    WrongWriter,
    WrongSequence,
    WrongContent,
    WrongId,
    WrongProtocol,
    WrongState,
}
pub struct Service {
    pub peer: Document,
    pub root: PathBuf,
    pub connect_failures: VecDeque<TransportError>,
    pub upload_failures: VecDeque<Option<TransportError>>,
    pub receipt_mode: ReceiptMode,
    pub connects: usize,
    pub disconnects: usize,
    pub uploads: Vec<editac_cli::storage::JournalEntry>,
    pub exchanges: usize,
    pub handshake: bool,
    pub negotiation_identity: Option<StoredIdentity>,
    pub capability: Option<String>,
    pub extra_frames: Vec<Vec<u8>>,
    pub later_frames: VecDeque<Vec<Vec<u8>>>,
    pub receipt_override: Option<DurableReceipt>,
}
pub fn receipt(entry: &editac_cli::storage::JournalEntry) -> DurableReceipt {
    DurableReceipt {
        protocol: RECEIPT_PROTOCOL.into(),
        state: "durable".into(),
        identity: entry.identity.clone(),
        writer_id: entry.writer_id,
        sequence: entry.sequence,
        checksum: entry.checksum.clone(),
        update_id: entry.update_id.clone(),
    }
}
impl Transport for Service {
    fn connect(
        &mut self,
        _: &str,
        identity: &StoredIdentity,
        credential: &editac_cli::auth::Credential,
    ) -> Result<Negotiation, TransportError> {
        self.connects += 1;
        self.handshake = false;
        assert_eq!(credential.expose_to_adapter(), hosted::TOKEN);
        // Transport observes durable recovery from a separately opened native reader.
        let storage = NativeStorage::open(&self.root, self.peer.identity().clone()).unwrap();
        assert!(storage.recover().is_ok());
        if let Some(e) = self.connect_failures.pop_front() {
            return Err(e);
        }
        Ok(Negotiation {
            identity: self
                .negotiation_identity
                .clone()
                .unwrap_or_else(|| identity.clone()),
            receipt_protocol: self.capability.clone(),
        })
    }
    fn exchange(&mut self, frames: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, TransportError> {
        self.exchanges += 1;
        let mut result = vec![];
        for bytes in frames {
            let (kind, payload) = sync::decode_frame(bytes)?;
            if kind == 0 {
                result.push(sync::frame(1, &self.peer.update_v1(payload).unwrap())?);
                if !self.handshake {
                    result.push(sync::frame(0, &self.peer.state_vector_v1())?);
                    self.handshake = true;
                }
            } else {
                self.peer
                    .apply_replay(&ReplayUpdate {
                        identity: self.peer.identity().clone(),
                        bytes: payload.to_vec(),
                    })
                    .unwrap();
            }
        }
        result.append(&mut self.extra_frames);
        if let Some(mut batch) = self.later_frames.pop_front() {
            result.append(&mut batch);
        }
        Ok(result)
    }
    fn upload(
        &mut self,
        entry: &editac_cli::storage::JournalEntry,
    ) -> Result<Delivery, TransportError> {
        self.uploads.push(entry.clone());
        if let Some(Some(e)) = self.upload_failures.pop_front() {
            return Err(e);
        }
        self.peer
            .apply_replay(&ReplayUpdate {
                identity: self.peer.identity().clone(),
                bytes: entry.bytes.clone(),
            })
            .unwrap();
        let mut r = self
            .receipt_override
            .clone()
            .unwrap_or_else(|| receipt(entry));
        match self.receipt_mode {
            ReceiptMode::Missing => return Ok(Delivery { receipt: None }),
            ReceiptMode::WrongDocument => {
                r.identity.document_id = "ac000000-0000-4000-8000-ffffffffffff".into()
            }
            ReceiptMode::WrongEpoch => {
                r.identity.epoch = "ac000000-0000-4000-8000-ffffffffffff".into()
            }
            ReceiptMode::WrongSchema => r.identity.schema = "other".into(),
            ReceiptMode::WrongWriter => r.writer_id += 1,
            ReceiptMode::WrongSequence => r.sequence += 1,
            ReceiptMode::WrongContent => r.checksum = "0".repeat(64),
            ReceiptMode::WrongId => r.update_id = "unknown".into(),
            ReceiptMode::WrongProtocol => r.protocol = "future".into(),
            ReceiptMode::WrongState => r.state = "delivered".into(),
            ReceiptMode::Exact => {}
        }
        Ok(Delivery { receipt: Some(r) })
    }
    fn disconnect(&mut self) {
        self.disconnects += 1;
    }
}
pub fn peer_edit(peer: &mut Document, text: &str) -> ReplayUpdate {
    let owner = peer
        .snapshot()
        .unwrap()
        .visible
        .iter()
        .find(|n| n.kind == "block")
        .unwrap()
        .id
        .clone();
    let plan = peer
        .prepare_editor(
            &[EditorCommand::ReplaceAt {
                owner,
                from: Utf16Offset(0),
                to: Utf16Offset(0),
                text: text.into(),
            }],
            &TextOrigin::new("prepared-peer").unwrap(),
        )
        .unwrap();
    peer.commit_editor(plan).unwrap().update
}
