//! CLI-1.9 canonical native acquisition/restore/transport/outbox integration.
#[path = "support/sync.rs"]
mod harness;
use editac_cli::{
    engine::document::Document,
    status::{self, ConnectionState},
    storage::{Boundary, IoHooks, RecordKind},
    sync::{self, RetryPolicy, TransportError},
};
use harness::{Harness, ReceiptMode, peer_edit, receipt};
use std::{fs, io};

#[test]
fn persistence_precedes_credentials_and_transport_attachment() {
    let mut h = Harness::new();
    let _unsaved = h.edit("changed\n\nbeta\n");
    let credentials = h.host.credentials.calls;
    let result = h.run();
    assert!(!result.status.locally_saved);
    assert_eq!(
        result.status.connection,
        ConnectionState::LocalPersistenceFailure
    );
    assert_eq!(h.transport.connects, 0);
    assert_eq!(h.transport.exchanges, 0);
    assert_eq!(h.host.credentials.calls, credentials);
    assert!(
        h.session
            .source_baseline()
            .unwrap()
            .opening()
            .source
            .contains("changed")
    );
    assert!(h.session.storage.read_draft().unwrap().is_some());
}
#[test]
fn durable_receipts_prune_pending_only_and_survive_restart() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    let pending = h.session.storage.recover().unwrap().pending;
    let result = h.run();
    assert_eq!(result.status.pending_upload, 0);
    assert_eq!(result.status.remote_durability, "remotely-committed");
    assert!(result.status.locally_saved);
    assert_eq!(h.transport.uploads, pending);
    let reopened = h.reopened();
    assert!(reopened.recovery.pending.is_empty());
    assert_eq!(reopened.recovery.tail, pending);
    assert_eq!(
        status::inspect(&reopened).unwrap().remote_committed_updates,
        1
    );
    assert!(
        reopened
            .source_baseline()
            .unwrap()
            .opening()
            .source
            .contains("changed")
    );
    assert_eq!(h.transport.disconnects, 1);
}
#[test]
fn disconnect_reconnect_retries_exact_bytes_and_identity() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    let pending = h.session.storage.recover().unwrap().pending;
    h.transport
        .upload_failures
        .extend([Some(TransportError::Unavailable), None]);
    let result = h.run();
    assert_eq!(result.attempts, 2);
    assert_eq!(result.status.pending_upload, 0);
    assert_eq!(
        h.transport.uploads,
        vec![pending[0].clone(), pending[0].clone()]
    );
    assert_eq!(
        h.clock.0.iter().map(|d| d.as_millis()).collect::<Vec<_>>(),
        vec![50]
    );
    assert_eq!(h.transport.disconnects, 2);
}
#[test]
fn retry_exhaustion_is_bounded_and_retains_all_pending_work() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    let pending = h.session.storage.recover().unwrap().pending;
    h.transport
        .connect_failures
        .extend([TransportError::Unavailable; 3]);
    let result = h.run();
    assert_eq!(result.attempts, 3);
    assert_eq!(result.status.connection, ConnectionState::RetryExhausted);
    assert_eq!(
        h.clock.0.iter().map(|d| d.as_millis()).collect::<Vec<_>>(),
        vec![50, 100]
    );
    assert_eq!(h.reopened().recovery.pending, pending);
    assert_eq!(h.transport.disconnects, 3);
}
macro_rules! wrong_receipt {
    ($name:ident,$mode:ident) => {
        #[test]
        fn $name() {
            let mut h = Harness::new();
            h.local("changed\n\nbeta\n");
            let pending = h.session.storage.recover().unwrap().pending;
            h.transport.receipt_mode = ReceiptMode::$mode;
            let result = h.run();
            assert_eq!(result.status.pending_upload, 1);
            assert_eq!(result.status.remote_durability, "unknown");
            assert_eq!(result.ignored_receipts, 1);
            assert_eq!(h.reopened().recovery.pending, pending);
            assert_eq!(result.status.remote_committed_updates, 0);
        }
    };
}
wrong_receipt!(wrong_document_receipt, WrongDocument);
wrong_receipt!(wrong_epoch_receipt, WrongEpoch);
wrong_receipt!(wrong_schema_receipt, WrongSchema);
wrong_receipt!(wrong_writer_receipt, WrongWriter);
wrong_receipt!(wrong_sequence_receipt, WrongSequence);
wrong_receipt!(wrong_content_receipt, WrongContent);
wrong_receipt!(wrong_update_id_receipt, WrongId);
wrong_receipt!(wrong_protocol_receipt, WrongProtocol);
wrong_receipt!(delivery_is_not_a_durable_receipt, WrongState);
#[test]
fn missing_receipt_preserves_pending_after_matching_state_vectors() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    h.transport.receipt_mode = ReceiptMode::Missing;
    let result = h.run();
    assert_eq!(result.status.pending_upload, 1);
    assert_eq!(result.status.remote_durability, "unknown");
    assert_eq!(result.ignored_receipts, 0);
    assert_eq!(
        h.session.recovery.document.state_vector_v1(),
        h.transport.peer.state_vector_v1()
    );
    assert_eq!(h.reopened().recovery.pending.len(), 1);
}
#[test]
fn service_without_receipts_syncs_but_never_reports_remote_saved() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    h.transport.capability = None;
    let result = h.run();
    assert_eq!(result.status.pending_upload, 1);
    assert_eq!(result.status.remote_durability, "unknown");
    assert!(h.transport.uploads.is_empty());
    assert_eq!(
        h.transport.peer.state_vector_v1(),
        h.session.recovery.document.state_vector_v1()
    );
    assert_eq!(h.reopened().recovery.pending.len(), 1);
}
#[test]
fn revocation_after_partial_ack_keeps_remaining_exact_outbox_and_draft() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    h.local("changed\n\nsecond\n");
    let pending = h.session.storage.recover().unwrap().pending;
    h.transport
        .upload_failures
        .extend([None, Some(TransportError::Revoked)]);
    let result = h.run();
    assert_eq!(result.status.connection, ConnectionState::Revoked);
    assert_eq!(result.attempts, 1);
    assert_eq!(h.reopened().recovery.pending, vec![pending[1].clone()]);
    assert_eq!(result.status.remote_committed_updates, 1);
    assert!(h.session.storage.read_draft().unwrap().is_some());
    assert!(
        h.session
            .source_baseline()
            .unwrap()
            .opening()
            .source
            .contains("second")
    );
    assert!(!h.session.authorized_online);
}
#[test]
fn denied_auth_and_schema_epoch_failures_preserve_exportability() {
    for error in [TransportError::Authorization, TransportError::Incompatible] {
        let mut h = Harness::new();
        h.local("changed\n\nbeta\n");
        let pending = h.session.storage.recover().unwrap().pending;
        h.transport.connect_failures.push_back(error);
        let result = h.run();
        assert_eq!(result.attempts, 1);
        assert!(result.status.locally_saved);
        assert_eq!(h.reopened().recovery.pending, pending);
        assert!(
            h.session
                .source_baseline()
                .unwrap()
                .opening()
                .source
                .contains("changed")
        );
        assert!(h.transport.uploads.is_empty());
    }
}
#[test]
fn full_peer_identity_and_receipt_capability_are_admitted_before_exchange() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    let mut identity = editac_cli::storage::StoredIdentity::from(&h.host.identity);
    identity.epoch = "other".into();
    h.transport.negotiation_identity = Some(identity);
    let result = h.run();
    assert_eq!(
        result.status.connection,
        ConnectionState::IncompatibleSchemaEpoch
    );
    assert_eq!(h.transport.exchanges, 0);
    h.transport.negotiation_identity = None;
    h.transport.capability = Some("future".into());
    let result = h.run();
    assert_eq!(result.status.connection, ConnectionState::InvalidProtocol);
    assert_eq!(h.transport.exchanges, 0);
    assert_eq!(h.reopened().recovery.pending.len(), 1);
}
#[test]
fn received_duplicate_reordered_independent_updates_persist_without_echo() {
    let mut h = Harness::new();
    h.local("alpha\n\nlocal beta\n");
    let checkpoint = h.host.service.checkpoint.clone();
    let mut a = Document::restore(&h.host.identity, &checkpoint, &[]).unwrap();
    let mut b = Document::restore(&h.host.identity, &checkpoint, &[]).unwrap();
    let ua = peer_edit(&mut a, "A");
    let ub = peer_edit(&mut b, "B");
    h.transport.extra_frames = vec![
        sync::frame(2, &ub.bytes).unwrap(),
        sync::frame(2, &ua.bytes).unwrap(),
        sync::frame(2, &ub.bytes).unwrap(),
    ];
    let result = h.run();
    assert_eq!(result.status.connection, ConnectionState::Connected);
    assert_eq!(result.status.pending_upload, 0);
    assert_eq!(h.transport.uploads.len(), 1);
    assert!(result.received_updates >= 2);
    let reopened = h.reopened();
    let source = reopened.source_baseline().unwrap().opening().source.clone();
    assert!(source.contains('A') && source.contains('B') && source.contains("local beta"));
    assert!(
        reopened
            .recovery
            .tail
            .iter()
            .filter(|e| e.origin == "editac:remote:sync")
            .count()
            >= 2
    );
}
struct Fail {
    kind: RecordKind,
    boundary: Boundary,
}
impl IoHooks for Fail {
    fn boundary(&self, kind: RecordKind, boundary: Boundary) -> io::Result<()> {
        if kind == self.kind && boundary == self.boundary {
            Err(io::Error::other("prepared interruption"))
        } else {
            Ok(())
        }
    }
}
#[test]
fn received_update_storage_failure_stops_sends_and_retains_live_export() {
    let mut h = Harness::new();
    h.local("alpha\n\nlocal beta\n");
    let update = peer_edit(&mut h.transport.peer, "remote ");
    h.transport.extra_frames = vec![sync::frame(2, &update.bytes).unwrap()];
    let result = sync::synchronize_with(
        &mut h.session,
        &h.host.config,
        &mut h.host.credentials,
        &mut h.transport,
        RetryPolicy::default(),
        &Fail {
            kind: RecordKind::Journal,
            boundary: Boundary::BeforeWrite,
        },
        &mut h.clock,
    )
    .unwrap();
    assert_eq!(
        result.status.connection,
        ConnectionState::LocalPersistenceFailure
    );
    assert!(!result.status.locally_saved);
    assert!(h.transport.uploads.is_empty());
    assert!(
        h.session
            .source_baseline()
            .unwrap()
            .opening()
            .source
            .contains("remote")
    );
    assert_eq!(h.reopened().recovery.pending.len(), 1);
}
#[test]
fn receipt_publication_failures_never_delete_recovery_bytes() {
    for boundary in [
        Boundary::BeforeWrite,
        Boundary::AfterWrite,
        Boundary::AfterFileSync,
        Boundary::BeforePublish,
        Boundary::AfterPublish,
        Boundary::AfterDirectorySync,
    ] {
        let mut h = Harness::new();
        h.local("changed\n\nbeta\n");
        let journal = h.session.storage.recover().unwrap().pending;
        let error = h
            .session
            .storage
            .accept_receipt_with(
                &receipt(&journal[0]),
                &Fail {
                    kind: RecordKind::Receipt,
                    boundary,
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("prepared interruption"));
        let recovered = h.reopened();
        assert_eq!(recovered.recovery.tail, journal);
        if matches!(
            boundary,
            Boundary::AfterPublish | Boundary::AfterDirectorySync
        ) {
            assert!(recovered.recovery.pending.is_empty());
        } else {
            assert_eq!(recovered.recovery.pending, journal);
        }
    }
}
#[test]
fn corrupt_receipt_marker_fails_safe_without_erasing_pending_or_drafts() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    h.run();
    fs::write(h.host.cache().join("receipts.json"), "corrupt").unwrap();
    assert!(h.session.storage.recover().is_err());
    assert!(h.session.storage.read_draft().unwrap().is_some());
    assert!(
        h.session
            .source_baseline()
            .unwrap()
            .opening()
            .source
            .contains("changed")
    );
    assert!(fs::read_dir(h.host.cache()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_str()
            .unwrap()
            .starts_with("journal-")
    }));
}
#[test]
fn duplicate_receipts_are_idempotent_even_after_checkpoint_fallback() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    let pending = h.session.storage.recover().unwrap().pending;
    let coverage = pending
        .iter()
        .map(|e| e.update_id.clone())
        .collect::<Vec<_>>();
    h.session
        .storage
        .publish_checkpoint(&h.session.recovery.document, &coverage)
        .unwrap();
    h.run();
    assert!(
        h.session
            .storage
            .accept_receipt(&receipt(&pending[0]))
            .unwrap()
    );
    fs::write(h.host.cache().join("latest.json"), "corrupt").unwrap();
    let recovered = h.reopened();
    assert_eq!(
        recovered.recovery.point,
        editac_cli::storage::RecoveryPoint::Previous
    );
    assert!(recovered.recovery.pending.is_empty());
    assert_eq!(
        status::inspect(&recovered)
            .unwrap()
            .remote_committed_updates,
        1
    );
}
#[test]
fn protocol_rejects_malformed_oversized_and_unbounded_frames_before_mutation() {
    for bytes in [
        vec![],
        vec![3, 1, 0],
        vec![0, 1, 128],
        vec![128, 0, 1, 0],
        vec![0, 2, 0],
        vec![0, 1, 0, 9],
    ] {
        assert!(sync::decode_frame(&bytes).is_err());
    }
    assert!(sync::frame(2, &vec![0; sync::MAX_FRAME_BYTES]).is_err());
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    h.transport.extra_frames = vec![vec![3, 1, 0]];
    let result = h.run();
    assert_eq!(result.status.connection, ConnectionState::InvalidProtocol);
    assert_eq!(h.reopened().recovery.pending.len(), 1);
    assert!(h.transport.uploads.is_empty());
}
#[test]
fn baseline_v1_layout_matches_pinned_y_protocols_vectors() {
    assert_eq!(sync::frame(0, &[0]).unwrap(), vec![0, 1, 0]);
    assert_eq!(sync::frame(0, &[1, 42, 7]).unwrap(), vec![0, 3, 1, 42, 7]);
    let payload = vec![0; 128];
    let framed = sync::frame(2, &payload).unwrap();
    assert_eq!(&framed[..3], &[2, 128, 1]);
    assert_eq!(
        sync::decode_frame(&framed).unwrap(),
        (2, payload.as_slice())
    );
}

#[test]
fn receipt_storage_failure_preserves_truthful_local_saved_status() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    let result = sync::synchronize_with(
        &mut h.session,
        &h.host.config,
        &mut h.host.credentials,
        &mut h.transport,
        RetryPolicy::default(),
        &Fail {
            kind: RecordKind::Receipt,
            boundary: Boundary::BeforePublish,
        },
        &mut h.clock,
    )
    .unwrap();
    assert_eq!(
        result.status.connection,
        ConnectionState::ReceiptPersistenceFailure
    );
    assert!(result.status.locally_saved);
    assert_eq!(result.status.pending_upload, 1);
    assert_eq!(result.status.remote_durability, "unknown");
}
#[test]
fn absent_credentials_do_not_attach_and_keep_recovery_export() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    h.host.credentials.absent = true;
    let result = h.run();
    assert_eq!(
        result.status.connection,
        ConnectionState::AuthorizationFailure
    );
    assert_eq!(result.attempts, 0);
    assert_eq!(h.transport.connects, 0);
    assert_eq!(h.reopened().recovery.pending.len(), 1);
    assert!(result.status.locally_saved);
}
#[test]
fn replay_page_bound_leaves_later_work_pending_until_next_explicit_sync() {
    let mut h = Harness::new();
    for i in 0..33 {
        h.local(&format!("change {i}\n\nbeta\n"));
    }
    let result = h.run();
    assert_eq!(h.transport.uploads.len(), 32);
    assert_eq!(result.status.pending_upload, 1);
    assert_eq!(result.status.remote_durability, "unknown");
    let result = h.run();
    assert_eq!(h.transport.uploads.len(), 33);
    assert_eq!(result.status.pending_upload, 0);
    assert_eq!(result.status.remote_committed_updates, 33);
}
#[test]
fn native_frames_and_receipts_export_actual_oracle_inputs() {
    let mut h = Harness::new();
    h.local("changed 🦀\n\nbeta\n");
    let entry = h.session.storage.recover().unwrap().pending.remove(0);
    let facts = serde_json::json!({
        "identity":entry.identity, "checkpoint":h.host.service.checkpoint.bytes,
        "entry":entry, "receipt":receipt(&entry),
        "step1":sync::frame(0,&h.session.recovery.document.state_vector_v1()).unwrap(),
        "step2":sync::frame(1,&h.session.recovery.document.update_v1(&[0]).unwrap()).unwrap(),
        "update":sync::frame(2,&entry.bytes).unwrap()
    });
    if let Some(path) = std::env::var_os("EDITAC_CLI_SYNC_EXCHANGE") {
        fs::write(path, serde_json::to_vec_pretty(&facts).unwrap()).unwrap();
    }
    assert_eq!(
        sync::decode_frame(&serde_json::from_value::<Vec<u8>>(facts["update"].clone()).unwrap())
            .unwrap()
            .0,
        2
    );
}

#[test]
fn local_retry_after_uncertain_publication_reuses_exact_pending_identity() {
    let mut h = Harness::new();
    let update = h.edit("changed\n\nbeta\n");
    assert!(
        sync::persist_local_with(
            &mut h.session,
            &update,
            &Fail {
                kind: RecordKind::Journal,
                boundary: Boundary::AfterPublish
            }
        )
        .is_err()
    );
    let pending = h.session.storage.recover().unwrap().pending;
    sync::persist_local(&mut h.session, &update).unwrap();
    assert_eq!(h.session.storage.recover().unwrap().pending, pending);
    assert_eq!(pending.len(), 1);
    let result = h.run();
    assert_eq!(result.status.pending_upload, 0);
    assert_eq!(h.transport.uploads, pending);
}
#[test]
fn attachment_crosses_recovered_native_flush_before_connect() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    let result = sync::synchronize_with(
        &mut h.session,
        &h.host.config,
        &mut h.host.credentials,
        &mut h.transport,
        RetryPolicy::default(),
        &Fail {
            kind: RecordKind::Latest,
            boundary: Boundary::AfterFileSync,
        },
        &mut h.clock,
    )
    .unwrap();
    assert_eq!(
        result.status.connection,
        ConnectionState::LocalPersistenceFailure
    );
    assert_eq!(h.transport.connects, 0);
    assert!(!result.status.locally_saved);
    assert_eq!(h.reopened().recovery.pending.len(), 1);
}

#[test]
fn another_pending_entries_receipt_cannot_acknowledge_the_current_upload() {
    let mut h = Harness::new();
    h.local("first\n\nbeta\n");
    h.local("second\n\nbeta\n");
    let pending = h.session.storage.recover().unwrap().pending;
    h.transport.receipt_override = Some(receipt(&pending[1]));
    h.transport
        .upload_failures
        .extend([None, Some(TransportError::Revoked)]);
    let result = h.run();
    assert_eq!(result.ignored_receipts, 1);
    assert_eq!(result.status.connection, ConnectionState::Revoked);
    assert_eq!(result.status.remote_committed_updates, 0);
    assert_eq!(h.reopened().recovery.pending, pending);
}

#[test]
fn dependent_frames_reordered_within_and_across_exchanges_recover_without_echo() {
    for split in [false, true] {
        let mut h = Harness::new();
        h.local("alpha\n\nlocal beta\n");
        let mut peer =
            Document::restore(&h.host.identity, &h.host.service.checkpoint, &[]).unwrap();
        let first = peer_edit(&mut peer, "first ");
        let second = peer_edit(&mut peer, "second ");
        let frames = vec![
            sync::frame(2, &second.bytes).unwrap(),
            sync::frame(2, &first.bytes).unwrap(),
            sync::frame(2, &second.bytes).unwrap(),
        ];
        if split {
            h.transport.later_frames.push_back(vec![frames[0].clone()]);
            h.transport.later_frames.push_back(frames[1..].to_vec());
        } else {
            h.transport.extra_frames = frames;
        }
        let result = h.run();
        assert_eq!(result.status.connection, ConnectionState::Connected);
        assert_eq!(result.status.pending_upload, 0);
        assert_eq!(h.transport.uploads.len(), 1);
        let reopened = h.reopened();
        let source = reopened.source_baseline().unwrap().opening().source.clone();
        assert!(
            source.contains("second first alpha") && source.contains("local beta"),
            "{source}"
        );
        assert_eq!(
            reopened
                .recovery
                .tail
                .iter()
                .filter(|e| e.origin == "editac:remote:sync")
                .count(),
            2
        );
    }
}

#[test]
fn missing_dependencies_have_a_bounded_receive_window_and_preserve_local_outbox() {
    let mut h = Harness::new();
    h.local("alpha\n\nlocal beta\n");
    let pending = h.session.storage.recover().unwrap().pending;
    let mut peer = Document::restore(&h.host.identity, &h.host.service.checkpoint, &[]).unwrap();
    peer_edit(&mut peer, "missing ");
    let second = peer_edit(&mut peer, "dependent ");
    h.transport.extra_frames = vec![sync::frame(2, &second.bytes).unwrap()];
    let result = h.run();
    assert_eq!(result.status.connection, ConnectionState::InvalidProtocol);
    assert_eq!(h.transport.exchanges, sync::MAX_FRAMES);
    assert!(h.transport.uploads.is_empty());
    assert_eq!(h.reopened().recovery.pending, pending);
    assert!(
        h.session
            .source_baseline()
            .unwrap()
            .opening()
            .source
            .contains("local beta")
    );
}

#[test]
fn dependent_piggybacked_frames_on_no_receipt_upload_follow_the_same_receive_boundary() {
    let mut h = Harness::new();
    h.local("alpha\n\nlocal beta\n");
    let mut peer = Document::restore(&h.host.identity, &h.host.service.checkpoint, &[]).unwrap();
    let first = peer_edit(&mut peer, "first ");
    let second = peer_edit(&mut peer, "second ");
    h.transport.capability = None;
    h.transport.later_frames.extend([
        vec![],
        vec![],
        vec![sync::frame(2, &second.bytes).unwrap()],
        vec![sync::frame(2, &first.bytes).unwrap()],
    ]);
    let result = h.run();
    assert_eq!(result.status.connection, ConnectionState::Connected);
    assert_eq!(result.status.remote_durability, "unknown");
    assert_eq!(result.status.pending_upload, 1);
    assert_eq!(result.received_updates, 2);
    let reopened = h.reopened();
    assert!(
        reopened
            .source_baseline()
            .unwrap()
            .opening()
            .source
            .contains("second first alpha")
    );
}

#[test]
fn receive_window_capacity_counts_unresolved_frames_across_exchanges() {
    let mut h = Harness::new();
    h.local("alpha\n\nlocal beta\n");
    let pending = h.session.storage.recover().unwrap().pending;
    let mut peer = Document::restore(&h.host.identity, &h.host.service.checkpoint, &[]).unwrap();
    peer_edit(&mut peer, "missing ");
    let second = peer_edit(&mut peer, "dependent ");
    let bytes = sync::frame(2, &second.bytes).unwrap();
    h.transport
        .later_frames
        .extend([vec![bytes.clone(); sync::MAX_FRAMES - 2], vec![bytes; 3]]);
    let result = h.run();
    assert_eq!(result.status.connection, ConnectionState::InvalidProtocol);
    assert_eq!(h.transport.exchanges, 2);
    assert_eq!(h.reopened().recovery.pending, pending);
    assert!(h.transport.uploads.is_empty());
}
