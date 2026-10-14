//! Original TUI-1.3: real importer/catalog/session/native-store acquisition.
//! The fixture host supplies grants; no network, credentials or provider exists.
use editac_cli::{
    engine::document::DocumentIdentity,
    markdown::reconcile::SourceMode,
    storage::StoredIdentity,
    target::ResolveOptions,
    tui::session::{Representation, Request, open_local},
};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

const DOC: &str = "ac000000-0000-5000-8000-000000000015";

const SOURCE: &str = "# Cached 🦀\n\nExisting content\n";
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    db: PathBuf,
    catalog: PathBuf,
    identity: DocumentIdentity,
}
impl Fixture {
    fn new() -> Self {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "editac-tui-targets-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        Self {
            db: root.join("cache.sqlite"),
            catalog: root.join("catalog.json"),
            identity: DocumentIdentity::v5(DOC, format!("ac000000-0000-4000-8000-{sequence:012x}"))
                .unwrap(),
            root,
        }
    }
    fn cache(&self, source: &str) {
        let path = self.root.join("cached.md");
        fs::write(&path, source).unwrap();
        drop(
            open_local(
                &path,
                ResolveOptions::default(),
                &self.db,
                self.identity.clone(),
            )
            .unwrap(),
        );
    }
    fn config(&self, access: &str, writable: bool, expiry: Option<u64>, cached: bool) {
        let store = if cached { json!(self.db) } else { Value::Null };
        let entry = |locator: String| json!({"locator":locator,"identity":StoredIdentity::from(&self.identity),"store":store,"access":access,"writable":writable,"expiresAt":expiry});
        self.write_config(json!({"authorities":[{"name":"work","appHost":"docs.example"}], "entries":[entry(format!("ac://{DOC}")), entry(format!("ac://work/{DOC}"))]}));
    }
    fn write_config(&self, config: Value) {
        fs::write(&self.catalog, serde_json::to_vec(&config).unwrap()).unwrap();
    }
    fn request(&self, target: &str) -> Request {
        Request::parse(&[
            "tui".into(),
            target.into(),
            "--catalog".into(),
            self.catalog.clone().into_os_string(),
            "--offline".into(),
        ])
        .unwrap()
    }
    fn run(&self, args: &[OsString]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_editac"))
            .args(args)
            .env_clear()
            .current_dir(&self.root)
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn local_args(path: &Path, store: &Path) -> Vec<OsString> {
    vec![
        "tui".into(),
        path.into(),
        "--store".into(),
        store.into(),
        "--offline".into(),
    ]
}

#[test]
fn local_markdown_retains_exact_spelling_and_identity_without_a_host() {
    let f = Fixture::new();
    let path = f.root.join("local.md");
    let source = "Heading\r\n=======\r\n\r\n*literal source spelling* 🦀\r\n";
    fs::write(&path, source).unwrap();
    let session = open_local(&path, ResolveOptions::default(), &f.db, f.identity.clone()).unwrap();
    assert_eq!(session.source, source);
    assert_eq!(session.representation, Representation::ExactSource);
    assert_eq!(session.service.session().document().identity(), &f.identity);
    assert!(session.service.capabilities(0).can_apply);
    assert_eq!(fs::read_to_string(path).unwrap(), source);
}
#[test]
fn literal_text_never_runs_markdown_import_rules() {
    let f = Fixture::new();
    let path = f.root.join("literal.txt");
    let source = "# Not a heading\r\n**not bold** 🦀\n";
    fs::write(&path, source).unwrap();
    let session = Request::parse(&local_args(&path, &f.db))
        .unwrap()
        .open()
        .unwrap();
    assert!(matches!(session.mode, SourceMode::Literal));
    assert_eq!(session.source, source);
    let snapshot = session.service.read(0).unwrap().snapshot;
    assert!(snapshot.visible.iter().any(|n| {
        n.runs
            .iter()
            .any(|run| run.text.contains("# Not a heading"))
    }));
}
#[test]
fn missing_file_requires_explicit_create_and_format_without_eager_publication() {
    let f = Fixture::new();
    let path = f.root.join("new.data");
    let mut args = local_args(&path, &f.db);
    assert!(Request::parse(&args).unwrap().open().is_err());
    assert!(!f.db.exists());
    args.push("--create".into());
    assert!(Request::parse(&args).unwrap().open().is_err());
    args.extend(["--format".into(), "markdown".into()]);
    let session = Request::parse(&args).unwrap().open().unwrap();
    assert!(session.source.is_empty());
    assert!(session.service.capabilities(0).can_apply);
    assert!(!path.exists(), "opening is not saving");
}
#[test]
fn recognized_markdown_suffix_allows_explicit_create_without_format_override() {
    let f = Fixture::new();
    let path = f.root.join("new.md");
    let mut args = local_args(&path, &f.db);
    assert!(Request::parse(&args).unwrap().open().is_err());
    args.push("--create".into());
    let session = Request::parse(&args).unwrap().open().unwrap();
    assert!(matches!(session.mode, SourceMode::Markdown(_)));
    assert!(session.source.is_empty());
    assert!(!path.exists());
}
#[test]
fn explicit_format_handles_literal_files_with_other_suffixes() {
    let f = Fixture::new();
    let path = f.root.join("literal.data");
    fs::write(&path, "# literal").unwrap();
    let mut args = local_args(&path, &f.db);
    assert!(Request::parse(&args).unwrap().open().is_err());
    args.extend(["--format".into(), "text".into()]);
    let session = Request::parse(&args).unwrap().open().unwrap();
    assert!(matches!(session.mode, SourceMode::Literal));
    assert_eq!(session.source, "# literal");
}
#[test]
fn local_readonly_result_is_enforced_by_shared_capabilities() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let path = f.root.join("readonly.md");
    fs::write(&path, SOURCE).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    let session = open_local(&path, ResolveOptions::default(), &f.db, f.identity.clone()).unwrap();
    let caps = session.service.capabilities(0);
    assert!(!caps.can_prepare);
    assert!(!caps.can_apply);
    assert!(!caps.can_history);
    assert_eq!(session.source, SOURCE);
}
#[test]
fn cached_ac_and_configured_https_share_existing_identity_offline() {
    let f = Fixture::new();
    f.cache(SOURCE);
    f.config("allowed", true, None, true);
    for target in [
        format!("ac://{DOC}"),
        format!("ac://work/{DOC}"),
        format!("https://docs.example/ac/{DOC}"),
    ] {
        let session = f.request(&target).open().unwrap();
        assert_eq!(session.service.session().document().identity(), &f.identity);
        assert_eq!(session.representation, Representation::HostedProjection);
        assert!(session.source.contains("Cached 🦀"));
        assert!(session.source.contains("Existing content"));
        assert!(session.service.capabilities(0).can_apply);
    }
}
#[test]
fn cached_empty_document_is_an_existing_identity_not_an_import() {
    let f = Fixture::new();
    f.cache("");
    f.config("allowed", true, None, true);
    let session = f.request(&format!("ac://{DOC}")).open().unwrap();
    assert_eq!(session.service.session().document().identity(), &f.identity);
    assert_eq!(session.representation, Representation::HostedProjection);
    assert!(session.source.trim().is_empty());
}
#[test]
fn readonly_and_expired_host_grants_preserve_read_access_and_deny_edits() {
    let f = Fixture::new();
    f.cache(SOURCE);
    for (writable, expiry) in [(false, None), (true, Some(5))] {
        f.config("allowed", writable, expiry, true);
        let session = f.request(&format!("ac://{DOC}")).open().unwrap();
        let read = session.service.read(6).unwrap();
        assert!(!read.capabilities.can_prepare);
        assert!(!read.capabilities.can_apply);
        assert!(!read.capabilities.can_history);
        assert!(session.source.contains("Existing content"));
    }
}
#[test]
fn access_policy_precedes_cached_database_acquisition() {
    let f = Fixture::new(); // Deliberately invalid bytes: rejection must precede SQLite open.
    fs::write(&f.db, b"untouched cache").unwrap();
    for access in ["unauthorized", "locked", "incompatible"] {
        f.config(access, true, None, true);
        assert!(f.request(&format!("ac://{DOC}")).open().is_err());
        assert_eq!(fs::read(&f.db).unwrap(), b"untouched cache");
        assert!(!f.db.with_extension("sqlite-wal").exists());
    }
}
#[test]
fn known_uncached_host_and_unknown_identity_never_create_replacements() {
    let f = Fixture::new();
    f.config("allowed", true, None, false);
    for target in [
        format!("ac://{DOC}"),
        format!("https://docs.example/ac/{DOC}"),
        "ac://ac000000-0000-5000-8000-000000000099".into(),
    ] {
        assert!(f.request(&target).open().is_err());
        assert!(!f.db.exists());
    }
}
#[test]
fn unknown_authorities_and_noncanonical_urls_reject_without_empty_documents() {
    let f = Fixture::new();
    f.config("allowed", true, None, true);
    for target in [
        format!("http://docs.example/ac/{DOC}"),
        format!("https://evil.example/ac/{DOC}"),
        format!("https://docs.example/{DOC}"),
        format!("ac://unknown/{DOC}"),
        format!("ac://{}", DOC.to_uppercase()),
        format!("ac://{DOC}?version=1"),
        format!("ac://{DOC}/"),
        format!("ac://work/%61{DOC}"),
        format!("https://docs.example:443/ac/{DOC}"),
        format!("https://secret@docs.example/ac/{DOC}"),
    ] {
        assert!(f.request(&target).open().is_err(), "{target}");
        assert!(!f.db.exists());
    }
}
#[test]
fn missing_corrupt_or_wrong_epoch_cache_never_initializes_a_replacement() {
    let f = Fixture::new();
    f.config("allowed", true, None, true);
    assert!(f.request(&format!("ac://{DOC}")).open().is_err());
    assert!(!f.db.exists());
    fs::write(&f.db, b"corrupt").unwrap();
    assert!(f.request(&format!("ac://{DOC}")).open().is_err());
    assert_eq!(fs::read(&f.db).unwrap(), b"corrupt");
    fs::remove_file(&f.db).unwrap();
    f.cache(SOURCE);
    let mut config: Value = serde_json::from_slice(&fs::read(&f.catalog).unwrap()).unwrap();
    for entry in config["entries"].as_array_mut().unwrap() {
        entry["identity"]["epoch"] = json!("ac000000-0000-4000-8000-000000000099");
    }
    f.write_config(config);
    assert!(f.request(&format!("ac://{DOC}")).open().is_err());
    f.config("allowed", true, None, true);
    assert!(f.request(&format!("ac://{DOC}")).open().is_ok());
}
#[test]
fn command_acquires_local_and_hosted_sessions_and_reports_honest_phase() {
    let f = Fixture::new();
    let path = f.root.join("literal.txt");
    fs::write(&path, "# literal 🦀").unwrap();
    let output = f.run(&local_args(&path, &f.db));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["phase"], "session-acquired");
    assert_eq!(report["presentation"], "terminal loop pending");
    assert_eq!(report["representation"], "ExactSource");
    assert_eq!(report["canApply"], true);
    assert!(report["identity"]["documentId"].as_str().is_some());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("# literal"));
    // Different fixture supplies a prepared hosted grant over an existing shared cache.
    let hosted = Fixture::new();
    hosted.cache(SOURCE);
    hosted.config("allowed", false, None, true);
    let output = hosted.run(&[
        "tui".into(),
        format!("https://docs.example/ac/{DOC}").into(),
        "--catalog".into(),
        hosted.catalog.clone().into_os_string(),
        "--offline".into(),
    ]);
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["identity"],
        json!(StoredIdentity::from(&hosted.identity))
    );
    assert_eq!(report["representation"], "HostedProjection");
    assert_eq!(report["canApply"], false);
}
#[test]
fn diagnostic_redacts_url_credentials_and_rejects_secret_configuration() {
    let f = Fixture::new();
    f.config("allowed", true, None, false);
    let secret = "fixture-secret-never-print";
    for target in [
        format!("https://{secret}@docs.example/ac/{DOC}"),
        format!("https://docs.example/ac/{DOC}?token={secret}"),
    ] {
        let output = f.run(&[
            "tui".into(),
            target.into(),
            "--catalog".into(),
            f.catalog.clone().into_os_string(),
        ]);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
    let mut config: Value = serde_json::from_slice(&fs::read(&f.catalog).unwrap()).unwrap();
    config["authorities"][0]["token"] = json!(secret);
    f.write_config(config);
    let output = f.run(&[
        "tui".into(),
        format!("ac://{DOC}").into(),
        "--catalog".into(),
        f.catalog.clone().into_os_string(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    assert!(!f.db.exists());
}
#[test]
fn invalid_options_and_remote_create_never_mutate_files() {
    for args in [
        vec!["tui"],
        vec!["tui", "local.md"],
        vec!["tui", "local.md", "--store"],
        vec!["tui", "local.md", "--store", "/tmp/new", "--unknown"],
        vec!["tui", "ac://x", "--catalog", "/tmp/config", "--create"],
        vec![
            "tui",
            "ac://x",
            "--catalog",
            "/tmp/config",
            "--offline",
            "--offline",
        ],
    ] {
        let args = args.into_iter().map(OsString::from).collect::<Vec<_>>();
        assert!(Request::parse(&args).is_err());
    }
}
#[test]
fn existing_store_is_not_overwritten_by_local_import() {
    let f = Fixture::new();
    let path = f.root.join("local.md");
    fs::write(&path, SOURCE).unwrap();
    fs::write(&f.db, b"preserved").unwrap();
    assert!(
        Request::parse(&local_args(&path, &f.db))
            .unwrap()
            .open()
            .is_err()
    );
    assert_eq!(fs::read(&f.db).unwrap(), b"preserved");
    assert_eq!(fs::read_to_string(path).unwrap(), SOURCE);
}
