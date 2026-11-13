//! The subjects and objects events carry, read from the world's tables
//! inside the write that changes them. A write reads an object before and
//! after it changes it, and its event's `changes` are the difference, so
//! every field an object holds is read here and nowhere else.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::events::*;
use crate::types::{
    ActorEmail, ActorKind, CheckConclusion, CheckStatus, OrgRole, PullState, ReviewState, TeamRole,
};
use crate::{Result, WorldError, queries};

/// The rows `sql` selects with `id` as its one parameter.
pub(crate) fn list_by<T>(
    conn: &Connection,
    sql: &str,
    id: i64,
    f: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    Ok(conn
        .prepare(sql)?
        .query_map(params![id], f)?
        .collect::<rusqlite::Result<Vec<T>>>()?)
}

fn actor_ref_at(r: &Row<'_>, id: usize) -> rusqlite::Result<ActorRef> {
    Ok(ActorRef {
        id: r.get(id)?,
        login: r.get(id + 1)?,
    })
}

fn optional_actor_ref_at(r: &Row<'_>, id: usize) -> rusqlite::Result<Option<ActorRef>> {
    let id_value: Option<i64> = r.get(id)?;
    Ok(match id_value {
        Some(id_value) => Some(ActorRef {
            id: id_value,
            login: r.get(id + 1)?,
        }),
        None => None,
    })
}

pub(crate) fn actor_ref(conn: &Connection, id: i64) -> Result<ActorRef> {
    conn.query_row(
        "SELECT id, login FROM actors WHERE id = ?1",
        params![id],
        |r| actor_ref_at(r, 0),
    )
    .optional()?
    .ok_or(WorldError::NoSuchActorId(id))
}

pub(crate) fn world_object(conn: &Connection) -> Result<WorldObject> {
    let (clock_unix, clock_mode) = queries::clock_state(conn)?;
    Ok(WorldObject {
        clock_unix,
        clock_mode,
    })
}

pub(crate) fn actor_subject(conn: &Connection, id: i64) -> Result<ActorSubject> {
    conn.query_row(
        "SELECT id, login, kind FROM actors WHERE id = ?1",
        params![id],
        |r| {
            Ok(ActorSubject {
                kind: ActorType::Tag,
                id: r.get(0)?,
                login: r.get(1)?,
                actor_kind: ActorKind::from_db(&r.get::<_, String>(2)?),
            })
        },
    )
    .optional()?
    .ok_or(WorldError::NoSuchActorId(id))
}

pub(crate) fn actor_object(conn: &Connection, id: i64) -> Result<ActorObject> {
    let (login, kind, created_unix) = conn
        .query_row(
            "SELECT login, kind, created_clock_unix FROM actors WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    ActorKind::from_db(&r.get::<_, String>(1)?),
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or(WorldError::NoSuchActorId(id))?;
    let emails = list_by(
        conn,
        "SELECT email, is_primary, verified FROM actor_emails WHERE actor_id = ?1
         ORDER BY is_primary DESC, email",
        id,
        |r| {
            Ok(ActorEmail {
                email: r.get(0)?,
                primary: r.get(1)?,
                verified: r.get(2)?,
            })
        },
    )?;
    Ok(ActorObject {
        id,
        login,
        kind,
        created_unix,
        emails,
    })
}

pub(crate) fn oauth_app_subject(conn: &Connection, id: i64) -> Result<OAuthAppSubject> {
    Ok(conn.query_row(
        "SELECT id, client_id, name FROM oauth_apps WHERE id = ?1",
        params![id],
        |r| {
            Ok(OAuthAppSubject {
                kind: OAuthAppType::Tag,
                id: r.get(0)?,
                client_id: r.get(1)?,
                name: r.get(2)?,
            })
        },
    )?)
}

pub(crate) fn oauth_app_object(conn: &Connection, id: i64) -> Result<OAuthAppObject> {
    Ok(conn.query_row(
        "SELECT id, name, client_id, callback_url, created_clock_unix FROM oauth_apps
         WHERE id = ?1",
        params![id],
        |r| {
            Ok(OAuthAppObject {
                id: r.get(0)?,
                name: r.get(1)?,
                client_id: r.get(2)?,
                callback_url: r.get(3)?,
                created_unix: r.get(4)?,
            })
        },
    )?)
}

pub(crate) fn oauth_authorization_subject(
    conn: &Connection,
    id: i64,
) -> Result<OAuthAuthorizationSubject> {
    let object = oauth_authorization_object(conn, id)?;
    Ok(OAuthAuthorizationSubject {
        kind: OAuthAuthorizationType::Tag,
        id,
        app: object.app,
        actor: object.actor,
    })
}

pub(crate) fn oauth_authorization_object(
    conn: &Connection,
    id: i64,
) -> Result<OAuthAuthorizationObject> {
    Ok(conn.query_row(
        "SELECT c.id, p.id, p.client_id, a.id, a.login, c.redirect_uri, c.scope,
                c.created_clock_unix, c.exchanged_clock_unix
         FROM oauth_codes c JOIN oauth_apps p ON p.id = c.app_id
         JOIN actors a ON a.id = c.actor_id
         WHERE c.id = ?1",
        params![id],
        |r| {
            Ok(OAuthAuthorizationObject {
                id: r.get(0)?,
                app: AppRef {
                    id: r.get(1)?,
                    client_id: r.get(2)?,
                },
                actor: actor_ref_at(r, 3)?,
                redirect_uri: r.get(5)?,
                scope: r.get(6)?,
                created_unix: r.get(7)?,
                exchanged_unix: r.get(8)?,
            })
        },
    )?)
}

/// A repository's ref and its owner's.
pub(crate) fn repo_refs(conn: &Connection, repo_id: i64) -> Result<(RepoRef, ActorRef)> {
    conn.query_row(
        "SELECT r.id, r.name, a.id, a.login FROM repos r JOIN actors a ON a.id = r.owner_id
         WHERE r.id = ?1",
        params![repo_id],
        |r| {
            Ok((
                RepoRef {
                    id: r.get(0)?,
                    name: r.get(1)?,
                },
                actor_ref_at(r, 2)?,
            ))
        },
    )
    .optional()?
    .ok_or(WorldError::NoSuchRepo(repo_id))
}

pub(crate) fn repo_subject(conn: &Connection, repo_id: i64) -> Result<RepoSubject> {
    let (repo, owner) = repo_refs(conn, repo_id)?;
    Ok(RepoSubject {
        kind: RepoType::Tag,
        id: repo.id,
        name: repo.name,
        owner,
    })
}

pub(crate) fn repo_object(conn: &Connection, repo_id: i64) -> Result<RepoObject> {
    conn.query_row(
        "SELECT r.id, r.name, o.id, o.login, r.description, r.private, r.default_branch,
                c.id, c.login, r.created_clock_unix, r.pushed_at_clock_unix
         FROM repos r JOIN actors o ON o.id = r.owner_id JOIN actors c ON c.id = r.created_by
         WHERE r.id = ?1",
        params![repo_id],
        |r| {
            Ok(RepoObject {
                id: r.get(0)?,
                name: r.get(1)?,
                owner: actor_ref_at(r, 2)?,
                description: r.get(4)?,
                private: r.get(5)?,
                default_branch: r.get(6)?,
                created_by: actor_ref_at(r, 7)?,
                created_unix: r.get(9)?,
                pushed_unix: r.get(10)?,
            })
        },
    )
    .optional()?
    .ok_or(WorldError::NoSuchRepo(repo_id))
}

pub(crate) fn ref_subject(conn: &Connection, repo_id: i64, name: &str) -> Result<RefSubject> {
    let (repo, owner) = repo_refs(conn, repo_id)?;
    Ok(RefSubject {
        kind: RefType::Tag,
        name: name.to_string(),
        repo,
        owner,
    })
}

/// A pull's ref, and its repository's and owner's.
fn pull_refs(conn: &Connection, pull_id: i64) -> Result<(PullRef, RepoRef, ActorRef)> {
    let (number, repo_id): (i64, i64) = conn
        .query_row(
            "SELECT number, repo_id FROM pulls WHERE id = ?1",
            params![pull_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or(WorldError::NoSuchPull(pull_id))?;
    let (repo, owner) = repo_refs(conn, repo_id)?;
    Ok((
        PullRef {
            id: pull_id,
            number,
        },
        repo,
        owner,
    ))
}

pub(crate) fn pull_subject(conn: &Connection, pull_id: i64) -> Result<PullSubject> {
    let (pull, repo, owner) = pull_refs(conn, pull_id)?;
    Ok(PullSubject {
        kind: PullType::Tag,
        id: pull.id,
        number: pull.number,
        repo,
        owner,
    })
}

pub(crate) fn pull_object(conn: &Connection, pull_id: i64) -> Result<PullObject> {
    let mut pull = conn
        .query_row(
            "SELECT p.id, p.number, p.title, p.body, a.id, a.login, p.state, p.draft,
                    p.head_ref, p.head_sha, p.base_ref, p.base_sha, p.merged,
                    p.merge_commit_sha, m.id, m.login, p.created_clock_unix,
                    p.updated_clock_unix, p.closed_clock_unix, p.base_moved,
                    p.test_merge_stale
             FROM pulls p JOIN actors a ON a.id = p.author_id
             LEFT JOIN actors m ON m.id = p.merged_by_id
             WHERE p.id = ?1",
            params![pull_id],
            |r| {
                Ok(PullObject {
                    id: r.get(0)?,
                    number: r.get(1)?,
                    title: r.get(2)?,
                    body: r.get(3)?,
                    author: actor_ref_at(r, 4)?,
                    state: PullState::from_db(&r.get::<_, String>(6)?),
                    draft: r.get(7)?,
                    head: Branch {
                        name: r.get(8)?,
                        sha: r.get(9)?,
                    },
                    base: Branch {
                        name: r.get(10)?,
                        sha: r.get(11)?,
                    },
                    merged: r.get(12)?,
                    merge_commit_sha: r.get(13)?,
                    merged_by: optional_actor_ref_at(r, 14)?,
                    created_unix: r.get(16)?,
                    updated_unix: r.get(17)?,
                    closed_unix: r.get(18)?,
                    labels: Vec::new(),
                    requested_reviewers: Vec::new(),
                    requested_teams: Vec::new(),
                    base_moved: r.get(19)?,
                    test_merge_stale: r.get(20)?,
                })
            },
        )
        .optional()?
        .ok_or(WorldError::NoSuchPull(pull_id))?;
    pull.labels = list_by(
        conn,
        "SELECT name FROM pull_labels WHERE pull_id = ?1 ORDER BY name COLLATE NOCASE",
        pull_id,
        |r| r.get(0),
    )?;
    pull.requested_reviewers = list_by(
        conn,
        "SELECT a.id, a.login FROM review_requests q JOIN actors a ON a.id = q.reviewer_id
         WHERE q.pull_id = ?1 ORDER BY q.id",
        pull_id,
        |r| actor_ref_at(r, 0),
    )?;
    pull.requested_teams = list_by(
        conn,
        "SELECT t.id, t.slug FROM review_requests q JOIN teams t ON t.id = q.team_id
         WHERE q.pull_id = ?1 ORDER BY q.id",
        pull_id,
        |r| {
            Ok(TeamRef {
                id: r.get(0)?,
                slug: r.get(1)?,
            })
        },
    )?;
    Ok(pull)
}

pub(crate) fn timeline_item_subject(conn: &Connection, id: i64) -> Result<TimelineItemSubject> {
    let pull_id: i64 = conn.query_row(
        "SELECT pull_id FROM pull_timeline WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    let (pull, repo, owner) = pull_refs(conn, pull_id)?;
    Ok(TimelineItemSubject {
        kind: TimelineItemType::Tag,
        id,
        pull,
        repo,
        owner,
    })
}

pub(crate) fn timeline_item_object(conn: &Connection, id: i64) -> Result<TimelineItemObject> {
    Ok(conn.query_row(
        "SELECT i.id, i.kind, i.label, i.ref_name, v.id, v.login, i.team_id, i.team_slug,
                a.id, a.login, i.clock_unix
         FROM pull_timeline i JOIN actors a ON a.id = i.actor_id
         LEFT JOIN actors v ON v.id = i.reviewer_id
         WHERE i.id = ?1",
        params![id],
        |r| {
            let team_id: Option<i64> = r.get(6)?;
            Ok(TimelineItemObject {
                id: r.get(0)?,
                kind: timeline_item_kind(&r.get::<_, String>(1)?),
                label: r.get(2)?,
                ref_name: r.get(3)?,
                reviewer: optional_actor_ref_at(r, 4)?,
                team: match team_id {
                    Some(id) => Some(TeamRef {
                        id,
                        slug: r.get(7)?,
                    }),
                    None => None,
                },
                actor: actor_ref_at(r, 8)?,
                created_unix: r.get(10)?,
            })
        },
    )?)
}

/// The kinds the table's CHECK admits.
fn timeline_item_kind(s: &str) -> TimelineItemKind {
    match s {
        "labeled" => TimelineItemKind::Labeled,
        "unlabeled" => TimelineItemKind::Unlabeled,
        "ready_for_review" => TimelineItemKind::ReadyForReview,
        "review_requested" => TimelineItemKind::ReviewRequested,
        "review_request_removed" => TimelineItemKind::ReviewRequestRemoved,
        "closed" => TimelineItemKind::Closed,
        "head_ref_deleted" => TimelineItemKind::HeadRefDeleted,
        "base_ref_deleted" => TimelineItemKind::BaseRefDeleted,
        _ => TimelineItemKind::HeadRefRestored,
    }
}

pub(crate) fn review_subject(conn: &Connection, id: i64) -> Result<ReviewSubject> {
    let pull_id: i64 = conn.query_row(
        "SELECT pull_id FROM reviews WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    let (pull, repo, owner) = pull_refs(conn, pull_id)?;
    Ok(ReviewSubject {
        kind: ReviewType::Tag,
        id,
        pull,
        repo,
        owner,
    })
}

pub(crate) fn review_object(conn: &Connection, id: i64) -> Result<ReviewObject> {
    Ok(conn.query_row(
        "SELECT v.id, a.id, a.login, v.state, v.body, v.commit_sha, v.submitted_clock_unix
         FROM reviews v JOIN actors a ON a.id = v.author_id WHERE v.id = ?1",
        params![id],
        |r| {
            Ok(ReviewObject {
                id: r.get(0)?,
                author: actor_ref_at(r, 1)?,
                state: ReviewState::from_db(&r.get::<_, String>(3)?),
                body: r.get(4)?,
                commit_sha: r.get(5)?,
                submitted_unix: r.get(6)?,
            })
        },
    )?)
}

pub(crate) fn comment_subject(conn: &Connection, id: i64) -> Result<CommentSubject> {
    let pull_id: i64 = conn.query_row(
        "SELECT pull_id FROM issue_comments WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    let (pull, repo, owner) = pull_refs(conn, pull_id)?;
    Ok(CommentSubject {
        kind: CommentType::Tag,
        id,
        pull,
        repo,
        owner,
    })
}

pub(crate) fn comment_object(conn: &Connection, id: i64) -> Result<CommentObject> {
    Ok(conn.query_row(
        "SELECT c.id, a.id, a.login, c.body, c.created_clock_unix, c.updated_clock_unix
         FROM issue_comments c JOIN actors a ON a.id = c.author_id WHERE c.id = ?1",
        params![id],
        |r| {
            Ok(CommentObject {
                id: r.get(0)?,
                author: actor_ref_at(r, 1)?,
                body: r.get(3)?,
                created_unix: r.get(4)?,
                updated_unix: r.get(5)?,
            })
        },
    )?)
}

pub(crate) fn check_run_subject(conn: &Connection, id: i64) -> Result<CheckRunSubject> {
    let (repo_id, name, head_sha): (i64, String, String) = conn
        .query_row(
            "SELECT repo_id, name, head_sha FROM check_runs WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or(WorldError::NoSuchCheckRun(id))?;
    let (repo, owner) = repo_refs(conn, repo_id)?;
    Ok(CheckRunSubject {
        kind: CheckRunType::Tag,
        id,
        name,
        head_sha,
        repo,
        owner,
    })
}

pub(crate) fn check_run_object(conn: &Connection, id: i64) -> Result<CheckRunObject> {
    conn.query_row(
        "SELECT c.id, c.head_sha, c.name, c.status, c.conclusion, c.details_url,
                c.external_id, c.output, a.id, a.login, c.started_clock_unix,
                c.completed_clock_unix, c.created_clock_unix
         FROM check_runs c JOIN actors a ON a.id = c.creator_id WHERE c.id = ?1",
        params![id],
        |r| {
            let output: Option<String> = r.get(7)?;
            Ok(CheckRunObject {
                id: r.get(0)?,
                head_sha: r.get(1)?,
                name: r.get(2)?,
                status: CheckStatus::from_db(&r.get::<_, String>(3)?),
                conclusion: r
                    .get::<_, Option<String>>(4)?
                    .as_deref()
                    .map(CheckConclusion::from_db),
                details_url: r.get(5)?,
                external_id: r.get(6)?,
                output: output.map(|o| serde_json::from_str(&o).unwrap_or(serde_json::Value::Null)),
                creator: actor_ref_at(r, 8)?,
                started_unix: r.get(10)?,
                completed_unix: r.get(11)?,
                created_unix: r.get(12)?,
            })
        },
    )
    .optional()?
    .ok_or(WorldError::NoSuchCheckRun(id))
}

pub(crate) fn org_membership_subject(
    conn: &Connection,
    org_id: i64,
    member_id: i64,
) -> Result<OrgMembershipSubject> {
    Ok(OrgMembershipSubject {
        kind: OrgMembershipType::Tag,
        member: actor_ref(conn, member_id)?,
        owner: actor_ref(conn, org_id)?,
    })
}

/// `None` when `member_id` is not a member of the organization.
pub(crate) fn org_membership_object(
    conn: &Connection,
    org_id: i64,
    member_id: i64,
) -> Result<Option<OrgMembershipObject>> {
    Ok(conn
        .query_row(
            "SELECT a.id, a.login, m.role, m.created_clock_unix
             FROM org_members m JOIN actors a ON a.id = m.member_id
             WHERE m.org_id = ?1 AND m.member_id = ?2",
            params![org_id, member_id],
            |r| {
                Ok(OrgMembershipObject {
                    member: actor_ref_at(r, 0)?,
                    role: OrgRole::from_db(&r.get::<_, String>(2)?),
                    created_unix: r.get(3)?,
                })
            },
        )
        .optional()?)
}

pub(crate) fn team_subject(conn: &Connection, team_id: i64) -> Result<TeamSubject> {
    let (slug, org_id): (String, i64) = conn.query_row(
        "SELECT slug, org_id FROM teams WHERE id = ?1",
        params![team_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(TeamSubject {
        kind: TeamType::Tag,
        id: team_id,
        slug,
        owner: actor_ref(conn, org_id)?,
    })
}

pub(crate) fn team_object(conn: &Connection, team_id: i64) -> Result<TeamObject> {
    Ok(conn.query_row(
        "SELECT id, slug, name, description, created_clock_unix FROM teams WHERE id = ?1",
        params![team_id],
        |r| {
            Ok(TeamObject {
                id: r.get(0)?,
                slug: r.get(1)?,
                name: r.get(2)?,
                description: r.get(3)?,
                created_unix: r.get(4)?,
            })
        },
    )?)
}

pub(crate) fn team_membership_subject(
    conn: &Connection,
    team_id: i64,
    member_id: i64,
) -> Result<TeamMembershipSubject> {
    let team = team_subject(conn, team_id)?;
    Ok(TeamMembershipSubject {
        kind: TeamMembershipType::Tag,
        member: actor_ref(conn, member_id)?,
        team: TeamRef {
            id: team.id,
            slug: team.slug,
        },
        owner: team.owner,
    })
}

/// `None` when `member_id` is not on the team.
pub(crate) fn team_membership_object(
    conn: &Connection,
    team_id: i64,
    member_id: i64,
) -> Result<Option<TeamMembershipObject>> {
    Ok(conn
        .query_row(
            "SELECT a.id, a.login, m.role, m.created_clock_unix
             FROM team_members m JOIN actors a ON a.id = m.member_id
             WHERE m.team_id = ?1 AND m.member_id = ?2",
            params![team_id, member_id],
            |r| {
                Ok(TeamMembershipObject {
                    member: actor_ref_at(r, 0)?,
                    role: TeamRole::from_db(&r.get::<_, String>(2)?),
                    created_unix: r.get(3)?,
                })
            },
        )
        .optional()?)
}

fn app_ref_at(r: &Row<'_>, id: usize) -> rusqlite::Result<AppRef> {
    Ok(AppRef {
        id: r.get(id)?,
        client_id: r.get(id + 1)?,
    })
}

pub(crate) fn app_subject(conn: &Connection, id: i64) -> Result<AppSubject> {
    Ok(conn.query_row(
        "SELECT a.id, b.login, o.client_id
         FROM apps a JOIN oauth_apps o ON o.id = a.id JOIN actors b ON b.id = a.bot_id
         WHERE a.id = ?1",
        params![id],
        |r| {
            Ok(AppSubject {
                kind: AppType::Tag,
                id: r.get(0)?,
                slug: r.get(1)?,
                client_id: r.get(2)?,
            })
        },
    )?)
}

pub(crate) fn app_object(conn: &Connection, id: i64) -> Result<AppObject> {
    let mut object = conn.query_row(
        "SELECT a.id, b.login, o.name, o.client_id, w.id, w.login, b.id, b.login,
                o.callback_url, a.permissions, a.events, o.created_clock_unix
         FROM apps a
         JOIN oauth_apps o ON o.id = a.id
         JOIN actors b ON b.id = a.bot_id
         JOIN actors w ON w.id = a.owner_id
         WHERE a.id = ?1",
        params![id],
        |r| {
            Ok(AppObject {
                id: r.get(0)?,
                slug: r.get(1)?,
                name: r.get(2)?,
                client_id: r.get(3)?,
                owner: actor_ref_at(r, 4)?,
                bot: actor_ref_at(r, 6)?,
                callback_urls: vec![r.get(8)?],
                permissions: crate::apps::json_column(r, 9)?,
                events: crate::apps::json_column(r, 10)?,
                created_unix: r.get(11)?,
            })
        },
    )?;
    object.callback_urls.extend(list_by(
        conn,
        "SELECT url FROM app_callback_urls WHERE app_id = ?1 ORDER BY n",
        id,
        |r| r.get::<_, String>(0),
    )?);
    Ok(object)
}

pub(crate) fn app_key_subject(conn: &Connection, id: i64) -> Result<AppKeySubject> {
    Ok(conn.query_row(
        "SELECT k.id, o.id, o.client_id FROM app_keys k JOIN oauth_apps o ON o.id = k.app_id
         WHERE k.id = ?1",
        params![id],
        |r| {
            Ok(AppKeySubject {
                kind: AppKeyType::Tag,
                id: r.get(0)?,
                app: app_ref_at(r, 1)?,
            })
        },
    )?)
}

pub(crate) fn app_key_object(conn: &Connection, id: i64) -> Result<AppKeyObject> {
    Ok(conn.query_row(
        "SELECT id, public_key, created_clock_unix FROM app_keys WHERE id = ?1",
        params![id],
        |r| {
            Ok(AppKeyObject {
                id: r.get(0)?,
                public_key: r.get(1)?,
                created_unix: r.get(2)?,
            })
        },
    )?)
}

pub(crate) fn installation_subject(conn: &Connection, id: i64) -> Result<InstallationSubject> {
    Ok(conn.query_row(
        "SELECT i.id, o.id, o.client_id, c.id, c.login
         FROM app_installations i
         JOIN oauth_apps o ON o.id = i.app_id
         JOIN actors c ON c.id = i.account_id
         WHERE i.id = ?1",
        params![id],
        |r| {
            Ok(InstallationSubject {
                kind: InstallationType::Tag,
                id: r.get(0)?,
                app: app_ref_at(r, 1)?,
                account: actor_ref_at(r, 3)?,
            })
        },
    )?)
}

pub(crate) fn installation_object(conn: &Connection, id: i64) -> Result<InstallationObject> {
    let mut object = conn.query_row(
        "SELECT i.id, c.id, c.login, i.repository_selection, i.permissions, i.events,
                i.created_clock_unix
         FROM app_installations i JOIN actors c ON c.id = i.account_id
         WHERE i.id = ?1",
        params![id],
        |r| {
            Ok(InstallationObject {
                id: r.get(0)?,
                account: actor_ref_at(r, 1)?,
                repository_selection: crate::RepositorySelection::from_db(&r.get::<_, String>(3)?),
                repositories: Vec::new(),
                permissions: crate::apps::json_column(r, 4)?,
                events: crate::apps::json_column(r, 5)?,
                created_unix: r.get(6)?,
            })
        },
    )?;
    object.repositories = list_by(
        conn,
        "SELECT r.id, r.name FROM app_installation_repos s JOIN repos r ON r.id = s.repo_id
         WHERE s.installation_id = ?1 ORDER BY r.id",
        id,
        repo_ref_at,
    )?;
    Ok(object)
}

pub(crate) fn installation_token_subject(
    conn: &Connection,
    id: i64,
) -> Result<InstallationTokenSubject> {
    Ok(conn.query_row(
        "SELECT t.id, i.id, o.id, o.client_id
         FROM app_installation_tokens t
         JOIN app_installations i ON i.id = t.installation_id
         JOIN oauth_apps o ON o.id = i.app_id
         WHERE t.id = ?1",
        params![id],
        |r| {
            Ok(InstallationTokenSubject {
                kind: InstallationTokenType::Tag,
                id: r.get(0)?,
                installation: InstallationRef { id: r.get(1)? },
                app: app_ref_at(r, 2)?,
            })
        },
    )?)
}

pub(crate) fn installation_token_object(
    conn: &Connection,
    id: i64,
) -> Result<InstallationTokenObject> {
    let mut object = conn.query_row(
        "SELECT id, permissions, created_clock_unix, expires_clock_unix
         FROM app_installation_tokens WHERE id = ?1",
        params![id],
        |r| {
            Ok(InstallationTokenObject {
                id: r.get(0)?,
                repositories: Vec::new(),
                permissions: crate::apps::json_column(r, 1)?,
                created_unix: r.get(2)?,
                expires_unix: r.get(3)?,
            })
        },
    )?;
    object.repositories = list_by(
        conn,
        "SELECT r.id, r.name FROM app_installation_token_repos s JOIN repos r ON r.id = s.repo_id
         WHERE s.token_id = ?1 ORDER BY r.id",
        id,
        repo_ref_at,
    )?;
    Ok(object)
}

fn repo_ref_at(r: &Row<'_>) -> rusqlite::Result<RepoRef> {
    Ok(RepoRef {
        id: r.get(0)?,
        name: r.get(1)?,
    })
}
