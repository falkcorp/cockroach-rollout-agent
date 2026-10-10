<!-- file: docs/deploy.md -->
<!-- version: 2.2.0 -->
<!-- guid: 41eb3d6e-f70e-431d-8f3e-33d1ca5e45c1 -->
<!-- last-edited: 2026-10-10 -->

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
   release-line step as `proposed`.

   ```bash
   sudo -u cockroach env $(sudo cat /etc/cockroach-rollout-agent.env | xargs) \
     cockroach-rollout-agent status
   ```

3. **Approve it.** Agents then upgrade one node at a time. Each waits for the
   install lease and for the health gate.

   ```bash
   cockroach-rollout-agent approve v25.4.1
   ```

   If the release notes matched a warning pattern, read them first, then add
   `--accept-release-note-warnings`.

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
  already upgraded stay upgraded.

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
