//! Deterministic child-process fixture, never a provider/production-data runner.
#[path = "mod.rs"]
mod support;
use editac_core::document::{Document, ReplayUpdate};
use editac_session::{ports::*, store::*};
use std::{path::PathBuf, sync::Arc};
use support::*;
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let path = PathBuf::from(&args[1]);
    let mode = &args[2];
    let identity = checkpoint().identity.clone();
    let observer: Option<TransactionObserver> = if mode.starts_with("kill-") {
        let target = match mode.as_str() {
            m if m.ends_with("-write") => TransactionBoundary::BeforeWrite,
            m if m.ends_with("-commit") => TransactionBoundary::BeforeCommit,
            _ => TransactionBoundary::AfterCommit,
        };
        let signal = PathBuf::from(&args[3]);
        Some(Arc::new(move |boundary| {
            if boundary == target {
                std::fs::write(&signal, format!("{boundary:?}")).unwrap();
                loop {
                    std::thread::park();
                }
            }
        }))
    } else {
        None
    };
    // Reserve before observing so the selected boundary belongs to the actual edit/publication.
    let store =
        NativeStore::open_existing(&path, identity.clone(), StoreLimits::default()).unwrap();
    let recovered = store.recover(&identity).unwrap();
    let tail: Vec<_> = recovered
        .journal
        .iter()
        .map(|e| ReplayUpdate {
            identity: identity.clone(),
            bytes: e.bytes().to_vec(),
        })
        .collect();
    let mut doc = Document::restore(&identity, recovered.checkpoint.checkpoint(), &tail).unwrap();
    insert(&mut doc, 0, "native child 😀");
    let entry = JournalEntry::new(
        identity.clone(),
        doc.writer_id(),
        1,
        "human".into(),
        doc.last_patch()
            .unwrap()
            .update
            .as_ref()
            .unwrap()
            .bytes
            .clone(),
    )
    .unwrap();
    store.claim_writer(doc.writer_id()).unwrap();
    if mode.contains("publish") || mode.contains("receipt") {
        store.append(entry.clone()).unwrap();
    }
    drop(store);
    let store = NativeStore::open_existing_observed(
        &path,
        identity.clone(),
        StoreLimits::default(),
        observer,
    )
    .unwrap();
    if mode == "cas" {
        let signal = PathBuf::from(&args[3]);
        let go = PathBuf::from(&args[4]);
        let generation = store.generation(&identity).unwrap();
        std::fs::write(signal, generation.to_string()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !go.exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        match store.publish(
            generation,
            CheckpointRecord::new(recovered.checkpoint.checkpoint().clone(), vec![]).unwrap(),
        ) {
            Ok(_) => std::process::exit(0),
            Err(PortError::Conflict) => std::process::exit(10),
            Err(e) => panic!("unexpected {e}"),
        }
    }
    if mode == "claim" {
        match store.claim_writer(10001) {
            Ok(()) => std::process::exit(0),
            Err(PortError::Conflict) => std::process::exit(10),
            Err(e) => panic!("{e}"),
        }
    }
    if mode.contains("publish") {
        store
            .publish(
                1,
                CheckpointRecord::new(doc.checkpoint(), vec![entry.id().clone()]).unwrap(),
            )
            .unwrap();
    } else if mode.contains("receipt") {
        store
            .accept_receipt(
                ReceiptCapability::DurableV1,
                DurableReceipt {
                    protocol: ReceiptCapability::DurableV1,
                    update: entry.id().clone(),
                },
            )
            .unwrap();
    } else {
        store.append(entry).unwrap();
    }
    if mode == "ack-exit" {
        std::fs::write(&args[3], b"acknowledged").unwrap();
        std::process::exit(0);
    }
    panic!("probe did not reach the requested termination boundary");
}
