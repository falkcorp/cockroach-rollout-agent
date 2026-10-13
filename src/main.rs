// file: src/main.rs
// version: 3.14.0
// guid: d16be11a-b10c-4d2e-853f-d4a1c0a3c617
// last-edited: 2026-10-12

mod cluster_view;
mod events;
mod health;
mod layout;
mod ops;
mod releases;
mod rollout;
mod settings;
mod uicert;
mod web;

use std::ffi::OsStr;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
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

use crate::events::Source;
use crate::health::{ClusterHealth, safe_to_restart};
use crate::layout::{BinaryLayout, probe_writable};
use crate::ops::Actor;
use crate::rollout::{
    FollowerAction, LeaderAction, NodeObservation, RestartAction, RolloutKind, RolloutStatus,
    all_nodes_on, follower_action, leader_action, live_version_span, proposal_declined,
    restart_action, restart_complete, restarted_on, should_note,
};
use crate::settings::SettingsReport;

const DEFAULT_BASE_URL: &str = "https://binaries.cockroachdb.com";
const DEFAULT_GITHUB_API_URL: &str =
    "https://api.github.com/repos/cockroachdb/cockroach/tags?per_page=100";
const DEFAULT_RELEASE_NOTES_BASE_URL: &str = "https://www.cockroachlabs.com/docs/releases";
const DEFAULT_ARTIFACTS_DIR: &str = "dist";
const DEFAULT_AUDIT_LOG: &str = "/var/log/cockroach-rollout-agent/audit.log";
const DEFAULT_MANIFEST_PATH: &str = "dist/manifest.json";
const DEFAULT_DAEMON_INTERVAL_SECONDS: u64 = 300;
const DEFAULT_LEASE_SECONDS: u64 = 90;
const DEFAULT_AGENT_ROOT: &str = "/var/lib/cockroach-rollout-agent";
const DEFAULT_RESTART_TIMEOUT_SECONDS: u64 = 600;
const DEFAULT_UI_LISTEN: &str = "0.0.0.0:38090";
const LEADER_LEASE: &str = "leader";
const INSTALL_LEASE: &str = "install";
const UI_CERT_RENEW_LEASE: &str = "ui_cert_renew";
/// The install lease outlives a full download + restart + health wait, so a
/// crashed holder blocks the next node for a bounded time instead of forever.
const INSTALL_LEASE_EXTRA_SECONDS: u64 = 900;
const HEALTH_POLL_SECONDS: u64 = 5;
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

#[derive(Debug, Clone, Parser)]
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

    /// Agent-owned directory holding `bin/cockroach` and `versions/`. The
    /// CockroachDB unit's `ExecStart` must run `<agent-root>/bin/cockroach`.
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

    /// CockroachDB binary whose version counts as installed. Defaults to
    /// `<agent-root>/bin/cockroach`; override only on hosts without the
    /// agent layout, for example when running `plan` from a workstation.
    #[arg(long, env = "CROACH_ROLLOUT_BINARY_PATH")]
    binary_path: Option<PathBuf>,

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

    /// Address the web UI listens on, or `off`.
    #[arg(long, env = "CROACH_ROLLOUT_UI_LISTEN", default_value = DEFAULT_UI_LISTEN)]
    ui_listen: String,

    /// Directory holding `node.crt` and `node.key` for the UI. Defaults to
    /// the CockroachDB unit's `--certs-dir`.
    #[arg(long, env = "CROACH_ROLLOUT_UI_CERTS_DIR")]
    ui_certs_dir: Option<PathBuf>,

    /// How long a UI login lasts.
    #[arg(long, env = "CROACH_ROLLOUT_UI_SESSION_HOURS", default_value_t = 12)]
    ui_session_hours: u64,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Clone, Subcommand)]
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

    /// Decline the proposed rollout; the leader will not propose its target again.
    Decline {
        #[arg(long, default_value = "declined by operator")]
        reason: String,
    },

    /// Allow a declined version to be proposed again.
    Undecline {
        /// The declined target version, for example 26.1.8.
        version: String,
    },

    /// Cancel the open rollout. Nodes already upgraded stay upgraded.
    Cancel {
        #[arg(long, default_value = "cancelled by operator")]
        reason: String,
    },

    /// Restart every node, one at a time, on the version it already runs.
    /// Same install lease and health gate as an upgrade.
    RestartCluster,

    /// Set the shared web UI password and sign out every session. Reads the
    /// password from stdin, prompting without echo on a terminal.
    SetUiPassword,

    /// Manage the web UI's certificates: a name-constrained private CA on
    /// one node, and leaf certificates staged for a person to approve.
    UiCert {
        #[command(subcommand)]
        action: UiCertCommand,
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

#[derive(Debug, Clone, Subcommand)]
enum UiCertCommand {
    /// Create the Rollout UI CA on this node, constrained to every agent's
    /// hostname and every node's IP. Refuses to replace an existing CA.
    InitCa {
        /// Another DNS name or IP the CA may sign for. Repeatable.
        #[arg(long)]
        extra_name: Vec<String>,
    },
    /// Stage a new leaf certificate when none is approved or the approved
    /// one expires soon. Does nothing on nodes without the CA key.
    Renew {
        /// Renew when the approved certificate expires within this many days.
        #[arg(long, default_value_t = uicert::RENEW_BEFORE_DAYS)]
        before_days: i64,
        /// Lifetime of the new certificate, in days.
        #[arg(long, default_value_t = uicert::LEAF_DAYS)]
        days: i64,
    },
    /// Stage a certificate and key from files for approval.
    Stage { cert: PathBuf, key: PathBuf },
    /// Approve the staged certificate; every agent then installs it.
    Approve { fingerprint: String },
    /// Reject the staged certificate.
    Reject { fingerprint: String },
    /// Show the approved and staged certificates.
    Show,
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
    /// Overridden settings the target changes; `None` for manifests made
    /// before v0.2.0, by `prepare`, and for restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settings_report: Option<settings::SettingsReport>,
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
        Commands::Decline { reason } => with_client(&cli, |client| {
            ops::decline(&cli, client, &Actor::Cli, None, reason)
        }),
        Commands::Undecline { version } => with_client(&cli, |client| {
            ops::undecline(&cli, client, &Actor::Cli, version)
        }),
        Commands::Cancel { reason } => cancel_command(&cli, reason),
        Commands::RestartCluster => restart_cluster_command(&cli),
        Commands::SetUiPassword => set_ui_password_command(&cli),
        Commands::UiCert { action } => ui_cert_command(&cli, action),
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
        settings_report: None,
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
/// Fetches the release list live; the daemon uses the cached catalog.
fn next_upgrade_plan(
    cli: &Cli,
    current_version: Option<&str>,
    target_version: Option<&str>,
) -> Result<Option<UpgradePlan>, AppError> {
    let current = match current_version {
        Some(version) => parse_cockroach_version(version)?,
        None => installed_cockroach_version(&cockroach_binary(cli))?,
    };
    let target = target_version.map(parse_cockroach_version).transpose()?;
    let versions = available_cockroach_versions(&cli.github_api_url)?;
    plan_from_versions(cli, &current, target.as_ref(), versions)?
        .map(with_release_notes)
        .transpose()
}

/// The next release-line step from `current` toward `target` (default: the
/// newest release), without the release-note scan. Cheap: no network.
fn plan_from_versions(
    cli: &Cli,
    current: &Version,
    target: Option<&Version>,
    available_versions: Vec<Version>,
) -> Result<Option<UpgradePlan>, AppError> {
    let latest = available_versions
        .iter()
        .max()
        .cloned()
        .ok_or_else(|| AppError::Message("no CockroachDB releases discovered".to_string()))?;
    let target = target.cloned().unwrap_or_else(|| latest.clone());
    let upgrade_steps = build_upgrade_steps(
        current,
        &target,
        &available_versions,
        &cli.release_notes_base_url,
    )?;
    let Some(next_step) = upgrade_steps.first().cloned() else {
        return Ok(None);
    };
    Ok(Some(UpgradePlan {
        current_version: current.clone(),
        requested_target_version: target,
        next_version: next_step.to_version.clone(),
        latest_version: latest,
        release_notes_url: next_step.release_notes_url.clone(),
        release_note_warnings: Vec::new(),
        upgrade_steps,
        release_line_by_release_line: true,
    }))
}

fn with_release_notes(mut plan: UpgradePlan) -> Result<UpgradePlan, AppError> {
    let release_notes = fetch_text(&plan.release_notes_url)?;
    plan.release_note_warnings = scan_release_notes(&release_notes)?;
    Ok(plan)
}

fn install_command(
    cli: &Cli,
    manifest_path: &Path,
    arch: Option<&str>,
    dry_run: bool,
) -> Result<(), AppError> {
    let manifest: RolloutManifest = read_json_file(manifest_path)?;
    let current = installed_cockroach_version(&cockroach_binary(cli))?;
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
    // Dry runs promise to write nothing, and UI actions write.
    let ui = if dry_run { None } else { start_web_ui(cli)? };

    let mut schema_ready = false;
    loop {
        let result = (|| -> Result<(), AppError> {
            let mut client = db_client(cli)?;
            if !schema_ready && !dry_run {
                ensure_schema(cli, &mut client)?;
                schema_ready = true;
            }
            db_daemon_tick(
                cli,
                &mut client,
                &agent_id,
                dry_run,
                auto_finalize,
                ui.as_deref(),
            )
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
    ui: Option<&web::UiServer>,
) -> Result<(), AppError> {
    let installed = installed_cockroach_version(&cockroach_binary(cli))?;

    if dry_run {
        let holder = lease_holder(cli, client, LEADER_LEASE)?;
        println!(
            "dry run: agent={agent_id} installed={installed} leader={}",
            holder.as_deref().unwrap_or("none")
        );
    } else {
        if let Some(ui) = ui {
            sync_ui_cert(cli, client, agent_id, ui)?;
        }
        heartbeat_agent(
            cli,
            client,
            agent_id,
            &installed,
            ui.and_then(web::UiServer::served).as_deref(),
        )?;
        let lease = acquire_lease(cli, client, LEADER_LEASE, agent_id, DEFAULT_LEASE_SECONDS)?;
        if !lease.is_leader {
            audit(
                cli,
                "leader_observed",
                &format!("holder={}", lease.holder_id),
            )?;
            return follower_reconcile(cli, client, agent_id, false);
        }
        events::prune(cli, client)?;
        refresh_catalog_if_due(cli, client)?;
    }

    leader_reconcile(cli, client, auto_finalize, dry_run)?;
    follower_reconcile(cli, client, agent_id, dry_run)
}

/// Installs the approved UI certificate when it is not the one being served.
/// A certificate this node cannot use is reported on its agent row and the
/// old one keeps serving; it never fails the tick.
fn sync_ui_cert(
    cli: &Cli,
    client: &mut Client,
    agent_id: &str,
    ui: &web::UiServer,
) -> Result<(), AppError> {
    let Some(material) = uicert::material(cli, client, None, "approved")? else {
        return Ok(());
    };
    if ui.served().as_deref() == Some(material.fingerprint.as_str()) {
        return Ok(());
    }
    let result = install_ui_cert(cli, client, agent_id, &material, ui);
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!("UPDATE {schema}.agent_status SET ui_cert_error = $2 WHERE agent_id = $1"),
        &[&agent_id, &result.as_ref().err()],
    )?;
    if result.is_ok() {
        events::note(
            cli,
            client,
            Source::Agent,
            "ui_cert_installed",
            &format!("fingerprint={}", material.fingerprint),
        )?;
    }
    Ok(())
}

fn install_ui_cert(
    cli: &Cli,
    client: &mut Client,
    agent_id: &str,
    material: &uicert::Material,
    ui: &web::UiServer,
) -> Result<(), String> {
    let now = unix_time().map_err(|error| error.to_string())? as i64;
    let info = uicert::validate(
        &material.pem.cert,
        &material.pem.key,
        &material.chain_pem,
        now,
    )?;
    let hostname = agent_id.split(':').next().unwrap_or(agent_id);
    let ips = own_gossip_ips(client).map_err(|error| error.to_string())?;
    if !uicert::covers(&info, hostname, &ips) {
        return Err(format!(
            "this node ({hostname}, {}) is not among the certificate's names ({})",
            ips.iter()
                .map(IpAddr::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            info.hosts().describe()
        ));
    }
    // Load it the way the listener will before touching disk, so a pair
    // that validates but does not load never replaces the working files.
    web::tls::server_config(material.pem.cert.as_bytes(), material.pem.key.as_bytes())?;
    write_ui_cert_files(&managed_ui_certs_dir(cli), &material.pem)
        .map_err(|error| format!("cannot write the certificate: {error}"))?;
    ui.install(&material.pem.cert, &material.pem.key, &material.fingerprint)
}

/// This node's address as other nodes see it, when it is an IP.
fn own_gossip_ips(client: &mut Client) -> Result<Vec<IpAddr>, AppError> {
    let row = client.query_one(
        "SELECT address FROM crdb_internal.gossip_nodes WHERE node_id = crdb_internal.node_id()",
        &[],
    )?;
    let address: String = row.get(0);
    Ok(uicert::hosts_from(&[], &[address], &[]).ips)
}

/// The agent-owned directory approved UI certificates are written to.
fn managed_ui_certs_dir(cli: &Cli) -> PathBuf {
    cli.agent_root.join("ui-certs")
}

fn ui_ca_dir(cli: &Cli) -> PathBuf {
    cli.agent_root.join("ui-ca")
}

/// Writes `node.crt` and `node.key` by rename, keeping the previous pair as
/// `*.prev`, so a crash never leaves a half-written certificate.
fn write_ui_cert_files(dir: &Path, pem: &uicert::Pem) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    // Key first: a crash between the renames then leaves the old cert with
    // the new key, which fails to load and falls back, never the reverse
    // half-state that looks installed.
    for (name, contents) in [("node.key", &pem.key), ("node.crt", &pem.cert)] {
        let path = dir.join(name);
        if path.exists() {
            fs::copy(&path, dir.join(format!("{name}.prev")))?;
        }
        let tmp = dir.join(format!(".{name}.tmp"));
        let _ = fs::remove_file(&tmp);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, &path)?;
    }
    Ok(())
}

/// Fetches the release list when the cached one is due. A fetch failure is
/// recorded on the catalog and never fails the tick: the old list stays.
fn refresh_catalog_if_due(cli: &Cli, client: &mut Client) -> Result<(), AppError> {
    let catalog = releases::load_catalog(cli, client)?;
    let due = releases::refresh_due(
        catalog
            .as_ref()
            .and_then(|catalog| catalog.fetched_age_secs),
        catalog
            .as_ref()
            .and_then(|catalog| catalog.refresh_requested_age_secs),
    );
    if !due {
        return Ok(());
    }
    match available_cockroach_versions(&cli.github_api_url) {
        Ok(versions) => {
            releases::store_catalog(cli, client, &versions)?;
            audit(
                cli,
                "release_catalog_fetched",
                &format!("versions={}", versions.len()),
            )
        }
        Err(error) => {
            let error = error.to_string();
            releases::store_catalog_error(cli, client, &error)?;
            audit(cli, "release_catalog_failed", &error)
        }
    }
}

fn leader_reconcile(
    cli: &Cli,
    client: &mut Client,
    auto_finalize: bool,
    dry_run: bool,
) -> Result<(), AppError> {
    let open = open_rollout(cli, client)?;
    let nodes = cluster_nodes(client)?;
    if let Some(rollout) = open
        .as_ref()
        .filter(|rollout| rollout.kind == RolloutKind::Restart)
    {
        return leader_restart(cli, client, rollout, &nodes, dry_run);
    }
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
            events::note(
                cli,
                client,
                Source::Agent,
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

/// A restart has nothing to propose or finalize; it ends once every live
/// node's agent has recorded it.
fn leader_restart(
    cli: &Cli,
    client: &mut Client,
    rollout: &OpenRollout,
    nodes: &[NodeObservation],
    dry_run: bool,
) -> Result<(), AppError> {
    if rollout.status != RolloutStatus::Active {
        return Ok(());
    }
    let rows = cluster_view::agent_rows(cli, client)?;
    let lease_seconds = cli.restart_timeout_seconds + INSTALL_LEASE_EXTRA_SECONDS;
    if let Some(stalled) = cluster_view::stalled_restart(&rows, &rollout.id, lease_seconds) {
        let reason = format!(
            "{}: node did not come back from its restart within {lease_seconds}s",
            stalled.observation.agent_id
        );
        if dry_run {
            println!("dry run: would fail restart {}: {reason}", rollout.id);
            return Ok(());
        }
        if fail_rollout_if_active(cli, client, &rollout.id, &reason)? {
            events::note(cli, client, Source::Agent, "restart_failed", &reason)?;
        }
        return Ok(());
    }
    let agents: Vec<_> = rows.into_iter().map(|row| row.observation).collect();
    let complete = restart_complete(&agents, nodes, &rollout.id);
    if dry_run {
        println!("dry run: restart {} complete={complete}", rollout.id);
        return Ok(());
    }
    if complete {
        set_rollout_status(cli, client, &rollout.id, RolloutStatus::Finalized, None)?;
        events::note(
            cli,
            client,
            Source::Agent,
            "restart_complete",
            &format!(
                "version={} nodes={}",
                rollout.manifest.target_version,
                nodes.len()
            ),
        )?;
    }
    Ok(())
}

/// Records the next release-line step as a `proposed` rollout. Nothing
/// installs until an operator runs `approve`.
fn propose_next_rollout(
    cli: &Cli,
    client: &mut Client,
    nodes: &[NodeObservation],
) -> Result<(), AppError> {
    let (oldest, newest) = match live_version_span(nodes) {
        Ok(span) => span,
        Err(reason) => return note_skip(cli, client, &reason),
    };
    let Some(catalog) =
        releases::load_catalog(cli, client)?.filter(|catalog| !catalog.versions.is_empty())
    else {
        return note_skip(cli, client, "the release catalog has not been fetched yet");
    };
    let Some(plan) = plan_from_versions(cli, &oldest, None, catalog.versions)? else {
        if newest != oldest {
            return note_skip(
                cli,
                client,
                &format!("nodes run {oldest} and {newest}, but {oldest} needs no upgrade step"),
            );
        }
        return Ok(());
    };
    // A partially applied upgrade is resumed only when the nodes already
    // ahead run exactly this step's target; they then count as done.
    if newest != oldest && plan.next_version != newest {
        return note_skip(
            cli,
            client,
            &format!(
                "nodes run {oldest} and {newest}, but the next step from {oldest} is {}; finish or revert the partial upgrade by hand",
                plan.next_version
            ),
        );
    }
    if proposal_declined(&plan.next_version, &ops::declined_targets(cli, client)?) {
        return note_skip(
            cli,
            client,
            &format!(
                "{oldest} -> {} was declined; undo with: cockroach-rollout-agent undecline {}",
                plan.next_version, plan.next_version
            ),
        );
    }
    let plan = with_release_notes(plan)?;
    let mut manifest = create_manifest_for_plan(cli, plan)?;
    let report = settings_report(cli, client, &manifest).unwrap_or_else(SettingsReport::failed);
    manifest.settings_report = Some(report);
    publish_rollout(cli, client, &manifest)
}

/// Diffs the cluster's overridden settings against the target's defaults.
/// Runs the downloaded target binary from a scratch dir in the versions dir,
/// which the unit's sandbox lets the agent execute from.
fn settings_report(
    cli: &Cli,
    client: &mut Client,
    manifest: &RolloutManifest,
) -> Result<SettingsReport, String> {
    let overrides = settings::load_overrides(client).map_err(|error| error.to_string())?;
    let installed = cockroach_binary(cli);
    let installed_version =
        installed_cockroach_version(&installed).map_err(|error| error.to_string())?;
    if installed_version != manifest.current_version {
        return Err(format!(
            "this leader runs {installed_version}, not the rollout's starting version {}",
            manifest.current_version
        ));
    }
    let current = settings::gen_settings(&installed)?;

    let arch = normalized_arch();
    let artifact = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.arch == arch)
        .ok_or_else(|| format!("the manifest has no {arch} artifact"))?;
    let versions_dir = BinaryLayout::new(&cli.agent_root).versions_dir();
    let scratch = tempfile::Builder::new()
        .prefix(".settings-diff-")
        .tempdir_in(&versions_dir)
        .map_err(|error| {
            format!(
                "cannot make a scratch dir in {}: {error}",
                versions_dir.display()
            )
        })?;
    run_command(
        "tar",
        [
            "-xzf".as_ref(),
            Path::new(&artifact.path).as_os_str(),
            "-C".as_ref(),
            scratch.path().as_os_str(),
        ],
    )
    .map_err(|error| error.to_string())?;
    let binary = find_cockroach_binary(scratch.path()).map_err(|error| error.to_string())?;
    let target = settings::gen_settings(&binary)?;
    Ok(settings::diff(&overrides, &current, &target))
}

/// Records why no proposal was made, once per distinct reason: the leader
/// re-checks every tick, and the reason rarely changes between ticks.
fn note_skip(cli: &Cli, client: &mut Client, detail: &str) -> Result<(), AppError> {
    let last = events::last_detail(cli, client, "proposal_skipped")?;
    if should_note(last.as_deref(), &sanitize_log_field(detail)) {
        events::note(cli, client, Source::Agent, "proposal_skipped", detail)?;
    }
    Ok(())
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
    if rollout.kind == RolloutKind::Restart {
        return follower_restart(cli, client, agent_id, &rollout, dry_run);
    }
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

    if let Err(reason) = health_verdict(client) {
        record_agent_state(
            cli,
            client,
            agent_id,
            "waiting_for_health",
            &running,
            &rollout.id,
            Some(&reason),
        )?;
        events::note(cli, client, Source::Agent, "gate_wait", &reason)?;
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
            events::note(
                cli,
                client,
                Source::Agent,
                "upgraded",
                &format!(
                    "{} -> {}",
                    manifest.current_version, manifest.target_version
                ),
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
            events::note(cli, client, Source::Agent, "upgrade_failed", &message)?;
            release_lease(cli, client, INSTALL_LEASE, agent_id)?;
            Err(error)
        }
    }
}

/// Restarts this node once per restart rollout, under the install lease and
/// health gate. Nothing is swapped, so failure has nothing to roll back: the
/// rollout halts as `failed` for an operator, like a failed upgrade.
fn follower_restart(
    cli: &Cli,
    client: &mut Client,
    agent_id: &str,
    rollout: &OpenRollout,
    dry_run: bool,
) -> Result<(), AppError> {
    let restarted = restarted_rollout_id(cli, client, agent_id)?;
    if restart_action(&rollout.id, restarted.as_deref()) == RestartAction::AlreadyRestarted {
        if !dry_run {
            release_lease(cli, client, INSTALL_LEASE, agent_id)?;
        }
        return Ok(());
    }
    let running = own_node_version(client)?;
    if dry_run {
        println!("dry run: would take the install lease and restart this node on {running}");
        return Ok(());
    }

    let service = service_name(cli)?;
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
    if let Err(reason) = health_verdict(client) {
        record_agent_state(
            cli,
            client,
            agent_id,
            "waiting_for_health",
            &running,
            &rollout.id,
            Some(&reason),
        )?;
        events::note(cli, client, Source::Agent, "gate_wait", &reason)?;
        return release_lease(cli, client, INSTALL_LEASE, agent_id);
    }

    record_agent_state(
        cli,
        client,
        agent_id,
        "restarting",
        &running,
        &rollout.id,
        None,
    )?;
    let started_before = own_started_at(client)?;
    events::note(
        cli,
        client,
        Source::Agent,
        "restart_begin",
        &format!("service={service} version={running}"),
    )?;
    let timeout = Duration::from_secs(cli.restart_timeout_seconds);
    let outcome = run_systemctl("restart", service)
        .and_then(|()| wait_for_restart(cli, &running, started_before, timeout));

    // The node restarted, so the old connection is gone. The agent's DB is
    // its own node, so wait for it however long it takes: giving up here
    // would leave the outcome unrecorded. The leader marks the rollout
    // failed meanwhile if this outlives the install lease.
    *client = reconnect_patiently(cli);
    match outcome {
        Ok(()) => {
            mark_restarted(cli, client, agent_id, &rollout.id)?;
            record_agent_state(
                cli,
                client,
                agent_id,
                "complete",
                &running,
                &rollout.id,
                None,
            )?;
            events::note(
                cli,
                client,
                Source::Agent,
                "restarted",
                &format!("version={running}"),
            )?;
            release_lease(cli, client, INSTALL_LEASE, agent_id)
        }
        Err(error) => {
            let message = error.to_string();
            fail_rollout_if_active(cli, client, &rollout.id, &format!("{agent_id}: {message}"))?;
            record_agent_state(
                cli,
                client,
                agent_id,
                "failed",
                &running,
                &rollout.id,
                Some(&message),
            )?;
            events::note(cli, client, Source::Agent, "restart_failed", &message)?;
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
            if active && installed_cockroach_version(&cockroach_binary(self.cli))? == *expected {
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
    let service = service_name(cli)?;
    layout
        .verify_wired(&service_exec_path(service).map_err(AppError::Message)?)
        .map_err(AppError::Message)?;
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

/// Like `reconnect`, but never gives up; logs every minute while waiting.
fn reconnect_patiently(cli: &Cli) -> Client {
    let mut attempts: u64 = 0;
    loop {
        match db_client(cli) {
            Ok(client) => return client,
            Err(error) => {
                if attempts.is_multiple_of(60 / HEALTH_POLL_SECONDS) {
                    eprintln!("waiting for the local node to accept SQL again: {error}");
                }
                attempts += 1;
                thread::sleep(Duration::from_secs(HEALTH_POLL_SECONDS));
            }
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
    let mut client = db_client(cli)?;
    ensure_schema(cli, &mut client)?;
    println!(
        "{}",
        ops::approve(
            cli,
            &mut client,
            &Actor::Cli,
            version,
            accept_release_note_warnings
        )?
    );
    Ok(())
}

/// Runs one operator action against a fresh connection and prints its message.
fn with_client(
    cli: &Cli,
    action: impl FnOnce(&mut Client) -> Result<String, AppError>,
) -> Result<(), AppError> {
    let mut client = db_client(cli)?;
    ensure_schema(cli, &mut client)?;
    println!("{}", action(&mut client)?);
    Ok(())
}

fn cancel_command(cli: &Cli, reason: &str) -> Result<(), AppError> {
    let mut client = db_client(cli)?;
    ensure_schema(cli, &mut client)?;
    println!("{}", ops::cancel(cli, &mut client, &Actor::Cli, reason)?);
    Ok(())
}

fn restart_cluster_command(cli: &Cli) -> Result<(), AppError> {
    let mut client = db_client(cli)?;
    ensure_schema(cli, &mut client)?;
    println!("{}", ops::restart_cluster(cli, &mut client, &Actor::Cli)?);
    Ok(())
}

fn status_command(cli: &Cli) -> Result<(), AppError> {
    let mut client = db_client(cli)?;
    ensure_schema(cli, &mut client)?;
    let view = cluster_view::load(cli, &mut client)?;

    println!("ROLLOUTS (newest first)");
    for rollout in view.rollouts.iter().take(5) {
        println!(
            "  {:<10} {:<7} {} -> {}  proposed by {} at {}",
            rollout.status.as_str(),
            rollout.kind.as_str(),
            rollout.from,
            rollout.to,
            rollout.created_by,
            rollout.created_at
        );
        if let Some(approver) = &rollout.approved_by {
            println!("             approved by {approver}");
        }
        if let Some(error) = &rollout.error {
            println!("             error: {error}");
        }
    }

    println!("\nNODES");
    for node in &view.nodes {
        println!(
            "  n{:<3} live={:<5} version={}",
            node.node_id,
            node.is_live,
            node.version
                .as_ref()
                .map(|version| version.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        );
    }

    println!("\nAGENTS");
    for agent in &view.agents {
        let freshness = if agent.is_stale(view.stale_after_secs) {
            format!("STALE ({}s)", agent.age_secs)
        } else {
            format!("{}s ago", agent.age_secs)
        };
        println!(
            "  {:<28} {:<18} {:<10} {}",
            agent.observation.agent_id, agent.state, agent.version, freshness
        );
        if let Some(error) = &agent.error {
            println!("  {:<28} {error}", "");
        }
    }

    println!("\nLEASES");
    for lease in &view.leases {
        println!(
            "  {:<8} {}",
            lease.name,
            lease.holder.as_deref().unwrap_or("free")
        );
    }

    match &view.health {
        Ok(health) => println!(
            "\nHEALTH\n  {}/{} nodes live, {} under-replicated, {} unavailable ranges; gate: {}",
            health.live_nodes,
            health.active_nodes,
            health.underreplicated_ranges,
            health.unavailable_ranges,
            safe_to_restart(health)
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
    let mut client = Client::connect(database_url, MakeTlsConnector::new(tls))?;
    allow_internals(&mut client)?;
    Ok(client)
}

/// CockroachDB 26.1 refuses `crdb_internal` and `system` reads unless the
/// session opts in, and the agent reads `crdb_internal.gossip_nodes` to see
/// each node's liveness and build. Without this an upgraded node never looks
/// healthy and every 26.1 install rolls back. Older releases that do not know
/// the variable already allow the reads.
fn allow_internals(client: &mut Client) -> Result<(), AppError> {
    match client.batch_execute("SET allow_unsafe_internals = true") {
        Ok(()) => Ok(()),
        Err(error) if unknown_session_variable(&error.to_string()) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn unknown_session_variable(message: &str) -> bool {
    message.contains("unrecognized configuration parameter")
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
        format!(
            "ALTER TABLE {schema}.rollouts ADD COLUMN IF NOT EXISTS kind STRING NOT NULL DEFAULT 'upgrade'"
        ),
        format!(
            "ALTER TABLE {schema}.agent_status ADD COLUMN IF NOT EXISTS restarted_rollout_id STRING NULL"
        ),
        format!("ALTER TABLE {schema}.agent_status ADD COLUMN IF NOT EXISTS node_id INT8 NULL"),
        format!(
            "ALTER TABLE {schema}.agent_status ADD COLUMN IF NOT EXISTS agent_version STRING NULL"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {schema}.ui_auth (
                id INT8 PRIMARY KEY DEFAULT 1 CHECK (id = 1),
                password_hash STRING NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {schema}.ui_sessions (
                token_sha256 STRING PRIMARY KEY,
                csrf_token STRING NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                expires_at TIMESTAMPTZ NOT NULL,
                client_ip STRING NOT NULL
            )"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {schema}.ui_certs (
                fingerprint STRING PRIMARY KEY,
                cert_pem STRING NOT NULL,
                key_pem STRING NOT NULL,
                chain_pem STRING NOT NULL,
                status STRING NOT NULL,
                not_before INT8 NOT NULL,
                not_after INT8 NOT NULL,
                created_by STRING NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                decided_by STRING NULL,
                decided_at TIMESTAMPTZ NULL
            )"
        ),
        format!(
            "ALTER TABLE {schema}.agent_status ADD COLUMN IF NOT EXISTS ui_cert_fingerprint STRING NULL"
        ),
        format!(
            "ALTER TABLE {schema}.agent_status ADD COLUMN IF NOT EXISTS ui_cert_error STRING NULL"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {schema}.release_catalog (
                id INT8 PRIMARY KEY DEFAULT 1 CHECK (id = 1),
                versions_json STRING NULL,
                fetched_at TIMESTAMPTZ NULL,
                refresh_requested_at TIMESTAMPTZ NULL,
                last_error STRING NULL
            )"
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {schema}.events (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                at TIMESTAMPTZ NOT NULL DEFAULT now(),
                agent_id STRING NOT NULL,
                source STRING NOT NULL,
                kind STRING NOT NULL,
                detail STRING NOT NULL
            )"
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

/// This node's process start time in microseconds, from gossip. Changes on
/// every restart, so it tells a fresh observation from a stale one.
fn own_started_at(client: &mut Client) -> Result<i64, AppError> {
    let row = client.query_one(
        "
        SELECT (extract(epoch FROM started_at) * 1000000)::INT8
        FROM crdb_internal.gossip_nodes
        WHERE node_id = crdb_internal.node_id()
        ",
        &[],
    )?;
    Ok(row.get(0))
}

fn restarted_rollout_id(
    cli: &Cli,
    client: &mut Client,
    agent_id: &str,
) -> Result<Option<String>, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let rows = client.query(
        &format!("SELECT restarted_rollout_id FROM {schema}.agent_status WHERE agent_id = $1"),
        &[&agent_id],
    )?;
    Ok(rows.first().and_then(|row| row.get::<_, Option<String>>(0)))
}

fn mark_restarted(
    cli: &Cli,
    client: &mut Client,
    agent_id: &str,
    rollout_id: &str,
) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!("UPDATE {schema}.agent_status SET restarted_rollout_id = $2 WHERE agent_id = $1"),
        &[&agent_id, &rollout_id],
    )?;
    Ok(())
}

/// Waits until this node is live on `expected` with a start time newer than
/// `started_before`. Reconnects each poll because the node went away.
fn wait_for_restart(
    cli: &Cli,
    expected: &Version,
    started_before: i64,
    timeout: Duration,
) -> Result<(), AppError> {
    let deadline = SystemTime::now() + timeout;
    let mut last = String::from("no observation yet");
    while SystemTime::now() < deadline {
        thread::sleep(Duration::from_secs(HEALTH_POLL_SECONDS));
        let observed = db_client(cli).and_then(|mut client| {
            Ok((
                own_node_observation(&mut client)?,
                own_started_at(&mut client)?,
            ))
        });
        match observed {
            Ok((node, started)) if restarted_on(expected, started_before, &node, started) => {
                return Ok(());
            }
            Ok((node, started)) => {
                last = format!(
                    "node {} live={} version={} started_at_us={started} (before {started_before})",
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
        "node did not restart on {expected} within {}s; last: {last}",
        timeout.as_secs()
    )))
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

/// Fail closed: if health cannot be measured, it is not known to be safe.
fn health_verdict(client: &mut Client) -> Result<(), String> {
    cluster_health(client)
        .map_err(|error| format!("could not measure cluster health: {error}"))
        .and_then(|health| safe_to_restart(&health))
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
    ui_cert_fingerprint: Option<&str>,
) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!(
            "
            INSERT INTO {schema}.agent_status
                (agent_id, state, version, updated_at, last_seen, node_id, agent_version,
                 ui_cert_fingerprint)
            VALUES ($1, 'idle', $2, now(), now(), crdb_internal.node_id(), $3, $4)
            ON CONFLICT (agent_id) DO UPDATE
            SET version = excluded.version,
                last_seen = now(),
                node_id = excluded.node_id,
                agent_version = excluded.agent_version,
                ui_cert_fingerprint = excluded.ui_cert_fingerprint
            "
        ),
        &[
            &agent_id,
            &version.to_string(),
            &env!("CARGO_PKG_VERSION"),
            &ui_cert_fingerprint,
        ],
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
    kind: RolloutKind,
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
            SELECT id::STRING, kind, status, manifest_json
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
    let kind: String = row.get(1);
    let status: String = row.get(2);
    let manifest_json: String = row.get(3);
    Ok(Some(OpenRollout {
        id: row.get(0),
        kind: kind.parse().map_err(AppError::Message)?,
        status: status.parse().map_err(AppError::Message)?,
        manifest: serde_json::from_str(&manifest_json)?,
    }))
}

/// Marks `id` failed only while it is still active, so a late failure never
/// reopens a rollout an operator cancelled or a leader already failed.
/// Returns whether this call made the change.
fn fail_rollout_if_active(
    cli: &Cli,
    client: &mut Client,
    id: &str,
    error: &str,
) -> Result<bool, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let updated = client.execute(
        &format!(
            "UPDATE {schema}.rollouts SET status = 'failed', error = $2 \
             WHERE id::STRING = $1 AND status = 'active'"
        ),
        &[&id, &error],
    )?;
    Ok(updated == 1)
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
        settings_report: None,
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
    let inserted = client.execute(
        &format!(
            "
            INSERT INTO {schema}.rollouts
                (kind, status, current_version, target_version, manifest_json, created_by)
            SELECT 'upgrade', 'proposed', $1, $2, $3, $4
            WHERE NOT EXISTS (
                SELECT 1 FROM {schema}.rollouts WHERE status IN ('proposed', 'active', 'failed')
            )
            "
        ),
        &[
            &manifest.current_version.to_string(),
            &manifest.target_version.to_string(),
            &manifest_json,
            &agent_id,
        ],
    )?;
    if inserted != 1 {
        return events::note(
            cli,
            client,
            Source::Agent,
            "proposal_skipped",
            "another rollout opened while this proposal was being prepared",
        );
    }
    events::note(
        cli,
        client,
        Source::Agent,
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
    let mut client = db_client(cli)?;
    ensure_schema(cli, &mut client)?;
    println!(
        "{}",
        ops::finalize(cli, &mut client, &Actor::Cli, target_version, dry_run)?
    );
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
    events::note(
        cli,
        client,
        Source::Agent,
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

    for command in ["tar", "systemctl"] {
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
    if let Ok(service) = &service {
        check(
            "binary layout",
            service_exec_path(service).and_then(|exec_path| {
                layout
                    .verify_wired(&exec_path)
                    .map(|()| format!("{service} runs {}", layout.current_link().display()))
            }),
        );
    }
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
        installed_cockroach_version(&cockroach_binary(cli))
            .map(|version| version.to_string())
            .map_err(|error| error.to_string()),
    );

    if let Ok(service) = &service {
        check("restart authorization", polkit_can_restart(service));
    }
    match ui_listen_addr(&cli.ui_listen) {
        Ok(None) => check(
            "ui tls",
            Ok("UI disabled (CROACH_ROLLOUT_UI_LISTEN=off)".to_string()),
        ),
        Ok(Some(listen)) => check(
            "ui tls",
            ui_tls(cli).map(|(dir, _)| format!("{listen} with {}/node.crt", dir.display())),
        ),
        Err(reason) => check("ui tls", Err(reason)),
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

fn ui_listen_addr(value: &str) -> Result<Option<SocketAddr>, String> {
    if value.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    value.parse().map(Some).map_err(|error| {
        format!("CROACH_ROLLOUT_UI_LISTEN={value} is neither an address nor off: {error}")
    })
}

/// The UI's certs dir and TLS config, the same resolution `self-check` reports.
fn ui_tls(cli: &Cli) -> Result<(PathBuf, rustls::ServerConfig), String> {
    choose_ui_tls(
        cli.ui_certs_dir.as_deref(),
        &managed_ui_certs_dir(cli),
        || web::tls::resolve_certs_dir(None, service_name(cli).map_err(|error| error.to_string())?),
    )
    .map(|(dir, config, _)| (dir, config))
}

/// The override, else an installed UI certificate, else the CockroachDB
/// node certificate from `fallback`. An installed pair that does not load
/// is skipped rather than disabling the UI: the agent then sees it is not
/// serving the approved certificate and installs it again. The flag says
/// whether the installed pair was used.
fn choose_ui_tls(
    override_dir: Option<&Path>,
    managed: &Path,
    fallback: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<(PathBuf, rustls::ServerConfig, bool), String> {
    if let Some(dir) = override_dir {
        let config = web::tls::load_server_config(dir)?;
        return Ok((dir.to_path_buf(), config, false));
    }
    if managed.join("node.crt").is_file() {
        match web::tls::load_server_config(managed) {
            Ok(config) => return Ok((managed.to_path_buf(), config, true)),
            // The approved certificate is installed again on the next tick,
            // and any failure then is shown on the dashboard.
            Err(_) => {
                eprintln!("ui: the installed UI certificate does not load; using the fallback")
            }
        }
    }
    let dir = fallback()?;
    let config = web::tls::load_server_config(&dir)?;
    Ok((dir, config, false))
}

/// Starts the UI thread. Every failure leaves the UI off and the daemon
/// running: rollouts never depend on the UI, and it never falls back to HTTP.
fn start_web_ui(cli: &Cli) -> Result<Option<Arc<web::UiServer>>, AppError> {
    let disabled = |reason: &str| -> Result<Option<Arc<web::UiServer>>, AppError> {
        eprintln!("ui disabled: {reason}");
        audit(cli, "ui_disabled", reason)?;
        Ok(None)
    };
    let listen = match ui_listen_addr(&cli.ui_listen) {
        Ok(Some(listen)) => listen,
        Ok(None) => {
            audit(cli, "ui_disabled", "CROACH_ROLLOUT_UI_LISTEN=off")?;
            return Ok(None);
        }
        Err(reason) => return disabled(&reason),
    };
    let chosen = choose_ui_tls(
        cli.ui_certs_dir.as_deref(),
        &managed_ui_certs_dir(cli),
        || web::tls::resolve_certs_dir(None, service_name(cli).map_err(|error| error.to_string())?),
    );
    let (dir, tls, is_managed) = match chosen {
        Ok(found) => found,
        Err(reason) => return disabled(&reason),
    };
    // Only an installed UI certificate counts as served; a node certificate
    // reports none, so the dashboard says "node certificate".
    let served = is_managed
        .then(|| fs::read_to_string(dir.join("node.crt")).ok())
        .flatten()
        .and_then(|pem| uicert::inspect(&pem).ok())
        .map(|info| info.fingerprint);
    let server = Arc::new(web::UiServer::new(tls, served));
    let state = web::routes::AppState::new(
        Arc::new(web::db::DbBackend::new(cli.clone())),
        Duration::from_secs(cli.ui_session_hours.saturating_mul(3600)),
    );
    match web::spawn(listen, state, server.clone()) {
        Ok(_) => {
            audit(
                cli,
                "ui_started",
                &format!("listen={listen} certs={}", dir.display()),
            )?;
            Ok(Some(server))
        }
        Err(error) => disabled(&format!("cannot listen on {listen}: {error}")),
    }
}

fn ui_cert_command(cli: &Cli, action: &UiCertCommand) -> Result<(), AppError> {
    // The daily timer runs renew on every node; only the CA node does work,
    // and the rest stop here without touching the database.
    let renew_ca = match action {
        UiCertCommand::Renew { .. } => {
            match read_ui_ca(&ui_ca_dir(cli)).map_err(AppError::Message)? {
                Some(ca) => Some(ca),
                None => {
                    println!(
                        "no UI CA key in {}; nothing to do on this node",
                        ui_ca_dir(cli).display()
                    );
                    return Ok(());
                }
            }
        }
        _ => None,
    };
    let mut client = db_client(cli)?;
    ensure_schema(cli, &mut client)?;
    let client = &mut client;
    let now = unix_time()? as i64;
    let by = Actor::Cli.label(cli);
    match action {
        UiCertCommand::InitCa { extra_name } => {
            let dir = ui_ca_dir(cli);
            if dir.join("ca.key").exists() {
                return Err(AppError::Message(format!(
                    "{} already holds a UI CA; remove it by hand to start over",
                    dir.display()
                )));
            }
            let agent_ids: Vec<String> = cluster_view::agent_rows(cli, client)?
                .into_iter()
                .map(|row| row.observation.agent_id)
                .collect();
            let addresses: Vec<String> = discover_nodes(client)?
                .into_iter()
                .map(|node| node.address)
                .collect();
            let hosts = uicert::hosts_from(&agent_ids, &addresses, extra_name);
            let ca = uicert::generate_ca(&hosts, time::OffsetDateTime::now_utc())
                .map_err(AppError::Message)?;
            write_private_files(&dir, &[("ca.crt", &ca.cert), ("ca.key", &ca.key)])?;
            let info = uicert::inspect(&ca.cert).map_err(AppError::Message)?;
            events::note(
                cli,
                client,
                Source::Cli,
                "ui_ca_created",
                &format!(
                    "fingerprint={} hosts={} by={by}",
                    info.fingerprint,
                    hosts.describe()
                ),
            )?;
            println!(
                "created the Rollout UI CA in {}\n  may sign for: {}\n  sha256: {}\n\
                 next: cockroach-rollout-agent ui-cert renew",
                dir.display(),
                hosts.describe(),
                info.fingerprint
            );
        }
        UiCertCommand::Renew { before_days, days } => {
            let ca = renew_ca.expect("renew read the CA before connecting");
            let holder = agent_node_id(cli)?;
            let lease = acquire_lease(cli, client, UI_CERT_RENEW_LEASE, &holder, 300)?;
            if !lease.is_leader {
                println!("{} is renewing; nothing to do", lease.holder_id);
                return Ok(());
            }
            if let Some(staged) = uicert::material(cli, client, None, "staged")? {
                println!(
                    "certificate {} is staged and waiting for approval; nothing to do",
                    staged.fingerprint
                );
                return Ok(());
            }
            if let Some(approved) = uicert::material(cli, client, None, "approved")? {
                let info = uicert::inspect(&approved.pem.cert).map_err(AppError::Message)?;
                if !uicert::renew_due(info.not_after, now, *before_days) {
                    println!(
                        "the approved certificate is good for {} more days; nothing to do",
                        (info.not_after - now) / 86_400
                    );
                    return Ok(());
                }
            }
            let hosts = uicert::ca_hosts(&ca.cert).map_err(AppError::Message)?;
            let leaf = uicert::issue_leaf(&ca, &hosts, time::OffsetDateTime::now_utc(), *days)
                .map_err(AppError::Message)?;
            let info = uicert::stage(cli, client, &leaf, &ca.cert, &holder, now)?;
            events::note(
                cli,
                client,
                Source::Cli,
                "ui_cert_staged",
                &format!("fingerprint={} by={holder}", info.fingerprint),
            )?;
            println!(
                "staged certificate {} for {}; approve it in the UI or with: \
                 cockroach-rollout-agent ui-cert approve {}",
                info.fingerprint,
                info.hosts().describe(),
                info.fingerprint
            );
        }
        UiCertCommand::Stage { cert, key } => {
            let pem = uicert::Pem {
                cert: fs::read_to_string(cert)?,
                key: fs::read_to_string(key)?,
            };
            let chain = ui_ca_for_staging(cli, client)?;
            let info = uicert::stage(cli, client, &pem, &chain, &by, now)?;
            events::note(
                cli,
                client,
                Source::Cli,
                "ui_cert_uploaded",
                &format!("fingerprint={} by={by}", info.fingerprint),
            )?;
            println!("staged certificate {}", info.fingerprint);
        }
        UiCertCommand::Approve { fingerprint } | UiCertCommand::Reject { fingerprint } => {
            let approve = matches!(action, UiCertCommand::Approve { .. });
            ops::decide_ui_cert(cli, client, &Actor::Cli, fingerprint, approve)?;
            println!(
                "{}",
                if approve {
                    "approved; every agent installs it on its next tick"
                } else {
                    "rejected"
                }
            );
        }
        UiCertCommand::Show => {
            let rows = uicert::rows(cli, client)?;
            if rows.is_empty() {
                println!("no UI certificate is approved or staged");
            }
            for row in rows {
                println!(
                    "{} {}\n  names: {}\n  valid until: {}\n  staged by {} at {}",
                    row.status,
                    row.info.fingerprint,
                    row.info.hosts().describe(),
                    row.info.not_after,
                    row.created_by,
                    row.created_at
                );
            }
        }
    }
    Ok(())
}

/// This node's UI CA, `None` when it has none. Any other read failure is
/// an error, so a key the agent cannot read never looks like "not the CA
/// node" and lets the certificate expire unnoticed.
fn read_ui_ca(dir: &Path) -> Result<Option<uicert::Pem>, String> {
    let read = |name: &str| match fs::read_to_string(dir.join(name)) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", dir.join(name).display())),
    };
    match (read("ca.crt")?, read("ca.key")?) {
        (Some(cert), Some(key)) => Ok(Some(uicert::Pem { cert, key })),
        (None, None) => Ok(None),
        _ => Err(format!("{} holds only half of the UI CA", dir.display())),
    }
}

/// The CA an uploaded certificate must chain to: this node's, else the one
/// the approved certificate chains to.
fn ui_ca_for_staging(cli: &Cli, client: &mut Client) -> Result<String, AppError> {
    if let Ok(cert) = fs::read_to_string(ui_ca_dir(cli).join("ca.crt")) {
        return Ok(cert);
    }
    uicert::material(cli, client, None, "approved")?
        .map(|material| material.chain_pem)
        .ok_or_else(|| {
            AppError::Message(
                "no UI CA is known yet: run ui-cert init-ca on one node first".to_string(),
            )
        })
}

/// Creates `dir` (0700) and each file (0600), refusing to overwrite.
fn write_private_files(dir: &Path, files: &[(&str, &String)]) -> Result<(), AppError> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    for (name, contents) in files {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(name))?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    Ok(())
}

fn set_ui_password_command(cli: &Cli) -> Result<(), AppError> {
    let password = read_new_password()?;
    let hash = web::auth::hash_password(&password).map_err(AppError::Message)?;
    let mut client = db_client(cli)?;
    ensure_schema(cli, &mut client)?;
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!(
            "UPSERT INTO {schema}.ui_auth (id, password_hash, updated_at) VALUES (1, $1, now())"
        ),
        &[&hash],
    )?;
    let cleared = client.execute(&format!("DELETE FROM {schema}.ui_sessions"), &[])?;
    events::note(
        cli,
        &mut client,
        Source::Cli,
        "ui_password_set",
        &format!("by={} sessions_cleared={cleared}", Actor::Cli.label(cli)),
    )?;
    println!("UI password set; {cleared} existing session(s) signed out");
    Ok(())
}

fn read_new_password() -> Result<String, AppError> {
    if io::stdin().is_terminal() {
        let first = rpassword::prompt_password("New UI password: ")?;
        let second = rpassword::prompt_password("Repeat it: ")?;
        if first != second {
            return Err(AppError::Message("the passwords do not match".to_string()));
        }
        return Ok(first);
    }
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Proves polkit lets this process manage `service`, without restarting it.
///
/// `pkcheck --detail` is reserved for root, so an unprivileged probe cannot
/// pass the `unit` and `verb` details the rule matches on. Instead this asks
/// systemd itself to `reset-failed` the unit: systemd consults polkit with
/// the real details (verb `reset-failed`, which the rule allows alongside
/// start/stop/restart), and on a unit that has not failed it changes nothing.
fn polkit_can_restart(service: &str) -> Result<String, String> {
    let output = Command::new("systemctl")
        .args(["--no-ask-password", "reset-failed", service])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not run systemctl: {error}"))?;
    if output.status.success() {
        Ok(format!("polkit allows managing {service}"))
    } else {
        Err(format!(
            "polkit denies managing {service}; install the polkit rule for this unit ({})",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn available_cockroach_versions(github_api_url: &str) -> Result<Vec<Version>, AppError> {
    // One unpaginated call returns every `v*` tag; paging `/tags` needed ten
    // calls and silently lost releases past the first thousand tags.
    let url = releases::matching_refs_url(github_api_url).map_err(AppError::Message)?;
    let body = reqwest::blocking::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, "cockroach-rollout-agent")
        .send()?
        .error_for_status()?
        .text()?;
    let versions = releases::parse_matching_refs(&body).map_err(AppError::Message)?;
    if versions.is_empty() {
        return Err(AppError::Message(
            "GitHub listed no CockroachDB release tags".to_string(),
        ));
    }
    Ok(versions)
}

/// The binary the agent reads the installed version from.
fn cockroach_binary(cli: &Cli) -> PathBuf {
    cli.binary_path
        .clone()
        .unwrap_or_else(|| BinaryLayout::new(&cli.agent_root).current_link())
}

/// Returns the executable `service`'s `ExecStart` runs, as systemd will
/// start it after the next restart (drop-ins included).
fn service_exec_path(service: &str) -> Result<PathBuf, String> {
    let output = Command::new("systemctl")
        .args(["show", "--property=ExecStart", "--value", service])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not run systemctl show: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "systemctl show {service} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_exec_start_path(&String::from_utf8_lossy(&output.stdout))
        .map_err(|error| format!("{service}: {error}"))
}

/// Parses `systemctl show --property=ExecStart --value` output, which holds
/// one `{ path=... ; argv[]=... ; ... }` group per `ExecStart=` line.
fn parse_exec_start_path(output: &str) -> Result<PathBuf, String> {
    // Split on the group opener, not bare `path=`: arguments such as
    // `--store=path=/mnt/data` contain it too.
    let paths: Vec<&str> = output
        .split("{ path=")
        .skip(1)
        .filter_map(|rest| rest.split(" ;").next())
        .map(str::trim)
        .collect();
    match paths.as_slice() {
        [path] if !path.is_empty() => Ok(PathBuf::from(path)),
        [] | [_] => Err("has no ExecStart".to_string()),
        _ => Err(format!(
            "has {} ExecStart lines; expected exactly one",
            paths.len()
        )),
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
    fn write_pair(dir: &Path, pem: &uicert::Pem) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("node.crt"), &pem.cert).unwrap();
        fs::write(dir.join("node.key"), &pem.key).unwrap();
    }

    fn test_pair(name: &str) -> uicert::Pem {
        let cert = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
        uicert::Pem {
            cert: cert.cert.pem(),
            key: cert.signing_key.serialize_pem(),
        }
    }

    #[test]
    fn a_broken_installed_pair_falls_back_instead_of_disabling_the_ui() {
        let root = tempfile::tempdir().unwrap();
        let managed = root.path().join("ui-certs");
        let node = root.path().join("node-certs");
        write_pair(&node, &test_pair("node.test"));
        let fallback = || Ok(node.clone());

        let (dir, _, is_managed) = choose_ui_tls(None, &managed, fallback).unwrap();
        assert_eq!(
            (dir, is_managed),
            (node.clone(), false),
            "nothing installed"
        );

        let good = test_pair("ui.test");
        write_pair(&managed, &good);
        let (dir, _, is_managed) = choose_ui_tls(None, &managed, fallback).unwrap();
        assert_eq!((dir, is_managed), (managed.clone(), true));

        // A crash between the two renames leaves a new cert with the old key.
        fs::write(managed.join("node.key"), test_pair("other.test").key).unwrap();
        let (dir, _, is_managed) = choose_ui_tls(None, &managed, fallback).unwrap();
        assert_eq!(
            (dir, is_managed),
            (node.clone(), false),
            "a mismatched pair must not take the UI down"
        );
    }

    #[test]
    fn an_unreadable_ca_key_is_an_error_not_a_missing_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        assert!(read_ui_ca(dir.path()).unwrap().is_none(), "no CA here");
        fs::write(dir.path().join("ca.crt"), "c").unwrap();
        fs::write(dir.path().join("ca.key"), "k").unwrap();
        assert!(read_ui_ca(dir.path()).unwrap().is_some());
        fs::set_permissions(dir.path().join("ca.key"), fs::Permissions::from_mode(0o000)).unwrap();
        let error = read_ui_ca(dir.path()).unwrap_err();
        assert!(error.contains("ca.key"), "{error}");
    }

    #[test]
    fn ui_cert_files_are_replaced_atomically_keeping_the_previous_pair() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("ui-certs");
        let first = uicert::Pem {
            cert: "cert one".to_string(),
            key: "key one".to_string(),
        };
        write_ui_cert_files(&target, &first).unwrap();
        let second = uicert::Pem {
            cert: "cert two".to_string(),
            key: "key two".to_string(),
        };
        write_ui_cert_files(&target, &second).unwrap();
        assert_eq!(
            fs::read_to_string(target.join("node.crt")).unwrap(),
            "cert two"
        );
        assert_eq!(
            fs::read_to_string(target.join("node.key.prev")).unwrap(),
            "key one"
        );
        let mode = fs::metadata(target.join("node.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn only_an_unknown_variable_is_ignored_when_allowing_internals() {
        assert!(unknown_session_variable(
            "db error: ERROR: unrecognized configuration parameter \"allow_unsafe_internals\""
        ));
        assert!(!unknown_session_variable(
            "db error: ERROR: permission denied to set allow_unsafe_internals"
        ));
    }

    use super::*;

    #[test]
    fn ui_listen_accepts_an_address_or_off() {
        assert_eq!(ui_listen_addr("off"), Ok(None));
        assert_eq!(ui_listen_addr("OFF"), Ok(None));
        assert_eq!(
            ui_listen_addr("0.0.0.0:38090"),
            Ok(Some("0.0.0.0:38090".parse().unwrap()))
        );
        let error = ui_listen_addr("38090").unwrap_err();
        assert!(error.contains("CROACH_ROLLOUT_UI_LISTEN"), "{error}");
    }

    #[test]
    fn exec_start_path_is_parsed_from_systemctl_show() {
        let one = "{ path=/var/lib/cockroach-rollout-agent/bin/cockroach ; argv[]=/var/lib/cockroach-rollout-agent/bin/cockroach start --certs-dir=/certs ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }\n";
        assert_eq!(
            parse_exec_start_path(one).unwrap(),
            PathBuf::from("/var/lib/cockroach-rollout-agent/bin/cockroach")
        );
        assert!(parse_exec_start_path("").is_err(), "no ExecStart");
        let store = "{ path=/var/lib/cockroach-rollout-agent/bin/cockroach ; argv[]=/var/lib/cockroach-rollout-agent/bin/cockroach start --store=path=/var/lib/cockroach,attrs=ssd ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }\n";
        assert_eq!(
            parse_exec_start_path(store).unwrap(),
            PathBuf::from("/var/lib/cockroach-rollout-agent/bin/cockroach"),
            "--store=path= inside argv is not a second ExecStart"
        );
        let two = format!("{one}{one}");
        assert!(parse_exec_start_path(&two).is_err(), "two ExecStart lines");
    }

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
