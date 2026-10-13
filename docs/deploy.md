<!-- file: docs/deploy.md -->
<!-- version: 2.4.0 -->
<!-- guid: 41eb3d6e-f70e-431d-8f3e-33d1ca5e45c1 -->
<!-- last-edited: 2026-10-12 -->

# Deployment

Systemd is the primary deployment model because the agent replaces a host
binary and restarts a host service.

## 1. Build

```bash
cargo build --release --locked
sudo install -o root -g root -m 0755 \
  target/release/cockroach-rollout-agent \
  /usr/local/bin/cockroach-rollout-agent
```

## 2. Permission Model

The agent runs as `cockroach` and never needs root at runtime:

```text
/etc/systemd/system/<unit>.d/50-cockroach-rollout-agent.conf   ExecStart= -> /var/lib/cockroach-rollout-agent/bin/cockroach
/var/lib/cockroach-rollout-agent/bin/cockroach  -> ../versions/cockroach-v25.3.0   (agent swaps this)
/var/lib/cockroach-rollout-agent/versions/cockroach-v25.3.0                         (real binary)
/usr/local/bin/cockroach                                                            (root-owned copy, CLI only)
```

- **Root never runs agent-writable code:** the `cockroach` user can replace
  anything under `/var/lib/cockroach-rollout-agent`, so only the CockroachDB
  unit, which already runs as `cockroach`, executes from there. Operators keep
  using `/usr/local/bin/cockroach`, a root-owned file the agent never writes.
  After a rollout it lags the server; install the matching official binary
  there when convenient. Do not run anything under the agent root as root.

- **Binary swap:** the agent stages the new binary under `versions/` and
  atomically renames `bin/cockroach` to point at it while CockroachDB is still
  running. Only the restart causes downtime. If the node does not rejoin on the
  new build within `CROACH_ROLLOUT_RESTART_TIMEOUT_SECONDS`, the link is
  pointed back at the previous version and the unit is restarted again.
- **Service control:** `examples/50-cockroach-rollout-agent.rules` is a polkit
  rule allowing only `start`, `stop` and `restart` of the one CockroachDB unit,
  and only for the `cockroach` user. sudo does not work here, because the agent
  unit sets `NoNewPrivileges=true`.
- **Unit name:** `CROACH_ROLLOUT_SERVICE` has no default, because the name
  differs between hosts (`cockroach.service` on some, `cockroachdb.service` on
  others). The env file, the agent unit's `After=`, and the polkit rule must
  all name the same unit. The installer fills in all three from one value.

## 3. SQL Identity

Create a dedicated user. Do **not** reuse the node certificate: it
authenticates as `node`, which is effectively root.

```sql
CREATE USER rollout;
GRANT CREATE ON DATABASE defaultdb TO rollout;
GRANT SYSTEM VIEWCLUSTERMETADATA TO rollout;   -- gossip_nodes, kv_store_status
GRANT SYSTEM MODIFYCLUSTERSETTING TO rollout;  -- preserve_downgrade_option, finalize
```

Mint its certificate on the machine that holds the CA key:

```bash
cockroach cert create-client rollout --certs-dir=certs --ca-key=my-safe-directory/ca.key
```

The installer converts the key to PKCS#8, which is the only format the TLS
layer accepts. The `postgres` URL parser rejects `sslrootcert`, `sslcert` and
`sslkey`, so the paths travel as `CROACH_ROLLOUT_SSL_*` variables instead.

Point each agent at its **own** node's SQL address, so reporting status never
depends on a peer being up and "is my node back?" asks the right node.

## 4. Install on Each Host

Copy the agent binary, `scripts/`, `examples/`, and a directory holding
`ca.crt`, `client.rollout.crt` and `client.rollout.key` to the host. Then run
as root:

```bash
sudo scripts/install-rollout-agent.sh \
  --agent-binary ./cockroach-rollout-agent \
  --sql-addr <this-node-ip>:<sql-port> \
  --certs-src ./certs
```

The script:

- detects the CockroachDB unit, or takes `--service`;
- stages the running `/usr/local/bin/cockroach` version under `versions/`,
  doing every write inside the agent root as `cockroach`, never as root;
- adds the `ExecStart` drop-in and checks with `systemctl show` that only the
  executable changed. It takes effect at the unit's next restart, so
  CockroachDB is not restarted;
- writes the env file, the agent unit and the polkit rule;
- runs `self-check` inside the unit's own sandbox;
- leaves the agent **disabled** unless `--enable` is passed.

It is idempotent. `--uninstall` removes the drop-in, the unit and the polkit
rule. It refuses while `/usr/local/bin/cockroach` is a different version than
the agent-managed binary, because the next restart would silently switch
versions; install the matching official binary there first.

`self-check` verifies all of the following without stopping anything:

- that the CockroachDB unit's `ExecStart` runs the agent link, which resolves;
- that the agent's directories are writable through the sandbox;
- polkit authorization for the CockroachDB unit, probed with
  `systemctl reset-failed`, which changes nothing on a healthy unit;
- SQL access.

Treat a failed self-check as a blocker.

## 5. Run a Rollout

1. **Enable the agent on every node.** A rollout cannot complete while any live
   node lacks an agent.

   ```bash
   sudo systemctl enable --now cockroach-rollout-agent.service
   ```

2. **Review the proposal.** Within one tick, the leader records the next
   release-line step as `proposed`. The proposal carries two kinds of
   warning:
   - release-note matches;
   - a **settings diff**: every cluster setting you override that the target
     removes, retypes, or re-defaults, found by running
     `gen settings-list` on both binaries.
   Other default changes are listed for context.

   ```bash
   sudo -u cockroach env $(sudo cat /etc/cockroach-rollout-agent.env | xargs) \
     cockroach-rollout-agent status
   ```

3. **Approve it.** Agents then upgrade one node at a time. Each waits for the
   install lease and for the health gate.

   ```bash
   cockroach-rollout-agent approve v25.4.1
   ```

   If the release notes or the settings diff produced warnings, review them
   first, then add `--accept-release-note-warnings`.

   **Not now?** `decline` refuses the proposal, and the leader never
   proposes that version again. A newer patch is still proposed.
   `undecline <version>` reverses it.

4. **Watch it.** Use `status`, `journalctl -u cockroach-rollout-agent -f`, and
   `/var/log/cockroach-rollout-agent/audit.log`.

5. **Finalize.** For a major-line step, once `status` shows every node on the
   target, run `finalize --target-version v25.4.1`. Until you do,
   `cluster.preserve_downgrade_option` keeps the cluster able to roll back.
   After it, it cannot.

- **Failed install:** a failed install rolls its own node back and marks the
  rollout `failed`, which halts every other node. Investigate, then run
  `cancel`.
- **Cancel:** `cancel` works on a proposed or active rollout too. Nodes that
  already upgraded stay upgraded. A cancelled proposal comes back on the
  next tick; use `decline` to stop it.
- **Release list:** the leader fetches CockroachDB's release tags from GitHub
  at most hourly and caches them in `<schema>.release_catalog`. Proposals
  and the dashboard's Releases panel read the cache. **Check now** on the
  dashboard asks for a fetch on the next tick, at most once every 10
  minutes.

## 6. Web UI

Every agent serves the same dashboard at `https://<node-ip>:38090/`. It
uses the approved UI certificate (see below) once one exists. Before that,
it uses the node's own `node.crt` and `node.key`.

- **What it does:** shows rollouts, nodes, agents, locks, health and recent
  events. You can approve, cancel or finalize an upgrade, and start a
  rolling restart.
- **Safety:** every action runs the same checks as the CLI.

1. **Set the password once, on any node.** Run it as the agent user with
   the env file loaded:

   ```bash
   sudo -u cockroach bash -c 'set -a; . /etc/cockroach-rollout-agent.env; cockroach-rollout-agent set-ui-password'
   ```

   - The password must be at least 12 characters.
   - Running the command again changes the password and signs out every
     session.

2. **Trust a CA in your browser.** Use the Rollout UI CA from the next
   section. Until it exists, use the cluster's `ca.crt`.
   - **macOS:** Safari and Chrome refuse CockroachDB's default 5-year node
     certificates even with the cluster CA trusted, because Apple caps TLS
     certificates at 825 days. Use the UI CA.

3. **Check it.** `cockroach-rollout-agent self-check` reports `ui tls` with
   the address and certificate it will use.

- **Rolling restart:** restarts every node in turn on the version it
  already runs. It uses the same install lease and health gate as an
  upgrade.
  - **When it is offered:** only when nothing else is open, every node runs
    one version, and every node's agent is 0.1.0 or newer.
  - **If a node does not come back:** if a node fails to rejoin within
    `CROACH_ROLLOUT_RESTART_TIMEOUT_SECONDS`, the restart becomes `failed`
    and halts. Investigate, then cancel it.
### UI certificates

The UI has its own private CA, kept apart from the cluster CA.

- **Name-constrained.** It may only sign for this cluster's hostnames and
  node IPs. Trusting it in a browser vouches for nothing else.
- **What it signs.** 397-day leaf certificates, which every browser
  accepts.
- **Approval.** A person approves every certificate before any agent
  serves it.

1. **Create the CA on one node.** This uses every agent's hostname and
   every node's IP. Add `--extra-name` for anything else.

   ```bash
   sudo -u cockroach bash -c 'set -a; . /etc/cockroach-rollout-agent.env; cockroach-rollout-agent ui-cert init-ca'
   ```

   The key stays in `/var/lib/cockroach-rollout-agent/ui-ca/` on that node,
   mode 0600. It is never stored in the database. Adding a host later needs
   a new CA, which you then trust again.

2. **Stage the first certificate.** After this, the daily
   `cockroach-rollout-agent-ui-cert.timer` stages renewals 30 days before
   expiry, and only on the node with the CA key.

   ```bash
   sudo -u cockroach bash -c 'set -a; . /etc/cockroach-rollout-agent.env; cockroach-rollout-agent ui-cert renew'
   ```

3. **Review and approve it on the dashboard.**
   - The **UI certificate** panel shows the staged certificate's names,
     validity and SHA-256.
   - It links the CA certificate, the leaf, and the private key. Every key
     download is logged.
   - Approve or reject it. The CLI equivalents are
     `ui-cert approve <sha256>` and `ui-cert reject <sha256>`.

4. **Distribution.** Within one tick every agent installs the approved
   certificate:
   - it checks the certificate against the CA and against its own hostname
     and IP;
   - it writes it under `ui-certs/`, keeping the previous pair;
   - it hot-reloads, with no restart.

   An agent that cannot use it keeps its old certificate and shows why in
   the panel.

5. **Trust the UI CA.** Download it from the panel. On macOS:
   `sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain rollout-ui-ca.crt`.

You can also paste a certificate and key into the panel's **Stage a
certificate by hand**. It must chain to the UI CA, and it still needs
approval.

- **Disabling the UI:** set `CROACH_ROLLOUT_UI_LISTEN=off`. If the
  certificate cannot be loaded, the agent logs `ui disabled: <reason>` and
  keeps running. It never serves plain HTTP.

## Docker

The Docker image is useful for `plan`, `prepare`, `discover`, and other
controller-style operations. It is not the recommended install mechanism because
install mode needs host filesystem writes and host systemd access.

Build:

```bash
docker build -t cockroach-rollout-agent:local .
```

Run a plan:

```bash
docker run --rm cockroach-rollout-agent:local \
  plan --current-version v25.2.9 --target-version v25.4.0
```

If you choose to run install mode in a container, you must deliberately provide
host mounts and service-control access. That is more dangerous than the systemd
deployment and should be avoided unless you have a strong operational reason.
