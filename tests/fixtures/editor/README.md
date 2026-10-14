# External-editor session checks

`tests/editor.rs` invokes the executable `fake editor.sh` directly, with a mode,
payload and absolute working-buffer path as separate arguments. It needs only
native `/bin/sh`, `cp`, `dd`, `rm`, `mkdir` and `ln`; no account or transport.
The changed payload is committed here. Other outcomes are deterministic script
operations (exit 130/SIGINT cancel, exit 7/SIGTERM fail, invalid/oversized/missing
or replaced buffers). Test workspaces are removed by fixture teardown.

The library selects explicit configuration before nonempty VISUAL, then EDITOR.
Its parser separates words with whitespace, accepts single/double quotes and
backslash escapes, preserves empty quoted arguments, and performs no expansion.
Inside single quotes backslashes are literal. Use quotes around an executable or
argument containing spaces; an unclosed quote, trailing escape, NUL or empty
executable fails. The configured executable and arguments are passed to
`std::process::Command`, followed by one absolute source-file path, never a shell
command. `$HOME`, semicolons, backticks and `$(...)` are literal argument data.
An editor needing a different cancel code can use an explicit wrapper.

Call `editor::run` with a resolved `LocalTarget`, `EditorCommand` and an existing,
caller-controlled recovery directory. This native boundary currently requires
Unix (as does target identity acquisition); tests here qualify macOS/APFS only.
An exclusive 0700 session directory contains 0600 exact baseline and working
files. The configured editor is trusted native code. Sessions are not a sandbox.
Only a successful, changed, bounded UTF-8 buffer yields `ChangedBuffer`; its
original target baseline remains available for future reconciliation/stale-save
checks. No-op, cancel and failure never save or create the opened target. Cancel
and failure keep the draft/baseline, including raw rejected bytes, and errors name
recovery paths. Files are removed only by explicit `RecoveryFiles::discard`.

These files are recoverable drafts, not a committed journal, a target save, a
power-loss durability claim, whole-client or cross-platform qualification. Help
continues to expose only scaffold capabilities until preparation/save are ready.
