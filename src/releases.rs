// file: src/releases.rs
// version: 1.0.0
// guid: 93dac1ed-690a-4baa-a51a-3f8362abd9a2
// last-edited: 2026-10-12

//! The upstream release catalog. The leader fetches it from GitHub at most
//! hourly and keeps it in the coordination DB; proposals and every dashboard
//! read the DB copy, so neither burns the unauthenticated rate limit.

use postgres::Client;
use semver::Version;
use serde::Deserialize;

use crate::{AppError, Cli, build_upgrade_steps, major_line, release_notes_url, sql_ident};

/// A cached catalog older than this is fetched again.
pub const CATALOG_MAX_AGE_SECS: i64 = 3600;
/// "Check now" never fetches more often than this.
pub const CATALOG_MIN_REFRESH_SECS: i64 = 600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    pub versions: Vec<Version>,
    /// `None` until the first successful fetch.
    pub fetched_age_secs: Option<i64>,
    pub refresh_requested_age_secs: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineMark {
    /// The line the cluster runs.
    Current,
    /// The open proposal's target line.
    Proposed,
    /// A later step on the path the leader will take, counted from 1.
    Step(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseLine {
    pub line: (u64, u64),
    pub latest: Version,
    pub mark: LineMark,
    pub notes_url: String,
}

/// What the dashboard's Releases panel shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasesView {
    pub current: Option<Version>,
    pub lines: Vec<ReleaseLine>,
    pub fetched_age_secs: Option<i64>,
    pub refresh_requested: bool,
    pub last_error: Option<String>,
}

#[derive(Deserialize)]
struct MatchingRef {
    #[serde(rename = "ref")]
    name: String,
}

/// Stable versions from a `git/matching-refs/tags/v` response, sorted and
/// deduplicated. Pre-releases and non-version tags are dropped.
pub fn parse_matching_refs(json: &str) -> Result<Vec<Version>, String> {
    let refs: Vec<MatchingRef> =
        serde_json::from_str(json).map_err(|error| format!("bad tag list: {error}"))?;
    let mut versions: Vec<Version> = refs
        .iter()
        .filter_map(|tag| tag.name.strip_prefix("refs/tags/v"))
        .filter_map(|text| Version::parse(text).ok())
        .filter(|version| version.pre.is_empty() && version.build.is_empty())
        .collect();
    versions.sort();
    versions.dedup();
    Ok(versions)
}

/// The one-call tag listing for the repository `github_api_url` points at,
/// whatever endpoint of it was configured.
pub fn matching_refs_url(github_api_url: &str) -> Result<String, String> {
    let marker = "/repos/";
    let start = github_api_url
        .find(marker)
        .ok_or_else(|| format!("{github_api_url} is not a GitHub repository API URL"))?;
    let base = &github_api_url[..start + marker.len()];
    let mut parts = github_api_url[start + marker.len()..].split(['/', '?']);
    match (parts.next(), parts.next()) {
        (Some(owner), Some(repo)) if !owner.is_empty() && !repo.is_empty() => {
            Ok(format!("{base}{owner}/{repo}/git/matching-refs/tags/v"))
        }
        _ => Err(format!(
            "{github_api_url} does not name an owner and repository"
        )),
    }
}

/// Whether the leader should fetch now: never fetched, older than an hour,
/// or asked for after the last fetch and that fetch is over ten minutes old.
pub fn refresh_due(fetched_age_secs: Option<i64>, requested_age_secs: Option<i64>) -> bool {
    let Some(fetched) = fetched_age_secs else {
        return true;
    };
    if fetched > CATALOG_MAX_AGE_SECS {
        return true;
    }
    requested_age_secs.is_some_and(|requested| requested < fetched)
        && fetched > CATALOG_MIN_REFRESH_SECS
}

/// One row per release line from the cluster's own upward, newest first,
/// marked with the path the leader will take from `current`.
pub fn release_lines(
    current: &Version,
    versions: &[Version],
    proposed: Option<&Version>,
    notes_base_url: &str,
) -> Vec<ReleaseLine> {
    let current_line = major_line(current);
    let mut lines: Vec<(u64, u64)> = versions
        .iter()
        .map(major_line)
        .filter(|line| *line >= current_line)
        .collect();
    lines.sort();
    lines.dedup();
    let steps = versions
        .iter()
        .max()
        .and_then(|latest| build_upgrade_steps(current, latest, versions, "").ok())
        .unwrap_or_default();
    lines
        .into_iter()
        .rev()
        .filter_map(|line| {
            let latest = versions
                .iter()
                .filter(|version| major_line(version) == line)
                .max()?
                .clone();
            let step = steps
                .iter()
                .position(|step| major_line(&step.to_version) == line);
            let mark = if line == current_line {
                LineMark::Current
            } else if proposed.is_some_and(|target| major_line(target) == line) {
                LineMark::Proposed
            } else {
                LineMark::Step(step? + 1)
            };
            Some(ReleaseLine {
                line,
                notes_url: release_notes_url(notes_base_url, &latest),
                latest,
                mark,
            })
        })
        .collect()
}

pub fn load_catalog(cli: &Cli, client: &mut Client) -> Result<Option<Catalog>, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let rows = client.query(
        &format!(
            "
            SELECT versions_json,
                   extract(epoch FROM now() - fetched_at)::INT8,
                   extract(epoch FROM now() - refresh_requested_at)::INT8,
                   last_error
            FROM {schema}.release_catalog WHERE id = 1
            "
        ),
        &[],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let versions_json: Option<String> = row.get(0);
    let versions = match versions_json {
        Some(json) => serde_json::from_str::<Vec<String>>(&json)?
            .iter()
            .map(|text| Version::parse(text))
            .collect::<Result<_, _>>()?,
        None => Vec::new(),
    };
    Ok(Some(Catalog {
        versions,
        fetched_age_secs: row.get(1),
        refresh_requested_age_secs: row.get(2),
        last_error: row.get(3),
    }))
}

pub fn store_catalog(cli: &Cli, client: &mut Client, versions: &[Version]) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    let json = serde_json::to_string(&versions.iter().map(Version::to_string).collect::<Vec<_>>())?;
    client.execute(
        &format!(
            "
            INSERT INTO {schema}.release_catalog (id, versions_json, fetched_at, last_error)
            VALUES (1, $1, now(), NULL)
            ON CONFLICT (id) DO UPDATE
            SET versions_json = excluded.versions_json, fetched_at = now(), last_error = NULL
            "
        ),
        &[&json],
    )?;
    Ok(())
}

/// Keeps the previous list: a failed fetch must not empty the catalog.
pub fn store_catalog_error(cli: &Cli, client: &mut Client, error: &str) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!(
            "
            INSERT INTO {schema}.release_catalog (id, last_error) VALUES (1, $1)
            ON CONFLICT (id) DO UPDATE SET last_error = excluded.last_error
            "
        ),
        &[&error],
    )?;
    Ok(())
}

pub fn request_refresh(cli: &Cli, client: &mut Client) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!(
            "
            INSERT INTO {schema}.release_catalog (id, refresh_requested_at) VALUES (1, now())
            ON CONFLICT (id) DO UPDATE SET refresh_requested_at = now()
            "
        ),
        &[],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).expect("literal should parse")
    }

    fn versions(texts: &[&str]) -> Vec<Version> {
        texts.iter().map(|text| v(text)).collect()
    }

    #[test]
    fn matching_refs_keep_stable_versions_only() {
        let json = r#"[
            {"ref": "refs/tags/v26.1.8", "object": {}},
            {"ref": "refs/tags/v25.4.17"},
            {"ref": "refs/tags/v26.2.0-rc.1"},
            {"ref": "refs/tags/v26.2.0-alpha.20250101"},
            {"ref": "refs/tags/vfoo"},
            {"ref": "refs/tags/v25.4.17"},
            {"ref": "refs/heads/v99.0.0"}
        ]"#;
        assert_eq!(
            parse_matching_refs(json).unwrap(),
            versions(&["25.4.17", "26.1.8"])
        );
        assert!(parse_matching_refs("{\"message\": \"rate limited\"}").is_err());
    }

    #[test]
    fn tags_url_becomes_matching_refs_url() {
        let want = "https://api.github.com/repos/cockroachdb/cockroach/git/matching-refs/tags/v";
        for configured in [
            "https://api.github.com/repos/cockroachdb/cockroach/tags?per_page=100",
            "https://api.github.com/repos/cockroachdb/cockroach/releases/latest",
            "https://api.github.com/repos/cockroachdb/cockroach",
        ] {
            assert_eq!(matching_refs_url(configured).unwrap(), want, "{configured}");
        }
        assert!(matching_refs_url("https://example.com/tags").is_err());
        assert!(matching_refs_url("https://api.github.com/repos/onlyowner").is_err());
    }

    #[test]
    fn refresh_is_due_when_old_or_requested_and_not_throttled() {
        assert!(refresh_due(None, None), "never fetched");
        assert!(!refresh_due(Some(60), None));
        assert!(refresh_due(Some(CATALOG_MAX_AGE_SECS + 1), None));
        assert!(
            !refresh_due(Some(300), Some(10)),
            "asked for, but fetched under ten minutes ago"
        );
        assert!(refresh_due(Some(900), Some(10)), "asked for after a fetch");
        assert!(
            !refresh_due(Some(900), Some(1200)),
            "the request predates the last fetch"
        );
    }

    #[test]
    fn release_lines_mark_current_proposed_and_path() {
        let catalog = versions(&[
            "25.3.9", "25.4.16", "25.4.17", "26.1.7", "26.1.8", "26.2.6", "26.2.7",
        ]);
        let base = "https://docs.example/releases";
        let lines = release_lines(&v("25.4.17"), &catalog, Some(&v("26.1.8")), base);
        assert_eq!(
            lines,
            vec![
                ReleaseLine {
                    line: (26, 2),
                    latest: v("26.2.7"),
                    mark: LineMark::Step(2),
                    notes_url: format!("{base}/v26.2"),
                },
                ReleaseLine {
                    line: (26, 1),
                    latest: v("26.1.8"),
                    mark: LineMark::Proposed,
                    notes_url: format!("{base}/v26.1"),
                },
                ReleaseLine {
                    line: (25, 4),
                    latest: v("25.4.17"),
                    mark: LineMark::Current,
                    notes_url: format!("{base}/v25.4"),
                },
            ]
        );
        let quiet = release_lines(&v("25.4.17"), &catalog, None, base);
        assert_eq!(quiet[1].mark, LineMark::Step(1));
        assert!(release_lines(&v("25.4.17"), &[], None, base).is_empty());
    }
}
