<!-- file: README.md -->
<!-- version: 1.0.0 -->
<!-- guid: c68a62ce-72d1-45cc-a6c8-d3dfc41d0e34 -->
<!-- last-edited: 2026-05-25 -->

# cockroach-rollout-agent

Public-safe Rust tooling for staged CockroachDB binary rollouts.

This repository intentionally contains no hostnames, IP addresses, cluster
names, shared keys, service accounts, or inventory. Runtime deployment details
must be supplied locally through environment variables, files owned by the
target system, or a secret manager.

## Current Scope

- Downloads CockroachDB Linux `amd64` and `arm64` tarballs from the official
  binary endpoint.
- Provides a guarded local installer that stops a systemd service, backs up the
  current binary, replaces it, and starts the service again.
- Writes append-only audit events with timestamps and action details.
- Documents the intended quorum design without committing private topology.

CockroachDB does not publish official ARMv6 Linux artifacts through the normal
binary endpoint. The implementation targets `arm64`, which is the supported
Linux ARM build.

## Commands

```bash
cargo run -- fetch
cargo run -- self-check
cargo run -- install dist/cockroach-latest.linux-amd64.tgz
cargo run -- daemon
```

## Runtime Configuration

| Variable | Default |
| --- | --- |
| `CROACH_ROLLOUT_BASE_URL` | `https://binaries.cockroachdb.com` |
| `CROACH_ROLLOUT_VERSION` | `latest` |
| `CROACH_ROLLOUT_ARTIFACTS_DIR` | `dist` |
| `CROACH_ROLLOUT_SERVICE` | `cockroachdb.service` |
| `CROACH_ROLLOUT_BINARY_PATH` | `/usr/local/bin/cockroach` |
| `CROACH_ROLLOUT_AUDIT_LOG` | `/var/log/cockroach-rollout-agent/audit.log` |

## Rollout Design

The preferred production design is pull-based:

1. A publisher downloads artifacts and publishes signed metadata.
2. Agents discover peers from CockroachDB cluster metadata, not from a checked-in
   inventory file.
3. Agents elect one rollout coordinator by acquiring a short-lived SQL lease in
   CockroachDB. This reuses CockroachDB's existing quorum and avoids
   unauthenticated LAN discovery.
4. The coordinator announces the target version. Agents pull the artifact over
   mutually authenticated TLS or an equivalent authenticated channel.
5. Each agent validates the signed metadata, confirms the artifact digest,
   stops only its local CockroachDB systemd service, atomically replaces the
   binary, restarts the service, and records audit events.

mDNS can be added later as an optional discovery hint, but it should not be the
trust root. A LAN broadcast protocol is too easy to abuse unless every message
is authenticated and authorization is still anchored in the cluster lease.

## Local Permission Model

Run the daemon as the `cockroach` user. Grant the minimum extra privileges:

- permission to restart only the CockroachDB service;
- write permission to the CockroachDB binary path or a controlled install
  directory;
- write permission to the audit log directory.

Prefer a sudoers or polkit rule that permits only:

```text
/bin/systemctl stop cockroachdb.service
/bin/systemctl start cockroachdb.service
/bin/systemctl restart cockroachdb.service
```

If the binary path is root-owned, use a narrow ACL or install directory
ownership policy instead of giving the daemon broad root access.

## Public Repository Safety

Before publishing, verify:

- no real hostnames, addresses, usernames beyond public noreply metadata, or
  cluster names are committed;
- no PSKs, tokens, certificates, private keys, or generated config files are
  committed;
- examples use placeholders only;
- logs, artifacts, backups, and local state are ignored by Git.
