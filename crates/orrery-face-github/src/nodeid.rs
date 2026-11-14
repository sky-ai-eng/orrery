//! Node ids, shared by the REST face's `node_id` fields and the GraphQL
//! face's `id` fields / `nodes(ids:)` lookups. One scheme across both faces
//! is load-bearing: a consumer stores an id from a REST listing and feeds it
//! back to GraphQL, so the two must agree byte-for-byte. The ids are opaque
//! to consumers but self-describing to a human reading a log.

pub fn actor(id: i64) -> String {
    format!("O_actor_{id}")
}

pub fn repo(id: i64) -> String {
    format!("O_repo_{id}")
}

pub fn pull(id: i64) -> String {
    format!("O_pull_{id}")
}

pub fn review(id: i64) -> String {
    format!("O_review_{id}")
}

pub fn comment(id: i64) -> String {
    format!("O_comment_{id}")
}

pub fn check(id: i64) -> String {
    format!("O_check_{id}")
}

pub fn team(id: i64) -> String {
    format!("O_team_{id}")
}

pub fn app(id: i64) -> String {
    format!("O_app_{id}")
}

/// A parsed node id — only the kinds the GraphQL face can resolve appear
/// here; every other string (foreign ids, junk) parses to `None`, which the
/// query layer renders as a null node, GitHub's own answer for an id that
/// doesn't resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRef {
    Pull(i64),
    Review(i64),
}

pub fn parse(s: &str) -> Option<NodeRef> {
    if let Some(rest) = s.strip_prefix("O_pull_") {
        return rest.parse().ok().map(NodeRef::Pull);
    }
    if let Some(rest) = s.strip_prefix("O_review_") {
        return rest.parse().ok().map(NodeRef::Review);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        assert_eq!(parse(&pull(7)), Some(NodeRef::Pull(7)));
        assert_eq!(parse(&review(3)), Some(NodeRef::Review(3)));
        assert_eq!(parse(&repo(1)), None);
        assert_eq!(parse("PR_kwDOexample"), None);
        assert_eq!(parse("O_pull_x"), None);
    }
}
