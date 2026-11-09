//! Organization membership and teams. GitHub lets only an organization's
//! members join its teams, so a team membership requires an org membership.
//! The world keeps no permission model: any authenticated actor may make
//! these writes, as any actor may label or merge.

use rusqlite::{Connection, OptionalExtension, params};

use crate::events::{self, Attribution, EventBody};
use crate::types::*;
use crate::{Result, World, WorldError, objects, queries, uniqueness_violation};

impl World {
    /// Add `login` to `org` with `role`, or change the role of a member,
    /// GitHub's `PUT /orgs/{org}/memberships/{username}`. Only a person can
    /// be a member. Setting the role a member already has writes nothing.
    pub fn set_org_membership(
        &self,
        org: &str,
        login: &str,
        role: OrgRole,
        by: Attribution,
    ) -> Result<OrgMembership> {
        self.write(by, |w| {
            let tx = w.tx;
            let org_id = require_org(tx, org)?;
            let member_id = require_person(tx, login)?;
            if let Some(existing) = org_membership(tx, org_id, member_id)?
                && existing.role == role
            {
                return Ok(existing);
            }
            let before = objects::org_membership_object(tx, org_id, member_id)?;
            let clock = queries::clock(tx)?;
            let membership = tx.query_row(
                &format!(
                    "INSERT INTO org_members (org_id, member_id, role, created_clock_unix)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (org_id, member_id) DO UPDATE SET role = excluded.role
                     RETURNING {}",
                    queries::ORG_MEMBER_COLUMNS
                ),
                params![org_id, member_id, role.as_str(), clock],
                queries::row_to_org_member,
            )?;
            let subject = objects::org_membership_subject(tx, org_id, member_id)?;
            let after = objects::org_membership_object(tx, org_id, member_id)?
                .expect("the membership was just written");
            w.operation(match before {
                None => EventBody::OrgMembershipCreated(events::OrgMembershipCreated {
                    subject,
                    object: after,
                }),
                Some(before) => EventBody::OrgMembershipUpdated(events::OrgMembershipChanged {
                    subject,
                    changes: events::OrgMembershipChanges::between(&before, &after),
                }),
            });
            Ok(membership)
        })
    }

    /// `login`'s membership of `org`, if any.
    pub fn org_membership(&self, org: &str, login: &str) -> Result<Option<OrgMembership>> {
        self.with_conn(|conn| {
            let Some(org_id) = org_id(conn, org)? else {
                return Ok(None);
            };
            let Some(member_id) = person_id(conn, login)? else {
                return Ok(None);
            };
            org_membership(conn, org_id, member_id)
        })
    }

    /// Create a team in `org`, GitHub's `POST /orgs/{org}/teams`. The slug is
    /// derived from the name as GitHub derives it. A creator who is a member
    /// of the organization becomes the team's first maintainer, as on GitHub.
    pub fn create_team(
        &self,
        org: &str,
        name: &str,
        description: Option<&str>,
        by: Attribution,
    ) -> Result<Team> {
        let name = name.trim();
        let slug = team_slug(name);
        if slug.is_empty() {
            return Err(WorldError::InvalidTeamName(name.to_string()));
        }
        self.write(by, |w| {
            let tx = w.tx;
            let org_id = require_org(tx, org)?;
            let clock = queries::clock(tx)?;
            let team = tx
                .query_row(
                    &format!(
                        "INSERT INTO teams (org_id, slug, name, description, created_clock_unix)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         RETURNING {}",
                        queries::TEAM_COLUMNS
                    ),
                    params![org_id, slug, name, description, clock],
                    queries::row_to_team,
                )
                .map_err(|e| match uniqueness_violation(&e) {
                    true => WorldError::TeamExists(name.to_string()),
                    false => WorldError::from(e),
                })?;
            w.operation(EventBody::TeamCreated(events::TeamCreated {
                subject: objects::team_subject(tx, team.id)?,
                object: objects::team_object(tx, team.id)?,
            }));
            if org_membership(tx, org_id, by.actor_id)?.is_some() {
                tx.execute(
                    "INSERT INTO team_members (team_id, member_id, role, created_clock_unix)
                     VALUES (?1, ?2, 'maintainer', ?3)",
                    params![team.id, by.actor_id, clock],
                )?;
                w.emit(EventBody::TeamMembershipCreated(
                    events::TeamMembershipCreated {
                        subject: objects::team_membership_subject(tx, team.id, by.actor_id)?,
                        object: objects::team_membership_object(tx, team.id, by.actor_id)?
                            .expect("the membership was just written"),
                    },
                ));
            }
            Ok(team)
        })
    }

    pub fn team_by_slug(&self, org: &str, slug: &str) -> Result<Option<Team>> {
        self.with_conn(|conn| {
            let Some(org_id) = org_id(conn, org)? else {
                return Ok(None);
            };
            team_by_slug(conn, org_id, slug)
        })
    }

    /// Delete a team with its memberships and its pending review requests,
    /// GitHub's `DELETE /orgs/{org}/teams/{team_slug}`. Its past requests
    /// stay on pull timelines, which name the team by its id.
    pub fn delete_team(&self, org: &str, slug: &str, by: Attribution) -> Result<()> {
        self.write(by, |w| {
            let team = require_team(w.tx, org, slug)?;
            w.operation(EventBody::TeamDeleted(events::TeamDeleted {
                subject: objects::team_subject(w.tx, team.id)?,
                object: objects::team_object(w.tx, team.id)?,
            }));
            let pulls = ids(
                w.tx,
                "SELECT pull_id FROM review_requests WHERE team_id = ?1 ORDER BY id",
                team.id,
            )?;
            for pull_id in pulls {
                w.pull(pull_id, EventBody::PullReviewRequestRemoved, |tx| {
                    tx.execute(
                        "DELETE FROM review_requests WHERE pull_id = ?1 AND team_id = ?2",
                        params![pull_id, team.id],
                    )?;
                    Ok(())
                })?;
            }
            let members = ids(
                w.tx,
                "SELECT member_id FROM team_members WHERE team_id = ?1 ORDER BY member_id",
                team.id,
            )?;
            for member_id in members {
                remove_member(w, team.id, member_id)?;
            }
            w.tx.execute("DELETE FROM teams WHERE id = ?1", params![team.id])?;
            Ok(())
        })
    }

    /// Add `login` to a team or change their role on it, GitHub's `PUT
    /// /orgs/{org}/teams/{team_slug}/memberships/{username}`. The person must
    /// already be a member of the organization. Setting the role a member
    /// already has writes nothing.
    pub fn set_team_membership(
        &self,
        org: &str,
        slug: &str,
        login: &str,
        role: TeamRole,
        by: Attribution,
    ) -> Result<TeamMembership> {
        self.write(by, |w| {
            let tx = w.tx;
            let team = require_team(tx, org, slug)?;
            let member_id = require_person(tx, login)?;
            if org_membership(tx, team.org_id, member_id)?.is_none() {
                return Err(WorldError::NotOrgMember {
                    org: team.org_login,
                    login: login.to_string(),
                });
            }
            if let Some(existing) = team_membership(tx, team.id, member_id)?
                && existing.role == role
            {
                return Ok(existing);
            }
            let before = objects::team_membership_object(tx, team.id, member_id)?;
            let clock = queries::clock(tx)?;
            let membership = tx.query_row(
                &format!(
                    "INSERT INTO team_members (team_id, member_id, role, created_clock_unix)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (team_id, member_id) DO UPDATE SET role = excluded.role
                     RETURNING {}",
                    queries::TEAM_MEMBER_COLUMNS
                ),
                params![team.id, member_id, role.as_str(), clock],
                queries::row_to_team_member,
            )?;
            let subject = objects::team_membership_subject(tx, team.id, member_id)?;
            let after = objects::team_membership_object(tx, team.id, member_id)?
                .expect("the membership was just written");
            w.operation(match before {
                None => EventBody::TeamMembershipCreated(events::TeamMembershipCreated {
                    subject,
                    object: after,
                }),
                Some(before) => EventBody::TeamMembershipUpdated(events::TeamMembershipChanged {
                    subject,
                    changes: events::TeamMembershipChanges::between(&before, &after),
                }),
            });
            Ok(membership)
        })
    }

    /// `login`'s membership of the team, if any.
    pub fn team_membership(
        &self,
        org: &str,
        slug: &str,
        login: &str,
    ) -> Result<Option<TeamMembership>> {
        self.with_conn(|conn| {
            let Some(org_id) = org_id(conn, org)? else {
                return Ok(None);
            };
            let Some(team) = team_by_slug(conn, org_id, slug)? else {
                return Ok(None);
            };
            let Some(member_id) = person_id(conn, login)? else {
                return Ok(None);
            };
            team_membership(conn, team.id, member_id)
        })
    }

    /// Remove `login` from a team, GitHub's `DELETE
    /// /orgs/{org}/teams/{team_slug}/memberships/{username}`. A user who is
    /// not on the team is no error: GitHub documents only 204 and 403 for
    /// this call, so removing them succeeds and writes nothing.
    pub fn remove_team_membership(
        &self,
        org: &str,
        slug: &str,
        login: &str,
        by: Attribution,
    ) -> Result<()> {
        self.write(by, |w| {
            let team = require_team(w.tx, org, slug)?;
            let member_id = require_person(w.tx, login)?;
            remove_member(w, team.id, member_id)
        })
    }

    /// An organization's teams in creation order, optionally only those with
    /// at least one of `member_logins` on them.
    pub fn list_org_teams(
        &self,
        org_id: i64,
        member_logins: Option<&[String]>,
    ) -> Result<Vec<Team>> {
        self.with_conn(|conn| {
            let teams = list_teams(
                conn,
                &format!(
                    "SELECT {} FROM teams WHERE org_id = ?1 ORDER BY id",
                    queries::TEAM_COLUMNS
                ),
                org_id,
            )?;
            let Some(logins) = member_logins else {
                return Ok(teams);
            };
            let mut kept = Vec::new();
            for team in teams {
                let mut stmt = conn.prepare(
                    "SELECT a.login FROM team_members m JOIN actors a ON a.id = m.member_id
                     WHERE m.team_id = ?1",
                )?;
                let members = stmt
                    .query_map(params![team.id], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if members
                    .iter()
                    .any(|m| logins.iter().any(|l| l.eq_ignore_ascii_case(m)))
                {
                    kept.push(team);
                }
            }
            Ok(kept)
        })
    }

    /// The teams `member_id` is on, across organizations, in creation order.
    pub fn list_teams_for_member(&self, member_id: i64) -> Result<Vec<Team>> {
        self.with_conn(|conn| {
            list_teams(
                conn,
                &format!(
                    "SELECT {} FROM teams
                     WHERE id IN (SELECT team_id FROM team_members WHERE member_id = ?1)
                     ORDER BY id",
                    queries::TEAM_COLUMNS
                ),
                member_id,
            )
        })
    }

    /// A team by id, which is never reused once the team is deleted.
    pub fn team_by_id(&self, id: i64) -> Result<Option<Team>> {
        self.with_conn(|conn| team_by_id(conn, id))
    }

    /// How many people are on a team.
    pub fn team_members_count(&self, team_id: i64) -> Result<i64> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM team_members WHERE team_id = ?1",
                params![team_id],
                |r| r.get(0),
            )?)
        })
    }
}

/// Take `member_id` off the team, if they are on it, and emit the removal:
/// the write's operation if it has none yet.
fn remove_member(w: &mut crate::log::Write<'_>, team_id: i64, member_id: i64) -> Result<()> {
    let Some(object) = objects::team_membership_object(w.tx, team_id, member_id)? else {
        return Ok(());
    };
    let subject = objects::team_membership_subject(w.tx, team_id, member_id)?;
    w.tx.execute(
        "DELETE FROM team_members WHERE team_id = ?1 AND member_id = ?2",
        params![team_id, member_id],
    )?;
    let removed =
        EventBody::TeamMembershipDeleted(events::TeamMembershipDeleted { subject, object });
    match w.has_operation() {
        true => w.emit(removed),
        false => w.operation(removed),
    }
    Ok(())
}

/// The ids `sql` selects with `id` as its one parameter.
fn ids(conn: &Connection, sql: &str, id: i64) -> Result<Vec<i64>> {
    Ok(conn
        .prepare(sql)?
        .query_map(params![id], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?)
}

/// GitHub's slug for a team name: lower-cased, with every run of characters
/// other than ASCII letters and digits turned into one hyphen, and no hyphen
/// at either end.
fn team_slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_string()
}

fn org_id(conn: &Connection, org: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM actors WHERE login = ?1 AND kind = 'org'",
            params![org],
            |r| r.get(0),
        )
        .optional()?)
}

fn require_org(conn: &Connection, org: &str) -> Result<i64> {
    org_id(conn, org)?.ok_or_else(|| WorldError::NoSuchOrg(org.to_string()))
}

fn person_id(conn: &Connection, login: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM actors WHERE login = ?1 AND kind = 'user'",
            params![login],
            |r| r.get(0),
        )
        .optional()?)
}

fn require_person(conn: &Connection, login: &str) -> Result<i64> {
    person_id(conn, login)?.ok_or_else(|| WorldError::NoSuchActor(login.to_string()))
}

fn org_membership(conn: &Connection, org_id: i64, member_id: i64) -> Result<Option<OrgMembership>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {} FROM org_members WHERE org_id = ?1 AND member_id = ?2",
                queries::ORG_MEMBER_COLUMNS
            ),
            params![org_id, member_id],
            queries::row_to_org_member,
        )
        .optional()?)
}

fn team_membership(
    conn: &Connection,
    team_id: i64,
    member_id: i64,
) -> Result<Option<TeamMembership>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {} FROM team_members WHERE team_id = ?1 AND member_id = ?2",
                queries::TEAM_MEMBER_COLUMNS
            ),
            params![team_id, member_id],
            queries::row_to_team_member,
        )
        .optional()?)
}

fn team_by_id(conn: &Connection, id: i64) -> Result<Option<Team>> {
    Ok(conn
        .query_row(
            &format!("SELECT {} FROM teams WHERE id = ?1", queries::TEAM_COLUMNS),
            params![id],
            queries::row_to_team,
        )
        .optional()?)
}

fn team_by_slug(conn: &Connection, org_id: i64, slug: &str) -> Result<Option<Team>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {} FROM teams WHERE org_id = ?1 AND slug = ?2",
                queries::TEAM_COLUMNS
            ),
            params![org_id, slug],
            queries::row_to_team,
        )
        .optional()?)
}

fn require_team(conn: &Connection, org: &str, slug: &str) -> Result<Team> {
    let org_id = require_org(conn, org)?;
    team_by_slug(conn, org_id, slug)?.ok_or_else(|| WorldError::NoSuchTeam(slug.to_string()))
}

fn list_teams(conn: &Connection, sql: &str, arg: i64) -> Result<Vec<Team>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![arg], queries::row_to_team)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::team_slug;

    #[test]
    fn slugs_follow_github() {
        assert_eq!(team_slug("Platform Team"), "platform-team");
        assert_eq!(team_slug("  Web & API!! "), "web-api");
        assert_eq!(team_slug("core_infra"), "core-infra");
        assert_eq!(team_slug("!!!"), "");
    }
}
