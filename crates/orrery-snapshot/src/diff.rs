//! Semantic diff between two world states — the engine's verdict (rule 11):
//! "what happened between these two moments", spoken in world vocabulary
//! (repos, refs, pulls, reviews, checks), derived entirely from the two
//! states themselves. Judges may interpret above this; nothing overrides it.
//!
//! Rows correlate by primary key, which is sound because the two sides share
//! a lineage (a snapshot and the world it came from, or two snapshots of one
//! world) — ids never renumber within a lineage. Rows with no id of their
//! own, or one that can be freed and handed out again (memberships, labels,
//! addresses, review requests), correlate by what they name instead.
//! Entries always name things by stable identity — a login, `owner/repo` and
//! a pull number, `org/slug` — never by row id. The schema is fixed: every
//! section always present, empty when nothing happened, so an assertion
//! script never branches on shape.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use orrery_face_git::store;
use orrery_world::{ActorEmail, Pull, PullState, RequestedReviewer, Team, TimelineKind, WorldView};

use crate::Result;

/// One side of a diff: a name for the report, a view over its database, and
/// its bare-repo directory for ref comparison.
pub struct DiffSide<'a> {
    pub name: &'a str,
    pub view: &'a dyn WorldView,
    pub repos_dir: &'a Path,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SideMeta {
    pub name: String,
    pub clock_unix: i64,
    pub last_seq: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DefaultBranchChange {
    pub repo: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RefChange {
    pub repo: String,
    pub r#ref: String,
    /// Absent on either end means the ref did not exist there.
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PullRef {
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub author: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PullMerged {
    pub repo: String,
    pub number: i64,
    pub merge_commit_sha: Option<String>,
    pub merged_by: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PullSync {
    pub repo: String,
    pub number: i64,
    pub from_sha: String,
    pub to_sha: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PullEdited {
    pub repo: String,
    pub number: i64,
    pub fields: Vec<&'static str>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReviewAdded {
    pub repo: String,
    pub pull_number: i64,
    pub reviewer: String,
    pub state: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CommentAdded {
    pub repo: String,
    pub pull_number: i64,
    pub author: String,
    pub body: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckRunChange {
    pub repo: String,
    pub head_sha: String,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ActorAdded {
    pub login: String,
    pub kind: String,
    /// The addresses it holds on the `to` side, primary first.
    pub emails: Vec<ActorEmail>,
}

/// An existing actor whose addresses differ, with both whole sets, primary
/// first.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EmailsChanged {
    pub login: String,
    pub from: Vec<ActorEmail>,
    pub to: Vec<ActorEmail>,
}

/// An OAuth app, by its public identity; the client secret is never reported.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OAuthAppAdded {
    pub name: String,
    pub client_id: String,
}

/// An actor's authorization of an OAuth app. The code itself is never
/// reported.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Authorization {
    pub client_id: String,
    pub app: String,
    pub actor: String,
    pub scope: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LabelChange {
    pub repo: String,
    pub number: i64,
    pub label: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DraftChange {
    pub repo: String,
    pub number: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReadyForReview {
    pub repo: String,
    pub number: i64,
    pub by: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OrgMembershipChange {
    pub org: String,
    pub login: String,
    pub role: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OrgRoleChange {
    pub org: String,
    pub login: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TeamRef {
    pub org: String,
    pub slug: String,
    pub name: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TeamMembershipChange {
    pub org: String,
    pub team: String,
    pub login: String,
    pub role: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TeamRoleChange {
    pub org: String,
    pub team: String,
    pub login: String,
    pub from: String,
    pub to: String,
}

/// Whose review a request asks for: `{"user": login}` or
/// `{"team": "org/slug"}`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reviewer {
    User(String),
    Team(String),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReviewRequestChange {
    pub repo: String,
    pub number: i64,
    pub reviewer: Reviewer,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LogSpan {
    /// The newest `seq` both sides' logs hold, or 0 when they share no
    /// entry. The two logs are the same up to it and differ after it: it is
    /// `from`'s `last_seq` when `to` carries on from `from`, `to`'s when
    /// `from` carries on from `to`, and older than both when a restore
    /// between them discarded `from`'s newest entries.
    pub shared_seq: i64,
    /// How many of `to`'s entries follow `shared_seq`, counted rather than
    /// subtracted because `seq` has a gap wherever a restore discarded
    /// entries. The entries themselves are served by the log list read on a
    /// world whose log is `to`'s, from `after_seq: shared_seq`, not
    /// duplicated here.
    pub appended: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WorldDiff {
    pub from: SideMeta,
    pub to: SideMeta,
    pub actors_added: Vec<ActorAdded>,
    pub emails_changed: Vec<EmailsChanged>,
    pub oauth_apps_added: Vec<OAuthAppAdded>,
    /// Issued between the two sides; one also exchanged there appears in
    /// both lists, as a pull opened and merged in one interval does.
    pub authorizations_issued: Vec<Authorization>,
    pub authorizations_exchanged: Vec<Authorization>,
    pub repos_created: Vec<String>,
    pub repos_deleted: Vec<String>,
    pub default_branch_changes: Vec<DefaultBranchChange>,
    pub ref_changes: Vec<RefChange>,
    pub pulls_opened: Vec<PullRef>,
    pub pulls_closed: Vec<PullRef>,
    pub pulls_reopened: Vec<PullRef>,
    pub pulls_merged: Vec<PullMerged>,
    pub pulls_synchronized: Vec<PullSync>,
    pub pulls_edited: Vec<PullEdited>,
    /// From the timeline, so a pull marked ready and converted back within
    /// the interval still shows it was marked ready, and by whom.
    pub pulls_ready_for_review: Vec<ReadyForReview>,
    /// From the pull's state: the world keeps no timeline item for a
    /// conversion. A pull marked ready and converted back within the
    /// interval shows only in `pulls_ready_for_review`.
    pub pulls_converted_to_draft: Vec<DraftChange>,
    pub labels_added: Vec<LabelChange>,
    pub labels_removed: Vec<LabelChange>,
    /// Requests pending on `to` and not on `from`, and the reverse. A
    /// request cleared by the reviewer's review, or by its team's deletion,
    /// is removed like a withdrawn one.
    pub review_requests_added: Vec<ReviewRequestChange>,
    pub review_requests_removed: Vec<ReviewRequestChange>,
    pub reviews_added: Vec<ReviewAdded>,
    pub comments_added: Vec<CommentAdded>,
    pub check_runs_added: Vec<CheckRunChange>,
    pub check_runs_updated: Vec<CheckRunChange>,
    pub org_memberships_added: Vec<OrgMembershipChange>,
    pub org_memberships_removed: Vec<OrgMembershipChange>,
    pub org_roles_changed: Vec<OrgRoleChange>,
    /// Teams correlate by id, which is never reused: a team deleted and a
    /// new one created under its slug is one deletion and one creation.
    pub teams_created: Vec<TeamRef>,
    pub teams_deleted: Vec<TeamRef>,
    /// A deleted team's memberships are removed with it.
    pub team_memberships_added: Vec<TeamMembershipChange>,
    pub team_memberships_removed: Vec<TeamMembershipChange>,
    pub team_roles_changed: Vec<TeamRoleChange>,
    pub log: LogSpan,
}

pub fn diff(from: DiffSide<'_>, to: DiffSide<'_>) -> Result<WorldDiff> {
    let from_meta = from.view.view_meta()?;
    let to_meta = to.view.view_meta()?;

    let from_actors: BTreeMap<i64, _> = from
        .view
        .list_actors()?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    let from_emails = addresses(from.view)?;
    let mut to_emails = addresses(to.view)?;
    let mut actors_added = Vec::new();
    let mut emails_changed = Vec::new();
    for a in to.view.list_actors()? {
        let now = to_emails.remove(&a.id).unwrap_or_default();
        if !from_actors.contains_key(&a.id) {
            actors_added.push(ActorAdded {
                login: a.login,
                kind: a.kind.as_str().to_string(),
                emails: now,
            });
            continue;
        }
        let before = from_emails.get(&a.id).cloned().unwrap_or_default();
        if !same_addresses(&before, &now) {
            emails_changed.push(EmailsChanged {
                login: a.login,
                from: before,
                to: now,
            });
        }
    }

    let from_apps: BTreeSet<String> = from
        .view
        .list_oauth_apps()?
        .into_iter()
        .map(|a| a.client_id)
        .collect();
    let oauth_apps_added = to
        .view
        .list_oauth_apps()?
        .into_iter()
        .filter(|a| !from_apps.contains(&a.client_id))
        .map(|a| OAuthAppAdded {
            name: a.name,
            client_id: a.client_id,
        })
        .collect();

    let from_auths: BTreeMap<i64, _> = from
        .view
        .list_oauth_authorizations()?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    let mut authorizations_issued = Vec::new();
    let mut authorizations_exchanged = Vec::new();
    for a in to.view.list_oauth_authorizations()? {
        let old = from_auths.get(&a.id);
        let entry = Authorization {
            client_id: a.client_id.clone(),
            app: a.app_name.clone(),
            actor: a.actor_login.clone(),
            scope: a.scope.clone(),
        };
        let was_exchanged = old.is_some_and(|o| o.exchanged_clock_unix.is_some());
        if a.exchanged_clock_unix.is_some() && !was_exchanged {
            authorizations_exchanged.push(entry.clone());
        }
        if old.is_none() {
            authorizations_issued.push(entry);
        }
    }

    let from_repos: BTreeMap<i64, _> = from
        .view
        .list_repos()?
        .into_iter()
        .map(|r| (r.id, r))
        .collect();
    let to_repos: BTreeMap<i64, _> = to
        .view
        .list_repos()?
        .into_iter()
        .map(|r| (r.id, r))
        .collect();
    let repos_created = to_repos
        .values()
        .filter(|r| !from_repos.contains_key(&r.id))
        .map(|r| r.full_name())
        .collect();
    let repos_deleted = from_repos
        .values()
        .filter(|r| !to_repos.contains_key(&r.id))
        .map(|r| r.full_name())
        .collect();
    let default_branch_changes = to_repos
        .values()
        .filter_map(|r| {
            let old = from_repos.get(&r.id)?;
            (old.default_branch != r.default_branch).then(|| DefaultBranchChange {
                repo: r.full_name(),
                from: old.default_branch.clone(),
                to: r.default_branch.clone(),
            })
        })
        .collect();

    // Refs, from each side's actual git store. A repo absent on one side
    // contributes an empty ref set there, so creations and deletions read as
    // refs appearing/disappearing wholesale.
    let mut ref_changes = Vec::new();
    let repo_ids: BTreeSet<i64> = from_repos.keys().chain(to_repos.keys()).copied().collect();
    for id in repo_ids {
        let repo = to_repos.get(&id).or_else(|| from_repos.get(&id));
        let Some(repo) = repo else { continue };
        let full = repo.full_name();
        let from_refs = side_refs(from.repos_dir, &full, from_repos.contains_key(&id))?;
        let to_refs = side_refs(to.repos_dir, &full, to_repos.contains_key(&id))?;
        let names: BTreeSet<&String> = from_refs.keys().chain(to_refs.keys()).collect();
        for name in names {
            let old = from_refs.get(name);
            let new = to_refs.get(name);
            if old != new {
                ref_changes.push(RefChange {
                    repo: full.clone(),
                    r#ref: name.clone(),
                    from: old.cloned(),
                    to: new.cloned(),
                });
            }
        }
    }

    // Pulls: classify each id's transition. A pull that arrived and finished
    // between the two sides appears both as opened and as merged/closed —
    // both facts are true of the interval.
    let repo_name = |id: i64| -> String {
        to_repos
            .get(&id)
            .or_else(|| from_repos.get(&id))
            .map(|r| r.full_name())
            .unwrap_or_else(|| format!("repo:{id}"))
    };
    let from_pulls: BTreeMap<i64, Pull> = from
        .view
        .list_pulls()?
        .into_iter()
        .map(|p| (p.id, p))
        .collect();
    let mut pulls_opened = Vec::new();
    let mut pulls_closed = Vec::new();
    let mut pulls_reopened = Vec::new();
    let mut pulls_merged = Vec::new();
    let mut pulls_synchronized = Vec::new();
    let mut pulls_edited = Vec::new();
    let mut pulls_converted_to_draft = Vec::new();
    let mut labels_added = Vec::new();
    let mut labels_removed = Vec::new();
    for p in to.view.list_pulls()? {
        let repo = repo_name(p.repo_id);
        let pull_ref = || PullRef {
            repo: repo.clone(),
            number: p.number,
            title: p.title.clone(),
            author: p.author_login.clone(),
        };
        let old = from_pulls.get(&p.id);
        let old_labels = old.map(|o| label_set(&o.labels)).unwrap_or_default();
        let new_labels = label_set(&p.labels);
        for (key, label) in &new_labels {
            if !old_labels.contains_key(key) {
                labels_added.push(LabelChange {
                    repo: repo.clone(),
                    number: p.number,
                    label: label.clone(),
                });
            }
        }
        for (key, label) in &old_labels {
            if !new_labels.contains_key(key) {
                labels_removed.push(LabelChange {
                    repo: repo.clone(),
                    number: p.number,
                    label: label.clone(),
                });
            }
        }
        match old {
            None => {
                pulls_opened.push(pull_ref());
                if p.merged {
                    pulls_merged.push(merged_entry(&repo, &p));
                } else if p.state == PullState::Closed {
                    pulls_closed.push(pull_ref());
                }
            }
            Some(old) => {
                if !old.merged && p.merged {
                    pulls_merged.push(merged_entry(&repo, &p));
                } else if old.state == PullState::Open && p.state == PullState::Closed {
                    pulls_closed.push(pull_ref());
                } else if old.state == PullState::Closed && p.state == PullState::Open {
                    pulls_reopened.push(pull_ref());
                }
                if !old.draft && p.draft {
                    pulls_converted_to_draft.push(DraftChange {
                        repo: repo.clone(),
                        number: p.number,
                    });
                }
                if old.head_sha != p.head_sha {
                    pulls_synchronized.push(PullSync {
                        repo: repo.clone(),
                        number: p.number,
                        from_sha: old.head_sha.clone(),
                        to_sha: p.head_sha.clone(),
                    });
                }
                let mut fields = Vec::new();
                if old.title != p.title {
                    fields.push("title");
                }
                if old.body != p.body {
                    fields.push("body");
                }
                if old.base_ref != p.base_ref {
                    fields.push("base");
                }
                if !fields.is_empty() {
                    pulls_edited.push(PullEdited {
                        repo: repo.clone(),
                        number: p.number,
                        fields,
                    });
                }
            }
        }
    }

    let pull_number = {
        let mut numbers: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
        for p in to.view.list_pulls()? {
            numbers.insert(p.id, (p.repo_id, p.number));
        }
        for (id, p) in &from_pulls {
            numbers.entry(*id).or_insert((p.repo_id, p.number));
        }
        numbers
    };
    let locate = |pull_id: i64| -> (String, i64) {
        match pull_number.get(&pull_id) {
            Some((repo_id, number)) => (repo_name(*repo_id), *number),
            None => (format!("pull:{pull_id}"), 0),
        }
    };

    let from_reviews: BTreeSet<i64> = from
        .view
        .list_reviews()?
        .into_iter()
        .map(|r| r.id)
        .collect();
    let reviews_added = to
        .view
        .list_reviews()?
        .into_iter()
        .filter(|r| !from_reviews.contains(&r.id))
        .map(|r| {
            let (repo, pull_number) = locate(r.pull_id);
            ReviewAdded {
                repo,
                pull_number,
                reviewer: r.author_login,
                state: r.state.as_str().to_string(),
            }
        })
        .collect();

    let from_comments: BTreeSet<i64> = from
        .view
        .list_issue_comments()?
        .into_iter()
        .map(|c| c.id)
        .collect();
    let comments_added = to
        .view
        .list_issue_comments()?
        .into_iter()
        .filter(|c| !from_comments.contains(&c.id))
        .map(|c| {
            let (repo, pull_number) = locate(c.pull_id);
            CommentAdded {
                repo,
                pull_number,
                author: c.author_login,
                body: c.body,
            }
        })
        .collect();

    let from_checks: BTreeMap<i64, _> = from
        .view
        .list_check_runs()?
        .into_iter()
        .map(|c| (c.id, c))
        .collect();
    let mut check_runs_added = Vec::new();
    let mut check_runs_updated = Vec::new();
    for c in to.view.list_check_runs()? {
        let entry = CheckRunChange {
            repo: repo_name(c.repo_id),
            head_sha: c.head_sha.clone(),
            name: c.name.clone(),
            status: c.status.as_str().to_string(),
            conclusion: c.conclusion.map(|x| x.as_str().to_string()),
        };
        match from_checks.get(&c.id) {
            None => check_runs_added.push(entry),
            Some(old) if *old != c => check_runs_updated.push(entry),
            Some(_) => {}
        }
    }

    let from_timeline: BTreeSet<i64> = from
        .view
        .list_timeline()?
        .into_iter()
        .map(|t| t.id)
        .collect();
    let pulls_ready_for_review = to
        .view
        .list_timeline()?
        .into_iter()
        .filter(|t| !from_timeline.contains(&t.id) && t.kind == TimelineKind::ReadyForReview)
        .map(|t| {
            let (repo, number) = locate(t.pull_id);
            ReadyForReview {
                repo,
                number,
                by: t.actor_login,
            }
        })
        .collect();

    let from_teams: BTreeMap<i64, Team> = from
        .view
        .list_teams()?
        .into_iter()
        .map(|t| (t.id, t))
        .collect();
    let to_teams: BTreeMap<i64, Team> = to
        .view
        .list_teams()?
        .into_iter()
        .map(|t| (t.id, t))
        .collect();
    let team_ref = |t: &Team| TeamRef {
        org: t.org_login.clone(),
        slug: t.slug.clone(),
        name: t.name.clone(),
    };
    let teams_created = to_teams
        .values()
        .filter(|t| !from_teams.contains_key(&t.id))
        .map(team_ref)
        .collect();
    let teams_deleted = from_teams
        .values()
        .filter(|t| !to_teams.contains_key(&t.id))
        .map(team_ref)
        .collect();
    // Every row naming a team references it, so a team id read from either
    // side is a team on that side.
    let team_name = |id: i64| -> (String, String) {
        let t = to_teams
            .get(&id)
            .or_else(|| from_teams.get(&id))
            .expect("a referenced team exists on the side that references it");
        (t.org_login.clone(), t.slug.clone())
    };

    let requests = |view: &dyn WorldView| -> Result<BTreeSet<(i64, RequestKey)>> {
        Ok(view
            .list_review_requests()?
            .into_iter()
            .map(|q| {
                let key = match q.reviewer {
                    RequestedReviewer::User(login) => RequestKey::User(login),
                    RequestedReviewer::Team { id, .. } => RequestKey::Team(id),
                };
                (q.pull_id, key)
            })
            .collect())
    };
    let from_requests = requests(from.view)?;
    let to_requests = requests(to.view)?;
    let request_change = |(pull_id, key): &(i64, RequestKey)| {
        let (repo, number) = locate(*pull_id);
        let reviewer = match key {
            RequestKey::User(login) => Reviewer::User(login.clone()),
            RequestKey::Team(id) => {
                let (org, slug) = team_name(*id);
                Reviewer::Team(format!("{org}/{slug}"))
            }
        };
        ReviewRequestChange {
            repo,
            number,
            reviewer,
        }
    };
    let review_requests_added = to_requests
        .iter()
        .filter(|k| !from_requests.contains(k))
        .map(request_change)
        .collect();
    let review_requests_removed = from_requests
        .iter()
        .filter(|k| !to_requests.contains(k))
        .map(request_change)
        .collect();

    let org_members = |view: &dyn WorldView| -> Result<BTreeMap<(String, String), String>> {
        Ok(view
            .list_org_members()?
            .into_iter()
            .map(|m| ((m.org_login, m.member_login), m.role.as_str().to_string()))
            .collect())
    };
    let from_org = org_members(from.view)?;
    let to_org = org_members(to.view)?;
    let mut org_memberships_added = Vec::new();
    let mut org_roles_changed = Vec::new();
    for ((org, login), role) in &to_org {
        match from_org.get(&(org.clone(), login.clone())) {
            None => org_memberships_added.push(OrgMembershipChange {
                org: org.clone(),
                login: login.clone(),
                role: role.clone(),
            }),
            Some(old) if old != role => org_roles_changed.push(OrgRoleChange {
                org: org.clone(),
                login: login.clone(),
                from: old.clone(),
                to: role.clone(),
            }),
            Some(_) => {}
        }
    }
    let org_memberships_removed = from_org
        .iter()
        .filter(|(k, _)| !to_org.contains_key(*k))
        .map(|((org, login), role)| OrgMembershipChange {
            org: org.clone(),
            login: login.clone(),
            role: role.clone(),
        })
        .collect();

    let team_members = |view: &dyn WorldView| -> Result<BTreeMap<(i64, String), String>> {
        Ok(view
            .list_team_members()?
            .into_iter()
            .map(|m| ((m.team_id, m.member_login), m.role.as_str().to_string()))
            .collect())
    };
    let from_tm = team_members(from.view)?;
    let to_tm = team_members(to.view)?;
    let membership = |team_id: i64, login: &str, role: &str| {
        let (org, team) = team_name(team_id);
        TeamMembershipChange {
            org,
            team,
            login: login.to_string(),
            role: role.to_string(),
        }
    };
    let mut team_memberships_added = Vec::new();
    let mut team_roles_changed = Vec::new();
    for ((team_id, login), role) in &to_tm {
        match from_tm.get(&(*team_id, login.clone())) {
            None => team_memberships_added.push(membership(*team_id, login, role)),
            Some(old) if old != role => {
                let (org, team) = team_name(*team_id);
                team_roles_changed.push(TeamRoleChange {
                    org,
                    team,
                    login: login.clone(),
                    from: old.clone(),
                    to: role.clone(),
                });
            }
            Some(_) => {}
        }
    }
    let team_memberships_removed = from_tm
        .iter()
        .filter(|(k, _)| !to_tm.contains_key(*k))
        .map(|((team_id, login), role)| membership(*team_id, login, role))
        .collect();

    Ok(WorldDiff {
        from: SideMeta {
            name: from.name.to_string(),
            clock_unix: from_meta.clock_unix,
            last_seq: from_meta.last_seq,
        },
        to: SideMeta {
            name: to.name.to_string(),
            clock_unix: to_meta.clock_unix,
            last_seq: to_meta.last_seq,
        },
        actors_added,
        emails_changed,
        oauth_apps_added,
        authorizations_issued,
        authorizations_exchanged,
        repos_created,
        repos_deleted,
        default_branch_changes,
        ref_changes,
        pulls_opened,
        pulls_closed,
        pulls_reopened,
        pulls_merged,
        pulls_synchronized,
        pulls_edited,
        pulls_ready_for_review,
        pulls_converted_to_draft,
        labels_added,
        labels_removed,
        review_requests_added,
        review_requests_removed,
        reviews_added,
        comments_added,
        check_runs_added,
        check_runs_updated,
        org_memberships_added,
        org_memberships_removed,
        org_roles_changed,
        teams_created,
        teams_deleted,
        team_memberships_added,
        team_memberships_removed,
        team_roles_changed,
        log: {
            let shared_seq = shared_seq(from.view, from_meta.last_seq, to.view, to_meta.last_seq)?;
            LogSpan {
                shared_seq,
                appended: to.view.log_count_after(shared_seq)?,
            }
        },
    })
}

/// The newest `seq` both logs hold, given each one's newest.
///
/// A `seq` is issued once per world, above every `seq` issued before it, so
/// every log that holds an entry holds the same entries below it. Two logs
/// of one world are therefore the same up to the newest entry they share,
/// and share none after it: the entries of `from`'s that `to` holds are its
/// oldest ones, and the boundary between them is found by bisecting on
/// `seq`, reading neither log whole.
fn shared_seq(from: &dyn WorldView, from_seq: i64, to: &dyn WorldView, to_seq: i64) -> Result<i64> {
    if from_seq == 0 || to.log_holds(from_seq)? {
        return Ok(from_seq);
    }
    if to_seq == 0 || from.log_holds(to_seq)? {
        return Ok(to_seq);
    }
    // Whether `to` holds `from`'s newest entry at or below `m`: true at 0,
    // and false from the oldest entry of `from`'s that `to` lacks onwards,
    // which is at or below `from_seq`.
    let shared_at = |m: i64| -> Result<bool> {
        let seq = from.log_seq_at_or_before(m)?;
        Ok(seq == 0 || to.log_holds(seq)?)
    };
    let (mut shared, mut unshared) = (0, from_seq);
    while unshared - shared > 1 {
        let mid = shared + (unshared - shared) / 2;
        if shared_at(mid)? {
            shared = mid;
        } else {
            unshared = mid;
        }
    }
    Ok(from.log_seq_at_or_before(shared)?)
}

/// A review request's subject. A user is keyed by login, since a request's
/// row id can be freed and reused; a team by its id, which never is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum RequestKey {
    User(String),
    Team(i64),
}

/// Each actor's addresses, primary first, as the list read orders them.
fn addresses(view: &dyn WorldView) -> Result<BTreeMap<i64, Vec<ActorEmail>>> {
    let mut by_actor: BTreeMap<i64, Vec<ActorEmail>> = BTreeMap::new();
    for a in view.list_actor_addresses()? {
        by_actor.entry(a.actor_id).or_default().push(a.email);
    }
    Ok(by_actor)
}

/// Addresses compare without regard to case, as the world stores them.
fn same_addresses(a: &[ActorEmail], b: &[ActorEmail]) -> bool {
    let key = |e: &ActorEmail| (e.email.to_ascii_lowercase(), e.primary, e.verified);
    let a: BTreeSet<_> = a.iter().map(key).collect();
    let b: BTreeSet<_> = b.iter().map(key).collect();
    a == b
}

/// A pull's labels keyed without regard to case, as the world stores them.
fn label_set(labels: &[String]) -> BTreeMap<String, String> {
    labels
        .iter()
        .map(|l| (l.to_ascii_lowercase(), l.clone()))
        .collect()
}

fn merged_entry(repo: &str, p: &Pull) -> PullMerged {
    PullMerged {
        repo: repo.to_string(),
        number: p.number,
        merge_commit_sha: p.merge_commit_sha.clone(),
        merged_by: p.merged_by_login.clone(),
    }
}

/// A repo's refs on one side. `present` is whether the repo row exists there.
/// A present row with no bare directory is also an empty set: the kernel row
/// lands before disk init, so a snapshot can catch a repo in that gap.
fn side_refs(repos_dir: &Path, full_name: &str, present: bool) -> Result<BTreeMap<String, String>> {
    if !present {
        return Ok(BTreeMap::new());
    }
    let (owner, name) = full_name
        .split_once('/')
        .expect("full_name is owner/name by construction");
    let bare = store::bare_dir(repos_dir, owner, name);
    if !bare.exists() {
        return Ok(BTreeMap::new());
    }
    Ok(store::list_refs(&bare)?)
}
