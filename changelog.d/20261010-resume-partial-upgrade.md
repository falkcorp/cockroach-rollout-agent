### Fixed

#### A partially upgraded cluster could never start a rollout

The leader proposed a rollout only when every node ran the same version, so a
cluster with one node upgraded by hand (for example one node on v25.4.17 and
the rest on v25.3.0) stalled forever. The leader now resumes such an upgrade:
when the live nodes span exactly two versions and the newer one is the next
upgrade step from the older, it proposes that step, the nodes already on it
count as done, and the rest upgrade one at a time under the health gate. Any
other spread is skipped with an audit entry explaining why.
