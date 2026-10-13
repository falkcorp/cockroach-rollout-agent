// file: src/settings.rs
// version: 1.0.0
// guid: 746e9bdc-6e5d-4b71-8d21-a2a0a4e5bcaa
// last-edited: 2026-10-12

//! Which of the cluster's overridden settings an upgrade touches. Defaults
//! come from `cockroach gen settings-list` on the installed and the target
//! binary, so both sides are formatted by the same code; the overrides come
//! from `SHOW ALL CLUSTER SETTINGS`. Values are redacted before they are
//! stored, because the report is shown on the dashboard.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use postgres::Client;
use serde::{Deserialize, Serialize};

use crate::AppError;

/// Never shown, whatever their type.
const ALWAYS_REDACTED: &[&str] = &[
    "cluster.secret",
    "enterprise.license",
    "cloudstorage.http.custom_ca",
];

/// Types whose values are plain and safe to show.
const SHOWN_TYPES: &[&str] = &[
    "boolean",
    "integer",
    "float",
    "duration",
    "enumeration",
    "byte size",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingDefault {
    pub typ: String,
    pub default: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    pub name: String,
    /// Long type name, as `gen settings-list` prints it.
    pub typ: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    Removed,
    DefaultChanged,
    TypeChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingFinding {
    pub name: String,
    pub kind: FindingKind,
    /// Already redacted.
    pub from: String,
    pub to: String,
    /// Our override, redacted; empty for settings we do not override.
    #[serde(default)]
    pub ours: String,
}

impl SettingFinding {
    pub fn describe(&self) -> String {
        let ours = if self.ours.is_empty() {
            String::new()
        } else {
            format!(" (cluster overrides it to {})", self.ours)
        };
        match self.kind {
            FindingKind::Removed => format!("{} is removed in the target{ours}", self.name),
            FindingKind::DefaultChanged => format!(
                "{} default changes from {} to {}{ours}",
                self.name, self.from, self.to
            ),
            FindingKind::TypeChanged => format!(
                "{} changes type from {} to {}{ours}",
                self.name, self.from, self.to
            ),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsReport {
    /// About settings this cluster overrides; approval must acknowledge them.
    pub warnings: Vec<SettingFinding>,
    /// Default changes on settings left at their default.
    pub info: Vec<SettingFinding>,
    /// Why the diff could not be made; also needs acknowledgement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SettingsReport {
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::default()
        }
    }

    /// One line per thing the approver must acknowledge.
    pub fn warning_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.warnings.iter().map(SettingFinding::describe).collect();
        if let Some(error) = &self.error {
            lines.push(format!("settings diff unavailable: {error}"));
        }
        lines
    }

    pub fn info_lines(&self) -> Vec<String> {
        self.info.iter().map(SettingFinding::describe).collect()
    }
}

#[derive(Deserialize)]
struct ListedSetting {
    #[serde(rename = "Setting")]
    name: String,
    #[serde(rename = "Type")]
    typ: String,
    #[serde(rename = "Default")]
    default: String,
}

/// Parses `gen settings-list --format=ndjson`, or a JSON array of the same
/// objects.
pub fn parse_settings_list(text: &str) -> Result<BTreeMap<String, SettingDefault>, String> {
    let trimmed = text.trim_start();
    let listed: Vec<ListedSetting> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed).map_err(|error| format!("bad settings list: {error}"))?
    } else {
        trimmed
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line).map_err(|error| format!("bad settings line: {error}"))
            })
            .collect::<Result<_, _>>()?
    };
    if listed.is_empty() {
        return Err("the settings list is empty".to_string());
    }
    Ok(listed
        .into_iter()
        .map(|setting| {
            (
                setting.name,
                SettingDefault {
                    typ: setting.typ,
                    default: setting.default,
                },
            )
        })
        .collect())
}

/// `SHOW ALL CLUSTER SETTINGS` reports one-letter types; `gen settings-list`
/// spells them out.
pub fn long_type(code: &str) -> String {
    match code {
        "b" => "boolean",
        "i" => "integer",
        "f" => "float",
        "d" => "duration",
        "e" => "enumeration",
        "z" => "byte size",
        "s" => "string",
        "m" => "protobuf",
        "v" => "version",
        other => other,
    }
    .to_string()
}

pub fn redact(name: &str, typ: &str, value: &str) -> String {
    if ALWAYS_REDACTED.contains(&name) || !SHOWN_TYPES.contains(&typ) {
        "(redacted)".to_string()
    } else if value.is_empty() {
        "(empty)".to_string()
    } else {
        value.to_string()
    }
}

pub fn diff(
    overrides: &[Override],
    current: &BTreeMap<String, SettingDefault>,
    target: &BTreeMap<String, SettingDefault>,
) -> SettingsReport {
    let mut report = SettingsReport::default();
    for setting in overrides {
        let ours = redact(&setting.name, &setting.typ, &setting.value);
        let finding = match (current.get(&setting.name), target.get(&setting.name)) {
            (_, None) => Some((FindingKind::Removed, String::new(), String::new())),
            (Some(old), Some(new)) => change(&setting.name, old, new),
            (None, Some(_)) => None,
        };
        if let Some((kind, from, to)) = finding {
            report.warnings.push(SettingFinding {
                name: setting.name.clone(),
                kind,
                from,
                to,
                ours,
            });
        }
    }
    for (name, old) in current {
        if overrides.iter().any(|setting| &setting.name == name) {
            continue;
        }
        if let Some(new) = target.get(name)
            && let Some((kind, from, to)) = change(name, old, new)
        {
            report.info.push(SettingFinding {
                name: name.clone(),
                kind,
                from,
                to,
                ours: String::new(),
            });
        }
    }
    report
}

fn change(
    name: &str,
    old: &SettingDefault,
    new: &SettingDefault,
) -> Option<(FindingKind, String, String)> {
    if old.typ != new.typ {
        Some((FindingKind::TypeChanged, old.typ.clone(), new.typ.clone()))
    } else if old.default != new.default {
        Some((
            FindingKind::DefaultChanged,
            redact(name, &old.typ, &old.default),
            redact(name, &new.typ, &new.default),
        ))
    } else {
        None
    }
}

/// Every setting this cluster overrides, except the two the agent itself
/// manages during an upgrade: the cluster version and the downgrade pin.
pub fn load_overrides(client: &mut Client) -> Result<Vec<Override>, AppError> {
    Ok(client
        .query(
            "SELECT variable, setting_type, value FROM [SHOW ALL CLUSTER SETTINGS] \
             WHERE origin = 'override' \
             AND variable NOT IN ('version', 'cluster.preserve_downgrade_option') \
             ORDER BY variable",
            &[],
        )?
        .iter()
        .map(|row| Override {
            name: row.get(0),
            typ: long_type(row.get(1)),
            value: row.get(2),
        })
        .collect())
}

/// Runs `binary gen settings-list` and parses it.
pub fn gen_settings(binary: &Path) -> Result<BTreeMap<String, SettingDefault>, String> {
    let output = Command::new(binary)
        .args(["gen", "settings-list", "--all-settings", "--format=ndjson"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not run {}: {error}", binary.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} gen settings-list failed: {}",
            binary.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_settings_list(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults(rows: &[(&str, &str, &str)]) -> BTreeMap<String, SettingDefault> {
        rows.iter()
            .map(|(name, typ, default)| {
                (
                    name.to_string(),
                    SettingDefault {
                        typ: typ.to_string(),
                        default: default.to_string(),
                    },
                )
            })
            .collect()
    }

    fn ours(name: &str, typ: &str, value: &str) -> Override {
        Override {
            name: name.to_string(),
            typ: typ.to_string(),
            value: value.to_string(),
        }
    }

    #[test]
    fn diff_flags_removed_changed_and_retyped_overrides() {
        let current = defaults(&[
            ("a.gone", "boolean", "false"),
            ("b.default", "integer", "4"),
            ("c.retyped", "integer", "4"),
            ("d.same", "boolean", "false"),
        ]);
        let target = defaults(&[
            ("b.default", "integer", "8"),
            ("c.retyped", "duration", "4s"),
            ("d.same", "boolean", "false"),
        ]);
        let overrides = [
            ours("a.gone", "boolean", "true"),
            ours("b.default", "integer", "2"),
            ours("c.retyped", "integer", "2"),
            ours("d.same", "boolean", "true"),
        ];
        let report = diff(&overrides, &current, &target);
        let kinds: Vec<_> = report
            .warnings
            .iter()
            .map(|finding| (finding.name.as_str(), finding.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("a.gone", FindingKind::Removed),
                ("b.default", FindingKind::DefaultChanged),
                ("c.retyped", FindingKind::TypeChanged),
            ]
        );
        assert_eq!(
            report.warning_lines()[1],
            "b.default default changes from 4 to 8 (cluster overrides it to 2)"
        );
        assert!(report.info.is_empty());
    }

    #[test]
    fn default_changes_on_untouched_settings_are_info() {
        let current = defaults(&[("x", "boolean", "false"), ("y", "boolean", "true")]);
        let target = defaults(&[("x", "boolean", "true"), ("y", "boolean", "true")]);
        let report = diff(&[], &current, &target);
        assert!(report.warnings.is_empty());
        assert_eq!(
            report.info_lines(),
            ["x default changes from false to true"]
        );
    }

    #[test]
    fn strings_and_denied_names_are_redacted() {
        assert_eq!(redact("cluster.secret", "boolean", "x"), "(redacted)");
        assert_eq!(
            redact("enterprise.license", "string", "crl-0-x"),
            "(redacted)"
        );
        assert_eq!(
            redact("ui.default_timezone", "string", "america"),
            "(redacted)"
        );
        assert_eq!(redact("kv.foo", "duration", "5s"), "5s");
        assert_eq!(redact("kv.foo", "byte size", ""), "(empty)");
        assert_eq!(redact("kv.foo", "protobuf", "x"), "(redacted)");
        let current = defaults(&[("cluster.secret", "string", "a")]);
        let target = defaults(&[("cluster.secret", "string", "b")]);
        let report = diff(
            &[ours("cluster.secret", "string", "s3cret")],
            &current,
            &target,
        );
        let line = &report.warning_lines()[0];
        assert!(!line.contains("s3cret") && !line.contains(" a "), "{line}");
        assert!(line.contains("(redacted)"), "{line}");
    }

    #[test]
    fn ndjson_and_json_both_parse() {
        let ndjson = "{\"Default\":\"true\",\"Description\":\"d\",\"Setting\":\"a\",\"Type\":\"boolean\",\"Visibility\":\"public\"}\n\
                      {\"Default\":\"5ms\",\"Description\":\"d\",\"Setting\":\"b\",\"Type\":\"duration\"}\n";
        let parsed = parse_settings_list(ndjson).unwrap();
        assert_eq!(parsed["b"].default, "5ms");
        let json = "[{\"Default\":\"true\",\"Setting\":\"a\",\"Type\":\"boolean\"}]";
        assert_eq!(parse_settings_list(json).unwrap()["a"].typ, "boolean");
        assert!(parse_settings_list("").is_err());
        assert!(parse_settings_list("Setting\tType\tDefault").is_err());
    }

    #[test]
    fn short_types_are_spelled_out() {
        assert_eq!(long_type("b"), "boolean");
        assert_eq!(long_type("z"), "byte size");
        assert_eq!(long_type("s"), "string");
    }

    #[test]
    fn a_failed_diff_is_a_warning() {
        let report = SettingsReport::failed("could not run cockroach");
        assert_eq!(
            report.warning_lines(),
            ["settings diff unavailable: could not run cockroach"]
        );
    }
}
