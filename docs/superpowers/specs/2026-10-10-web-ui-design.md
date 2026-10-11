<!-- file: docs/superpowers/specs/2026-10-10-web-ui-design.md -->
<!-- version: 1.1.0 -->
<!-- guid: 8a1fc377-9402-46d9-aac1-dd7f6724861f -->
<!-- last-edited: 2026-10-11 -->

# Built-in web UI (v1.0.0)

## Intent

Stated by the operator:

- Every agent serves a built-in web UI.
- It shows rollout status and lets the operator approve, restart, and so on.
- Login is a simple password form, with the password stored in the database.
- The UI ships in v1.0.0.

Agreed during design:

- One operator, one shared password. No accounts, roles, SSO, or API tokens.
- "Restart" means a **rolling restart of the whole cluster**: every node in
  turn, under the same install lease and health gate as an upgrade, with no
  version change.
- The UI is served over **HTTPS with each node's existing CockroachDB node
  certificate**. Plain HTTP is never offered.
- It is built with **server-rendered HTML forms and no JS build step**.
  The HTTP server is **axum on a dedicated tokio runtime** inside the web
  thread. The rest of the agent stays synchronous. (Amended 2026-10-11: the
  original choice, `tiny_http`, pins rustls 0.20 for TLS, which is
  unmaintained and fails the audit gate with RUSTSEC-2024-0336.)

**Success:** the operator can take a rollout from proposal to finalize in a
browser, and can run a rolling restart. They can do either from any node,
without SSH. Every safety check the CLI enforces applies equally to the UI.

## Facts this design relies on

These were checked on the live cluster on 2026-10-10:

- **Nodes:** five nodes, all amd64. They are len-serv-001/002/003
  (172.16.3.92/.94/.96), unimatrixone (172.16.2.35) and unimatrixzero
  (172.16.2.30). Each runs `cockroach.service` as `User=cockroach`.
- **Certificates:** each node's `node.crt` carries its own IP in the SAN and
  is valid until 2030 or later. `node.key` is `0600 cockroach:cockroach`, so
  the agent, which runs as `cockroach`, can read both.
- **Certs directory:** the location differs per host. It is
  `/var/lib/cockroach/certs` on four hosts and
  `/mnt/bigdata/cockroach/certs` on unimatrixzero. The unit's
  `--certs-dir` may also be relative to `WorkingDirectory`.
- **Code style:** the agent is synchronous throughout, using blocking
  `postgres` and blocking `reqwest`. `status_command` queries and prints in
  one function.
- **Dependency tree:** tokio, hyper, tower, http, httparse and rustls 0.23
  (with the `ring` provider) are already compiled in through reqwest.
- **Agent identity:** `agent_status` is keyed by `hostname:arch` and records
  no CockroachDB node id, so agents cannot yet be matched to nodes.
- **Audit log:** the audit log is a host-local file, so one node cannot see
  another node's audit lines.

## Architecture

The UI runs as a thread inside the existing `daemon` process. There is no new
systemd unit. Every agent serves the UI.

```text
daemon
├── existing tick loop (leader / follower)          unchanged control flow
└── web thread (if CROACH_ROLLOUT_UI_LISTEN != off)
      own tokio runtime: axum-server + rustls 0.23 (ring)  ──►  web::router
      every DB / ops:: call runs in spawn_blocking
                                 ├── auth  (ui_auth, ui_sessions)
                                 ├── cluster_view::load()   read-only
                                 └── ops::{approve,cancel,finalize,restart_cluster}
```

### New modules

| Module | Purpose | Depends on |
|---|---|---|
| `cluster_view.rs` | Loads one `ClusterView` value: rollouts, nodes, agents, leases, health snapshot and the gate verdict, plus recent events. `status` prints from it. | DB client, `health`, `rollout` |
| `ops.rs` | `approve`, `cancel`, `finalize` and `restart_cluster` as functions that take an `Actor` (`Cli` or `Web { ip }`). These hold all the precondition checks. The CLI subcommands become thin wrappers. | DB client, `rollout`, events |
| `events.rs` | `record_event(client, source, kind, detail)` inserts into the cluster-wide `events` table. Called next to the existing `audit()` calls wherever a DB client is in hand. | DB client |
| `web/mod.rs` | Server startup: builds the tokio runtime, binds the listener, serves the router with TLS, isolates panics. Defines the `Backend` trait (auth store, view, ops) that the router calls. | axum, axum-server, tokio |
| `web/auth.rs` | Password hashing and verification (argon2id), sessions, CSRF tokens, the per-IP failure counter. | argon2, rand, sha2, subtle |
| `web/routes.rs` | Request handlers for `/login`, `/`, `/logout` and the action POSTs. | auth, cluster_view, ops |
| `web/render.rs` | HTML templates as Rust functions with mandatory escaping. Also the small CSS and inline-free JS assets. | — |
| `web/tls.rs` | Resolves the certs dir from the unit and loads `node.crt` and `node.key` into a rustls `ServerConfig`. | systemctl, rustls |

`main.rs` loses `status_command`'s query code and the bodies of
approve, cancel and finalize to these modules.

### New dependencies

- `axum` 0.8, with default features off and `http1`, `tokio`, `form` and
  `query` on;
- `axum-server` 0.8, with only `tls-rustls-no-provider`, so aws-lc-rs is
  never compiled in;
- `rustls` 0.23 with `ring`, already in the tree; PEM parsing uses its
  `pki_types::pem::PemObject`;
- `tokio` 1, with `rt-multi-thread`, `net`, `time` and `macros`;
- `tower-http` 0.6, for `CatchPanicLayer`;
- `argon2` 0.6, `rand` 0.10 and `subtle` 2.6;
- `rpassword` 7, for the no-echo prompt in `set-ui-password`;
- dev-only: `rcgen` 0.14, to create test certificates.

All of these fit the existing `rust-version = 1.87`.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `CROACH_ROLLOUT_UI_LISTEN` | `0.0.0.0:38090` | Listen address, or `off` to disable the UI. |
| `CROACH_ROLLOUT_UI_CERTS_DIR` | unset | Overrides the certs dir. When unset, the dir is resolved from the CockroachDB unit (see TLS below). |
| `CROACH_ROLLOUT_UI_SESSION_HOURS` | `12` | Session lifetime. |

## Data model

All new tables live in the existing schema, which defaults to
`cockroach_rollout`. `ensure_schema` creates them with `IF NOT EXISTS`.

```sql
CREATE TABLE ui_auth (
    id INT PRIMARY KEY DEFAULT 1 CHECK (id = 1),   -- single row
    password_hash STRING NOT NULL,                 -- argon2id PHC string
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE ui_sessions (
    token_sha256 STRING PRIMARY KEY,               -- hex SHA-256 of the cookie token
    csrf_token STRING NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    client_ip STRING NOT NULL
);

CREATE TABLE events (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    at TIMESTAMPTZ NOT NULL DEFAULT now(),
    agent_id STRING NOT NULL,
    source STRING NOT NULL,                        -- agent | cli | web
    kind STRING NOT NULL,                          -- e.g. proposal_skipped, gate_wait, upgraded, rolled_back, approved
    detail STRING NOT NULL
);

ALTER TABLE rollouts ADD COLUMN IF NOT EXISTS kind STRING NOT NULL DEFAULT 'upgrade';  -- upgrade | restart
ALTER TABLE agent_status ADD COLUMN IF NOT EXISTS restarted_rollout_id STRING NULL;
ALTER TABLE agent_status ADD COLUMN IF NOT EXISTS node_id INT8 NULL;         -- crdb_internal.node_id(), written on heartbeat
ALTER TABLE agent_status ADD COLUMN IF NOT EXISTS agent_version STRING NULL; -- CARGO_PKG_VERSION, written on heartbeat
```

- **Restart rows:** a restart stores a `RolloutManifest` like any rollout,
  with `current_version = target_version`, no artifacts and no warnings.
  This keeps `manifest_json` NOT NULL and `open_rollout` unchanged in shape.

- **Expired sessions:** the leader deletes expired `ui_sessions` rows on
  each tick.
- **Events retention:** each tick, the leader deletes all but the newest 500
  `events` rows.
- **Local audit log:** the host-local audit log stays as it is. The
  `events` table is in addition to it, not a replacement.

## TLS

1. If `CROACH_ROLLOUT_UI_CERTS_DIR` is set, use it.
2. Otherwise, read `systemctl show -p ExecStart -p WorkingDirectory
   <service>` and take `--certs-dir=` from argv. If the path is relative,
   resolve it against `WorkingDirectory`, or against `/` if that is unset.
3. Load `node.crt` (the full chain as written) and `node.key` (PKCS#1 or
   PKCS#8) into a rustls `ServerConfig`. Allow TLS 1.2 and 1.3 only.
4. **On failure:** log `ui disabled: <reason>`, do not start the web thread,
   and keep the daemon running. **Never fall back to HTTP.**

`self-check` gains a `ui tls` line that runs the same resolution and load.
It fails if the load fails while the UI is enabled.

The cert files are read once at startup. A renewed node cert takes effect
after the agent restarts, which matches CockroachDB's own behavior without
SIGHUP.

## Authentication

- **Setting the password.** `cockroach-rollout-agent set-ui-password` reads
  the password from stdin, without echo when stdin is a TTY.
  - It rejects passwords shorter than 12 characters.
  - It stores an argon2id hash with the default parameters and a random salt.
  - It deletes every `ui_sessions` row.
  - It records an event.
- **No password set.** Every page renders "UI password not set; run
  `cockroach-rollout-agent set-ui-password` on any node". No login form is
  shown.
- **Login.** `POST /login` with `password`:
  - If the client IP has 5 consecutive failures in the last 60 seconds,
    respond 429 without checking the password. This counter is in memory
    and per node.
  - Verify the password against `ui_auth.password_hash`.
  - On success, generate a 32-byte random token (hex) and a separate
    32-byte CSRF token. Insert a `ui_sessions` row keyed by
    SHA-256(token), with `expires_at = now() + SESSION_HOURS`.
  - Set the cookie `crra_session=<token>; Secure; HttpOnly;
    SameSite=Strict; Path=/`, then redirect 303 to `/`.
  - Record a `login` event that contains the IP, never the password.
- **Every request.** Look up the session by SHA-256(cookie). If it is
  missing or expired, redirect 303 to `/login`.
- **CSRF.** Every POST other than `/login` must carry `csrf=<token>`
  matching the session row, compared in constant time. Otherwise respond
  403 and record a `csrf_rejected` event.
- **Logout.** `POST /logout` with CSRF deletes the session row and clears
  the cookie.
- **Security headers on every response:**
  - `Content-Security-Policy: default-src 'self'; form-action 'self';
    frame-ancestors 'none'`
  - `X-Frame-Options: DENY`
  - `Referrer-Policy: no-referrer`
  - `X-Content-Type-Options: nosniff`
  - on authenticated pages, also `Cache-Control: no-store`.

## Pages

- `GET /login` shows a single password field.
- `GET /` shows the dashboard with these sections:
  - **Rollouts:** newest first, with kind, status, from → to, who created
    and approved it and when, and any error.
  - **Nodes:** id, live, version.
  - **Agents:** agent id, state, version, last seen (marked stale per
    `stale_after`), error.
  - **Locks:** leader and install holders, with expiry.
  - **Health:** the snapshot and the gate verdict, including the reason
    text when the gate blocks.
  - **Recent events:** the latest 50 rows from `events`.
  - **Actions:** see below.
- `GET /static/app.css` and `GET /static/app.js` are compiled into the
  binary with `include_str!`. `app.js` only re-fetches `/` every 10 seconds
  and swaps `<main>`, and pauses while a form field has focus. The page
  works fully without JS.

**Rendering.** All dynamic values go through one `escape()` function.
Templates take typed values, never pre-built HTML strings.

## Actions

Each action calls the same `ops::` function as its CLI subcommand. The
dashboard shows only the actions valid for the current `ClusterView`. The
server re-checks validity on POST, because the view may be stale.

| State (from `ClusterView`) | Action | Form fields | CLI equivalent |
|---|---|---|---|
| Newest open rollout is `proposed` | Approve | `version` (hidden, the proposal's target); `accept_warnings` checkbox, required if the manifest has warnings | `approve <version> [--accept-release-note-warnings]` |
| Open rollout `active` or `failed` (either kind) | Cancel | `reason` (required) | `cancel --reason` |
| Upgrade `active`, needs finalization, and every node is live on the target (the leader's `AwaitOperatorFinalize`) | Finalize | `confirm_version` must equal the target | `finalize --target-version` |
| No open rollout, and `live_version_span` gives one version | Rolling restart | `confirm` must equal `restart` | `restart-cluster` (new) |

After a POST, the server redirects 303 to `/` and shows a one-time flash
message carrying the `ops::` result or error text, unchanged. Errors are
never hidden behind generic messages.

## Rolling restart

A restart is a rollout with `kind = 'restart'` and `current_version =
target_version =` the uniform cluster version.

- **Creation.** `ops::restart_cluster` creates it with `status = 'active'`
  and `approved_by` set to the actor, because starting it is the operator's
  approval. It refuses when any rollout is open, or when the cluster is not
  live on one version.
- **Branching on kind.** `open_rollout` returns `kind`. Both the leader's
  and the follower's reconcile branch on it **before** any upgrade logic.
  Because `from == to`, the upgrade path would otherwise call the restart
  already complete on the first tick: the leader would see every node on
  the target, and each follower would see itself on the target. A restart
  never reaches artifact download, the binary swap or the finalize branches.
- **Version guard.** `ops::restart_cluster` refuses unless every live node
  has an agent row whose `agent_version` is at least the first
  restart-capable release, 0.1.0. The error names the agents that are
  missing or too old. Older agents ignore `kind`, and would end the restart
  without restarting anything.
- **Follower.** This is a new pure function, `restart_action(rollout_id,
  restarted_rollout_id) -> {AlreadyRestarted, Restart}`.
  - On `Restart`, the follower takes the install lease, passes
    `safe_to_restart`, records the node's `started_at` from
    `crdb_internal.gossip_nodes`, runs `systemctl restart <service>`, and
    waits for the node to be live on the same version **with a newer
    `started_at`**. Without that last check, gossip from before the restart
    could pass.
  - It then sets `agent_status.restarted_rollout_id = id`, records a
    `restarted` event, and releases the lease.
  - If the node does not rejoin before `restart_timeout`, the follower
    sets the rollout to `failed`, writes the error to `rollouts.error` and
    `agent_status.error`, and releases the lease. This is the same terminal
    path a failed upgrade takes. There is no binary to roll back.
- **Leader.** This is a new pure function, `restart_complete(agents, nodes,
  rollout_id) -> bool`. It returns true only when every active node is live
  and has an agent, matched by `agent_status.node_id`, whose
  `restarted_rollout_id` equals the rollout id.
  When it is true, the leader sets `status = 'finalized'`, the existing
  terminal success state. A restart never touches
  `preserve_downgrade_option`.
- **Errors.** A `failed` restart halts, like a failed upgrade. No follower
  acts on it, and the operator cancels it to clear it.
- **Finalize scope.** `ops::finalize` acts only on an open rollout of kind
  `upgrade`.
- **Exclusivity.** At most one open rollout of either kind. This is
  enforced in `ops::` and by the leader's proposal logic, which already
  skips proposing while a rollout is open.

## Error handling

| Failure | Behavior |
|---|---|
| The web thread fails to bind or panics | Log it. The daemon loop continues, because rollouts never depend on the UI. A panicking handler returns 500, through `CatchPanicLayer` or the `spawn_blocking` `JoinError`. |
| DB unreachable while rendering | The dashboard shows a "database unreachable: <error>" banner and no stale data. Every POST is refused. |
| An `ops::` precondition fails | The flash shows the exact error, the same text as the CLI. |
| Session expired mid-form | Redirect to `/login`. The form is not replayed. |
| Certs missing or unreadable | The UI is disabled with a log line. `self-check` reports `FAIL ui tls`. |

## Testing

**Pure unit tests:**

- `available_actions(&ClusterView)` for every row of the actions table,
  plus the stale-view re-check;
- `restart_action`, and `restart_complete`, including an agentless live
  node;
- certs-dir resolution: absolute, relative with `WorkingDirectory`,
  relative without it, and the override;
- the session-expiry check, and constant-time CSRF comparison;
- `escape()` on HTML metacharacters, plus a render test that a node error
  containing `<script>` comes out escaped;
- the per-IP failure counter: lockout after 5, and reset after 60 seconds.

**An HTTPS integration test** lives in-crate under `web/`, because the
crate is bin-only. It starts the web server on `127.0.0.1:0` with an
`rcgen` certificate and an in-memory `Backend`, then asserts:

- `GET /` without a session redirects to `/login`;
- a wrong password returns 401, and the 6th attempt returns 429;
- a correct password sets the cookie and redirects 303 to `/`;
- a POST without CSRF returns 403;
- the security headers are present.

**The DB layer** stays thin and is exercised live during rollout
verification, as the existing SQL is today.

## Installer and docs

- **Installer:** `install-rollout-agent.sh` writes
  `CROACH_ROLLOUT_UI_LISTEN=0.0.0.0:38090` into the env file. The agent
  unit needs no new writable paths, because the cert reads are covered by
  `ProtectSystem=strict`.
- **Docs:** `docs/deploy.md` gains a "Web UI" section covering
  `set-ui-password`, importing `ca.crt` into the browser, and the port.
  `docs/security.md` documents the threat model: one password, LAN only,
  TLS with node certs, CSRF and SameSite, and the per-node lockout as a
  speed bump.
- **README:** the configuration table gains the three UI variables.

## Ship plan

1. Implement on a branch, then PR and CI, then release **v0.1.0**.
2. Rerun the installer on all five nodes. It is idempotent, and
   unimatrixzero runs it locally.
3. Run `set-ui-password` once, and import the cluster `ca.crt` into the
   browser.
4. **Live verification:**
   - start a rolling restart from the UI on any node. This also applies the
     2026-10-10 `--join` changes;
   - then finalize v25.4 from the UI.
5. When both succeed end to end, cut **v1.0.0**.

## Out of scope for v1

- user accounts, roles, SSO, and API tokens;
- a JSON API for scripts. The CLI already covers scripting;
- restarting a single chosen node;
- retrying a node that failed or was rolled back;
- live push updates. The page polls every 10 seconds;
- serving on hosts without a node certificate.
