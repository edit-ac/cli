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
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "editac — local-first editor client scaffold

Usage: editac [--help | --version]

Options:
  --help       Show this help
  --version    Show the development version

No document commands are implemented.
"
    );
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
fn unimplemented_commands_fail_without_touching_targets() {
    for command in ["edit", "status", "sync", "validate", "convert", "inspect"] {
        let output = run(&[command, "/nonexistent/document.md"]);
        assert_eq!(output.status.code(), Some(2), "{command}");
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "Unsupported argument. Run editac --help; no data commands are implemented.\n"
        );
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
