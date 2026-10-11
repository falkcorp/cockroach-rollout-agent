// file: src/rollout.rs
// version: 1.3.1
// guid: ceae8795-4955-43fe-9829-2f28332e8005
// last-edited: 2026-10-11

//! Pure rollout decisions, kept free of SQL and I/O so they can be unit tested.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use semver::Version;

/// Lifecycle of a row in `<schema>.rollouts`.
///
/// ```text
/// proposed --approve--> active --all nodes on target--> finalized
///    |                    |
///    +-----cancel---------+--> cancelled
///                         +--install failed--> failed --cancel--> cancelled
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutStatus {
    /// Computed by the leader; nothing happens until an operator approves it.
    Proposed,
    /// Approved; agents take the install lease one at a time.
    Active,
    Finalized,
    /// An install failed and was rolled back. Everything halts until cancel.
    Failed,
    Cancelled,
}

impl RolloutStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Active => "active",
            Self::Finalized => "finalized",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for RolloutStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(self.as_str())
    }
}

impl FromStr for RolloutStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "proposed" => Ok(Self::Proposed),
            "active" => Ok(Self::Active),
            "finalized" => Ok(Self::Finalized),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(format!("unknown rollout status: {other}")),
        }
    }
}

/// What a rollout does to each node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutKind {
    /// Swap the binary to a new version, one node at a time.
    Upgrade,
    /// Restart every node in turn on the version it already runs.
    Restart,
}

impl RolloutKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upgrade => "upgrade",
            Self::Restart => "restart",
        }
    }
}

impl fmt::Display for RolloutKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(self.as_str())
    }
}

impl FromStr for RolloutKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "upgrade" => Ok(Self::Upgrade),
            "restart" => Ok(Self::Restart),
            other => Err(format!("unknown rollout kind: {other}")),
        }
    }
}

/// One non-decommissioned cluster member as seen through gossip.
#[derive(Debug, Clone)]
pub struct NodeObservation {
    pub node_id: i64,
    pub is_live: bool,
    /// Parsed from `build_tag`; `None` if the tag did not parse.
    pub version: Option<Version>,
}

/// True when every active member is live and reports `target` as its build.
///
/// This reads what the cluster says each node runs, instead of trusting agent
/// status rows, so a node without an agent correctly blocks completion.
pub fn all_nodes_on(target: &Version, nodes: &[NodeObservation]) -> bool {
    !nodes.is_empty()
        && nodes
            .iter()
            .all(|node| node.is_live && node.version.as_ref() == Some(target))
}

/// The oldest and newest build across active members.
///
/// A settled cluster reports one version, so both are equal. A cluster left
/// partway through an upgrade, for example by a hand-upgraded node, reports
/// two, and the leader may resume from the older one. Errors explain why no
/// proposal can be made, for the audit log.
pub fn live_version_span(nodes: &[NodeObservation]) -> Result<(Version, Version), String> {
    if nodes.is_empty() {
        return Err("no active cluster members observed".to_string());
    }
    let mut versions = Vec::with_capacity(nodes.len());
    for node in nodes {
        if !node.is_live {
            return Err(format!("node {} is not live", node.node_id));
        }
        let Some(version) = node.version.clone() else {
            return Err(format!(
                "node {} reports an unparseable build",
                node.node_id
            ));
        };
        versions.push(version);
    }
    versions.sort();
    versions.dedup();
    match versions.as_slice() {
        [only] => Ok((only.clone(), only.clone())),
        [oldest, newest] => Ok((oldest.clone(), newest.clone())),
        _ => Err(format!(
            "nodes run {} different versions; at most two (one upgrade step) can be resumed",
            versions.len()
        )),
    }
}

/// What the leader should do with the newest open rollout.
#[derive(Debug, PartialEq, Eq)]
pub enum LeaderAction {
    /// No open rollout: compute and record a proposal.
    Propose,
    /// Waiting on an operator, on installs, or on a failed rollout to be cancelled.
    Wait,
    /// Patch rollout finished: record it as finalized, nothing to run.
    MarkFinalized,
    /// Major rollout finished and auto-finalize is on.
    Finalize,
    /// Major rollout finished; an operator must run `finalize`.
    AwaitOperatorFinalize,
}

pub fn leader_action(
    open: Option<(RolloutStatus, bool)>,
    all_nodes_on_target: bool,
    auto_finalize: bool,
) -> LeaderAction {
    match open {
        None => LeaderAction::Propose,
        Some((RolloutStatus::Active, requires_finalization)) if all_nodes_on_target => {
            if !requires_finalization {
                LeaderAction::MarkFinalized
            } else if auto_finalize {
                LeaderAction::Finalize
            } else {
                LeaderAction::AwaitOperatorFinalize
            }
        }
        Some(_) => LeaderAction::Wait,
    }
}

/// What a node's agent should do about an active rollout.
#[derive(Debug, PartialEq, Eq)]
pub enum FollowerAction {
    /// This node already runs the target.
    AlreadyOnTarget,
    /// This node runs something other than the rollout's starting version.
    UnexpectedVersion,
    /// This node needs the upgrade; it must hold the install lease first.
    Upgrade,
}

pub fn follower_action(installed: &Version, from: &Version, target: &Version) -> FollowerAction {
    if installed == target {
        FollowerAction::AlreadyOnTarget
    } else if installed == from {
        FollowerAction::Upgrade
    } else {
        FollowerAction::UnexpectedVersion
    }
}

/// The oldest agent release that understands `RolloutKind::Restart`. Older
/// agents ignore `kind`, so as leader they would end a restart at once and as
/// followers they would report done without restarting anything.
pub const MIN_RESTART_AGENT_VERSION: &str = "0.1.0";

/// One `agent_status` row, reduced to what the restart decisions need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentObservation {
    pub agent_id: String,
    /// The CockroachDB node the agent runs beside; `None` for agents older
    /// than `MIN_RESTART_AGENT_VERSION`, which never wrote it.
    pub node_id: Option<i64>,
    pub agent_version: Option<String>,
    pub restarted_rollout_id: Option<String>,
}

/// What a node's agent should do about an active restart.
#[derive(Debug, PartialEq, Eq)]
pub enum RestartAction {
    AlreadyRestarted,
    /// Take the install lease, pass the health gate, then restart.
    Restart,
}

pub fn restart_action(rollout_id: &str, restarted_rollout_id: Option<&str>) -> RestartAction {
    if restarted_rollout_id == Some(rollout_id) {
        RestartAction::AlreadyRestarted
    } else {
        RestartAction::Restart
    }
}

/// True when every active node is live and its agent has recorded this
/// restart. A node without a matching agent keeps the restart open, like a
/// node without an agent keeps an upgrade open.
pub fn restart_complete(
    agents: &[AgentObservation],
    nodes: &[NodeObservation],
    rollout_id: &str,
) -> bool {
    !nodes.is_empty()
        && nodes.iter().all(|node| {
            node.is_live
                && agents.iter().any(|agent| {
                    agent.node_id == Some(node.node_id)
                        && agent.restarted_rollout_id.as_deref() == Some(rollout_id)
                })
        })
}

/// Why the agents cannot run a rolling restart, one line per node; empty
/// when every node has an agent at `minimum` or newer.
pub fn restart_agent_gaps(
    agents: &[AgentObservation],
    nodes: &[NodeObservation],
    minimum: &Version,
) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|node| {
            let Some(agent) = agents
                .iter()
                .find(|agent| agent.node_id == Some(node.node_id))
            else {
                return Some(format!(
                    "node {} has no agent reporting its node id (agents older than {minimum} do not)",
                    node.node_id
                ));
            };
            match agent.agent_version.as_deref().map(Version::parse) {
                Some(Ok(version)) if version >= *minimum => None,
                Some(Ok(version)) => Some(format!(
                    "{} runs agent {version}; it needs {minimum} or newer",
                    agent.agent_id
                )),
                _ => Some(format!(
                    "{} has not reported an agent version; upgrade it to {minimum} or newer",
                    agent.agent_id
                )),
            }
        })
        .collect()
}

/// True once this node is back, live on `expected`, and gossip reports a
/// start time newer than the one read before `systemctl restart`. The start
/// time check rejects gossip that predates the restart.
pub fn restarted_on(
    expected: &Version,
    started_before_us: i64,
    node: &NodeObservation,
    started_now_us: i64,
) -> bool {
    node.is_live && node.version.as_ref() == Some(expected) && started_now_us > started_before_us
}

/// How long an agent may go without a heartbeat before `status` calls it stale.
///
/// Three ticks, so one slow tick or a node restart does not look like a dead agent.
pub fn stale_after(interval: Duration) -> Duration {
    interval.saturating_mul(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).expect("literal should parse")
    }

    fn node(id: i64, live: bool, build: &str) -> NodeObservation {
        NodeObservation {
            node_id: id,
            is_live: live,
            version: Version::parse(build).ok(),
        }
    }

    fn agent(
        node_id: i64,
        agent_version: Option<&str>,
        restarted: Option<&str>,
    ) -> AgentObservation {
        AgentObservation {
            agent_id: format!("host{node_id}:amd64"),
            node_id: Some(node_id),
            agent_version: agent_version.map(str::to_string),
            restarted_rollout_id: restarted.map(str::to_string),
        }
    }

    #[test]
    fn restart_action_skips_nodes_already_restarted() {
        assert_eq!(restart_action("r1", None), RestartAction::Restart);
        assert_eq!(restart_action("r1", Some("r0")), RestartAction::Restart);
        assert_eq!(
            restart_action("r1", Some("r1")),
            RestartAction::AlreadyRestarted
        );
    }

    #[test]
    fn restart_completes_only_when_every_live_node_restarted() {
        let nodes = [node(1, true, "25.4.17"), node(2, true, "25.4.17")];
        let new = Some("0.1.0");
        assert!(
            !restart_complete(&[], &[], "r1"),
            "no nodes is never complete"
        );
        assert!(!restart_complete(&[], &nodes, "r1"));
        assert!(
            !restart_complete(&[agent(1, new, Some("r1"))], &nodes, "r1"),
            "node 2 has no agent"
        );
        assert!(!restart_complete(
            &[agent(1, new, Some("r1")), agent(2, new, Some("r0"))],
            &nodes,
            "r1"
        ));
        assert!(restart_complete(
            &[agent(1, new, Some("r1")), agent(2, new, Some("r1"))],
            &nodes,
            "r1"
        ));
        assert!(
            !restart_complete(
                &[agent(1, new, Some("r1")), agent(2, new, Some("r1"))],
                &[node(1, true, "25.4.17"), node(2, false, "25.4.17")],
                "r1"
            ),
            "a node that is down again is not done"
        );
    }

    #[test]
    fn restart_agent_gaps_names_missing_and_old_agents() {
        let minimum = version("0.1.0");
        let nodes = [
            node(1, true, "25.4.17"),
            node(2, true, "25.4.17"),
            node(3, true, "25.4.17"),
            node(4, true, "25.4.17"),
        ];
        let agents = [
            agent(1, Some("0.1.0"), None),
            agent(2, None, None),
            agent(4, Some("0.0.3"), None),
        ];
        let gaps = restart_agent_gaps(&agents, &nodes, &minimum);
        assert_eq!(gaps.len(), 3, "{gaps:?}");
        assert!(gaps[0].contains("host2:amd64"), "{gaps:?}");
        assert!(gaps[1].contains("node 3"), "{gaps:?}");
        assert!(
            gaps[2].contains("host4:amd64") && gaps[2].contains("0.0.3"),
            "{gaps:?}"
        );
        assert!(
            restart_agent_gaps(&[agent(1, Some("1.0.0"), None)], &nodes[..1], &minimum).is_empty()
        );
    }

    #[test]
    fn restart_is_confirmed_only_by_a_newer_start() {
        let expected = version("25.4.17");
        let live = node(1, true, "25.4.17");
        assert!(restarted_on(&expected, 100, &live, 200));
        assert!(
            !restarted_on(&expected, 100, &live, 100),
            "an unchanged start time is gossip from before the restart"
        );
        assert!(!restarted_on(
            &expected,
            100,
            &node(1, false, "25.4.17"),
            200
        ));
        assert!(!restarted_on(
            &expected,
            100,
            &node(1, true, "25.4.16"),
            200
        ));
    }

    #[test]
    fn status_round_trips_through_text() {
        for status in [
            RolloutStatus::Proposed,
            RolloutStatus::Active,
            RolloutStatus::Finalized,
            RolloutStatus::Failed,
            RolloutStatus::Cancelled,
        ] {
            assert_eq!(status.as_str().parse::<RolloutStatus>(), Ok(status));
        }
        assert!("bogus".parse::<RolloutStatus>().is_err());
    }

    #[test]
    fn kind_round_trips_through_text() {
        for kind in [RolloutKind::Upgrade, RolloutKind::Restart] {
            assert_eq!(kind.as_str().parse::<RolloutKind>(), Ok(kind));
        }
        assert!("rebuild".parse::<RolloutKind>().is_err());
        assert_eq!(format!("{:<8}|", RolloutKind::Restart), "restart |");
    }

    #[test]
    fn version_span_covers_settled_and_partial_clusters() {
        let settled = [node(1, true, "25.3.0"), node(2, true, "25.3.0")];
        assert_eq!(
            live_version_span(&settled).unwrap(),
            (version("25.3.0"), version("25.3.0"))
        );

        let partial = [
            node(1, true, "25.4.17"),
            node(2, true, "25.3.0"),
            node(3, true, "25.3.0"),
        ];
        assert_eq!(
            live_version_span(&partial).unwrap(),
            (version("25.3.0"), version("25.4.17"))
        );

        assert!(live_version_span(&[]).is_err(), "no nodes");
        assert!(
            live_version_span(&[node(1, true, "25.3.0"), node(2, false, "25.3.0")]).is_err(),
            "a dead node blocks proposals"
        );
        assert!(
            live_version_span(&[node(1, true, "25.3.0"), node(2, true, "garbage")]).is_err(),
            "an unparseable build blocks proposals"
        );
        assert!(
            live_version_span(&[
                node(1, true, "25.2.9"),
                node(2, true, "25.3.0"),
                node(3, true, "25.4.17"),
            ])
            .is_err(),
            "three versions is more than one step"
        );
    }

    #[test]
    fn completion_requires_every_node_live_on_target() {
        let target = version("25.4.1");
        assert!(!all_nodes_on(&target, &[]), "no nodes is never complete");
        assert!(all_nodes_on(
            &target,
            &[node(1, true, "25.4.1"), node(2, true, "25.4.1")]
        ));
        assert!(!all_nodes_on(
            &target,
            &[node(1, true, "25.4.1"), node(2, true, "25.3.0")]
        ));
        assert!(!all_nodes_on(
            &target,
            &[node(1, true, "25.4.1"), node(2, false, "25.4.1")]
        ));
        assert!(!all_nodes_on(
            &target,
            &[node(1, true, "25.4.1"), node(2, true, "garbage")]
        ));
    }

    #[test]
    fn leader_only_proposes_when_nothing_is_open() {
        assert_eq!(leader_action(None, false, false), LeaderAction::Propose);
        for status in [RolloutStatus::Proposed, RolloutStatus::Failed] {
            assert_eq!(
                leader_action(Some((status, true)), true, true),
                LeaderAction::Wait,
                "{status} must never auto-advance"
            );
        }
    }

    #[test]
    fn leader_finishes_active_rollouts_by_kind() {
        let active = |requires| Some((RolloutStatus::Active, requires));
        assert_eq!(
            leader_action(active(false), false, true),
            LeaderAction::Wait
        );
        assert_eq!(
            leader_action(active(false), true, false),
            LeaderAction::MarkFinalized
        );
        assert_eq!(
            leader_action(active(true), true, true),
            LeaderAction::Finalize
        );
        assert_eq!(
            leader_action(active(true), true, false),
            LeaderAction::AwaitOperatorFinalize
        );
    }

    #[test]
    fn follower_upgrades_only_from_the_expected_version() {
        let from = version("25.3.0");
        let target = version("25.4.1");
        assert_eq!(
            follower_action(&target, &from, &target),
            FollowerAction::AlreadyOnTarget
        );
        assert_eq!(
            follower_action(&from, &from, &target),
            FollowerAction::Upgrade
        );
        assert_eq!(
            follower_action(&version("25.2.9"), &from, &target),
            FollowerAction::UnexpectedVersion
        );
    }

    #[test]
    fn stale_window_is_three_ticks() {
        assert_eq!(
            stale_after(Duration::from_secs(300)),
            Duration::from_secs(900)
        );
    }
}
