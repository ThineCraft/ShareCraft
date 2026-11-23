# ShareCraft: instructions for coding agents

ShareCraft is an open-source SharePoint alternative written in Rust (server, records, protocol,
collab-* crates) with plain JavaScript pages in `web/`. Read `handoff.html` first: it says where
every piece of work stands, what is next and why decisions were made. `BUILDING.html` is the
technical guide; `scope.html`, `sequencing.html` and `co-authoring-approach.html` hold the
product decisions.

## The owner's standing rules

- Never print the contents of `.env`, database URLs, passwords or link tokens, not even in logs
  or test output you paste back.
- Work on a branch. Merging, pushing, creating repositories and installing tools each need the
  owner's go-ahead. (Pushing a work branch so GitHub Actions can test it is approved.)
- Plans, specs, scope and handoff documents are `.html`, never `.md`. Tables and long lists need
  alternating row shading that works in both light and dark themes.
- Write for the owner in plain English. Say what was and was not checked.
- Tell the owner before starting anything large.
- Windows features must never need per-machine administrator work.
- Commits in this repository end with a `Co-Authored-By:` line for the agent that wrote them.
  (The GridCraft fork is the opposite: no attribution there; see its `AGENTS.md`.)
- Do not use OpenAI Codex for this work unless the owner asks again.
- ShareCraft runs on Windows and Linux servers. Never hide a failing test or feature behind
  `cfg(windows)`; close the Linux gap (File Share: Linux paths and mounted SMB/CIFS shares as well
  as Windows UNC and drive paths).

## How work is done here

- Every feature has a plan in `plans/*.html` with numbered tasks. Work one task at a time:
  write the failing tests first and record the red run, implement, run the gates, commit.
- Each plan has a ledger, `.superpowers/sdd/<plan>.html/progress.md` (git-ignored, on disk in
  the worktree that runs the plan). It records every commit, review finding and **ruling**.
  Rulings deliberately change the plan; read them before changing code they cover.
- After each task an independent review checks it against the plan; findings marked Critical or
  Important are fixed before the next task. A plan ends with a whole-branch review.
- Nothing reachable from network input may panic; bound every allocation from hostile input.
  `collab-core`, `collab-live`, `collab-text`, `protocol` and `web-client` must keep building
  for `wasm32-unknown-unknown`.

## Building and testing on this machine (Windows)

- Run cargo from `%TEMP%` with an absolute `--manifest-path` when building GridCraft: the parent
  folder `H:\Source\repos\.cargo\config.toml` holds a personal patch override. ShareCraft's own
  commands run from its folder.
- Build output goes to drive J:, one folder per worktree:
  `CARGO_TARGET_DIR=J:\cargo-target\<worktree-name>` and `CARGO_INCREMENTAL=0` (J: fills up).
  Use `CARGO_BUILD_JOBS=1` if the paging file runs out.
- Database tests: `cargo xtask test -p <crate> --test <name>` (it loads `.env` and the
  PostgreSQL URL). Never run the whole workspace locally; run only the tests for what you
  changed, then push the branch and let GitHub run everything.
- GitHub Actions (`.github/workflows/check.yml`) runs the full check in parallel jobs: lints,
  four test shards with cargo-nextest (retries flaky tests twice), WebAssembly builds and page
  scripts. Read failures with `gh run view <id> --log-failed`.
- `cargo xtask check` still runs everything locally; `cargo xtask licences|wasm|pages` run parts.
- Page scripts: `node --test web/*.test.mjs`.
- Known flaky tests (pass when run alone): `library_kinds::new_from_template_copies_latest_version_only`,
  `live_room::log_survives_server_restart`, the GitHub-source timing tests in `source_api`,
  `source_worker` and `source_sync`, and PostgreSQL error 53300 (out of connection slots) when
  many test processes run at once.
- If linking fails with LNK1104, point `TMP`/`TEMP` at a folder inside the workspace.
