//! Apps: OAuth apps with a bot, which accounts install. An installation
//! covers repositories its account owns, and the app issues tokens for it
//! that act as the bot and reach only those repositories.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::credentials;
use crate::events::{self, Attribution, Door, EventBody};
use crate::{
    Actor, ActorKind, App, AppKey, Installation, InstallationGrant, InstallationToken, NewApp,
    Permissions, RegisteredApp, Repo, RepositorySelection, Result, World, WorldError, app_keys,
    identity, objects, queries, uniqueness_violation, validate_login,
};

/// How long an installation token lasts: GitHub's hour, on the world's
/// clock.
pub const INSTALLATION_TOKEN_TTL_SECONDS: i64 = 3600;

/// The most callback URLs an app holds, as on GitHub.
const MAX_CALLBACK_URLS: usize = 10;

/// An app with its bot and its owner, each as [`queries::ACTOR_COLUMNS`],
/// the bot's first.
const APP_SELECT: &str = "SELECT a.id, o.name, o.client_id, o.callback_url, o.created_clock_unix,
            a.permissions, a.events,
            b.id, b.login, b.kind, b.created_clock_unix,
            w.id, w.login, w.kind, w.created_clock_unix
     FROM apps a
     JOIN oauth_apps o ON o.id = a.id
     JOIN actors b ON b.id = a.bot_id
     JOIN actors w ON w.id = a.owner_id";

/// An installation with its app's slug and client id, then its account as
/// [`queries::ACTOR_COLUMNS`].
const INSTALLATION_SELECT: &str = "SELECT i.id, i.app_id, b.login, o.client_id,
            i.repository_selection, i.permissions, i.events, i.created_clock_unix,
            c.id, c.login, c.kind, c.created_clock_unix
     FROM app_installations i
     JOIN apps a ON a.id = i.app_id
     JOIN oauth_apps o ON o.id = a.id
     JOIN actors b ON b.id = a.bot_id
     JOIN actors c ON c.id = i.account_id";

impl World {
    /// Register an app. Its OAuth app is made with it, and so is its bot,
    /// unless a bot that has no app already holds the slug, which then
    /// becomes the app's. The client id and the first client secret are
    /// derived as an OAuth app's are.
    pub fn create_app(&self, new: &NewApp<'_>, by: Attribution) -> Result<RegisteredApp> {
        validate_app(new)?;
        self.write(by, |w| {
            let tx = w.tx;
            let owner = account(tx, new.owner)?.ok_or_else(|| WorldError::InvalidApp {
                field: "owner",
                message: format!("no user or organization {:?}", new.owner),
            })?;
            let (bot, bot_created) = match actor_by_login(tx, new.slug)? {
                None => (crate::insert_actor(tx, new.slug, ActorKind::Bot)?.0, true),
                Some(held) if held.kind != ActorKind::Bot => {
                    return Err(WorldError::AppSlugTaken(new.slug.to_string()));
                }
                Some(bot) => {
                    let taken: bool = tx.query_row(
                        "SELECT EXISTS (SELECT 1 FROM apps WHERE bot_id = ?1)",
                        params![bot.id],
                        |r| r.get(0),
                    )?;
                    if taken {
                        return Err(WorldError::AppExists(new.slug.to_string()));
                    }
                    (bot, false)
                }
            };
            let oauth = identity::insert_oauth_app(tx, new.name, &new.callback_urls[0])?;
            tx.execute(
                "INSERT INTO apps (id, bot_id, owner_id, permissions, events)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    oauth.id,
                    bot.id,
                    owner.id,
                    json_text(new.permissions),
                    json_text(new.events)
                ],
            )?;
            for (n, url) in new.callback_urls.iter().enumerate().skip(1) {
                tx.execute(
                    "INSERT INTO app_callback_urls (app_id, n, url) VALUES (?1, ?2, ?3)",
                    params![oauth.id, n as i64, url],
                )?;
            }
            w.operation(EventBody::AppCreated(events::AppCreated {
                subject: objects::app_subject(tx, oauth.id)?,
                object: objects::app_object(tx, oauth.id)?,
            }));
            if bot_created {
                w.emit(EventBody::ActorCreated(events::ActorCreated {
                    subject: objects::actor_subject(tx, bot.id)?,
                    object: objects::actor_object(tx, bot.id)?,
                }));
            }
            w.emit(EventBody::OAuthAppCreated(events::OAuthAppCreated {
                subject: objects::oauth_app_subject(tx, oauth.id)?,
                object: objects::oauth_app_object(tx, oauth.id)?,
            }));
            Ok(RegisteredApp {
                app: require_app(tx, "a.id = ?1", &oauth.id)?,
                client_secret: oauth.client_secret,
            })
        })
    }

    /// Add a public key an app's signatures are checked against, from its
    /// PEM ([`app_keys::rsa_public_key_from_pem`]).
    pub fn add_app_key(&self, slug: &str, pem: &str, by: Attribution) -> Result<AppKey> {
        let der = app_keys::rsa_public_key_from_pem(pem)?;
        let public_key = base64_standard(&der);
        self.write(by, |w| {
            let tx = w.tx;
            let app = app_where(tx, "b.login = ?1", &slug)?
                .ok_or_else(|| WorldError::NoSuchApp(slug.to_string()))?;
            let key = tx
                .query_row(
                    "INSERT INTO app_keys (app_id, public_key, created_clock_unix)
                     VALUES (?1, ?2, ?3)
                     RETURNING id, public_key, created_clock_unix",
                    params![app.id, public_key, queries::clock(tx)?],
                    row_to_app_key,
                )
                .map_err(|e| match uniqueness_violation(&e) {
                    true => WorldError::AppKeyExists,
                    false => WorldError::from(e),
                })?;
            w.operation(EventBody::AppKeyCreated(events::AppKeyCreated {
                subject: objects::app_key_subject(tx, key.id)?,
                object: objects::app_key_object(tx, key.id)?,
            }));
            Ok(key)
        })
    }

    /// Install an app on a user or an organization: on every repository the
    /// account owns, or, with `repositories`, on the ones it names. The
    /// installation takes the app's permissions and events as they are now.
    pub fn create_installation(
        &self,
        slug: &str,
        account_login: &str,
        repositories: Option<&[String]>,
        by: Attribution,
    ) -> Result<Installation> {
        self.write(by, |w| {
            let tx = w.tx;
            let app = app_where(tx, "b.login = ?1", &slug)?
                .ok_or_else(|| WorldError::NoSuchApp(slug.to_string()))?;
            let account =
                account(tx, account_login)?.ok_or_else(|| WorldError::InvalidInstallation {
                    field: "account",
                    message: format!("no user or organization {account_login:?}"),
                })?;
            let repos = match repositories {
                None => None,
                Some([]) => {
                    return Err(WorldError::InvalidInstallation {
                        field: "repositories",
                        message: "names no repository; leave it out to cover every \
                                  repository the account owns"
                            .to_string(),
                    });
                }
                Some(names) => Some(owned_repos(tx, &account, names)?),
            };
            let selection = match repos {
                Some(_) => RepositorySelection::Selected,
                None => RepositorySelection::All,
            };
            let id: i64 = tx
                .query_row(
                    "INSERT INTO app_installations (app_id, account_id, repository_selection,
                                                    permissions, events, created_clock_unix)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     RETURNING id",
                    params![
                        app.id,
                        account.id,
                        selection.as_str(),
                        json_text(&app.permissions),
                        json_text(&app.events),
                        queries::clock(tx)?
                    ],
                    |r| r.get(0),
                )
                .map_err(|e| match uniqueness_violation(&e) {
                    true => WorldError::InstallationExists {
                        app: app.slug.clone(),
                        account: account.login.clone(),
                    },
                    false => WorldError::from(e),
                })?;
            for repo_id in repos.iter().flatten() {
                tx.execute(
                    "INSERT INTO app_installation_repos (installation_id, repo_id)
                     VALUES (?1, ?2)",
                    params![id, repo_id],
                )?;
            }
            w.operation(EventBody::InstallationCreated(
                events::InstallationCreated {
                    subject: objects::installation_subject(tx, id)?,
                    object: objects::installation_object(tx, id)?,
                },
            ));
            require_installation(tx, id)
        })
    }

    /// Issue a token for one of an app's installations. It acts as the app's
    /// bot, reaches the repositories the installation covers, and expires
    /// [`INSTALLATION_TOKEN_TTL_SECONDS`] later on the world's clock.
    /// `repositories` narrows it to the ones it names, each of which the
    /// installation must cover, and `permissions` to the levels it gives,
    /// none above the installation's. An empty one of either narrows
    /// nothing. The app issues it with its own signature, so the write is
    /// its bot's, through the app.
    pub fn create_installation_token(
        &self,
        app_id: i64,
        installation_id: i64,
        repositories: Option<&[String]>,
        permissions: Option<&Permissions>,
    ) -> Result<InstallationToken> {
        let by = |tx: &Connection| -> Result<(Attribution, Installation)> {
            let installation = installation_where(tx, "i.id = ?1", installation_id)?
                .filter(|i| i.app_id == app_id)
                .ok_or(WorldError::NoSuchInstallation(installation_id))?;
            let bot_id: i64 = tx.query_row(
                "SELECT bot_id FROM apps WHERE id = ?1",
                params![app_id],
                |r| r.get(0),
            )?;
            let by = Attribution::new(bot_id, Door::Rest).with_app(Some(app_id));
            Ok((by, installation))
        };
        self.write_by(by, |w, installation| {
            let tx = w.tx;
            let repos = match repositories.filter(|names| !names.is_empty()) {
                Some(names) => Some(covered_repos(tx, &installation, names)?),
                None => None,
            };
            let permissions = match permissions.filter(|asked| !asked.is_empty()) {
                Some(asked) => {
                    for (name, level) in asked {
                        match installation.permissions.get(name) {
                            None => return Err(WorldError::InstallationPermission(name.clone())),
                            Some(held) if held < level => {
                                return Err(WorldError::InstallationPermissionLevel(name.clone()));
                            }
                            Some(_) => {}
                        }
                    }
                    asked.clone()
                }
                None => installation.permissions.clone(),
            };
            let clock = queries::clock(tx)?;
            let id: i64 = tx.query_row(
                "SELECT COALESCE(MAX(id), 0) + 1 FROM app_installation_tokens",
                [],
                |r| r.get(0),
            )?;
            let (token, expires_clock_unix): (String, i64) = tx.query_row(
                "INSERT INTO app_installation_tokens (id, token, installation_id, permissions,
                                                      created_clock_unix, expires_clock_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 RETURNING token, expires_clock_unix",
                params![
                    id,
                    credentials::installation_token(&credentials::world_secret(tx)?, id),
                    installation.id,
                    json_text(&permissions),
                    clock,
                    clock.saturating_add(INSTALLATION_TOKEN_TTL_SECONDS)
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            for repo in repos.iter().flatten() {
                tx.execute(
                    "INSERT INTO app_installation_token_repos (token_id, repo_id)
                     VALUES (?1, ?2)",
                    params![id, repo.id],
                )?;
            }
            w.operation(EventBody::InstallationTokenCreated(
                events::InstallationTokenCreated {
                    subject: objects::installation_token_subject(tx, id)?,
                    object: objects::installation_token_object(tx, id)?,
                },
            ));
            Ok(InstallationToken {
                token,
                installation,
                permissions,
                repositories: repos,
                expires_clock_unix,
            })
        })
    }

    pub fn app_by_slug(&self, slug: &str) -> Result<Option<App>> {
        app_where(&self.lock(), "b.login = ?1", &slug)
    }

    pub fn app_by_id(&self, id: i64) -> Result<Option<App>> {
        app_where(&self.lock(), "a.id = ?1", &id)
    }

    pub fn app_by_client_id(&self, client_id: &str) -> Result<Option<App>> {
        app_where(&self.lock(), "o.client_id = ?1", &client_id)
    }

    /// The URLs an OAuth app's authorizations may redirect to, the default
    /// first: its callback URL, then, for an app, the ones after its first.
    pub fn oauth_callback_urls(&self, oauth_app_id: i64) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut urls: Vec<String> = objects::list_by(
            &conn,
            "SELECT callback_url FROM oauth_apps WHERE id = ?1",
            oauth_app_id,
            |r| r.get(0),
        )?;
        urls.extend(objects::list_by(
            &conn,
            "SELECT url FROM app_callback_urls WHERE app_id = ?1 ORDER BY n",
            oauth_app_id,
            |r| r.get::<_, String>(0),
        )?);
        Ok(urls)
    }

    /// Whether `client_secret` is one of the OAuth app's: the one issued
    /// when it was registered, or any issued since.
    pub fn oauth_app_holds_secret(&self, oauth_app_id: i64, client_secret: &str) -> Result<bool> {
        Ok(self.lock().query_row(
            "SELECT EXISTS (SELECT 1 FROM oauth_apps WHERE id = ?1 AND client_secret = ?2)
                 OR EXISTS (SELECT 1 FROM oauth_app_secrets WHERE app_id = ?1 AND secret = ?2)",
            params![oauth_app_id, client_secret],
            |r| r.get(0),
        )?)
    }

    /// One page of the apps in slug order, and how many there are. With
    /// `resume`, the slug of the previous page's last app, the page holds
    /// only apps past it, so a walk neither repeats nor skips an app while
    /// the list grows. Slugs compare as logins do, without case. The count
    /// is read under the same lock as the page.
    pub fn apps_page(&self, resume: Option<&str>, limit: usize) -> Result<(Vec<App>, i64)> {
        let conn = self.lock();
        let apps = apps_from(
            &conn,
            &format!(
                "{APP_SELECT} WHERE b.login > ?1 COLLATE NOCASE
                 ORDER BY b.login COLLATE NOCASE LIMIT ?2"
            ),
            // Every login is non-empty, so "" sorts before all of them.
            params![resume.unwrap_or(""), limit as i64],
        )?;
        let total = conn.query_row("SELECT COUNT(*) FROM apps", [], |r| r.get(0))?;
        Ok((apps, total))
    }

    /// An app's keys, oldest first.
    pub fn app_keys(&self, app_id: i64) -> Result<Vec<AppKey>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, public_key, created_clock_unix FROM app_keys
             WHERE app_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![app_id], row_to_app_key)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn installation_by_id(&self, id: i64) -> Result<Option<Installation>> {
        installation_where(&self.lock(), "i.id = ?1", id)
    }

    /// One page of an app's installations in id order, and how many it has.
    /// With `resume`, the id of the previous page's last installation, the
    /// page holds only installations past it. The count is read under the
    /// same lock as the page.
    pub fn installations_page(
        &self,
        app_id: i64,
        resume: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<Installation>, i64)> {
        let conn = self.lock();
        let installations = installations_from(
            &conn,
            &format!(
                "{INSTALLATION_SELECT} WHERE i.app_id = ?1 AND i.id > ?2 ORDER BY i.id LIMIT ?3"
            ),
            params![app_id, resume.unwrap_or(0), limit as i64],
        )?;
        Ok((installations, count_installations(&conn, app_id)?))
    }

    /// The `limit` installations of an app after its first `offset`, newest
    /// first, and how many it has.
    pub fn installations_slice(
        &self,
        app_id: i64,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<Installation>, i64)> {
        let conn = self.lock();
        let installations = installations_from(
            &conn,
            &format!(
                "{INSTALLATION_SELECT} WHERE i.app_id = ?1 ORDER BY i.id DESC LIMIT ?3 OFFSET ?2"
            ),
            // An offset past what SQLite counts is past every row.
            params![
                app_id,
                i64::try_from(offset).unwrap_or(i64::MAX),
                limit as i64
            ],
        )?;
        Ok((installations, count_installations(&conn, app_id)?))
    }

    /// How many installations an app has.
    pub fn installation_count(&self, app_id: i64) -> Result<i64> {
        count_installations(&self.lock(), app_id)
    }

    /// The repositories an installation token reaches, in id order.
    pub fn granted_repos(&self, grant: &InstallationGrant) -> Result<Vec<Repo>> {
        let conn = self.lock();
        match &grant.repos {
            None => Ok(conn
                .prepare(&format!(
                    "SELECT {} FROM repos
                     WHERE owner_id = (SELECT id FROM actors WHERE login = ?1) ORDER BY id",
                    queries::REPO_COLUMNS
                ))?
                .query_map(params![grant.account], queries::row_to_repo)?
                .collect::<rusqlite::Result<_>>()?),
            Some(ids) => {
                let mut repos = Vec::with_capacity(ids.len());
                for id in ids {
                    repos.extend(
                        conn.query_row(
                            &format!("SELECT {} FROM repos WHERE id = ?1", queries::REPO_COLUMNS),
                            params![id],
                            queries::row_to_repo,
                        )
                        .optional()?,
                    );
                }
                Ok(repos)
            }
        }
    }
}

/// The caller an installation token authenticates, at the time `now`: its
/// app's bot, through the app, reaching what the token covers. An expired
/// token authenticates nobody.
pub(crate) fn caller_by_installation_token(
    conn: &Connection,
    token: &str,
    now: i64,
) -> Result<Option<crate::Caller>> {
    let found = conn
        .query_row(
            "SELECT b.id, b.login, b.kind, b.created_clock_unix,
                    t.id, t.expires_clock_unix, i.id, i.app_id, i.repository_selection, c.login
             FROM app_installation_tokens t
             JOIN app_installations i ON i.id = t.installation_id
             JOIN apps a ON a.id = i.app_id
             JOIN actors b ON b.id = a.bot_id
             JOIN actors c ON c.id = i.account_id
             WHERE t.token = ?1",
            params![token],
            |r| {
                Ok((
                    queries::row_to_actor(r)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    RepositorySelection::from_db(&r.get::<_, String>(8)?),
                    r.get::<_, String>(9)?,
                ))
            },
        )
        .optional()?;
    let Some((actor, token_id, expires, installation_id, app_id, selection, account)) = found
    else {
        return Ok(None);
    };
    if now >= expires {
        return Ok(None);
    }
    let narrowed = repo_ids(
        conn,
        "SELECT repo_id FROM app_installation_token_repos WHERE token_id = ?1 ORDER BY repo_id",
        token_id,
    )?;
    let repos = match (narrowed.is_empty(), selection) {
        (false, _) => Some(narrowed),
        (true, RepositorySelection::Selected) => Some(repo_ids(
            conn,
            "SELECT repo_id FROM app_installation_repos
             WHERE installation_id = ?1 ORDER BY repo_id",
            installation_id,
        )?),
        (true, RepositorySelection::All) => None,
    };
    Ok(Some(crate::Caller {
        actor,
        app_id: Some(app_id),
        installation: Some(InstallationGrant {
            installation_id,
            account,
            repos,
        }),
    }))
}

/// Every app, in id order.
pub(crate) fn list_apps(conn: &Connection) -> Result<Vec<App>> {
    apps_from(conn, &format!("{APP_SELECT} ORDER BY a.id"), [])
}

/// Every installation, in id order.
pub(crate) fn list_installations(conn: &Connection) -> Result<Vec<Installation>> {
    installations_from(conn, &format!("{INSTALLATION_SELECT} ORDER BY i.id"), [])
}

fn repo_ids(conn: &Connection, sql: &str, id: i64) -> Result<Vec<i64>> {
    objects::list_by(conn, sql, id, |r| r.get(0))
}

fn count_installations(conn: &Connection, app_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM app_installations WHERE app_id = ?1",
        params![app_id],
        |r| r.get(0),
    )?)
}

fn actor_by_login(conn: &Connection, login: &str) -> Result<Option<Actor>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {} FROM actors WHERE login = ?1",
                queries::ACTOR_COLUMNS
            ),
            params![login],
            queries::row_to_actor,
        )
        .optional()?)
}

/// The user or organization holding `login`: what owns an app, and what an
/// app is installed on.
fn account(conn: &Connection, login: &str) -> Result<Option<Actor>> {
    Ok(actor_by_login(conn, login)?.filter(|a| matches!(a.kind, ActorKind::User | ActorKind::Org)))
}

/// The ids of the repositories `account` owns under `names`, in the order
/// named.
fn owned_repos(conn: &Connection, account: &Actor, names: &[String]) -> Result<Vec<i64>> {
    let mut ids = Vec::with_capacity(names.len());
    for name in names {
        let id: i64 = conn
            .query_row(
                "SELECT id FROM repos WHERE owner_id = ?1 AND name = ?2",
                params![account.id, name],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| WorldError::InvalidInstallation {
                field: "repositories",
                message: format!("{} owns no repository {name:?}", account.login),
            })?;
        if ids.contains(&id) {
            return Err(WorldError::InvalidInstallation {
                field: "repositories",
                message: format!("{name:?} is listed twice"),
            });
        }
        ids.push(id);
    }
    Ok(ids)
}

/// The repositories of the installation's account under `names`, each of
/// which the installation must cover, in id order.
fn covered_repos(
    conn: &Connection,
    installation: &Installation,
    names: &[String],
) -> Result<Vec<Repo>> {
    let mut repos: Vec<Repo> = Vec::with_capacity(names.len());
    for name in names {
        let repo = conn
            .query_row(
                &format!(
                    "SELECT {} FROM repos WHERE owner_id = ?1 AND name = ?2",
                    queries::REPO_COLUMNS
                ),
                params![installation.account.id, name],
                queries::row_to_repo,
            )
            .optional()?
            .filter(|repo| {
                installation.repository_selection == RepositorySelection::All
                    || installation
                        .repositories
                        .iter()
                        .any(|listed| listed.eq_ignore_ascii_case(&repo.name))
            })
            .ok_or_else(|| WorldError::InstallationRepository(name.clone()))?;
        if !repos.iter().any(|held| held.id == repo.id) {
            repos.push(repo);
        }
    }
    repos.sort_by_key(|repo| repo.id);
    Ok(repos)
}

fn app_where(
    conn: &Connection,
    condition: &str,
    param: &dyn rusqlite::ToSql,
) -> Result<Option<App>> {
    Ok(apps_from(conn, &format!("{APP_SELECT} WHERE {condition}"), [param])?.pop())
}

fn require_app(conn: &Connection, condition: &str, param: &dyn rusqlite::ToSql) -> Result<App> {
    app_where(conn, condition, param)?
        .ok_or(WorldError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
}

fn apps_from(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<App>> {
    let mut apps = conn
        .prepare(sql)?
        .query_map(params, |r| {
            Ok(App {
                id: r.get(0)?,
                name: r.get(1)?,
                client_id: r.get(2)?,
                callback_urls: vec![r.get(3)?],
                created_clock_unix: r.get(4)?,
                permissions: json_column(r, 5)?,
                events: json_column(r, 6)?,
                bot: actor_at(r, 7)?,
                owner: actor_at(r, 11)?,
                slug: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<App>>>()?;
    for app in &mut apps {
        app.callback_urls.extend(objects::list_by(
            conn,
            "SELECT url FROM app_callback_urls WHERE app_id = ?1 ORDER BY n",
            app.id,
            |r| r.get::<_, String>(0),
        )?);
    }
    Ok(apps)
}

fn installation_where(conn: &Connection, condition: &str, id: i64) -> Result<Option<Installation>> {
    Ok(installations_from(
        conn,
        &format!("{INSTALLATION_SELECT} WHERE {condition}"),
        params![id],
    )?
    .pop())
}

fn require_installation(conn: &Connection, id: i64) -> Result<Installation> {
    installation_where(conn, "i.id = ?1", id)?.ok_or(WorldError::NoSuchInstallation(id))
}

fn installations_from(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Installation>> {
    let mut installations = conn
        .prepare(sql)?
        .query_map(params, |r| {
            Ok(Installation {
                id: r.get(0)?,
                app_id: r.get(1)?,
                app_slug: r.get(2)?,
                client_id: r.get(3)?,
                repository_selection: RepositorySelection::from_db(&r.get::<_, String>(4)?),
                repositories: Vec::new(),
                permissions: json_column(r, 5)?,
                events: json_column(r, 6)?,
                created_clock_unix: r.get(7)?,
                account: actor_at(r, 8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<Installation>>>()?;
    for installation in &mut installations {
        installation.repositories = objects::list_by(
            conn,
            "SELECT r.name FROM app_installation_repos s JOIN repos r ON r.id = s.repo_id
             WHERE s.installation_id = ?1 ORDER BY r.id",
            installation.id,
            |r| r.get(0),
        )?;
    }
    Ok(installations)
}

/// An actor from four columns in [`queries::ACTOR_COLUMNS`]'s order,
/// starting at `at`.
fn actor_at(r: &Row<'_>, at: usize) -> rusqlite::Result<Actor> {
    Ok(Actor {
        id: r.get(at)?,
        login: r.get(at + 1)?,
        kind: ActorKind::from_db(&r.get::<_, String>(at + 2)?),
        created_clock_unix: r.get(at + 3)?,
    })
}

fn row_to_app_key(r: &Row<'_>) -> rusqlite::Result<AppKey> {
    Ok(AppKey {
        id: r.get(0)?,
        public_key: r.get(1)?,
        created_clock_unix: r.get(2)?,
    })
}

/// A value as the JSON text its column holds.
fn json_text<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).expect("permissions and events serialize")
}

/// A column holding JSON text, decoded.
pub(crate) fn json_column<T: serde::de::DeserializeOwned>(
    r: &Row<'_>,
    at: usize,
) -> rusqlite::Result<T> {
    serde_json::from_str(&r.get::<_, String>(at)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(at, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn base64_standard(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn validate_app(new: &NewApp<'_>) -> Result<()> {
    validate_app_slug(new.slug)?;
    validate_app_name(new.name)?;
    validate_app_callback_urls(new.callback_urls)?;
    validate_app_permissions(new.permissions)?;
    validate_app_events(new.events)
}

fn invalid_app(field: &'static str, message: String) -> WorldError {
    WorldError::InvalidApp { field, message }
}

/// An app's slug, which is its bot's login. Each of an app's values has a
/// check of its own, so a caller holding only some of them can check those.
pub fn validate_app_slug(slug: &str) -> Result<()> {
    validate_login(slug).map_err(|e| invalid_app("slug", e.to_string()))
}

/// The name an app is registered under, held to an OAuth app's rule.
pub fn validate_app_name(name: &str) -> Result<()> {
    identity::validate_oauth_app_name(name).map_err(|e| match e {
        WorldError::InvalidOAuthApp { message, .. } => invalid_app("name", message),
        other => other,
    })
}

/// The URLs an app's authorizations redirect to: at least one, each held to
/// an OAuth app's rule, and none twice.
pub fn validate_app_callback_urls(callback_urls: &[String]) -> Result<()> {
    if callback_urls.is_empty() || callback_urls.len() > MAX_CALLBACK_URLS {
        return Err(invalid_app(
            "callback_urls",
            format!("must hold 1 to {MAX_CALLBACK_URLS} URLs"),
        ));
    }
    for (n, url) in callback_urls.iter().enumerate() {
        identity::validate_oauth_app_callback_url(url).map_err(|e| match e {
            WorldError::InvalidOAuthApp { message, .. } => {
                invalid_app("callback_urls", format!("{url:?} {message}"))
            }
            other => other,
        })?;
        if callback_urls[..n].contains(url) {
            return Err(invalid_app(
                "callback_urls",
                format!("{url:?} is listed twice"),
            ));
        }
    }
    Ok(())
}

/// The names of the permissions an app holds.
pub fn validate_app_permissions(permissions: &Permissions) -> Result<()> {
    match permissions.keys().find(|name| !is_name(name)) {
        Some(name) => Err(invalid_app(
            "permissions",
            format!("{name:?} is not a permission name: lowercase letters and underscores"),
        )),
        None => Ok(()),
    }
}

/// The events an app subscribes to: each a name, and none twice.
pub fn validate_app_events(events: &[String]) -> Result<()> {
    for (n, event) in events.iter().enumerate() {
        if !is_name(event) {
            return Err(invalid_app(
                "events",
                format!("{event:?} is not an event name: lowercase letters and underscores"),
            ));
        }
        if events[..n].contains(event) {
            return Err(invalid_app("events", format!("{event:?} is listed twice")));
        }
    }
    Ok(())
}

/// A permission's or an event's name: lowercase letters and underscores, as
/// GitHub spells them.
fn is_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
}
