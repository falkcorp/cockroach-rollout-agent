<!-- file: README.md -->
<!-- version: 3.6.0 -->
<!-- guid: c68a62ce-72d1-45cc-a6c8-d3dfc41d0e34 -->
<!-- last-edited: 2026-05-25 -->

# cockroach-rollout-agent

Public-safe Rust tooling for staged CockroachDB binary rollouts.

This repository intentionally contains no hostnames, IP addresses, cluster
names, shared keys, service accounts, or inventory. Runtime deployment details
must be supplied locally through environment variables, files owned by the
target system, or a secret manager.

## Current Scope

- Discovers production CockroachDB versions from the upstream GitHub tags API.
- Refuses alpha, beta, RC, and other prerelease builds.
- Plans upgrades release-line by release-line using CockroachDB `vYY.R` major
  lines.
- Fetches release notes and blocks by default if warning patterns are found.
- Downloads CockroachDB Linux `amd64` and `arm64` tarballs from the official
  binary endpoint.
- Writes a JSON manifest containing artifact URLs, sizes, SHA-256 digests, and
  release-note scan results.
- Provides a guarded local installer that validates the manifest, stops a
  systemd service, backs up the current binary, replaces it, and starts the
  service again.
- Writes append-only audit events with timestamps and action details.
- Provides a polling daemon mode that can consume a manifest URL or file.
- Provides a CockroachDB SQL coordination mode with leader election, cluster
  discovery, manifest publication, agent heartbeats, and completion tracking.

CockroachDB does not publish official ARMv6 Linux artifacts through the normal
binary endpoint. The implementation targets `arm64`, which is the supported
Linux ARM build.

## Commands

```bash
cargo run -- plan --current-version v25.2.3
cargo run -- prepare --current-version v25.2.3
cargo run -- prepare --current-version v25.2.3 --allow-breaking-warnings
cargo run -- self-check
cargo run -- install --manifest dist/manifest.json --dry-run
cargo run -- finalize --target-version v25.4.3 --dry-run
cargo run -- daemon --manifest-url https://example.invalid/manifest.json --dry-run
cargo run -- --database-url "$COCKROACH_ROLLOUT_DATABASE_URL" init-db
cargo run -- --database-url "$COCKROACH_ROLLOUT_DATABASE_URL" discover
cargo run -- --database-url "$COCKROACH_ROLLOUT_DATABASE_URL" daemon --dry-run
```

## Linux Self Install

After a release publishes Linux assets, install the latest release with:

```bash
curl -fsSL https://jdfalk.github.io/cockroach-rollout-agent/install.sh | sudo bash
```

Install the binary plus systemd template files with:

```bash
curl -fsSL https://jdfalk.github.io/cockroach-rollout-agent/install.sh | sudo bash -s -- --with-systemd
```

The installer downloads `SHA256SUMS` from the matching GitHub release and
refuses to install if checksum validation fails.

Release assets are also covered by GitHub build-provenance attestations through
`actions/attest`. This repository does not use Codecov; pull request coverage is
uploaded to GitHub Code Quality with the first-party
`actions/upload-code-coverage` action after generating Cobertura XML with
`cargo-llvm-cov`.

## Runtime Configuration

| Variable | Default |
| --- | --- |
| `CROACH_ROLLOUT_BASE_URL` | `https://binaries.cockroachdb.com` |
| `CROACH_ROLLOUT_GITHUB_API_URL` | `https://api.github.com/repos/cockroachdb/cockroach/tags?per_page=100` |
| `CROACH_ROLLOUT_RELEASE_NOTES_BASE_URL` | `https://www.cockroachlabs.com/docs/releases` |
| `CROACH_ROLLOUT_ARTIFACTS_DIR` | `dist` |
| `CROACH_ROLLOUT_SERVICE` | `cockroachdb.service` |
| `CROACH_ROLLOUT_BINARY_PATH` | `/usr/local/bin/cockroach` |
| `CROACH_ROLLOUT_AUDIT_LOG` | `/var/log/cockroach-rollout-agent/audit.log` |
| `CROACH_ROLLOUT_DATABASE_URL` | unset |
| `CROACH_ROLLOUT_SCHEMA` | `cockroach_rollout` |
| `CROACH_ROLLOUT_NODE_ID` | hostname plus architecture |
| `CROACH_ROLLOUT_CURRENT_VERSION` | unset |
| `CROACH_ROLLOUT_TARGET_VERSION` | unset |
| `CROACH_ROLLOUT_MANIFEST_URL` | unset |
| `CROACH_ROLLOUT_MANIFEST_FILE` | unset |
| `CROACH_ROLLOUT_PSK` | unset |

When `CROACH_ROLLOUT_PSK` is set, HTTP downloads use it as a bearer token. This
supports a simple PSK-over-TLS deployment for manifest hosting. Do not commit
this value.

For the recommended SQL-coordinated deployment, a PSK is not required. Agents
authenticate to CockroachDB SQL, discover the cluster from CockroachDB internal
metadata, read the active rollout manifest from SQL, and download official
CockroachDB artifacts over HTTPS with SHA-256 validation.

## Rollout Design

The preferred production design is pull-based:

1. Agents discover peers from CockroachDB cluster metadata, not from a checked-in
   inventory file.
2. Agents elect one rollout coordinator by acquiring a short-lived SQL lease in
   CockroachDB. This reuses CockroachDB's existing quorum and avoids
   unauthenticated LAN discovery.
3. The coordinator creates the next-step manifest and publishes it in SQL.
4. Agents read the manifest from SQL and download the official artifact over
   HTTPS.
5. Each agent validates the manifest, confirms the artifact digest, confirms
   the manifest was prepared for the node's current binary version, verifies
   release-note warning approval, stops only its local CockroachDB systemd
   service, atomically replaces the binary, restarts the service, and records
   audit events.
6. After every node has rejoined the cluster on the new binary, patch upgrades
   are complete. Major-line upgrades must be finalized, either automatically by
   CockroachDB or manually with `finalize`.

## Upgrade Sequencing

CockroachDB major versions are `vYY.R` release lines. `plan` emits every
release line between the current version and the requested target. `prepare`
downloads only the next step in that plan. After rolling that manifest to all
nodes and finalizing when required, run `prepare` again from the new current
version to create the next step.

The tool deliberately excludes prerelease versions because CockroachDB warns
that clusters upgraded to alpha binaries or manually built master binaries
cannot later be upgraded to a production release.

## SQL Coordination Deployment

For a complete host deployment walkthrough, see [Deployment](docs/deploy.md).

Use a CockroachDB SQL user with permission to create and update objects in the
configured rollout schema. Initialize once:

```bash
cockroach-rollout-agent \
  --database-url "$COCKROACH_ROLLOUT_DATABASE_URL" \
  init-db
```

Then start the daemon on every CockroachDB host:

```bash
cockroach-rollout-agent \
  --database-url "$COCKROACH_ROLLOUT_DATABASE_URL" \
  daemon
```

Daemon behavior:

- every agent heartbeats its local binary version into SQL;
- one agent becomes leader by holding a TTL lease row;
- the leader discovers live CockroachDB nodes from
  `crdb_internal.gossip_nodes`;
- the leader publishes one active manifest for the next required release line;
- followers download, validate, install, and report completion;
- patch rollouts are marked finalized after every live node reports completion;
- major-line rollouts wait for manual `finalize` unless the daemon is started
  with `--auto-finalize`.

Discovery uses:

```sql
SELECT node_id, address, sql_address, is_live
FROM crdb_internal.gossip_nodes
WHERE is_live;
```

This means the project does not need a separate Raft library or mDNS trust
model. CockroachDB already provides consensus for the SQL lease and rollout
state.

Use a secure CockroachDB SQL URL in `CROACH_ROLLOUT_DATABASE_URL`, for example
with `sslmode=require` or the certificate parameters your cluster already uses.
Do not put the URL in the repository; place it in `/etc/cockroach-rollout-agent.env`
or another host-local secret source.

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

## Remaining Production Hardening

The tool is now runnable, but two production hardening items should be added
before unattended fleet rollout:

- manifest signatures, preferably Sigstore or minisign, so digest metadata has
  an authenticity guarantee beyond TLS;
- a stricter mapping between discovered CockroachDB node IDs and reporting agent
  IDs if multiple agents can run outside the CockroachDB hosts.
