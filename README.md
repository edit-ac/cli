# edit.ac CLI

A local Markdown and literal UTF-8 editor client. Editing requires an external
editor executable, no account and no network connection. Hosted editing,
synchronization and remote durability commands are unavailable. Native artifacts
are development builds; no registry release or publication is claimed.

```sh
editac edit notes.md --editor 'vim -f'
editac edit notes.md --create --editor 'vim -f'
editac edit notes.txt --editor 'nano'
editac edit source.data --format text --editor 'nano'
editac recover /path/to/.editac-recovery/session-id
editac recover /path/to/.editac-recovery/session-id --output recovered.md
```

`--editor` wins over nonempty `VISUAL`, then `EDITOR`. Configuration parses
whitespace-separated words, single/double quotes and backslash escapes; single
quotes keep backslashes literal. There is no variable expansion or shell
substitution. The private source path is appended as one argument. Use a blocking
editor (for example `vim -f`, or an appropriate wait flag for a graphical editor).
The editor is trusted executable code. Exit 130 or SIGINT means cancellation;
other unsuccessful exits return code 2 with recovery paths. A successful unchanged
buffer preserves the original bytes and file identity; cancellation also leaves
the original untouched. Exit 0 reports `unchanged`, `cancelled` or `locally-saved`.
Other errors return code 2; SIGTERM/process interruption has the OS signal result.

`.md` selects Markdown and `.txt` selects literal text; other suffixes require
`--format markdown` or `--format text`. Missing targets require `--create`.
Successful editing keeps exact source spelling, Unicode and declared line endings.
The qualified Markdown profile supports proven text, marks, block presentation and
check-state edits with unchanged block count/container topology. Structural edits,
ambiguous mappings, changed unknown semantics and unsupported opening diagnostics
are refused with the source retained. Literal mode never interprets Markdown.
Inputs are capped at 1 MiB; shared parser node/run/depth limits also apply.

Recovery defaults to a private `.editac-recovery` directory beside the resolved
target. `--recovery-dir` selects an existing parent or a newly created directory;
existing roots must be directories with mode 0700. Each session contains the
opening checkpoint, exact baseline/editor buffer, independent draft, and any
committed identified native update. A draft persists before semantic commit;
committed updates persist before target publication. `locally-saved` is emitted
only after target file and directory flushes pass. A stale/replaced file, failed
save, or uncertain publication retains recovery. Recovery restores native state,
reports retained update count and exports the exact draft to a **new** explicit
path; it never automatically reapplies a draft over the original. If interruption
occurred in the external editor, the retained editor buffer is available instead.
Corrupt drafts and ambiguous/missing buffers are refused. If native committed
state cannot restore, an independently validated draft can still export with
`draft-recovered`, `committedState: unavailable` and an unknown update count;
this does not initialize a replacement document. A corrupt latest
checkpoint may fall back to a previous validated generation, reported by `fallback`.
Keep this directory until work is resolved; successful changed sessions remain
available for explicit recovery. No-op sessions are removed. Ordinary text files
remain usable independently of this directory.

Native saves use cooperating locks, baseline identity/byte revalidation and a
same-directory temporary file, file flush, atomic publication and directory flush.
Non-cooperating writers can still race after revalidation: rename is not a
compare-and-swap. Source conflicts preserve the external target and local draft.
POSIX permissions are preserved; unsupported ACLs/attributes are refused. The
qualified artifact envelope is macOS arm64/APFS. Other Unix filesystems/platforms
remain candidates until their checks are executed; Windows native identity/save
is unsupported. Read-only/full-disk and flush/publication failures are covered by
deterministic native adapter fixtures, not physical power-loss experiments.
`remoteDurability: not-applicable` never asserts remote storage or upload.

Secondary commands require explicit formats:

```sh
editac validate notes.md --input-format markdown
editac inspect snapshot.json --input-format json
editac convert notes.md --input-format markdown --output-format json --output snapshot.json
```

`--allow-lossy` explicitly accepts conversion loss; `--overwrite` authorizes a
different existing export path. Identified incremental update inspection needs an
initialized `--base`; raw unqualified CRDT bytes are not editable source.

Build with Rust 1.97.1 and the committed lockfile. Engine/Markdown source is vendored
at the immutable revision in `vendor/core/provenance.json`; no sibling repository
is needed. Dependency acquisition can occur at build setup; test/editor execution
uses deterministic fixtures and no credentials or provider transports.

```sh
cargo +1.97.1 fetch --locked
cargo +1.97.1 fmt --all -- --check
cargo +1.97.1 test --test local_workflows --test commands --locked --offline
cargo +1.97.1 test --locked --offline
cargo +1.97.1 build --release --locked --offline
python3 scripts/package-local.py --binary target/release/editac --output target/local-package --revision "$(git rev-parse HEAD)"
```

The archive contains the binary, this guide, Apache-2.0 license, third-party
notices, immutable source provenance and per-file hashes. Packaging is local;
Cargo publishing stays disabled. Qualification counts, failures and platform
limits are recorded in the architecture local-client evidence receipt.
