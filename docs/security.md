<!-- file: docs/security.md -->
<!-- version: 2.1.0 -->
<!-- guid: b1107208-a9c3-4018-9e86-a44cbf5c7f79 -->
<!-- last-edited: 2026-09-29 -->

# Security Model

The rollout agent must be treated as privileged infrastructure automation.

## Abuse Resistance

- Agents never accept an unsolicited binary as sufficient authority to install.
- Every artifact must match signed metadata and an expected digest before
  installation.
- The updater refuses to cross more than one CockroachDB major version in a
  single action.
- The updater refuses alpha, beta, RC, and other prerelease CockroachDB builds.
- Upgrade plans move release-line by release-line using CockroachDB `vYY.R`
  major lines so each major-line upgrade can be finalized before the next one.
- Release notes are scanned before manifest generation. Warning matches block
  by default and require an explicit review override.
- If a PSK is used, it is supplied only at runtime and sent as a bearer token
  over TLS. The PSK is never stored in the repository or manifest.
- Rollout coordination should use a short-lived CockroachDB SQL lease so the
  cluster's existing quorum decides who is leader.
- Network discovery is only a hint. Trust comes from TLS identity, artifact
  signatures, and the CockroachDB-backed lease.
- In SQL-coordinated mode, CockroachDB SQL is the trust root for rollout state.
  A separate PSK is unnecessary when agents authenticate to SQL and validate
  official artifacts by digest.
- The daemon runs as `cockroach`, not root, with `NoNewPrivileges=true` and
  `ProtectSystem=strict`. Its only writable paths are its own state and log
  directories.
- It never writes to `/usr/local/bin`. That path is a root-owned symlink into
  the agent's own directory, and the agent swaps a second symlink inside it.
- Service control is a polkit rule allowing only start, stop, and restart of
  the one CockroachDB unit, for the `cockroach` user only. There is no sudo
  grant.
- No upgrade starts without an operator running `approve`.
- The SQL identity is a dedicated `rollout` user with `CREATE` on the
  coordination database and the `VIEWCLUSTERMETADATA` and
  `MODIFYCLUSTERSETTING` system privileges. The node certificate is not
  reused: it authenticates as `node`, which is effectively root.

## Audit Events

Useful audit fields include:

- timestamp;
- local node ID when available;
- event name;
- requested version;
- artifact digest;
- authenticated peer identity;
- lease holder;
- systemd action;
- binary path and backup path;
- success or failure status.

Do not log secrets, PSKs, private keys, bearer tokens, or full certificate
private material.
