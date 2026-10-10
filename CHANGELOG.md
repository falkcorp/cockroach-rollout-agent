# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- scriv-insert-here -->

<a id='changelog-v0.0.1'></a>
## v0.0.1 — 2026-10-10

### Added

#### Adopt the `todo.d/` fragment system for `TODO.md`

New tasks are now added by dropping a uniquely-named Markdown fragment in
`todo.d/` instead of editing `TODO.md` directly, so parallel PRs no longer
collide on the TODO list — the same fragment-per-change model this repo already
uses for `CHANGELOG.md`.

`scripts/assemble_todo.py` folds fragments in below the
`<!-- todo-insert-here -->` marker and deletes the ones it consumed;
`.github/workflows/todo-collect.yml` runs it daily and on `workflow_dispatch`.
The system is add-only (checking a task off stays a direct edit) and opt-in by
presence of `todo.d/todo.ini`.

- `approve <version>`: the leader only proposes the next step, and nothing
  installs until an operator approves it.
- `cancel`: stops the open rollout.
- `status`: shows rollouts, nodes, agents, leases and cluster health.
- `scripts/install-rollout-agent.sh`: an idempotent per-host installer. It
  converts the binary layout without restarting CockroachDB.

### Changed

- `self-check` now checks, without stopping anything:
  - the binary layout;
  - that the agent's directories are writable through the sandbox;
  - polkit restart authorization;
  - SQL access.
- The `daemon --allow-breaking-warnings` flag is replaced by
  `approve --accept-release-note-warnings`.

#### Adopt changelog fragments (`changelog.d/`) for assembling CHANGELOG.md

`CHANGELOG.md` is now assembled from per-change Markdown fragments under
`changelog.d/` by `scriv`, instead of being edited by hand. Contributors add a
fragment with `scriv create`; a CI check requires one on each PR, and the
fragments are folded into `CHANGELOG.md` when a release is published. This
removes changelog merge conflicts across parallel PRs.

#### Document the changelog/TODO fragment system for AI agents

`CLAUDE.md` and `.github/copilot-instructions.md` now instruct AI agents to use
the `changelog.d/` and `todo.d/` fragment systems instead of editing
`CHANGELOG.md` or the `TODO.md` inbox directly, preventing parallel-PR
collisions on those files.

### Removed

- `examples/cockroach-rollout-agent.sudoers`.

### Fixed

#### SQL coordination could not connect to any secure CockroachDB cluster

`db_client` built a bare `native-tls` connector with no root certificate and no
client identity, so every SQL-coordinated command — `init-db`, `discover`,
`daemon` — failed against a normal cluster. `sslmode=require` died at
`error performing TLS handshake`, because only the system trust store was
consulted and CockroachDB deployments use a private CA. Supplying libpq's
`sslrootcert`/`sslcert`/`sslkey` in the connection string failed differently,
with `invalid connection string`, because the `postgres` crate's parser
understands `sslmode` and nothing else. There was no combination that worked,
which made the recommended SQL-coordinated deployment unreachable in practice.

TLS material is now configured outside the URL, through `--ssl-root-cert`,
`--ssl-client-cert` and `--ssl-client-key` (and the matching
`CROACH_ROLLOUT_SSL_*` environment variables), and wired into the connector via
`add_root_certificate` and `Identity`.

One sharp edge is called out rather than papered over: `native-tls` accepts only
PKCS#8 keys, while `cockroach cert create-client` emits PKCS#1. That case is
detected and the error names the `openssl pkcs8 -topk8` conversion instead of
surfacing a bare parse failure.

#### Enabling the agent could take down the whole cluster

Every agent installed the moment a rollout became active, so all nodes
restarted together. Agents now take a cluster-wide `install` lease and must pass
a cluster health gate before restarting, so at most one node is down at a time.

#### A failed binary swap left the node stopped (#16)

The agent stopped CockroachDB, then failed to copy the new binary because
`/usr/local/bin/cockroach` is root-owned, and returned without restarting it.
The sudo fallback could never help, because the unit runs with
`NoNewPrivileges=true`.

The binary now lives in an agent-owned symlink layout. The new build is staged
and swapped in atomically while the node is still running, then the unit is
restarted. If the node does not rejoin on the target build, the agent swaps the
old binary back and restarts again. Restart authority comes from a polkit rule
scoped to the one CockroachDB unit. There is no sudo grant.

#### Rollouts could never complete

The heartbeat overwrote each agent's state with `running` on every tick. The
leader therefore never saw its own node as complete. Completion is now read
from the cluster itself: every non-decommissioned node must be live and report
the target `build_tag`.

#### `--dry-run` wrote real rollouts to the database

Dry-run now takes no leases and writes no coordination state.

#### `finalize` could not authenticate (#18)

`finalize` shelled out to `cockroach sql` with no connection settings. It now
runs over the agent's own authenticated connection. Approving a major-line step
also pins `cluster.preserve_downgrade_option`, so CockroachDB cannot finalize on
its own before an operator decides.

#### The unit name was silently wrong on some hosts (#19)

`CROACH_ROLLOUT_SERVICE` no longer defaults to `cockroachdb.service`, and the
examples use a placeholder. `self-check` probes restart authorization with
`pkcheck` and fails before a rollout rather than during one.

#### The arm64 release build could not link OpenSSL

Cross-compiling `openssl-sys` for `aarch64` on the amd64 runner failed for lack
of an arm64 OpenSSL. Each architecture now builds natively, arm64 on
`ubuntu-24.04-arm`, and a final job writes `SHA256SUMS`, uploads, and attests.

#### Releases shipped cargo build debris instead of binaries

Every release from rc.1 to rc.20 carried about 500 files from cargo's
`target/` directory and no usable binary: the shared Rust release build
uploaded the directory wholesale, and `release-assets.yml` never ran because
a release created with `GITHUB_TOKEN` does not trigger `release: published`.
The shared build is now disabled for this repo, and the release workflow calls
`release-assets.yml` directly, attaching the linux-amd64 and linux-arm64
tarballs, `SHA256SUMS`, and build attestations. The broken releases were
deleted; their tags remain.

### Security

#### Bump `quinn-proto` for RUSTSEC-2026-0185

`quinn-proto` 0.11.14 is affected by RUSTSEC-2026-0185, "Remote memory exhaustion
from unbounded out-of-order stream reassembly" (severity 7.5, high). Updated to
0.11.16, which clears `cargo audit`.

#### Root could run a binary the `cockroach` user controls

The installer made `/usr/local/bin/cockroach` a symlink into the agent's
directory, which the `cockroach` user can write. Anything running as that user
could replace the binary, and the next `sudo cockroach …` ran it as root. The
installer also ran the staged binary as root, and `--uninstall` followed the
agent-owned symlink as root and copied whatever it resolved to into a
world-readable `/usr/local/bin/cockroach`.

The CockroachDB unit now runs `/var/lib/cockroach-rollout-agent/bin/cockroach`
through an `ExecStart` systemd drop-in. `/usr/local/bin/cockroach` stays a
root-owned file the agent never touches; it can lag the server after a rollout.
The installer does every operation inside the agent root as `cockroach` and
never follows links there as root, and refuses to add the drop-in unless the
CockroachDB unit itself runs as `cockroach`. The client certificate directory
is now `root:cockroach 0750` so root never writes into an agent-owned path. `--uninstall` removes the drop-in and refuses
while the two binaries are different versions. The agent's `self-check` and
upgrade path verify the unit's `ExecStart` with `systemctl show`.
`CROACH_ROLLOUT_BINARY_PATH` now defaults to `<agent-root>/bin/cockroach`.

#### Bump `rustls` for RUSTSEC-2026-0285

`rustls` 0.23.40 is affected by RUSTSEC-2026-0285, "TLS 1.3 handshake messages
incorrectly accepted across encryption level boundaries" (severity 5.3,
medium). Updated to 0.23.45, which clears `cargo audit`.
