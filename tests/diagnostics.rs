//! CLI-1.10 canonical shared codecs + native guarded-publication + binary.
use editac_cli::{
    diagnostics::{self as d, Format, MAX_INPUT_BYTES},
    engine::{
        commands::text::{TextCommand, TextOrigin},
        document::{CreationPermit, Document, DocumentIdentity, InitialNode},
    },
    files::{FailureKind, Filesystem},
    storage::StoredIdentity,
};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
const MARKDOWN: &[u8] = include_bytes!("fixtures/diagnostics/source.md");
const LOSSY: &[u8] = include_bytes!("fixtures/diagnostics/underline.json");
struct Fixture {
    root: PathBuf,
    source: PathBuf,
    output: PathBuf,
}
impl Fixture {
    fn new(bytes: &[u8]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let root = std::env::temp_dir().join(format!(
            "editac-diagnostics-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let source = root.join("source.unknown");
        let output = root.join("output.unknown");
        fs::write(&source, bytes).unwrap();
        Self {
            root,
            source,
            output,
        }
    }
    fn convert(
        &self,
        input: Format,
        output: Format,
        lossy: bool,
        overwrite: bool,
    ) -> Result<(PathBuf, d::Conversion), d::Error> {
        d::convert_file(&self.source, &self.output, input, output, lossy, overwrite)
    }
    fn run(&self, args: &[&std::ffi::OsStr]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_editac"))
            .args(args)
            .env_clear()
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn update() -> (Document, Vec<u8>) {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let i = DocumentIdentity::v5(
        format!("11111111-1111-4111-8111-{id:012x}"),
        "22222222-2222-4222-8222-222222222222",
    )
    .unwrap();
    let doc = Document::create(
        CreationPermit::authorize_new(i).unwrap(),
        &[InitialNode {
            kind: "block".into(),
            text: "secret source".into(),
        }],
    )
    .unwrap();
    let bytes = envelope(&doc, doc.checkpoint().bytes);
    (doc, bytes)
}
fn envelope(doc: &Document, bytes: Vec<u8>) -> Vec<u8> {
    serde_json::to_vec(&json!({"identity":StoredIdentity::from(doc.identity()),"bytes":bytes}))
        .unwrap()
}
fn err(bytes: &[u8], format: Format) -> &'static str {
    d::validate(bytes, format, None).unwrap_err().code
}
#[test]
fn markdown_report_is_stable_and_content_free() {
    let a = d::validate(MARKDOWN, Format::Markdown, None).unwrap();
    assert_eq!(a, d::inspect(MARKDOWN, Format::Markdown, None).unwrap());
    assert_eq!(a.blocks, Some(2));
    assert!(a.diagnostics.is_empty());
    assert_eq!(a.durability, "not-assessed");
    let report = serde_json::to_string(&a).unwrap();
    assert!(!report.contains("Hello"));
    assert!(!report.contains("🦀"));
}
#[test]
fn markdown_json_round_trip_uses_shared_semantics() {
    let json = d::convert(MARKDOWN, Format::Markdown, Format::Json, false).unwrap();
    assert!(!json.lossy);
    assert_eq!(
        d::validate(&json.bytes, Format::Json, None).unwrap().blocks,
        Some(2)
    );
    let md = d::convert(&json.bytes, Format::Json, Format::Markdown, false).unwrap();
    let a = editac_cli::markdown::import::import(std::str::from_utf8(MARKDOWN).unwrap()).unwrap();
    let b = editac_cli::markdown::import::import(std::str::from_utf8(&md.bytes).unwrap()).unwrap();
    assert!(editac_cli::markdown::export::equivalent(
        &a.blocks, &b.blocks
    ));
}
#[test]
fn json_metadata_never_exposes_payloads() {
    let r = serde_json::to_string(&d::inspect(LOSSY, Format::Json, None).unwrap()).unwrap();
    assert!(!r.contains("private-token"));
    assert!(r.contains("not-assessed"));
}
#[test]
fn lossy_export_requires_explicit_consent_and_retains_source() {
    let e = d::convert(LOSSY, Format::Json, Format::Markdown, false).unwrap_err();
    assert_eq!(e.code, "lossy-conversion-requires-consent");
    assert_eq!(e.recovery.as_deref(), Some(LOSSY));
    let c = d::convert(LOSSY, Format::Json, Format::Markdown, true).unwrap();
    assert!(c.lossy);
    assert!(!c.diagnostics.is_empty());
}
#[test]
fn source_fallback_requires_consent_for_json_export() {
    let source = b"<script>secret</script>\n";
    assert!(
        !d::validate(source, Format::Markdown, None)
            .unwrap()
            .diagnostics
            .is_empty()
    );
    assert_eq!(
        d::convert(source, Format::Markdown, Format::Json, false)
            .unwrap_err()
            .code,
        "lossy-conversion-requires-consent"
    );
}
#[test]
fn malformed_json_and_utf8_are_rejected() {
    assert_eq!(err(b"{", Format::Json), "invalid-json");
    assert_eq!(err(&[255], Format::Markdown), "invalid-utf8");
    assert!(d::validate(b"[]", Format::Json, None).is_err());
}
#[test]
fn oversized_input_fails_before_shared_parse_or_io() {
    let large = vec![b'a'; MAX_INPUT_BYTES + 1];
    for format in [Format::Markdown, Format::Json, Format::Update] {
        assert_eq!(err(&large, format), "resource-limit");
    }
    let f = Fixture::new(&large);
    assert!(d::read(&f.source).is_err());
    assert!(
        f.convert(Format::Markdown, Format::Json, false, false)
            .is_err()
    );
    assert!(!f.output.exists());
}
#[test]
fn unknown_fragment_version_schema_and_fields_fail_closed() {
    let v: Value = serde_json::from_slice(LOSSY).unwrap();
    for (field, value) in [
        ("version", json!(99)),
        ("schema", json!("unknown")),
        ("extra", json!(true)),
    ] {
        let mut bad = v.clone();
        bad[field] = value;
        assert!(d::validate(&serde_json::to_vec(&bad).unwrap(), Format::Json, None).is_err());
    }
}
#[test]
fn semantic_fragment_validation_rejects_unsafe_links_and_limits() {
    let mut v: Value = serde_json::from_slice(LOSSY).unwrap();
    v["blocks"][0]["runs"][0]["marks"] = json!([{"mark":"link","href":"javascript:alert(1)"}]);
    assert!(d::validate(&serde_json::to_vec(&v).unwrap(), Format::Json, None).is_err());
    v["blocks"] = json!(vec![v["blocks"][0].clone(); 129]);
    assert!(d::validate(&serde_json::to_vec(&v).unwrap(), Format::Json, None).is_err());
}
#[test]
fn identified_complete_update_is_admitted_and_metadata_is_safe() {
    let (_, bytes) = update();
    let a = d::validate(&bytes, Format::Update, None).unwrap();
    assert!(a.update_bytes.unwrap() > 0);
    assert_eq!(a.blocks, None);
    let r = serde_json::to_string(&a).unwrap();
    assert!(!r.contains("11111111"));
    assert!(!r.contains("secret"));
    assert_eq!(a.durability, "not-assessed");
    let c = d::convert(&bytes, Format::Update, Format::Update, false).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&c.bytes).unwrap(),
        serde_json::from_slice::<Value>(&bytes).unwrap()
    );
}
#[test]
fn incremental_update_requires_explicit_matching_base() {
    let (mut doc, base) = update();
    let vector = doc.state_vector_v1();
    let snapshot = doc.snapshot().unwrap();
    let id = snapshot.visible[0].id.clone();
    let at = doc
        .anchor(
            &id,
            editac_cli::engine::Utf16Offset(0),
            editac_cli::engine::anchors::Bias::Right,
        )
        .unwrap();
    let plan = doc
        .prepare_text(
            &TextCommand::Insert {
                at,
                text: "🦀".into(),
            },
            &TextOrigin::new("diagnostics").unwrap(),
        )
        .unwrap();
    doc.commit_text(plan).unwrap();
    let delta = envelope(&doc, doc.update_v1(&vector).unwrap());
    assert_eq!(err(&delta, Format::Update), "invalid-or-incomplete-update");
    assert!(d::validate(&delta, Format::Update, Some(&base)).is_ok());
    let mut wrong: Value = serde_json::from_slice(&base).unwrap();
    wrong["identity"]["epoch"] = json!("33333333-3333-4333-8333-333333333333");
    assert_eq!(
        d::validate(
            &delta,
            Format::Update,
            Some(&serde_json::to_vec(&wrong).unwrap())
        )
        .unwrap_err()
        .code,
        "base-identity-mismatch"
    );
}
#[test]
fn identified_update_versions_and_binary_corruption_fail_safely() {
    let (_, bytes) = update();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    for (field, value) in [
        ("protocolVersion", json!(99)),
        ("updateEncoding", json!(2)),
        ("offsets", json!("bytes")),
        ("schema", json!("unknown")),
        ("profile", json!("unknown")),
    ] {
        let mut bad = v.clone();
        bad["identity"][field] = value;
        assert_eq!(
            err(&serde_json::to_vec(&bad).unwrap(), Format::Update),
            "unsupported-version"
        );
    }
    for binary in [vec![], vec![255], vec![255; 100]] {
        let mut bad = v.clone();
        bad["bytes"] = json!(binary);
        assert!(d::validate(&serde_json::to_vec(&bad).unwrap(), Format::Update, None).is_err());
    }
    let mut bad = v;
    bad["bytes"].as_array_mut().unwrap().push(json!(0));
    assert!(d::validate(&serde_json::to_vec(&bad).unwrap(), Format::Update, None).is_err());
}
#[test]
fn anonymous_binary_and_cross_identity_conversion_are_refused() {
    assert!(d::validate(&[0, 0], Format::Update, None).is_err());
    assert!(d::convert(MARKDOWN, Format::Markdown, Format::Update, false).is_err());
    let (_, bytes) = update();
    assert!(d::convert(&bytes, Format::Update, Format::Json, true).is_err());
    assert!(d::validate(MARKDOWN, Format::Markdown, Some(&bytes)).is_err());
}
#[test]
fn native_conversion_is_atomic_preserves_source_and_requires_overwrite() {
    let f = Fixture::new(MARKDOWN);
    fs::write(&f.output, b"existing\r\n").unwrap();
    assert_eq!(
        f.convert(Format::Markdown, Format::Json, false, false)
            .unwrap_err()
            .code,
        "overwrite-required"
    );
    assert_eq!(fs::read(&f.output).unwrap(), b"existing\r\n");
    let (_, c) = f
        .convert(Format::Markdown, Format::Json, false, true)
        .unwrap();
    assert_eq!(fs::read(&f.output).unwrap(), c.bytes);
    assert_eq!(fs::read(&f.source).unwrap(), MARKDOWN);
}
#[test]
fn failed_preparation_never_creates_or_changes_destination() {
    let f = Fixture::new(LOSSY);
    assert!(
        f.convert(Format::Json, Format::Markdown, false, false)
            .is_err()
    );
    assert!(!f.output.exists());
    fs::write(&f.output, b"existing").unwrap();
    assert!(
        f.convert(Format::Json, Format::Markdown, false, true)
            .is_err()
    );
    assert_eq!(fs::read(&f.output).unwrap(), b"existing");
    assert_eq!(fs::read(&f.source).unwrap(), LOSSY);
}
#[test]
fn source_path_and_hardlink_alias_are_never_overwritten() {
    let f = Fixture::new(MARKDOWN);
    for consent in [false, true] {
        assert_eq!(
            d::convert_file(
                &f.source,
                &f.source,
                Format::Markdown,
                Format::Json,
                false,
                consent
            )
            .unwrap_err()
            .code,
            "source-destination-alias"
        );
    }
    fs::hard_link(&f.source, &f.output).unwrap();
    assert_eq!(
        f.convert(Format::Markdown, Format::Json, false, true)
            .unwrap_err()
            .code,
        "source-destination-alias"
    );
    assert_eq!(fs::read(&f.source).unwrap(), MARKDOWN);
}
#[cfg(unix)]
#[test]
fn symlink_inputs_destinations_and_nonregular_inputs_fail_closed() {
    let f = Fixture::new(MARKDOWN);
    std::os::unix::fs::symlink(&f.source, &f.output).unwrap();
    assert!(d::read(&f.output).is_err());
    assert!(
        f.convert(Format::Markdown, Format::Json, false, true)
            .is_err()
    );
    assert!(d::read(&f.root).is_err());
    assert_eq!(fs::read(&f.source).unwrap(), MARKDOWN);
}
#[cfg(unix)]
#[test]
fn read_only_destination_preserves_permissions_and_bytes() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(MARKDOWN);
    fs::write(&f.output, b"old").unwrap();
    fs::set_permissions(&f.output, fs::Permissions::from_mode(0o400)).unwrap();
    let e = f
        .convert(Format::Markdown, Format::Json, false, true)
        .unwrap_err();
    assert_eq!(e.publication, Some(FailureKind::ReadOnly));
    assert_eq!(fs::read(&f.output).unwrap(), b"old");
    assert_eq!(
        fs::metadata(&f.output).unwrap().permissions().mode() & 0o777,
        0o400
    );
}
#[derive(Clone, Copy)]
enum Fault {
    Write,
    FileSync,
    BeforePublish,
    AfterPublish,
    DirectorySync,
    Stale,
}
struct FaultIo {
    fault: Fault,
    syncs: usize,
    destination: PathBuf,
}
impl Filesystem for FaultIo {
    fn write_all(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        if matches!(self.fault, Fault::Write) {
            return Err(io::Error::from(io::ErrorKind::StorageFull));
        }
        use std::io::Write;
        file.write_all(bytes)
    }
    fn sync_file(&mut self, file: &File) -> io::Result<()> {
        if matches!(self.fault, Fault::FileSync) {
            return Err(io::Error::other("prepared file sync failure"));
        }
        if matches!(self.fault, Fault::Stale) {
            fs::write(&self.destination, b"external edit")?;
        }
        file.sync_all()
    }
    fn publish(&mut self, temp: &Path, dest: &Path, replace: bool) -> io::Result<()> {
        if matches!(self.fault, Fault::BeforePublish) {
            return Err(io::Error::other("prepared publication failure"));
        }
        if replace {
            fs::rename(temp, dest)?;
        } else {
            fs::hard_link(temp, dest)?;
        }
        if matches!(self.fault, Fault::AfterPublish) {
            return Err(io::Error::other("prepared postpublication failure"));
        }
        Ok(())
    }
    fn sync_directory(&mut self, file: &File) -> io::Result<()> {
        self.syncs += 1;
        if matches!(self.fault, Fault::DirectorySync) && self.syncs == 2 {
            return Err(io::Error::other("prepared directory sync failure"));
        }
        file.sync_all()
    }
}
fn failure(fault: Fault) {
    let f = Fixture::new(MARKDOWN);
    fs::write(&f.output, b"old").unwrap();
    let mut io = FaultIo {
        fault,
        syncs: 0,
        destination: f.output.clone(),
    };
    let e = d::convert_file_with(
        &f.source,
        &f.output,
        Format::Markdown,
        Format::Json,
        false,
        true,
        &mut io,
    )
    .unwrap_err();
    assert!(e.recovery.is_some());
    assert_eq!(fs::read(&f.source).unwrap(), MARKDOWN);
    match fault {
        Fault::AfterPublish | Fault::DirectorySync => {
            assert_eq!(e.publication, Some(FailureKind::PublicationUncertain));
            assert_eq!(fs::read(&f.output).unwrap(), e.recovery.unwrap());
        }
        Fault::BeforePublish => {
            assert_eq!(e.publication, Some(FailureKind::PublicationUncertain));
            assert_eq!(fs::read(&f.output).unwrap(), b"old");
        }
        Fault::Stale => {
            assert_eq!(e.publication, Some(FailureKind::Stale));
            assert_eq!(fs::read(&f.output).unwrap(), b"external edit");
        }
        _ => {
            assert_eq!(e.publication, Some(FailureKind::Io));
            assert_eq!(fs::read(&f.output).unwrap(), b"old");
        }
    }
    assert!(!fs::read_dir(&f.root).unwrap().any(|p| {
        p.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".editac-save-")
    }));
}
#[test]
fn full_disk_preserves_old_output_and_recovery() {
    failure(Fault::Write)
}
#[test]
fn file_sync_failure_preserves_old_output_and_recovery() {
    failure(Fault::FileSync)
}
#[test]
fn publication_failure_is_explicitly_uncertain() {
    failure(Fault::BeforePublish)
}
#[test]
fn postpublication_failure_is_explicitly_uncertain() {
    failure(Fault::AfterPublish)
}
#[test]
fn directory_durability_failure_retains_recovery() {
    failure(Fault::DirectorySync)
}
#[test]
fn external_edit_during_staging_is_not_overwritten() {
    failure(Fault::Stale)
}
#[test]
fn binary_validation_inspection_and_conversion_are_offline_and_explicit() {
    let f = Fixture::new(MARKDOWN);
    for command in ["validate", "inspect"] {
        let o = f.run(&[
            command.as_ref(),
            f.source.as_os_str(),
            "--input-format".as_ref(),
            "markdown".as_ref(),
        ]);
        assert!(o.status.success(), "{:?}", o.stderr);
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        assert_eq!(v["blocks"], 2);
        assert_eq!(v["durability"], "not-assessed");
    }
    let o = f.run(&[
        "convert".as_ref(),
        f.source.as_os_str(),
        "--input-format".as_ref(),
        "markdown".as_ref(),
        "--output-format".as_ref(),
        "json".as_ref(),
        "--output".as_ref(),
        f.output.as_os_str(),
    ]);
    assert!(o.status.success(), "{:?}", o.stderr);
    assert_eq!(fs::read(&f.source).unwrap(), MARKDOWN);
    assert!(d::validate(&fs::read(&f.output).unwrap(), Format::Json, None).is_ok());
}
#[test]
fn binary_rejects_missing_duplicate_or_irrelevant_options_without_writes() {
    let f = Fixture::new(MARKDOWN);
    for args in [
        vec!["validate", "--input-format", "markdown", "--overwrite"],
        vec![
            "inspect",
            "--input-format",
            "markdown",
            "--input-format",
            "json",
        ],
        vec!["convert", "--input-format", "markdown"],
        vec!["validate"],
    ] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_editac"));
        cmd.arg(args[0]).arg(&f.source).args(&args[1..]).env_clear();
        let o = cmd.output().unwrap();
        assert_eq!(o.status.code(), Some(2));
        assert!(o.stdout.is_empty());
        assert!(!f.output.exists());
        assert_eq!(fs::read(&f.source).unwrap(), MARKDOWN);
    }
}

#[test]
fn empty_identified_update_is_a_valid_noop_without_initialization_claim() {
    let (doc, _) = update();
    let bytes = envelope(&doc, vec![0, 0]);
    let report = d::validate(&bytes, Format::Update, None).unwrap();
    assert_eq!(report.update_bytes, Some(2));
    assert_eq!(
        report.update_scope,
        Some("self-contained-replay;initialization-not-assessed")
    );
    assert_eq!(report.durability, "not-assessed");
}

#[test]
fn shared_fragment_numeric_version_policy_is_preserved() {
    let source = std::str::from_utf8(LOSSY)
        .unwrap()
        .replace("\"version\":1", "\"version\":1.0");
    assert!(d::validate(source.as_bytes(), Format::Json, None).is_ok());
}

#[test]
fn binary_loss_and_overwrite_flags_are_required_independently() {
    let f = Fixture::new(LOSSY);
    let run = |flags: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_editac"));
        cmd.args(["convert"])
            .arg(&f.source)
            .args([
                "--input-format",
                "json",
                "--output-format",
                "markdown",
                "--output",
            ])
            .arg(&f.output)
            .args(flags)
            .env_clear();
        cmd.output().unwrap()
    };
    let o = run(&[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(o.stdout.is_empty());
    assert!(!f.output.exists());
    fs::write(&f.output, b"existing").unwrap();
    let o = run(&["--allow-lossy"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(fs::read(&f.output).unwrap(), b"existing");
    let o = run(&["--allow-lossy", "--overwrite"]);
    assert!(o.status.success(), "{:?}", o.stderr);
    let report: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(report["lossy"], true);
    assert_eq!(report["durability"], "locally-saved");
    assert_eq!(fs::read(&f.source).unwrap(), LOSSY);
}

#[test]
fn binary_identified_update_inspection_uses_shared_admission_without_exposing_identity() {
    let (_, bytes) = update();
    let f = Fixture::new(&bytes);
    let o = f.run(&[
        "inspect".as_ref(),
        f.source.as_os_str(),
        "--input-format".as_ref(),
        "update".as_ref(),
    ]);
    assert!(o.status.success(), "{:?}", o.stderr);
    let r = String::from_utf8(o.stdout).unwrap();
    assert!(!r.contains("11111111"));
    assert!(!r.contains("secret"));
    assert!(r.contains("initialization-not-assessed"));
    assert!(r.contains("not-assessed"));
}
