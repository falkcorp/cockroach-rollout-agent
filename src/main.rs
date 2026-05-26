// file: src/main.rs
// version: 2.1.0
// guid: d16be11a-b10c-4d2e-853f-d4a1c0a3c617

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use native_tls::TlsConnector;
use postgres::Client;
use postgres_native_tls::MakeTlsConnector;
use regex::Regex;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const DEFAULT_BASE_URL: &str = "https://binaries.cockroachdb.com";
const DEFAULT_GITHUB_API_URL: &str =
    "https://api.github.com/repos/cockroachdb/cockroach/tags?per_page=100";
const DEFAULT_RELEASE_NOTES_BASE_URL: &str = "https://www.cockroachlabs.com/docs/releases";
const DEFAULT_ARTIFACTS_DIR: &str = "dist";
const DEFAULT_SERVICE_NAME: &str = "cockroachdb.service";
const DEFAULT_BINARY_PATH: &str = "/usr/local/bin/cockroach";
const DEFAULT_AUDIT_LOG: &str = "/var/log/cockroach-rollout-agent/audit.log";
const DEFAULT_MANIFEST_PATH: &str = "dist/manifest.json";
const DEFAULT_DAEMON_INTERVAL_SECONDS: u64 = 300;
const DEFAULT_LEASE_SECONDS: u64 = 90;
const DEFAULT_AGENT_STALE_SECONDS: u64 = 300;
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

    #[arg(long, env = "CROACH_ROLLOUT_SERVICE", default_value = DEFAULT_SERVICE_NAME)]
    service_name: String,

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

        #[arg(long)]
        dry_run: bool,

        /// Permit release-note warning matches when this daemon is elected leader.
        #[arg(long)]
        allow_breaking_warnings: bool,

        /// Finalize major-line upgrades after all discovered live nodes report the target binary.
        #[arg(long)]
        auto_finalize: bool,
    },

    /// Initialize the CockroachDB SQL coordination schema.
    InitDb,

    /// Print discovered CockroachDB nodes from crdb_internal.gossip_nodes.
    Discover,

    /// Finalize a major-version upgrade after every node is on the new binary.
    Finalize {
        /// Target CockroachDB version. Uses the major line, for example v25.4.3 finalizes 25.4.
        #[arg(long, env = "CROACH_ROLLOUT_TARGET_VERSION")]
        target_version: String,

        /// Print the SQL command without executing it.
        #[arg(long)]
        dry_run: bool,
    },

    /// Validate local commands and permissions.
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

#[derive(Debug, Deserialize)]
struct DbRolloutRow {
    manifest_json: String,
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
            allow_breaking_warnings,
            auto_finalize,
        } => daemon_command(
            &cli,
            manifest_url.as_deref(),
            manifest_file.as_deref(),
            *interval_seconds,
            *dry_run,
            *allow_breaking_warnings,
            *auto_finalize,
        ),
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
    let next_step = upgrade_steps
        .first()
        .cloned()
        .ok_or_else(|| AppError::Message("no upgrade step is required".to_string()))?;

    let release_notes = fetch_text(&next_step.release_notes_url)?;
    let warnings = scan_release_notes(&release_notes)?;

    Ok(UpgradePlan {
        current_version: current,
        requested_target_version: target,
        next_version: next_step.to_version.clone(),
        latest_version: latest,
        release_notes_url: next_step.release_notes_url.clone(),
        release_note_warnings: warnings,
        upgrade_steps,
        release_line_by_release_line: true,
    })
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

    install_artifact(cli, artifact_path)
}

fn daemon_command(
    cli: &Cli,
    manifest_url: Option<&str>,
    manifest_file: Option<&Path>,
    interval_seconds: u64,
    dry_run: bool,
    allow_breaking_warnings: bool,
    auto_finalize: bool,
) -> Result<(), AppError> {
    if cli.database_url.is_some() {
        return db_daemon_command(
            cli,
            interval_seconds,
            dry_run,
            allow_breaking_warnings,
            auto_finalize,
        );
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
    allow_breaking_warnings: bool,
    auto_finalize: bool,
) -> Result<(), AppError> {
    let node_id = agent_node_id(cli)?;
    audit(
        cli,
        "db_daemon_start",
        &format!("node_id={node_id} schema={}", cli.schema),
    )?;

    loop {
        if let Err(error) = db_daemon_tick(
            cli,
            &node_id,
            dry_run,
            allow_breaking_warnings,
            auto_finalize,
        ) {
            audit(cli, "db_daemon_tick_failed", &error.to_string())?;
            eprintln!("db daemon tick failed: {error}");
        }
        thread::sleep(Duration::from_secs(interval_seconds));
    }
}

fn db_daemon_tick(
    cli: &Cli,
    agent_id: &str,
    dry_run: bool,
    allow_breaking_warnings: bool,
    auto_finalize: bool,
) -> Result<(), AppError> {
    let mut client = db_client(cli)?;
    ensure_schema(cli, &mut client)?;
    let current = installed_cockroach_version(&cli.binary_path)?;
    let lease = acquire_lease(cli, &mut client, agent_id)?;
    let nodes = discover_nodes(&mut client)?;
    heartbeat_agent(cli, &mut client, agent_id, &current)?;

    if lease.is_leader {
        audit(cli, "leader_acquired", &format!("node_id={agent_id}"))?;
        leader_reconcile(
            cli,
            &mut client,
            &current,
            &nodes,
            allow_breaking_warnings,
            auto_finalize,
            dry_run,
        )?;
    } else {
        audit(
            cli,
            "leader_observed",
            &format!("holder={}", lease.holder_id),
        )?;
    }

    follower_reconcile(cli, &mut client, dry_run)
}

fn leader_reconcile(
    cli: &Cli,
    client: &mut Client,
    current: &Version,
    nodes: &[DiscoveredNode],
    allow_breaking_warnings: bool,
    auto_finalize: bool,
    dry_run: bool,
) -> Result<(), AppError> {
    if let Some(active) = active_rollout(cli, client)? {
        let manifest: RolloutManifest = serde_json::from_str(&active.manifest_json)?;
        if rollout_is_complete(cli, client, &manifest, nodes)? {
            audit(
                cli,
                "rollout_complete",
                &format!("target={}", manifest.target_version),
            )?;
            if auto_finalize
                && major_line(&manifest.current_version) != major_line(&manifest.target_version)
            {
                if dry_run {
                    audit(
                        cli,
                        "finalize_ready_dry_run",
                        &format!("target={}", manifest.target_version),
                    )?;
                } else {
                    finalize_command(cli, &manifest.target_version.to_string(), false)?;
                    mark_rollout_finalized(cli, client, &manifest.target_version)?;
                }
            } else if major_line(&manifest.current_version) == major_line(&manifest.target_version)
            {
                mark_rollout_finalized(cli, client, &manifest.target_version)?;
            } else {
                audit(
                    cli,
                    "rollout_waiting_for_finalization",
                    &format!("target={}", manifest.target_version),
                )?;
            }
        }
        return Ok(());
    }

    let plan = build_upgrade_plan(cli, Some(&current.to_string()), None)?;
    let manifest = create_manifest_for_plan(cli, plan, allow_breaking_warnings)?;
    publish_rollout(cli, client, &manifest)?;
    Ok(())
}

fn follower_reconcile(cli: &Cli, client: &mut Client, dry_run: bool) -> Result<(), AppError> {
    let Some(active) = active_rollout(cli, client)? else {
        return Ok(());
    };

    let manifest: RolloutManifest = serde_json::from_str(&active.manifest_json)?;
    let current = installed_cockroach_version(&cli.binary_path)?;
    if current == manifest.target_version {
        record_agent_state(cli, client, "complete", &current, None)?;
        return Ok(());
    }

    if current != manifest.current_version {
        record_agent_state(
            cli,
            client,
            "waiting",
            &current,
            Some(&format!(
                "active rollout expects current={} target={}",
                manifest.current_version, manifest.target_version
            )),
        )?;
        return Ok(());
    }

    let artifact = manifest
        .artifacts
        .iter()
        .find(|candidate| candidate.os == "linux" && candidate.arch == normalized_arch())
        .ok_or_else(|| {
            AppError::Message("active manifest lacks artifact for this host arch".to_string())
        })?;
    let artifact_path = Path::new(&artifact.path);
    if !artifact_path.exists() {
        download_to_file(&artifact.url, artifact_path)?;
    }
    verify_artifact(artifact_path, artifact)?;
    record_agent_state(cli, client, "installing", &current, None)?;

    let manifest_file = tempfile::Builder::new()
        .prefix("cockroach-rollout-active-")
        .suffix(".json")
        .tempfile()?;
    write_json_file(manifest_file.path(), &manifest)?;
    install_command(cli, manifest_file.path(), None, dry_run)?;
    let new_version = if dry_run {
        current
    } else {
        installed_cockroach_version(&cli.binary_path)?
    };
    record_agent_state(cli, client, "complete", &new_version, None)?;
    Ok(())
}

fn db_client(cli: &Cli) -> Result<Client, AppError> {
    let database_url = cli.database_url.as_deref().ok_or_else(|| {
        AppError::Message("--database-url or CROACH_ROLLOUT_DATABASE_URL is required".to_string())
    })?;
    let tls = TlsConnector::builder()
        .build()
        .map_err(|error| AppError::Message(error.to_string()))?;
    Ok(Client::connect(database_url, MakeTlsConnector::new(tls))?)
}

fn ensure_schema(cli: &Cli, client: &mut Client) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.batch_execute(&format!(
        "
        CREATE SCHEMA IF NOT EXISTS {schema};
        CREATE TABLE IF NOT EXISTS {schema}.leases (
            name STRING PRIMARY KEY,
            holder_id STRING NOT NULL,
            expires_at TIMESTAMPTZ NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        CREATE TABLE IF NOT EXISTS {schema}.rollouts (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            status STRING NOT NULL,
            current_version STRING NOT NULL,
            target_version STRING NOT NULL,
            manifest_json STRING NOT NULL,
            created_by STRING NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            finalized_at TIMESTAMPTZ NULL
        );
        CREATE TABLE IF NOT EXISTS {schema}.agent_status (
            agent_id STRING PRIMARY KEY,
            state STRING NOT NULL,
            version STRING NOT NULL,
            error STRING NULL,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        "
    ))?;
    Ok(())
}

fn acquire_lease(cli: &Cli, client: &mut Client, holder_id: &str) -> Result<LeaseResult, AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!(
            "
            INSERT INTO {schema}.leases (name, holder_id, expires_at, updated_at)
            VALUES ('leader', $1, now() + ($2::INT8 * INTERVAL '1 second'), now())
            ON CONFLICT (name) DO NOTHING
            "
        ),
        &[&holder_id, &(DEFAULT_LEASE_SECONDS as i64)],
    )?;

    let rows = client.query(
        &format!(
            "
            UPDATE {schema}.leases
            SET holder_id = $1,
                expires_at = now() + ($2::INT8 * INTERVAL '1 second'),
                updated_at = now()
            WHERE name = 'leader'
              AND (holder_id = $1 OR expires_at < now())
            RETURNING holder_id
            "
        ),
        &[&holder_id, &(DEFAULT_LEASE_SECONDS as i64)],
    )?;
    if let Some(row) = rows.first() {
        let current_holder: String = row.get(0);
        return Ok(LeaseResult {
            is_leader: current_holder == holder_id,
            holder_id: current_holder,
        });
    }

    let row = client.query_one(
        &format!("SELECT holder_id FROM {schema}.leases WHERE name = 'leader'"),
        &[],
    )?;
    let current_holder: String = row.get(0);
    Ok(LeaseResult {
        is_leader: current_holder == holder_id,
        holder_id: current_holder,
    })
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
            UPSERT INTO {schema}.agent_status (agent_id, state, version, error, updated_at)
            VALUES ($1, 'running', $2, NULL, now())
            "
        ),
        &[&agent_id, &version.to_string()],
    )?;
    Ok(())
}

fn record_agent_state(
    cli: &Cli,
    client: &mut Client,
    state: &str,
    version: &Version,
    error: Option<&str>,
) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    let agent_id = agent_node_id(cli)?;
    client.execute(
        &format!(
            "
            UPSERT INTO {schema}.agent_status (agent_id, state, version, error, updated_at)
            VALUES ($1, $2, $3, $4, now())
            "
        ),
        &[&agent_id, &state, &version.to_string(), &error],
    )?;
    Ok(())
}

fn active_rollout(cli: &Cli, client: &mut Client) -> Result<Option<DbRolloutRow>, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let rows = client.query(
        &format!(
            "
            SELECT manifest_json
            FROM {schema}.rollouts
            WHERE status = 'active'
            ORDER BY created_at DESC
            LIMIT 1
            "
        ),
        &[],
    )?;
    Ok(rows.into_iter().next().map(|row| DbRolloutRow {
        manifest_json: row.get(0),
    }))
}

fn create_manifest_for_plan(
    cli: &Cli,
    plan: UpgradePlan,
    allow_breaking_warnings: bool,
) -> Result<RolloutManifest, AppError> {
    if !plan.release_note_warnings.is_empty() && !allow_breaking_warnings {
        return Err(AppError::Message(format!(
            "release notes contain warning patterns; rerun daemon with --allow-breaking-warnings after review: {}",
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
        release_note_warnings_approved: allow_breaking_warnings,
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
            VALUES ('active', $1, $2, $3, $4)
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
        "rollout_published",
        &format!(
            "current={} target={}",
            manifest.current_version, manifest.target_version
        ),
    )?;
    Ok(())
}

fn rollout_is_complete(
    cli: &Cli,
    client: &mut Client,
    manifest: &RolloutManifest,
    nodes: &[DiscoveredNode],
) -> Result<bool, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let live_count = nodes.len() as i64;
    if live_count == 0 {
        return Ok(false);
    }

    let row = client.query_one(
        &format!(
            "
            SELECT count(*)
            FROM {schema}.agent_status
            WHERE state = 'complete'
              AND version = $1
              AND updated_at > now() - ($2::INT8 * INTERVAL '1 second')
            "
        ),
        &[
            &manifest.target_version.to_string(),
            &(DEFAULT_AGENT_STALE_SECONDS as i64),
        ],
    )?;
    let complete_count: i64 = row.get(0);
    Ok(complete_count >= live_count)
}

fn mark_rollout_finalized(
    cli: &Cli,
    client: &mut Client,
    target_version: &Version,
) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!(
            "
            UPDATE {schema}.rollouts
            SET status = 'finalized',
                finalized_at = now()
            WHERE status = 'active'
              AND target_version = $1
            "
        ),
        &[&target_version.to_string()],
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

fn finalize_command(cli: &Cli, target_version: &str, dry_run: bool) -> Result<(), AppError> {
    let version = parse_cockroach_version(target_version)?;
    reject_prerelease(&version)?;
    let cluster_version = major_line_string(&version);
    let sql = format!("SET CLUSTER SETTING version = '{cluster_version}';");

    audit(
        cli,
        "finalize_requested",
        &format!("target={} cluster_version={cluster_version}", version),
    )?;

    if dry_run {
        println!("dry run: {} sql -e \"{sql}\"", cli.binary_path.display());
        return Ok(());
    }

    let status = Command::new(&cli.binary_path)
        .arg("sql")
        .arg("-e")
        .arg(&sql)
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(AppError::Message(format!(
            "finalization SQL exited with status {status}"
        )));
    }

    audit(
        cli,
        "finalize_complete",
        &format!("cluster_version={cluster_version}"),
    )?;
    Ok(())
}

fn self_check_command(cli: &Cli) -> Result<(), AppError> {
    audit(
        cli,
        "self_check_start",
        "validating local permissions and commands",
    )?;
    require_command("tar")?;
    require_command("systemctl")?;

    if !cli.binary_path.exists() {
        return Err(AppError::Message(format!(
            "binary path does not exist: {}",
            cli.binary_path.display()
        )));
    }

    let parent = cli.binary_path.parent().ok_or_else(|| {
        AppError::Message(format!(
            "binary path has no parent: {}",
            cli.binary_path.display()
        ))
    })?;

    if !parent.is_dir() {
        return Err(AppError::Message(format!(
            "binary parent is not a directory: {}",
            parent.display()
        )));
    }

    installed_cockroach_version(&cli.binary_path)?;
    audit(cli, "self_check_complete", "local validation completed")?;
    Ok(())
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

fn install_artifact(cli: &Cli, artifact: &Path) -> Result<(), AppError> {
    let work_dir = tempfile::Builder::new()
        .prefix("cockroach-rollout-")
        .tempdir()?;
    let backup_path = cli
        .binary_path
        .with_extension(format!("bak.{}", unix_time()?));

    audit(
        cli,
        "install_start",
        &format!(
            "artifact={} binary={}",
            artifact.display(),
            cli.binary_path.display()
        ),
    )?;

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

    run_command(
        "systemctl",
        [OsStr::new("stop"), OsStr::new(&cli.service_name)],
    )?;
    fs::copy(&cli.binary_path, &backup_path)?;
    fs::copy(&extracted, &cli.binary_path)?;
    run_command("chmod", ["0755".as_ref(), cli.binary_path.as_os_str()])?;
    run_command(
        "systemctl",
        [OsStr::new("start"), OsStr::new(&cli.service_name)],
    )?;

    audit(
        cli,
        "install_complete",
        &format!(
            "backup={} binary={}",
            backup_path.display(),
            cli.binary_path.display()
        ),
    )?;
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
