### Fixed

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

### Added

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

### Removed

- `examples/cockroach-rollout-agent.sudoers`.
