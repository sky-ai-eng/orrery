//! Identity beyond the actor row: an actor's email addresses, and the OAuth
//! web flow — apps, the authorization codes actors grant them, and the tokens
//! those codes are traded for.

use rusqlite::{Connection, OptionalExtension, Row, params};
use sha2::{Digest, Sha256};

use crate::{
    Actor, ActorEmail, ActorKind, OAuthApp, OAuthToken, Result, World, WorldError,
    constraint_violation, log_locked, queries,
};

/// How long an authorization code stays exchangeable: GitHub's ten minutes,
/// measured on the virtual clock, so a code outlives a test that never
/// advances time.
pub const OAUTH_CODE_TTL_SECONDS: i64 = 600;

const EMAIL_SELECT: &str = "SELECT email, is_primary, verified FROM actor_emails";
const EMAIL_ORDER: &str = "ORDER BY is_primary DESC, email";
const OAUTH_APP_SELECT: &str =
    "SELECT id, name, client_id, client_secret, callback_url, created_clock_unix FROM oauth_apps";

impl World {
    /// An actor's addresses, primary first.
    pub fn actor_emails(&self, actor_id: i64) -> Result<Vec<ActorEmail>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare(&format!("{EMAIL_SELECT} WHERE actor_id = ?1 {EMAIL_ORDER}"))?;
        let rows = stmt.query_map(params![actor_id], row_to_email)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Replace an actor's addresses. This is what reality does when a person
    /// verifies an address or picks a new primary, which no public API can
    /// express. `by` is the actor the action log attributes the change to.
    pub fn set_actor_emails(
        &self,
        login: &str,
        emails: &[ActorEmail],
        by: i64,
    ) -> Result<Vec<ActorEmail>> {
        validate_emails(emails)?;
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let actor = tx
            .query_row(
                &format!("{} WHERE login = ?1", queries::ACTOR_SELECT),
                params![login],
                queries::row_to_actor,
            )
            .optional()?
            .filter(|a| a.kind != ActorKind::System)
            .ok_or_else(|| WorldError::NoSuchActor(login.to_string()))?;
        tx.execute(
            "DELETE FROM actor_emails WHERE actor_id = ?1",
            params![actor.id],
        )?;
        let mut stored = emails
            .iter()
            .map(|e| insert_email(&tx, actor.id, e))
            .collect::<Result<Vec<_>>>()?;
        // The point read's order: primary first, then by address (NOCASE).
        stored.sort_by_key(|e| (!e.primary, e.email.to_ascii_lowercase()));
        log_locked(
            &tx,
            by,
            "actor.emails.set",
            &serde_json::json!({ "login": actor.login, "emails": stored }),
        )?;
        tx.commit()?;
        Ok(stored)
    }

    /// Register an OAuth app. Its client id and secret are derived from its
    /// id (rule 5), so a replayed world issues the same ones.
    pub fn create_oauth_app(&self, name: &str, callback_url: &str, by: i64) -> Result<OAuthApp> {
        if name.trim().is_empty() || name.len() > 100 {
            return Err(WorldError::InvalidOAuthApp {
                field: "name",
                message: "must be 1-100 characters and not blank".to_string(),
            });
        }
        if !is_absolute_http_url(callback_url) {
            return Err(WorldError::InvalidOAuthApp {
                field: "callback_url",
                message: "must be an absolute http or https URL".to_string(),
            });
        }
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let clock = queries::clock(&tx)?;
        let id: i64 = tx.query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM oauth_apps", [], |r| {
            r.get(0)
        })?;
        let app = tx.query_row(
            "INSERT INTO oauth_apps (id, name, client_id, client_secret, callback_url,
                                     created_clock_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             RETURNING id, name, client_id, client_secret, callback_url, created_clock_unix",
            params![
                id,
                name,
                format!(
                    "orr_client_{}",
                    derive_hex(&format!("oauth-app-client:{id}"), 10)
                ),
                format!(
                    "orr_secret_{}",
                    derive_hex(&format!("oauth-app-secret:{id}"), 20)
                ),
                callback_url,
                clock
            ],
            row_to_oauth_app,
        )?;
        log_locked(
            &tx,
            by,
            "oauth_app.create",
            &serde_json::json!({
                "name": app.name,
                "client_id": app.client_id,
                "callback_url": app.callback_url,
            }),
        )?;
        tx.commit()?;
        Ok(app)
    }

    pub fn oauth_app_by_client_id(&self, client_id: &str) -> Result<Option<OAuthApp>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                &format!("{OAUTH_APP_SELECT} WHERE client_id = ?1"),
                params![client_id],
                row_to_oauth_app,
            )
            .optional()?)
    }

    /// Record `actor` authorizing `app` and return the authorization code.
    /// The redirect URI is the one the code was issued for; an exchange that
    /// names a different one is refused.
    pub fn issue_oauth_code(
        &self,
        app: &OAuthApp,
        actor: &Actor,
        redirect_uri: &str,
        scope: &str,
    ) -> Result<String> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let clock = queries::clock(&tx)?;
        let id: i64 = tx.query_row(
            "SELECT COALESCE(MAX(id), 0) + 1 FROM oauth_codes",
            [],
            |r| r.get(0),
        )?;
        let code: String = tx.query_row(
            "INSERT INTO oauth_codes (id, code, app_id, actor_id, redirect_uri, scope,
                                      created_clock_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             RETURNING code",
            params![
                id,
                derive_hex(&format!("oauth-code:{id}"), 10),
                app.id,
                actor.id,
                redirect_uri,
                scope,
                clock
            ],
            |r| r.get(0),
        )?;
        log_locked(
            &tx,
            actor.id,
            "oauth.authorize",
            &serde_json::json!({ "client_id": app.client_id, "scope": scope }),
        )?;
        tx.commit()?;
        Ok(code)
    }

    /// Trade an authorization code for a token acting as the actor who granted
    /// it. A code is spent by its first successful exchange.
    pub fn exchange_oauth_code(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: Option<&str>,
    ) -> Result<OAuthToken> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let clock = queries::clock(&tx)?;
        let app = tx
            .query_row(
                &format!("{OAUTH_APP_SELECT} WHERE client_id = ?1"),
                params![client_id],
                row_to_oauth_app,
            )
            .optional()?
            .filter(|app| app.client_secret == client_secret)
            .ok_or(WorldError::OAuthClientCredentials)?;
        let (code_id, actor_id, issued_redirect, scope, created, exchanged) = tx
            .query_row(
                "SELECT id, actor_id, redirect_uri, scope, created_clock_unix, exchanged_clock_unix
                 FROM oauth_codes WHERE code = ?1 AND app_id = ?2",
                params![code, app.id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or(WorldError::OAuthBadCode)?;
        if exchanged.is_some() || clock > created + OAUTH_CODE_TTL_SECONDS {
            return Err(WorldError::OAuthBadCode);
        }
        if redirect_uri.is_some_and(|r| r != issued_redirect) {
            return Err(WorldError::OAuthRedirectMismatch);
        }
        tx.execute(
            "UPDATE oauth_codes SET exchanged_clock_unix = ?1 WHERE id = ?2",
            params![clock, code_id],
        )?;
        let token = format!("orr_{}", derive_hex(&format!("oauth-token:{code_id}"), 16));
        tx.execute(
            "INSERT INTO tokens (token, actor_id, created_clock_unix) VALUES (?1, ?2, ?3)",
            params![token, actor_id, clock],
        )?;
        let actor = tx.query_row(
            &format!("{} WHERE id = ?1", queries::ACTOR_SELECT),
            params![actor_id],
            queries::row_to_actor,
        )?;
        log_locked(
            &tx,
            actor.id,
            "oauth.token",
            &serde_json::json!({ "client_id": app.client_id, "scope": scope }),
        )?;
        tx.commit()?;
        Ok(OAuthToken {
            actor,
            token,
            scope,
        })
    }
}

/// The address every new actor starts with: `.invalid`, so it can never
/// route (rule 12: worlds are safe to share).
pub(crate) fn default_email(login: &str) -> ActorEmail {
    ActorEmail {
        email: format!("{login}@orrery.invalid"),
        primary: true,
        verified: true,
    }
}

pub(crate) fn insert_email(conn: &Connection, actor_id: i64, e: &ActorEmail) -> Result<ActorEmail> {
    conn.query_row(
        "INSERT INTO actor_emails (actor_id, email, is_primary, verified)
         VALUES (?1, ?2, ?3, ?4)
         RETURNING email, is_primary, verified",
        params![actor_id, e.email, e.primary, e.verified],
        row_to_email,
    )
    .map_err(|err| match constraint_violation(&err) {
        true => WorldError::InvalidEmails(format!("{:?} is listed twice", e.email)),
        false => WorldError::from(err),
    })
}

fn row_to_email(r: &Row<'_>) -> rusqlite::Result<ActorEmail> {
    Ok(ActorEmail {
        email: r.get(0)?,
        primary: r.get(1)?,
        verified: r.get(2)?,
    })
}

pub(crate) fn list_oauth_apps(conn: &Connection) -> Result<Vec<OAuthApp>> {
    let mut stmt = conn.prepare(&format!("{OAUTH_APP_SELECT} ORDER BY id"))?;
    let rows = stmt.query_map([], row_to_oauth_app)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn row_to_oauth_app(r: &Row<'_>) -> rusqlite::Result<OAuthApp> {
    Ok(OAuthApp {
        id: r.get(0)?,
        name: r.get(1)?,
        client_id: r.get(2)?,
        client_secret: r.get(3)?,
        callback_url: r.get(4)?,
        created_clock_unix: r.get(5)?,
    })
}

fn validate_emails(emails: &[ActorEmail]) -> Result<()> {
    if emails.is_empty() {
        return Err(WorldError::InvalidEmails(
            "an actor holds at least one address".to_string(),
        ));
    }
    let primaries = emails.iter().filter(|e| e.primary).count();
    if primaries != 1 {
        return Err(WorldError::InvalidEmails(format!(
            "exactly one address must be primary, got {primaries}"
        )));
    }
    for e in emails {
        let shaped = e.email.len() <= 254
            && !e.email.chars().any(char::is_whitespace)
            && e.email.split_once('@').is_some_and(|(local, domain)| {
                !local.is_empty() && !domain.is_empty() && !domain.contains('@')
            });
        if !shaped {
            return Err(WorldError::InvalidEmails(format!(
                "{:?} is not an email address",
                e.email
            )));
        }
    }
    Ok(())
}

fn is_absolute_http_url(s: &str) -> bool {
    s.strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .is_some_and(|rest| {
            let host = rest.split(['/', '?', '#']).next().unwrap_or("");
            !host.is_empty() && !host.chars().any(char::is_whitespace)
        })
}

/// Hex of the first `bytes` bytes of `input`'s SHA-256: the derived-not-random
/// identifiers rule 5 requires.
fn derive_hex(input: &str, bytes: usize) -> String {
    hex::encode(&Sha256::digest(input)[..bytes])
}
