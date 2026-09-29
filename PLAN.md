<!-- file: PLAN.md -->
<!-- version: 1.0.0 -->
<!-- guid: 26cc964b-f21f-4cf6-bd60-eccb3a0c0ebe -->
<!-- last-edited: 2026-09-29 -->

# Safe, approved, one-node-at-a-time rollouts

## Goal

Make the agent safe to turn on for the 5-node production cluster: it must never
take more than one node down, never leave a node stopped, never start an upgrade
without an explicit `approve`, and actually be able to finish and finalize a
rollout. Then redeploy it to all five nodes (disabled until a separate go-ahead).

Decisions already made: symlink + atomic swap (no sudo, no root), explicit
approve, CLI `status` now and web UI later.

## Defects this fixes

| # | Defect | Fix |
|---|---|---|
| new | All followers install at once, so the whole cluster restarts together | One-at-a-time `install` lease + cluster health gate |
| new | `--dry-run` still writes rollouts/finalization to the DB | Dry-run never writes coordination state |
| new | Heartbeat overwrites `state` to `running` every tick, so the leader never sees itself complete | Heartbeat updates only `last_seen`; state is separate |
| new | Leader auto-publishes the newest release immediately | Leader writes a `proposed` rollout; `approve` activates it |
| new | Stale window = tick interval, so live agents look stale | Stale window = 3 × interval |
| #16 | Stop → copy fails → node left down; sudo fallback impossible under `NoNewPrivileges` | Stage + atomic symlink swap while running, then restart, health-check, auto-rollback |
| #18 | `finalize` shells out to `cockroach sql` with no config | Finalize over `db_client()`; pin `cluster.preserve_downgrade_option` on approve of a major hop, reset it on finalize |
| #19 | Service name hardcoded; self-check can't catch auth mismatch | Placeholder in examples; self-check probes polkit with `pkcheck` (non-destructive) and binary-layout writability |
| #17 | Releases had no tarballs | Verify rc.19 assets; cut rc.20 and confirm |

## Binary layout (per host)

```
/usr/local/bin/cockroach                       -> /var/lib/cockroach-rollout-agent/bin/cockroach   (root-owned, fixed, never changes)
/var/lib/cockroach-rollout-agent/bin/cockroach -> versions/cockroach-v25.3.0                       (agent swaps this by rename)
/var/lib/cockroach-rollout-agent/versions/cockroach-v25.3.0                                         (0755, previous kept for rollback)
```

The agent only ever writes inside `/var/lib/cockroach-rollout-agent`, so the unit
keeps `NoNewPrivileges` and `ProtectSystem=strict`. Restart authority comes from a
polkit rule scoped to user `cockroach` + the one cockroach unit.

## Affected files

- `src/main.rs`
  - schema: `agent_status.last_seen`, `rollouts.status` ∈ proposed/active/finalized/failed/cancelled, `install` lease row
  - `leader_reconcile`: propose only; complete/finalize logic; mark `failed` rollout halts everyone
  - `follower_reconcile`: acquire install lease → health gate → swap → restart → wait healthy → release, or roll back + mark failed
  - `install_artifact`: replaced by stage/swap/rollback on the symlink layout
  - `finalize_command`: SQL over `db_client()`
  - `self_check_command`: layout writability + `pkcheck` probe
  - new subcommands: `approve <version>`, `cancel`, `status`
  - remove `run_systemctl` sudo fallback
  - new unit tests for the pure decision functions
- `examples/cockroach-rollout-agent.service`: drop the `/usr/local/bin/cockroach` RW path, `After=@CROACH_ROLLOUT_SERVICE@`
- `examples/cockroach-rollout-agent.sudoers`: deleted
- `examples/50-cockroach-rollout-agent.rules`: new polkit rule
- `examples/cockroach-rollout-agent.env.example`: placeholder service name, SSL vars
- `scripts/install-rollout-agent.sh`: new, idempotent per-host installer; converts the binary to the symlink layout; `--service` auto-detected
- `docs/deploy.md`, `README.md`, `docs/security.md`: new permission model, approve flow, SQL grants
- `changelog.d/20260929-safe-rollout.md`: fragment
- ubuntu-autoinstall-agent: matching `ApplicationSpec` changes, in a separate PR in that repo, scoped from the subagent report

## Steps (one commit each)

1. Schema + heartbeat/state split + stale window + dry-run never writes.
2. Proposed/approve/cancel/status commands; leader proposes instead of publishing.
3. Symlink stage/swap/rollback install path; remove the sudo fallback.
4. Install lease + health gate (**you write the health predicate**; I'll scaffold it).
5. Finalize over SQL + preserve_downgrade handling.
6. Self-check (layout + `pkcheck`).
7. Examples, installer script, docs, changelog.
8. PR → CI green → merge → cut `v0.0.1-rc.20` → confirm tarballs + SHA256SUMS.
9. Deploy **disabled** to all 5 nodes:
   - len-serv-001/002/003 over SSH with passwordless sudo
   - U0 and U1: you run the installer via `! ssh -t …` (no passwordless sudo)
10. Enabled but idle: agents run with no approved rollout, `status` shows all 5 nodes, leader proposes v25.4.x. **Separate go-ahead from you before this.**
11. `approve`: **another separate go-ahead**, since it starts the real v25.3 → v25.4 upgrade.

## Test strategy

- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
- Unit tests:
  - propose/approve state machine
  - install-lease eligibility
  - health predicate
  - stale-window math
  - rollback-on-restart-failure (swap logic against a tempdir)
- Live, on len-serv-001, scratch only: use a scratch `CROACH_ROLLOUT_SCHEMA`, a dummy systemd unit (`rollout-test.service` running `sleep`), and a scratch binary dir. Exercise swap → restart → health fail → rollback. The real `cockroach.service` is never touched.
- `self-check` on each host after install must pass, including the `pkcheck` probe.

## Rollback

- Code: revert the PR; hosts keep running the old agent, which is disabled.
- Host layout: `install-rollout-agent.sh --uninstall` restores `/usr/local/bin/cockroach` as a real file, copied from the current version target, and removes the polkit rule.
- Mid-rollout: `cockroach-rollout-agent cancel` marks the rollout cancelled, and followers stop taking the lease. Any already-upgraded node stays on the new binary, which is supported in mixed-version mode until finalization. Downgrade is only possible before finalize: approve the old version back, and the swap reverses.

## ubuntu-autoinstall-agent follow-up (separate PR, after this one merges)

This work only applies at install time. There is no way to re-apply config to a
host that is already running, so live hosts use `scripts/install-rollout-agent.sh`
from this repo.

- `config.rs` `CockroachRolloutAgentSpec` (:316-350, defaults :804-821):
  - set `certs-dir` to `/etc/cockroach-rollout-agent/certs`
  - add the client cert/key source, as a dedicated `rollout` PKCS#8 key rather than the node cert
  - fix the round-trip test (:982)
- `applications.rs` `install_cockroach_rollout_agent` (:149-247):
  - symlink layout
  - polkit rule
  - `After=` derived from `spec.service`
  - `ReadWritePaths` without `/usr/local/bin/cockroach`
  - fix the env keys
  - add a mock-executor test
- `applications.rs` `install_cockroach` (:392-399): `cp -f` writes through the symlink, so make it symlink-aware.
- **Security:** today's spec points the agent at `node.crt`/`node.key`, which authenticates as the `node` user, effectively root. Switch it to the least-privilege `rollout` identity.
- uaa-control CA (`crates/uaa-control/src/ca.rs`): check whether it can issue a client cert for the `rollout` SQL user.
- `scripts/vm-validate.sh:821-831` and the two example yamls: update them.

## Needs from you

- The read-only cluster query I was blocked from running, on len-serv-001:
  `SHOW CLUSTER SETTING version; SHOW CLUSTER SETTING cluster.preserve_downgrade_option; SHOW GRANTS FOR rollout;`
- SQL grant for `rollout`: `GRANT SYSTEM MODIFYCLUSTERSETTING TO rollout;`, needed for preserve_downgrade and finalize.
