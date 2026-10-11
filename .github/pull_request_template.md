<!-- file: .github/pull_request_template.md -->
<!-- version: 1.0.0 -->
<!-- guid: 6eb397d1-3446-4736-bb8c-1380f93125d8 -->
<!-- last-edited: 2026-10-11 -->

<!--
================================================================================
 INSTRUCTIONS FOR AI AGENTS (and humans) — read all of this, then delete
 nothing: these comments are invisible in the rendered PR. Replace only the
 visible placeholder lines below.
================================================================================

TITLE
  Conventional commit: `type(scope): description`, lowercase, imperative,
  no trailing period. Types: feat, fix, refactor, test, docs, chore, perf, ci.
  Scopes used here: rollout, ops, web, web-ui, events, schema, status,
  daemon, self-check, installer, release, deps, spec, plan, changelog.
  Example: `fix(rollout): fail a rolling restart whose node never comes back`

BEFORE OPENING — every item must be true, not assumed:
  1. File headers: every file you created or modified has its header
     (`file`, `version`, `guid`, `last-edited`) in that file's comment
     syntax. Bump `version` on every edit (minor = feature, patch = fix) and
     set `last-edited` to today. New files start at 1.0.0 with a fresh guid
     (`uuidgen | tr A-Z a-z`). Cargo.toml and Cargo.lock carry no header.
  2. Changelog: add a fragment under `changelog.d/` (`scriv create`, or a
     hand-written Markdown file named `YYYYMMDD-short-slug.md` with Keep a
     Changelog headings such as `### Added` / `### Fixed` / `### Changed`
     and a `####` title per entry). NEVER edit CHANGELOG.md by hand.
     The `Require changelog fragment` check fails without one; only skip it
     with the `skip-changelog` label or `[skip changelog]` in the title, and
     only for changes with nothing worth recording.
  3. TODOs: new tasks go in a fragment under `todo.d/`, never straight into
     the TODO.md inbox. Checking a task off is a direct TODO.md edit.
  4. Local gates, run from the repo root, all green:
       cargo fmt --all --check
       cargo clippy --all-targets -- -D warnings
       cargo test
       cargo audit
       cargo tree -e normal | grep -c aws-lc     # must print 0 (ring only)
  5. Tests: new behavior has a test you watched fail before the fix. Pure
     decisions belong in pure functions (see src/rollout.rs, src/ops.rs
     prechecks) so they are unit-testable without a database.

INVARIANTS THIS REPO MUST NEVER BREAK — say in the body which ones your
change touches and how you preserved them:
  - One node at a time: every restart or upgrade holds the `install` lease
    and passes `safe_to_restart` (fail closed if health cannot be measured).
  - Nothing installs or restarts without operator approval (`approve`, or
    starting a restart, which counts as approval). Finalize stays an
    explicit operator step unless auto-finalize is configured.
  - Rollback must be possible before anything changes; a failed install
    rolls its own node back and marks the rollout `failed`, halting others.
  - At most one open rollout (proposed/active/failed) of any kind; inserts
    use `WHERE NOT EXISTS`.
  - CLI and web UI enforce the same rules: every operator action goes
    through `src/ops.rs` prechecks; never duplicate checks in a handler.
  - Web UI: HTTPS only (never fall back to plain HTTP), every POST except
    /login checks CSRF, every dynamic value goes through `render::escape`,
    blocking DB clients only inside `spawn_blocking` (never on async
    workers), no `#[tokio::main]`.
  - SQL: user input only as bind parameters; schema names only via
    `sql_ident`.
  - Downgrade pinning: approving a major-line step sets
    `cluster.preserve_downgrade_option`; only finalize resets it.

WRITING THE BODY
  - Summary: what changed and WHY, for a reviewer with no context. Lead with
    the operator-visible effect. Link the spec/plan under
    docs/superpowers/ if one exists.
  - Changes: group by area; name files or modules, not line numbers.
  - Test plan: list exactly what you ran and its result (counts, not
    "tests pass"). Use unchecked boxes for anything still to be verified
    live on the cluster, and say who must do it.
  - Risk: what could go wrong in production and how it would show up
    (status output, events table, audit log, journal).
  - Never paste secrets, certificates, connection strings, real hostnames
    beyond the documented cluster IPs, or customer data.
  - End the body with the attribution footer your harness requires (for
    Claude Code: the "Generated with Claude Code" line and session link).

AFTER OPENING
  Watch CI (`gh pr checks <n> --watch`). If anything fails, fix it on this
  branch and push; do not merge red. Merge with `gh pr merge --rebase`
  (this repo is rebase/fast-forward only; never squash unless asked).
  Releases, `approve`, `finalize`, and anything that touches the live
  cluster always need an explicit go-ahead from the operator.
================================================================================
-->

## Summary

<!-- 1–3 sentences: the operator-visible effect and why it was needed. Link spec/plan if any. -->

## Changes

<!-- Grouped bullets by area, e.g. "**Rollout:** …", "**Web UI:** …", "**Docs:** …". -->

-

## Invariants touched

<!-- Name each invariant from the instructions above that this change touches, and how it still holds. Write "None" if none. -->

-

## Test plan

<!-- Commands you ran with their results (e.g. "cargo test: 68 passed"), then unchecked items still to verify live. -->

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test`
- [ ] `cargo audit`

## Risk and rollback

<!-- What could fail in production, how it would show up, and how to back it out. -->

## Checklist

- [ ] File headers bumped on every changed file
- [ ] Changelog fragment added under `changelog.d/` (or `skip-changelog` justified)
- [ ] New TODOs added as `todo.d/` fragments
- [ ] Docs updated (`README.md`, `docs/deploy.md`, `docs/security.md`) where behavior changed
