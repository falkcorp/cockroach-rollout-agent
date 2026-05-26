<!-- file: docs/security.md -->
<!-- version: 1.0.0 -->
<!-- guid: b1107208-a9c3-4018-9e86-a44cbf5c7f79 -->
<!-- last-edited: 2026-05-25 -->

# Security Model

The rollout agent must be treated as privileged infrastructure automation.

## Abuse Resistance

- Agents never accept an unsolicited binary as sufficient authority to install.
- Every artifact must match signed metadata and an expected digest before
  installation.
- Rollout coordination should use a short-lived CockroachDB SQL lease so the
  cluster's existing quorum decides who is leader.
- Network discovery is only a hint. Trust comes from TLS identity, artifact
  signatures, and the CockroachDB-backed lease.
- The daemon should run as `cockroach`, not root.
- Any service-management privilege must be restricted to the CockroachDB
  service only.

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
