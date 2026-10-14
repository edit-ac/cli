//! CLI-1.9 explicit native commands over the real cache and prepared host adapter.
#[path = "support/sync.rs"]
mod harness;
use editac_cli::{status, storage::StoredIdentity};
use harness::{Harness, ReceiptMode};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_editac"))
        .args(args)
        .env_clear()
        .output()
        .unwrap()
}
fn config(h: &Harness) -> std::path::PathBuf {
    let path = h.host.root.join("service.json");
    fs::write(&path,serde_json::to_vec(&serde_json::json!({"host":"app.example.test","endpoint":"https://api.example.test/documents","identity":StoredIdentity::from(&h.host.identity)})).unwrap()).unwrap();
    path
}
fn adapter(h: &Harness, error: Option<&str>) -> std::path::PathBuf {
    adapter_frames(h, error, vec![])
}
fn adapter_frames(
    h: &Harness,
    error: Option<&str>,
    batches: Vec<Vec<Vec<u8>>>,
) -> std::path::PathBuf {
    let path = h.host.root.join("prepared adapter.py");
    let log = h.host.root.join("adapter.log");
    let program = format!(
        r#"#!/usr/bin/python3
import json,sys,hashlib,os
r=json.load(sys.stdin)
op=r['operation']
with open({log},'a') as f: f.write(op+'\n')
error={error}
if op=='credential': result='prepared-command-secret'
elif op=='connect': result={{'identity':r['identity'],'receiptProtocol':'editac-durable-receipt-v1'}}
elif op=='exchange':
    batches={batches}
    counter={counter}
    n=int(open(counter).read()) if os.path.exists(counter) else 0
    with open(counter,'w') as f: f.write(str(n+1))
    result=batches[n] if n<len(batches) else []
elif op=='upload':
    if error: print(json.dumps({{'error':error}})); sys.exit(0)
    e=r['entry']
    assert hashlib.sha256(bytes(e['bytes'])).hexdigest()==e['checksum']
    result={{'receipt':{{'protocol':'editac-durable-receipt-v1','state':'durable',**{{k:e[k] for k in ['identity','writerId','sequence','checksum','updateId']}}}}}}
else: result=None
print(json.dumps({{'ok':result}}))
"#,
        log = serde_json::to_string(&log.to_str().unwrap()).unwrap(),
        batches = serde_json::to_string(&batches).unwrap(),
        counter =
            serde_json::to_string(&h.host.root.join("exchange-count").to_str().unwrap()).unwrap(),
        error = error.map(|s| format!("'{s}'")).unwrap_or("None".into())
    );
    fs::write(&path, program).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    path
}
fn output(o: Output) -> serde_json::Value {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(o.stderr.is_empty());
    assert!(!String::from_utf8_lossy(&o.stdout).contains("prepared-command-secret"));
    serde_json::from_slice(&o.stdout).unwrap()
}
fn strpath(p: &Path) -> &str {
    p.to_str().unwrap()
}
#[test]
fn offline_status_distinguishes_native_saved_pending_and_remote_unknown() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    let c = config(&h);
    let before = h.host.bytes();
    let s = output(run(&[
        "status",
        &h.host.url,
        "--service-config",
        strpath(&c),
        "--cache-dir",
        strpath(&h.host.root),
    ]));
    assert_eq!(s["locallySaved"], true);
    assert_eq!(s["pendingUpload"], 1);
    assert_eq!(s["remoteDurability"], "unknown");
    assert_eq!(s["connection"], "offline");
    assert_eq!(h.host.bytes(), before);
    assert!(!h.host.root.join("adapter.log").exists());
}
#[test]
fn explicit_sync_command_uses_restored_native_outbox_and_exact_receipt() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    let c = config(&h);
    let a = adapter(&h, None);
    let report = output(run(&[
        "sync",
        &h.host.url,
        "--service-config",
        strpath(&c),
        "--cache-dir",
        strpath(&h.host.root),
        "--adapter",
        strpath(&a),
    ]));
    assert_eq!(report["status"]["pendingUpload"], 0);
    assert_eq!(report["status"]["remoteDurability"], "remotely-committed");
    assert_eq!(report["attempts"], 1);
    let log = fs::read_to_string(h.host.root.join("adapter.log")).unwrap();
    assert_eq!(log, "credential\nconnect\nexchange\nupload\ndisconnect\n");
    let s = output(run(&[
        "status",
        &h.host.url,
        "--service-config",
        strpath(&c),
        "--cache-dir",
        strpath(&h.host.root),
    ]));
    assert_eq!(s["remoteDurability"], "remotely-committed");
    assert_eq!(
        fs::read_to_string(h.host.root.join("adapter.log")).unwrap(),
        log
    );
    assert_eq!(h.reopened().recovery.tail.len(), 1);
    assert!(h.reopened().recovery.pending.is_empty());
}
#[test]
fn command_revocation_preserves_native_outbox_and_reports_authorization_loss() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    let pending = h.session.storage.recover().unwrap().pending;
    let c = config(&h);
    let a = adapter(&h, Some("revoked"));
    let report = output(run(&[
        "sync",
        &h.host.url,
        "--service-config",
        strpath(&c),
        "--cache-dir",
        strpath(&h.host.root),
        "--adapter",
        strpath(&a),
    ]));
    assert_eq!(report["status"]["connection"], "revoked");
    assert_eq!(report["status"]["pendingUpload"], 1);
    assert_eq!(report["attempts"], 1);
    assert_eq!(h.reopened().recovery.pending, pending);
}
#[test]
fn command_validation_never_creates_an_empty_cache_or_accesses_adapter() {
    let h = Harness::new();
    let c = config(&h);
    let a = adapter(&h, None);
    for extra in [
        vec!["--attempts", "0"],
        vec!["--attempts", "6"],
        vec!["--attempts", "1", "--attempts", "2"],
    ] {
        let mut args = vec![
            "sync",
            &h.host.url,
            "--service-config",
            strpath(&c),
            "--cache-dir",
            strpath(&h.host.root),
            "--adapter",
            strpath(&a),
        ];
        args.extend(extra);
        let o = run(&args);
        assert_eq!(o.status.code(), Some(2));
        assert!(!h.host.root.join("adapter.log").exists());
    }
    let absent = h.host.root.join("absent");
    fs::create_dir(&absent).unwrap();
    let o = run(&[
        "sync",
        &h.host.url,
        "--service-config",
        strpath(&c),
        "--cache-dir",
        strpath(&absent),
        "--adapter",
        strpath(&a),
    ]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(fs::read_dir(absent).unwrap().count(), 0);
}
#[test]
fn local_file_status_requires_neither_service_nor_credentials_and_is_truthful() {
    let h = Harness::new();
    let source = h.host.root.join("local.md");
    fs::write(&source, "ordinary file\n").unwrap();
    let s = output(run(&["status", strpath(&source)]));
    assert_eq!(s["sourceAvailable"], true);
    assert_eq!(s["localDurability"], "unknown");
    assert_eq!(s["pendingUpload"], 0);
    assert_eq!(s["remoteDurability"], "unknown");
    assert_eq!(fs::read_to_string(source).unwrap(), "ordinary file\n");
}
#[test]
fn missing_receipts_never_become_saved_on_status_reopen() {
    let mut h = Harness::new();
    h.local("changed\n\nbeta\n");
    h.transport.receipt_mode = ReceiptMode::Missing;
    h.run();
    let s = status::inspect(&h.reopened()).unwrap();
    assert_eq!(s.pending_upload, 1);
    assert_eq!(s.remote_durability, "unknown");
    assert!(s.locally_saved);
}
#[test]
fn unsaved_live_changes_are_distinct_from_saved_recovery_on_disk() {
    let mut h = Harness::new();
    h.edit("uncommitted durability\n\nbeta\n");
    let live = status::inspect(&h.session).unwrap();
    assert!(!live.locally_saved);
    assert!(live.unresolved_draft);
    let disk = status::inspect(&h.reopened()).unwrap();
    assert!(disk.locally_saved);
    assert!(disk.unresolved_draft);
    assert_eq!(disk.remote_durability, "unknown");
}
#[test]
fn conflict_draft_and_export_remain_available_to_offline_status() {
    let mut h = Harness::new();
    let base = h.session.source_baseline().unwrap();
    let u = harness::peer_edit(&mut h.transport.peer, "remote ");
    h.session.apply_received(&u).unwrap();
    h.session
        .storage
        .persist_update(&h.session.recovery.document, &u, 1, "editac:remote:sync")
        .unwrap();
    assert!(
        h.session
            .prepare_source(&base, "changed alpha\n\nbeta\n")
            .is_err()
    );
    let s = status::inspect(&h.session).unwrap();
    assert!(s.unresolved_draft);
    assert!(s.conflict);
    assert!(
        h.session
            .storage
            .read_draft()
            .unwrap()
            .unwrap()
            .source
            .contains("changed alpha")
    );
}

#[test]
fn configured_command_persists_reordered_remote_frames_before_exact_local_receipts() {
    let mut h = Harness::new();
    h.local("alpha\n\nlocal beta\n");
    let first = harness::peer_edit(&mut h.transport.peer, "first ");
    let second = harness::peer_edit(&mut h.transport.peer, "second ");
    let a = adapter_frames(
        &h,
        None,
        vec![
            vec![editac_cli::sync::frame(2, &second.bytes).unwrap()],
            vec![
                editac_cli::sync::frame(2, &first.bytes).unwrap(),
                editac_cli::sync::frame(2, &second.bytes).unwrap(),
            ],
        ],
    );
    let c = config(&h);
    let result = output(run(&[
        "sync",
        &h.host.url,
        "--service-config",
        strpath(&c),
        "--cache-dir",
        strpath(&h.host.root),
        "--adapter",
        strpath(&a),
    ]));
    assert_eq!(result["status"]["connection"], "connected");
    assert_eq!(result["status"]["locallySaved"], true);
    assert_eq!(result["receivedUpdates"], 2);
    assert_eq!(result["status"]["pendingUpload"], 0);
    assert_eq!(result["status"]["remoteCommittedUpdates"], 1);
    let reopened = h.reopened();
    let source = reopened.source_baseline().unwrap().opening().source.clone();
    assert!(
        source.contains("second first alpha") && source.contains("local beta"),
        "{source}"
    );
    let log = fs::read_to_string(h.host.root.join("adapter.log")).unwrap();
    assert_eq!(log.lines().filter(|op| *op == "upload").count(), 1);
    assert_eq!(reopened.recovery.tail.len(), 3);
}
