// file: src/web/render.rs
// version: 1.2.0
// guid: 7e2b2274-de88-426c-bdd9-4faeced6f062
// last-edited: 2026-10-13

//! HTML for the web UI. Every dynamic value passes through `escape`, and the
//! public functions take typed values, never pre-built HTML.

use std::fmt::Write as _;

use crate::agentupdate::AgentReleaseRow;
use crate::cluster_view::{AgentRow, ClusterView, EventRow, LeaseRow, RolloutRow};
use crate::health::safe_to_restart;
use crate::ops::AvailableAction;
use crate::releases::{LineMark, ReleasesView};
use crate::rollout::NodeObservation;
use crate::uicert::UiCertRow;

pub const APP_CSS: &str = include_str!("assets/app.css");
pub const APP_JS: &str = include_str!("assets/app.js");
const TITLE: &str = "Cockroach rollout";

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            other => out.push(other),
        }
    }
    out
}

fn layout(body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{TITLE}</title><link rel=\"stylesheet\" href=\"/static/app.css\">\
         <script src=\"/static/app.js\" defer></script></head><body>{body}</body></html>"
    )
}

fn csrf_field(csrf: &str) -> String {
    format!(
        "<input type=\"hidden\" name=\"csrf\" value=\"{}\">",
        escape(csrf)
    )
}

pub fn login_page(error: Option<&str>) -> String {
    let error = error
        .map(|error| format!("<p class=\"error\">{}</p>", escape(error)))
        .unwrap_or_default();
    layout(&format!(
        "<main class=\"login\"><h1>{TITLE}</h1>{error}\
         <form method=\"post\" action=\"/login\">\
         <label for=\"password\">Password</label>\
         <input id=\"password\" name=\"password\" type=\"password\" autocomplete=\"current-password\" required autofocus>\
         <button type=\"submit\">Sign in</button></form></main>"
    ))
}

pub fn password_unset_page() -> String {
    message_page(
        "UI password not set",
        "Run `cockroach-rollout-agent set-ui-password` on any node, then reload this page.",
    )
}

pub fn message_page(heading: &str, text: &str) -> String {
    layout(&format!(
        "<main class=\"login\"><h1>{}</h1><p>{}</p></main>",
        escape(heading),
        escape(text)
    ))
}

pub fn db_unreachable(error: &str) -> String {
    layout(&format!(
        "<header><h1>{TITLE}</h1></header><main id=\"main\">\
         <p class=\"banner\">database unreachable: {}</p></main>",
        escape(error)
    ))
}

pub fn dashboard(
    view: &ClusterView,
    actions: &[AvailableAction],
    csrf: &str,
    flash: Option<&str>,
) -> String {
    let mut main = String::new();
    if let Some(flash) = flash {
        let _ = write!(main, "<p class=\"flash\">{}</p>", escape(flash));
    }
    main.push_str(&actions_section(actions, csrf));
    main.push_str(&rollouts_section(&view.rollouts));
    if let Some(releases) = &view.releases {
        main.push_str(&releases_section(releases, csrf));
    }
    main.push_str(&nodes_section(&view.nodes));
    main.push_str(&agents_section(&view.agents, view.stale_after_secs));
    main.push_str(&agent_update_section(
        &view.agent_releases,
        &view.agents,
        &view.leases,
        csrf,
    ));
    main.push_str(&ui_cert_section(&view.ui_certs, &view.agents, csrf));
    main.push_str(&locks_section(&view.leases));
    main.push_str(&health_section(view));
    main.push_str(&events_section(&view.events));
    layout(&format!(
        "<header><h1>{TITLE}</h1><form method=\"post\" action=\"/logout\">{}\
         <button type=\"submit\">Sign out</button></form></header><main id=\"main\">{main}</main>",
        csrf_field(csrf)
    ))
}

/// A table whose cells are plain text; every cell is escaped here.
fn table(heading: &str, columns: &[&str], rows: Vec<Vec<String>>, empty: &str) -> String {
    let mut html = format!("<section><h2>{}</h2>", escape(heading));
    if rows.is_empty() {
        let _ = write!(html, "<p>{}</p></section>", escape(empty));
        return html;
    }
    html.push_str("<div class=\"scroll\"><table><thead><tr>");
    for column in columns {
        let _ = write!(html, "<th>{}</th>", escape(column));
    }
    html.push_str("</tr></thead><tbody>");
    for row in rows {
        html.push_str("<tr>");
        for cell in row {
            let _ = write!(html, "<td>{}</td>", escape(&cell));
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table></div></section>");
    html
}

fn rollouts_section(rollouts: &[RolloutRow]) -> String {
    let rows = rollouts
        .iter()
        .map(|rollout| {
            vec![
                rollout.kind.as_str().to_string(),
                rollout.status.as_str().to_string(),
                format!("{} → {}", rollout.from, rollout.to),
                format!("{} at {}", rollout.created_by, rollout.created_at),
                match (&rollout.approved_by, &rollout.approved_at) {
                    (Some(by), Some(at)) => format!("{by} at {at}"),
                    (Some(by), None) => by.clone(),
                    _ => "—".to_string(),
                },
                rollout.error.clone().unwrap_or_default(),
            ]
        })
        .collect();
    table(
        "Rollouts",
        &["Kind", "Status", "Versions", "Created", "Approved", "Error"],
        rows,
        "No rollouts yet.",
    )
}

/// Only http(s) links are rendered, so a hostile URL cannot run script.
fn link(url: &str, text: &str) -> String {
    if url.starts_with("https://") || url.starts_with("http://") {
        format!(
            "<a href=\"{}\" rel=\"noreferrer\">{}</a>",
            escape(url),
            escape(text)
        )
    } else {
        escape(text)
    }
}

fn releases_section(releases: &ReleasesView, csrf: &str) -> String {
    let mut html = String::from("<section><h2>Releases</h2>");
    if releases.lines.is_empty() {
        html.push_str("<p>No release lines to show yet.</p>");
    } else {
        html.push_str(
            "<div class=\"scroll\"><table><thead><tr><th>Line</th><th>Latest</th>\
             <th>Status</th><th>Notes</th></tr></thead><tbody>",
        );
        for line in &releases.lines {
            let status = match line.mark {
                LineMark::Current => match &releases.current {
                    Some(current) if *current < line.latest => {
                        format!("cluster runs this line ({current})")
                    }
                    _ => "cluster runs this line".to_string(),
                },
                LineMark::Proposed => "proposed".to_string(),
                LineMark::Step(step) => format!("step {step}"),
            };
            let _ = write!(
                html,
                "<tr><td>{}.{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                line.line.0,
                line.line.1,
                escape(&line.latest.to_string()),
                escape(&status),
                link(&line.notes_url, "release notes")
            );
        }
        html.push_str("</tbody></table></div>");
    }
    let checked = match releases.fetched_age_secs {
        Some(secs) if secs < 120 => "checked just now".to_string(),
        Some(secs) => format!("checked {} min ago", secs / 60),
        None => "not checked yet".to_string(),
    };
    let _ = write!(html, "<p>{}", escape(&checked));
    if let Some(error) = &releases.last_error {
        let _ = write!(html, "; last check failed: {}", escape(error));
    }
    html.push_str("</p>");
    if releases.refresh_requested {
        html.push_str("<p>A check is queued for the leader's next tick.</p>");
    } else {
        let _ = write!(
            html,
            "<form method=\"post\" action=\"/actions/check-releases\">{}\
             <button type=\"submit\">Check now</button></form>",
            csrf_field(csrf)
        );
    }
    html.push_str("</section>");
    html
}

fn nodes_section(nodes: &[NodeObservation]) -> String {
    let rows = nodes
        .iter()
        .map(|node| {
            vec![
                format!("n{}", node.node_id),
                if node.is_live { "live" } else { "DOWN" }.to_string(),
                node.version
                    .as_ref()
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
            ]
        })
        .collect();
    table(
        "Nodes",
        &["Node", "Live", "Version"],
        rows,
        "No nodes observed.",
    )
}

fn agents_section(agents: &[AgentRow], stale_after_secs: u64) -> String {
    let rows = agents
        .iter()
        .map(|agent| {
            vec![
                agent.observation.agent_id.clone(),
                agent
                    .observation
                    .node_id
                    .map(|id| format!("n{id}"))
                    .unwrap_or_else(|| "—".to_string()),
                agent.state.clone(),
                agent.version.clone(),
                agent
                    .observation
                    .agent_version
                    .clone()
                    .unwrap_or_else(|| "—".to_string()),
                if agent.is_stale(stale_after_secs) {
                    format!("STALE ({}s)", agent.age_secs)
                } else {
                    format!("{}s ago", agent.age_secs)
                },
                agent.error.clone().unwrap_or_default(),
            ]
        })
        .collect();
    table(
        "Agents",
        &[
            "Agent",
            "Node",
            "State",
            "CockroachDB",
            "Agent version",
            "Last seen",
            "Error",
        ],
        rows,
        "No agents have reported.",
    )
}

fn utc_date(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .map(|at| at.date().to_string())
        .unwrap_or_else(|_| unix.to_string())
}

fn cert_details(row: &UiCertRow) -> String {
    let fp = &row.info.fingerprint;
    let file = |name: &str, label: &str| {
        format!(
            "<a href=\"/ui-cert/{}/{}\">{}</a>",
            escape(fp),
            name,
            escape(label)
        )
    };
    format!(
        "<dl><dt>Names</dt><dd>{}</dd><dt>Valid</dt><dd>{} to {}</dd>\
         <dt>SHA-256</dt><dd><code>{}</code></dd><dt>Issuer</dt><dd>{}</dd>\
         <dt>Staged by</dt><dd>{} at {}</dd></dl><p>{} · {} · {} (download is logged)</p>",
        escape(&row.info.hosts().describe()),
        utc_date(row.info.not_before),
        utc_date(row.info.not_after),
        escape(fp),
        escape(&row.info.issuer),
        escape(&row.created_by),
        escape(&row.created_at),
        file("ca.crt", "UI CA certificate"),
        file("cert.pem", "certificate"),
        file("key.pem", "private key"),
    )
}

fn ui_cert_section(certs: &[UiCertRow], agents: &[AgentRow], csrf: &str) -> String {
    let mut html = String::from("<section><h2>UI certificate</h2>");
    let active = certs.iter().find(|row| row.status == "approved");
    let staged = certs.iter().find(|row| row.status == "staged");
    match active {
        Some(row) => {
            html.push_str("<h3>Approved</h3>");
            html.push_str(&cert_details(row));
        }
        None => html.push_str(
            "<p>No UI certificate is approved; agents serve their CockroachDB node \
             certificates.</p>",
        ),
    }
    if let Some(row) = staged {
        let fp = escape(&row.info.fingerprint);
        let field = format!(
            "{}<input type=\"hidden\" name=\"fingerprint\" value=\"{fp}\">",
            csrf_field(csrf)
        );
        html.push_str("<h3>Staged, waiting for approval</h3>");
        html.push_str(&cert_details(row));
        let _ = write!(
            html,
            "<form method=\"post\" action=\"/actions/ui-cert-approve\" class=\"action\">{field}\
             <p>Every agent installs it on its next tick and keeps serving the old one \
             if it cannot.</p><button type=\"submit\">Approve certificate</button></form>\
             <form method=\"post\" action=\"/actions/ui-cert-reject\">{field}\
             <button type=\"submit\">Reject</button></form>"
        );
    }
    let rows = agents
        .iter()
        .map(|agent| {
            vec![
                agent.observation.agent_id.clone(),
                agent
                    .ui_cert_fingerprint
                    .as_deref()
                    .map(|fp| match active {
                        Some(row) if row.info.fingerprint == fp => "approved".to_string(),
                        _ => format!("{}…", &fp[..fp.len().min(12)]),
                    })
                    .unwrap_or_else(|| "node certificate".to_string()),
                agent.ui_cert_error.clone().unwrap_or_default(),
            ]
        })
        .collect::<Vec<_>>();
    if !rows.is_empty() {
        html.push_str(
            "<div class=\"scroll\"><table><thead><tr><th>Agent</th><th>Serving</th>\
                       <th>Install error</th></tr></thead><tbody>",
        );
        for row in rows {
            html.push_str("<tr>");
            for cell in row {
                let _ = write!(html, "<td>{}</td>", escape(&cell));
            }
            html.push_str("</tr>");
        }
        html.push_str("</tbody></table></div>");
    }
    let _ = write!(
        html,
        "<details><summary>Stage a certificate by hand</summary>\
         <form method=\"post\" action=\"/actions/ui-cert-upload\" class=\"action\">{}\
         <p>It must chain to the UI CA and is only staged: it still needs approval.</p>\
         <label>Certificate PEM <textarea name=\"cert_pem\" rows=\"6\" required></textarea></label>\
         <label>Private key PEM <textarea name=\"key_pem\" rows=\"6\" required></textarea></label>\
         <button type=\"submit\">Stage</button></form></details></section>",
        csrf_field(csrf)
    );
    html
}

fn agent_update_section(
    releases: &[AgentReleaseRow],
    agents: &[AgentRow],
    leases: &[LeaseRow],
    csrf: &str,
) -> String {
    let mut html = String::from("<section><h2>Agent updates</h2>");
    let leader = leases
        .iter()
        .find(|lease| lease.name == crate::LEADER_LEASE)
        .and_then(|lease| lease.holder.as_deref())
        .and_then(|holder| {
            agents
                .iter()
                .find(|agent| agent.observation.agent_id == holder)
        });
    if let Some((leader, version)) = leader.and_then(|agent| {
        let version = agent.observation.agent_version.as_deref()?;
        semver::Version::parse(version)
            .ok()
            .filter(|version| *version < crate::agentupdate::MIN_SELF_UPDATE_VERSION)
            .map(|_| (agent, version))
    }) {
        let _ = write!(
            html,
            "<p>The leader, {}, runs agent {} and cannot propose agent updates. \
             Reinstall it with install-rollout-agent.sh.</p>",
            escape(&leader.observation.agent_id),
            escape(version)
        );
    }
    let open: Vec<&AgentReleaseRow> = releases
        .iter()
        .filter(|row| matches!(row.status.as_str(), "proposed" | "approved"))
        .collect();
    if open.is_empty() {
        html.push_str("<p>No agent release is waiting for approval or rolling out.</p>");
    }
    for row in open {
        let version = escape(&row.version.to_string());
        let field = format!(
            "{}<input type=\"hidden\" name=\"version\" value=\"{version}\">",
            csrf_field(csrf)
        );
        let _ = write!(
            html,
            "<h3>v{version}: {}</h3><p>{}</p><div class=\"scroll\"><table><thead><tr>\
             <th>Architecture</th><th>SHA-256</th></tr></thead><tbody>",
            escape(&row.status),
            link(&row.html_url, "release page")
        );
        for (arch, asset) in &row.assets {
            let _ = write!(
                html,
                "<tr><td>{}</td><td><code>{}</code></td></tr>",
                escape(arch),
                escape(&asset.sha256)
            );
        }
        html.push_str("</tbody></table></div>");
        if row.status == "proposed" {
            let _ = write!(
                html,
                "<form method=\"post\" action=\"/actions/agent-update-approve\" class=\"action\">\
                 {field}<p>Agents download exactly these builds, refuse anything else, and \
                 update one at a time. A build that does not settle is rolled back.</p>\
                 <button type=\"submit\">Approve agent v{version}</button></form>\
                 <form method=\"post\" action=\"/actions/agent-update-decline\">{field}\
                 <button type=\"submit\">Decline</button></form>"
            );
        } else {
            let _ = write!(
                html,
                "<form method=\"post\" action=\"/actions/agent-update-decline\">{field}\
                 <button type=\"submit\">Stop this update</button></form>"
            );
        }
    }
    let rows: Vec<[String; 3]> = agents
        .iter()
        .map(|agent| {
            [
                agent.observation.agent_id.clone(),
                agent
                    .observation
                    .agent_version
                    .clone()
                    .unwrap_or_else(|| "—".to_string()),
                crate::agentupdate::progress(
                    agent.observation.agent_version.as_deref(),
                    agent.agent_update_to.as_deref(),
                    agent.agent_update_error.as_deref(),
                ),
            ]
        })
        .collect();
    if !rows.is_empty() {
        html.push_str(
            "<div class=\"scroll\"><table><thead><tr><th>Agent</th><th>Version</th>\
             <th>Update</th></tr></thead><tbody>",
        );
        for row in rows {
            html.push_str("<tr>");
            for cell in row {
                let _ = write!(html, "<td>{}</td>", escape(&cell));
            }
            html.push_str("</tr>");
        }
        html.push_str("</tbody></table></div>");
    }
    if agents
        .iter()
        .any(|agent| agent.agent_update_to.is_some() && agent.agent_update_error.is_some())
    {
        let _ = write!(
            html,
            "<form method=\"post\" action=\"/actions/agent-update-retry\">{}\
             <button type=\"submit\">Retry failed agent updates</button></form>",
            csrf_field(csrf)
        );
    }
    html.push_str("</section>");
    html
}

fn locks_section(leases: &[LeaseRow]) -> String {
    let rows = leases
        .iter()
        .map(|lease| {
            vec![
                lease.name.clone(),
                lease.holder.clone().unwrap_or_else(|| "free".to_string()),
                lease.expires_at.clone().unwrap_or_default(),
            ]
        })
        .collect();
    table("Locks", &["Lock", "Holder", "Expires"], rows, "No locks.")
}

fn health_section(view: &ClusterView) -> String {
    let text = match &view.health {
        Ok(health) => format!(
            "{}/{} nodes live, {} under-replicated, {} unavailable ranges; restart gate: {}",
            health.live_nodes,
            health.active_nodes,
            health.underreplicated_ranges,
            health.unavailable_ranges,
            match safe_to_restart(health) {
                Ok(()) => "open".to_string(),
                Err(reason) => format!("blocked — {reason}"),
            }
        ),
        Err(error) => format!("health unavailable: {error}"),
    };
    format!("<section><h2>Health</h2><p>{}</p></section>", escape(&text))
}

fn events_section(events: &[EventRow]) -> String {
    let rows = events
        .iter()
        .map(|event| {
            vec![
                event.at.clone(),
                event.agent_id.clone(),
                event.source.clone(),
                event.kind.clone(),
                event.detail.clone(),
            ]
        })
        .collect();
    table(
        "Recent events",
        &["At", "Agent", "Source", "Event", "Detail"],
        rows,
        "No events yet.",
    )
}

fn actions_section(actions: &[AvailableAction], csrf: &str) -> String {
    let mut html = String::from("<section><h2>Actions</h2>");
    if actions.is_empty() {
        html.push_str("<p>No actions are available right now.</p>");
    }
    for action in actions {
        html.push_str(&action_form(action, csrf));
    }
    html.push_str("</section>");
    html
}

fn action_form(action: &AvailableAction, csrf: &str) -> String {
    let csrf = csrf_field(csrf);
    match action {
        AvailableAction::Approve {
            version,
            warnings,
            release_notes_url,
            settings_warnings,
            settings_info,
        } => {
            let version = escape(&version.to_string());
            let notes = if release_notes_url.starts_with("https://")
                || release_notes_url.starts_with("http://")
            {
                format!("<p>{}</p>", link(release_notes_url, "Release notes"))
            } else {
                String::new()
            };
            let mut acknowledge = String::new();
            if !warnings.is_empty() {
                let _ = write!(
                    acknowledge,
                    "<p class=\"warning\">Release notes matched: {}</p>",
                    escape(&warnings.join(", "))
                );
            }
            if !settings_warnings.is_empty() {
                acknowledge.push_str(
                    "<p class=\"warning\">Overridden cluster settings this upgrade touches:</p><ul>",
                );
                for line in settings_warnings {
                    let _ = write!(acknowledge, "<li>{}</li>", escape(line));
                }
                acknowledge.push_str("</ul>");
            }
            if !settings_info.is_empty() {
                let _ = write!(
                    acknowledge,
                    "<details><summary>{} other defaults change</summary><ul>",
                    settings_info.len()
                );
                for line in settings_info {
                    let _ = write!(acknowledge, "<li>{}</li>", escape(line));
                }
                acknowledge.push_str("</ul></details>");
            }
            if !warnings.is_empty() || !settings_warnings.is_empty() {
                acknowledge.push_str(
                    "<label><input type=\"checkbox\" name=\"accept_warnings\" required> \
                     I reviewed the warnings</label>",
                );
            }
            format!(
                "<form method=\"post\" action=\"/actions/approve\" class=\"action\">{csrf}\
                 <input type=\"hidden\" name=\"version\" value=\"{version}\">\
                 <h3>Approve upgrade to {version}</h3>{notes}{acknowledge}\
                 <button type=\"submit\">Approve</button></form>"
            )
        }
        AvailableAction::Decline { rollout_id, target } => format!(
            "<form method=\"post\" action=\"/actions/decline\" class=\"action\">{csrf}\
             <input type=\"hidden\" name=\"rollout_id\" value=\"{}\">\
             <h3>Decline {}</h3>\
             <p>Not now: the leader will not propose this version again. \
             A newer patch is still proposed.</p>\
             <label>Reason <input name=\"reason\" autocomplete=\"off\"></label>\
             <button type=\"submit\">Decline</button></form>",
            escape(rollout_id),
            escape(&target.to_string())
        ),
        AvailableAction::Cancel => format!(
            "<form method=\"post\" action=\"/actions/cancel\" class=\"action\">{csrf}\
             <h3>Cancel the open rollout</h3>\
             <label>Reason <input name=\"reason\" required autocomplete=\"off\"></label>\
             <button type=\"submit\">Cancel rollout</button></form>"
        ),
        AvailableAction::Finalize { target } => {
            let target = escape(&target.to_string());
            format!(
                "<form method=\"post\" action=\"/actions/finalize\" class=\"action\">{csrf}\
                 <h3>Finalize {target}</h3>\
                 <p class=\"warning\">Finalizing closes the downgrade window for good.</p>\
                 <label>Type {target} to confirm \
                 <input name=\"confirm_version\" required autocomplete=\"off\"></label>\
                 <button type=\"submit\">Finalize</button></form>"
            )
        }
        AvailableAction::Restart { version } => format!(
            "<form method=\"post\" action=\"/actions/restart\" class=\"action\">{csrf}\
             <h3>Rolling restart</h3>\
             <p>Restarts every node on {}, one at a time, behind the health gate.</p>\
             <label>Type restart to confirm \
             <input name=\"confirm\" required autocomplete=\"off\"></label>\
             <button type=\"submit\">Start rolling restart</button></form>",
            escape(&version.to_string())
        ),
        AvailableAction::RestartBlocked { reason } => format!(
            "<div class=\"action\"><h3>Rolling restart unavailable</h3><p>{}</p></div>",
            escape(reason)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_view::EventRow;
    use semver::Version;

    #[test]
    fn escape_covers_html_metacharacters() {
        assert_eq!(
            escape(r#"<a href="x">'&'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&#x27;&amp;&#x27;&lt;/a&gt;"
        );
    }

    #[test]
    fn hostile_text_is_escaped_everywhere() {
        let mut view = ClusterView::sample();
        view.agents[0].error = Some("<script>alert(1)</script>".to_string());
        view.events.push(EventRow {
            at: "2026-10-11".to_string(),
            agent_id: "host1:amd64".to_string(),
            source: "agent".to_string(),
            kind: "gate_wait".to_string(),
            detail: "<img src=x onerror=alert(2)>".to_string(),
        });
        let html = dashboard(&view, &[], "tok", Some("<b>flash</b>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("<script>alert"));
        assert!(!html.contains("<img src=x"));
        assert!(!html.contains("<b>flash"));
    }

    #[test]
    fn approve_asks_for_acknowledgement_only_with_warnings() {
        let view = ClusterView::sample();
        let quiet = AvailableAction::Approve {
            version: Version::new(25, 4, 17),
            warnings: Vec::new(),
            release_notes_url: "https://www.cockroachlabs.com/docs/releases/v25.4".to_string(),
            settings_warnings: Vec::new(),
            settings_info: Vec::new(),
        };
        let html = dashboard(&view, &[quiet], "tok", None);
        assert!(html.contains("action=\"/actions/approve\""));
        assert!(!html.contains("accept_warnings"));
        assert!(html.contains("href=\"https://www.cockroachlabs.com/docs/releases/v25.4\""));

        let warned = AvailableAction::Approve {
            version: Version::new(25, 4, 17),
            warnings: vec!["removed".to_string()],
            release_notes_url: "javascript:alert(1)".to_string(),
            settings_warnings: Vec::new(),
            settings_info: Vec::new(),
        };
        let html = dashboard(&view, &[warned], "tok", None);
        assert!(html.contains("name=\"accept_warnings\""));
        assert!(
            !html.contains("javascript:"),
            "only http(s) links are rendered"
        );
    }

    #[test]
    fn every_form_carries_the_csrf_token() {
        let view = ClusterView::sample();
        let actions = [
            AvailableAction::Cancel,
            AvailableAction::Finalize {
                target: Version::new(25, 4, 17),
            },
            AvailableAction::Restart {
                version: Version::new(25, 4, 17),
            },
        ];
        let html = dashboard(&view, &actions, "tok123", None);
        let forms = html.matches("<form").count();
        assert_eq!(
            forms, 5,
            "three actions, the certificate upload, and sign out"
        );
        assert_eq!(
            html.matches("name=\"csrf\" value=\"tok123\"").count(),
            forms
        );
    }

    #[test]
    fn blocked_restart_shows_its_reason_without_a_form() {
        let view = ClusterView::sample();
        let html = dashboard(
            &view,
            &[AvailableAction::RestartBlocked {
                reason: "node 3 has no agent".to_string(),
            }],
            "tok",
            None,
        );
        assert!(html.contains("node 3 has no agent"));
        assert!(!html.contains("/actions/restart"));
    }

    #[test]
    fn decline_posts_the_rollout_id_with_csrf() {
        let view = ClusterView::sample();
        let decline = AvailableAction::Decline {
            rollout_id: "r-<1>".to_string(),
            target: Version::new(26, 1, 8),
        };
        let html = dashboard(&view, &[decline], "tok", None);
        assert!(html.contains("action=\"/actions/decline\""));
        assert!(html.contains("name=\"rollout_id\" value=\"r-&lt;1&gt;\""));
        assert!(html.contains("Decline 26.1.8"));
        assert_eq!(
            html.matches("name=\"csrf\" value=\"tok\"").count(),
            html.matches("<form").count()
        );
    }

    #[test]
    fn releases_panel_marks_the_path_and_offers_a_check() {
        use crate::releases::{LineMark, ReleaseLine, ReleasesView};
        let mut view = ClusterView::sample();
        assert!(
            !dashboard(&view, &[], "tok", None).contains("Releases"),
            "no catalog yet, no panel"
        );
        view.releases = Some(ReleasesView {
            current: Some(Version::new(25, 4, 17)),
            lines: vec![
                ReleaseLine {
                    line: (26, 2),
                    latest: Version::new(26, 2, 7),
                    mark: LineMark::Step(2),
                    notes_url: "https://www.cockroachlabs.com/docs/releases/v26.2".to_string(),
                },
                ReleaseLine {
                    line: (26, 1),
                    latest: Version::new(26, 1, 8),
                    mark: LineMark::Proposed,
                    notes_url: "https://www.cockroachlabs.com/docs/releases/v26.1".to_string(),
                },
                ReleaseLine {
                    line: (25, 4),
                    latest: Version::new(25, 4, 17),
                    mark: LineMark::Current,
                    notes_url: "https://www.cockroachlabs.com/docs/releases/v25.4".to_string(),
                },
            ],
            fetched_age_secs: Some(720),
            refresh_requested: false,
            last_error: Some("<rate limited>".to_string()),
        });
        let html = dashboard(&view, &[], "tok", None);
        assert!(html.contains("<h2>Releases</h2>"));
        for text in [
            "26.2.7",
            "step 2",
            "proposed",
            "cluster runs this line",
            "checked 12 min ago",
        ] {
            assert!(html.contains(text), "{text} missing");
        }
        assert!(html.contains("href=\"https://www.cockroachlabs.com/docs/releases/v26.1\""));
        assert!(html.contains("last check failed: &lt;rate limited&gt;"));
        assert!(html.contains("action=\"/actions/check-releases\""));
        assert_eq!(
            html.matches("name=\"csrf\" value=\"tok\"").count(),
            html.matches("<form").count()
        );
    }

    #[test]
    fn settings_findings_show_on_the_approve_card() {
        let view = ClusterView::sample();
        let approve = AvailableAction::Approve {
            version: Version::new(26, 1, 8),
            warnings: Vec::new(),
            release_notes_url: String::new(),
            settings_warnings: vec!["kv.<x> is removed in the target".to_string()],
            settings_info: vec![
                "a default changes".to_string(),
                "b default changes".to_string(),
            ],
        };
        let html = dashboard(&view, &[approve], "tok", None);
        assert!(html.contains("kv.&lt;x&gt; is removed in the target"));
        assert!(
            html.contains("name=\"accept_warnings\""),
            "settings warnings need the checkbox"
        );
        assert!(html.contains("<details><summary>2 other defaults change</summary>"));
    }

    #[test]
    fn ui_cert_panel_shows_staged_details_downloads_and_bound_decisions() {
        use crate::uicert::{CertInfo, UiCertRow};
        let fp = "ab".repeat(32);
        let mut view = ClusterView::sample();
        view.agents[0].ui_cert_fingerprint = Some(fp.clone());
        view.agents[1].ui_cert_error = Some("not <among> the names".to_string());
        view.ui_certs = vec![UiCertRow {
            info: CertInfo {
                fingerprint: fp.clone(),
                subject: "CN=cockroach-rollout-agent UI".to_string(),
                issuer: "CN=Rollout UI CA".to_string(),
                dns: vec!["len-serv-001".to_string()],
                ips: vec!["172.16.3.92".parse().unwrap()],
                not_before: 0,
                not_after: 86_400 * 397,
            },
            status: "staged".to_string(),
            created_by: "len-serv-001:amd64".to_string(),
            created_at: "2026-10-12".to_string(),
            decided_by: None,
        }];
        let html = dashboard(&view, &[], "tok", None);
        assert!(html.contains("<h2>UI certificate</h2>"));
        assert!(html.contains("len-serv-001, 172.16.3.92"));
        assert!(html.contains(&format!("href=\"/ui-cert/{fp}/key.pem\"")));
        assert!(html.contains(&format!("href=\"/ui-cert/{fp}/ca.crt\"")));
        assert!(html.contains("action=\"/actions/ui-cert-approve\""));
        assert!(html.contains("action=\"/actions/ui-cert-reject\""));
        assert_eq!(
            html.matches(&format!("name=\"fingerprint\" value=\"{fp}\""))
                .count(),
            2
        );
        assert!(html.contains("not &lt;among&gt; the names"));
        assert!(html.contains("action=\"/actions/ui-cert-upload\""));
        let forms = html.matches("<form").count();
        assert_eq!(html.matches("name=\"csrf\" value=\"tok\"").count(), forms);
    }

    #[test]
    fn agent_update_panel_shows_digests_progress_and_bound_decisions() {
        use crate::agentupdate::{AgentReleaseRow, Asset};
        let digest = "cd".repeat(32);
        let mut view = ClusterView::sample();
        for agent in &mut view.agents {
            agent.observation.agent_version = Some("0.3.0".to_string());
        }
        view.agents[0].agent_update_to = Some("0.3.1".to_string());
        view.agents[1].agent_update_error = Some("runs /usr/<local>/bin".to_string());
        view.agents[2].agent_update_to = Some("0.3.1".to_string());
        view.agents[2].agent_update_error = Some("self-check failed".to_string());
        view.agent_releases = vec![
            AgentReleaseRow {
                version: Version::new(0, 3, 1),
                status: "proposed".to_string(),
                html_url: "https://github.com/o/r/releases/tag/v0.3.1".to_string(),
                assets: [("amd64", "https://dl/a"), ("arm64", "https://dl/b")]
                    .into_iter()
                    .map(|(arch, url)| {
                        (
                            arch.to_string(),
                            Asset {
                                url: url.to_string(),
                                sha256: digest.clone(),
                            },
                        )
                    })
                    .collect(),
                proposed_at: "2026-10-13".to_string(),
                decided_by: None,
            },
            AgentReleaseRow {
                version: Version::new(0, 3, 0),
                status: "declined".to_string(),
                html_url: String::new(),
                assets: Default::default(),
                proposed_at: "2026-10-12".to_string(),
                decided_by: Some("cli".to_string()),
            },
        ];
        let html = dashboard(&view, &[], "tok", None);
        assert!(html.contains("<h2>Agent updates</h2>"));
        assert_eq!(html.matches(&digest).count(), 2, "full digest per arch");
        assert!(html.contains("action=\"/actions/agent-update-approve\""));
        assert!(html.contains("action=\"/actions/agent-update-decline\""));
        assert_eq!(html.matches("name=\"version\" value=\"0.3.1\"").count(), 2);
        assert!(
            !html.contains("value=\"0.3.0\""),
            "settled releases offer nothing"
        );
        assert!(html.contains("updating to 0.3.1"));
        assert!(html.contains("runs /usr/&lt;local&gt;/bin"));
        assert!(html.contains("update to 0.3.1 failed: self-check failed"));
        assert!(html.contains("action=\"/actions/agent-update-retry\""));
        assert!(!html.contains("cannot propose"));
        let forms = html.matches("<form").count();
        assert_eq!(html.matches("name=\"csrf\" value=\"tok\"").count(), forms);
    }

    #[test]
    fn an_old_leader_and_old_agents_are_called_out() {
        let html = dashboard(&ClusterView::sample(), &[], "tok", None);
        assert!(html.contains("The leader, host1:amd64, runs agent 0.1.0 and cannot propose"));
        assert!(html.contains("cannot update itself before 0.3.0"));
    }
}
