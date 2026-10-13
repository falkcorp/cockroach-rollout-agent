### Fixed

#### Upgrades to CockroachDB 26.1 no longer roll back

CockroachDB 26.1 rejects `crdb_internal` and `system` reads unless the session
sets `allow_unsafe_internals`. The agent reads `crdb_internal.gossip_nodes`
to see each node's liveness and build, so after an upgraded node restarted
on 26.1 the health wait could never see it, and the node rolled back once
the restart timeout ran out.

Every agent connection now sets `allow_unsafe_internals = true`. Older
releases that don't know the variable ignore it and keep working.
