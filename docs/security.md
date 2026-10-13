<!-- file: docs/security.md -->
<!-- version: 2.5.0 -->
<!-- guid: b1107208-a9c3-4018-9e86-a44cbf5c7f79 -->
<!-- last-edited: 2026-10-13 -->

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
- It never writes to `/usr/local/bin`. The CockroachDB unit runs the
  agent-managed `/var/lib/cockroach-rollout-agent/bin/cockroach` through an
  `ExecStart` drop-in. `/usr/local/bin/cockroach` stays a separate root-owned
  file, so root never executes a binary the `cockroach` user can replace. The
  installer runs every command inside the agent root as `cockroach` and never
  follows links there as root.
- Service control is a polkit rule allowing only start, stop, and restart of
  the one CockroachDB unit, for the `cockroach` user only. There is no sudo
  grant.
- No upgrade starts without an operator running `approve`.
- **Agent self-update** is approval-gated too, and pinned to bytes:
  - The leader proposes only stable releases. It stores each architecture's
    SHA-256 from the release's `SHA256SUMS`, and the operator approves those
    digests.
  - Agents verify the download against the stored digest, not a freshly
    fetched `SHA256SUMS`. A release asset replaced after approval is refused.
  - The trust root is GitHub over TLS at proposal time, plus the operator's
    review. The release's build-provenance attestations can be checked by
    hand with `gh attestation verify` before approving.
  - The new binary runs as `cockroach`, the user that could already replace
    it, so self-update grants no new privilege.
  - The rollback step (`agent-preflight`) always runs the root-owned
    `/usr/local/bin` copy, so a broken update cannot disable its own
    rollback.
  - No polkit grant covers the agent's own unit. It restarts by exiting with
    status 75.
- The SQL identity is a dedicated `rollout` user with `CREATE` on the
  coordination database and the `VIEWCLUSTERMETADATA` and
  `MODIFYCLUSTERSETTING` system privileges. The node certificate is not
  reused: it authenticates as `node`, which is effectively root.

## Web UI

- **Scope:** one shared operator password, intended for a trusted LAN.
  There are no accounts or roles.
- **Password storage:** the password is stored only as an argon2id hash
  in `<schema>.ui_auth`.
- **Sessions:** the session cookie `crra_session` is `Secure`, `HttpOnly`
  and `SameSite=Strict`, and lasts `CROACH_ROLLOUT_UI_SESSION_HOURS`. The
  database stores only the cookie's SHA-256.
- **CSRF:** every state-changing form carries a per-session CSRF token,
  compared in constant time. A mismatch returns 403 and records a
  `csrf_rejected` event.
- **TLS:** TLS 1.2 or 1.3. The UI never serves plain HTTP. It uses the
  approved UI certificate, or the node's own CockroachDB certificate until
  one is approved.
- **UI CA:**
  - **Name constraints.** Critical X.509 name constraints limit it to the
    cluster's hostnames and node IPs, so a leaked CA key cannot impersonate
    any other site to a browser that trusts it. A unit test checks that
    webpki refuses a leaf outside the constraints.
  - **CA key.** It lives on one node, `ui-ca/ca.key` mode 0600, owned by
    the agent user. It is never stored in the database.
- **UI leaf key:**
  - **Where it is stored.** In `<schema>.ui_certs`, so every agent can
    install it. Anyone who can read that schema, which today means the
    `rollout` client certificate, can impersonate the UI on the LAN. That
    certificate can already approve upgrades, so this adds no new
    authority.
  - **Approval.** Approval is bound to the certificate's SHA-256.
  - **Downloads.** A key download needs a session, is sent `no-store` as
    an attachment, and is recorded as `ui_cert_key_downloaded` before it is
    served.
- **Settings diff:** only values of plain types (bool, int, float,
  duration, enum, byte size) are shown. `cluster.secret`,
  `enterprise.license` and `cloudstorage.http.custom_ca` are always
  redacted. Setting values never go into events or audit logs.
- **Security headers:** `Content-Security-Policy` (`default-src 'self'`,
  no framing), `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`,
  `X-Content-Type-Options: nosniff` and `Cache-Control: no-store`.
- **Lockout:** after 5 failed logins from one IP within 60 seconds, that
  node answers 429 for the rest of the minute.
  - The counter is in memory on each node, so it is a speed bump, not a
    cluster-wide lockout.
  - Use a long password.
- **Event log:** logins, CSRF rejections, password changes and every
  action are recorded in the cluster-wide `<schema>.events` table, as well
  as each host's audit log.

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
