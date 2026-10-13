<!-- file: changelog.d/20261013-agent-confirm-unreachable.md -->
<!-- version: 1.0.0 -->
<!-- guid: 7f2d1c9e-3b4a-4e8f-a6d2-9c1e5b7a3f60 -->
<!-- last-edited: 2026-10-13 -->

### Fixed

- A freshly updated agent that cannot reach its node or the network keeps
  waiting instead of being rolled back after 10 minutes. Only failures that
  reach the database count towards the confirm deadline.
