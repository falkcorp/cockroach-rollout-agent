// file: src/web/render.rs
// version: 1.1.0
// guid: 7e2b2274-de88-426c-bdd9-4faeced6f062
// last-edited: 2026-10-12

//! HTML for the web UI. Every dynamic value passes through `escape`, and the
//! public functions take typed values, never pre-built HTML.

use std::fmt::Write as _;

use crate::cluster_view::{AgentRow, ClusterView, EventRow, LeaseRow, RolloutRow};
use crate::health::safe_to_restart;
use crate::ops::AvailableAction;
use crate::releases::{LineMark, ReleasesView};
use crate::rollout::NodeObservation;

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
        } => {
            let version = escape(&version.to_string());
            let notes = if release_notes_url.starts_with("https://")
                || release_notes_url.starts_with("http://")
            {
                format!("<p>{}</p>", link(release_notes_url, "Release notes"))
            } else {
                String::new()
            };
            let acknowledge = if warnings.is_empty() {
                String::new()
            } else {
                format!(
                    "<p class=\"warning\">Release notes matched: {}</p>\
                     <label><input type=\"checkbox\" name=\"accept_warnings\" required> \
                     I reviewed the release-note warnings</label>",
                    escape(&warnings.join(", "))
                )
            };
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
        };
        let html = dashboard(&view, &[quiet], "tok", None);
        assert!(html.contains("action=\"/actions/approve\""));
        assert!(!html.contains("accept_warnings"));
        assert!(html.contains("href=\"https://www.cockroachlabs.com/docs/releases/v25.4\""));

        let warned = AvailableAction::Approve {
            version: Version::new(25, 4, 17),
            warnings: vec!["removed".to_string()],
            release_notes_url: "javascript:alert(1)".to_string(),
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
        assert_eq!(forms, 4, "three actions plus sign out");
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
        assert_eq!(html.matches("name=\"csrf\" value=\"tok\"").count(), 2);
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
        assert_eq!(html.matches("name=\"csrf\" value=\"tok\"").count(), 2);
    }
}
