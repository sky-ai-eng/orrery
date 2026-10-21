//! The GitHub issue-search query language, restricted to the qualifiers
//! consumers actually send (spec/surface-github.md §3: TF's four backfill
//! templates plus `repo:` scoping). A qualifier outside that set is refused
//! loudly by the caller — evaluating a half-understood query would silently
//! return wrong results, the one failure mode rule 1 forbids.

use orrery_world::{Pull, PullState, Repo, WorldView};

use crate::GithubFace;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedSearch {
    /// `is:issue` was requested; the world has no issues, so nothing matches.
    pub matches_nothing: bool,
    /// `is:open` / `is:closed`.
    pub open: Option<bool>,
    /// `is:merged` / `is:unmerged`.
    pub merged: Option<bool>,
    pub author: Option<String>,
    pub reviewed_by: Option<String>,
    /// `repo:owner/name` scopes, OR'd; empty means all repos.
    pub repos: Vec<String>,
    /// `merged:>=DATE` as a unix cutoff (inclusive).
    pub merged_after: Option<i64>,
    /// `closed:>=DATE` as a unix cutoff (inclusive).
    pub closed_after: Option<i64>,
}

/// Parse a search string; `Err` carries the first term outside the mocked
/// subset, verbatim, for the caller's refusal message.
pub fn parse(query: &str) -> Result<ParsedSearch, String> {
    let mut out = ParsedSearch::default();
    for term in query.split_whitespace() {
        match term {
            "is:pr" => {}
            "is:issue" => out.matches_nothing = true,
            "is:open" => out.open = Some(true),
            "is:closed" => out.open = Some(false),
            "is:merged" => out.merged = Some(true),
            "is:unmerged" => out.merged = Some(false),
            _ => {
                if let Some(v) = term.strip_prefix("author:") {
                    out.author = Some(v.to_ascii_lowercase());
                } else if let Some(v) = term.strip_prefix("reviewed-by:") {
                    out.reviewed_by = Some(v.to_ascii_lowercase());
                } else if let Some(v) = term.strip_prefix("repo:") {
                    out.repos.push(v.to_ascii_lowercase());
                } else if let Some(v) = term.strip_prefix("merged:") {
                    out.merged_after = Some(parse_cutoff(v).ok_or_else(|| term.to_string())?);
                } else if let Some(v) = term.strip_prefix("closed:") {
                    out.closed_after = Some(parse_cutoff(v).ok_or_else(|| term.to_string())?);
                } else {
                    return Err(term.to_string());
                }
            }
        }
    }
    Ok(out)
}

/// Only the `>=` comparator is mocked — the one consumers send. A date is
/// midnight UTC; a full timestamp is taken as-is.
fn parse_cutoff(value: &str) -> Option<i64> {
    let value = value.strip_prefix(">=")?;
    if let Ok(ts) = value.parse::<jiff::Timestamp>() {
        return Some(ts.as_second());
    }
    let date = value.parse::<jiff::civil::Date>().ok()?;
    let zoned = date.to_zoned(jiff::tz::TimeZone::UTC).ok()?;
    Some(zoned.timestamp().as_second())
}

/// Evaluate against the live world: every pull, filtered, in the listing
/// order the pull endpoints use (created descending, id tiebreaker).
pub fn eval(
    face: &GithubFace,
    q: &ParsedSearch,
) -> Result<Vec<(Pull, Repo)>, orrery_world::WorldError> {
    if q.matches_nothing {
        return Ok(Vec::new());
    }
    let repos: std::collections::BTreeMap<i64, Repo> = face
        .world
        .list_repos()?
        .into_iter()
        .map(|r| (r.id, r))
        .collect();
    let reviewers: std::collections::BTreeMap<i64, Vec<String>> = if q.reviewed_by.is_some() {
        let mut map: std::collections::BTreeMap<i64, Vec<String>> = Default::default();
        for review in face.world.list_reviews()? {
            map.entry(review.pull_id)
                .or_default()
                .push(review.author_login.to_ascii_lowercase());
        }
        map
    } else {
        Default::default()
    };

    let mut matches: Vec<(Pull, Repo)> = Vec::new();
    for pull in face.world.list_pulls()? {
        let Some(repo) = repos.get(&pull.repo_id) else {
            continue;
        };
        if !q.repos.is_empty() && !q.repos.contains(&repo.full_name().to_ascii_lowercase()) {
            continue;
        }
        if let Some(open) = q.open
            && (pull.state == PullState::Open) != open
        {
            continue;
        }
        if let Some(merged) = q.merged
            && pull.merged != merged
        {
            continue;
        }
        if let Some(author) = &q.author
            && pull.author_login.to_ascii_lowercase() != *author
        {
            continue;
        }
        if let Some(reviewer) = &q.reviewed_by
            && !reviewers
                .get(&pull.id)
                .is_some_and(|authors| authors.contains(reviewer))
        {
            continue;
        }
        // Merge and close share one instant, so both cutoffs read the close
        // clock; a cutoff never matches a pull that hasn't reached that state.
        if let Some(cut) = q.merged_after
            && !(pull.merged && pull.closed_clock_unix.is_some_and(|t| t >= cut))
        {
            continue;
        }
        if let Some(cut) = q.closed_after
            && pull.closed_clock_unix.is_none_or(|t| t < cut)
        {
            continue;
        }
        matches.push((pull, repo.clone()));
    }
    matches.sort_by_key(|(p, _)| std::cmp::Reverse((p.created_clock_unix, p.id)));
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tf_backfill_templates() {
        let q = parse("is:pr is:merged author:alice merged:>=2026-01-01 repo:alice/demo").unwrap();
        assert_eq!(q.merged, Some(true));
        assert_eq!(q.author.as_deref(), Some("alice"));
        assert_eq!(q.repos, vec!["alice/demo"]);
        assert_eq!(q.merged_after, Some(1767225600));

        let q = parse("is:pr is:closed is:unmerged reviewed-by:Bob closed:>=2026-01-01").unwrap();
        assert_eq!(q.open, Some(false));
        assert_eq!(q.merged, Some(false));
        assert_eq!(q.reviewed_by.as_deref(), Some("bob"));
    }

    #[test]
    fn refuses_unmocked_qualifiers() {
        assert_eq!(parse("is:pr label:bug").unwrap_err(), "label:bug");
        assert_eq!(
            parse("is:pr merged:>2026-01-01").unwrap_err(),
            "merged:>2026-01-01"
        );
        assert_eq!(parse("freetext").unwrap_err(), "freetext");
    }
}
