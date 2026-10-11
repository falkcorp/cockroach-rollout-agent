// file: src/cluster_view.rs
// version: 1.0.0
// guid: b65f6d37-171d-4be5-8fbc-306d362e20c2
// last-edited: 2026-10-11

//! One read-only snapshot of everything an operator looks at. `status` prints
//! it and the web dashboard renders it.

use std::time::Duration;

use postgres::Client;
use semver::Version;

use crate::health::ClusterHealth;
use crate::rollout::{AgentObservation, NodeObservation, RolloutKind, RolloutStatus, stale_after};
use crate::{
    AppError, Cli, DEFAULT_DAEMON_INTERVAL_SECONDS, INSTALL_LEASE, LEADER_LEASE, OpenRollout,
    cluster_health, cluster_nodes, major_line, open_rollout, sql_ident,
};

const ROLLOUTS_SHOWN: i64 = 10;
const EVENTS_SHOWN: i64 = 50;

#[derive(Debug, Clone)]
pub struct ClusterView {
    pub rollouts: Vec<RolloutRow>,
    pub open: Option<OpenSummary>,
    pub nodes: Vec<NodeObservation>,
    pub agents: Vec<AgentRow>,
    pub leases: Vec<LeaseRow>,
    pub health: Result<ClusterHealth, String>,
    pub events: Vec<EventRow>,
    pub stale_after_secs: u64,
}

#[derive(Debug, Clone)]
pub struct RolloutRow {
    pub id: String,
    pub kind: RolloutKind,
    pub status: RolloutStatus,
    pub from: String,
    pub to: String,
    pub created_by: String,
    pub created_at: String,
    pub approved_by: Option<String>,
    pub approved_at: Option<String>,
    pub error: Option<String>,
}

/// The open rollout, reduced to what operators and prechecks need.
#[derive(Debug, Clone)]
pub struct OpenSummary {
    pub id: String,
    pub kind: RolloutKind,
    pub status: RolloutStatus,
    pub from: Version,
    pub to: Version,
    pub warnings: Vec<String>,
    pub release_notes_url: String,
}

impl OpenSummary {
    pub fn requires_finalization(&self) -> bool {
        major_line(&self.from) != major_line(&self.to)
    }
}

impl From<&OpenRollout> for OpenSummary {
    fn from(rollout: &OpenRollout) -> Self {
        Self {
            id: rollout.id.clone(),
            kind: rollout.kind,
            status: rollout.status,
            from: rollout.manifest.current_version.clone(),
            to: rollout.manifest.target_version.clone(),
            warnings: rollout.manifest.release_note_warnings.clone(),
            release_notes_url: rollout.manifest.release_notes_url.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentRow {
    pub observation: AgentObservation,
    pub state: String,
    pub version: String,
    pub error: Option<String>,
    /// Seconds since the agent's last heartbeat.
    pub age_secs: i64,
}

impl AgentRow {
    pub fn is_stale(&self, stale_after_secs: u64) -> bool {
        self.age_secs > stale_after_secs as i64
    }
}

#[derive(Debug, Clone)]
pub struct LeaseRow {
    pub name: String,
    pub holder: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub at: String,
    pub agent_id: String,
    pub source: String,
    pub kind: String,
    pub detail: String,
}

pub fn load(cli: &Cli, client: &mut Client) -> Result<ClusterView, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let mut rollouts = Vec::new();
    for row in client.query(
        &format!(
            "
            SELECT id::STRING, kind, status, current_version, target_version, created_by,
                   created_at::STRING, approved_by, approved_at::STRING, error
            FROM {schema}.rollouts
            ORDER BY created_at DESC
            LIMIT $1
            "
        ),
        &[&ROLLOUTS_SHOWN],
    )? {
        let kind: String = row.get(1);
        let status: String = row.get(2);
        rollouts.push(RolloutRow {
            id: row.get(0),
            kind: kind.parse().map_err(AppError::Message)?,
            status: status.parse().map_err(AppError::Message)?,
            from: row.get(3),
            to: row.get(4),
            created_by: row.get(5),
            created_at: row.get(6),
            approved_by: row.get(7),
            approved_at: row.get(8),
            error: row.get(9),
        });
    }

    let open = open_rollout(cli, client)?.as_ref().map(OpenSummary::from);
    let nodes = cluster_nodes(client)?;
    let agents = agent_rows(cli, client)?;

    let mut leases = Vec::new();
    for name in [LEADER_LEASE, INSTALL_LEASE] {
        let rows = client.query(
            &format!(
                "SELECT holder_id, expires_at::STRING FROM {schema}.leases WHERE name = $1 AND expires_at > now()"
            ),
            &[&name],
        )?;
        leases.push(LeaseRow {
            name: name.to_string(),
            holder: rows.first().map(|row| row.get(0)),
            expires_at: rows.first().map(|row| row.get(1)),
        });
    }

    let health = cluster_health(client).map_err(|error| error.to_string());

    let events = client
        .query(
            &format!(
                "
                SELECT at::STRING, agent_id, source, kind, detail
                FROM {schema}.events
                ORDER BY at DESC
                LIMIT $1
                "
            ),
            &[&EVENTS_SHOWN],
        )?
        .iter()
        .map(|row| EventRow {
            at: row.get(0),
            agent_id: row.get(1),
            source: row.get(2),
            kind: row.get(3),
            detail: row.get(4),
        })
        .collect();

    Ok(ClusterView {
        rollouts,
        open,
        nodes,
        agents,
        leases,
        health,
        events,
        stale_after_secs: stale_after(Duration::from_secs(DEFAULT_DAEMON_INTERVAL_SECONDS))
            .as_secs(),
    })
}

fn agent_rows(cli: &Cli, client: &mut Client) -> Result<Vec<AgentRow>, AppError> {
    let schema = sql_ident(&cli.schema)?;
    Ok(client
        .query(
            &format!(
                "
                SELECT agent_id, state, version, error,
                       extract(epoch FROM now() - coalesce(last_seen, updated_at))::INT8,
                       node_id, agent_version, restarted_rollout_id
                FROM {schema}.agent_status
                ORDER BY agent_id
                "
            ),
            &[],
        )?
        .iter()
        .map(|row| AgentRow {
            observation: AgentObservation {
                agent_id: row.get(0),
                node_id: row.get(5),
                agent_version: row.get(6),
                restarted_rollout_id: row.get(7),
            },
            state: row.get(1),
            version: row.get(2),
            error: row.get(3),
            age_secs: row.get(4),
        })
        .collect())
}

pub fn agent_observations(
    cli: &Cli,
    client: &mut Client,
) -> Result<Vec<AgentObservation>, AppError> {
    Ok(agent_rows(cli, client)?
        .into_iter()
        .map(|row| row.observation)
        .collect())
}

#[cfg(test)]
impl ClusterView {
    /// Three healthy nodes on 25.4.17 with current agents and nothing open.
    pub fn sample() -> Self {
        let version = Version::new(25, 4, 17);
        Self {
            rollouts: Vec::new(),
            open: None,
            nodes: (1..=3)
                .map(|node_id| NodeObservation {
                    node_id,
                    is_live: true,
                    version: Some(version.clone()),
                })
                .collect(),
            agents: (1..=3)
                .map(|node_id| AgentRow {
                    observation: AgentObservation {
                        agent_id: format!("host{node_id}:amd64"),
                        node_id: Some(node_id),
                        agent_version: Some("0.1.0".to_string()),
                        restarted_rollout_id: None,
                    },
                    state: "idle".to_string(),
                    version: version.to_string(),
                    error: None,
                    age_secs: 10,
                })
                .collect(),
            leases: vec![
                LeaseRow {
                    name: LEADER_LEASE.to_string(),
                    holder: Some("host1:amd64".to_string()),
                    expires_at: Some("2026-10-11 12:00:00+00".to_string()),
                },
                LeaseRow {
                    name: INSTALL_LEASE.to_string(),
                    holder: None,
                    expires_at: None,
                },
            ],
            health: Ok(ClusterHealth {
                active_nodes: 3,
                live_nodes: 3,
                underreplicated_ranges: 0,
                unavailable_ranges: 0,
            }),
            events: Vec::new(),
            stale_after_secs: 900,
        }
    }
}

#[cfg(test)]
impl OpenSummary {
    pub fn sample(kind: RolloutKind, status: RolloutStatus, from: &str, to: &str) -> Self {
        Self {
            id: "r1".to_string(),
            kind,
            status,
            from: Version::parse(from).expect("literal should parse"),
            to: Version::parse(to).expect("literal should parse"),
            warnings: Vec::new(),
            release_notes_url: "https://www.cockroachlabs.com/docs/releases/v25.4".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalization_is_needed_only_across_release_lines() {
        let major = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Active,
            "25.3.0",
            "25.4.17",
        );
        let patch = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Active,
            "25.4.16",
            "25.4.17",
        );
        assert!(major.requires_finalization());
        assert!(!patch.requires_finalization());
    }

    #[test]
    fn open_summary_copies_the_rollout() {
        let rollout = crate::OpenRollout {
            id: "r9".to_string(),
            kind: RolloutKind::Restart,
            status: RolloutStatus::Active,
            manifest: crate::RolloutManifest {
                schema_version: 1,
                created_unix: 0,
                current_version: Version::new(25, 4, 17),
                target_version: Version::new(25, 4, 17),
                release_notes_url: String::new(),
                release_note_warnings: vec!["removed".to_string()],
                release_note_warnings_approved: true,
                artifacts: Vec::new(),
            },
        };
        let summary = OpenSummary::from(&rollout);
        assert_eq!(summary.id, "r9");
        assert_eq!(summary.kind, RolloutKind::Restart);
        assert_eq!(summary.to, Version::new(25, 4, 17));
        assert_eq!(summary.warnings, vec!["removed".to_string()]);
    }

    #[test]
    fn agents_go_stale_after_the_window() {
        let mut agent = ClusterView::sample().agents.remove(0);
        agent.age_secs = 900;
        assert!(!agent.is_stale(900));
        agent.age_secs = 901;
        assert!(agent.is_stale(900));
    }
}
