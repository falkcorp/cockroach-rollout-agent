// file: src/events.rs
// version: 1.1.0
// guid: 4081c194-4ba9-4683-8e2e-d1b605dbf4f7
// last-edited: 2026-10-12

//! The cluster-wide event log. The audit log is a file on each host, so
//! without this a dashboard on one node cannot see what agents on the other
//! nodes did.

use postgres::Client;

use crate::{AppError, Cli, agent_node_id, audit, sanitize_log_field, sql_ident};

/// Rows kept in `events`; the leader deletes older ones every tick.
pub const EVENTS_KEPT: i64 = 500;

/// Who caused an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Agent,
    Cli,
    Web,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Cli => "cli",
            Self::Web => "web",
        }
    }
}

pub fn record_event(
    cli: &Cli,
    client: &mut Client,
    source: Source,
    kind: &str,
    detail: &str,
) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    let agent_id = agent_node_id(cli)?;
    client.execute(
        &format!(
            "INSERT INTO {schema}.events (agent_id, source, kind, detail) VALUES ($1, $2, $3, $4)"
        ),
        &[
            &agent_id,
            &source.as_str(),
            &kind,
            &sanitize_log_field(detail),
        ],
    )?;
    Ok(())
}

/// Writes the host-local audit line and the cluster-wide event together.
pub fn note(
    cli: &Cli,
    client: &mut Client,
    source: Source,
    kind: &str,
    detail: &str,
) -> Result<(), AppError> {
    audit(cli, kind, detail)?;
    record_event(cli, client, source, kind, detail)
}

/// The newest event's detail of `kind`, for skip-note dedupe.
pub fn last_detail(cli: &Cli, client: &mut Client, kind: &str) -> Result<Option<String>, AppError> {
    let schema = sql_ident(&cli.schema)?;
    Ok(client
        .query(
            &format!("SELECT detail FROM {schema}.events WHERE kind = $1 ORDER BY at DESC LIMIT 1"),
            &[&kind],
        )?
        .first()
        .map(|row| row.get(0)))
}

/// Leader housekeeping: keep the newest `EVENTS_KEPT` events and drop
/// expired UI sessions.
pub fn prune(cli: &Cli, client: &mut Client) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    client.execute(
        &format!(
            "
            DELETE FROM {schema}.events
            WHERE id NOT IN (SELECT id FROM {schema}.events ORDER BY at DESC LIMIT $1)
            "
        ),
        &[&EVENTS_KEPT],
    )?;
    client.execute(
        &format!("DELETE FROM {schema}.ui_sessions WHERE expires_at < now()"),
        &[],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_are_stored_as_lowercase_words() {
        assert_eq!(Source::Agent.as_str(), "agent");
        assert_eq!(Source::Cli.as_str(), "cli");
        assert_eq!(Source::Web.as_str(), "web");
    }
}
