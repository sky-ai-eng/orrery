//! The world secret and every credential derived from it.
//!
//! Credentials are derived, not random, so replay reproduces them (rule 5).
//! Each derivation is an HMAC-SHA256 keyed by the world's secret over public
//! inputs — a login and an id — so knowing the inputs is not enough to compute
//! a working credential. The
//! secret lives only in the `world` table: never in an event, and no read
//! returns it.

use std::path::Path;

use hmac::{Hmac, Mac};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use crate::events::{self, Attribution, Door, EventBody};
use crate::{Result, SYSTEM_ACTOR_ID, World, queries};

/// The key behind every credential a world derives.
#[derive(Clone)]
pub struct WorldSecret([u8; 32]);

impl WorldSecret {
    /// The fixed, well-known secret of local worlds, so a local run mints the
    /// same credentials every time. Anyone who reads this line can compute
    /// them; a world reachable by strangers needs a random secret.
    pub const LOCAL: WorldSecret = WorldSecret([0; 32]);

    pub const fn from_bytes(bytes: [u8; 32]) -> WorldSecret {
        WorldSecret(bytes)
    }

    fn derive(&self, input: &str) -> [u8; 32] {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC accepts a key of any length");
        mac.update(input.as_bytes());
        mac.finalize().into_bytes().into()
    }
}

// Redacted, so a secret held by something printed with `{:?}` stays out of
// logs and panic messages.
impl std::fmt::Debug for WorldSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorldSecret(..)")
    }
}

impl ToSql for WorldSecret {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(hex::encode(self.0)))
    }
}

impl FromSql for WorldSecret {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let mut bytes = [0; 32];
        hex::decode_to_slice(value.as_str()?, &mut bytes)
            .map_err(|e| FromSqlError::Other(Box::new(e)))?;
        Ok(WorldSecret(bytes))
    }
}

/// What a stored token was derived from. A token's value cannot say, and
/// re-keying needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenOrigin {
    /// The actor's `n`th token: 0 for the one minted with it, then each one
    /// issued for it since, counting from 1.
    Actor(i64),
    /// Issued for the authorization code with this id.
    OAuthCode(i64),
}

impl ToSql for TokenOrigin {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(match self {
            TokenOrigin::Actor(0) => "actor".to_string(),
            TokenOrigin::Actor(n) => format!("actor:{n}"),
            TokenOrigin::OAuthCode(id) => format!("oauth:{id}"),
        }))
    }
}

impl FromSql for TokenOrigin {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let origin = value.as_str()?;
        let numbered = |prefix: &str| origin.strip_prefix(prefix).and_then(|n| n.parse().ok());
        match origin {
            "actor" => Some(TokenOrigin::Actor(0)),
            _ => numbered("actor:")
                .map(TokenOrigin::Actor)
                .or_else(|| numbered("oauth:").map(TokenOrigin::OAuthCode)),
        }
        .ok_or_else(|| FromSqlError::Other(format!("{origin:?} is not a token origin").into()))
    }
}

/// The actor's `n`th token (see [`TokenOrigin::Actor`]). The `orrfake_` prefix
/// marks a world's credentials as fake wherever they turn up, and keeps them
/// outside every real GitHub token namespace (`ghp_`, `ghs_`, `gho_`, ...).
pub(crate) fn actor_token(secret: &WorldSecret, login: &str, actor_id: i64, n: i64) -> String {
    let input = match n {
        0 => format!("{login}:{actor_id}"),
        n => format!("{login}:{actor_id}:{n}"),
    };
    format!("orrfake_{}", hex::encode(&secret.derive(&input)[..16]))
}

pub(crate) fn oauth_token(secret: &WorldSecret, code_id: i64) -> String {
    format!(
        "orrfake_{}",
        hex::encode(&secret.derive(&format!("oauth-token:{code_id}"))[..16])
    )
}

/// The token with this id issued for an installation.
pub(crate) fn installation_token(secret: &WorldSecret, token_id: i64) -> String {
    format!(
        "orrfake_{}",
        hex::encode(&secret.derive(&format!("installation-token:{token_id}"))[..16])
    )
}

pub(crate) fn oauth_code(secret: &WorldSecret, code_id: i64) -> String {
    hex::encode(&secret.derive(&format!("oauth-code:{code_id}"))[..10])
}

/// The app's `n`th client secret: 0 for the one issued at registration, then
/// each one issued for it since, counting from 1.
pub(crate) fn oauth_client_secret(secret: &WorldSecret, app_id: i64, n: i64) -> String {
    let input = match n {
        0 => format!("oauth-app-secret:{app_id}"),
        n => format!("oauth-app-secret:{app_id}:{n}"),
    };
    format!(
        "orrfake_secret_{}",
        hex::encode(&secret.derive(&input)[..20])
    )
}

/// Every client id starts with this, so a value without it names no app.
pub const OAUTH_CLIENT_ID_PREFIX: &str = "orrfake_client_";

/// Unkeyed: a client id is public by nature, sent in the clear on every
/// authorize link.
pub(crate) fn oauth_client_id(app_id: i64) -> String {
    format!(
        "{OAUTH_CLIENT_ID_PREFIX}{}",
        hex::encode(&Sha256::digest(format!("oauth-app-client:{app_id}"))[..10])
    )
}

pub(crate) fn world_secret(conn: &Connection) -> Result<WorldSecret> {
    Ok(conn.query_row("SELECT secret FROM world WHERE id = 1", [], |r| r.get(0))?)
}

impl World {
    /// Open the world at `root` — a fresh copy of another world's state — as
    /// a new world: give it `secret` and re-derive every credential it
    /// inherited, so it accepts none of its source's. An app's keys stay:
    /// each is the public half of a pair its caller holds, which no secret
    /// of a world's derives. This is the only way a
    /// world is re-keyed, and only at its creation, so every event after its
    /// `world:rekeyed` was made under the secret it holds now. The `orrery`
    /// command's fork is the one caller, so the write is the system actor's,
    /// through the CLI.
    pub fn open_rekeyed(root: &Path, secret: &WorldSecret) -> Result<World> {
        let world = World::open(root)?;
        world.rekey(secret)?;
        Ok(world)
    }

    /// One transaction, so the world never holds credentials derived under
    /// two secrets. Spent authorization codes keep their values: they can
    /// never be exchanged again.
    fn rekey(&self, secret: &WorldSecret) -> Result<()> {
        let by = Attribution::new(SYSTEM_ACTOR_ID, Door::Cli);
        self.write(by, |w| {
            let tx = w.tx;
            tx.execute("UPDATE world SET secret = ?1 WHERE id = 1", params![secret])?;

            let tokens = queries::list(
                tx,
                "SELECT t.token, t.origin, a.login, a.id
             FROM tokens t JOIN actors a ON a.id = t.actor_id
             ORDER BY t.rowid",
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, TokenOrigin>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )?;
            for (old, origin, login, actor_id) in &tokens {
                let new = match origin {
                    TokenOrigin::Actor(n) => actor_token(secret, login, *actor_id, *n),
                    TokenOrigin::OAuthCode(code_id) => oauth_token(secret, *code_id),
                };
                tx.execute(
                    "UPDATE tokens SET token = ?1 WHERE token = ?2",
                    params![new, old],
                )?;
            }

            let apps: Vec<i64> =
                queries::list(tx, "SELECT id FROM oauth_apps ORDER BY id", |r| r.get(0))?;
            for id in &apps {
                tx.execute(
                    "UPDATE oauth_apps SET client_secret = ?1 WHERE id = ?2",
                    params![oauth_client_secret(secret, *id, 0), id],
                )?;
            }

            let app_secrets = queries::list(
                tx,
                "SELECT app_id, n FROM oauth_app_secrets ORDER BY app_id, n",
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )?;
            for (app_id, n) in &app_secrets {
                tx.execute(
                    "UPDATE oauth_app_secrets SET secret = ?1 WHERE app_id = ?2 AND n = ?3",
                    params![oauth_client_secret(secret, *app_id, *n), app_id, n],
                )?;
            }

            let codes: Vec<i64> = queries::list(
                tx,
                "SELECT id FROM oauth_codes WHERE exchanged_clock_unix IS NULL ORDER BY id",
                |r| r.get(0),
            )?;
            for id in &codes {
                tx.execute(
                    "UPDATE oauth_codes SET code = ?1 WHERE id = ?2",
                    params![oauth_code(secret, *id), id],
                )?;
            }

            let installation_tokens: Vec<i64> = queries::list(
                tx,
                "SELECT id FROM app_installation_tokens ORDER BY id",
                |r| r.get(0),
            )?;
            for id in &installation_tokens {
                tx.execute(
                    "UPDATE app_installation_tokens SET token = ?1 WHERE id = ?2",
                    params![installation_token(secret, *id), id],
                )?;
            }

            w.operation(EventBody::WorldRekeyed(events::WorldRekeyed {
                subject: events::WorldSubject::default(),
                rekeyed: events::Rekeyed {
                    tokens: tokens.len() as i64,
                    oauth_apps: apps.len() as i64,
                    oauth_app_secrets: app_secrets.len() as i64,
                    oauth_codes: codes.len() as i64,
                    installation_tokens: installation_tokens.len() as i64,
                },
            }));
            Ok(())
        })
    }
}
