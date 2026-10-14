//! TUI-1.12: executable discovery and documented clean-consumer argument routes.
use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Consumer(PathBuf);
impl Consumer {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "editac-tui-help-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(path)
    }
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_editac"))
            .args(args)
            .current_dir(&self.0)
            .env_clear()
            .output()
            .unwrap()
    }
    fn acquired(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["phase"], "session-acquired");
        assert_eq!(
            report["presentation"],
            "noninteractive session report; terminal requires a TTY"
        );
        report
    }
}
impl Drop for Consumer {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn tui_help_is_offline_side_effect_free_and_documents_runtime_choices() {
    let c = Consumer::new();
    let output = c.run(&["tui", "--help"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let help = String::from_utf8(output.stdout).unwrap();
    for required in [
        "--store <absolute-db>",
        "--catalog <host-granted-json>",
        "--create",
        "--format <markdown|text>",
        "--offline | --adapter",
        "Ctrl-S",
        "Ctrl-Enter",
        "Ctrl-Z/Ctrl-Y",
        "Ctrl-D",
        "Ctrl-E",
        "Ctrl-R",
        "Ctrl-P",
        "Ctrl-Q",
        "mutable identifier, not a credential",
        "not an automatic file save",
        "remote durability",
        "Read-only",
        "docs/tui.md",
        "unqualified",
        "editac edit <path> --editor <configuration>",
    ] {
        assert!(help.contains(required), "missing {required}");
    }
    assert!(!help.contains('\u{1b}'));
    assert_eq!(fs::read_dir(&c.0).unwrap().count(), 0);
}

#[test]
fn root_help_discovers_terminal_help_and_preserves_external_commands() {
    let c = Consumer::new();
    let explicit = c.run(&["--help"]);
    assert!(explicit.status.success());
    let help = String::from_utf8(explicit.stdout.clone()).unwrap();
    for command in [
        "editac tui --help",
        "editac edit <path>",
        "--editor <configuration>",
        "editac recover",
        "editac sync",
        "editac status",
        "editac validate",
    ] {
        assert!(help.contains(command), "missing {command}");
    }
    assert_eq!(c.run(&[]).stdout, explicit.stdout);
    assert_eq!(fs::read_dir(&c.0).unwrap().count(), 0);
}

#[test]
#[cfg(unix)]
fn documented_markdown_open_and_store_reuse_preserve_source_and_identity() {
    let c = Consumer::new();
    let source = "# Odd  🦀\r\n";
    fs::write(c.0.join("notes.md"), source).unwrap();
    let store = c.0.join("notes.sqlite");
    let args = ["tui", "notes.md", "--store", store.to_str().unwrap()];
    let first = c.acquired(&args);
    let second = c.acquired(&args);
    assert_eq!(first["identity"], second["identity"]);
    assert_eq!(first["sourceBytes"], source.len());
    assert_eq!(first["representation"], "ExactSource");
    assert_eq!(first["canApply"], true);
    assert_eq!(fs::read(c.0.join("notes.md")).unwrap(), source.as_bytes());
    assert!(c.0.join("notes.sqlite.tui-recovery/draft.json").is_file());
}

#[test]
#[cfg(unix)]
fn documented_explicit_text_format_opens_literal_source_without_normalization() {
    let c = Consumer::new();
    let source = "# **literal**\n\u{1b}[31m";
    fs::write(c.0.join("source.data"), source).unwrap();
    let store = c.0.join("literal.sqlite");
    let report = c.acquired(&[
        "tui",
        "source.data",
        "--format",
        "text",
        "--store",
        store.to_str().unwrap(),
    ]);
    assert_eq!(report["sourceBytes"], source.len());
    assert_eq!(report["canApply"], true);
    assert_eq!(fs::read_to_string(c.0.join("source.data")).unwrap(), source);
}

#[test]
#[cfg(unix)]
fn explicit_create_acquires_an_empty_session_without_claiming_target_save() {
    let c = Consumer::new();
    let store = c.0.join("new.sqlite");
    let args = [
        "tui",
        "new.txt",
        "--create",
        "--store",
        store.to_str().unwrap(),
    ];
    let report = c.acquired(&args);
    assert_eq!(report["sourceBytes"], 0);
    assert!(!c.0.join("new.txt").exists());
    assert!(store.exists());
}

#[test]
fn documented_rejections_do_not_create_editable_replacement_state() {
    let c = Consumer::new();
    let store = c.0.join("must-not-exist.sqlite");
    let catalog = c.0.join("catalog.json");
    fs::write(&catalog, br#"{"authorities":[],"entries":[]}"#).unwrap();
    fs::write(c.0.join("unknown.data"), "literal").unwrap();
    for args in [
        vec!["tui", "missing.md", "--store", store.to_str().unwrap()],
        vec!["tui", "unknown.data", "--store", store.to_str().unwrap()],
        vec![
            "tui",
            "https://unknown.example.test/ac/secret",
            "--catalog",
            catalog.to_str().unwrap(),
        ],
        vec![
            "tui",
            "ac://ac000000-0000-5000-8000-000000000015",
            "--catalog",
            catalog.to_str().unwrap(),
            "--offline",
        ],
        vec![
            "tui",
            "ac://ac000000-0000-5000-8000-000000000015",
            "--catalog",
            catalog.to_str().unwrap(),
            "--offline",
            "--adapter",
            "/must-not-launch",
        ],
    ] {
        let output = c.run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("secret"));
        assert!(!store.exists());
        assert!(!c.0.join("missing.md").exists());
    }
    assert_eq!(fs::read_dir(&c.0).unwrap().count(), 2);
}
