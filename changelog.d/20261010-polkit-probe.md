### Fixed

#### `self-check` always failed restart authorization

The probe ran `pkcheck --detail unit … --detail verb restart` as the
`cockroach` user, but polkit only lets root pass details to
`CheckAuthorization`, so it failed with `NotAuthorized` on every host no matter
what the rule said. `self-check` now runs `systemctl reset-failed <unit>`,
which makes systemd consult polkit with the real unit and verb and is a no-op
on a unit that has not failed. The polkit rule allows `reset-failed` alongside
start, stop, and restart for the one CockroachDB unit.
