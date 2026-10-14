# Terminal editor

`editac tui` is the built-in source editor in this development artifact. Extract
the archive and run `bin/editac tui --help`; the binary needs no sibling source
checkout, external editor, account or MCP server for local editing. The existing
`editac edit … --editor …` workflow remains available.

## Open and recover local files

Create a private parent directory, then use absolute store paths:

```sh
mkdir -m 700 /tmp/editac-notes
bin/editac tui notes.md --store /tmp/editac-notes/notes.sqlite
bin/editac tui new.txt --create --store /tmp/editac-notes/new.sqlite
bin/editac tui source.data --format text --store /tmp/editac-notes/data.sqlite
```

Replace example paths with your own. Existing `.md` files select Markdown;
`.txt` selects literal UTF-8. Other suffixes require `--format markdown` or
`--format text`. Missing files require `--create`; this does not authorize
replacement of an existing file. Read-only files stay read-only. A fresh session
requires an unused absolute `--store` path in an existing private directory.
Reopen with the **same target and store** to recover the native document, pending
updates and draft/conflict state. A store is bound to its original target and
identity; reusing it for a different file is refused. Externally changed files
also require explicit recovery rather than silent replacement.

Keep the SQLite store, any SQLite sidecars, and `<store>.tui-recovery` together.
Do not delete them to resolve a failed save. Ctrl-E exports complete private
recovery, including source and semantic data. Protect that directory: exports
can contain the document. The external-editor `editac recover <session-directory>`
command uses a different recovery format; it does not import TUI stores.

Without terminal stdin **and** stdout, the command acquires the session and emits
a JSON `session-acquired` report. It creates/restores the store and recovery but
does not run editing or pretend to have saved the target. Use a TTY for the UI.

## Targets and host authority

```sh
bin/editac tui ac://DOCUMENT-UUID --catalog /absolute/private/catalog.json --offline
bin/editac tui https://app.example.test/ac/DOCUMENT-UUID --catalog /absolute/private/catalog.json --adapter /absolute/path/to/host-adapter
```

The host supplies the catalog and prepares its private native stores. There is
no automatic host discovery or production endpoint. The catalog has `authorities`
(`name`, optional `appHost`) and `entries` (`locator`, complete `identity`,
absolute `store` or null, `writable`, optional `expiresAt`, `access`). Identity
contains `documentId`, `epoch`, `schema`, `profile`, `protocolVersion`,
`updateEncoding` and `offsets`. The granting host must supply these exact values;
do not invent an identity or initialize an empty replacement for a missing cache.
`access` is `allowed`, `unauthorized`, `locked` or `incompatible`. Unknown catalog
fields reject, including credential fields. Treat host configuration as trusted
policy; a URI alone cannot grant permission.

`ac://DOC_ID` names a mutable document, not a bearer credential or immutable
content hash. Only configured adapter authorities map supported application URLs
to catalog entries. Arbitrary web URLs, unknown authorities, uncached identities,
unauthorized/locked/incompatible grants fail closed. Read-only or expired write
permission prevents mutations. Local files are never uploaded because a host
profile happens to exist.

No adapter is launched without explicit `--adapter`. It must be an absolute
trusted executable and is allowed only for cached host targets; `--offline`
rejects adapter attachment. A cached target can open offline without credentials.
An adapter adds bounded private IPC updates through the existing host/session
services, not a new identity or merge protocol. Do not put secrets in targets,
catalogs, source, terminal messages or evidence. No ambient credential discovery
or built-in network client is provided.

## Keys

| Context | Keys and behavior |
| --- | --- |
| Source | Left/Right move by grapheme; Up/Down keep the presentation column; Home/End move within a line. Shift extends selection; Ctrl-A selects all. |
| Input | Enter inserts a line break (CRLF when present in source); Tab inserts a tab; Backspace/Delete remove the selection or adjacent grapheme. Bracketed paste inserts literal text, at most 256 KiB per paste. The source budget is at most 1 MiB and is reduced by the opening buffer size. |
| Commit/save | Ctrl-Enter prepares and commits one local history group. Ctrl-S commits then saves the local target, or uses hosted persistence/flush. |
| History | Ctrl-Z undo; Ctrl-Y redo. These operate on committed local groups. Commit or resolve an uncommitted draft first; remote changes are not local typing history. |
| Review | Ctrl-D opens/closes diff. Up/Down, PageUp/PageDown, Home/End navigate. Ctrl-Enter revalidates and commits the reviewed candidate; Esc returns to source. |
| Recovery | Ctrl-R retries storage recovery. Ctrl-E exports private source/semantic recovery. |
| Conflict | Ctrl-T reopens the retained conflict. Tab cycles base/local/current/resolution panes. Ctrl-L chooses local source; Ctrl-G chooses current source. Edit the resolution, Ctrl-P prepares, Ctrl-Enter applies. Esc cancels the view while retaining drafts. Ctrl-E exports base/local/current/resolution. |
| Exit | Ctrl-Q or Esc in source requests quit; Ctrl-C interrupts. Quit checkpoints drafts without automatically saving the target file. |

Some terminals do not distinguish Ctrl-Enter from Enter; use Ctrl-S for an editable
source commit/save. Conflict preparation/application needs the distinct key event;
export the draft if unavailable. PTY key fixtures do not establish real keyboard
compatibility. Terminal- or OS-reserved shortcuts may need user configuration.

Focus loss pauses editing input; resize redraws bounded source/status/help regions.
Source positions use graphemes and checked UTF-16 conversions; screen columns
are presentation only. Tabs use four-column stops. Wide/combining/ZWJ characters
have explicit mappings; directional and invisible control characters are shown
as escaped source. Logical source order is preserved; this is not a visual bidi
editor. Terminal control bytes are escaped and cannot execute document commands.

## Source, review and collaboration

Text mode is literal and has no Markdown shortcuts. Markdown retains exact source
spelling where shared mapping qualifies the edit. No-op preserves source bytes.
Unsupported structural changes, ambiguous/deleted identities or unknown module
content can reject preparation; the live document stays intact and the candidate
remains recoverable. A source/opaque fallback is not permission to normalize away
unknown content. Private exports retain semantic data alongside source.

Diff labels base/current revisions and exact source versus hosted Markdown
projection; additions, deletions, context and opaque limitations remain explicit.
The view is read-only until explicit revision-checked application. A stale preview
never authorizes a commit. Read-only sessions reject typing, paste, save, history
and resolution; permitted review and private export remain usable.

Clean buffers refresh from shared structured remote updates. Dirty buffers retain
base/local/current; qualified disjoint mappings reconcile through shared source
preparation. Overlap, incompatible epoch or ambiguous mapping retains a conflict.
A second remote change invalidates prepared resolution; inspect current state and
prepare again. Canceling a view does not discard the unresolved draft.

## Save states and interruption

Draft text is separate from a committed local operation. Local journal/checkpoint
durability, pending upload, sent-with-durability-unknown, authenticated remote
receipt, conflict and authorization state are distinct. A locally saved file or
successful adapter send never implies remote durability. Local file save reports
success only after its file/directory persistence barriers; stale external bytes
are retained rather than overwritten. After failed persistence, mutations freeze
until ordered recovery succeeds; newer drafts remain recoverable.

If quit encounters a flush failure, Ctrl-R retries recovery, Ctrl-Q retries exit,
Ctrl-E exits only with private recovery, and Esc cancels exit. Normal exit,
startup/read/draw failure and supported SIGINT/SIGTERM/SIGTSTP/SIGCONT paths restore
terminal ownership and dispose task resources. Forced termination cannot promise
a signal handler will run: restart depends on the last durable journal/draft.
Keep recovery artifacts after any uncertain result. A failed target publication
can leave committed local state while the target file is unchanged.

## Qualified environment and limits

| Cell | Recorded evidence and scope |
| --- | --- |
| macOS 26.4.1 (25E253), arm64 desktop test host | Native APFS/session persistence and packaged CLI consumers. TUI-1.11 exercises the actual binary in the OS PTY; TUI-1.12 adds clean-package help/local workflows. |
| Terminal protocol | `xterm-256color` PTY, scripted keys/paste, 240×24 plus 64×12 and 120×24 resizing. Python 3.9.6 harness; Rust 1.97.1, crossterm 0.29.0, ratatui 0.30.2. This is a test-host virtual terminal boundary. |
| Terminal applications / physical keyboard | Unqualified: Terminal.app, iTerm2, other emulators, SSH/multiplexers and actual key encoding each need recorded runtime evidence. |
| Accessibility | Unqualified: screen-reader speech, braille, assistive keyboard and high-contrast behavior need actual tested evidence or documented target-runtime virtual equivalents. Source/status/help text and escaped controls alone do not qualify accessibility. |
| Other OS / architectures | Unqualified: Linux and other Unix targets need their own packaged runtime checks. Windows native identity/save is unsupported. No cross-platform claim follows from backend availability. |

The archive includes this guide, binary, README, license/third-party notices,
immutable engine provenance, Cargo.lock and hashed manifest. Build/setup can fetch
dependencies; execution and prepared qualification require no service. Milestones
are recorded by the workspace release owner in `architecture/evidence/tui-pty.md`
and `architecture/evidence/tui-client.md`; those are evidence references, not runtime
dependencies. Physical devices, named simulators/emulators and desktop OS VMs/test
hosts can qualify their actual tested cells; unavailable hardware alone is not a
blocker. Unexecuted cells remain unresolved for their release owner. This local
development artifact does not imply publication or final cross-client qualification.
