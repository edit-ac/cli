#![cfg(unix)]

use editac_cli::editor::{self, EditorCommand, EditorOutcome, RecoveryFiles};
use editac_cli::target::{LocalTarget, ResolveOptions, resolve};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const ORIGINAL: &str = "\u{feff}# 🦀 e\u{301} مرحبا\r\n\r\n*exact*  \r\n";

struct Fixture {
    root: PathBuf,
    original: PathBuf,
    target: LocalTarget,
    before: fs::Metadata,
}
impl Fixture {
    fn new(extension: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "editac-editor-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let original = root.join(format!("source ; literal.{extension}"));
        fs::write(&original, ORIGINAL).unwrap();
        let before = fs::metadata(&original).unwrap();
        let target = resolve(&original, ResolveOptions::default()).unwrap();
        Self {
            root,
            original,
            target,
            before,
        }
    }
    fn command(&self, mode: &str) -> EditorCommand {
        EditorCommand::parse(&format!(
            "'{}' {mode} '{}'",
            fixture_path("fake editor.sh").display(),
            fixture_path("changed.md").display()
        ))
        .unwrap()
    }
    fn preserved(&self, recovery: &RecoveryFiles) {
        assert_eq!(fs::read(&self.original).unwrap(), ORIGINAL.as_bytes());
        let after = fs::metadata(&self.original).unwrap();
        assert_eq!(self.before.ino(), after.ino());
        assert_eq!(self.before.modified().unwrap(), after.modified().unwrap());
        assert_eq!(self.before.permissions().mode(), after.permissions().mode());
        assert_eq!(fs::read(recovery.baseline()).unwrap(), ORIGINAL.as_bytes());
        assert!(recovery.buffer().is_absolute());
        assert!(recovery.directory().starts_with(&self.root));
        assert_eq!(
            fs::metadata(recovery.directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(recovery.baseline())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/editor")
        .join(name)
}
fn recovery(outcome: &EditorOutcome) -> &RecoveryFiles {
    match outcome {
        EditorOutcome::Changed(changed) => changed.recovery(),
        EditorOutcome::Unchanged(files) | EditorOutcome::Cancelled(files) => files,
    }
}

#[test]
fn successful_changed_markdown_alone_yields_a_preparation_candidate() {
    let f = Fixture::new("md");
    let result = editor::run(&f.target, &f.command("changed"), &f.root).unwrap();
    f.preserved(recovery(&result));
    let EditorOutcome::Changed(changed) = result else {
        panic!("expected changed source")
    };
    assert_eq!(
        changed.source().as_bytes(),
        fs::read(fixture_path("changed.md")).unwrap()
    );
    assert_eq!(changed.target().source(), ORIGINAL);
    assert_eq!(changed.target().baseline(), f.target.baseline());
    assert_eq!(changed.target().resolved_path(), f.original);
    assert_eq!(
        fs::metadata(changed.recovery().buffer())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let (_, source, files) = changed.into_parts();
    assert_eq!(source.as_bytes(), fs::read(files.buffer()).unwrap());
}

#[test]
fn no_op_preserves_original_spelling_identity_and_line_endings() {
    let f = Fixture::new("md");
    let result = editor::run(&f.target, &f.command("noop"), &f.root).unwrap();
    assert!(matches!(result, EditorOutcome::Unchanged(_)));
    f.preserved(recovery(&result));
    assert_eq!(
        fs::read(recovery(&result).buffer()).unwrap(),
        ORIGINAL.as_bytes()
    );
}

#[test]
fn literal_text_stays_literal_with_unicode_and_crlf() {
    let f = Fixture::new("txt");
    let payload = f.root.join("literal.txt");
    let text = "<script>literal</script> **🦀**\r\n\0";
    fs::write(&payload, text).unwrap();
    let command = EditorCommand::parse(&format!(
        "'{}' changed '{}'",
        fixture_path("fake editor.sh").display(),
        payload.display()
    ))
    .unwrap();
    let result = editor::run(&f.target, &command, &f.root).unwrap();
    f.preserved(recovery(&result));
    let EditorOutcome::Changed(changed) = result else {
        panic!("expected change")
    };
    assert_eq!(changed.source(), text);
    assert_eq!(changed.recovery().buffer().extension().unwrap(), "txt");
}

#[test]
fn cancelled_modified_buffer_is_recoverable_but_never_a_candidate() {
    let f = Fixture::new("md");
    let result = editor::run(&f.target, &f.command("cancel"), &f.root).unwrap();
    assert!(matches!(result, EditorOutcome::Cancelled(_)));
    f.preserved(recovery(&result));
    assert_eq!(
        fs::read(recovery(&result).buffer()).unwrap(),
        fs::read(fixture_path("changed.md")).unwrap()
    );
}

#[test]
fn sigint_is_cancellation_without_target_mutation() {
    let f = Fixture::new("md");
    let result = editor::run(&f.target, &f.command("signal-cancel"), &f.root).unwrap();
    assert!(matches!(result, EditorOutcome::Cancelled(_)));
    f.preserved(recovery(&result));
}

#[test]
fn failure_keeps_changed_source_and_baseline_after_error_is_dropped() {
    let f = Fixture::new("md");
    let error = editor::run(&f.target, &f.command("fail"), &f.root).unwrap_err();
    assert_eq!(error.code(), "editor_exit");
    assert!(error.to_string().contains("exit status: 7"));
    assert!(error.to_string().contains("draft:"));
    let files = error.recovery().unwrap().clone();
    drop(error);
    f.preserved(&files);
    assert_eq!(
        fs::read(files.buffer()).unwrap(),
        fs::read(fixture_path("changed.md")).unwrap()
    );
}

#[test]
fn signal_failure_retains_both_sources() {
    let f = Fixture::new("md");
    let error = editor::run(&f.target, &f.command("signal-fail"), &f.root).unwrap_err();
    assert_eq!(error.code(), "editor_exit");
    f.preserved(error.recovery().unwrap());
}

#[test]
fn launch_failure_keeps_private_draft_and_actionable_error() {
    let f = Fixture::new("md");
    let command =
        EditorCommand::parse(&format!("'{}'", f.root.join("missing editor").display())).unwrap();
    let error = editor::run(&f.target, &command, &f.root).unwrap_err();
    assert_eq!(error.code(), "editor_launch");
    assert!(
        error
            .to_string()
            .contains("check its executable and arguments")
    );
    f.preserved(error.recovery().unwrap());
    assert_eq!(
        fs::read(error.recovery().unwrap().buffer()).unwrap(),
        ORIGINAL.as_bytes()
    );
}

#[test]
fn invalid_utf8_retains_raw_failed_buffer() {
    let f = Fixture::new("md");
    let error = editor::run(&f.target, &f.command("invalid"), &f.root).unwrap_err();
    assert_eq!(error.code(), "editor_utf8");
    f.preserved(error.recovery().unwrap());
    assert_eq!(fs::read(error.recovery().unwrap().buffer()).unwrap(), [255]);
}

#[test]
fn oversized_successful_buffer_cannot_advance() {
    let f = Fixture::new("md");
    let error = editor::run(&f.target, &f.command("oversized"), &f.root).unwrap_err();
    assert_eq!(error.code(), "editor_buffer");
    assert!(error.to_string().contains("1 MiB"));
    f.preserved(error.recovery().unwrap());
    assert_eq!(
        fs::metadata(error.recovery().unwrap().buffer())
            .unwrap()
            .len(),
        1048577
    );
}

#[test]
fn deleted_directory_and_symlink_buffers_fail_safely() {
    for mode in ["missing", "directory", "symlink"] {
        let f = Fixture::new("md");
        let error = editor::run(&f.target, &f.command(mode), &f.root).unwrap_err();
        assert_eq!(error.code(), "editor_buffer", "{mode}");
        f.preserved(error.recovery().unwrap());
    }
}

#[test]
fn creation_stays_pending_even_after_successful_editor() {
    let f = Fixture::new("md");
    let missing = f.root.join("new.md");
    let target = resolve(
        &missing,
        ResolveOptions {
            create: true,
            format: None,
        },
    )
    .unwrap();
    let result = editor::run(&target, &f.command("changed"), &f.root).unwrap();
    assert!(matches!(result, EditorOutcome::Changed(_)));
    assert!(!missing.exists());
    assert_eq!(fs::read(recovery(&result).baseline()).unwrap(), b"");
}

#[test]
fn separate_sessions_never_share_live_working_files() {
    let f = Fixture::new("md");
    let a = editor::run(&f.target, &f.command("noop"), &f.root).unwrap();
    let b = editor::run(&f.target, &f.command("changed"), &f.root).unwrap();
    assert_ne!(recovery(&a).directory(), recovery(&b).directory());
    f.preserved(recovery(&a));
    f.preserved(recovery(&b));
    assert_eq!(
        fs::read(recovery(&a).buffer()).unwrap(),
        ORIGINAL.as_bytes()
    );
}

#[test]
fn cleanup_requires_explicit_discard_and_keeps_original() {
    let f = Fixture::new("md");
    let result = editor::run(&f.target, &f.command("noop"), &f.root).unwrap();
    let EditorOutcome::Unchanged(files) = result else {
        panic!("expected no-op")
    };
    let directory = files.directory().to_owned();
    files.discard().unwrap();
    assert!(!directory.exists());
    assert_eq!(fs::read(&f.original).unwrap(), ORIGINAL.as_bytes());
}

#[test]
fn nonexistent_recovery_root_returns_actionable_error_before_launch() {
    let f = Fixture::new("md");
    let error = editor::run(&f.target, &f.command("changed"), f.root.join("missing")).unwrap_err();
    assert_eq!(error.code(), "editor_workspace");
    assert!(error.to_string().contains("existing recovery directory"));
    assert!(error.recovery().is_none());
    assert_eq!(fs::read(&f.original).unwrap(), ORIGINAL.as_bytes());
}

#[test]
fn parser_preserves_quotes_escapes_empty_arguments_and_literal_shell_syntax() {
    let command = EditorCommand::parse(r#"'/editor with spaces' --wait "two words" '' a\ b "$HOME;$(touch nope)`id`" 'back\slash'"#).unwrap();
    assert_eq!(command.executable(), "/editor with spaces");
    assert_eq!(
        command.arguments(),
        [
            "--wait",
            "two words",
            "",
            "a b",
            "$HOME;$(touch nope)`id`",
            "back\\slash"
        ]
    );
}

#[test]
fn parser_rejects_missing_malformed_and_nul_configuration() {
    for input in [
        "",
        "  ",
        "'' arg",
        "'unclosed",
        "\"unclosed",
        "editor\\",
        "editor\0",
        "editor\\\0",
    ] {
        let error = EditorCommand::parse(input).unwrap_err();
        assert_eq!(error.code(), "editor_configuration", "{input:?}");
        assert!(error.recovery().is_none());
    }
}

#[test]
fn configured_visual_editor_precedence_and_invalid_selected_config_are_explicit() {
    assert_eq!(
        EditorCommand::select(Some("configured --wait"), Some("visual"), Some("editor"))
            .unwrap()
            .executable(),
        "configured"
    );
    assert_eq!(
        EditorCommand::select(None, Some("visual"), Some("editor"))
            .unwrap()
            .executable(),
        "visual"
    );
    assert_eq!(
        EditorCommand::select(None, Some(" "), Some("editor"))
            .unwrap()
            .executable(),
        "editor"
    );
    assert_eq!(
        EditorCommand::select(None, None, Some("editor"))
            .unwrap()
            .executable(),
        "editor"
    );
    assert!(EditorCommand::select(None, None, None).is_err());
    assert!(EditorCommand::select(Some(""), Some("visual"), Some("editor")).is_err());
    assert!(EditorCommand::select(None, Some("'broken"), Some("editor")).is_err());
}

#[test]
fn shell_metacharacters_are_passed_as_data_to_the_executable() {
    let f = Fixture::new("md");
    let injected = f.root.join("should not exist");
    let payload = format!("$(touch '{}');$HOME`id`", injected.display());
    // Double-quoted parser input, not a shell invocation. Single quotes are data.
    let command = EditorCommand::parse(&format!(
        "'{}' record \"{payload}\"",
        fixture_path("fake editor.sh").display()
    ))
    .unwrap();
    let result = editor::run(&f.target, &command, &f.root).unwrap();
    let EditorOutcome::Changed(changed) = result else {
        panic!("expected change")
    };
    assert_eq!(changed.source(), payload);
    f.preserved(changed.recovery());
    assert!(!injected.exists());
}

#[test]
fn environment_selection_uses_actual_variables_without_global_test_races() {
    const MARKER: &str = "EDITAC_EDITOR_ENV_TEST";
    if let Ok(case) = std::env::var(MARKER) {
        let result =
            EditorCommand::from_environment((case == "explicit").then_some("configured --wait"));
        match case.as_str() {
            "explicit" => assert_eq!(result.unwrap().executable(), "configured"),
            "visual" => assert_eq!(result.unwrap().executable(), "visual"),
            "editor" => assert_eq!(result.unwrap().executable(), "editor"),
            "missing" | "invalid" => assert_eq!(result.unwrap_err().code(), "editor_configuration"),
            _ => panic!("unknown environment fixture"),
        }
        return;
    }
    use std::os::unix::ffi::OsStringExt;
    for case in ["explicit", "visual", "editor", "missing", "invalid"] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .env(MARKER, case)
            .arg("--exact")
            .arg("environment_selection_uses_actual_variables_without_global_test_races");
        match case {
            "explicit" | "visual" => {
                command
                    .env("VISUAL", "visual --wait")
                    .env("EDITOR", "editor");
            }
            "editor" => {
                command.env("VISUAL", " ").env("EDITOR", "editor");
            }
            "invalid" => {
                command
                    .env("VISUAL", std::ffi::OsString::from_vec(vec![255]))
                    .env("EDITOR", "editor");
            }
            _ => {}
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{case}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    }
}
