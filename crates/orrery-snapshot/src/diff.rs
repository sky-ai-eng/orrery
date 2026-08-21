//! Semantic diff between two world states — the engine's verdict (rule 11):
//! "what happened between these two moments", spoken in world vocabulary
//! (repos, refs, pulls, reviews, checks), derived entirely from the two
//! states themselves. Judges may interpret above this; nothing overrides it.
//!
//! Rows correlate by primary key, which is sound because the two sides share
//! a lineage (a snapshot and the world it came from, or two snapshots of one
//! world) — ids never renumber within a lineage. The schema is fixed: every
//! section always present, empty when nothing happened, so an assertion
//! script never branches on shape.

use std::collections::BTreeMap;
use std::path::Path;

use orrery_face_git::store;
use orrery_world::{Pull, PullState, WorldView};

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
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LogSpan {
    pub from_seq: i64,
    pub to_seq: i64,
    /// `to - from`; the entries themselves are served by the log list read,
    /// keyed from `from_seq`, not duplicated here.
    pub appended: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WorldDiff {
    pub from: SideMeta,
    pub to: SideMeta,
    pub actors_added: Vec<ActorAdded>,
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
    pub reviews_added: Vec<ReviewAdded>,
    pub comments_added: Vec<CommentAdded>,
    pub check_runs_added: Vec<CheckRunChange>,
    pub check_runs_updated: Vec<CheckRunChange>,
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
    let actors_added = to
        .view
        .list_actors()?
        .into_iter()
        .filter(|a| !from_actors.contains_key(&a.id))
        .map(|a| ActorAdded {
            login: a.login,
            kind: a.kind.as_str().to_string(),
        })
        .collect();

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
    let repo_ids: std::collections::BTreeSet<i64> =
        from_repos.keys().chain(to_repos.keys()).copied().collect();
    for id in repo_ids {
        let repo = to_repos.get(&id).or_else(|| from_repos.get(&id));
        let Some(repo) = repo else { continue };
        let full = repo.full_name();
        let from_refs = side_refs(from.repos_dir, &full, from_repos.contains_key(&id))?;
        let to_refs = side_refs(to.repos_dir, &full, to_repos.contains_key(&id))?;
        let names: std::collections::BTreeSet<&String> =
            from_refs.keys().chain(to_refs.keys()).collect();
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
    for p in to.view.list_pulls()? {
        let repo = repo_name(p.repo_id);
        let pull_ref = || PullRef {
            repo: repo.clone(),
            number: p.number,
            title: p.title.clone(),
            author: p.author_login.clone(),
        };
        let old = from_pulls.get(&p.id);
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

    let from_reviews: std::collections::BTreeSet<i64> = from
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

    let from_comments: std::collections::BTreeSet<i64> = from
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
        reviews_added,
        comments_added,
        check_runs_added,
        check_runs_updated,
        log: LogSpan {
            from_seq: from_meta.last_seq,
            to_seq: to_meta.last_seq,
            appended: to_meta.last_seq - from_meta.last_seq,
        },
    })
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
