// file: src/rollout.rs
// version: 1.1.0
// guid: ceae8795-4955-43fe-9829-2f28332e8005
// last-edited: 2026-10-10

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
        formatter.write_str(self.as_str())
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
