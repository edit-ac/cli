# edit.ac CLI

Rust editor client for local-first document editing. The intended workflow uses
Markdown as the editable representation for local files and online hosted
documents. Local editing works without an account or network connection; hosted
documents add a recoverable local cache and optional synchronization.

This scaffold currently supports `--help` and `--version` only. Editing,
synchronization, validation and conversion commands are not implemented. Unknown
arguments return exit code 2.

The planned first workflow opens Markdown in the user's configured text editor
and applies validated changes through the shared Rust engine. Literal text files
are supported without Markdown interpretation. Hosted editing preserves document
identity and history, keeps offline work locally, and reports local saving
separately from remote durability. Concurrent file or hosted edits require safe
reconciliation or an explicit conflict; a stale Markdown buffer cannot silently
replace newer content.

The authoritative [CLI contract](https://github.com/edit-ac/architecture/blob/main/specs/cli.md)
and [development roadmap](https://github.com/edit-ac/architecture/blob/main/roadmaps/11-offline-cli.md)
define staged implementation gates. The CLI reuses the shared engine and Markdown
semantics. Local editing can ship before hosted connectivity is qualified.

## Development

Use Rust 1.97.1:

```sh
cargo fmt --all -- --check
cargo check --all-targets --locked --offline
cargo test --locked --offline
cargo run --locked --offline -- --help
```

There are currently no external dependencies or implemented data commands.
This is a public Apache-2.0 repository, with Cargo publishing disabled until a
useful implementation passes its release gates. The binary is named `editac`.
