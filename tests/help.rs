use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_editac"))
        .args(args)
        .env_clear()
        .output()
        .expect("run packaged CLI without credentials or editor configuration")
}

#[test]
fn help_lists_only_implemented_options() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let help = String::from_utf8(output.stdout).unwrap();
    for command in [
        "edit <path>",
        "recover <session-directory>",
        "validate <path>",
        "inspect <path>",
        "convert <path>",
        "--input-format",
        "--output-format",
        "--allow-lossy",
        "--overwrite",
    ] {
        assert!(help.contains(command), "{command}");
    }
    assert!(help.contains("edit <hosted-url>"));
    assert!(help.contains("--offline"));
    for command in ["editac sync", "editac status"] {
        assert!(help.contains(command));
    }
}

#[test]
fn no_arguments_show_the_same_help() {
    let default = run(&[]);
    let explicit = run(&["--help"]);
    assert!(default.status.success());
    assert_eq!(default.stdout, explicit.stdout);
    assert!(default.stderr.is_empty());
}

#[test]
fn version_reports_the_pinned_engine_without_claiming_release() {
    let output = run(&["--version"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "editac {} (development scaffold; engine {})\n",
            env!("CARGO_PKG_VERSION"),
            editac_cli::EDITOR_REVISION
        )
    );
}

#[test]
fn client_commands_require_valid_targets_and_explicit_configuration() {
    for command in ["status", "sync"] {
        let output = run(&[command, "/nonexistent/document.md"]);
        assert_eq!(output.status.code(), Some(2), "{command}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn shared_markdown_prepares_engine_fragments_without_a_second_replica() {
    let prepared = editac_cli::markdown::import::import("# Hello 🦀\n").unwrap();
    assert_eq!(prepared.source, "# Hello 🦀\n");
    assert!(prepared.diagnostics.is_empty());
    let block: &editac_cli::engine::commands::editor::FragmentBlock = &prepared.blocks[0];
    assert_eq!(block.runs[0].text, "Hello 🦀");
    assert_eq!(
        block.attributes,
        Some(editac_cli::engine::document::BlockAttributes::Heading(1))
    );
    // These portable reconciliation types must remain available to later tasks.
    let _ = std::mem::size_of::<editac_cli::markdown::reconcile::SourceBaseline>();
    let _ = std::mem::size_of::<editac_cli::markdown::reconcile::PreparedSourceEdit>();
}

#[test]
fn vendored_shared_sources_match_the_immutable_provenance() {
    use sha2::{Digest, Sha256};
    use std::{fs, path::Path};
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/core");
    let p: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join("provenance.json")).unwrap()).unwrap();
    assert_eq!(p["revision"], editac_cli::EDITOR_REVISION);
    let hashes = p["sourceHashes"].as_object().unwrap();
    assert_eq!(hashes.len(), 59);
    for (path, expected) in hashes {
        if path == "Cargo.toml" {
            continue;
        }
        assert_eq!(
            format!("{:x}", Sha256::digest(fs::read(root.join(path)).unwrap())),
            expected.as_str().unwrap(),
            "{path}"
        );
    }
    let manifest = fs::read(root.join("Cargo.toml")).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&manifest)),
        p["vendoredManifestSha256"].as_str().unwrap()
    );
    let original = String::from_utf8(manifest).unwrap().replace(
        "members = [\"crates/editac-core\", \"crates/editac-markdown\", \"crates/editac-session\", \"crates/editac-diff\"]",
        "members = [\"crates/editac-core\", \"crates/editac-wasm\", \"crates/editac-markdown\", \"crates/editac-markdown-wasm\", \"crates/editac-ffi\", \"crates/editac-session\", \"crates/editac-diff\"]");
    assert_eq!(
        format!("{:x}", Sha256::digest(original)),
        hashes["Cargo.toml"].as_str().unwrap()
    );
}
