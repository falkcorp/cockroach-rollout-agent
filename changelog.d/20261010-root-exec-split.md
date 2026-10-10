### Security

#### Root could run a binary the `cockroach` user controls

The installer made `/usr/local/bin/cockroach` a symlink into the agent's
directory, which the `cockroach` user can write. Anything running as that user
could replace the binary, and the next `sudo cockroach …` ran it as root. The
installer also ran the staged binary as root, and `--uninstall` followed the
agent-owned symlink as root and copied whatever it resolved to into a
world-readable `/usr/local/bin/cockroach`.

The CockroachDB unit now runs `/var/lib/cockroach-rollout-agent/bin/cockroach`
through an `ExecStart` systemd drop-in. `/usr/local/bin/cockroach` stays a
root-owned file the agent never touches; it can lag the server after a rollout.
The installer does every operation inside the agent root as `cockroach` and
never follows links there as root. `--uninstall` removes the drop-in and refuses
while the two binaries are different versions. The agent's `self-check` and
upgrade path verify the unit's `ExecStart` with `systemctl show`.
`CROACH_ROLLOUT_BINARY_PATH` now defaults to `<agent-root>/bin/cockroach`.
