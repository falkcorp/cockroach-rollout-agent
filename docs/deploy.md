<!-- file: docs/deploy.md -->
<!-- version: 1.0.0 -->
<!-- guid: 41eb3d6e-f70e-431d-8f3e-33d1ca5e45c1 -->
<!-- last-edited: 2026-05-26 -->

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

## 2. Configure Host Directories

```bash
sudo install -o cockroach -g cockroach -m 0750 -d /var/lib/cockroach-rollout-agent
sudo install -o cockroach -g cockroach -m 0750 -d /var/lib/cockroach-rollout-agent/artifacts
sudo install -o cockroach -g cockroach -m 0750 -d /var/log/cockroach-rollout-agent
```

If `/usr/local/bin/cockroach` is root-owned, grant the narrowest practical write
permission to the CockroachDB binary path or use a controlled install directory
owned by `cockroach`.

## 3. Configure SQL Access

Create a CockroachDB SQL user for rollout coordination and grant access to the
database where the rollout schema will be created.

Example shape:

```sql
CREATE USER rollout;
GRANT CREATE ON DATABASE defaultdb TO rollout;
GRANT SELECT ON DATABASE defaultdb TO rollout;
```

Use the certificate/authentication model already approved for your cluster.
Store the connection string only on each host:

```bash
sudo install -o root -g cockroach -m 0640 \
  examples/cockroach-rollout-agent.env.example \
  /etc/cockroach-rollout-agent.env
sudo editor /etc/cockroach-rollout-agent.env
```

Set `CROACH_ROLLOUT_DATABASE_URL` to a secure CockroachDB SQL URL.

## 4. Initialize Coordination Tables

Run once from any host:

```bash
sudo -u cockroach /usr/local/bin/cockroach-rollout-agent \
  --database-url "$CROACH_ROLLOUT_DATABASE_URL" \
  init-db
```

Verify discovery:

```bash
sudo -u cockroach /usr/local/bin/cockroach-rollout-agent \
  --database-url "$CROACH_ROLLOUT_DATABASE_URL" \
  discover
```

## 5. Install Systemd Unit

```bash
sudo install -o root -g root -m 0644 \
  examples/cockroach-rollout-agent.service \
  /etc/systemd/system/cockroach-rollout-agent.service
sudo systemctl daemon-reload
sudo systemctl enable --now cockroach-rollout-agent.service
```

Check logs:

```bash
journalctl -u cockroach-rollout-agent.service -f
sudo tail -f /var/log/cockroach-rollout-agent/audit.log
```

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
