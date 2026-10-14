# Qualified shared engine snapshot

This minimal snapshot comes from reviewed core revision `021c22c001f376c794370dd60beb495ab8755b56`. It contains the core, Markdown, session and diff crates consumed by the CLI. `provenance.json` binds every original source byte to its upstream path. The root manifest narrows workspace members; semantic sources are unchanged.

The upstream presenter/restore changes retain monitor qualification in `.roadmap-coordination/monitor/20261014T152503/integration-receipt.json`. Shared session transport lifecycle forwarding is qualified by `architecture/evidence/tui-remote/session-20261014/recovery-iteration1/core-session-final.log`, including native and memory replica/history/receipt cases. This is an immutable native dependency artifact, not a registry or release claim.

Replace sources only from a separately qualified upstream commit. Cargo.lock pins external dependencies; a sibling checkout is not required by consumers.
