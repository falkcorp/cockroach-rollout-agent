<!-- file: README.md -->
<!-- version: 4.1.0 -->
<!-- guid: c68a62ce-72d1-45cc-a6c8-d3dfc41d0e34 -->
<!-- last-edited: 2026-10-10 -->

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
- Provides a guarded installer that validates the manifest, stages the new
  binary, swaps it in atomically while the node is still running, restarts the
  service, waits for the node to rejoin on the new build, and rolls back
  automatically if it does not.
- Writes append-only audit events with timestamps and action details.
- Provides a polling daemon mode that can consume a manifest URL or file.
- Provides a CockroachDB SQL coordination mode with leader election, cluster
  discovery, operator-approved rollouts, one-node-at-a-time installs behind a
  cluster health gate, and completion tracking from the cluster's own view of
  each node's build.

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
cargo run -- --database-url "$COCKROACH_ROLLOUT_DATABASE_URL" status
cargo run -- --database-url "$COCKROACH_ROLLOUT_DATABASE_URL" approve v25.4.1
cargo run -- --database-url "$COCKROACH_ROLLOUT_DATABASE_URL" cancel --reason "..."
```

## Linux Self Install

After a release publishes Linux assets, install the latest release with:

```bash
curl -fsSL https://jdfalk.github.io/cockroach-rollout-agent/install.sh | sudo bash
```

Install the binary plus the per-host setup kit (then run the printed
`install-rollout-agent.sh` command) with:

```bash
curl -fsSL https://jdfalk.github.io/cockroach-rollout-agent/install.sh | sudo bash -s -- --with-systemd
```

The installer downloads `SHA256SUMS` from the matching GitHub release and
refuses to install if checksum validation fails.

Release assets are also covered by GitHub build-provenance attestations through
`actions/attest`. This repository does not use Codecov; pull request coverage is
uploaded to GitHub Code Quality with the first-party
`actions/upload-code-coverage` action after generating Cobertura XML with
`cargo-llvm-cov`. Coverage upload errors are treated as warnings until GitHub
Code Quality is enabled for the repository or account during the public preview.

## Runtime Configuration

| Variable | Default |
| --- | --- |
| `CROACH_ROLLOUT_BASE_URL` | `https://binaries.cockroachdb.com` |
| `CROACH_ROLLOUT_GITHUB_API_URL` | `https://api.github.com/repos/cockroachdb/cockroach/tags?per_page=100` |
| `CROACH_ROLLOUT_RELEASE_NOTES_BASE_URL` | `https://www.cockroachlabs.com/docs/releases` |
| `CROACH_ROLLOUT_ARTIFACTS_DIR` | `dist` |
| `CROACH_ROLLOUT_SERVICE` | unset, required for installs (the unit name differs between hosts) |
| `CROACH_ROLLOUT_BINARY_PATH` | `<agent-root>/bin/cockroach` |
| `CROACH_ROLLOUT_AGENT_ROOT` | `/var/lib/cockroach-rollout-agent` |
| `CROACH_ROLLOUT_RESTART_TIMEOUT_SECONDS` | `600` |
| `CROACH_ROLLOUT_AUTO_FINALIZE` | unset (operator runs `finalize`) |
| `CROACH_ROLLOUT_AUDIT_LOG` | `/var/log/cockroach-rollout-agent/audit.log` |
| `CROACH_ROLLOUT_DATABASE_URL` | unset |
| `CROACH_ROLLOUT_SCHEMA` | `cockroach_rollout` |
| `CROACH_ROLLOUT_SSL_ROOT_CERT` | unset |
| `CROACH_ROLLOUT_SSL_CLIENT_CERT` | unset |
| `CROACH_ROLLOUT_SSL_CLIENT_KEY` | unset |
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
3. The coordinator records the next release-line step as a **proposed**
   rollout. Nothing installs until an operator runs `approve <version>`.
4. Agents read the approved manifest from SQL and download the official
   artifact over HTTPS.
5. Agents take a cluster-wide install lease, so only one node upgrades at a
   time, and proceed only when the cluster health gate passes. The agent
   validates the artifact digest, stages the binary, swaps it in atomically
   while the node is still running, restarts only its local CockroachDB unit,
   and waits for the node to rejoin on the new build. If it does not, the
   agent swaps the old binary back, restarts again, and marks the rollout
   failed so no other node proceeds.
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

- every agent heartbeats its liveness and local binary version into SQL;
- one agent becomes leader by holding a TTL lease row;
- the leader proposes the next release-line step; `status` shows it and
  `approve <version>` activates it;
- approving a major-line step pins `cluster.preserve_downgrade_option`, so
  CockroachDB cannot finalize on its own before an operator decides;
- agents upgrade one at a time under an `install` lease, gated on cluster
  health, with automatic rollback;
- a rollout is complete when every non-decommissioned node is live and reports
  the target build in `crdb_internal.gossip_nodes.build_tag`;
- patch rollouts are then marked finalized; major-line rollouts wait for
  `finalize` unless `--auto-finalize` is set;
- a failed install halts the rollout until `cancel`.

Discovery uses:

```sql
SELECT node_id, address, sql_address, is_live
FROM crdb_internal.gossip_nodes
WHERE is_live;
```

This means the project does not need a separate Raft library or mDNS trust
model. CockroachDB already provides consensus for the SQL lease and rollout
state.

### TLS material

The `postgres` connection-string parser only understands `sslmode`; it rejects
libpq's `sslrootcert`, `sslcert`, and `sslkey` with `invalid connection string`.
Supply those paths through `CROACH_ROLLOUT_SSL_ROOT_CERT`,
`CROACH_ROLLOUT_SSL_CLIENT_CERT`, and `CROACH_ROLLOUT_SSL_CLIENT_KEY` instead.
`CROACH_ROLLOUT_SSL_ROOT_CERT` is required against a cluster using a private CA,
which is the normal CockroachDB deployment; without it the handshake fails
because only the system trust store is consulted.

Client certificate authentication needs a **PKCS#8** key. `cockroach cert
create-client` emits PKCS#1, so convert it once:

```bash
openssl pkcs8 -topk8 -nocrypt \
  -in certs/client.rollout.key \
  -out certs/client.rollout.pk8
```

Only `sslmode=disable`, `prefer`, and `require` are accepted; `verify-ca` and
`verify-full` are not recognised by the parser. Chain and hostname verification
still happen through the TLS connector once a root certificate is configured.

Use a secure CockroachDB SQL URL in `CROACH_ROLLOUT_DATABASE_URL`, for example
with `sslmode=require` or the certificate parameters your cluster already uses.
Do not put the URL in the repository; place it in `/etc/cockroach-rollout-agent.env`
or another host-local secret source.

mDNS can be added later as an optional discovery hint, but it should not be the
trust root. A LAN broadcast protocol is too easy to abuse unless every message
is authenticated and authorization is still anchored in the cluster lease.

## Local Permission Model

The daemon runs as the `cockroach` user with `NoNewPrivileges=true` and
`ProtectSystem=strict`. It needs no root and no sudo:

- the CockroachDB unit runs `/var/lib/cockroach-rollout-agent/bin/cockroach`
  through an `ExecStart` drop-in, and the agent swaps that symlink between
  staged versions. `/usr/local/bin/cockroach` stays a separate root-owned copy,
  so root never executes a binary the `cockroach` user can write;
- a polkit rule (`examples/50-cockroach-rollout-agent.rules`) allows only
  start, stop, and restart of the one CockroachDB unit, for `cockroach` only;
- the audit log and state directories are the only writable paths.

`scripts/install-rollout-agent.sh` sets all of this up per host. See
[Deployment](docs/deploy.md).

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
