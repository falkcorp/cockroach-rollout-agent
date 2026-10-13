// file: src/web/db.rs
// version: 1.1.0
// guid: 143f3009-b250-465e-80ae-1e93b2544162
// last-edited: 2026-10-12

//! `Backend` over the coordination database. Every method opens its own
//! connection; the caller runs it inside `spawn_blocking`, so the blocking
//! client never touches an async worker.

use std::sync::Arc;
use std::time::Duration;

use postgres::Client;

use super::{Backend, WebAction, auth};
use crate::cluster_view::{self, ClusterView};
use crate::events::{self, Source};
use crate::ops::{self, Actor};
use crate::{Cli, db_client, sql_ident};

pub struct DbBackend {
    cli: Arc<Cli>,
}

impl DbBackend {
    pub fn new(cli: Cli) -> Self {
        Self { cli: Arc::new(cli) }
    }

    fn client(&self) -> Result<Client, String> {
        db_client(&self.cli).map_err(|error| error.to_string())
    }

    fn schema(&self) -> Result<String, String> {
        sql_ident(&self.cli.schema).map_err(|error| error.to_string())
    }
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

impl Backend for DbBackend {
    fn password_hash(&self) -> Result<Option<String>, String> {
        let schema = self.schema()?;
        let rows = self
            .client()?
            .query(
                &format!("SELECT password_hash FROM {schema}.ui_auth WHERE id = 1"),
                &[],
            )
            .map_err(text)?;
        Ok(rows.first().map(|row| row.get(0)))
    }

    fn create_session(
        &self,
        token_sha256: &str,
        csrf_token: &str,
        ttl: Duration,
        client_ip: &str,
    ) -> Result<(), String> {
        let schema = self.schema()?;
        let seconds = ttl.as_secs() as i64;
        self.client()?
            .execute(
                &format!(
                    "
                    INSERT INTO {schema}.ui_sessions (token_sha256, csrf_token, expires_at, client_ip)
                    VALUES ($1, $2, now() + ($3::INT8 * INTERVAL '1 second'), $4)
                    "
                ),
                &[&token_sha256, &csrf_token, &seconds, &client_ip],
            )
            .map_err(text)?;
        Ok(())
    }

    fn session(&self, token_sha256: &str) -> Result<Option<auth::Session>, String> {
        let schema = self.schema()?;
        let rows = self
            .client()?
            .query(
                &format!(
                    "
                    SELECT csrf_token, extract(epoch FROM expires_at)::INT8
                    FROM {schema}.ui_sessions
                    WHERE token_sha256 = $1 AND expires_at > now()
                    "
                ),
                &[&token_sha256],
            )
            .map_err(text)?;
        Ok(rows.first().map(|row| auth::Session {
            csrf_token: row.get(0),
            expires_unix: row.get(1),
        }))
    }

    fn delete_session(&self, token_sha256: &str) -> Result<(), String> {
        let schema = self.schema()?;
        self.client()?
            .execute(
                &format!("DELETE FROM {schema}.ui_sessions WHERE token_sha256 = $1"),
                &[&token_sha256],
            )
            .map_err(text)?;
        Ok(())
    }

    fn record_event(&self, kind: &str, detail: &str) -> Result<(), String> {
        let mut client = self.client()?;
        events::record_event(&self.cli, &mut client, Source::Web, kind, detail).map_err(text)
    }

    fn view(&self) -> Result<ClusterView, String> {
        let mut client = self.client()?;
        cluster_view::load(&self.cli, &mut client).map_err(text)
    }

    fn act(&self, action: WebAction, actor: &Actor) -> Result<String, String> {
        let mut client = self.client()?;
        let cli = &self.cli;
        match action {
            WebAction::Approve {
                version,
                accept_warnings,
            } => ops::approve(cli, &mut client, actor, &version, accept_warnings),
            WebAction::Decline { rollout_id, reason } => {
                ops::decline(cli, &mut client, actor, Some(&rollout_id), &reason)
            }
            WebAction::Cancel { reason } => ops::cancel(cli, &mut client, actor, &reason),
            WebAction::Finalize { confirm_version } => {
                ops::finalize(cli, &mut client, actor, &confirm_version, false)
            }
            WebAction::Restart => ops::restart_cluster(cli, &mut client, actor),
        }
        .map_err(text)
    }
}
