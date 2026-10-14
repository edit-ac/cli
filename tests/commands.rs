mod support;
use std::process::Command;
use support::{Fixture, json};
fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_editac"))
        .env_clear()
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn missing_editor_configuration_has_actionable_error_and_no_mutation() {
    let f = Fixture::new("txt", "old", "new");
    let o = Command::new(env!("CARGO_BIN_EXE_editac"))
        .env_clear()
        .arg("edit")
        .arg(&f.path)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("VISUAL or EDITOR"));
    assert_eq!(f.source(), "old");
    assert!(!f.recovery.exists());
}
#[test]
fn option_errors_do_not_launch_editor() {
    let f = Fixture::new("txt", "old", "new");
    for flags in [
        &["--create", "--create"][..],
        &["--format", "json"],
        &["--overwrite"],
        &["--editor"],
    ] {
        assert_eq!(f.edit("changed", flags).status.code(), Some(2));
        assert_eq!(f.source(), "old");
    }
    assert!(!f.recovery.exists());
}
#[test]
fn hosted_commands_require_explicit_configuration() {
    for args in [
        &["sync", "https://edit.ac/document"][..],
        &["status", "https://edit.ac/document"],
        &["edit", "https://edit.ac/document.md"],
    ] {
        assert_eq!(run(args).status.code(), Some(2));
    }
}
#[test]
fn help_advertises_local_and_configured_hosted_commands() {
    let o = run(&["--help"]);
    assert!(o.status.success());
    let s = String::from_utf8(o.stdout).unwrap();
    assert!(s.contains("edit <path>"));
    assert!(s.contains("recover <session-directory>"));
    assert!(s.contains("editac sync"));
    assert!(s.contains("editac status"));
}
#[test]
fn visual_selection_works_without_account_or_network_environment() {
    let f = Fixture::new("txt", "old", "new");
    let o = Command::new(env!("CARGO_BIN_EXE_editac"))
        .env_clear()
        .env("VISUAL", f.command("changed"))
        .env("EDITOR", "/invalid")
        .arg("edit")
        .arg(&f.path)
        .arg("--recovery-dir")
        .arg(&f.recovery)
        .output()
        .unwrap();
    assert_eq!(json(&o)["state"], "locally-saved");
    assert_eq!(f.source(), "new");
}
#[test]
fn missing_recovery_session_does_not_create_it() {
    let f = Fixture::new("txt", "old", "new");
    let o = Command::new(env!("CARGO_BIN_EXE_editac"))
        .env_clear()
        .arg("recover")
        .arg(&f.recovery)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(!f.recovery.exists());
}
#[test]
fn unsafe_recovery_root_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("txt", "old", "new");
    std::fs::create_dir(&f.recovery).unwrap();
    std::fs::set_permissions(&f.recovery, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(f.edit("changed", &[]).status.code(), Some(2));
    assert_eq!(f.source(), "old");
}

#[test]
fn recovery_missing_native_state_does_not_initialize_replacement() {
    let f = Fixture::new("txt", "old", "new");
    json(&f.edit("changed", &[]));
    let state = f.session().join("state");
    std::fs::remove_dir_all(&state).unwrap();
    assert_eq!(f.recover(None).status.code(), Some(2));
    assert!(!state.exists());
    assert_eq!(f.source(), "new");
}
#[test]
fn native_filename_policy_is_explicit_without_replacement() {
    use std::os::unix::ffi::OsStringExt;
    #[allow(unused_mut)]
    let mut f = Fixture::new("txt", "old", "new");
    let path = f
        .root
        .join(std::ffi::OsString::from_vec(b"file-\xff.txt".to_vec()));
    #[cfg(target_os = "macos")]
    {
        // APFS rejects invalid-byte names at the OS boundary, not a CLI skip.
        assert!(std::fs::rename(&f.path, &path).is_err());
        let o = Command::new(env!("CARGO_BIN_EXE_editac"))
            .env_clear()
            .arg("edit")
            .arg(&path)
            .args(["--format", "text", "--editor", "/bin/false"])
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(2));
        assert_eq!(f.source(), "old");
        assert!(!f.recovery.exists());
        return;
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::rename(&f.path, &path).unwrap();
        f.path = path;
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Explicit editor parameters stay Unicode, while the target is passed as an OS argument.
        let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/editor/fake editor.sh");
        let command = format!(
            "/bin/sh '{}' changed '{}'",
            script.display(),
            f.payload.display()
        );
        let o = Command::new(env!("CARGO_BIN_EXE_editac"))
            .env_clear()
            .arg("edit")
            .arg(&f.path)
            .args(["--editor", &command, "--recovery-dir"])
            .arg(&f.recovery)
            .output()
            .unwrap();
        assert_eq!(json(&o)["state"], "locally-saved");
        assert_eq!(f.source(), "new");
        json(&f.recover(None));
    }
}
