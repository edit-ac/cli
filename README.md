# edit.ac CLI

A local Markdown and literal UTF-8 editor client with a built-in terminal UI and
an external-editor mode. Local editing requires no account or network connection. Optional hosted editing
uses an explicitly configured service adapter and private native cache. Native artifacts
are development builds; no registry release or publication is claimed.

Start the terminal editor from an interactive terminal:

```sh
editac tui --help
editac tui notes.md --store /absolute/private/notes.sqlite
editac tui notes.txt --create --store /absolute/private/new-notes.sqlite
```

Create the private parent directory first (mode 0700). Use a new store for each
new target; reuse the same store/target to recover. Ctrl-S saves; Ctrl-Q quits
and retains drafts without automatically saving the file. The packaged
[terminal guide](docs/tui.md) covers keys, hosted catalog grants, conflict/recovery,
save states and the actual support matrix. macOS arm64 PTY qualification does
not qualify a terminal application or accessibility stack.

External-editor commands remain available:

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
the original untouched. Exit 0 reports `unchanged`, `cancelled`, `locally-saved` or a hosted `conflict`.
Inspect the JSON state; a retained conflict is not a successful save.
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

The archive contains the binary, this README, `docs/tui.md`, Apache-2.0 license, third-party
notices, immutable source provenance and per-file hashes. Packaging is local;
Cargo publishing stays disabled. Qualification counts, failures and platform
limits are recorded in the architecture local-client evidence receipt.


## Configured hosted editing

```sh
editac edit https://app.example.test/ac/DOCUMENT-UUID --service-config service.json --cache-dir /private/cache --adapter /absolute/path/to/host-adapter --editor 'vim -f'
editac edit https://app.example.test/ac/DOCUMENT-UUID --service-config service.json --cache-dir /private/cache --offline --editor 'vim -f'
editac status https://app.example.test/ac/DOCUMENT-UUID --service-config service.json --cache-dir /private/cache
editac sync https://app.example.test/ac/DOCUMENT-UUID --service-config service.json --cache-dir /private/cache --adapter /absolute/path/to/host-adapter --attempts 3
```

Replace the illustrative URL/UUID and adapter with your authorized host settings.
`service.json` contains `host`, HTTPS `endpoint` and a complete `identity` envelope
(`profile`, `schema`, `protocolVersion`, `updateEncoding`, `offsets`, `documentId`,
`epoch`). It contains no bearer credentials. The configured host owns URL identity,
authorization, saved-credential access and the existing service protocol. Arbitrary
web pages are not writable targets. Configuration is bounded to 8192 bytes and
must name the exact expected schema/epoch. The cache parent must already exist as
a private non-symlink directory (0700). No default production endpoint, ambient
credential search, background daemon or built-in network client is provided.

The adapter is a trusted absolute executable, invoked directly with no arguments
or inherited environment, with one local JSON request on stdin and one
`{"ok": value}` or `{"error": code}` response on stdout. This is host IPC, not a
new server protocol. Every request includes an attachment identifier. Operations:
`credential(endpoint)` returns a token or null; `resolve(endpoint, documentId,
credential)` returns the complete stored identity; `bootstrap(endpoint, identity,
credential)` returns `{identity, initialized, writerId, bytes}` for an initialized
CRDT checkpoint. `connect(endpoint, identity, credential)` returns `{identity,
receiptProtocol}`; `exchange(frames)` returns baseline v1 frames; `upload(entry)`
returns `{receipt}`; `disconnect` returns null. Error codes are
`unavailable`, `authorization`, `revoked`, `incompatible`, `invalid-protocol`.
The host adapter must provide its existing credential-store and service integration;
the prepared adapter in tests is a fixture. CLI responses never print tokens or
adapter stdout/stderr. Each adapter invocation has a three-second deadline and
96 MiB IPC limit, with private temporary files removed and the child reaped.

The first online edit authorizes and bootstraps once; later opens restore the same
native cache with distinct live writer identities. Explicit `--offline` never
loads credentials or invokes an adapter, and refuses an uncached target. It does
not accept `--adapter`. Hosted Markdown is a deterministic projection of the
replica; it need not preserve the original author's spelling. No-op/cancel leaves
committed state untouched in offline mode, retaining private source sessions.
Online opening/refresh may independently receive or acknowledge existing work.

Online editing receives updates before opening and after the editor exits, then
revalidates the original semantic anchors. Proven disjoint changes survive;
overlap/deletion/unsupported semantics returns `state: conflict` with private
base/local/current source and current initialized checkpoint in `conflict.json`.
No stale full-document replacement is uploaded. The cache's separate `draft.json`
and returned source-session directory retain unresolved source, including editor
failure/cancellation. A new edit refuses an unresolved native draft. To resolve,
copy the retained source to an ordinary local Markdown file, inspect the current
cached projection and merge explicitly; keep the evidence and draft until resolved.
There is no automatic conflict-resolution command. Local `recover` operates on
local editing sessions; it does not reinterpret a hosted cache as a local session.

Admitted edits persist identified journal bytes before any upload and clear only
the matching admitted draft after the local file/directory durability barrier.
`status` is offline and distinguishes `locallySaved`, `pendingUpload`,
`remoteCommittedUpdates`, `remoteDurability`, `unresolvedDraft`, `conflict` and
`connection`. `locally-saved` does not mean remotely saved. Only exact negotiated
durable application receipts permit `remotely-committed`; socket delivery, state
vectors and missing/wrong receipts leave remote durability `unknown`. Disconnect,
revocation and incompatible schema/epoch retain local commits and retransmittable
bytes. Reopen offline to retain/export local work; explicit `sync` retries it.
The native transport bounds each batch to 32 frames/16 MiB and 32 uploads, with
1–5 attempts and capped backoff; large pending sets may require another explicit
sync. No hardware power-loss, network-filesystem or universal OS guarantee is made.

Hosted qualification uses the real external-editor/cache/reconciliation/transport
command path with prepared local service/auth/receipt executables, including
concurrency, reconnect, revocation and wrong/missing receipts. It never uses live
credentials or production documents. The supported tested envelope is macOS
arm64/APFS; Linux CI is configured but is not local execution evidence. Windows
is unsupported. Counts, failures and package hashes live in
`architecture/evidence/cli/hosted-client/receipt.json` in the architecture repository.
Local-only guarantees are separately replayed against the same native package.
No hosted backend deployment or registry publication is implied.

```sh
cargo +1.97.1 test --test hosted_workflows --test commands --locked --offline
python3 scripts/package-local.py --profile hosted --binary target/release/editac --output target/hosted-package --revision "$(git rev-parse HEAD)"
python3 scripts/probe-hosted.py --archive target/hosted-package/editac-hosted.tar.gz --receipt target/hosted-package/hosted-probes.json
```
