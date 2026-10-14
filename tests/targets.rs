use editac_cli::target::{Baseline, Format, MAX_SOURCE_BYTES, ResolveOptions, resolve};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "editac-targets-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn options(format: Format) -> ResolveOptions {
    ResolveOptions {
        format: Some(format),
        create: false,
    }
}
fn fingerprint(
    target: &editac_cli::target::LocalTarget,
) -> &editac_cli::target::ContentFingerprint {
    match target.baseline() {
        Baseline::Existing { fingerprint, .. } => fingerprint,
        _ => panic!("expected existing target"),
    }
}

#[test]
fn markdown_preserves_exact_unicode_bom_whitespace_and_line_endings() {
    let fixture = Fixture::new();
    let bytes = "\u{feff}# 🦀 e\u{301} مرحبا\r\n\r\n*literal*  \r\n".as_bytes();
    let path = fixture.file("source.md", bytes);
    let before = fs::metadata(&path).unwrap();
    let target = resolve(&path, ResolveOptions::default()).unwrap();
    assert_eq!(target.format(), Format::Markdown);
    assert_eq!(target.source_bytes(), bytes);
    assert!(fingerprint(&target).matches(bytes));
    assert_eq!(target.requested_path(), path);
    assert_eq!(target.resolved_path(), path);
    assert_eq!(
        before.modified().unwrap(),
        fs::metadata(&path).unwrap().modified().unwrap()
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn literal_text_is_not_parsed_as_markdown_or_normalized() {
    let fixture = Fixture::new();
    let bytes = b"# not a heading\n<script>literal</script>\r\n\0";
    let path = fixture.file("source.TXT", bytes);
    let target = resolve(path, ResolveOptions::default()).unwrap();
    assert_eq!(target.format(), Format::Text);
    assert_eq!(target.source_bytes(), bytes);
}

#[test]
fn format_inference_and_explicit_override_are_stable() {
    let fixture = Fixture::new();
    for (name, expected) in [
        ("a.md", Format::Markdown),
        ("b.MD", Format::Markdown),
        ("c.txt", Format::Text),
    ] {
        let path = fixture.file(name, b"# literal");
        assert_eq!(
            resolve(&path, ResolveOptions::default()).unwrap().format(),
            expected
        );
        assert_eq!(
            resolve(&path, options(Format::Text)).unwrap().format(),
            Format::Text
        );
        assert_eq!(
            resolve(&path, options(Format::Markdown)).unwrap().format(),
            Format::Markdown
        );
    }
    for name in [
        "no-suffix",
        "source.markdown",
        "source.json",
        "source.md.txtx",
    ] {
        let path = fixture.file(name, b"unparsed");
        assert_eq!(
            resolve(&path, ResolveOptions::default())
                .unwrap_err()
                .code(),
            "format-required"
        );
        assert_eq!(
            resolve(&path, options(Format::Markdown)).unwrap().source(),
            "unparsed"
        );
        assert_eq!(fs::read(path).unwrap(), b"unparsed");
    }
    assert_eq!(Format::parse("markdown").unwrap(), Format::Markdown);
    assert_eq!(Format::parse("text").unwrap(), Format::Text);
    for value in ["json", "Markdown", "", "binary"] {
        let error = Format::parse(value).unwrap_err();
        assert_eq!(error.code(), "unsupported-format");
        assert!(error.to_string().contains("choose markdown or text"));
    }
}

#[test]
fn creation_requires_authorization_and_remains_unpublished() {
    let fixture = Fixture::new();
    let path = fixture.0.join("new.md");
    assert_eq!(
        resolve(&path, ResolveOptions::default())
            .unwrap_err()
            .code(),
        "creation-required"
    );
    let target = resolve(
        &path,
        ResolveOptions {
            create: true,
            format: None,
        },
    )
    .unwrap();
    assert!(matches!(target.baseline(), Baseline::Missing { .. }));
    assert_eq!(target.source_bytes(), b"");
    assert_eq!(target.resolved_path(), path);
    assert!(!path.exists());
    let ambiguous = fixture.0.join("new");
    assert_eq!(
        resolve(
            &ambiguous,
            ResolveOptions {
                create: true,
                format: None
            }
        )
        .unwrap_err()
        .code(),
        "format-required"
    );
    assert_eq!(
        resolve(
            &ambiguous,
            ResolveOptions {
                create: true,
                format: Some(Format::Text)
            }
        )
        .unwrap()
        .format(),
        Format::Text
    );
    assert!(!ambiguous.exists());
    let absent_parent = fixture.0.join("absent/new.md");
    assert_eq!(
        resolve(
            absent_parent,
            ResolveOptions {
                create: true,
                format: None
            }
        )
        .unwrap_err()
        .code(),
        "target-io"
    );
    assert!(!fixture.0.join("absent").exists());
}

#[test]
fn create_option_never_truncates_an_existing_file() {
    let fixture = Fixture::new();
    let path = fixture.file("existing.md", b"keep me\r\n");
    let target = resolve(
        &path,
        ResolveOptions {
            create: true,
            format: None,
        },
    )
    .unwrap();
    assert!(matches!(target.baseline(), Baseline::Existing { .. }));
    assert_eq!(target.source_bytes(), b"keep me\r\n");
    assert_eq!(fs::read(path).unwrap(), b"keep me\r\n");
}

#[test]
fn invalid_utf8_rejects_without_changing_bytes() {
    let fixture = Fixture::new();
    for (i, bytes) in [vec![0xff, 0xfe], vec![0xf0, 0x9f], vec![0xc0, 0xaf]]
        .iter()
        .enumerate()
    {
        let path = fixture.file(&format!("invalid-{i}.md"), bytes);
        for format in [Format::Markdown, Format::Text] {
            assert_eq!(
                resolve(&path, options(format)).unwrap_err().code(),
                "invalid-utf8"
            );
        }
        assert_eq!(fs::read(path).unwrap(), *bytes);
    }
}

#[test]
fn bounded_read_accepts_limit_and_rejects_larger_source() {
    let fixture = Fixture::new();
    let bytes = vec![b'a'; MAX_SOURCE_BYTES];
    let path = fixture.file("limit.txt", &bytes);
    assert_eq!(
        resolve(&path, ResolveOptions::default())
            .unwrap()
            .source_bytes()
            .len(),
        MAX_SOURCE_BYTES
    );
    let large = fixture.file("large.txt", &vec![b'a'; MAX_SOURCE_BYTES + 1]);
    assert_eq!(
        resolve(&large, ResolveOptions::default())
            .unwrap_err()
            .code(),
        "source-too-large"
    );
    assert_eq!(
        fs::metadata(large).unwrap().len(),
        (MAX_SOURCE_BYTES + 1) as u64
    );
}

#[test]
fn directories_urls_and_empty_targets_are_rejected() {
    let fixture = Fixture::new();
    for path in [
        fixture.0.clone(),
        PathBuf::from("https://edit.ac/document.md"),
        PathBuf::from("ac://document/123"),
        PathBuf::from("ac:123"),
        PathBuf::from(""),
    ] {
        assert_eq!(
            resolve(path, options(Format::Text)).unwrap_err().code(),
            "unsupported-target"
        );
    }
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
}

#[test]
fn identities_and_fingerprints_distinguish_replacement_from_content_change() {
    let fixture = Fixture::new();
    let path = fixture.file("identity.md", b"same");
    let before = resolve(&path, ResolveOptions::default()).unwrap();
    let same = resolve(&path, ResolveOptions::default()).unwrap();
    assert_eq!(before.baseline(), same.baseline());
    fs::write(&path, b"edit").unwrap();
    let edited = resolve(&path, ResolveOptions::default()).unwrap();
    assert!(!fingerprint(&before).matches(edited.source_bytes()));
    let identity = |target: &editac_cli::target::LocalTarget| match target.baseline() {
        Baseline::Existing { identity, .. } => *identity,
        _ => panic!(),
    };
    assert_eq!(identity(&before), identity(&edited));
    let replacement = fixture.file("replacement.md", b"same");
    fs::rename(replacement, &path).unwrap();
    let replaced = resolve(path, ResolveOptions::default()).unwrap();
    assert!(fingerprint(&before).matches(replaced.source_bytes()));
    assert_ne!(identity(&before), identity(&replaced));
}

#[cfg(unix)]
#[test]
fn symlinks_dangling_links_and_special_files_never_open_or_mutate() {
    use std::os::unix::{fs::symlink, net::UnixListener};
    let fixture = Fixture::new();
    let path = fixture.file("real.md", b"preserve");
    let link = fixture.0.join("linked.md");
    symlink(&path, &link).unwrap();
    let dangling = fixture.0.join("dangling.md");
    symlink(fixture.0.join("absent.md"), &dangling).unwrap();
    let socket = fixture.0.join("socket.md");
    let _listener = UnixListener::bind(&socket).unwrap();
    for target in [&link, &dangling, &socket] {
        assert_eq!(
            resolve(
                target,
                ResolveOptions {
                    create: true,
                    format: None
                }
            )
            .unwrap_err()
            .code(),
            "unsupported-target"
        );
    }
    assert_eq!(fs::read(path).unwrap(), b"preserve");
    assert!(!fixture.0.join("absent.md").exists());
    assert_eq!(fs::read_link(link).unwrap(), fixture.0.join("real.md"));
}

#[cfg(unix)]
#[test]
fn read_only_targets_open_without_requiring_write_access() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let path = fixture.file("readonly.txt", b"offline");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    let target = resolve(&path, ResolveOptions::default()).unwrap();
    assert!(matches!(
        target.baseline(),
        Baseline::Existing { readonly: true, .. }
    ));
    assert_eq!(target.source(), "offline");
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o444
    );
}

#[cfg(unix)]
#[test]
fn canonical_parent_and_unicode_local_names_preserve_target_identity() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let alias = fixture.0.join("alias");
    symlink(&fixture.0, &alias).unwrap();
    let name = std::ffi::OsString::from("local-🦀-é.txt");
    let path = fixture.0.join(&name);
    fs::write(&path, b"exact").unwrap();
    let target = resolve(alias.join(&name), ResolveOptions::default()).unwrap();
    assert_eq!(target.resolved_path(), path);
    assert_eq!(target.source_bytes(), b"exact");
    assert_eq!(
        target.baseline(),
        resolve(&path, ResolveOptions::default())
            .unwrap()
            .baseline()
    );
}

#[test]
fn offline_probe() {
    if let Some(path) = std::env::var_os("EDITAC_OFFLINE_TARGET_PROBE") {
        assert_eq!(
            std::env::vars_os().count(),
            1,
            "probe environment must contain only its fixture path"
        );
        for (name, expected) in [
            ("offline.md", Format::Markdown),
            ("offline.txt", Format::Text),
        ] {
            let target =
                resolve(PathBuf::from(&path).join(name), ResolveOptions::default()).unwrap();
            assert_eq!(target.format(), expected);
            assert_eq!(target.source_bytes(), b"# offline\r\n");
        }
    } else {
        let fixture = Fixture::new();
        fixture.file("offline.md", b"# offline\r\n");
        fixture.file("offline.txt", b"# offline\r\n");
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "offline_probe", "--nocapture"])
            .current_dir(&fixture.0)
            .env_clear()
            .env("EDITAC_OFFLINE_TARGET_PROBE", ".")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
    }
}

#[cfg(unix)]
#[test]
fn trailing_separator_cannot_reinterpret_a_file_as_a_valid_target() {
    let fixture = Fixture::new();
    let path = fixture.file("file.md", b"unchanged");
    let trailing = PathBuf::from(format!("{}/", path.display()));
    assert_eq!(
        resolve(trailing, ResolveOptions::default())
            .unwrap_err()
            .code(),
        "unsupported-target"
    );
    assert_eq!(fs::read(path).unwrap(), b"unchanged");
    let missing = PathBuf::from(format!("{}/missing.md/", fixture.0.display()));
    assert_eq!(
        resolve(
            missing,
            ResolveOptions {
                create: true,
                format: None
            }
        )
        .unwrap_err()
        .code(),
        "unsupported-target"
    );
    assert!(!fixture.0.join("missing.md").exists());
}

#[test]
fn empty_existing_source_is_distinct_from_authorized_creation() {
    let fixture = Fixture::new();
    let existing = fixture.file("empty.txt", b"");
    let target = resolve(existing, ResolveOptions::default()).unwrap();
    assert_eq!(target.source(), "");
    assert!(fingerprint(&target).matches(b""));
    let missing = resolve(
        fixture.0.join("new.txt"),
        ResolveOptions {
            create: true,
            format: None,
        },
    )
    .unwrap();
    assert_eq!(missing.source(), "");
    assert!(matches!(missing.baseline(), Baseline::Missing { .. }));
    assert_ne!(target.baseline(), missing.baseline());
}
