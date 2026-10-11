// file: src/ops.rs
// version: 1.0.0
// guid: 2f1695d6-80fb-4adc-9927-98f3e3b1c028
// last-edited: 2026-10-11

//! Operator actions shared by the CLI and the web UI. Every precondition is
//! checked here, so the two paths can never enforce different rules.

use postgres::Client;
use semver::Version;

use crate::cluster_view::{self, ClusterView, OpenSummary};
use crate::events::{self, Source};
use crate::rollout::{
    AgentObservation, MIN_RESTART_AGENT_VERSION, NodeObservation, RolloutKind, RolloutStatus,
    all_nodes_on, live_version_span, restart_agent_gaps,
};
use crate::{
    AppError, Cli, OpenRollout, RolloutManifest, agent_node_id, cluster_nodes, finalize_cluster,
    major_line_string, open_rollout, parse_cockroach_version, reject_prerelease,
    set_rollout_status, sql_ident, unix_time,
};

/// Who asked for an action; recorded as the approver and on the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    Cli,
    Web { ip: String },
}

impl Actor {
    pub fn label(&self, cli: &Cli) -> String {
        match self {
            Self::Cli => {
                std::env::var("USER").unwrap_or_else(|_| agent_node_id(cli).unwrap_or_default())
            }
            Self::Web { ip } => format!("web:{ip}"),
        }
    }

    pub fn source(&self) -> Source {
        match self {
            Self::Cli => Source::Cli,
            Self::Web { .. } => Source::Web,
        }
    }
}

/// An action the dashboard may offer for the current snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvailableAction {
    Approve {
        version: Version,
        warnings: Vec<String>,
        release_notes_url: String,
    },
    Cancel,
    Finalize {
        target: Version,
    },
    Restart {
        version: Version,
    },
    /// Nothing is open, but a restart is not safe yet; shows the reason.
    RestartBlocked {
        reason: String,
    },
}

pub fn approve_precheck(
    open: Option<&OpenSummary>,
    version: &Version,
    accept_warnings: bool,
) -> Result<(), String> {
    let open = open.ok_or_else(|| "there is no proposed rollout".to_string())?;
    if open.status != RolloutStatus::Proposed {
        return Err(format!(
            "the open rollout to {} is {}, not proposed",
            open.to, open.status
        ));
    }
    if open.to != *version {
        return Err(format!("the proposal targets {}, not {version}", open.to));
    }
    if !open.warnings.is_empty() && !accept_warnings {
        return Err(format!(
            "release notes matched {:?}; review {}, then accept the release-note warnings \
             (--accept-release-note-warnings on the CLI, the checkbox in the UI)",
            open.warnings, open.release_notes_url
        ));
    }
    Ok(())
}

pub fn finalize_precheck(
    open: Option<&OpenSummary>,
    nodes: &[NodeObservation],
    version: &Version,
) -> Result<(), String> {
    reject_prerelease(version).map_err(|error| error.to_string())?;
    if let Some(open) = open {
        if open.kind == RolloutKind::Restart {
            return Err("a rolling restart is open; finalize applies to upgrades".to_string());
        }
        if open.to != *version {
            return Err(format!(
                "the open upgrade targets {}, not {version}",
                open.to
            ));
        }
    }
    if !all_nodes_on(version, nodes) {
        return Err(format!(
            "refusing to finalize: not every node is live on {version}; see status"
        ));
    }
    Ok(())
}

/// Returns the version every node will restart on.
pub fn restart_precheck(
    open: Option<&OpenSummary>,
    nodes: &[NodeObservation],
    agents: &[AgentObservation],
) -> Result<Version, String> {
    if let Some(open) = open {
        return Err(format!(
            "a {} rollout to {} is {}; finish or cancel it first",
            open.kind, open.to, open.status
        ));
    }
    let (oldest, newest) = live_version_span(nodes)?;
    if oldest != newest {
        return Err(format!(
            "nodes run {oldest} and {newest}; a rolling restart needs every node on one version"
        ));
    }
    let minimum = Version::parse(MIN_RESTART_AGENT_VERSION).expect("constant is a valid version");
    let gaps = restart_agent_gaps(agents, nodes, &minimum);
    if !gaps.is_empty() {
        return Err(format!("cannot restart safely: {}", gaps.join("; ")));
    }
    Ok(oldest)
}

/// Actions valid for `view`. The server runs the matching precheck again on
/// POST, because the view may be stale by then.
pub fn available_actions(view: &ClusterView) -> Vec<AvailableAction> {
    let open = view.open.as_ref();
    let mut actions = Vec::new();
    if let Some(open) = open {
        if approve_precheck(Some(open), &open.to, true).is_ok() {
            actions.push(AvailableAction::Approve {
                version: open.to.clone(),
                warnings: open.warnings.clone(),
                release_notes_url: open.release_notes_url.clone(),
            });
        }
        if matches!(open.status, RolloutStatus::Active | RolloutStatus::Failed) {
            actions.push(AvailableAction::Cancel);
        }
        if open.kind == RolloutKind::Upgrade
            && open.status == RolloutStatus::Active
            && open.requires_finalization()
            && finalize_precheck(Some(open), &view.nodes, &open.to).is_ok()
        {
            actions.push(AvailableAction::Finalize {
                target: open.to.clone(),
            });
        }
    }
    let agents: Vec<AgentObservation> = view
        .agents
        .iter()
        .map(|agent| agent.observation.clone())
        .collect();
    match restart_precheck(open, &view.nodes, &agents) {
        Ok(version) => actions.push(AvailableAction::Restart { version }),
        Err(reason) if open.is_none() => actions.push(AvailableAction::RestartBlocked { reason }),
        Err(_) => {}
    }
    actions
}

fn summary_of(
    cli: &Cli,
    client: &mut Client,
) -> Result<Option<(OpenRollout, OpenSummary)>, AppError> {
    Ok(open_rollout(cli, client)?.map(|rollout| {
        let summary = OpenSummary::from(&rollout);
        (rollout, summary)
    }))
}

pub fn approve(
    cli: &Cli,
    client: &mut Client,
    actor: &Actor,
    version: &str,
    accept_warnings: bool,
) -> Result<String, AppError> {
    let target = parse_cockroach_version(version)?;
    let open = summary_of(cli, client)?;
    approve_precheck(
        open.as_ref().map(|(_, summary)| summary),
        &target,
        accept_warnings,
    )
    .map_err(AppError::Message)?;
    let (rollout, _) = open.expect("the precheck passed, so a rollout is open");

    if rollout.requires_finalization() {
        // Without this, CockroachDB finalizes on its own the moment the last
        // node restarts, and the downgrade window closes before anyone looks.
        let line = major_line_string(&rollout.manifest.current_version);
        client.batch_execute(&format!(
            "SET CLUSTER SETTING cluster.preserve_downgrade_option = '{line}'"
        ))?;
    }

    let mut manifest = rollout.manifest;
    manifest.release_note_warnings_approved = accept_warnings;
    let schema = sql_ident(&cli.schema)?;
    let approver = actor.label(cli);
    let updated = client.execute(
        &format!(
            "
            UPDATE {schema}.rollouts
            SET status = 'active', approved_by = $2, approved_at = now(), manifest_json = $3
            WHERE id::STRING = $1 AND status = 'proposed'
            "
        ),
        &[&rollout.id, &approver, &serde_json::to_string(&manifest)?],
    )?;
    if updated != 1 {
        return Err(AppError::Message(
            "the proposal changed while approving; reload status".to_string(),
        ));
    }
    events::note(
        cli,
        client,
        actor.source(),
        "rollout_approved",
        &format!(
            "current={} target={} by={approver}",
            manifest.current_version, manifest.target_version
        ),
    )?;
    Ok(format!(
        "approved {} -> {}; agents will upgrade one node at a time",
        manifest.current_version, manifest.target_version
    ))
}

pub fn cancel(
    cli: &Cli,
    client: &mut Client,
    actor: &Actor,
    reason: &str,
) -> Result<String, AppError> {
    let Some(rollout) = open_rollout(cli, client)? else {
        return Ok("no open rollout".to_string());
    };
    set_rollout_status(
        cli,
        client,
        &rollout.id,
        RolloutStatus::Cancelled,
        Some(reason),
    )?;
    events::note(
        cli,
        client,
        actor.source(),
        "rollout_cancelled",
        &format!(
            "kind={} target={} reason={reason} by={}",
            rollout.kind,
            rollout.manifest.target_version,
            actor.label(cli)
        ),
    )?;
    let mut message = format!(
        "cancelled {} {} rollout to {}",
        rollout.status, rollout.kind, rollout.manifest.target_version
    );
    if rollout.kind == RolloutKind::Upgrade
        && rollout.status != RolloutStatus::Proposed
        && rollout.requires_finalization()
    {
        message.push_str(
            "; cluster.preserve_downgrade_option is still set, so already-upgraded nodes can be \
             rolled back; RESET it only once every node runs one version",
        );
    }
    Ok(message)
}

pub fn finalize(
    cli: &Cli,
    client: &mut Client,
    actor: &Actor,
    target_version: &str,
    dry_run: bool,
) -> Result<String, AppError> {
    let version = parse_cockroach_version(target_version)?;
    let open = summary_of(cli, client)?;
    let nodes = cluster_nodes(client)?;
    finalize_precheck(open.as_ref().map(|(_, summary)| summary), &nodes, &version)
        .map_err(AppError::Message)?;
    let line = major_line_string(&version);
    if dry_run {
        return Ok(format!(
            "dry run: RESET CLUSTER SETTING cluster.preserve_downgrade_option; \
             SET CLUSTER SETTING version = '{line}'"
        ));
    }
    finalize_cluster(cli, client, &version)?;
    if let Some((rollout, _)) = open.filter(|(rollout, _)| {
        rollout.kind == RolloutKind::Upgrade && rollout.manifest.target_version == version
    }) {
        set_rollout_status(cli, client, &rollout.id, RolloutStatus::Finalized, None)?;
    }
    events::note(
        cli,
        client,
        actor.source(),
        "finalized",
        &format!("cluster_version={line} by={}", actor.label(cli)),
    )?;
    Ok(format!(
        "finalized cluster version {line}; downgrading to the previous release line is no longer possible"
    ))
}

pub fn restart_cluster(cli: &Cli, client: &mut Client, actor: &Actor) -> Result<String, AppError> {
    let open = summary_of(cli, client)?;
    let nodes = cluster_nodes(client)?;
    let agents = cluster_view::agent_observations(cli, client)?;
    let version = restart_precheck(open.as_ref().map(|(_, summary)| summary), &nodes, &agents)
        .map_err(AppError::Message)?;

    // A restart stores an ordinary manifest so manifest_json stays NOT NULL
    // and open_rollout keeps one shape: same version both ends, nothing to
    // download, nothing to warn about.
    let manifest = RolloutManifest {
        schema_version: 1,
        created_unix: unix_time()?,
        current_version: version.clone(),
        target_version: version.clone(),
        release_notes_url: String::new(),
        release_note_warnings: Vec::new(),
        release_note_warnings_approved: true,
        artifacts: Vec::new(),
    };
    let schema = sql_ident(&cli.schema)?;
    let by = actor.label(cli);
    let inserted = client.execute(
        &format!(
            "
            INSERT INTO {schema}.rollouts
                (kind, status, current_version, target_version, manifest_json,
                 created_by, approved_by, approved_at)
            SELECT 'restart', 'active', $1, $1, $2, $3, $3, now()
            WHERE NOT EXISTS (
                SELECT 1 FROM {schema}.rollouts WHERE status IN ('proposed', 'active', 'failed')
            )
            "
        ),
        &[
            &version.to_string(),
            &serde_json::to_string(&manifest)?,
            &by,
        ],
    )?;
    if inserted != 1 {
        return Err(AppError::Message(
            "another rollout opened while starting the restart; reload and try again".to_string(),
        ));
    }
    events::note(
        cli,
        client,
        actor.source(),
        "restart_started",
        &format!("version={version} nodes={} by={by}", nodes.len()),
    )?;
    Ok(format!(
        "rolling restart of {} nodes on {version} started; agents restart one node at a time",
        nodes.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).expect("literal should parse")
    }

    fn names(actions: &[AvailableAction]) -> Vec<&'static str> {
        actions
            .iter()
            .map(|action| match action {
                AvailableAction::Approve { .. } => "approve",
                AvailableAction::Cancel => "cancel",
                AvailableAction::Finalize { .. } => "finalize",
                AvailableAction::Restart { .. } => "restart",
                AvailableAction::RestartBlocked { .. } => "restart_blocked",
            })
            .collect()
    }

    fn with_open(open: OpenSummary) -> ClusterView {
        ClusterView {
            open: Some(open),
            ..ClusterView::sample()
        }
    }

    #[test]
    fn approve_needs_the_reviewed_proposal() {
        let proposed = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Proposed,
            "25.3.0",
            "25.4.17",
        );
        assert!(approve_precheck(Some(&proposed), &v("25.4.17"), false).is_ok());
        assert!(approve_precheck(None, &v("25.4.17"), false).is_err());
        let wrong = approve_precheck(Some(&proposed), &v("25.4.16"), false).unwrap_err();
        assert!(wrong.contains("targets 25.4.17"), "{wrong}");
        let active = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Active,
            "25.3.0",
            "25.4.17",
        );
        let not_proposed = approve_precheck(Some(&active), &v("25.4.17"), false).unwrap_err();
        assert!(not_proposed.contains("not proposed"), "{not_proposed}");
        let warned = OpenSummary {
            warnings: vec!["removed".to_string()],
            ..proposed
        };
        assert!(approve_precheck(Some(&warned), &v("25.4.17"), false).is_err());
        assert!(approve_precheck(Some(&warned), &v("25.4.17"), true).is_ok());
    }

    #[test]
    fn finalize_needs_every_node_on_an_upgrade_target() {
        let nodes = ClusterView::sample().nodes; // all live on 25.4.17
        assert!(finalize_precheck(None, &nodes, &v("25.4.17")).is_ok());
        assert!(finalize_precheck(None, &nodes, &v("25.4.16")).is_err());
        assert!(finalize_precheck(None, &nodes, &v("25.4.17-alpha.1")).is_err());
        let upgrade = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Active,
            "25.3.0",
            "25.4.17",
        );
        assert!(finalize_precheck(Some(&upgrade), &nodes, &v("25.4.17")).is_ok());
        let restart = OpenSummary::sample(
            RolloutKind::Restart,
            RolloutStatus::Active,
            "25.4.17",
            "25.4.17",
        );
        let refused = finalize_precheck(Some(&restart), &nodes, &v("25.4.17")).unwrap_err();
        assert!(refused.contains("restart"), "{refused}");
        let elsewhere = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Active,
            "25.3.0",
            "25.4.16",
        );
        let mismatch = finalize_precheck(Some(&elsewhere), &nodes, &v("25.4.17")).unwrap_err();
        assert!(mismatch.contains("targets 25.4.16"), "{mismatch}");
    }

    #[test]
    fn restart_needs_no_open_rollout_one_version_and_new_agents() {
        let view = ClusterView::sample();
        let agents: Vec<_> = view
            .agents
            .iter()
            .map(|agent| agent.observation.clone())
            .collect();
        assert_eq!(
            restart_precheck(None, &view.nodes, &agents),
            Ok(v("25.4.17"))
        );

        let open = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Proposed,
            "25.3.0",
            "25.4.17",
        );
        assert!(restart_precheck(Some(&open), &view.nodes, &agents).is_err());

        let mut mixed = view.nodes.clone();
        mixed[0].version = Some(v("25.4.16"));
        let reason = restart_precheck(None, &mixed, &agents).unwrap_err();
        assert!(reason.contains("one version"), "{reason}");

        let mut old = agents.clone();
        old[1].agent_version = Some("0.0.3".to_string());
        let reason = restart_precheck(None, &view.nodes, &old).unwrap_err();
        assert!(reason.contains("host2:amd64"), "{reason}");
    }

    #[test]
    fn available_actions_follow_the_cluster_state() {
        let proposed = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Proposed,
            "25.3.0",
            "25.4.17",
        );
        assert_eq!(names(&available_actions(&with_open(proposed))), ["approve"]);

        let midway = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Active,
            "25.3.0",
            "25.4.17",
        );
        let mut view = with_open(midway.clone());
        view.nodes[0].version = Some(v("25.3.0"));
        assert_eq!(names(&available_actions(&view)), ["cancel"]);

        assert_eq!(
            names(&available_actions(&with_open(midway))),
            ["cancel", "finalize"],
            "major line, every node on target: the leader awaits the operator"
        );

        let patch = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Active,
            "25.4.16",
            "25.4.17",
        );
        assert_eq!(
            names(&available_actions(&with_open(patch))),
            ["cancel"],
            "a patch rollout finalizes itself"
        );

        let failed = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Failed,
            "25.3.0",
            "25.4.17",
        );
        assert_eq!(names(&available_actions(&with_open(failed))), ["cancel"]);

        let restarting = OpenSummary::sample(
            RolloutKind::Restart,
            RolloutStatus::Active,
            "25.4.17",
            "25.4.17",
        );
        assert_eq!(
            names(&available_actions(&with_open(restarting))),
            ["cancel"]
        );

        assert_eq!(
            names(&available_actions(&ClusterView::sample())),
            ["restart"]
        );

        let mut mixed = ClusterView::sample();
        mixed.nodes[2].version = Some(v("25.3.0"));
        assert_eq!(names(&available_actions(&mixed)), ["restart_blocked"]);
    }

    #[test]
    fn a_stale_view_is_rechecked_when_the_action_runs() {
        // The page offered Approve while the rollout was proposed...
        let offered = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Proposed,
            "25.3.0",
            "25.4.17",
        );
        assert_eq!(names(&available_actions(&with_open(offered))), ["approve"]);
        // ...but someone approved it from the CLI before this POST arrived.
        let now = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Active,
            "25.3.0",
            "25.4.17",
        );
        assert!(approve_precheck(Some(&now), &v("25.4.17"), false).is_err());
        // Likewise a restart offered on a quiet cluster is refused once a proposal appears.
        let view = ClusterView::sample();
        let agents: Vec<_> = view
            .agents
            .iter()
            .map(|agent| agent.observation.clone())
            .collect();
        let appeared = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Proposed,
            "25.4.17",
            "25.4.18",
        );
        assert!(restart_precheck(Some(&appeared), &view.nodes, &agents).is_err());
    }
}
