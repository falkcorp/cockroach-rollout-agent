<!-- file: changelog.d/20261013-agent-self-update.md -->
<!-- version: 1.0.0 -->
<!-- guid: 0c8e7f4a-5d2b-4c1e-9a6f-3b7d2e8c1f45 -->
<!-- last-edited: 2026-10-13 -->

### Added

- **Agent self-update.** The leader proposes the newest stable agent release
  and pins each architecture's SHA-256 from its `SHA256SUMS`. You approve it in
  the **Agent updates** panel or with `agent-update approve <version>`.
  - Agents then update one at a time. Each one verifies the pinned digest,
    runs the new binary's `--version` and `self-check`, swaps
    `/var/lib/cockroach-rollout-agent/agent/bin/cockroach-rollout-agent`, and
    restarts by exiting with status 75.
  - A new binary that does not settle within two starts is rolled back by
    `agent-preflight`. Preflight runs from the root-owned copy before every
    start.
  - New commands: `agent-update show`, `agent-update decline <version>` (also
    stops an approved update) and `agent-update retry`.

### Changed

- The agent units now run the agent from its own layout, using a new
  `ExecStartPre=` (`agent-preflight`) and `RestartForceExitStatus=75`.
  - Rerun `install-rollout-agent.sh` once per host to convert it. After that,
    agent updates need no ssh or sudo.
  - `self-check` verifies the agent layout.
