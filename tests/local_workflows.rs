mod support;
use std::fs;
use support::{Fixture, json};
#[test]
fn markdown_exact_source_unicode_and_native_recovery() {
    let f = Fixture::new(
        "md",
        "# Old\r\n\r\nHello 🦀\r\n",
        "# New\r\n\r\nHello 👩‍💻 é שלום\r\n",
    );
    let r = json(&f.edit("changed", &[]));
    assert_eq!(r["state"], "locally-saved");
    assert_eq!(f.source(), fs::read_to_string(&f.payload).unwrap());
    let output = f.root.join("recovered.md");
    let r = json(&f.recover(Some(&output)));
    assert_eq!(r["pendingUpdates"], 1);
    assert_eq!(r["remoteDurability"], "not-applicable");
    assert_eq!(fs::read(output).unwrap(), fs::read(&f.payload).unwrap());
}
#[test]
fn literal_text_never_parses_markdown() {
    let f = Fixture::new("txt", "literal", "# *literal* 🦀\n");
    json(&f.edit("changed", &[]));
    assert_eq!(f.source(), "# *literal* 🦀\n");
}
#[test]
fn noop_preserves_bytes_and_file_identity() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new("md", "# Odd  \r\n", "unused");
    let m = fs::metadata(&f.path).unwrap();
    assert_eq!(json(&f.edit("noop", &[]))["state"], "unchanged");
    let n = fs::metadata(&f.path).unwrap();
    assert_eq!(m.ino(), n.ino());
    assert_eq!(m.modified().unwrap(), n.modified().unwrap());
    assert_eq!(f.source(), "# Odd  \r\n");
    assert_eq!(fs::read_dir(&f.recovery).unwrap().count(), 0);
}
#[test]
fn cancellation_retains_exact_editor_draft_without_commit() {
    let f = Fixture::new("txt", "old", "cancelled draft");
    assert_eq!(json(&f.edit("cancel", &[]))["state"], "cancelled");
    assert_eq!(f.source(), "old");
    let p = f.root.join("cancel.txt");
    assert_eq!(json(&f.recover(Some(&p)))["pendingUpdates"], 0);
    assert_eq!(fs::read_to_string(p).unwrap(), "cancelled draft");
}
#[test]
fn editor_failure_is_recoverable_without_commit() {
    let f = Fixture::new("txt", "old", "failed draft");
    let o = f.edit("fail", &[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("recover session"));
    assert_eq!(f.source(), "old");
    let p = f.root.join("failure.txt");
    assert_eq!(json(&f.recover(Some(&p)))["pendingUpdates"], 0);
    assert_eq!(fs::read_to_string(p).unwrap(), "failed draft");
}
#[test]
fn stale_save_retains_native_commit_and_all_source_evidence() {
    let f = Fixture::new("txt", "old", "local draft");
    let o = f.edit("stale", &[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("Stale"));
    assert_eq!(f.source(), "external writer\n");
    let p = f.root.join("conflict.txt");
    assert_eq!(json(&f.recover(Some(&p)))["pendingUpdates"], 1);
    assert_eq!(fs::read_to_string(p).unwrap(), "local draft");
    assert_eq!(f.source(), "external writer\n");
}
#[test]
fn replaced_target_is_never_overwritten() {
    let f = Fixture::new("txt", "old", "local draft");
    let o = f.edit("replace", &[]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(f.source(), "replacement\n");
    json(&f.recover(None));
}
#[test]
fn unsupported_structure_retains_draft_before_semantic_mutation() {
    let f = Fixture::new("md", "one\n", "one\n\ntwo\n");
    assert_eq!(f.edit("changed", &[]).status.code(), Some(2));
    assert_eq!(f.source(), "one\n");
    let p = f.root.join("rejected.md");
    assert_eq!(json(&f.recover(Some(&p)))["pendingUpdates"], 0);
    assert_eq!(fs::read_to_string(p).unwrap(), "one\n\ntwo\n");
}
#[test]
fn interrupted_process_recovers_editor_buffer_from_native_baseline() {
    let f = Fixture::new("txt", "old", "interrupted 🦀");
    assert!(!f.edit("interrupt", &[]).status.success());
    assert_eq!(f.source(), "old");
    let p = f.root.join("interrupted.txt");
    assert_eq!(json(&f.recover(Some(&p)))["pendingUpdates"], 0);
    assert_eq!(fs::read_to_string(p).unwrap(), "interrupted 🦀");
}
#[test]
fn recovery_refuses_existing_destination_and_keeps_session() {
    let f = Fixture::new("txt", "old", "new");
    json(&f.edit("changed", &[]));
    let o = f.recover(Some(&f.path));
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(f.source(), "new");
    json(&f.recover(None));
}
#[test]
fn explicit_creation_and_format_selection_are_offline() {
    let f = Fixture::new("unknown", "", "new text");
    fs::remove_file(&f.path).unwrap();
    assert_eq!(
        f.edit("changed", &["--format", "text"]).status.code(),
        Some(2)
    );
    assert!(!f.path.exists());
    json(&f.edit("changed", &["--create", "--format", "text"]));
    assert_eq!(f.source(), "new text");
}
#[test]
fn invalid_editor_bytes_do_not_commit_or_publish() {
    let f = Fixture::new("txt", "old", "new");
    assert_eq!(f.edit("invalid", &[]).status.code(), Some(2));
    assert_eq!(f.source(), "old");
    assert_eq!(f.recover(None).status.code(), Some(2));
}

#[test]
fn corrupt_native_checkpoint_can_export_independently_validated_draft() {
    let f = Fixture::new("txt", "old", "committed draft");
    json(&f.edit("changed", &[]));
    for name in ["latest.json", "previous.json"] {
        fs::write(f.session().join("state").join(name), b"corrupt").unwrap();
    }
    let p = f.root.join("draft-only.txt");
    let r = json(&f.recover(Some(&p)));
    assert_eq!(r["state"], "draft-recovered");
    assert_eq!(r["committedState"], "unavailable");
    assert!(r["pendingUpdates"].is_null());
    assert_eq!(fs::read_to_string(p).unwrap(), "committed draft");
    assert_eq!(
        fs::read(f.session().join("state/latest.json")).unwrap(),
        b"corrupt"
    );
}
#[test]
fn corrupt_draft_is_not_exported_or_replaced() {
    let f = Fixture::new("txt", "old", "new");
    json(&f.edit("changed", &[]));
    let draft = f.session().join("state/draft.json");
    fs::write(&draft, b"corrupt").unwrap();
    let p = f.root.join("export");
    assert_eq!(f.recover(Some(&p)).status.code(), Some(2));
    assert!(!p.exists());
    assert_eq!(fs::read(draft).unwrap(), b"corrupt");
}
#[test]
fn source_only_spelling_change_keeps_semantics_and_exact_source() {
    let f = Fixture::new("md", "# Old\n", "Old\n===\n");
    json(&f.edit("changed", &[]));
    assert_eq!(f.source(), "Old\n===\n");
    assert_eq!(json(&f.recover(None))["pendingUpdates"], 0);
}
#[test]
fn native_permissions_survive_command_publication() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("txt", "old", "new");
    fs::set_permissions(&f.path, fs::Permissions::from_mode(0o640)).unwrap();
    json(&f.edit("changed", &[]));
    assert_eq!(
        fs::metadata(&f.path).unwrap().permissions().mode() & 0o777,
        0o640
    );
}
#[test]
fn readonly_failure_retains_source_and_committed_draft() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("txt", "old", "new");
    fs::set_permissions(&f.path, fs::Permissions::from_mode(0o400)).unwrap();
    assert_eq!(f.edit("changed", &[]).status.code(), Some(2));
    assert_eq!(f.source(), "old");
    assert_eq!(json(&f.recover(None))["pendingUpdates"], 1);
}

#[test]
fn corrupt_latest_reports_validated_previous_checkpoint_fallback() {
    let f = Fixture::new("txt", "old", "new");
    json(&f.edit("changed", &[]));
    fs::write(f.session().join("state/latest.json"), b"corrupt").unwrap();
    let r = json(&f.recover(None));
    assert_eq!(r["state"], "recovered");
    assert_eq!(r["pendingUpdates"], 1);
    assert!(r["fallback"].is_string());
    assert_eq!(f.source(), "new");
}
