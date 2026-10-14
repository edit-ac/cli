//! CLI-1.12 canonical configured executable -> native cache/editor/reconcile/sync.
#[path = "support/hosted.rs"]
mod host;
use editac_cli::{
    engine::document::Document,
    reconcile::HostedBaseline,
    storage::{NativeStorage, StoredIdentity},
    sync,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
struct Workflow {
    h: host::Harness,
    config: PathBuf,
    adapter: PathBuf,
    fixture: Value,
}
impl Workflow {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let h = host::Harness::with_source("alpha\n\nbeta\n");
        let config = h.root.join("service.json");
        fs::write(&config, serde_json::to_vec(&json!({"host":"app.example.test", "endpoint":"https://api.example.test/documents", "identity":StoredIdentity::from(&h.identity)})).unwrap()).unwrap();
        let fixture = json!({"identity":StoredIdentity::from(&h.identity), "checkpoint":{
            "identity":StoredIdentity::from(&h.identity),"initialized":true,
            "writerId":h.service.checkpoint.writer_id,"bytes":h.service.checkpoint.bytes}});
        let adapter = h.root.join("adapter.py");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/workflows/hosted/adapter.py"),
            &adapter,
        )
        .unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            h,
            config,
            adapter,
            fixture,
        }
    }
    fn save(&self) {
        fs::write(
            self.h.root.join("fixture.json"),
            serde_json::to_vec(&self.fixture).unwrap(),
        )
        .unwrap();
    }
    fn cached(&mut self) {
        drop(self.h.open(false).unwrap());
    }
    fn command(&self, operation: &str) -> Command {
        // The exact same nonzero suite can probe an extracted native package.
        let binary = std::env::var_os("EDITAC_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_editac").into());
        let mut c = Command::new(binary);
        c.env_clear()
            .arg(operation)
            .arg(&self.h.url)
            .arg("--service-config")
            .arg(&self.config)
            .arg("--cache-dir")
            .arg(&self.h.root);
        c
    }
    fn edit(&self, source: &str, mode: &str, offline: bool) -> Output {
        self.save();
        let payload = self.h.root.join("candidate.md");
        fs::write(&payload, source).unwrap();
        let editor =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workflows/hosted/editor.sh");
        let configuration = format!(
            "/bin/sh '{}' {} '{}' '{}'",
            editor.display(),
            mode,
            payload.display(),
            self.h.root.join("editor-finished").display()
        );
        let mut c = self.command("edit");
        c.args(["--editor", &configuration]);
        if offline {
            c.arg("--offline");
        } else {
            c.arg("--adapter").arg(&self.adapter);
        }
        c.output().unwrap()
    }
    fn remote(&mut self, source: &str) {
        let mut peer =
            Document::restore(&self.h.identity, &self.h.service.checkpoint, &[]).unwrap();
        peer.enable_history(Default::default()).unwrap();
        let baseline = HostedBaseline::capture(&peer).unwrap();
        let commit = baseline
            .prepare(&peer, source)
            .unwrap()
            .commit(&mut peer)
            .unwrap()
            .commit
            .unwrap();
        self.fixture["remoteFrames"] = json!([sync::frame(2, &commit.update.bytes).unwrap()]);
    }
    fn source(&mut self) -> String {
        let s = self.h.open(true).unwrap();
        s.source_baseline().unwrap().opening().source.clone()
    }
    fn pending(&self) -> usize {
        NativeStorage::open(self.h.cache(), self.h.identity.clone())
            .unwrap()
            .recover()
            .unwrap()
            .pending
            .iter()
            .filter(|entry| editac_cli::status::uploadable(entry))
            .count()
    }
    fn ops(&self) -> String {
        fs::read_to_string(self.h.root.join("operations.jsonl")).unwrap_or_default()
    }
}
fn report(o: Output) -> Value {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(o.stderr.is_empty());
    assert!(!String::from_utf8_lossy(&o.stdout).contains("prepared-hosted-secret"));
    serde_json::from_slice(&o.stdout).unwrap()
}
#[test]
fn online_bootstrap_editor_commit_exact_receipt_and_native_reopen() {
    let mut w = Workflow::new();
    let r = report(w.edit("local 🦀\n\nbeta\n", "changed", false));
    assert_eq!(r["state"], "locally-saved");
    assert_eq!(r["status"]["pendingUpload"], 0);
    assert_eq!(r["status"]["remoteDurability"], "remotely-committed");
    assert_eq!(r["status"]["unresolvedDraft"], false);
    assert_eq!(w.source(), "local 🦀\n\nbeta\n");
    assert_eq!(w.ops().matches("bootstrap").count(), 1);
    for (_, bytes) in host::tree(&w.h.cache()) {
        assert!(!String::from_utf8_lossy(&bytes).contains("prepared-hosted-secret"));
    }
}
#[test]
fn cached_offline_edit_has_no_credential_service_or_transport_calls() {
    let mut w = Workflow::new();
    w.cached();
    let r = report(w.edit("offline\n\nbeta\n", "changed", true));
    assert_eq!(r["status"]["connection"], "offline");
    assert_eq!(r["status"]["pendingUpload"], 1);
    assert_eq!(r["status"]["remoteDurability"], "unknown");
    assert_eq!(w.ops(), "");
    assert_eq!(w.source(), "offline\n\nbeta\n");
    assert_eq!(w.pending(), 1);
}
#[test]
fn uncached_offline_never_initializes_a_replacement() {
    let w = Workflow::new();
    let o = w.edit("local\n", "changed", true);
    assert_eq!(o.status.code(), Some(2));
    assert!(!w.h.cache().exists());
    assert_eq!(w.ops(), "");
}
#[test]
fn disjoint_remote_edit_received_after_editor_preserves_both_changes() {
    let mut w = Workflow::new();
    w.remote("alpha\n\nremote\n");
    let r = report(w.edit("local\n\nbeta\n", "changed", false));
    assert_eq!(r["state"], "locally-saved");
    assert_eq!(w.source(), "local\n\nremote\n");
    assert_eq!(w.pending(), 0);
}
#[test]
fn overlapping_remote_edit_retains_base_local_current_without_uploading_stale_source() {
    let mut w = Workflow::new();
    w.remote("remote\n\nbeta\n");
    let r = report(w.edit("local\n\nbeta\n", "changed", false));
    assert_eq!(r["state"], "conflict");
    let e: Value =
        serde_json::from_slice(&fs::read(r["evidence"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(e["base"], "alpha\n\nbeta\n");
    assert_eq!(e["local"], "local\n\nbeta\n");
    assert_eq!(e["current"], "remote\n\nbeta\n");
    assert_eq!(w.source(), "remote\n\nbeta\n");
    assert!(!w.ops().contains("upload"));
    let r = report(w.command("status").output().unwrap());
    assert_eq!(r["conflict"], true);
    assert_eq!(r["unresolvedDraft"], true);
}
#[test]
fn reconnect_uses_bounded_retry_then_exact_receipt() {
    let mut w = Workflow::new();
    w.fixture["disconnects"] = json!(1);
    let r = report(w.edit("local\n\nbeta\n", "changed", false));
    assert_eq!(r["status"]["remoteDurability"], "remotely-committed");
    assert!(w.ops().matches("connect\"").count() >= 4);
}
#[test]
fn revocation_retains_local_commit_outbox_and_exportable_source() {
    let mut w = Workflow::new();
    w.fixture["revoked"] = json!(true);
    let r = report(w.edit("local\n\nbeta\n", "changed", false));
    assert_eq!(r["status"]["connection"], "revoked");
    assert_eq!(r["status"]["locallySaved"], true);
    assert_eq!(r["status"]["pendingUpload"], 1);
    assert_eq!(w.source(), "local\n\nbeta\n");
    assert_eq!(w.pending(), 1);
}
#[test]
fn wrong_and_missing_receipts_never_prune_pending_work() {
    for mode in ["wrongReceipt", "missingReceipt", "noReceipts"] {
        let mut w = Workflow::new();
        w.fixture[mode] = json!(true);
        let r = report(w.edit("local\n\nbeta\n", "changed", false));
        assert_eq!(r["status"]["pendingUpload"], 1, "{mode}");
        assert_eq!(r["status"]["connection"], "connected", "{mode}");
        assert_eq!(r["status"]["remoteDurability"], "unknown");
        assert_eq!(w.pending(), 1);
        assert_eq!(w.source(), "local\n\nbeta\n");
    }
}
#[test]
fn pending_offline_edit_survives_process_exit_and_explicit_sync() {
    let mut w = Workflow::new();
    w.cached();
    report(w.edit("offline\n\nbeta\n", "changed", true));
    assert_eq!(w.pending(), 1);
    w.save();
    let r = report(
        w.command("sync")
            .arg("--adapter")
            .arg(&w.adapter)
            .output()
            .unwrap(),
    );
    assert_eq!(r["status"]["pendingUpload"], 0);
    assert_eq!(r["status"]["remoteDurability"], "remotely-committed");
    assert_eq!(w.source(), "offline\n\nbeta\n");
}
#[test]
fn no_op_cancel_and_failure_preserve_native_state_and_private_source() {
    for mode in ["noop", "cancel", "fail", "invalid"] {
        let mut w = Workflow::new();
        w.cached();
        let before = w.source();
        let o = w.edit("candidate\n\nbeta\n", mode, true);
        if ["fail", "invalid"].contains(&mode) {
            assert_eq!(o.status.code(), Some(2));
            assert!(!o.stderr.is_empty());
        } else {
            let r = report(o);
            assert_eq!(
                r["state"],
                if mode == "noop" {
                    "unchanged"
                } else {
                    "cancelled"
                }
            );
        }
        assert_eq!(w.source(), before);
        assert_eq!(w.pending(), 0);
        assert_eq!(w.ops(), "");
        if mode != "noop" {
            assert!(
                host::tree(&w.h.cache())
                    .iter()
                    .any(|(name, _)| name == "source.md")
            );
        }
    }
}
#[test]
fn authorization_and_schema_failures_preserve_existing_cached_work() {
    for mode in ["authorizationFailure", "negotiationIdentity"] {
        let mut w = Workflow::new();
        w.cached();
        if mode == "authorizationFailure" {
            w.fixture[mode] = json!(true);
        } else {
            let mut identity = StoredIdentity::from(&w.h.identity);
            identity.epoch = "ac000000-0000-4000-8000-111111111111".into();
            w.fixture[mode] = json!(identity);
        }
        let o = w.edit("local\n\nbeta\n", "changed", false);
        if mode == "authorizationFailure" {
            assert_eq!(o.status.code(), Some(2));
            assert_eq!(w.source(), "alpha\n\nbeta\n");
        } else {
            let r = report(o);
            assert_eq!(r["status"]["connection"], "incompatible-schema-epoch");
            assert_eq!(w.pending(), 1);
        }
    }
}
#[test]
fn new_editor_session_refuses_to_overwrite_an_unresolved_conflict_draft() {
    let mut w = Workflow::new();
    w.remote("remote\n\nbeta\n");
    report(w.edit("local\n\nbeta\n", "changed", false));
    let before = fs::read(w.h.cache().join("draft.json")).unwrap();
    let o = w.edit("replacement\n", "changed", true);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(fs::read(w.h.cache().join("draft.json")).unwrap(), before);
}

#[test]
fn clearing_an_admitted_draft_preserves_a_newer_writers_draft() {
    let mut w = Workflow::new();
    w.cached();
    let storage = NativeStorage::open(w.h.cache(), w.h.identity.clone()).unwrap();
    let first = editac_cli::storage::Draft {
        baseline: "alpha\n\nbeta\n".into(),
        source: "first\n\nbeta\n".into(),
    };
    let newer = editac_cli::storage::Draft {
        baseline: first.baseline.clone(),
        source: "newer\n\nbeta\n".into(),
    };
    storage.persist_draft(&first).unwrap();
    storage.persist_draft(&newer).unwrap();
    assert!(!storage.clear_draft(&first).unwrap());
    assert_eq!(storage.read_draft().unwrap(), Some(newer.clone()));
    assert!(storage.clear_draft(&newer).unwrap());
    assert_eq!(storage.recover().unwrap().draft, None);
}

#[test]
fn invalid_hosted_options_fail_before_credentials_or_cache_initialization() {
    let w = Workflow::new();
    w.save();
    for flags in [
        &["--attempts", "0"][..],
        &["--offline", "--offline"],
        &["--offline", "--adapter", "/unused"],
    ] {
        let o = w
            .command("edit")
            .args(["--editor", "/bin/false"])
            .args(flags)
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(2));
        assert_eq!(w.ops(), "");
        assert!(!w.h.cache().exists());
    }
}
