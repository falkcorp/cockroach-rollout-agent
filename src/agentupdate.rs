// file: src/agentupdate.rs
// version: 1.0.0
// guid: e71c32fe-ae0a-49fb-9de9-6333f4abfd23
// last-edited: 2026-10-13

//! Agent self-update. The leader proposes the newest stable agent release
//! and pins each architecture's SHA-256 from its `SHA256SUMS`; a person
//! approves exactly those bytes. Agents then update one at a time: verify
//! the download against the pinned digest, run the new binary's `--version`
//! and `self-check`, swap `<agent_root>/agent/bin/cockroach-rollout-agent`,
//! and exit with `RESTART_EXIT_CODE` so systemd starts the new binary.
//!
//! A new binary that never confirms (connects to the DB and finishes a tick)
//! is rolled back by `agent-preflight`, which the unit runs from the
//! root-owned `/usr/local/bin` copy before every start.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use postgres::Client;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::layout::BinaryLayout;
use crate::{AppError, Cli, SUPPORTED_ARCHES, sql_ident};

/// Exit status that asks systemd to start the agent again
/// (`RestartForceExitStatus=` in the unit).
pub const RESTART_EXIT_CODE: u8 = 75;
/// Starts a new binary gets to confirm before `agent-preflight` rolls back.
pub const ROLLBACK_AFTER_STARTS: u32 = 2;
/// How long a freshly updated agent may take to finish its first good tick
/// before it exits, so the next start counts towards the rollback.
pub const CONFIRM_DEADLINE_SECS: u64 = 600;

const PENDING_FILE: &str = "pending.json";
const ROLLED_BACK_FILE: &str = "rolled-back";

pub fn tarball_name(arch: &str) -> String {
    format!("cockroach-rollout-agent-linux-{arch}.tar.gz")
}

/// The agent's own version.
pub fn own_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo.toml version is semver")
}

/// A downloadable agent build pinned to the digest a person approves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub url: String,
    pub sha256: String,
}

/// A stable agent release as the leader found it on GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRelease {
    pub version: Version,
    pub html_url: String,
    /// Keyed by architecture (`amd64`, `arm64`).
    pub assets: BTreeMap<String, Asset>,
}

/// What a GitHub "latest release" response says, before digests are known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseListing {
    pub version: Version,
    pub html_url: String,
    /// Asset file name to download URL.
    pub assets: HashMap<String, String>,
}

/// Parses `GET /repos/{owner}/{repo}/releases/latest`. Drafts, pre-releases
/// and pre-release versions are refused: only stable releases are proposed.
pub fn parse_release(json: &str) -> Result<ReleaseListing, String> {
    #[derive(Deserialize)]
    struct Raw {
        tag_name: String,
        html_url: String,
        #[serde(default)]
        draft: bool,
        #[serde(default)]
        prerelease: bool,
        #[serde(default)]
        assets: Vec<RawAsset>,
    }
    #[derive(Deserialize)]
    struct RawAsset {
        name: String,
        browser_download_url: String,
    }
    let raw: Raw = serde_json::from_str(json).map_err(|error| error.to_string())?;
    if raw.draft || raw.prerelease {
        return Err(format!("{} is not a stable release", raw.tag_name));
    }
    let version = Version::parse(raw.tag_name.trim_start_matches('v'))
        .map_err(|error| format!("release tag {}: {error}", raw.tag_name))?;
    if !version.pre.is_empty() {
        return Err(format!("{} is a pre-release version", raw.tag_name));
    }
    Ok(ReleaseListing {
        version,
        html_url: raw.html_url,
        assets: raw
            .assets
            .into_iter()
            .map(|asset| (asset.name, asset.browser_download_url))
            .collect(),
    })
}

/// Parses `sha256sum` output into file name to lowercase hex digest.
pub fn parse_sha256sums(text: &str) -> Result<HashMap<String, String>, String> {
    let mut sums = HashMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (digest, name) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("malformed SHA256SUMS line: {line}"))?;
        let name = name.trim_start().trim_start_matches('*');
        let digest = digest.to_ascii_lowercase();
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("malformed digest for {name}"));
        }
        sums.insert(name.to_string(), digest);
    }
    Ok(sums)
}

/// Pins every supported architecture's tarball to its digest. A release
/// missing either is not proposed, so no node is left without a build.
pub fn pin_assets(
    listing: &ReleaseListing,
    sums: &HashMap<String, String>,
) -> Result<AgentRelease, String> {
    let mut assets = BTreeMap::new();
    for arch in SUPPORTED_ARCHES {
        let name = tarball_name(arch);
        let url = listing
            .assets
            .get(&name)
            .ok_or_else(|| format!("v{} has no {name}", listing.version))?;
        let sha256 = sums
            .get(&name)
            .ok_or_else(|| format!("SHA256SUMS of v{} does not list {name}", listing.version))?;
        assets.insert(
            arch.to_string(),
            Asset {
                url: url.clone(),
                sha256: sha256.clone(),
            },
        );
    }
    Ok(AgentRelease {
        version: listing.version.clone(),
        html_url: listing.html_url.clone(),
        assets,
    })
}

/// Reads the version from `cockroach-rollout-agent --version` output.
pub fn version_from_output(output: &str) -> Option<Version> {
    output
        .split_whitespace()
        .last()
        .and_then(|word| Version::parse(word.trim_start_matches('v')).ok())
}

/// Whether the leader should propose `latest`: it is newer than this agent
/// and nobody has seen it yet. A declined version stays declined.
pub fn should_propose(latest: &Version, own: &Version, known: &[Version]) -> bool {
    latest > own && !known.contains(latest)
}

/// This agent's claim on an update, stored on its `agent_status` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub to: Version,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStep {
    Nothing,
    /// Drop this agent's marker. `confirmed` is true when the agent now runs
    /// the version the marker pointed at.
    Clear {
        confirmed: bool,
    },
    /// An update is due but must not start now.
    Blocked(String),
    Start(Version),
}

/// Decides what this agent does about its own binary this tick.
///
/// `in_flight` lists the other agents whose markers point at `approved`;
/// one of them updating, or failing to, holds everyone else back.
pub fn update_step(
    own: &Version,
    approved: Option<&Version>,
    marker: Option<&Marker>,
    in_flight: &[String],
) -> UpdateStep {
    if let Some(marker) = marker {
        // Reached, overtaken, or no longer the target: the claim is done.
        if marker.to <= *own || approved != Some(&marker.to) {
            return UpdateStep::Clear {
                confirmed: marker.to == *own,
            };
        }
    }
    let Some(target) = approved.filter(|target| *target > own) else {
        return UpdateStep::Nothing;
    };
    if let Some(Marker {
        error: Some(error), ..
    }) = marker
    {
        return UpdateStep::Blocked(format!(
            "the update to {target} failed here: {error}; \
             run `cockroach-rollout-agent agent-update retry` once it is fixed"
        ));
    }
    if !in_flight.is_empty() {
        return UpdateStep::Blocked(format!(
            "waiting for {} to finish updating to {target}",
            in_flight.join(", ")
        ));
    }
    UpdateStep::Start(target.clone())
}

/// One line on where an agent's self-update stands, for the CLI and UI.
pub fn progress(to: Option<&str>, error: Option<&str>) -> String {
    match (to, error) {
        (Some(to), Some(error)) => format!("update to {to} failed: {error}"),
        (Some(to), None) => format!("updating to {to}"),
        (None, Some(note)) => note.to_string(),
        (None, None) => String::new(),
    }
}

/// Recorded before the agent restarts into a new binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// File name under `versions/` that ran before the swap.
    pub from: String,
    pub to: Version,
    /// Starts of the new binary so far, counted by `agent-preflight`.
    pub starts: u32,
}

pub fn pending_path(layout: &BinaryLayout) -> PathBuf {
    layout.root().join(PENDING_FILE)
}

pub fn rolled_back_path(layout: &BinaryLayout) -> PathBuf {
    layout.root().join(ROLLED_BACK_FILE)
}

pub fn read_pending(layout: &BinaryLayout) -> io::Result<Option<Pending>> {
    match fs::read(pending_path(layout)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn write_pending(layout: &BinaryLayout, pending: &Pending) -> io::Result<()> {
    let path = pending_path(layout);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(pending)?)?;
    fs::File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, &path)
}

pub fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    /// No update is waiting to be confirmed.
    Idle,
    /// The new binary is starting for the `n`th time.
    Counted(u32),
    /// The new binary used up its starts; the previous one is back.
    RolledBack { message: String },
}

/// Runs before every agent start. Counts starts of an unconfirmed new
/// binary and, once it has had `ROLLBACK_AFTER_STARTS` of them, points the
/// link back at the previous binary and leaves a note for it to report.
pub fn preflight(layout: &BinaryLayout) -> io::Result<Preflight> {
    let pending = match read_pending(layout) {
        Ok(Some(pending)) => pending,
        Ok(None) => return Ok(Preflight::Idle),
        // Without `from` there is nothing to restore. Dropping the file
        // keeps the current binary from being counted towards a rollback.
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            remove_file_if_present(&pending_path(layout))?;
            return Ok(Preflight::Idle);
        }
        Err(error) => return Err(error),
    };
    if pending.starts < ROLLBACK_AFTER_STARTS {
        let starts = pending.starts + 1;
        write_pending(layout, &Pending { starts, ..pending })?;
        return Ok(Preflight::Counted(starts));
    }
    layout.restore(&pending.from)?;
    let message = format!(
        "v{} did not finish a tick in {} starts; restored {}",
        pending.to, pending.starts, pending.from
    );
    fs::write(rolled_back_path(layout), &message)?;
    remove_file_if_present(&pending_path(layout))?;
    Ok(Preflight::RolledBack { message })
}

/// Takes the note `preflight` left after a rollback, if any.
pub fn take_rolled_back(layout: &BinaryLayout) -> io::Result<Option<String>> {
    let path = rolled_back_path(layout);
    match fs::read_to_string(&path) {
        Ok(message) => {
            remove_file_if_present(&path)?;
            Ok(Some(message))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// An agent release in the coordination DB, as operators see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReleaseRow {
    pub version: Version,
    /// `proposed`, `approved`, `declined`, or `superseded`.
    pub status: String,
    pub html_url: String,
    pub assets: BTreeMap<String, Asset>,
    pub proposed_at: String,
    pub decided_by: Option<String>,
}

/// Releases shown on the dashboard, newest first.
pub const RELEASES_SHOWN: usize = 5;

/// Records a newly found release as `proposed`, superseding any older
/// proposal. Returns false when the version was already known.
pub fn store_proposal(
    cli: &Cli,
    client: &mut Client,
    release: &AgentRelease,
    by: &str,
) -> Result<bool, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let assets = serde_json::to_string(&release.assets)?;
    let mut tx = client.transaction()?;
    let inserted = tx.execute(
        &format!(
            "INSERT INTO {schema}.agent_releases (version, status, html_url, assets_json, proposed_by) \
             VALUES ($1, 'proposed', $2, $3, $4) ON CONFLICT (version) DO NOTHING"
        ),
        &[&release.version.to_string(), &release.html_url, &assets, &by],
    )?;
    if inserted == 1 {
        tx.execute(
            &format!(
                "UPDATE {schema}.agent_releases SET status = 'superseded' \
                 WHERE status = 'proposed' AND version != $1"
            ),
            &[&release.version.to_string()],
        )?;
    }
    tx.commit()?;
    Ok(inserted == 1)
}

pub fn rows(cli: &Cli, client: &mut Client) -> Result<Vec<AgentReleaseRow>, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let mut rows = client
        .query(
            &format!(
                "SELECT version, status, html_url, assets_json, proposed_at::STRING, decided_by \
                 FROM {schema}.agent_releases"
            ),
            &[],
        )?
        .iter()
        .map(|row| {
            let version: String = row.get(0);
            let assets: String = row.get(3);
            Ok(AgentReleaseRow {
                version: Version::parse(&version)?,
                status: row.get(1),
                html_url: row.get(2),
                assets: serde_json::from_str(&assets)?,
                proposed_at: row.get(4),
                decided_by: row.get(5),
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    rows.sort_by(|a, b| b.version.cmp(&a.version));
    Ok(rows)
}

/// The release agents update to: the newest approved one.
pub fn newest_approved(
    cli: &Cli,
    client: &mut Client,
) -> Result<Option<AgentReleaseRow>, AppError> {
    Ok(rows(cli, client)?
        .into_iter()
        .find(|row| row.status == "approved"))
}

/// Approves a proposal, or declines a proposal or an approval. Declining an
/// approved release stops agents that have not updated yet; agents already
/// on it stay there.
pub fn decide(
    cli: &Cli,
    client: &mut Client,
    version: &str,
    approve: bool,
    by: &str,
) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    let version = Version::parse(version.trim().trim_start_matches('v'))?.to_string();
    let (status, from) = if approve {
        ("approved", "('proposed')")
    } else {
        ("declined", "('proposed', 'approved')")
    };
    let changed = client.execute(
        &format!(
            "UPDATE {schema}.agent_releases SET status = $2, decided_by = $3, decided_at = now() \
             WHERE version = $1 AND status IN {from}"
        ),
        &[&version, &status, &by],
    )?;
    if changed == 0 {
        return Err(AppError::Message(format!(
            "agent release {version} is not {}; reload and try again",
            if approve {
                "proposed"
            } else {
                "proposed or approved"
            }
        )));
    }
    Ok(())
}

pub fn known_versions(cli: &Cli, client: &mut Client) -> Result<Vec<Version>, AppError> {
    Ok(rows(cli, client)?
        .into_iter()
        .map(|row| row.version)
        .collect())
}

/// This agent's update claim, if it has one.
pub fn marker(cli: &Cli, client: &mut Client, agent_id: &str) -> Result<Option<Marker>, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let rows = client.query(
        &format!(
            "SELECT agent_update_to, agent_update_error FROM {schema}.agent_status \
             WHERE agent_id = $1 AND agent_update_to IS NOT NULL"
        ),
        &[&agent_id],
    )?;
    rows.first()
        .map(|row| {
            let to: String = row.get(0);
            Ok(Marker {
                to: Version::parse(&to)?,
                error: row.get(1),
            })
        })
        .transpose()
}

/// Other agents whose claims point at `target`: updating, or stuck.
pub fn in_flight(
    cli: &Cli,
    client: &mut Client,
    agent_id: &str,
    target: &Version,
) -> Result<Vec<String>, AppError> {
    let schema = sql_ident(&cli.schema)?;
    Ok(client
        .query(
            &format!(
                "SELECT agent_id FROM {schema}.agent_status \
                 WHERE agent_id != $1 AND agent_update_to = $2 ORDER BY agent_id"
            ),
            &[&agent_id, &target.to_string()],
        )?
        .iter()
        .map(|row| row.get(0))
        .collect())
}

/// Claims the update to `target` for this agent unless another agent holds
/// a claim on it. One statement, so two agents cannot both win: CockroachDB
/// runs it serializably.
pub fn claim(
    cli: &Cli,
    client: &mut Client,
    agent_id: &str,
    target: &Version,
) -> Result<bool, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let changed = client.execute(
        &format!(
            "UPDATE {schema}.agent_status \
             SET agent_update_to = $2, agent_update_error = NULL \
             WHERE agent_id = $1 AND NOT EXISTS ( \
                 SELECT 1 FROM {schema}.agent_status \
                 WHERE agent_id != $1 AND agent_update_to = $2)"
        ),
        &[&agent_id, &target.to_string()],
    )?;
    Ok(changed == 1)
}

/// Records why this agent's update failed. With no claim, the text is a
/// note shown on the dashboard that blocks nobody.
pub fn set_error(
    cli: &Cli,
    client: &mut Client,
    agent_id: &str,
    error: Option<&str>,
) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!("UPDATE {schema}.agent_status SET agent_update_error = $2 WHERE agent_id = $1"),
        &[&agent_id, &error],
    )?;
    Ok(())
}

pub fn clear_marker(cli: &Cli, client: &mut Client, agent_id: &str) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!(
            "UPDATE {schema}.agent_status \
             SET agent_update_to = NULL, agent_update_error = NULL WHERE agent_id = $1"
        ),
        &[&agent_id],
    )?;
    Ok(())
}

/// Drops every failed claim so those agents try again. Returns how many.
pub fn retry(cli: &Cli, client: &mut Client) -> Result<u64, AppError> {
    let schema = sql_ident(&cli.schema)?;
    Ok(client.execute(
        &format!(
            "UPDATE {schema}.agent_status \
             SET agent_update_to = NULL, agent_update_error = NULL \
             WHERE agent_update_to IS NOT NULL AND agent_update_error IS NOT NULL"
        ),
        &[],
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).expect("literal should parse")
    }

    const DIGEST_A: &str = "a3f1c0e6b0b4e2b7d9c8f1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6";
    const DIGEST_B: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn release_json(tag: &str, prerelease: bool, draft: bool) -> String {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://github.com/o/r/releases/tag/{tag}",
               "draft":{draft},"prerelease":{prerelease},"assets":[
               {{"name":"cockroach-rollout-agent-linux-amd64.tar.gz","browser_download_url":"https://dl/amd64"}},
               {{"name":"cockroach-rollout-agent-linux-arm64.tar.gz","browser_download_url":"https://dl/arm64"}},
               {{"name":"SHA256SUMS","browser_download_url":"https://dl/sums"}}]}}"#
        )
    }

    #[test]
    fn only_stable_releases_parse() {
        let listing = parse_release(&release_json("v0.3.1", false, false)).unwrap();
        assert_eq!(listing.version, v("0.3.1"));
        assert_eq!(listing.assets["SHA256SUMS"], "https://dl/sums");
        assert!(parse_release(&release_json("v0.3.1", true, false)).is_err());
        assert!(parse_release(&release_json("v0.3.1", false, true)).is_err());
        assert!(parse_release(&release_json("v0.3.1-rc.1", false, false)).is_err());
    }

    #[test]
    fn sha256sums_parse_and_reject_garbage() {
        let sums = parse_sha256sums(&format!(
            "{DIGEST_A}  cockroach-rollout-agent-linux-amd64.tar.gz\n\
             {}  *cockroach-rollout-agent-linux-arm64.tar.gz\n",
            DIGEST_B.to_uppercase()
        ))
        .unwrap();
        assert_eq!(sums["cockroach-rollout-agent-linux-amd64.tar.gz"], DIGEST_A);
        assert_eq!(sums["cockroach-rollout-agent-linux-arm64.tar.gz"], DIGEST_B);
        assert!(parse_sha256sums("abc  file\n").is_err());
        assert!(parse_sha256sums("nodigest\n").is_err());
    }

    #[test]
    fn a_release_needs_every_arch_pinned() {
        let listing = parse_release(&release_json("v0.3.1", false, false)).unwrap();
        let mut sums = HashMap::from([
            (tarball_name("amd64"), DIGEST_A.to_string()),
            (tarball_name("arm64"), DIGEST_B.to_string()),
        ]);
        let release = pin_assets(&listing, &sums).unwrap();
        assert_eq!(release.assets["arm64"].sha256, DIGEST_B);
        assert_eq!(release.assets["amd64"].url, "https://dl/amd64");
        sums.remove(&tarball_name("arm64"));
        assert!(pin_assets(&listing, &sums).is_err());
    }

    #[test]
    fn versions_parse_from_clap_output() {
        assert_eq!(
            version_from_output("cockroach-rollout-agent 0.3.1\n"),
            Some(v("0.3.1"))
        );
        assert_eq!(version_from_output("garbage"), None);
    }

    #[test]
    fn only_new_unseen_releases_are_proposed() {
        assert!(should_propose(&v("0.3.1"), &v("0.3.0"), &[]));
        assert!(!should_propose(&v("0.3.0"), &v("0.3.0"), &[]));
        assert!(!should_propose(&v("0.2.9"), &v("0.3.0"), &[]));
        assert!(!should_propose(&v("0.3.1"), &v("0.3.0"), &[v("0.3.1")]));
    }

    fn marker(to: &str, error: Option<&str>) -> Marker {
        Marker {
            to: v(to),
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn update_steps_follow_approval_markers_and_peers() {
        let own = v("0.3.0");
        let target = v("0.3.1");
        // Nothing approved, or nothing newer.
        assert_eq!(update_step(&own, None, None, &[]), UpdateStep::Nothing);
        assert_eq!(
            update_step(&own, Some(&v("0.3.0")), None, &[]),
            UpdateStep::Nothing
        );
        assert_eq!(
            update_step(&own, Some(&v("0.2.0")), None, &[]),
            UpdateStep::Nothing,
            "never downgrade"
        );
        // Newer approved and nobody else updating.
        assert_eq!(
            update_step(&own, Some(&target), None, &[]),
            UpdateStep::Start(target.clone())
        );
        // Another agent is mid-update or failed: wait.
        assert!(matches!(
            update_step(&own, Some(&target), None, &["n2".to_string()]),
            UpdateStep::Blocked(reason) if reason.contains("n2")
        ));
        // Our own failed attempt is not retried until an operator says so.
        assert!(matches!(
            update_step(&own, Some(&target), Some(&marker("0.3.1", Some("bad"))), &[]),
            UpdateStep::Blocked(reason) if reason.contains("bad")
        ));
        // An interrupted attempt without an error starts again.
        assert_eq!(
            update_step(&own, Some(&target), Some(&marker("0.3.1", None)), &[]),
            UpdateStep::Start(target.clone())
        );
        // Running the marker's version confirms it.
        assert_eq!(
            update_step(&target, Some(&target), Some(&marker("0.3.1", None)), &[]),
            UpdateStep::Clear { confirmed: true }
        );
        // A marker for a version that is no longer the target is stale.
        assert_eq!(
            update_step(&own, None, Some(&marker("0.3.1", Some("bad"))), &[]),
            UpdateStep::Clear { confirmed: false }
        );
        assert_eq!(
            update_step(
                &own,
                Some(&v("0.3.2")),
                Some(&marker("0.3.1", Some("bad"))),
                &[]
            ),
            UpdateStep::Clear { confirmed: false }
        );
        // Overtaken (installed by hand past the claim): done, and it must not
        // hold the others back.
        assert_eq!(
            update_step(
                &v("0.3.2"),
                Some(&target),
                Some(&marker("0.3.1", None)),
                &[]
            ),
            UpdateStep::Clear { confirmed: false }
        );
    }

    fn agent_layout(scratch: &Path) -> BinaryLayout {
        let layout = BinaryLayout::agent(scratch);
        let source = scratch.join("src");
        for version in ["0.3.0", "0.3.1"] {
            fs::write(&source, version).unwrap();
            layout.stage(&source, &v(version)).unwrap();
        }
        layout.activate(&v("0.3.0")).unwrap();
        layout
    }

    #[test]
    fn the_agent_layout_lives_under_its_own_directory() {
        let scratch = tempfile::tempdir().unwrap();
        let layout = agent_layout(scratch.path());
        assert_eq!(
            layout.current_link(),
            scratch.path().join("agent/bin/cockroach-rollout-agent")
        );
        assert_eq!(
            layout.current_target_name().unwrap(),
            "cockroach-rollout-agent-v0.3.0"
        );
    }

    #[test]
    fn preflight_counts_starts_then_rolls_back() {
        let scratch = tempfile::tempdir().unwrap();
        let layout = agent_layout(scratch.path());
        assert_eq!(preflight(&layout).unwrap(), Preflight::Idle);

        let from = layout.activate(&v("0.3.1")).unwrap().unwrap();
        write_pending(
            &layout,
            &Pending {
                from,
                to: v("0.3.1"),
                starts: 0,
            },
        )
        .unwrap();

        assert_eq!(preflight(&layout).unwrap(), Preflight::Counted(1));
        assert_eq!(preflight(&layout).unwrap(), Preflight::Counted(2));
        assert!(matches!(
            preflight(&layout).unwrap(),
            Preflight::RolledBack { message } if message.contains("0.3.1")
        ));
        assert_eq!(fs::read_to_string(layout.current_link()).unwrap(), "0.3.0");
        assert_eq!(read_pending(&layout).unwrap(), None);
        let note = take_rolled_back(&layout).unwrap().unwrap();
        assert!(note.contains("0.3.1"));
        assert_eq!(take_rolled_back(&layout).unwrap(), None);
        assert_eq!(preflight(&layout).unwrap(), Preflight::Idle);
    }

    #[test]
    fn a_corrupt_pending_file_rolls_back_rather_than_wedging() {
        let scratch = tempfile::tempdir().unwrap();
        let layout = agent_layout(scratch.path());
        fs::write(pending_path(&layout), b"{not json").unwrap();
        // Without a readable `from` there is nothing to restore; the file
        // goes so the current binary is not counted towards a rollback.
        assert_eq!(preflight(&layout).unwrap(), Preflight::Idle);
        assert!(!pending_path(&layout).exists());
    }
}
