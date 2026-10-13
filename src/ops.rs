// file: src/ops.rs
// version: 1.2.0
// guid: 2f1695d6-80fb-4adc-9927-98f3e3b1c028
// last-edited: 2026-10-13

//! Operator actions shared by the CLI and the web UI. Every precondition is
//! checked here, so the two paths can never enforce different rules.

use postgres::Client;
use semver::Version;

use crate::agentupdate;
use crate::cluster_view::{self, ClusterView, OpenSummary};
use crate::events::{self, Source};
use crate::releases;
use crate::rollout::{
    AgentObservation, MIN_RESTART_AGENT_VERSION, NodeObservation, RolloutKind, RolloutStatus,
    all_nodes_on, live_version_span, restart_agent_gaps,
};
use crate::uicert;
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
        /// Must be acknowledged, like release-note warnings.
        settings_warnings: Vec<String>,
        settings_info: Vec<String>,
    },
    /// Say no to a proposal; the leader will not propose its target again.
    Decline {
        rollout_id: String,
        target: Version,
    },
    Cancel,
    Finalize {
        target: Version,
    },
    Restart {
        version: Version,
    },
    /// A restart is not possible right now; shows the reason.
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
    if !open.settings_warnings.is_empty() && !accept_warnings {
        return Err(format!(
            "the upgrade touches overridden cluster settings: {}; review them, then accept \
             the warnings (--accept-release-note-warnings on the CLI, the checkbox in the UI)",
            open.settings_warnings.join("; ")
        ));
    }
    Ok(())
}

/// `rollout_id` is the id the operator saw; the CLI passes `None` and
/// declines whatever is proposed.
pub fn decline_precheck(
    open: Option<&OpenSummary>,
    rollout_id: Option<&str>,
) -> Result<(), String> {
    let open = open.ok_or_else(|| "there is no proposed rollout".to_string())?;
    if rollout_id.is_some_and(|id| id != open.id) {
        return Err(
            "the open rollout changed since the page loaded; reload and try again".to_string(),
        );
    }
    if open.status != RolloutStatus::Proposed {
        return Err(format!(
            "the open rollout to {} is {}; only a proposal can be declined, use cancel",
            open.to, open.status
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
                settings_warnings: open.settings_warnings.clone(),
                settings_info: open.settings_info.clone(),
            });
        }
        if decline_precheck(Some(open), Some(&open.id)).is_ok() {
            actions.push(AvailableAction::Decline {
                rollout_id: open.id.clone(),
                target: open.to.clone(),
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
        Err(reason) => actions.push(AvailableAction::RestartBlocked { reason }),
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

pub fn decline(
    cli: &Cli,
    client: &mut Client,
    actor: &Actor,
    rollout_id: Option<&str>,
    reason: &str,
) -> Result<String, AppError> {
    let open = summary_of(cli, client)?;
    decline_precheck(open.as_ref().map(|(_, summary)| summary), rollout_id)
        .map_err(AppError::Message)?;
    let (rollout, _) = open.expect("the precheck passed, so a rollout is open");
    let schema = sql_ident(&cli.schema)?;
    let updated = client.execute(
        &format!(
            "UPDATE {schema}.rollouts SET status = 'declined', error = $2 \
             WHERE id::STRING = $1 AND status = 'proposed'"
        ),
        &[&rollout.id, &reason],
    )?;
    if updated != 1 {
        return Err(AppError::Message(
            "the proposal changed while declining; reload status".to_string(),
        ));
    }
    let target = &rollout.manifest.target_version;
    events::note(
        cli,
        client,
        actor.source(),
        "rollout_declined",
        &format!(
            "current={} target={target} reason={reason} by={}",
            rollout.manifest.current_version,
            actor.label(cli)
        ),
    )?;
    Ok(format!(
        "declined the upgrade to {target}; it will not be proposed again \
         (undo with: cockroach-rollout-agent undecline {target})"
    ))
}

/// Forgets every decline of `version`, so the leader may propose it again.
pub fn undecline(
    cli: &Cli,
    client: &mut Client,
    actor: &Actor,
    version: &str,
) -> Result<String, AppError> {
    let target = parse_cockroach_version(version)?;
    let schema = sql_ident(&cli.schema)?;
    let updated = client.execute(
        &format!(
            "UPDATE {schema}.rollouts SET status = 'cancelled' \
             WHERE status = 'declined' AND target_version = $1"
        ),
        &[&target.to_string()],
    )?;
    if updated == 0 {
        return Ok(format!("{target} was not declined"));
    }
    events::note(
        cli,
        client,
        actor.source(),
        "rollout_undeclined",
        &format!("target={target} by={}", actor.label(cli)),
    )?;
    Ok(format!("{target} may be proposed again"))
}

/// Targets an operator declined; the leader never proposes these.
pub fn declined_targets(cli: &Cli, client: &mut Client) -> Result<Vec<Version>, AppError> {
    let schema = sql_ident(&cli.schema)?;
    client
        .query(
            &format!(
                "SELECT DISTINCT target_version FROM {schema}.rollouts WHERE status = 'declined'"
            ),
            &[],
        )?
        .iter()
        .map(|row| parse_cockroach_version(row.get(0)))
        .collect()
}

/// Asks the leader to fetch the release list on its next tick.
pub fn check_releases(cli: &Cli, client: &mut Client, actor: &Actor) -> Result<String, AppError> {
    releases::request_refresh(cli, client)?;
    events::note(
        cli,
        client,
        actor.source(),
        "releases_check_requested",
        &format!("by={}", actor.label(cli)),
    )?;
    Ok(format!(
        "the leader will check for new releases within {} minutes \
         (at most once every {} minutes)",
        crate::DEFAULT_DAEMON_INTERVAL_SECONDS / 60,
        releases::CATALOG_MIN_REFRESH_SECS / 60
    ))
}

/// Approves or declines an agent release. Approval names the version, so it
/// cannot land on a newer proposal than the one whose digests were reviewed.
pub fn decide_agent_release(
    cli: &Cli,
    client: &mut Client,
    actor: &Actor,
    version: &str,
    approve: bool,
) -> Result<String, AppError> {
    let by = actor.label(cli);
    agentupdate::decide(cli, client, version, approve, &by)?;
    let kind = if approve {
        "agent_release_approved"
    } else {
        "agent_release_declined"
    };
    events::note(
        cli,
        client,
        actor.source(),
        kind,
        &format!("version={version} by={by}"),
    )?;
    Ok(if approve {
        format!(
            "approved agent {version}; agents update one at a time, each on its next tick \
             (about {} minutes apart)",
            crate::DEFAULT_DAEMON_INTERVAL_SECONDS / 60
        )
    } else {
        format!("declined agent {version}; agents that have not updated stay where they are")
    })
}

/// Lets agents whose update failed try again, and drops claims of agents
/// that have stopped reporting.
pub fn retry_agent_update(
    cli: &Cli,
    client: &mut Client,
    actor: &Actor,
) -> Result<String, AppError> {
    let stale = crate::rollout::stale_after(std::time::Duration::from_secs(
        crate::DEFAULT_DAEMON_INTERVAL_SECONDS,
    ));
    let cleared = agentupdate::retry(cli, client, stale.as_secs())?;
    events::note(
        cli,
        client,
        actor.source(),
        "agent_update_retry",
        &format!("cleared={cleared} by={}", actor.label(cli)),
    )?;
    Ok(format!(
        "cleared {cleared} failed or abandoned agent update claim(s)"
    ))
}

/// Approves or rejects the staged UI certificate the operator looked at.
pub fn decide_ui_cert(
    cli: &Cli,
    client: &mut Client,
    actor: &Actor,
    fingerprint: &str,
    approve: bool,
) -> Result<String, AppError> {
    let by = actor.label(cli);
    uicert::decide(cli, client, fingerprint, approve, &by)?;
    let kind = if approve {
        "ui_cert_approved"
    } else {
        "ui_cert_rejected"
    };
    events::note(
        cli,
        client,
        actor.source(),
        kind,
        &format!("fingerprint={fingerprint} by={by}"),
    )?;
    Ok(if approve {
        format!(
            "approved certificate {fingerprint}; every agent installs it within one tick \
             (about {} minutes)",
            crate::DEFAULT_DAEMON_INTERVAL_SECONDS / 60
        )
    } else {
        format!("rejected certificate {fingerprint}")
    })
}

/// Stages an uploaded certificate after checking it against the UI CA.
pub fn upload_ui_cert(
    cli: &Cli,
    client: &mut Client,
    actor: &Actor,
    pem: &uicert::Pem,
) -> Result<String, AppError> {
    let chain = crate::ui_ca_for_staging(cli, client)?;
    let by = actor.label(cli);
    let info = uicert::stage(cli, client, pem, &chain, &by, unix_time()? as i64)?;
    events::note(
        cli,
        client,
        actor.source(),
        "ui_cert_uploaded",
        &format!("fingerprint={} by={by}", info.fingerprint),
    )?;
    Ok(format!(
        "staged certificate {}; review it below, then approve or reject it",
        info.fingerprint
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
        settings_report: None,
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
                AvailableAction::Decline { .. } => "decline",
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
        assert_eq!(
            names(&available_actions(&with_open(proposed))),
            ["approve", "decline", "restart_blocked"],
            "a proposal can be declined, and says why it blocks a restart"
        );

        let midway = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Active,
            "25.3.0",
            "25.4.17",
        );
        let mut view = with_open(midway.clone());
        view.nodes[0].version = Some(v("25.3.0"));
        assert_eq!(
            names(&available_actions(&view)),
            ["cancel", "restart_blocked"]
        );

        assert_eq!(
            names(&available_actions(&with_open(midway))),
            ["cancel", "finalize", "restart_blocked"],
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
            ["cancel", "restart_blocked"],
            "a patch rollout finalizes itself"
        );

        let failed = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Failed,
            "25.3.0",
            "25.4.17",
        );
        assert_eq!(
            names(&available_actions(&with_open(failed))),
            ["cancel", "restart_blocked"]
        );

        let restarting = OpenSummary::sample(
            RolloutKind::Restart,
            RolloutStatus::Active,
            "25.4.17",
            "25.4.17",
        );
        assert_eq!(
            names(&available_actions(&with_open(restarting))),
            ["cancel", "restart_blocked"]
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
        assert_eq!(
            names(&available_actions(&with_open(offered))),
            ["approve", "decline", "restart_blocked"]
        );
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

    #[test]
    fn decline_needs_a_proposed_rollout_with_the_same_id() {
        let proposed = OpenSummary::sample(
            RolloutKind::Upgrade,
            RolloutStatus::Proposed,
            "25.4.17",
            "26.1.8",
        );
        assert!(decline_precheck(Some(&proposed), Some(&proposed.id)).is_ok());
        assert!(
            decline_precheck(Some(&proposed), None).is_ok(),
            "the CLI declines whatever is proposed"
        );
        let other = decline_precheck(Some(&proposed), Some("someone-else")).unwrap_err();
        assert!(other.contains("changed"), "{other}");
        assert!(decline_precheck(None, None).is_err());
        let active = OpenSummary {
            status: RolloutStatus::Active,
            ..proposed
        };
        let refused = decline_precheck(Some(&active), Some(&active.id)).unwrap_err();
        assert!(refused.contains("cancel"), "{refused}");
    }

    #[test]
    fn approve_requires_ack_for_settings_warnings() {
        let proposed = OpenSummary {
            settings_warnings: vec!["kv.x default changes from 1 to 2".to_string()],
            ..OpenSummary::sample(
                RolloutKind::Upgrade,
                RolloutStatus::Proposed,
                "25.4.17",
                "26.1.8",
            )
        };
        let refused = approve_precheck(Some(&proposed), &v("26.1.8"), false).unwrap_err();
        assert!(refused.contains("kv.x"), "{refused}");
        assert!(approve_precheck(Some(&proposed), &v("26.1.8"), true).is_ok());
    }
}
