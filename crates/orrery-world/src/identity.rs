//! Identity beyond the actor row: an actor's email addresses, and the OAuth
//! web flow — apps, the authorization codes actors grant them, and the tokens
//! those codes are traded for.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::credentials::{self, TokenOrigin};
use crate::{
    Actor, ActorEmail, ActorKind, ListedActor, ListedOAuthApp, OAuthApp, OAuthToken, Result, World,
    WorldError, constraint_violation, queries,
};

/// How long an authorization code stays exchangeable: GitHub's ten minutes,
/// measured on the virtual clock, so a code outlives a test that never
/// advances time.
pub const OAUTH_CODE_TTL_SECONDS: i64 = 600;

const EMAIL_SELECT: &str = "SELECT email, is_primary, verified FROM actor_emails";
const EMAIL_ORDER: &str = "ORDER BY is_primary DESC, email";
const OAUTH_APP_SELECT: &str =
    "SELECT id, name, client_id, client_secret, callback_url, created_clock_unix FROM oauth_apps";
const LISTED_OAUTH_APP_SELECT: &str = "SELECT name, client_id, callback_url FROM oauth_apps";

impl World {
    /// An actor's addresses, primary first.
    pub fn actor_emails(&self, actor_id: i64) -> Result<Vec<ActorEmail>> {
        emails_of(&self.lock(), actor_id)
    }

    /// The actor holding `login`, as [`World::actors_after`] lists it. The
    /// login compares under the column's `NOCASE` collation, so `"ALICE"`
    /// reads `alice`, and the answer carries the stored login. The system
    /// actor is not listed, so it is not found here either.
    pub fn listed_actor_by_login(&self, login: &str) -> Result<Option<ListedActor>> {
        let conn = self.lock();
        let actor = conn
            .query_row(
                &format!(
                    "{} WHERE a.kind != 'system' AND a.login = ?1",
                    queries::ACTOR_SELECT
                ),
                params![login],
                queries::row_to_actor,
            )
            .optional()?;
        actor.map(|a| listed_actor(&conn, a)).transpose()
    }

    /// Actors whose login sorts after `after_login`, in login order, at most
    /// `limit`, each with its addresses in [`World::actor_emails`] order.
    /// Logins compare under the column's `NOCASE` collation, under which
    /// they are unique, so the order is total and `"BOB"` is the same
    /// position as `"bob"`. The system actor is left out, since no token can
    /// be issued for it.
    pub fn actors_after(
        &self,
        after_login: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ListedActor>> {
        let conn = self.lock();
        let mut actors = conn.prepare(&format!(
            "{} WHERE a.kind != 'system' AND a.login > ?1 COLLATE NOCASE
             ORDER BY a.login COLLATE NOCASE LIMIT ?2",
            queries::ACTOR_SELECT
        ))?;
        // Every login is non-empty, so "" sorts before all of them.
        let rows = actors.query_map(
            params![after_login.unwrap_or(""), limit as i64],
            queries::row_to_actor,
        )?;
        let mut listed = Vec::new();
        for row in rows {
            listed.push(listed_actor(&conn, row?)?);
        }
        Ok(listed)
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
        self.log_locked(
            &tx,
            by,
            "actor.emails.set",
            &serde_json::json!({ "login": actor.login, "emails": stored }),
        )?;
        tx.commit()?;
        Ok(stored)
    }

    /// Register an OAuth app. Its client id and secret are derived from its
    /// id (rule 5), so a replayed world issues the same ones; the secret is
    /// keyed by the world secret, the public client id is not.
    pub fn create_oauth_app(&self, name: &str, callback_url: &str, by: i64) -> Result<OAuthApp> {
        validate_oauth_app_name(name)?;
        validate_oauth_app_callback_url(callback_url)?;
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let clock = queries::clock(&tx)?;
        let secret = credentials::world_secret(&tx)?;
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
                credentials::oauth_client_id(id),
                credentials::oauth_client_secret(&secret, id, 0),
                callback_url,
                clock
            ],
            row_to_oauth_app,
        )?;
        self.log_locked(
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

    /// Issue another client secret for a registered app. A stored secret is
    /// never read back, so this is how anyone gets one for an app they did
    /// not register, as in a world re-keyed at its creation. The secret
    /// issued at registration, and every one issued since, stays valid.
    pub fn issue_oauth_client_secret(&self, client_id: &str, by: i64) -> Result<String> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let app_id: i64 = tx
            .query_row(
                "SELECT id FROM oauth_apps WHERE client_id = ?1",
                params![client_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| WorldError::NoSuchOAuthApp(client_id.to_string()))?;
        let n: i64 = tx.query_row(
            "SELECT COALESCE(MAX(n), 0) + 1 FROM oauth_app_secrets WHERE app_id = ?1",
            params![app_id],
            |r| r.get(0),
        )?;
        let secret: String = tx.query_row(
            "INSERT INTO oauth_app_secrets (app_id, n, secret, created_clock_unix)
             VALUES (?1, ?2, ?3, ?4)
             RETURNING secret",
            params![
                app_id,
                n,
                credentials::oauth_client_secret(&credentials::world_secret(&tx)?, app_id, n),
                queries::clock(&tx)?
            ],
            |r| r.get(0),
        )?;
        self.log_locked(
            &tx,
            by,
            "oauth_app.secret.issue",
            &serde_json::json!({ "client_id": client_id, "n": n }),
        )?;
        tx.commit()?;
        Ok(secret)
    }

    /// The app holding `client_id`, as [`World::oauth_apps_after`] lists
    /// it. The query never reads a secret.
    pub fn listed_oauth_app_by_client_id(&self, client_id: &str) -> Result<Option<ListedOAuthApp>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                &format!("{LISTED_OAUTH_APP_SELECT} WHERE client_id = ?1"),
                params![client_id],
                row_to_listed_oauth_app,
            )
            .optional()?)
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

    /// OAuth apps whose client id sorts after `after_client_id`, in client id
    /// order, at most `limit`. The query never reads a secret.
    pub fn oauth_apps_after(
        &self,
        after_client_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ListedOAuthApp>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "{LISTED_OAUTH_APP_SELECT} WHERE client_id > ?1 ORDER BY client_id LIMIT ?2"
        ))?;
        // Every client id is non-empty, so "" sorts before all of them.
        let rows = stmt.query_map(
            params![after_client_id.unwrap_or(""), limit as i64],
            row_to_listed_oauth_app,
        )?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
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
        let secret = credentials::world_secret(&tx)?;
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
                credentials::oauth_code(&secret, id),
                app.id,
                actor.id,
                redirect_uri,
                scope,
                clock
            ],
            |r| r.get(0),
        )?;
        self.log_locked(
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
        // The secret issued at registration, or any issued since.
        let app = tx
            .query_row(
                &format!(
                    "{OAUTH_APP_SELECT} WHERE client_id = ?1
                     AND (client_secret = ?2 OR EXISTS (
                         SELECT 1 FROM oauth_app_secrets s
                         WHERE s.app_id = oauth_apps.id AND s.secret = ?2))"
                ),
                params![client_id, client_secret],
                row_to_oauth_app,
            )
            .optional()?
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
        let token = credentials::oauth_token(&credentials::world_secret(&tx)?, code_id);
        tx.execute(
            "INSERT INTO tokens (token, actor_id, origin, created_clock_unix)
             VALUES (?1, ?2, ?3, ?4)",
            params![token, actor_id, TokenOrigin::OAuthCode(code_id), clock],
        )?;
        let actor = tx.query_row(
            &format!("{} WHERE id = ?1", queries::ACTOR_SELECT),
            params![actor_id],
            queries::row_to_actor,
        )?;
        self.log_locked(
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

fn emails_of(conn: &Connection, actor_id: i64) -> Result<Vec<ActorEmail>> {
    let mut stmt =
        conn.prepare_cached(&format!("{EMAIL_SELECT} WHERE actor_id = ?1 {EMAIL_ORDER}"))?;
    let rows = stmt.query_map(params![actor_id], row_to_email)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// The one projection the list of actors and the point read share, so the
/// two answers cannot differ.
fn listed_actor(conn: &Connection, actor: Actor) -> Result<ListedActor> {
    Ok(ListedActor {
        emails: emails_of(conn, actor.id)?,
        login: actor.login,
        kind: actor.kind,
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

fn row_to_listed_oauth_app(r: &Row<'_>) -> rusqlite::Result<ListedOAuthApp> {
    Ok(ListedOAuthApp {
        name: r.get(0)?,
        client_id: r.get(1)?,
        callback_url: r.get(2)?,
    })
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

/// The name an OAuth app is registered under. Each of an app's values has a
/// check of its own, so a caller holding only some of them can check those.
pub fn validate_oauth_app_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.chars().count() > 100 {
        return Err(WorldError::InvalidOAuthApp {
            field: "name",
            message: "must be 1-100 characters and not blank".to_string(),
        });
    }
    Ok(())
}

/// The URL an OAuth app's authorizations redirect to.
pub fn validate_oauth_app_callback_url(callback_url: &str) -> Result<()> {
    if !is_absolute_http_url(callback_url) {
        return Err(WorldError::InvalidOAuthApp {
            field: "callback_url",
            message: "must be an absolute http or https URL".to_string(),
        });
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
