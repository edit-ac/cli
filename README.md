# edit.ac CLI

Reserved Rust command-line utility for edit.ac development. This scaffold supports
`--help` and `--version` only. It does not validate, convert, repair, synchronize,
or edit documents. Unknown arguments return exit code 2.

Implementation is deferred until the shared document schema and fixture contracts
are stable. Future work belongs to the
[development roadmaps](https://github.com/edit-ac/architecture); the CLI must reuse
the core semantics rather than create a separate editor model.

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
