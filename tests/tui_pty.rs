//! Actual packaged editac through a controlling PTY; native state, never a test presenter.
use editac_cli::{
    engine::document::DocumentIdentity,
    storage::StoredIdentity,
    target::ResolveOptions,
    tui::session::{Request, open_local},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

fn evidence() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::var_os("TUI_PTY_EVIDENCE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::temp_dir().join(format!("editac-pty-evidence-{}", std::process::id()))
            });
        assert!(root.is_absolute());
        fs::create_dir_all(&root).unwrap();
        root
    })
    .clone()
}
fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}
fn package() -> &'static Path {
    static PACKAGE: OnceLock<PathBuf> = OnceLock::new();
    PACKAGE.get_or_init(|| {
        let output = evidence().join("package");
        let revision = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        let run = Command::new("python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/package-local.py"))
            .args(["--binary", env!("CARGO_BIN_EXE_editac"), "--output"])
            .arg(&output)
            .arg("--revision")
            .arg(String::from_utf8(revision.stdout).unwrap().trim())
            .output()
            .unwrap();
        fs::write(
            evidence().join("package.log"),
            [&run.stdout[..], &run.stderr[..]].concat(),
        )
        .unwrap();
        assert!(run.status.success(), "packaging failed; see package.log");
        output.join("editac-local.tar.gz")
    })
}
struct Fixture {
    root: PathBuf,
    output: PathBuf,
    id: DocumentIdentity,
    args: Vec<String>,
}
struct SetupCleanup(PathBuf);
impl Drop for SetupCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
impl Fixture {
    fn new(name: &str, source: &str, hosted: Option<bool>) -> Self {
        let root = std::env::temp_dir().join(format!("editac-pty-{}-{name}", std::process::id()));
        assert!(!root.exists(), "fixture collision");
        fs::create_dir(&root).unwrap();
        let setup_cleanup = SetupCleanup(root.clone());
        let output = evidence().join(name);
        fs::create_dir_all(&output).unwrap();
        let unpack = Command::new("python3").args(["-c", "import pathlib,sys,tarfile; t=tarfile.open(sys.argv[1]); r=pathlib.Path(sys.argv[2]).resolve(); assert all(m.isfile() and r in (r/m.name).resolve().parents for m in t.getmembers()); t.extractall(r)"])
            .arg(package()).arg(root.join("consumer")).output().unwrap();
        fs::write(
            output.join("extract.log"),
            [&unpack.stdout[..], &unpack.stderr[..]].concat(),
        )
        .unwrap();
        assert!(unpack.status.success());
        let id = DocumentIdentity::v5(
            format!("ac000000-0000-5000-8000-{}", &hash(name)[..12]),
            format!("ac000000-0000-4000-8000-{}", &hash(name)[..12]),
        )
        .unwrap();
        let source_path = root.join(if name == "literal_unicode" {
            "source.txt"
        } else {
            "source.md"
        });
        fs::write(&source_path, source).unwrap();
        drop(
            open_local(
                &source_path,
                ResolveOptions::default(),
                &root.join("cache.sqlite"),
                id.clone(),
            )
            .unwrap(),
        );
        let args = if let Some(writable) = hosted {
            fs::write(
                root.join("catalog.json"),
                serde_json::to_vec(&json!({
                    "authorities":[], "entries":[{"locator":format!("ac://{}", id.document_id),
                    "identity":StoredIdentity::from(&id), "store":root.join("cache.sqlite"),
                    "access":"allowed", "writable":writable, "expiresAt":null}]
                }))
                .unwrap(),
            )
            .unwrap();
            vec![
                "tui".into(),
                format!("ac://{}", id.document_id),
                "--catalog".into(),
                root.join("catalog.json").display().to_string(),
                "--offline".into(),
            ]
        } else {
            vec![
                "tui".into(),
                source_path.display().to_string(),
                "--store".into(),
                root.join("cache.sqlite").display().to_string(),
                "--offline".into(),
            ]
        };
        std::mem::forget(setup_cleanup);
        Self {
            root,
            output,
            id,
            args,
        }
    }
    fn run(&self, stage: &str, mut scenario: Value, expected_success: bool) -> Value {
        scenario["args"] = json!(self.args);
        let output = self.output.join(stage);
        fs::create_dir_all(&output).unwrap();
        let path = output.join("fixture.json");
        fs::write(&path, serde_json::to_vec_pretty(&scenario).unwrap()).unwrap();
        let run = Command::new("python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/tui-pty.py"))
            .arg("--binary")
            .arg(self.root.join("consumer/bin/editac"))
            .arg("--root")
            .arg(&self.root)
            .arg("--output")
            .arg(&output)
            .arg("--fixture")
            .arg(path)
            .output()
            .unwrap();
        fs::write(
            output.join("controller.log"),
            [&run.stdout[..], &run.stderr[..]].concat(),
        )
        .unwrap();
        let result: Value =
            serde_json::from_slice(&fs::read(output.join("result.json")).unwrap()).unwrap();
        assert_eq!(result["reaped"], true);
        assert_eq!(result["terminalAttributesRestored"], true);
        assert!(result["frameCount"].as_u64().unwrap() > 0);
        assert_eq!(
            run.status.success(),
            expected_success,
            "PTY check failed: {} (see {})",
            result["failure"],
            output.display()
        );
        result
    }
    fn verify(&self, source: &str, pending: usize, file: Option<&str>) {
        let request =
            Request::parse(&self.args.iter().map(Into::into).collect::<Vec<_>>()).unwrap();
        let session = request.open().unwrap();
        assert_eq!(session.source, source);
        assert_eq!(session.service.session().document().identity(), &self.id);
        let state = session.service.session().state(0);
        assert_eq!(state.pending_upload, pending);
        if let Some(file) = file {
            let target = PathBuf::from(&self.args[1]);
            assert_eq!(fs::read(&target).unwrap(), file.as_bytes());
        }
        fs::write(self.output.join("state.json"), serde_json::to_vec_pretty(&json!({
            "sourceSha256":hash(&session.source), "pendingUpload":state.pending_upload,
            "checkpointSha256":hash(session.service.session().document().checkpoint().bytes),
            "local":format!("{:?}",state.local), "remote":format!("{:?}",state.remote),
            "identity":StoredIdentity::from(&self.id), "binarySha256":hash(fs::read(self.root.join("consumer/bin/editac")).unwrap()),
        })).unwrap()).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
        assert!(!self.root.exists());
        fs::write(
            self.output.join("cleanup.json"),
            b"{\"temporaryConsumerRemoved\":true,\"childrenReaped\":true}\n",
        )
        .unwrap();
    }
}
fn scenario(name: &str) -> Value {
    let fixtures: Value = serde_json::from_str(include_str!("fixtures/tui/pty.json")).unwrap();
    fixtures[name].clone()
}

#[test]
fn packaged_markdown_save_diff_undo_redo_and_noop() {
    let f = Fixture::new("markdown_history", "# Seed\n", None);
    f.run("edit", scenario("markdown_history"), true);
    f.verify("Edited # Seed\n", 3, Some("Edited # Seed\n"));
}
#[test]
fn packaged_literal_unicode_bracketed_paste_and_exact_source() {
    let f = Fixture::new("literal_unicode", "seed\r\n", None);
    f.run("edit", scenario("literal_unicode"), true);
    f.verify(
        "**literal** 🦀 e\u{301} 中\nseed\r\n",
        1,
        Some("**literal** 🦀 e\u{301} 中\nseed\r\n"),
    );
}
#[test]
fn packaged_cached_readonly_rejects_all_input_and_history() {
    let f = Fixture::new("readonly", "# Seed\n", Some(false));
    f.run("view", scenario("readonly"), true);
    f.verify("# Seed\n", 0, None);
}
#[test]
fn packaged_offline_cached_commit_and_restart_preserves_outbox() {
    let f = Fixture::new("hosted_restart", "# Seed\n", Some(true));
    f.run("commit", scenario("hosted_commit"), true);
    f.run("restart", scenario("hosted_restart"), true);
    f.verify("Cached # Seed\n", 1, None);
}
#[test]
fn packaged_uncommitted_draft_quit_and_offline_restart() {
    let f = Fixture::new("draft_restart", "# Seed\n", None);
    f.run("draft", scenario("draft_quit"), true);
    f.run("restart", scenario("draft_restart"), true);
    f.verify("# Seed\n", 0, Some("# Seed\n"));
}
#[test]
fn packaged_resize_focus_and_terminal_control_escaping() {
    let f = Fixture::new("resize", "# Seed\n\u{1b}[31munsafe\n", None);
    f.run("resize", scenario("resize"), true);
    f.verify(
        "# Seed\n\u{1b}[31munsafe\n",
        0,
        Some("# Seed\n\u{1b}[31munsafe\n"),
    );
}
#[test]
fn packaged_external_file_conflict_retains_draft_without_overwrite() {
    let f = Fixture::new("file_conflict", "# Seed\n", None);
    f.run("conflict", scenario("file_conflict"), true);
    let request = Request::parse(&f.args.iter().map(Into::into).collect::<Vec<_>>()).unwrap();
    assert_eq!(
        request.open().err(),
        Some("local recovery target changed; retain private draft and store")
    );
    assert_eq!(fs::read(f.root.join("source.md")).unwrap(), b"External\n");
    let store = editac_session::store::NativeStore::open_existing(
        f.root.join("cache.sqlite"),
        f.id.clone(),
        Default::default(),
    )
    .unwrap();
    let restored = editac_session::session::Session::open(
        store,
        &f.id,
        editac_session::session::CachedPermission::LOCAL,
    )
    .unwrap();
    let projected = editac_cli::reconcile::HostedProjection::capture(restored.document()).unwrap();
    assert_eq!(projected.source, "Local \\# Seed\n");
    assert_eq!(restored.state(0).pending_upload, 1);
    let recovery = editac_cli::storage::NativeStorage::open(
        editac_cli::tui::lifecycle::recovery_path(&f.root.join("cache.sqlite")),
        f.id.clone(),
    )
    .unwrap();
    let draft = recovery.read_draft().unwrap().unwrap();
    let snapshot: Value = serde_json::from_str(&draft.source).unwrap();
    assert_eq!(snapshot["committed"], "Local # Seed\n");
    assert_eq!(snapshot["draft"], "Local # Seed\n");
    fs::write(
        f.output.join("state.json"),
        serde_json::to_vec_pretty(&json!({
            "sourceSha256": hash(projected.source), "pendingUpload":1,
            "externalFileSha256":hash("External\n"), "reopen":"explicit target-changed rejection",
            "privateDraftRetained":true
        }))
        .unwrap(),
    )
    .unwrap();
}
#[test]
fn packaged_runtime_error_restores_terminal_and_reaps_child() {
    let f = Fixture::new("error_exit", "# Seed\n", None);
    let result = f.run("error", scenario("error_exit"), true);
    assert_eq!(result["exit"], 2);
}
#[test]
fn intentional_driver_failure_is_bounded_and_reaps_actual_binary() {
    let f = Fixture::new("intentional_failure", "# Seed\n", None);
    let result = f.run("expected-failure", scenario("intentional_failure"), false);
    assert!(
        result["failure"]
            .as_str()
            .unwrap()
            .contains("missing frame marker")
    );
}
