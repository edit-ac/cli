# edit.ac cli

This checkout is one repository in the edit.ac workspace. Authoritative contracts and runner roadmaps live in the sibling `../architecture` checkout ([edit-ac/architecture](https://github.com/edit-ac/architecture)). Read the active task's named spec and paths; inspect actual symbols before implementation.

Use Codanna MCP before code text search. Use `rg` for literals, configuration, documents or incomplete index coverage. Keep tool output bounded and save full build/test logs before printing summaries.

Run at most one roadmap parent task per iteration, including validation. Source roadmaps are controller-owned; write progress/evidence to the separate configured progress file. Missing tests, zero matching tests, failures and skips do not satisfy acceptance. Record exact results and interrupted-work handoff. A task that outgrows its boundary needs a proposed controller split, not silently reduced acceptance.

Rust owns committed semantics over one Yrs replica. DOM/native controls own temporary input composition only. Prepare/validate before mutation; offsets at public boundaries are UTF-16, async targets are stable anchors. Read the relevant architecture spec before changing schema, history, persistence, wire or ABI contracts.

All automated checks use deterministic fixtures and prepared/mock transports. No paid inference, `.secrets` loading or automatic provider recording. Live-content AI dogfood is outside these roadmaps and requires separate exact scope, cap and stop condition; follow the user's paid-inference policy before such work.

Scaffold packages remain unpublishable until documented APIs, packed consumer checks and release gates pass. The selected project license is Apache-2.0. Use plain-English imperative commit subjects without Conventional Commit prefixes.

Track and clean up task-owned temporary servers/browsers; verify ownership before stopping anything. Preserve other tasks' processes, source, data and evidence. Check data-volume free space after substantial build/test work; remove only this session's inactive reproducible artifacts.

## Source-only Git policy — user decision, 2026-10-20

Keep source code, authored documentation, configuration and deterministic input fixtures in Git. Never commit generated caches, qualification evidence, screenshots, recordings, test reports, benchmark results, compiled binaries, packed dependencies, runtime/source snapshots or test databases. Exclude these outputs with .gitignore.

Run focused checks using ignored, task-owned temporary directories. Keep only the diagnostics needed for an active failure, then remove them after debugging; do not create gigabytes of evidence or routine artifact archives during development. Report concise pass/fail counts in the conversation or commit description. Release artifacts belong outside source repositories. This supersedes older blanket evidence-retention wording for generated development/test output; preserve source, real user data, credentials and active processes.
