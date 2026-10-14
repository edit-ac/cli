//! CLI-1.5 canonical harness: real resolver -> fake editor -> pinned shared
//! import/prepare/commit -> native files, with deterministic failing I/O seams.
use editac_cli::{
    editor::{self, EditorCommand, EditorOutcome},
    engine::document::{CreationPermit, DocumentIdentity},
    files::{self, FailureKind, Filesystem, NativeFilesystem, SaveFailure, SaveState},
    reconcile::{LocalBaseline, ReconciledEdit},
    target::{LocalTarget, ResolveOptions, resolve},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};
struct Fixture {
    root: PathBuf,
    target: LocalTarget,
    id: usize,
}
impl Fixture {
    fn new(source: Option<&str>, extension: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("editac-files-{}-{id}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let path = root.join(format!("source.{extension}"));
        if let Some(source) = source {
            fs::write(&path, source).unwrap();
        }
        let target = resolve(
            &path,
            ResolveOptions {
                create: source.is_none(),
                ..Default::default()
            },
        )
        .unwrap();
        Self { root, target, id }
    }
    fn command(&self, action: &str, source: &str) -> EditorCommand {
        let draft = self.root.join("fixture-draft");
        fs::write(&draft, source).unwrap();
        let executable =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/editor/fake editor.sh");
        EditorCommand::parse(&format!(
            "'{}' {action} '{}'",
            executable.display(),
            draft.display()
        ))
        .unwrap()
    }
    fn edit(&self, source: &str) -> ReconciledEdit {
        let identity = DocumentIdentity::v5(
            &format!("99999999-9999-4999-8999-{:012x}", self.id),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        )
        .unwrap();
        let (mut document, baseline) = LocalBaseline::import(
            self.target.clone(),
            CreationPermit::authorize_new(identity).unwrap(),
        )
        .unwrap();
        let buffer = match editor::run(&self.target, &self.command("changed", source), &self.root)
            .unwrap()
        {
            EditorOutcome::Changed(buffer) => buffer,
            _ => panic!("expected changed fixture"),
        };
        let plan = baseline.prepare(&document, buffer).unwrap();
        let edit = plan.commit(&mut document).unwrap();
        assert_eq!(edit.buffer.source(), source);
        assert_eq!(
            fs::read_to_string(edit.buffer.recovery().buffer()).unwrap(),
            source
        );
        edit
    }
    fn recovery(&self, edit: &ReconciledEdit, expected: &str) {
        assert_eq!(edit.buffer.source(), expected);
        assert_eq!(
            fs::read_to_string(edit.buffer.recovery().buffer()).unwrap(),
            expected
        );
        assert_eq!(
            fs::read_to_string(edit.buffer.recovery().baseline()).unwrap(),
            self.target.source()
        );
    }
    fn original(&self, source: &str) {
        assert_eq!(
            fs::read_to_string(self.target.resolved_path()).unwrap(),
            source
        );
    }
    fn no_temporary(&self) {
        assert!(!fs::read_dir(&self.root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".editac-save-")
        }));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn failure(result: Result<files::SavedEdit, SaveFailure>, kind: FailureKind) -> SaveFailure {
    let error = match result {
        Ok(_) => panic!("expected {kind:?}"),
        Err(e) => e,
    };
    assert_eq!(error.kind, kind, "{error}");
    error
}

#[test]
fn markdown_unicode_exact_spelling_saved_with_one_shared_history_commit() {
    let f = Fixture::new(Some("# Before\n\nwords\n"), "md");
    let before = fs::metadata(f.target.resolved_path()).unwrap().ino();
    let draft = "# Après 🧑🏽‍💻\n\nwords\n";
    let edit = f.edit(draft);
    assert!(edit.commit.is_some());
    let saved = files::save(edit).unwrap();
    assert_eq!(saved.state, SaveState::Saved);
    assert_eq!(saved.path, f.target.resolved_path());
    assert_ne!(fs::metadata(&saved.path).unwrap().ino(), before);
    f.original(draft);
    f.recovery(&saved.edit, draft);
    f.no_temporary();
}
#[test]
fn literal_crlf_and_mixed_endings_are_not_normalized() {
    let f = Fixture::new(Some("*literal*\r\nline\n"), "txt");
    let draft = "*edited* 🦀\r\nline\n";
    let saved = files::save(f.edit(draft)).unwrap();
    f.original(draft);
    f.recovery(&saved.edit, draft);
}
#[test]
fn source_only_spelling_edit_is_published_even_without_semantic_commit() {
    let f = Fixture::new(Some("**bold**\n"), "md");
    let edit = f.edit("__bold__\n");
    assert!(edit.commit.is_none());
    files::save(edit).unwrap();
    f.original("__bold__\n");
}
#[test]
fn noop_and_cancel_leave_bytes_inode_permissions_and_timestamp_untouched() {
    for action in ["noop", "cancel"] {
        let f = Fixture::new(Some("original\r\n"), "txt");
        let before = fs::metadata(f.target.resolved_path()).unwrap();
        let result = editor::run(&f.target, &f.command(action, "changed"), &f.root).unwrap();
        match (action, result) {
            ("noop", EditorOutcome::Unchanged(_)) | ("cancel", EditorOutcome::Cancelled(_)) => (),
            _ => panic!("wrong outcome"),
        }
        let after = fs::metadata(f.target.resolved_path()).unwrap();
        assert_eq!(before.ino(), after.ino());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_eq!(before.mode(), after.mode());
        assert!(!files::lock_path(f.target.resolved_path()).exists());
        f.original("original\r\n");
        f.no_temporary();
    }
}
#[test]
fn explicitly_created_target_publishes_without_replacing_any_file() {
    let f = Fixture::new(None, "txt");
    let saved = files::save(f.edit("new 🦀\r\n")).unwrap();
    assert_eq!(saved.state, SaveState::Saved);
    f.original("new 🦀\r\n");
    f.no_temporary();
    assert_eq!(fs::metadata(&saved.path).unwrap().mode() & 0o777, 0o600);
}
#[test]
fn permissions_are_preserved_on_replacement() {
    let f = Fixture::new(Some("before"), "txt");
    fs::set_permissions(f.target.resolved_path(), fs::Permissions::from_mode(0o640)).unwrap();
    let before = fs::metadata(f.target.resolved_path()).unwrap();
    files::save(f.edit("after")).unwrap();
    let after = fs::metadata(f.target.resolved_path()).unwrap();
    assert_eq!((before.uid(), before.gid()), (after.uid(), after.gid()));
    assert_eq!(
        fs::metadata(f.target.resolved_path()).unwrap().mode() & 0o777,
        0o640
    );
}
#[test]
fn changed_content_with_same_length_and_inode_is_stale() {
    let f = Fixture::new(Some("before"), "txt");
    let edit = f.edit("draft!");
    fs::write(f.target.resolved_path(), "remote").unwrap();
    let e = failure(files::save(edit), FailureKind::Stale);
    f.original("remote");
    f.recovery(&e.edit, "draft!");
    f.no_temporary();
}
#[test]
fn replaced_identity_even_with_identical_source_is_rejected() {
    let f = Fixture::new(Some("before"), "txt");
    let edit = f.edit("after");
    let replacement = f.root.join("replacement");
    fs::write(&replacement, "before").unwrap();
    fs::rename(replacement, f.target.resolved_path()).unwrap();
    let e = failure(files::save(edit), FailureKind::TargetChanged);
    f.original("before");
    f.recovery(&e.edit, "after");
}
#[test]
fn removed_target_is_not_silently_recreated() {
    let f = Fixture::new(Some("before"), "txt");
    let edit = f.edit("after");
    fs::remove_file(f.target.resolved_path()).unwrap();
    let e = failure(files::save(edit), FailureKind::TargetChanged);
    assert!(!f.target.resolved_path().exists());
    f.recovery(&e.edit, "after");
}
#[test]
fn changed_final_symlink_never_overwrites_referent() {
    let f = Fixture::new(Some("before"), "txt");
    let edit = f.edit("after");
    let other = f.root.join("other");
    fs::write(&other, "protected").unwrap();
    fs::remove_file(f.target.resolved_path()).unwrap();
    symlink(&other, f.target.resolved_path()).unwrap();
    let e = failure(files::save(edit), FailureKind::TargetChanged);
    assert_eq!(fs::read_to_string(other).unwrap(), "protected");
    f.recovery(&e.edit, "after");
}
#[test]
fn parent_symlink_retarget_is_rejected() {
    let mut f = Fixture::new(Some("before"), "txt");
    let old = f.root.join("old");
    let new = f.root.join("new");
    fs::create_dir(&old).unwrap();
    fs::create_dir(&new).unwrap();
    fs::write(old.join("source.txt"), "before").unwrap();
    fs::write(new.join("source.txt"), "other").unwrap();
    let alias = f.root.join("alias");
    symlink(&old, &alias).unwrap();
    f.target = resolve(alias.join("source.txt"), ResolveOptions::default()).unwrap();
    let edit = f.edit("after");
    fs::remove_file(&alias).unwrap();
    symlink(&new, &alias).unwrap();
    let e = failure(files::save(edit), FailureKind::TargetChanged);
    assert_eq!(fs::read_to_string(new.join("source.txt")).unwrap(), "other");
    f.original("before");
    f.recovery(&e.edit, "after");
}
#[test]
fn target_created_since_open_is_rejected() {
    let f = Fixture::new(None, "txt");
    let edit = f.edit("draft");
    fs::write(f.target.resolved_path(), "someone else").unwrap();
    let e = failure(files::save(edit), FailureKind::TargetChanged);
    f.original("someone else");
    f.recovery(&e.edit, "draft");
}
#[test]
fn cooperating_lock_is_exclusive_and_persistent_then_releases_on_failure() {
    let f = Fixture::new(Some("before"), "txt");
    let path = files::lock_path(f.target.resolved_path());
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    lock.lock().unwrap();
    let inode = lock.metadata().unwrap().ino();
    let e = failure(files::save(f.edit("after")), FailureKind::Busy);
    f.original("before");
    f.recovery(&e.edit, "after");
    drop(lock);
    files::save(e.edit).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    let released = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    released.try_lock().unwrap();
}
#[test]
fn hostile_lock_symlink_is_rejected_without_touching_referent() {
    let f = Fixture::new(Some("before"), "txt");
    let other = f.root.join("protected");
    fs::write(&other, "protected").unwrap();
    symlink(&other, files::lock_path(f.target.resolved_path())).unwrap();
    let e = failure(files::save(f.edit("after")), FailureKind::TargetChanged);
    f.original("before");
    f.recovery(&e.edit, "after");
    assert_eq!(fs::read_to_string(other).unwrap(), "protected");
}
#[test]
fn read_only_file_preserves_original_and_recovery() {
    let f = Fixture::new(Some("before"), "txt");
    let edit = f.edit("after");
    fs::set_permissions(f.target.resolved_path(), fs::Permissions::from_mode(0o444)).unwrap();
    let e = failure(files::save(edit), FailureKind::ReadOnly);
    f.original("before");
    f.recovery(&e.edit, "after");
    f.no_temporary();
}
#[test]
fn read_only_directory_preserves_original_and_recovery() {
    let f = Fixture::new(Some("before"), "txt");
    let edit = f.edit("after");
    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o555)).unwrap();
    let result = files::save(edit);
    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o700)).unwrap();
    let e = failure(result, FailureKind::ReadOnly);
    f.original("before");
    f.recovery(&e.edit, "after");
    f.no_temporary();
}

#[derive(Default)]
struct Probe {
    events: Vec<&'static str>,
    fault: Option<&'static str>,
    directories: usize,
    mutate: Option<PathBuf>,
}
impl Filesystem for Probe {
    fn write_all(&mut self, file: &mut File, source: &[u8]) -> io::Result<()> {
        self.events.push("write");
        if self.fault == Some("full") {
            file.write_all(&source[..source.len() / 2])?;
            return Err(io::Error::from_raw_os_error(28));
        }
        file.write_all(source)
    }
    fn sync_file(&mut self, file: &File) -> io::Result<()> {
        self.events.push("file-sync");
        if self.fault == Some("file-sync") {
            return Err(io::Error::other("prepared file-sync failure"));
        }
        file.sync_all()
    }
    fn sync_directory(&mut self, file: &File) -> io::Result<()> {
        self.directories += 1;
        self.events.push(if self.directories == 1 {
            "directory-preflight"
        } else {
            "directory-durable"
        });
        if self.fault
            == Some(if self.directories == 1 {
                "directory-preflight"
            } else {
                "directory-durable"
            })
        {
            return Err(io::Error::other("prepared directory-sync failure"));
        }
        if let Some(path) = self.mutate.take() {
            fs::write(path, "external").unwrap();
        }
        file.sync_all()
    }
    fn publish(&mut self, temporary: &Path, destination: &Path, replace: bool) -> io::Result<()> {
        self.events.push("publish");
        assert_eq!(temporary.parent(), destination.parent());
        if self.fault == Some("publish-before") {
            return Err(io::Error::other("prepared pre-rename failure"));
        }
        if self.fault == Some("create-race") {
            fs::write(destination, "external")?;
        }
        NativeFilesystem.publish(temporary, destination, replace)?;
        if self.fault == Some("publish-after") {
            return Err(io::Error::other("prepared post-rename failure"));
        }
        Ok(())
    }
}
#[test]
fn native_publication_order_and_ack_follow_directory_durability() {
    let f = Fixture::new(Some("before"), "txt");
    let mut probe = Probe::default();
    let saved = files::save_with(f.edit("after"), None, false, &mut probe).unwrap();
    assert_eq!(
        probe.events,
        [
            "write",
            "file-sync",
            "directory-preflight",
            "publish",
            "directory-durable"
        ]
    );
    assert_eq!(saved.state, SaveState::Saved);
    f.original("after");
}
#[test]
fn full_disk_partial_write_preserves_original_draft_and_releases_lock() {
    let f = Fixture::new(Some("before"), "txt");
    let mut probe = Probe {
        fault: Some("full"),
        ..Default::default()
    };
    let e = failure(
        files::save_with(f.edit("after"), None, false, &mut probe),
        FailureKind::Io,
    );
    assert_eq!(e.cause.as_ref().unwrap().raw_os_error(), Some(28));
    assert_eq!(probe.events, ["write"]);
    f.original("before");
    f.recovery(&e.edit, "after");
    f.no_temporary();
    let lock = OpenOptions::new()
        .write(true)
        .open(files::lock_path(f.target.resolved_path()))
        .unwrap();
    lock.try_lock().unwrap();
}
#[test]
fn file_and_directory_preflight_sync_failures_do_not_publish() {
    for fault in ["file-sync", "directory-preflight"] {
        let f = Fixture::new(Some("before"), "txt");
        let mut probe = Probe {
            fault: Some(fault),
            ..Default::default()
        };
        let e = failure(
            files::save_with(f.edit("after"), None, false, &mut probe),
            FailureKind::Io,
        );
        assert!(!probe.events.contains(&"publish"));
        f.original("before");
        f.recovery(&e.edit, "after");
        f.no_temporary();
    }
}
#[test]
fn prepublication_and_postpublication_errors_report_uncertainty_with_recovery() {
    for fault in ["publish-before", "publish-after", "directory-durable"] {
        let f = Fixture::new(Some("before"), "txt");
        let mut probe = Probe {
            fault: Some(fault),
            ..Default::default()
        };
        let e = failure(
            files::save_with(f.edit("after"), None, false, &mut probe),
            FailureKind::PublicationUncertain,
        );
        f.original(if fault == "publish-before" {
            "before"
        } else {
            "after"
        });
        f.recovery(&e.edit, "after");
        f.no_temporary();
    }
}
#[test]
fn external_write_during_staging_is_revalidated_before_publish() {
    let f = Fixture::new(Some("before"), "txt");
    let mut probe = Probe {
        mutate: Some(f.target.resolved_path().to_owned()),
        ..Default::default()
    };
    let e = failure(
        files::save_with(f.edit("after"), None, false, &mut probe),
        FailureKind::Stale,
    );
    assert!(!probe.events.contains(&"publish"));
    f.original("external");
    f.recovery(&e.edit, "after");
    f.no_temporary();
}
#[test]
fn missing_target_publication_does_not_clobber_racing_creation() {
    let f = Fixture::new(None, "txt");
    let mut probe = Probe {
        fault: Some("create-race"),
        ..Default::default()
    };
    let e = failure(
        files::save_with(f.edit("after"), None, false, &mut probe),
        FailureKind::PublicationUncertain,
    );
    f.original("external");
    f.recovery(&e.edit, "after");
    f.no_temporary();
}
#[test]
fn different_existing_export_requires_explicit_overwrite_and_retains_permissions() {
    let f = Fixture::new(Some("before"), "txt");
    let destination = f.root.join("export.txt");
    fs::write(&destination, "unrelated").unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o640)).unwrap();
    let e = failure(
        files::export(f.edit("after"), &destination, false),
        FailureKind::OverwriteRequired,
    );
    assert_eq!(fs::read_to_string(&destination).unwrap(), "unrelated");
    f.recovery(&e.edit, "after");
    files::export(e.edit, &destination, true).unwrap();
    assert_eq!(fs::read_to_string(&destination).unwrap(), "after");
    assert_eq!(fs::metadata(destination).unwrap().mode() & 0o777, 0o640);
    f.original("before");
}
#[test]
fn missing_export_is_exclusively_created_without_modifying_opened_file() {
    let f = Fixture::new(Some("before"), "txt");
    let destination = f.root.join("export.txt");
    files::export(f.edit("after"), &destination, false).unwrap();
    assert_eq!(fs::read_to_string(destination).unwrap(), "after");
    f.original("before");
}
#[test]
fn alias_export_overwrite_cannot_bypass_original_baseline() {
    let f = Fixture::new(Some("before"), "txt");
    let edit = f.edit("after");
    let alias = f.root.join("alias");
    symlink(&f.root, &alias).unwrap();
    fs::write(f.target.resolved_path(), "external").unwrap();
    let e = failure(
        files::export(edit, &alias.join("source.txt"), true),
        FailureKind::Stale,
    );
    f.original("external");
    f.recovery(&e.edit, "after");
}
#[test]
fn existing_export_symlink_is_never_followed_even_with_overwrite() {
    let f = Fixture::new(Some("before"), "txt");
    let destination = f.root.join("export.txt");
    let other = f.root.join("protected");
    fs::write(&other, "protected").unwrap();
    symlink(&other, &destination).unwrap();
    let e = failure(
        files::export(f.edit("after"), &destination, true),
        FailureKind::Io,
    );
    assert_eq!(fs::read_to_string(other).unwrap(), "protected");
    f.original("before");
    f.recovery(&e.edit, "after");
}
