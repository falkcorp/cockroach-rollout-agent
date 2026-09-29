// file: src/main.rs
// version: 3.0.0
// guid: d16be11a-b10c-4d2e-853f-d4a1c0a3c617
// last-edited: 2026-09-29

mod health;
mod layout;
mod rollout;

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use native_tls::{Certificate, Identity, TlsConnector};
use postgres::Client;
use postgres_native_tls::MakeTlsConnector;
use regex::Regex;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::health::{ClusterHealth, safe_to_restart};
use crate::layout::{BinaryLayout, probe_writable};
use crate::rollout::{
    FollowerAction, LeaderAction, NodeObservation, RolloutStatus, all_nodes_on, follower_action,
    leader_action, stale_after,
};

const DEFAULT_BASE_URL: &str = "https://binaries.cockroachdb.com";
const DEFAULT_GITHUB_API_URL: &str =
    "https://api.github.com/repos/cockroachdb/cockroach/tags?per_page=100";
const DEFAULT_RELEASE_NOTES_BASE_URL: &str = "https://www.cockroachlabs.com/docs/releases";
const DEFAULT_ARTIFACTS_DIR: &str = "dist";
const DEFAULT_BINARY_PATH: &str = "/usr/local/bin/cockroach";
const DEFAULT_AUDIT_LOG: &str = "/var/log/cockroach-rollout-agent/audit.log";
const DEFAULT_MANIFEST_PATH: &str = "dist/manifest.json";
const DEFAULT_DAEMON_INTERVAL_SECONDS: u64 = 300;
const DEFAULT_LEASE_SECONDS: u64 = 90;
const DEFAULT_AGENT_ROOT: &str = "/var/lib/cockroach-rollout-agent";
const DEFAULT_RESTART_TIMEOUT_SECONDS: u64 = 600;
const LEADER_LEASE: &str = "leader";
const INSTALL_LEASE: &str = "install";
/// The install lease outlives a full download + restart + health wait, so a
/// crashed holder blocks the next node for a bounded time instead of forever.
const INSTALL_LEASE_EXTRA_SECONDS: u64 = 900;
const HEALTH_POLL_SECONDS: u64 = 5;
const POLKIT_MANAGE_UNITS: &str = "org.freedesktop.systemd1.manage-units";
const DEFAULT_SCHEMA: &str = "cockroach_rollout";
const SUPPORTED_ARCHES: &[&str] = &["amd64", "arm64"];
const BREAKING_CHANGE_PATTERNS: &[&str] = &[
    "backward incompatible",
    "backwards incompatible",
    "breaking change",
    "breaking changes",
    "incompatible change",
    "manual upgrade",
    "manual action",
    "cannot downgrade",
    "deprecat",
    "removed",
    "no longer supported",
    "requires",
    "migration",
];

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[arg(long, env = "CROACH_ROLLOUT_BASE_URL", default_value = DEFAULT_BASE_URL)]
    base_url: String,

    #[arg(
        long,
        env = "CROACH_ROLLOUT_GITHUB_API_URL",
        default_value = DEFAULT_GITHUB_API_URL
    )]
    github_api_url: String,

    #[arg(
        long,
        env = "CROACH_ROLLOUT_RELEASE_NOTES_BASE_URL",
        default_value = DEFAULT_RELEASE_NOTES_BASE_URL
    )]
    release_notes_base_url: String,

    #[arg(
        long,
        env = "CROACH_ROLLOUT_ARTIFACTS_DIR",
        default_value = DEFAULT_ARTIFACTS_DIR
    )]
    artifacts_dir: PathBuf,

    /// systemd unit that runs CockroachDB, for example `cockroach.service`.
    /// Deliberately has no default: the name differs between hosts, and a
    /// wrong guess fails only once a rollout is already underway.
    #[arg(long, env = "CROACH_ROLLOUT_SERVICE")]
    service_name: Option<String>,

    /// Agent-owned directory holding `bin/cockroach` and `versions/`.
    /// `--binary-path` must be a symlink to `<agent-root>/bin/cockroach`.
    #[arg(long, env = "CROACH_ROLLOUT_AGENT_ROOT", default_value = DEFAULT_AGENT_ROOT)]
    agent_root: PathBuf,

    /// How long a restarted node may take to rejoin on the new version
    /// before the agent rolls it back.
    #[arg(
        long,
        env = "CROACH_ROLLOUT_RESTART_TIMEOUT_SECONDS",
        default_value_t = DEFAULT_RESTART_TIMEOUT_SECONDS
    )]
    restart_timeout_seconds: u64,

    #[arg(
        long,
        env = "CROACH_ROLLOUT_BINARY_PATH",
        default_value = DEFAULT_BINARY_PATH
    )]
    binary_path: PathBuf,

    #[arg(long, env = "CROACH_ROLLOUT_AUDIT_LOG", default_value = DEFAULT_AUDIT_LOG)]
    audit_log: PathBuf,

    #[arg(long, env = "CROACH_ROLLOUT_DATABASE_URL")]
    database_url: Option<String>,

    /// PEM CA bundle used to verify the CockroachDB server certificate.
    /// Required for clusters that use a private CA, which is the normal
    /// CockroachDB deployment. The `postgres` connection string parser rejects
    /// libpq's `sslrootcert`, so the path is supplied here instead.
    #[arg(long, env = "CROACH_ROLLOUT_SSL_ROOT_CERT")]
    ssl_root_cert: Option<PathBuf>,

    /// PEM client certificate presented for CockroachDB certificate
    /// authentication. Must be set together with `--ssl-client-key`.
    #[arg(long, env = "CROACH_ROLLOUT_SSL_CLIENT_CERT")]
    ssl_client_cert: Option<PathBuf>,

    /// PEM private key matching `--ssl-client-cert`.
    #[arg(long, env = "CROACH_ROLLOUT_SSL_CLIENT_KEY")]
    ssl_client_key: Option<PathBuf>,

    #[arg(long, env = "CROACH_ROLLOUT_SCHEMA", default_value = DEFAULT_SCHEMA)]
    schema: String,

    #[arg(long, env = "CROACH_ROLLOUT_NODE_ID")]
    node_id: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Download artifacts, scan notes, and write a rollout manifest.
    Prepare {
        /// Current CockroachDB version. Defaults to running the configured binary.
        #[arg(long, env = "CROACH_ROLLOUT_CURRENT_VERSION")]
        current_version: Option<String>,

        /// Target version. Defaults to the latest GitHub release.
        #[arg(long, env = "CROACH_ROLLOUT_TARGET_VERSION")]
        target_version: Option<String>,

        /// Permit release-note warning matches without failing.
        #[arg(long)]
        allow_breaking_warnings: bool,
    },

    /// Print a JSON upgrade plan without downloading artifacts.
    Plan {
        /// Current CockroachDB version. Defaults to running the configured binary.
        #[arg(long, env = "CROACH_ROLLOUT_CURRENT_VERSION")]
        current_version: Option<String>,

        /// Target version. Defaults to the latest GitHub release.
        #[arg(long, env = "CROACH_ROLLOUT_TARGET_VERSION")]
        target_version: Option<String>,
    },

    /// Install an artifact after validating it against a manifest.
    Install {
        #[arg(long, default_value = DEFAULT_MANIFEST_PATH)]
        manifest: PathBuf,

        #[arg(long)]
        arch: Option<String>,

        #[arg(long)]
        dry_run: bool,
    },

    /// Poll a manifest URL or file and apply safe updates.
    Daemon {
        #[arg(long, env = "CROACH_ROLLOUT_MANIFEST_URL")]
        manifest_url: Option<String>,

        #[arg(long, env = "CROACH_ROLLOUT_MANIFEST_FILE")]
        manifest_file: Option<PathBuf>,

        #[arg(long, default_value_t = DEFAULT_DAEMON_INTERVAL_SECONDS)]
        interval_seconds: u64,

        /// Report what would happen without writing coordination state,
        /// taking leases, or touching the binary.
        #[arg(long)]
        dry_run: bool,

        /// Finalize major-line upgrades after every node runs the target binary.
        #[arg(long, env = "CROACH_ROLLOUT_AUTO_FINALIZE")]
        auto_finalize: bool,
    },

    /// Approve the proposed rollout so agents start upgrading, one node at a time.
    Approve {
        /// Target version of the proposal, as shown by `status`. Must match,
        /// so an approval cannot land on a different proposal than the one reviewed.
        version: String,

        /// Required when the release notes matched a warning pattern.
        #[arg(long)]
        accept_release_note_warnings: bool,
    },

    /// Cancel the open rollout. Nodes already upgraded stay upgraded.
    Cancel {
        #[arg(long, default_value = "cancelled by operator")]
        reason: String,
    },

    /// Show rollouts, cluster nodes, agents, and lease holders.
    Status,

    /// Initialize the CockroachDB SQL coordination schema.
    InitDb,

    /// Print discovered CockroachDB nodes from crdb_internal.gossip_nodes.
    Discover,

    /// Finalize a major-version upgrade after every node is on the new binary.
    Finalize {
        /// Target CockroachDB version. Uses the major line, for example v25.4.3 finalizes 25.4.
        #[arg(long, env = "CROACH_ROLLOUT_TARGET_VERSION")]
        target_version: String,

        /// Print the SQL without executing it.
        #[arg(long)]
        dry_run: bool,
    },

    /// Validate the binary layout, restart authorization, and SQL access.
    SelfCheck,
}

#[derive(Debug, Error)]
enum AppError {
    #[error("{0}")]
    Message(String),

    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Semver(#[from] semver::Error),

    #[error(transparent)]
    Regex(#[from] regex::Error),

    #[error(transparent)]
    Postgres(#[from] postgres::Error),
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    #[serde(alias = "name")]
    tag_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct UpgradePlan {
    current_version: Version,
    requested_target_version: Version,
    next_version: Version,
    latest_version: Version,
    release_notes_url: String,
    release_note_warnings: Vec<String>,
    upgrade_steps: Vec<UpgradeStep>,
    release_line_by_release_line: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpgradeStep {
    from_version: Version,
    to_version: Version,
    release_line: String,
    release_notes_url: String,
    requires_finalization: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct RolloutManifest {
    schema_version: u32,
    created_unix: u64,
    current_version: Version,
    target_version: Version,
    release_notes_url: String,
    release_note_warnings: Vec<String>,
    release_note_warnings_approved: bool,
    artifacts: Vec<Artifact>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Artifact {
    os: String,
    arch: String,
    url: String,
    path: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct DiscoveredNode {
    node_id: i64,
    address: String,
    sql_address: Option<String>,
    is_live: Option<bool>,
}

#[derive(Debug)]
struct LeaseResult {
    is_leader: bool,
    holder_id: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), AppError> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Prepare {
            current_version,
            target_version,
            allow_breaking_warnings,
        } => prepare_command(
            &cli,
            current_version.as_deref(),
            target_version.as_deref(),
            *allow_breaking_warnings,
        ),
        Commands::Plan {
            current_version,
            target_version,
        } => {
            let plan =
                build_upgrade_plan(&cli, current_version.as_deref(), target_version.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&plan)?);
            Ok(())
        }
        Commands::Install {
            manifest,
            arch,
            dry_run,
        } => install_command(&cli, manifest, arch.as_deref(), *dry_run),
        Commands::Daemon {
            manifest_url,
            manifest_file,
            interval_seconds,
            dry_run,
            auto_finalize,
        } => daemon_command(
            &cli,
            manifest_url.as_deref(),
            manifest_file.as_deref(),
            *interval_seconds,
            *dry_run,
            *auto_finalize,
        ),
        Commands::Approve {
            version,
            accept_release_note_warnings,
        } => approve_command(&cli, version, *accept_release_note_warnings),
        Commands::Cancel { reason } => cancel_command(&cli, reason),
        Commands::Status => status_command(&cli),
        Commands::InitDb => init_db_command(&cli),
        Commands::Discover => discover_command(&cli),
        Commands::Finalize {
            target_version,
            dry_run,
        } => finalize_command(&cli, target_version, *dry_run),
        Commands::SelfCheck => self_check_command(&cli),
    }
}

fn prepare_command(
    cli: &Cli,
    current_version: Option<&str>,
    target_version: Option<&str>,
    allow_breaking_warnings: bool,
) -> Result<(), AppError> {
    let plan = build_upgrade_plan(cli, current_version, target_version)?;
    if !plan.release_note_warnings.is_empty() && !allow_breaking_warnings {
        audit(
            cli,
            "prepare_blocked_release_notes",
            &format!(
                "target={} warning_count={}",
                plan.next_version,
                plan.release_note_warnings.len()
            ),
        )?;
        return Err(AppError::Message(format!(
            "release notes contain warning patterns; rerun with --allow-breaking-warnings after review: {}",
            plan.release_notes_url
        )));
    }

    fs::create_dir_all(&cli.artifacts_dir)?;
    let mut artifacts = Vec::new();
    for arch in SUPPORTED_ARCHES {
        let url = cockroach_url(&cli.base_url, &plan.next_version, arch);
        let destination = cli.artifacts_dir.join(format!(
            "cockroach-{}.linux-{}.tgz",
            plan.next_version, arch
        ));
        audit(
            cli,
            "download_start",
            &format!(
                "arch={arch} url={url} destination={}",
                destination.display()
            ),
        )?;
        download_to_file(&url, &destination)?;
        let (sha256, bytes) = sha256_file(&destination)?;
        artifacts.push(Artifact {
            os: "linux".to_string(),
            arch: (*arch).to_string(),
            url,
            path: destination.to_string_lossy().into_owned(),
            sha256,
            bytes,
        });
        audit(
            cli,
            "download_complete",
            &format!("arch={arch} destination={}", destination.display()),
        )?;
    }

    let manifest = RolloutManifest {
        schema_version: 1,
        created_unix: unix_time()?,
        current_version: plan.current_version,
        target_version: plan.next_version,
        release_notes_url: plan.release_notes_url,
        release_note_warnings: plan.release_note_warnings,
        release_note_warnings_approved: allow_breaking_warnings,
        artifacts,
    };

    let manifest_path = cli.artifacts_dir.join("manifest.json");
    write_json_file(&manifest_path, &manifest)?;
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    audit(
        cli,
        "manifest_written",
        &format!("manifest={}", manifest_path.display()),
    )?;
    Ok(())
}

fn build_upgrade_plan(
    cli: &Cli,
    current_version: Option<&str>,
    target_version: Option<&str>,
) -> Result<UpgradePlan, AppError> {
    next_upgrade_plan(cli, current_version, target_version)?
        .ok_or_else(|| AppError::Message("no upgrade step is required".to_string()))
}

/// Like `build_upgrade_plan`, but `None` when already on the target.
fn next_upgrade_plan(
    cli: &Cli,
    current_version: Option<&str>,
    target_version: Option<&str>,
) -> Result<Option<UpgradePlan>, AppError> {
    let current = match current_version {
        Some(version) => parse_cockroach_version(version)?,
        None => installed_cockroach_version(&cli.binary_path)?,
    };
    let available_versions = available_cockroach_versions(&cli.github_api_url)?;
    let latest = available_versions
        .iter()
        .max()
        .cloned()
        .ok_or_else(|| AppError::Message("no CockroachDB releases discovered".to_string()))?;
    let target = match target_version {
        Some(version) => parse_cockroach_version(version)?,
        None => latest.clone(),
    };

    let upgrade_steps = build_upgrade_steps(
        &current,
        &target,
        &available_versions,
        &cli.release_notes_base_url,
    )?;
    let Some(next_step) = upgrade_steps.first().cloned() else {
        return Ok(None);
    };

    let release_notes = fetch_text(&next_step.release_notes_url)?;
    let warnings = scan_release_notes(&release_notes)?;

    Ok(Some(UpgradePlan {
        current_version: current,
        requested_target_version: target,
        next_version: next_step.to_version.clone(),
        latest_version: latest,
        release_notes_url: next_step.release_notes_url.clone(),
        release_note_warnings: warnings,
        upgrade_steps,
        release_line_by_release_line: true,
    }))
}

fn install_command(
    cli: &Cli,
    manifest_path: &Path,
    arch: Option<&str>,
    dry_run: bool,
) -> Result<(), AppError> {
    let manifest: RolloutManifest = read_json_file(manifest_path)?;
    let current = installed_cockroach_version(&cli.binary_path)?;
    validate_manifest_current_version(&current, &manifest)?;

    if !manifest.release_note_warnings.is_empty() && !manifest.release_note_warnings_approved {
        return Err(AppError::Message(
            "manifest contains release-note warnings; generate an approved manifest after review"
                .to_string(),
        ));
    }

    let local_arch = arch.map(str::to_string).unwrap_or_else(normalized_arch);
    let artifact = manifest
        .artifacts
        .iter()
        .find(|candidate| candidate.os == "linux" && candidate.arch == local_arch)
        .ok_or_else(|| AppError::Message(format!("manifest has no linux/{local_arch} artifact")))?;
    let artifact_path = Path::new(&artifact.path);
    verify_artifact(artifact_path, artifact)?;

    audit(
        cli,
        "install_validated",
        &format!(
            "target={} arch={} artifact={}",
            manifest.target_version,
            local_arch,
            artifact_path.display()
        ),
    )?;

    if dry_run {
        println!(
            "dry run: would install {} from {}",
            manifest.target_version,
            artifact_path.display()
        );
        return Ok(());
    }

    upgrade_node(cli, &manifest, artifact_path, &UnitHealthWaiter { cli })
}

fn daemon_command(
    cli: &Cli,
    manifest_url: Option<&str>,
    manifest_file: Option<&Path>,
    interval_seconds: u64,
    dry_run: bool,
    auto_finalize: bool,
) -> Result<(), AppError> {
    if cli.database_url.is_some() {
        return db_daemon_command(cli, interval_seconds, dry_run, auto_finalize);
    }

    if manifest_url.is_none() && manifest_file.is_none() {
        return Err(AppError::Message(
            "daemon requires --manifest-url, --manifest-file, or --database-url".to_string(),
        ));
    }
    audit(cli, "daemon_start", "polling manifest source")?;
    loop {
        let result = poll_and_install_manifest(cli, manifest_url, manifest_file, dry_run);
        if let Err(error) = result {
            audit(cli, "daemon_poll_failed", &error.to_string())?;
            eprintln!("daemon poll failed: {error}");
        }
        thread::sleep(Duration::from_secs(interval_seconds));
    }
}

fn poll_and_install_manifest(
    cli: &Cli,
    manifest_url: Option<&str>,
    manifest_file: Option<&Path>,
    dry_run: bool,
) -> Result<(), AppError> {
    let manifest_path = tempfile::Builder::new()
        .prefix("cockroach-rollout-manifest-")
        .suffix(".json")
        .tempfile()?;

    if let Some(url) = manifest_url {
        download_to_file(url, manifest_path.path())?;
    } else if let Some(path) = manifest_file {
        fs::copy(path, manifest_path.path())?;
    }

    install_command(cli, manifest_path.path(), None, dry_run)
}

fn init_db_command(cli: &Cli) -> Result<(), AppError> {
    let mut client = db_client(cli)?;
    ensure_schema(cli, &mut client)?;
    println!("initialized SQL coordination schema {}", cli.schema);
    Ok(())
}

fn discover_command(cli: &Cli) -> Result<(), AppError> {
    let mut client = db_client(cli)?;
    let nodes = discover_nodes(&mut client)?;
    println!("{}", serde_json::to_string_pretty(&nodes)?);
    Ok(())
}

fn db_daemon_command(
    cli: &Cli,
    interval_seconds: u64,
    dry_run: bool,
    auto_finalize: bool,
) -> Result<(), AppError> {
    let agent_id = agent_node_id(cli)?;
    audit(
        cli,
        "db_daemon_start",
        &format!(
            "agent_id={agent_id} schema={} dry_run={dry_run}",
            cli.schema
        ),
    )?;

    let mut schema_ready = false;
    loop {
        let result = (|| -> Result<(), AppError> {
            let mut client = db_client(cli)?;
            if !schema_ready && !dry_run {
                ensure_schema(cli, &mut client)?;
                schema_ready = true;
            }
            db_daemon_tick(cli, &mut client, &agent_id, dry_run, auto_finalize)
        })();
        if let Err(error) = result {
            audit(cli, "db_daemon_tick_failed", &error.to_string())?;
            eprintln!("db daemon tick failed: {error}");
        }
        thread::sleep(Duration::from_secs(interval_seconds));
    }
}

fn db_daemon_tick(
    cli: &Cli,
    client: &mut Client,
    agent_id: &str,
    dry_run: bool,
    auto_finalize: bool,
) -> Result<(), AppError> {
    let installed = installed_cockroach_version(&cli.binary_path)?;

    if dry_run {
        let holder = lease_holder(cli, client, LEADER_LEASE)?;
        println!(
            "dry run: agent={agent_id} installed={installed} leader={}",
            holder.as_deref().unwrap_or("none")
        );
    } else {
        heartbeat_agent(cli, client, agent_id, &installed)?;
        let lease = acquire_lease(cli, client, LEADER_LEASE, agent_id, DEFAULT_LEASE_SECONDS)?;
        if !lease.is_leader {
            audit(
                cli,
                "leader_observed",
                &format!("holder={}", lease.holder_id),
            )?;
            return follower_reconcile(cli, client, agent_id, false);
        }
    }

    leader_reconcile(cli, client, auto_finalize, dry_run)?;
    follower_reconcile(cli, client, agent_id, dry_run)
}

fn leader_reconcile(
    cli: &Cli,
    client: &mut Client,
    auto_finalize: bool,
    dry_run: bool,
) -> Result<(), AppError> {
    let open = open_rollout(cli, client)?;
    let nodes = cluster_nodes(client)?;
    let complete = open
        .as_ref()
        .is_some_and(|rollout| all_nodes_on(&rollout.manifest.target_version, &nodes));
    let action = leader_action(
        open.as_ref()
            .map(|rollout| (rollout.status, rollout.requires_finalization())),
        complete,
        auto_finalize,
    );

    if dry_run {
        println!("dry run: leader action {action:?}");
        return Ok(());
    }

    match action {
        LeaderAction::Propose => propose_next_rollout(cli, client, &nodes),
        LeaderAction::Wait => Ok(()),
        LeaderAction::MarkFinalized => {
            let rollout = open.expect("MarkFinalized implies an open rollout");
            set_rollout_status(cli, client, &rollout.id, RolloutStatus::Finalized, None)?;
            audit(
                cli,
                "rollout_finalized",
                &format!("target={}", rollout.manifest.target_version),
            )
        }
        LeaderAction::Finalize => {
            let rollout = open.expect("Finalize implies an open rollout");
            finalize_cluster(cli, client, &rollout.manifest.target_version)?;
            set_rollout_status(cli, client, &rollout.id, RolloutStatus::Finalized, None)?;
            Ok(())
        }
        LeaderAction::AwaitOperatorFinalize => {
            let rollout = open.expect("AwaitOperatorFinalize implies an open rollout");
            audit(
                cli,
                "rollout_waiting_for_finalization",
                &format!(
                    "target={}; run: cockroach-rollout-agent finalize --target-version {}",
                    rollout.manifest.target_version, rollout.manifest.target_version
                ),
            )
        }
    }
}

/// Records the next release-line step as a `proposed` rollout. Nothing
/// installs until an operator runs `approve`.
fn propose_next_rollout(
    cli: &Cli,
    client: &mut Client,
    nodes: &[NodeObservation],
) -> Result<(), AppError> {
    let Some(current) = uniform_cluster_version(nodes) else {
        return audit(
            cli,
            "proposal_skipped",
            "cluster nodes are not all live on one version",
        );
    };
    let Some(plan) = next_upgrade_plan(cli, Some(&current.to_string()), None)? else {
        return Ok(());
    };
    let manifest = create_manifest_for_plan(cli, plan)?;
    publish_rollout(cli, client, &manifest)
}

fn uniform_cluster_version(nodes: &[NodeObservation]) -> Option<Version> {
    let first = nodes.first()?.version.clone()?;
    all_nodes_on(&first, nodes).then_some(first)
}

fn follower_reconcile(
    cli: &Cli,
    client: &mut Client,
    agent_id: &str,
    dry_run: bool,
) -> Result<(), AppError> {
    let Some(rollout) =
        open_rollout(cli, client)?.filter(|rollout| rollout.status == RolloutStatus::Active)
    else {
        if !dry_run {
            release_lease(cli, client, INSTALL_LEASE, agent_id)?;
        }
        return Ok(());
    };
    let manifest = &rollout.manifest;
    let running = own_node_version(client)?;

    match follower_action(
        &running,
        &manifest.current_version,
        &manifest.target_version,
    ) {
        FollowerAction::AlreadyOnTarget => {
            if !dry_run {
                record_agent_state(
                    cli,
                    client,
                    agent_id,
                    "complete",
                    &running,
                    &rollout.id,
                    None,
                )?;
                release_lease(cli, client, INSTALL_LEASE, agent_id)?;
            }
            return Ok(());
        }
        FollowerAction::UnexpectedVersion => {
            let detail = format!(
                "node runs {running}; rollout upgrades {} -> {}",
                manifest.current_version, manifest.target_version
            );
            if dry_run {
                println!("dry run: would wait: {detail}");
            } else {
                record_agent_state(
                    cli,
                    client,
                    agent_id,
                    "waiting",
                    &running,
                    &rollout.id,
                    Some(&detail),
                )?;
            }
            return Ok(());
        }
        FollowerAction::Upgrade => {}
    }

    if dry_run {
        println!(
            "dry run: would take the install lease and upgrade {} -> {}",
            manifest.current_version, manifest.target_version
        );
        return Ok(());
    }

    let lease_seconds = cli.restart_timeout_seconds + INSTALL_LEASE_EXTRA_SECONDS;
    let lease = acquire_lease(cli, client, INSTALL_LEASE, agent_id, lease_seconds)?;
    if !lease.is_leader {
        return record_agent_state(
            cli,
            client,
            agent_id,
            "queued",
            &running,
            &rollout.id,
            Some(&format!("install lease held by {}", lease.holder_id)),
        );
    }

    // Fail closed: if health cannot be measured, it is not known to be safe.
    let verdict = cluster_health(client)
        .map_err(|error| format!("could not measure cluster health: {error}"))
        .and_then(|health| safe_to_restart(&health));
    if let Err(reason) = verdict {
        record_agent_state(
            cli,
            client,
            agent_id,
            "waiting_for_health",
            &running,
            &rollout.id,
            Some(&reason),
        )?;
        return release_lease(cli, client, INSTALL_LEASE, agent_id);
    }

    record_agent_state(
        cli,
        client,
        agent_id,
        "installing",
        &running,
        &rollout.id,
        None,
    )?;
    let artifact_path = fetch_artifact_for_host(cli, manifest)?;
    let outcome = upgrade_node(cli, manifest, &artifact_path, &DbHealthWaiter { cli });

    // The node restarted, so the old connection is gone.
    *client = reconnect(cli)?;
    let running = own_node_version(client)?;
    match outcome {
        Ok(()) => {
            record_agent_state(
                cli,
                client,
                agent_id,
                "complete",
                &running,
                &rollout.id,
                None,
            )?;
            release_lease(cli, client, INSTALL_LEASE, agent_id)
        }
        Err(error) => {
            let message = error.to_string();
            set_rollout_status(
                cli,
                client,
                &rollout.id,
                RolloutStatus::Failed,
                Some(&format!("{agent_id}: {message}")),
            )?;
            record_agent_state(
                cli,
                client,
                agent_id,
                "failed",
                &running,
                &rollout.id,
                Some(&message),
            )?;
            release_lease(cli, client, INSTALL_LEASE, agent_id)?;
            Err(error)
        }
    }
}

/// Downloads (if needed) and verifies this host's artifact into the local
/// artifacts dir. The manifest's `path` is the leader's local path, so it is
/// never used here.
fn fetch_artifact_for_host(cli: &Cli, manifest: &RolloutManifest) -> Result<PathBuf, AppError> {
    let arch = normalized_arch();
    let artifact = manifest
        .artifacts
        .iter()
        .find(|candidate| candidate.os == "linux" && candidate.arch == arch)
        .ok_or_else(|| AppError::Message(format!("rollout has no linux/{arch} artifact")))?;
    fs::create_dir_all(&cli.artifacts_dir)?;
    let path = cli.artifacts_dir.join(format!(
        "cockroach-{}.linux-{}.tgz",
        manifest.target_version, arch
    ));
    if verify_artifact(&path, artifact).is_err() {
        download_to_file(&artifact.url, &path)?;
        verify_artifact(&path, artifact)?;
    }
    Ok(path)
}

/// Confirms a restarted node came back on the expected build.
trait HealthWaiter {
    fn wait_for(&self, expected: &Version, timeout: Duration) -> Result<(), AppError>;
}

/// Waits until this agent's own node is live in gossip on `expected`.
struct DbHealthWaiter<'a> {
    cli: &'a Cli,
}

impl HealthWaiter for DbHealthWaiter<'_> {
    fn wait_for(&self, expected: &Version, timeout: Duration) -> Result<(), AppError> {
        let deadline = SystemTime::now() + timeout;
        let mut last = String::from("no observation yet");
        while SystemTime::now() < deadline {
            thread::sleep(Duration::from_secs(HEALTH_POLL_SECONDS));
            match db_client(self.cli).and_then(|mut client| own_node_observation(&mut client)) {
                Ok(node) if node.is_live && node.version.as_ref() == Some(expected) => {
                    return Ok(());
                }
                Ok(node) => {
                    last = format!(
                        "node {} live={} version={}",
                        node.node_id,
                        node.is_live,
                        node.version
                            .map(|version| version.to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                    );
                }
                Err(error) => last = error.to_string(),
            }
        }
        Err(AppError::Message(format!(
            "node did not come back on {expected} within {}s; last: {last}",
            timeout.as_secs()
        )))
    }
}

/// Waits for the unit to be active and the binary to report `expected`.
/// Used by the single-host manifest mode, which has no SQL connection.
struct UnitHealthWaiter<'a> {
    cli: &'a Cli,
}

impl HealthWaiter for UnitHealthWaiter<'_> {
    fn wait_for(&self, expected: &Version, timeout: Duration) -> Result<(), AppError> {
        let service = service_name(self.cli)?;
        let deadline = SystemTime::now() + timeout;
        while SystemTime::now() < deadline {
            thread::sleep(Duration::from_secs(HEALTH_POLL_SECONDS));
            let active = command_status("systemctl", ["is-active", "--quiet", service])?;
            if active && installed_cockroach_version(&self.cli.binary_path)? == *expected {
                return Ok(());
            }
        }
        Err(AppError::Message(format!(
            "{service} was not active on {expected} within {}s",
            timeout.as_secs()
        )))
    }
}

/// Stages the target, swaps it in while the node is still running, restarts,
/// and waits for the node to rejoin. On any failure after the swap it points
/// the binary back at `manifest.current_version` and restarts again, so the
/// node is never left stopped or on a half-installed binary.
fn upgrade_node(
    cli: &Cli,
    manifest: &RolloutManifest,
    artifact: &Path,
    waiter: &dyn HealthWaiter,
) -> Result<(), AppError> {
    let layout = BinaryLayout::new(&cli.agent_root);
    layout
        .verify_wired(&cli.binary_path)
        .map_err(AppError::Message)?;
    let service = service_name(cli)?;
    let from = &manifest.current_version;
    let target = &manifest.target_version;

    // Rollback must be possible before anything changes.
    let rollback_name = BinaryLayout::version_file_name(from);
    if !layout.version_path(from).is_file() {
        return Err(AppError::Message(format!(
            "refusing to upgrade: rollback binary {} is missing",
            layout.version_path(from).display()
        )));
    }

    let work_dir = tempfile::Builder::new()
        .prefix("cockroach-rollout-")
        .tempdir()?;
    run_command(
        "tar",
        [
            "-xzf".as_ref(),
            artifact.as_os_str(),
            "-C".as_ref(),
            work_dir.path().as_os_str(),
        ],
    )?;
    let extracted = find_cockroach_binary(work_dir.path())?;
    let staged = layout.stage(&extracted, target)?;
    let staged_version = installed_cockroach_version(&staged)?;
    if staged_version != *target {
        return Err(AppError::Message(format!(
            "staged binary reports {staged_version}, expected {target}"
        )));
    }

    audit(
        cli,
        "install_swap",
        &format!("from={from} target={target} service={service}"),
    )?;
    layout.activate(target)?;

    let timeout = Duration::from_secs(cli.restart_timeout_seconds);
    let attempt = run_systemctl("restart", service).and_then(|()| waiter.wait_for(target, timeout));
    let Err(error) = attempt else {
        audit(cli, "install_complete", &format!("target={target}"))?;
        if let Err(prune_error) = layout.prune(Some(&rollback_name)) {
            audit(cli, "prune_failed", &prune_error.to_string())?;
        }
        return Ok(());
    };

    audit(
        cli,
        "install_rollback",
        &format!("target={target} error={error}"),
    )?;
    let rollback = layout
        .restore(&rollback_name)
        .map_err(AppError::from)
        .and_then(|()| run_systemctl("restart", service))
        .and_then(|()| waiter.wait_for(from, timeout));
    match rollback {
        Ok(()) => {
            audit(cli, "install_rolled_back", &format!("restored={from}"))?;
            Err(AppError::Message(format!(
                "upgrade to {target} failed and was rolled back to {from}: {error}"
            )))
        }
        Err(rollback_error) => {
            audit(
                cli,
                "install_rollback_failed",
                &format!("error={rollback_error}"),
            )?;
            Err(AppError::Message(format!(
                "upgrade to {target} failed ({error}) and rollback to {from} also failed \
                 ({rollback_error}); node needs attention"
            )))
        }
    }
}

fn reconnect(cli: &Cli) -> Result<Client, AppError> {
    let deadline = SystemTime::now() + Duration::from_secs(cli.restart_timeout_seconds);
    loop {
        match db_client(cli) {
            Ok(client) => return Ok(client),
            Err(error) if SystemTime::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_secs(HEALTH_POLL_SECONDS)),
        }
    }
}

fn approve_command(
    cli: &Cli,
    version: &str,
    accept_release_note_warnings: bool,
) -> Result<(), AppError> {
    let target = parse_cockroach_version(version)?;
    let mut client = db_client(cli)?;
    ensure_schema(cli, &mut client)?;
    let rollout = open_rollout(cli, &mut client)?
        .ok_or_else(|| AppError::Message("there is no proposed rollout".to_string()))?;
    if rollout.status != RolloutStatus::Proposed {
        return Err(AppError::Message(format!(
            "the open rollout to {} is {}, not proposed",
            rollout.manifest.target_version, rollout.status
        )));
    }
    if rollout.manifest.target_version != target {
        return Err(AppError::Message(format!(
            "the proposal targets {}, not {target}",
            rollout.manifest.target_version
        )));
    }
    if !rollout.manifest.release_note_warnings.is_empty() && !accept_release_note_warnings {
        return Err(AppError::Message(format!(
            "release notes matched {:?}; review {} and rerun with --accept-release-note-warnings",
            rollout.manifest.release_note_warnings, rollout.manifest.release_notes_url
        )));
    }

    if rollout.requires_finalization() {
        // Without this, CockroachDB finalizes on its own the moment the last
        // node restarts, and the downgrade window closes before anyone looks.
        let line = major_line_string(&rollout.manifest.current_version);
        client.batch_execute(&format!(
            "SET CLUSTER SETTING cluster.preserve_downgrade_option = '{line}'"
        ))?;
    }

    let mut manifest = rollout.manifest;
    manifest.release_note_warnings_approved = accept_release_note_warnings;
    let schema = sql_ident(&cli.schema)?;
    let approver = std::env::var("USER").unwrap_or_else(|_| agent_node_id(cli).unwrap_or_default());
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
            "the proposal changed while approving; rerun status".to_string(),
        ));
    }
    audit(
        cli,
        "rollout_approved",
        &format!(
            "current={} target={} by={approver}",
            manifest.current_version, manifest.target_version
        ),
    )?;
    println!(
        "approved {} -> {}; agents will upgrade one node at a time",
        manifest.current_version, manifest.target_version
    );
    Ok(())
}

fn cancel_command(cli: &Cli, reason: &str) -> Result<(), AppError> {
    let mut client = db_client(cli)?;
    ensure_schema(cli, &mut client)?;
    let Some(rollout) = open_rollout(cli, &mut client)? else {
        println!("no open rollout");
        return Ok(());
    };
    set_rollout_status(
        cli,
        &mut client,
        &rollout.id,
        RolloutStatus::Cancelled,
        Some(reason),
    )?;
    audit(
        cli,
        "rollout_cancelled",
        &format!("target={} reason={reason}", rollout.manifest.target_version),
    )?;
    println!(
        "cancelled {} rollout to {}",
        rollout.status, rollout.manifest.target_version
    );
    if rollout.status != RolloutStatus::Proposed && rollout.requires_finalization() {
        println!(
            "cluster.preserve_downgrade_option is still set, so already-upgraded nodes can be \
             rolled back; RESET it only once every node runs one version"
        );
    }
    Ok(())
}

fn status_command(cli: &Cli) -> Result<(), AppError> {
    let mut client = db_client(cli)?;
    let schema = sql_ident(&cli.schema)?;
    let stale = stale_after(Duration::from_secs(DEFAULT_DAEMON_INTERVAL_SECONDS));

    println!("ROLLOUTS (newest first)");
    for row in client.query(
        &format!(
            "
            SELECT status, current_version, target_version, created_by,
                   created_at::STRING, coalesce(approved_by, ''), coalesce(error, '')
            FROM {schema}.rollouts
            ORDER BY created_at DESC
            LIMIT 5
            "
        ),
        &[],
    )? {
        let (status, from, target): (String, String, String) = (row.get(0), row.get(1), row.get(2));
        let (by, at, approver, error): (String, String, String, String) =
            (row.get(3), row.get(4), row.get(5), row.get(6));
        println!("  {status:<10} {from} -> {target}  proposed by {by} at {at}");
        if !approver.is_empty() {
            println!("             approved by {approver}");
        }
        if !error.is_empty() {
            println!("             error: {error}");
        }
    }

    println!("\nNODES");
    for node in cluster_nodes(&mut client)? {
        println!(
            "  n{:<3} live={:<5} version={}",
            node.node_id,
            node.is_live,
            node.version
                .map(|version| version.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        );
    }

    println!("\nAGENTS");
    for row in client.query(
        &format!(
            "
            SELECT agent_id, state, version, coalesce(error, ''),
                   extract(epoch FROM now() - coalesce(last_seen, updated_at))::INT8
            FROM {schema}.agent_status
            ORDER BY agent_id
            "
        ),
        &[],
    )? {
        let (agent, state, version, error): (String, String, String, String) =
            (row.get(0), row.get(1), row.get(2), row.get(3));
        let age: i64 = row.get(4);
        let freshness = if age as u64 > stale.as_secs() {
            format!("STALE ({age}s)")
        } else {
            format!("{age}s ago")
        };
        println!("  {agent:<28} {state:<18} {version:<10} {freshness}");
        if !error.is_empty() {
            println!("  {:<28} {error}", "");
        }
    }

    println!("\nLEASES");
    for name in [LEADER_LEASE, INSTALL_LEASE] {
        println!(
            "  {name:<8} {}",
            lease_holder(cli, &mut client, name)?.unwrap_or_else(|| "free".to_string())
        );
    }

    match cluster_health(&mut client) {
        Ok(health) => println!(
            "\nHEALTH\n  {}/{} nodes live, {} under-replicated, {} unavailable ranges; gate: {}",
            health.live_nodes,
            health.active_nodes,
            health.underreplicated_ranges,
            health.unavailable_ranges,
            safe_to_restart(&health)
                .err()
                .unwrap_or_else(|| "ok".to_string())
        ),
        Err(error) => println!("\nHEALTH\n  unavailable: {error}"),
    }
    Ok(())
}

fn db_client(cli: &Cli) -> Result<Client, AppError> {
    let database_url = cli.database_url.as_deref().ok_or_else(|| {
        AppError::Message("--database-url or CROACH_ROLLOUT_DATABASE_URL is required".to_string())
    })?;
    let mut builder = TlsConnector::builder();

    if let Some(path) = cli.ssl_root_cert.as_deref() {
        let pem = fs::read(path).map_err(|error| {
            AppError::Message(format!("failed to read {}: {error}", path.display()))
        })?;
        let ca = Certificate::from_pem(&pem).map_err(|error| {
            AppError::Message(format!("failed to parse {}: {error}", path.display()))
        })?;
        builder.add_root_certificate(ca);
    }

    match (
        cli.ssl_client_cert.as_deref(),
        cli.ssl_client_key.as_deref(),
    ) {
        (Some(cert_path), Some(key_path)) => {
            let cert = fs::read(cert_path).map_err(|error| {
                AppError::Message(format!("failed to read {}: {error}", cert_path.display()))
            })?;
            let key = fs::read(key_path).map_err(|error| {
                AppError::Message(format!("failed to read {}: {error}", key_path.display()))
            })?;
            // `cockroach cert create-client` writes a PKCS#1 key
            // ("BEGIN RSA PRIVATE KEY"), but native-tls only accepts PKCS#8.
            // Point at the conversion instead of failing with a bare parse error.
            let identity = Identity::from_pkcs8(&cert, &key).map_err(|error| {
                let hint = if key.starts_with(b"-----BEGIN RSA PRIVATE KEY-----") {
                    format!(
                        "; {} is a PKCS#1 key, convert it with: \
                         openssl pkcs8 -topk8 -nocrypt -in {} -out {}.pk8",
                        key_path.display(),
                        key_path.display(),
                        key_path.display()
                    )
                } else {
                    String::new()
                };
                AppError::Message(format!(
                    "failed to load client identity from {} and {}: {error}{hint}",
                    cert_path.display(),
                    key_path.display()
                ))
            })?;
            builder.identity(identity);
        }
        (None, None) => {}
        _ => {
            return Err(AppError::Message(
                "--ssl-client-cert and --ssl-client-key must be set together".to_string(),
            ));
        }
    }

    let tls = builder
        .build()
        .map_err(|error| AppError::Message(error.to_string()))?;
    Ok(Client::connect(database_url, MakeTlsConnector::new(tls))?)
}

fn ensure_schema(cli: &Cli, client: &mut Client) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    // One statement per round trip: CockroachDB runs a multi-statement batch
    // as a single implicit transaction, and schema changes behave badly there.
    let statements = [
        format!("CREATE SCHEMA IF NOT EXISTS {schema}"),
        format!(
            "CREATE TABLE IF NOT EXISTS {schema}.leases (
                name STRING PRIMARY KEY,
                holder_id STRING NOT NULL,
                expires_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {schema}.rollouts (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                status STRING NOT NULL,
                current_version STRING NOT NULL,
                target_version STRING NOT NULL,
                manifest_json STRING NOT NULL,
                created_by STRING NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                finalized_at TIMESTAMPTZ NULL
            )"
        ),
        format!("ALTER TABLE {schema}.rollouts ADD COLUMN IF NOT EXISTS approved_by STRING NULL"),
        format!(
            "ALTER TABLE {schema}.rollouts ADD COLUMN IF NOT EXISTS approved_at TIMESTAMPTZ NULL"
        ),
        format!("ALTER TABLE {schema}.rollouts ADD COLUMN IF NOT EXISTS error STRING NULL"),
        format!(
            "CREATE TABLE IF NOT EXISTS {schema}.agent_status (
                agent_id STRING PRIMARY KEY,
                state STRING NOT NULL,
                version STRING NOT NULL,
                error STRING NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )"
        ),
        format!(
            "ALTER TABLE {schema}.agent_status ADD COLUMN IF NOT EXISTS last_seen TIMESTAMPTZ NULL"
        ),
        format!(
            "ALTER TABLE {schema}.agent_status ADD COLUMN IF NOT EXISTS rollout_id STRING NULL"
        ),
    ];
    for statement in statements {
        client.batch_execute(&statement)?;
    }
    Ok(())
}

fn acquire_lease(
    cli: &Cli,
    client: &mut Client,
    name: &str,
    holder_id: &str,
    seconds: u64,
) -> Result<LeaseResult, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let seconds = seconds as i64;
    client.execute(
        &format!(
            "
            INSERT INTO {schema}.leases (name, holder_id, expires_at, updated_at)
            VALUES ($1, $2, now() + ($3::INT8 * INTERVAL '1 second'), now())
            ON CONFLICT (name) DO NOTHING
            "
        ),
        &[&name, &holder_id, &seconds],
    )?;

    let rows = client.query(
        &format!(
            "
            UPDATE {schema}.leases
            SET holder_id = $2,
                expires_at = now() + ($3::INT8 * INTERVAL '1 second'),
                updated_at = now()
            WHERE name = $1
              AND (holder_id = $2 OR expires_at < now())
            RETURNING holder_id
            "
        ),
        &[&name, &holder_id, &seconds],
    )?;
    let current_holder: String = match rows.first() {
        Some(row) => row.get(0),
        None => client
            .query_one(
                &format!("SELECT holder_id FROM {schema}.leases WHERE name = $1"),
                &[&name],
            )?
            .get(0),
    };
    Ok(LeaseResult {
        is_leader: current_holder == holder_id,
        holder_id: current_holder,
    })
}

fn release_lease(
    cli: &Cli,
    client: &mut Client,
    name: &str,
    holder_id: &str,
) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!("DELETE FROM {schema}.leases WHERE name = $1 AND holder_id = $2"),
        &[&name, &holder_id],
    )?;
    Ok(())
}

fn lease_holder(cli: &Cli, client: &mut Client, name: &str) -> Result<Option<String>, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let rows = client.query(
        &format!("SELECT holder_id FROM {schema}.leases WHERE name = $1 AND expires_at > now()"),
        &[&name],
    )?;
    Ok(rows.first().map(|row| row.get(0)))
}

fn discover_nodes(client: &mut Client) -> Result<Vec<DiscoveredNode>, AppError> {
    let rows = client.query(
        "
        SELECT node_id, address, sql_address, is_live
        FROM crdb_internal.gossip_nodes
        WHERE is_live
        ORDER BY node_id
        ",
        &[],
    )?;
    let mut nodes = Vec::new();
    for row in rows {
        nodes.push(DiscoveredNode {
            node_id: row.get(0),
            address: row.get(1),
            sql_address: row.get(2),
            is_live: row.get(3),
        });
    }
    Ok(nodes)
}

/// Every member that has not been decommissioned, live or not, with the
/// build it runs. A dead member is included on purpose: it must block both
/// completion and the next restart.
fn cluster_nodes(client: &mut Client) -> Result<Vec<NodeObservation>, AppError> {
    let rows = client.query(
        "
        SELECT n.node_id, n.is_live, n.build_tag
        FROM crdb_internal.gossip_nodes AS n
        JOIN crdb_internal.gossip_liveness AS l ON l.node_id = n.node_id
        WHERE l.membership = 'active'
        ORDER BY n.node_id
        ",
        &[],
    )?;
    Ok(rows.iter().map(node_observation_from_row).collect())
}

fn own_node_observation(client: &mut Client) -> Result<NodeObservation, AppError> {
    let row = client.query_one(
        "
        SELECT node_id, is_live, build_tag
        FROM crdb_internal.gossip_nodes
        WHERE node_id = crdb_internal.node_id()
        ",
        &[],
    )?;
    Ok(node_observation_from_row(&row))
}

/// The version the local CockroachDB process is actually running, which can
/// differ from the file on disk if a swap happened without a restart.
fn own_node_version(client: &mut Client) -> Result<Version, AppError> {
    let node = own_node_observation(client)?;
    node.version.ok_or_else(|| {
        AppError::Message(format!(
            "node {} reports an unparseable build tag",
            node.node_id
        ))
    })
}

fn node_observation_from_row(row: &postgres::Row) -> NodeObservation {
    let build_tag: String = row.get(2);
    NodeObservation {
        node_id: row.get(0),
        is_live: row.get(1),
        version: parse_cockroach_version(&build_tag).ok(),
    }
}

fn cluster_health(client: &mut Client) -> Result<ClusterHealth, AppError> {
    let nodes = cluster_nodes(client)?;
    let row = client.query_one(
        "
        SELECT
            coalesce(sum((metrics->>'ranges.underreplicated')::FLOAT8), 0)::INT8,
            coalesce(sum((metrics->>'ranges.unavailable')::FLOAT8), 0)::INT8
        FROM crdb_internal.kv_store_status
        ",
        &[],
    )?;
    Ok(ClusterHealth {
        active_nodes: nodes.len(),
        live_nodes: nodes.iter().filter(|node| node.is_live).count(),
        underreplicated_ranges: row.get(0),
        unavailable_ranges: row.get(1),
    })
}

/// Liveness only. Leaves `state` alone so a heartbeat never erases the
/// outcome of the last install.
fn heartbeat_agent(
    cli: &Cli,
    client: &mut Client,
    agent_id: &str,
    version: &Version,
) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!(
            "
            INSERT INTO {schema}.agent_status (agent_id, state, version, updated_at, last_seen)
            VALUES ($1, 'idle', $2, now(), now())
            ON CONFLICT (agent_id) DO UPDATE
            SET version = excluded.version, last_seen = now()
            "
        ),
        &[&agent_id, &version.to_string()],
    )?;
    Ok(())
}

fn record_agent_state(
    cli: &Cli,
    client: &mut Client,
    agent_id: &str,
    state: &str,
    version: &Version,
    rollout_id: &str,
    error: Option<&str>,
) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!(
            "
            UPSERT INTO {schema}.agent_status
                (agent_id, state, version, error, rollout_id, updated_at, last_seen)
            VALUES ($1, $2, $3, $4, $5, now(), now())
            "
        ),
        &[&agent_id, &state, &version.to_string(), &error, &rollout_id],
    )?;
    Ok(())
}

struct OpenRollout {
    id: String,
    status: RolloutStatus,
    manifest: RolloutManifest,
}

impl OpenRollout {
    fn requires_finalization(&self) -> bool {
        major_line(&self.manifest.current_version) != major_line(&self.manifest.target_version)
    }
}

/// The newest rollout that is proposed, active, or failed. At most one is
/// open at a time because the leader only proposes when none is.
fn open_rollout(cli: &Cli, client: &mut Client) -> Result<Option<OpenRollout>, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let rows = client.query(
        &format!(
            "
            SELECT id::STRING, status, manifest_json
            FROM {schema}.rollouts
            WHERE status IN ('proposed', 'active', 'failed')
            ORDER BY created_at DESC
            LIMIT 1
            "
        ),
        &[],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let status: String = row.get(1);
    let manifest_json: String = row.get(2);
    Ok(Some(OpenRollout {
        id: row.get(0),
        status: status.parse().map_err(AppError::Message)?,
        manifest: serde_json::from_str(&manifest_json)?,
    }))
}

fn set_rollout_status(
    cli: &Cli,
    client: &mut Client,
    id: &str,
    status: RolloutStatus,
    error: Option<&str>,
) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!(
            "
            UPDATE {schema}.rollouts
            SET status = $2,
                error = coalesce($3, error),
                finalized_at = CASE WHEN $2 = 'finalized' THEN now() ELSE finalized_at END
            WHERE id::STRING = $1
            "
        ),
        &[&id, &status.as_str(), &error],
    )?;
    Ok(())
}

/// Downloads both architectures and records their checksums. Release-note
/// warnings are recorded, not enforced: `approve` enforces them.
fn create_manifest_for_plan(cli: &Cli, plan: UpgradePlan) -> Result<RolloutManifest, AppError> {
    fs::create_dir_all(&cli.artifacts_dir)?;
    let mut artifacts = Vec::new();
    for arch in SUPPORTED_ARCHES {
        let url = cockroach_url(&cli.base_url, &plan.next_version, arch);
        let destination = cli.artifacts_dir.join(format!(
            "cockroach-{}.linux-{}.tgz",
            plan.next_version, arch
        ));
        if !destination.exists() {
            download_to_file(&url, &destination)?;
        }
        let (sha256, bytes) = sha256_file(&destination)?;
        artifacts.push(Artifact {
            os: "linux".to_string(),
            arch: (*arch).to_string(),
            url,
            path: destination.to_string_lossy().into_owned(),
            sha256,
            bytes,
        });
    }

    Ok(RolloutManifest {
        schema_version: 1,
        created_unix: unix_time()?,
        current_version: plan.current_version,
        target_version: plan.next_version,
        release_notes_url: plan.release_notes_url,
        release_note_warnings: plan.release_note_warnings,
        release_note_warnings_approved: false,
        artifacts,
    })
}

fn publish_rollout(
    cli: &Cli,
    client: &mut Client,
    manifest: &RolloutManifest,
) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    let agent_id = agent_node_id(cli)?;
    let manifest_json = serde_json::to_string(manifest)?;
    client.execute(
        &format!(
            "
            INSERT INTO {schema}.rollouts
                (status, current_version, target_version, manifest_json, created_by)
            VALUES ('proposed', $1, $2, $3, $4)
            "
        ),
        &[
            &manifest.current_version.to_string(),
            &manifest.target_version.to_string(),
            &manifest_json,
            &agent_id,
        ],
    )?;
    audit(
        cli,
        "rollout_proposed",
        &format!(
            "current={} target={} warnings={}",
            manifest.current_version,
            manifest.target_version,
            manifest.release_note_warnings.len()
        ),
    )?;
    Ok(())
}

fn agent_node_id(cli: &Cli) -> Result<String, AppError> {
    if let Some(node_id) = &cli.node_id {
        return Ok(node_id.clone());
    }
    let hostname = hostname::get()
        .map_err(AppError::Io)?
        .to_string_lossy()
        .into_owned();
    Ok(format!("{hostname}:{}", normalized_arch()))
}

fn sql_ident(value: &str) -> Result<String, AppError> {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        Ok(value.to_string())
    } else {
        Err(AppError::Message(format!(
            "invalid SQL identifier: {value}"
        )))
    }
}

fn service_name(cli: &Cli) -> Result<&str, AppError> {
    cli.service_name.as_deref().ok_or_else(|| {
        AppError::Message(
            "--service-name or CROACH_ROLLOUT_SERVICE is required (for example cockroach.service)"
                .to_string(),
        )
    })
}

fn finalize_command(cli: &Cli, target_version: &str, dry_run: bool) -> Result<(), AppError> {
    let version = parse_cockroach_version(target_version)?;
    reject_prerelease(&version)?;
    let mut client = db_client(cli)?;

    let nodes = cluster_nodes(&mut client)?;
    if !all_nodes_on(&version, &nodes) {
        return Err(AppError::Message(format!(
            "refusing to finalize: not every node is live on {version}; see status"
        )));
    }
    if dry_run {
        println!(
            "dry run: RESET CLUSTER SETTING cluster.preserve_downgrade_option; \
             SET CLUSTER SETTING version = '{}'",
            major_line_string(&version)
        );
        return Ok(());
    }

    finalize_cluster(cli, &mut client, &version)?;
    if let Some(rollout) =
        open_rollout(cli, &mut client)?.filter(|rollout| rollout.manifest.target_version == version)
    {
        set_rollout_status(
            cli,
            &mut client,
            &rollout.id,
            RolloutStatus::Finalized,
            None,
        )?;
    }
    Ok(())
}

/// Finalizes over the agent's own authenticated connection. Needs the
/// `MODIFYCLUSTERSETTING` system privilege. After this, downgrading to the
/// previous release line is no longer possible.
fn finalize_cluster(cli: &Cli, client: &mut Client, version: &Version) -> Result<(), AppError> {
    let cluster_version = major_line_string(version);
    audit(
        cli,
        "finalize_requested",
        &format!("target={version} cluster_version={cluster_version}"),
    )?;
    client.batch_execute("RESET CLUSTER SETTING cluster.preserve_downgrade_option")?;
    client.batch_execute(&format!(
        "SET CLUSTER SETTING version = '{cluster_version}'"
    ))?;
    audit(
        cli,
        "finalize_complete",
        &format!("cluster_version={cluster_version}"),
    )
}

/// Checks every precondition a rollout depends on, without stopping or
/// restarting anything, so a misconfigured host fails here instead of
/// halfway through an upgrade.
fn self_check_command(cli: &Cli) -> Result<(), AppError> {
    audit(
        cli,
        "self_check_start",
        "validating layout, restart access, and SQL",
    )?;
    let mut failures = Vec::new();
    let mut check = |name: &str, result: Result<String, String>| match result {
        Ok(detail) => println!("ok    {name}: {detail}"),
        Err(detail) => {
            println!("FAIL  {name}: {detail}");
            failures.push(name.to_string());
        }
    };

    for command in ["tar", "systemctl", "pkcheck"] {
        check(
            &format!("command {command}"),
            require_command(command)
                .map(|()| "present".to_string())
                .map_err(|error| error.to_string()),
        );
    }

    let service = service_name(cli).map_err(|error| error.to_string());
    check("service name", service.clone().map(str::to_string));

    let layout = BinaryLayout::new(&cli.agent_root);
    check(
        "binary layout",
        layout.verify_wired(&cli.binary_path).map(|()| {
            format!(
                "{} -> {}",
                cli.binary_path.display(),
                layout.current_link().display()
            )
        }),
    );
    for dir in [
        layout.bin_dir(),
        layout.versions_dir(),
        cli.artifacts_dir.clone(),
    ] {
        check(
            &format!("writable {}", dir.display()),
            probe_writable(&dir).map(|()| "yes".to_string()),
        );
    }
    check(
        "installed version",
        installed_cockroach_version(&cli.binary_path)
            .map(|version| version.to_string())
            .map_err(|error| error.to_string()),
    );

    if let Ok(service) = &service {
        check("restart authorization", polkit_can_restart(service));
    }

    if cli.database_url.is_some() {
        check(
            "sql cluster view",
            db_client(cli)
                .and_then(|mut client| cluster_health(&mut client))
                .map(|health| format!("{}/{} nodes live", health.live_nodes, health.active_nodes))
                .map_err(|error| error.to_string()),
        );
        check(
            "sql cluster settings",
            db_client(cli)
                .and_then(|mut client| {
                    Ok(client.query_one(
                        "SHOW CLUSTER SETTING cluster.preserve_downgrade_option",
                        &[],
                    )?)
                })
                .map(|row| format!("preserve_downgrade_option='{}'", row.get::<_, String>(0)))
                .map_err(|error| error.to_string()),
        );
    }

    if failures.is_empty() {
        audit(cli, "self_check_complete", "all checks passed")?;
        Ok(())
    } else {
        audit(cli, "self_check_failed", &failures.join(","))?;
        Err(AppError::Message(format!(
            "{} check(s) failed: {}",
            failures.len(),
            failures.join(", ")
        )))
    }
}

/// Asks polkit whether this process may restart `service`, without
/// restarting it. systemd's rules key off the `unit` and `verb` details, so
/// the probe passes both.
fn polkit_can_restart(service: &str) -> Result<String, String> {
    let output = Command::new("pkcheck")
        .args([
            "--action-id",
            POLKIT_MANAGE_UNITS,
            "--process",
            &std::process::id().to_string(),
            "--detail",
            "unit",
            service,
            "--detail",
            "verb",
            "restart",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not run pkcheck: {error}"))?;
    if output.status.success() {
        Ok(format!("polkit allows restart of {service}"))
    } else {
        Err(format!(
            "polkit denies restart of {service}; install the polkit rule for this unit ({})",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn available_cockroach_versions(github_api_url: &str) -> Result<Vec<Version>, AppError> {
    let releases_url = github_tags_url(github_api_url);
    let client = reqwest::blocking::Client::new();
    let mut releases = Vec::new();
    for page in 1..=10 {
        let page_url = paginated_url(&releases_url, page);
        let page_releases: Vec<GitHubRelease> = client
            .get(page_url)
            .header(reqwest::header::USER_AGENT, "cockroach-rollout-agent")
            .send()?
            .error_for_status()?
            .json()?;
        let page_len = page_releases.len();
        releases.extend(page_releases);
        if page_len < 100 {
            break;
        }
    }

    let mut versions = releases
        .iter()
        .filter_map(|release| parse_cockroach_version(&release.tag_name).ok())
        .filter(|version| version.pre.is_empty())
        .collect::<Vec<_>>();
    versions.sort();
    versions.dedup();
    Ok(versions)
}

fn github_tags_url(github_api_url: &str) -> String {
    let base = github_api_url
        .trim_end_matches('/')
        .replace("/releases/latest", "/tags")
        .replace("/releases", "/tags")
        .trim_end_matches("/latest")
        .to_string();
    if base.contains('?') {
        format!("{base}&per_page=100")
    } else {
        format!("{base}?per_page=100")
    }
}

fn paginated_url(base: &str, page: u16) -> String {
    if base.contains("page=") {
        base.to_string()
    } else if base.contains('?') {
        format!("{base}&page={page}")
    } else {
        format!("{base}?page={page}")
    }
}

fn installed_cockroach_version(binary_path: &Path) -> Result<Version, AppError> {
    let output = Command::new(binary_path)
        .arg("version")
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(AppError::Message(format!(
            "{} version exited with status {}",
            binary_path.display(),
            output.status
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_cockroach_version(&stdout)
}

fn parse_cockroach_version(input: &str) -> Result<Version, AppError> {
    let regex = Regex::new(r"v?(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)")?;
    let captures = regex.captures(input).ok_or_else(|| {
        AppError::Message(format!("could not parse CockroachDB version: {input}"))
    })?;
    Ok(Version::parse(&captures[1])?)
}

fn reject_prerelease(version: &Version) -> Result<(), AppError> {
    if version.pre.is_empty() {
        Ok(())
    } else {
        Err(AppError::Message(format!(
            "pre-production CockroachDB releases are refused: v{version}"
        )))
    }
}

fn build_upgrade_steps(
    current: &Version,
    requested_target: &Version,
    available_versions: &[Version],
    release_notes_base_url: &str,
) -> Result<Vec<UpgradeStep>, AppError> {
    reject_prerelease(current)?;
    reject_prerelease(requested_target)?;

    if requested_target < current {
        return Err(AppError::Message(format!(
            "downgrades are not supported: current={current} target={requested_target}"
        )));
    }

    if current == requested_target {
        return Ok(Vec::new());
    }

    let current_line = major_line(current);
    let target_line = major_line(requested_target);
    if current_line == target_line {
        return Ok(vec![UpgradeStep {
            from_version: current.clone(),
            to_version: requested_target.clone(),
            release_line: major_line_string(requested_target),
            release_notes_url: release_notes_url(release_notes_base_url, requested_target),
            requires_finalization: false,
        }]);
    }

    let mut lines = available_versions
        .iter()
        .map(major_line)
        .filter(|line| *line > current_line && *line <= target_line)
        .collect::<Vec<_>>();
    lines.sort();
    lines.dedup();

    if !lines.contains(&target_line) {
        return Err(AppError::Message(format!(
            "target release line {} was not found in upstream production releases",
            major_line_string(requested_target)
        )));
    }

    let mut steps = Vec::new();
    let mut from_version = current.clone();
    for line in lines {
        let to_version = if line == target_line {
            requested_target.clone()
        } else {
            latest_version_for_line(line, available_versions)?
        };
        steps.push(UpgradeStep {
            from_version: from_version.clone(),
            to_version: to_version.clone(),
            release_line: major_line_string(&to_version),
            release_notes_url: release_notes_url(release_notes_base_url, &to_version),
            requires_finalization: true,
        });
        from_version = to_version;
    }

    Ok(steps)
}

fn latest_version_for_line(
    line: (u64, u64),
    available_versions: &[Version],
) -> Result<Version, AppError> {
    available_versions
        .iter()
        .filter(|version| major_line(version) == line)
        .max()
        .cloned()
        .ok_or_else(|| {
            AppError::Message(format!(
                "no upstream production release found for {}.{}",
                line.0, line.1
            ))
        })
}

fn validate_manifest_current_version(
    current: &Version,
    manifest: &RolloutManifest,
) -> Result<(), AppError> {
    reject_prerelease(current)?;
    reject_prerelease(&manifest.current_version)?;
    reject_prerelease(&manifest.target_version)?;

    if manifest.target_version < manifest.current_version {
        return Err(AppError::Message(format!(
            "manifest describes a downgrade: current={} target={}",
            manifest.current_version, manifest.target_version
        )));
    }

    if current != &manifest.current_version {
        return Err(AppError::Message(format!(
            "manifest was prepared for current={} but local binary is current={current}; finish and finalize prior steps first",
            manifest.current_version
        )));
    }

    Ok(())
}

fn major_line(version: &Version) -> (u64, u64) {
    (version.major, version.minor)
}

fn major_line_string(version: &Version) -> String {
    format!("{}.{}", version.major, version.minor)
}

fn release_notes_url(base_url: &str, version: &Version) -> String {
    format!(
        "{}/v{}.{}",
        base_url.trim_end_matches('/'),
        version.major,
        version.minor
    )
}

fn scan_release_notes(notes: &str) -> Result<Vec<String>, AppError> {
    let mut warnings = Vec::new();
    let lower_notes = notes.to_lowercase();
    for pattern in BREAKING_CHANGE_PATTERNS {
        if lower_notes.contains(pattern) {
            warnings.push((*pattern).to_string());
        }
    }
    warnings.sort();
    warnings.dedup();
    Ok(warnings)
}

fn cockroach_url(base_url: &str, version: &Version, arch: &str) -> String {
    format!(
        "{}/cockroach-v{}.linux-{}.tgz",
        base_url.trim_end_matches('/'),
        version,
        arch
    )
}

fn download_to_file(url: &str, destination: &Path) -> Result<(), AppError> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let request = reqwest::blocking::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, "cockroach-rollout-agent");
    let mut response = apply_optional_psk(request).send()?.error_for_status()?;
    let mut output = fs::File::create(destination)?;
    io::copy(&mut response, &mut output)?;
    Ok(())
}

fn fetch_text(url: &str) -> Result<String, AppError> {
    let request = reqwest::blocking::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, "cockroach-rollout-agent");
    Ok(apply_optional_psk(request)
        .send()?
        .error_for_status()?
        .text()?)
}

fn apply_optional_psk(
    request: reqwest::blocking::RequestBuilder,
) -> reqwest::blocking::RequestBuilder {
    match std::env::var("CROACH_ROLLOUT_PSK") {
        Ok(psk) if !psk.is_empty() => request.bearer_auth(psk),
        _ => request,
    }
}

fn verify_artifact(path: &Path, artifact: &Artifact) -> Result<(), AppError> {
    if !path.is_file() {
        return Err(AppError::Message(format!(
            "artifact is missing: {}",
            path.display()
        )));
    }
    let (sha256, bytes) = sha256_file(path)?;
    if sha256 != artifact.sha256 {
        return Err(AppError::Message(format!(
            "sha256 mismatch for {}: expected={} actual={}",
            path.display(),
            artifact.sha256,
            sha256
        )));
    }
    if bytes != artifact.bytes {
        return Err(AppError::Message(format!(
            "size mismatch for {}: expected={} actual={}",
            path.display(),
            artifact.bytes,
            bytes
        )));
    }
    Ok(())
}

fn find_cockroach_binary(root: &Path) -> Result<PathBuf, AppError> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            let entry_path = entry.path();
            if entry_path.is_dir() {
                stack.push(entry_path);
            } else if entry_path
                .file_name()
                .is_some_and(|name| name == "cockroach")
            {
                return Ok(entry_path);
            }
        }
    }
    Err(AppError::Message(format!(
        "no cockroach binary found under {}",
        root.display()
    )))
}

fn require_command(name: &str) -> Result<(), AppError> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::Message(format!(
            "required command is unavailable: {name}"
        )))
    }
}

fn run_command<I, S>(program: &str, args: I) -> Result<(), AppError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(AppError::Message(format!(
            "{program} exited with status {status}"
        )))
    }
}

/// Runs `systemctl <action> <unit>` as the agent user. Authorization comes
/// from a polkit rule scoped to this unit; there is deliberately no sudo
/// fallback, because the unit runs with `NoNewPrivileges=true`, which makes
/// setuid sudo fail every time.
fn run_systemctl(action: &str, service_name: &str) -> Result<(), AppError> {
    if command_status("systemctl", [OsStr::new(action), OsStr::new(service_name)])? {
        return Ok(());
    }
    Err(AppError::Message(format!(
        "systemctl {action} {service_name} was refused; install the polkit rule for this unit"
    )))
}

fn command_status<I, S>(program: &str, args: I) -> Result<bool, AppError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .status()?;
    Ok(status.success())
}

fn sha256_file(path: &Path) -> Result<(String, u64), AppError> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        hasher.update(&buffer[..read]);
    }

    Ok((hex::encode(hasher.finalize()), total))
}

fn read_json_file<T>(path: &Path) -> Result<T, AppError>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_json_file<T>(path: &Path, value: &T) -> Result<(), AppError>
where
    T: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::File::create(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn audit(cli: &Cli, event: &str, detail: &str) -> Result<(), AppError> {
    if let Some(parent) = cli.audit_log.parent() {
        fs::create_dir_all(parent)?;
    }

    let line = format!(
        "ts={} event={} detail={}\n",
        unix_time()?,
        sanitize_log_field(event),
        sanitize_log_field(detail)
    );
    append_file(&cli.audit_log, line.as_bytes())?;
    Ok(())
}

fn append_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(bytes)
}

fn sanitize_log_field(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

fn normalized_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "amd64".to_string(),
        "aarch64" => "arm64".to_string(),
        other => other.to_string(),
    }
}

fn unix_time() -> Result<u64, AppError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| AppError::Message(error.to_string()))?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_from_tag() {
        let version = parse_cockroach_version("v26.2.1").expect("version should parse");
        assert_eq!(
            version,
            Version::parse("26.2.1").expect("literal should parse")
        );
    }

    #[test]
    fn parse_version_from_command_output() {
        let version =
            parse_cockroach_version("CockroachDB CCL v25.4.3").expect("version should parse");
        assert_eq!(
            version,
            Version::parse("25.4.3").expect("literal should parse")
        );
    }

    #[test]
    fn parses_and_rejects_alpha_versions() {
        let version = parse_cockroach_version("v26.3.0-alpha.1").expect("version should parse");
        assert_eq!(version.pre.as_str(), "alpha.1");
        assert!(reject_prerelease(&version).is_err());
    }

    #[test]
    fn builds_release_line_steps() {
        let current = Version::parse("24.1.25").expect("literal should parse");
        let target = Version::parse("25.2.9").expect("literal should parse");
        let available = vec![
            Version::parse("24.3.23").expect("literal should parse"),
            Version::parse("25.1.10").expect("literal should parse"),
            Version::parse("25.2.9").expect("literal should parse"),
        ];

        let steps = build_upgrade_steps(
            &current,
            &target,
            &available,
            "https://www.cockroachlabs.com/docs/releases",
        )
        .expect("steps should build");

        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].to_version, Version::parse("24.3.23").unwrap());
        assert_eq!(steps[1].to_version, Version::parse("25.1.10").unwrap());
        assert_eq!(steps[2].to_version, Version::parse("25.2.9").unwrap());
        assert!(steps.iter().all(|step| step.requires_finalization));
    }

    #[test]
    fn patch_upgrade_is_single_non_finalizing_step() {
        let current = Version::parse("25.2.7").expect("literal should parse");
        let target = Version::parse("25.2.9").expect("literal should parse");
        let available = vec![target.clone()];
        let steps = build_upgrade_steps(
            &current,
            &target,
            &available,
            "https://www.cockroachlabs.com/docs/releases",
        )
        .expect("steps should build");

        assert_eq!(steps.len(), 1);
        assert!(!steps[0].requires_finalization);
    }

    #[test]
    fn release_notes_url_uses_official_path_shape() {
        let version = Version::parse("26.2.1").expect("literal should parse");
        assert_eq!(
            release_notes_url("https://www.cockroachlabs.com/docs/releases/", &version),
            "https://www.cockroachlabs.com/docs/releases/v26.2"
        );
    }

    #[test]
    fn release_note_scan_finds_breaking_patterns() {
        let warnings =
            scan_release_notes("Before upgrading, review backward incompatible changes.")
                .expect("scan should succeed");
        assert!(warnings.contains(&"backward incompatible".to_string()));
    }
}
