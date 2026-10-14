//! CLI-1.6 canonical native harness, with real pinned replica and native filesystem.
use editac_cli::{
    editor::{self, EditorCommand, EditorOutcome},
    engine::document::{CreationPermit, Document, DocumentIdentity, ReplayUpdate},
    files,
    reconcile::{LocalBaseline, ReconciledEdit},
    storage::{Boundary, Draft, FailureKind, IoHooks, NativeStorage, RecordKind, RecoveryPoint},
    target::{LocalTarget, ResolveOptions, resolve},
};
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

fn corpus() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/recovery/boundaries.json")).unwrap()
}
fn source(key: &str) -> String {
    corpus()[key].as_str().unwrap().into()
}
struct Fixture {
    root: PathBuf,
    target: LocalTarget,
    identity: DocumentIdentity,
    document: Document,
    baseline: LocalBaseline,
    store: NativeStorage,
}
impl Fixture {
    fn new(extension: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("editac-storage-{}-{id}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let key = if extension == "md" {
            "baselineMarkdown"
        } else {
            "baselineText"
        };
        let path = root.join(format!("source.{extension}"));
        fs::write(&path, source(key)).unwrap();
        let target = resolve(&path, ResolveOptions::default()).unwrap();
        let identity = DocumentIdentity::v5(
            format!("bbbbbbbb-bbbb-4bbb-8bbb-{id:012x}"),
            "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
        )
        .unwrap();
        let (document, baseline) = LocalBaseline::import(
            target.clone(),
            CreationPermit::authorize_new(identity.clone()).unwrap(),
        )
        .unwrap();
        let store = NativeStorage::open(root.join("session"), identity.clone()).unwrap();
        store.initialize(&document).unwrap();
        Self {
            root,
            target,
            identity,
            document,
            baseline,
            store,
        }
    }
    fn buffer(&self, text: &str) -> editac_cli::editor::ChangedBuffer {
        buffer(&self.root, &self.target, text)
    }
    fn edit(&mut self, text: &str) -> ReconciledEdit {
        let b = self.buffer(text);
        self.baseline
            .prepare(&self.document, b)
            .unwrap()
            .commit(&mut self.document)
            .unwrap()
    }
    fn changed(&mut self) -> ReconciledEdit {
        let key = if self.target.resolved_path().extension().unwrap() == "md" {
            "editedMarkdown"
        } else {
            "editedText"
        };
        self.edit(&source(key))
    }
    fn persist(&self, edit: &ReconciledEdit) -> String {
        self.store
            .persist_update(
                &self.document,
                &edit.commit.as_ref().unwrap().update,
                1,
                "local-source",
            )
            .unwrap()
            .update_id
    }
    fn assert_original(&self) {
        assert_eq!(
            fs::read_to_string(self.target.resolved_path()).unwrap(),
            self.target.source()
        );
    }
    fn snapshot(&self) -> Vec<(String, String)> {
        snapshot(&self.document)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn snapshot(document: &Document) -> Vec<(String, String)> {
    document
        .snapshot()
        .unwrap()
        .visible
        .iter()
        .filter(|n| n.kind == "block")
        .map(|n| {
            (
                n.id.clone(),
                n.runs.iter().map(|r| r.text.as_str()).collect(),
            )
        })
        .collect()
}
fn owners(d: &Document) -> Vec<String> {
    snapshot(d).into_iter().map(|(id, _)| id).collect()
}
fn buffer(
    root: &std::path::Path,
    target: &LocalTarget,
    text: &str,
) -> editac_cli::editor::ChangedBuffer {
    let candidate = root.join("candidate");
    fs::write(&candidate, text).unwrap();
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/editor/fake editor.sh");
    let command = EditorCommand::parse(&format!(
        "'{}' changed '{}'",
        script.display(),
        candidate.display()
    ))
    .unwrap();
    match editor::run(target, &command, root).unwrap() {
        EditorOutcome::Changed(b) => b,
        _ => panic!("fixture must change"),
    }
}
struct Fail {
    kind: RecordKind,
    boundary: Boundary,
}
impl IoHooks for Fail {
    fn boundary(&self, k: RecordKind, b: Boundary) -> io::Result<()> {
        if k == self.kind && b == self.boundary {
            Err(io::Error::from_raw_os_error(28))
        } else {
            Ok(())
        }
    }
}
fn published(b: Boundary) -> bool {
    matches!(b, Boundary::AfterPublish | Boundary::AfterDirectorySync)
}
fn journal_failure(b: Boundary) {
    let mut f = Fixture::new("md");
    let old = f.snapshot();
    let edit = f.changed();
    let expected = f.snapshot();
    let draft = Draft {
        baseline: f.target.source().into(),
        source: edit.buffer.source().into(),
    };
    f.store.persist_draft(&draft).unwrap();
    let result = f.store.persist_update_with(
        &f.document,
        &edit.commit.as_ref().unwrap().update,
        1,
        "local-source",
        &Fail {
            kind: RecordKind::Journal,
            boundary: b,
        },
    );
    let error = result.unwrap_err();
    assert_eq!(
        error.kind,
        if published(b) {
            FailureKind::PublicationUncertain
        } else {
            FailureKind::Io
        }
    );
    let r = f.store.recover().unwrap();
    assert_eq!(
        snapshot(&r.document),
        if published(b) { expected } else { old }
    );
    assert_eq!(r.pending.len(), usize::from(published(b)));
    assert_eq!(r.draft, Some(draft));
    f.assert_original();
    // A retry is an actual new durability crossing, even after uncertain publication.
    let id = f.persist(&edit);
    let r = f.store.recover().unwrap();
    assert_eq!(r.pending.len(), 1);
    assert_eq!(r.pending[0].update_id, id);
    assert_eq!(
        r.pending[0].bytes,
        edit.commit.as_ref().unwrap().update.bytes
    );
}
fn checkpoint_failure(b: Boundary) {
    let mut f = Fixture::new("md");
    let edit = f.changed();
    let id = f.persist(&edit);
    let expected = f.snapshot();
    let error = f
        .store
        .publish_checkpoint_with(
            &f.document,
            &[id.clone()],
            &Fail {
                kind: RecordKind::Latest,
                boundary: b,
            },
        )
        .unwrap_err();
    assert_eq!(
        error.kind,
        if published(b) {
            FailureKind::PublicationUncertain
        } else {
            FailureKind::Io
        }
    );
    let r = f.store.recover().unwrap();
    assert_eq!(snapshot(&r.document), expected);
    assert_eq!(r.pending.len(), 1);
    assert_eq!(r.tail.len(), usize::from(!published(b)));
    assert_eq!(r.coverage, if published(b) { vec![id] } else { vec![] });
    f.assert_original();
}
fn draft_failure(b: Boundary) {
    let f = Fixture::new("txt");
    let first = Draft {
        baseline: "base".into(),
        source: "first".into(),
    };
    let second = Draft {
        baseline: "base".into(),
        source: "second".into(),
    };
    f.store.persist_draft(&first).unwrap();
    let error = f
        .store
        .persist_draft_with(
            &second,
            &Fail {
                kind: RecordKind::Draft,
                boundary: b,
            },
        )
        .unwrap_err();
    assert_eq!(
        error.kind,
        if published(b) {
            FailureKind::PublicationUncertain
        } else {
            FailureKind::Io
        }
    );
    assert_eq!(
        f.store.read_draft().unwrap(),
        Some(if published(b) { second } else { first })
    );
    assert_eq!(snapshot(&f.store.recover().unwrap().document), f.snapshot());
}
macro_rules! boundary_tests { ($function:ident; $($name:ident => $boundary:ident),+ $(,)?) => { $(#[test] fn $name() { $function(Boundary::$boundary); })+ }; }
boundary_tests!(journal_failure; journal_before_write=>BeforeWrite,journal_after_write=>AfterWrite,journal_after_file_sync=>AfterFileSync,journal_before_publish=>BeforePublish,journal_after_publish=>AfterPublish,journal_after_directory_sync=>AfterDirectorySync);
boundary_tests!(checkpoint_failure; checkpoint_before_write=>BeforeWrite,checkpoint_after_write=>AfterWrite,checkpoint_after_file_sync=>AfterFileSync,checkpoint_before_publish=>BeforePublish,checkpoint_after_publish=>AfterPublish,checkpoint_after_directory_sync=>AfterDirectorySync);
boundary_tests!(draft_failure; draft_before_write=>BeforeWrite,draft_after_write=>AfterWrite,draft_after_file_sync=>AfterFileSync,draft_before_publish=>BeforePublish,draft_after_publish=>AfterPublish,draft_after_directory_sync=>AfterDirectorySync);

fn workflow(extension: &str) {
    let mut f = Fixture::new(extension);
    let edit = f.changed();
    let expected = f.snapshot();
    let writer = f.document.writer_id();
    let draft = Draft {
        baseline: f.target.source().into(),
        source: edit.buffer.source().into(),
    };
    f.store.persist_draft(&draft).unwrap();
    let id = f.persist(&edit);
    let bytes = edit.commit.as_ref().unwrap().update.bytes.clone();
    let saved = files::save(edit).unwrap();
    assert_eq!(saved.state, files::SaveState::Saved);
    assert_eq!(fs::read_to_string(&saved.path).unwrap(), draft.source);
    f.store
        .publish_checkpoint(&f.document, &[id.clone()])
        .unwrap();
    let r = f.store.recover().unwrap();
    assert_eq!(snapshot(&r.document), expected);
    assert_ne!(r.document.writer_id(), writer);
    assert!(r.tail.is_empty());
    assert_eq!(r.coverage, vec![id.clone()]);
    assert_eq!(r.pending[0].bytes, bytes);
    assert_eq!(r.pending[0].update_id, id);
    assert_eq!(r.draft, Some(draft));
    assert!(r.rejected_latest.is_none());
    // Plain files are still independently consumable, without a proprietary container.
    let plain = resolve(&saved.path, ResolveOptions::default()).unwrap();
    assert_eq!(plain.source(), saved.edit.buffer.source());
}
#[test]
fn markdown_resolver_editor_commit_save_checkpoint_restore() {
    workflow("md");
}
#[test]
fn literal_unicode_resolver_editor_commit_save_checkpoint_restore() {
    workflow("txt");
}
#[test]
fn restored_checkpoint_encoding_keeps_crdt_state_and_exact_outbox() {
    let mut f = Fixture::new("md");
    let edit = f.changed();
    let id = f.persist(&edit);
    let bytes = edit.commit.as_ref().unwrap().update.bytes.clone();
    let expected = f.snapshot();
    // Restoring repeatedly creates fresh live replicas and re-encodes nested
    // metadata maps; neither map order nor writer identity is persisted state.
    for _ in 0..8 {
        let r = f.store.recover().unwrap();
        f.store
            .publish_checkpoint(&r.document, &[id.clone()])
            .unwrap();
        let r = f.store.recover().unwrap();
        assert_eq!(snapshot(&r.document), expected);
        assert_eq!(r.pending[0].bytes, bytes);
        assert_eq!(r.coverage, vec![id.clone()]);
        assert!(r.tail.is_empty());
        assert!(r.rejected_latest.is_none());
    }
}

#[test]
fn two_live_restores_have_fresh_writers_and_keep_both_updates() {
    let f = Fixture::new("md");
    let mut a = f.store.recover().unwrap().document;
    let mut b = f.store.recover().unwrap().document;
    assert_ne!(a.writer_id(), b.writer_id());
    assert_ne!(a.writer_id(), f.document.writer_id());
    assert_ne!(b.writer_id(), f.document.writer_id());
    let ba = LocalBaseline::capture(&a, f.target.clone(), &owners(&a)).unwrap();
    let bb = LocalBaseline::capture(&b, f.target.clone(), &owners(&b)).unwrap();
    let ea = ba
        .prepare(&a, f.buffer("# Changed\r\n\r\nkeep _spelling_\r\n"))
        .unwrap()
        .commit(&mut a)
        .unwrap();
    let eb = bb
        .prepare(&b, f.buffer("# Before\r\n\r\nkeep _second_\r\n"))
        .unwrap()
        .commit(&mut b)
        .unwrap();
    // Publish in reverse live-writer order; recovery consumes both immutable keys.
    let ib = f
        .store
        .persist_update(&b, &eb.commit.as_ref().unwrap().update, 1, "local-source")
        .unwrap();
    let ia = f
        .store
        .persist_update(&a, &ea.commit.as_ref().unwrap().update, 1, "local-source")
        .unwrap();
    assert_ne!(ia.update_id, ib.update_id);
    let r = f.store.recover().unwrap();
    assert_eq!(r.pending.len(), 2);
    let texts: Vec<_> = snapshot(&r.document).into_iter().map(|(_, t)| t).collect();
    assert_eq!(texts, vec!["Changed", "keep second"]);
    f.store
        .publish_checkpoint(&r.document, &[ia.update_id, ib.update_id])
        .unwrap();
    assert_eq!(f.store.recover().unwrap().pending.len(), 2);
}
#[test]
fn exact_shared_journal_receipt_identity_and_duplicate_idempotence() {
    let mut f = Fixture::new("txt");
    let edit = f.changed();
    let id = f.persist(&edit);
    assert_eq!(f.persist(&edit), id);
    let r = f.store.recover().unwrap();
    assert_eq!(r.pending.len(), 1);
    let e = &r.pending[0];
    let expected_key = serde_json::to_string(&[
        serde_json::json!(f.identity.profile),
        serde_json::json!(f.identity.schema),
        serde_json::json!(f.identity.document_id),
        serde_json::json!(f.identity.epoch),
        serde_json::json!(1),
        serde_json::json!(1),
        serde_json::json!("utf16"),
    ])
    .unwrap();
    assert_eq!(e.identity.key(), expected_key);
    let checksum = format!("{:x}", Sha256::digest(&e.bytes));
    assert_eq!(
        e.update_id,
        serde_json::to_string(&(expected_key, e.writer_id, 1, &checksum)).unwrap()
    );
}
#[test]
fn immutable_key_collision_and_regressing_sequence_fail_without_mutation() {
    let mut f = Fixture::new("txt");
    let edit = f.changed();
    let u = &edit.commit.as_ref().unwrap().update;
    f.store
        .persist_update(&f.document, u, 2, "local-source")
        .unwrap();
    let before = fs::read(f.store.root().join(format!(
        "journal-{:010}-{:016}.json",
        f.document.writer_id(),
        2
    )))
    .unwrap();
    assert_eq!(
        f.store
            .persist_update(&f.document, u, 2, "changed-origin")
            .unwrap_err()
            .kind,
        FailureKind::Invalid
    );
    assert_eq!(
        f.store
            .persist_update(&f.document, u, 1, "local-source")
            .unwrap_err()
            .kind,
        FailureKind::Invalid
    );
    assert_eq!(
        before,
        fs::read(f.store.root().join(format!(
            "journal-{:010}-{:016}.json",
            f.document.writer_id(),
            2
        )))
        .unwrap()
    );
    assert_eq!(f.store.recover().unwrap().pending.len(), 1);
}
#[test]
fn corrupt_latest_recovers_previous_and_exact_tail_outbox_draft() {
    let mut f = Fixture::new("md");
    let edit = f.changed();
    let id = f.persist(&edit);
    f.store
        .persist_draft(&Draft {
            baseline: "base".into(),
            source: "unreconciled 👩‍💻".into(),
        })
        .unwrap();
    f.store
        .publish_checkpoint(&f.document, &[id.clone()])
        .unwrap();
    fs::write(f.store.root().join("latest.json"), b"{truncated").unwrap();
    let r = f.store.recover().unwrap();
    assert_eq!(r.point, RecoveryPoint::Previous);
    assert!(r.rejected_latest.is_some());
    assert_eq!(snapshot(&r.document), f.snapshot());
    assert_eq!(r.tail.len(), 1);
    assert_eq!(r.pending[0].update_id, id);
    assert_eq!(r.draft.unwrap().source, "unreconciled 👩‍💻");
    f.store.publish_checkpoint(&r.document, &[id]).unwrap();
    assert_eq!(f.store.recover().unwrap().point, RecoveryPoint::Latest);
}
#[test]
fn both_corrupt_checkpoints_fail_closed_but_draft_is_independently_recoverable() {
    let f = Fixture::new("md");
    let d = Draft {
        baseline: "base".into(),
        source: "retained".into(),
    };
    f.store.persist_draft(&d).unwrap();
    for name in ["latest.json", "previous.json"] {
        fs::write(f.store.root().join(name), b"bad").unwrap();
    }
    assert_eq!(f.store.recover().unwrap_err().kind, FailureKind::Missing);
    assert_eq!(f.store.read_draft().unwrap(), Some(d));
    assert!(f.store.initialize(&f.document).is_err());
    f.assert_original();
}
#[test]
fn corrupt_covered_journal_cannot_be_hidden_by_checkpoint() {
    let mut f = Fixture::new("md");
    let edit = f.changed();
    let id = f.persist(&edit);
    f.store.publish_checkpoint(&f.document, &[id]).unwrap();
    let r = f.store.recover().unwrap();
    let p = f.store.root().join(format!(
        "journal-{:010}-{:016}.json",
        r.pending[0].writer_id, 1
    ));
    fs::write(&p, b"corrupt").unwrap();
    assert_eq!(f.store.recover().unwrap_err().kind, FailureKind::Invalid);
    assert_eq!(fs::read(p).unwrap(), b"corrupt");
}
#[test]
fn corrupt_draft_reports_separate_error_without_losing_committed_state() {
    let f = Fixture::new("txt");
    fs::write(f.store.root().join("draft.json"), b"bad").unwrap();
    let r = f.store.recover().unwrap();
    assert_eq!(snapshot(&r.document), f.snapshot());
    assert!(r.draft.is_none());
    assert!(r.rejected_draft.is_some());
    assert!(f.store.read_draft().is_err());
}
#[test]
fn foreign_identity_schema_epoch_and_missing_store_never_create_empty_replacement() {
    let f = Fixture::new("md");
    let before = fs::read(f.store.root().join("latest.json")).unwrap();
    let mut identities = vec![
        DocumentIdentity::v4(f.identity.document_id.clone(), f.identity.epoch.clone()).unwrap(),
    ];
    let mut id = f.identity.clone();
    id.epoch = "dddddddd-dddd-4ddd-8ddd-dddddddddddd".into();
    identities.push(id);
    let mut id = f.identity.clone();
    id.document_id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd".into();
    identities.push(id);
    for id in identities {
        let s = NativeStorage::open(f.store.root(), id).unwrap();
        assert!(s.recover().is_err());
        assert!(s.initialize(&f.document).is_err());
    }
    assert_eq!(
        before,
        fs::read(f.store.root().join("latest.json")).unwrap()
    );
    let empty = NativeStorage::open(f.root.join("empty"), f.identity.clone()).unwrap();
    assert_eq!(empty.recover().unwrap_err().kind, FailureKind::Missing);
    assert!(!empty.root().join("latest.json").exists());
}
fn tamper(path: &std::path::Path, change: impl FnOnce(&mut serde_json::Value), reseal: bool) {
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    change(&mut v);
    if reseal {
        v["checksum"] = serde_json::json!(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&v["payload"]).unwrap())
        ));
    }
    fs::write(path, serde_json::to_vec(&v).unwrap()).unwrap();
}
#[test]
fn checkpoint_metadata_checksum_and_uncommitted_coverage_are_rejected() {
    let mut f = Fixture::new("md");
    let edit = f.changed();
    let id = f.persist(&edit);
    let initial = Document::restore(&f.identity, &f.document.checkpoint(), &[]).unwrap();
    assert!(
        f.store
            .publish_checkpoint(&initial, &["foreign".into()])
            .is_err()
    );
    assert!(
        f.store
            .publish_checkpoint(&initial, &[id.clone(), id.clone()])
            .is_err()
    );
    f.store.publish_checkpoint(&f.document, &[id]).unwrap();
    tamper(
        &f.store.root().join("latest.json"),
        |v| v["payload"]["coverage"] = serde_json::json!(["uncommitted"]),
        true,
    );
    let r = f.store.recover().unwrap();
    assert_eq!(r.point, RecoveryPoint::Previous);
    assert_eq!(snapshot(&r.document), f.snapshot());
}
#[test]
fn unsupported_record_version_and_bad_hash_fall_back_without_rewriting_evidence() {
    let f = Fixture::new("md");
    let p = f.store.root().join("latest.json");
    tamper(&p, |v| v["version"] = serde_json::json!(99), false);
    let bad = fs::read(&p).unwrap();
    assert_eq!(f.store.recover().unwrap().point, RecoveryPoint::Previous);
    assert_eq!(fs::read(&p).unwrap(), bad);
    tamper(
        &p,
        |v| {
            v["version"] = serde_json::json!(1);
            v["checksum"] = serde_json::json!("bad");
        },
        false,
    );
    assert_eq!(f.store.recover().unwrap().point, RecoveryPoint::Previous);
}
#[test]
fn invalid_update_and_sequence_never_acknowledge_or_mutate() {
    let mut f = Fixture::new("md");
    let edit = f.changed();
    let update = &edit.commit.as_ref().unwrap().update;
    for sequence in [0, u64::MAX] {
        assert!(
            f.store
                .persist_update(&f.document, update, sequence, "local")
                .is_err()
        );
    }
    let bad = ReplayUpdate {
        identity: f.identity.clone(),
        bytes: vec![0xff],
    };
    assert!(
        f.store
            .persist_update(&f.document, &bad, 1, "local")
            .is_err()
    );
    assert!(f.store.recover().unwrap().pending.is_empty());
    f.assert_original();
}
#[test]
fn uncommitted_and_stale_checkpoint_cannot_drop_state() {
    let mut f = Fixture::new("md");
    let initial = f.document.checkpoint();
    let edit = f.changed();
    let id = f.persist(&edit);
    let old = Document::restore(&f.identity, &initial, &[]).unwrap();
    assert!(f.store.publish_checkpoint(&old, &[]).is_err());
    assert!(f.store.publish_checkpoint(&old, &[id]).is_err());
    assert_eq!(snapshot(&f.store.recover().unwrap().document), f.snapshot());
}
#[test]
fn cooperating_lock_refuses_simultaneous_mutation() {
    let f = Fixture::new("md");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.store.root().join("lock"))
        .unwrap();
    lock.lock().unwrap();
    assert_eq!(f.store.recover().unwrap_err().kind, FailureKind::Busy);
    assert_eq!(
        f.store
            .persist_draft(&Draft {
                baseline: "".into(),
                source: "candidate".into()
            })
            .unwrap_err()
            .kind,
        FailureKind::Busy
    );
    lock.unlock().unwrap();
    drop(lock);
    assert!(f.store.recover().is_ok());
}
#[test]
fn symlink_record_and_changed_directory_are_rejected() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new("md");
    let p = f.store.root().join("latest.json");
    fs::remove_file(&p).unwrap();
    symlink(f.target.resolved_path(), &p).unwrap();
    assert_eq!(f.store.recover().unwrap().point, RecoveryPoint::Previous);
    let moved = f.root.join("moved");
    fs::rename(f.store.root(), &moved).unwrap();
    fs::create_dir(f.store.root()).unwrap();
    assert_eq!(f.store.recover().unwrap_err().kind, FailureKind::Invalid);
    f.assert_original();
}
#[test]
fn private_store_and_oversized_sparse_record_fail_before_decode() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("md");
    assert_eq!(
        fs::metadata(f.store.root()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let file = fs::OpenOptions::new()
        .write(true)
        .open(f.store.root().join("latest.json"))
        .unwrap();
    file.set_len(200 * 1024 * 1024).unwrap();
    assert_eq!(f.store.recover().unwrap().point, RecoveryPoint::Previous);
    let public = f.root.join("public");
    fs::create_dir(&public).unwrap();
    fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        NativeStorage::open(&public, f.identity.clone())
            .err()
            .unwrap()
            .kind,
        FailureKind::Invalid
    );
}

// Child process uses the same canonical resolver/editor/engine/storage path. No
// destructors run when a boundary hook exits; parent owns and joins every child.
struct ExitAt(Boundary);
impl IoHooks for ExitAt {
    fn boundary(&self, k: RecordKind, b: Boundary) -> io::Result<()> {
        if k == RecordKind::Journal && b == self.0 {
            std::process::exit(73);
        }
        Ok(())
    }
}
#[test]
fn process_worker() {
    let Ok(root) = std::env::var("EDITAC_RECOVERY_CHILD_ROOT") else {
        assert_eq!(corpus()["boundaries"].as_array().unwrap().len(), 6);
        return;
    };
    let root = PathBuf::from(root);
    let identity = DocumentIdentity::v5(
        std::env::var("EDITAC_RECOVERY_CHILD_DOCUMENT").unwrap(),
        "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
    )
    .unwrap();
    let store = NativeStorage::open(root.join("session"), identity).unwrap();
    let mut doc = store.recover().unwrap().document;
    let target = resolve(root.join("source.md"), ResolveOptions::default()).unwrap();
    let baseline = LocalBaseline::capture(&doc, target.clone(), &owners(&doc)).unwrap();
    let edit = baseline
        .prepare(&doc, buffer(&root, &target, &source("editedMarkdown")))
        .unwrap()
        .commit(&mut doc)
        .unwrap();
    let mode = std::env::var("EDITAC_RECOVERY_CHILD_BOUNDARY").unwrap();
    let boundary = match mode.as_str() {
        "BeforeWrite" => Boundary::BeforeWrite,
        "AfterWrite" => Boundary::AfterWrite,
        "AfterFileSync" => Boundary::AfterFileSync,
        "BeforePublish" => Boundary::BeforePublish,
        "AfterPublish" => Boundary::AfterPublish,
        _ => Boundary::AfterDirectorySync,
    };
    if mode == "ack" {
        let ack = store
            .persist_update(
                &doc,
                &edit.commit.as_ref().unwrap().update,
                1,
                "local-source",
            )
            .unwrap();
        // Publish the observation marker only after its exact ID is complete.
        // Existence of an eagerly created file could otherwise trigger kill
        // between create/truncate and write, after the store already acknowledged.
        fs::write(root.join("ack.prepared"), ack.update_id).unwrap();
        fs::File::open(root.join("ack.prepared"))
            .unwrap()
            .sync_all()
            .unwrap();
        fs::rename(root.join("ack.prepared"), root.join("ack")).unwrap();
        loop {
            std::thread::sleep(Duration::from_millis(100));
        }
    } else {
        store
            .persist_update_with(
                &doc,
                &edit.commit.as_ref().unwrap().update,
                1,
                "local-source",
                &ExitAt(boundary),
            )
            .unwrap();
        panic!("boundary must terminate");
    }
}
fn child(f: &Fixture, mode: &str) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process_worker", "--nocapture"])
        .env("EDITAC_RECOVERY_CHILD_ROOT", &f.root)
        .env("EDITAC_RECOVERY_CHILD_DOCUMENT", &f.identity.document_id)
        .env("EDITAC_RECOVERY_CHILD_BOUNDARY", mode)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}
fn crash(b: Boundary) {
    let f = Fixture::new("md");
    let mut process = child(&f, &format!("{b:?}"));
    assert_eq!(process.wait().unwrap().code(), Some(73));
    let r = f.store.recover().unwrap();
    assert_eq!(r.pending.len(), usize::from(published(b)));
    let expected = if published(b) {
        vec!["After 🌍", "keep spelling"]
    } else {
        vec!["Before", "keep spelling"]
    };
    assert_eq!(
        snapshot(&r.document)
            .into_iter()
            .map(|(_, t)| t)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(!f.root.join("ack").exists());
    f.assert_original();
}
boundary_tests!(crash; process_crash_before_write=>BeforeWrite,process_crash_after_write=>AfterWrite,process_crash_after_file_sync=>AfterFileSync,process_crash_before_publish=>BeforePublish,process_crash_after_publish=>AfterPublish,process_crash_after_directory_sync=>AfterDirectorySync);
#[test]
fn actual_process_kill_after_ack_preserves_exact_committed_update() {
    let f = Fixture::new("md");
    let mut process = child(&f, "ack");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.root.join("ack").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let acknowledged = f.root.join("ack").exists();
    process.kill().unwrap();
    process.wait().unwrap();
    assert!(acknowledged, "child did not cross acknowledgment boundary");
    let r = f.store.recover().unwrap();
    assert_eq!(r.pending.len(), 1);
    assert_eq!(
        r.pending[0].update_id,
        fs::read_to_string(f.root.join("ack")).unwrap()
    );
    assert_eq!(
        snapshot(&r.document)
            .into_iter()
            .map(|(_, t)| t)
            .collect::<Vec<_>>(),
        vec!["After 🌍", "keep spelling"]
    );
    f.assert_original();
}
