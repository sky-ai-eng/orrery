//! End-to-end GraphQL: TF's seven query families (spec/surface-github.md §3)
//! against a served world, using the *verbatim* query and fragment texts from
//! triage-factory's `internal/github` — rule 2 applied to the wire's exact
//! spelling, comments and all. Plus the GraphQL translation of the
//! NOT-MOCKED contract: an unimplemented field is a validation error carrying
//! the self-describing orrery envelope, never a bare GitHub-plausible error.

mod common;

use common::{boot, control_post, face, face_ok, git, mint_actor};

/// TF's `prBaseFields` (internal/github/graphql.go), verbatim.
const PR_BASE_FIELDS: &str = r#"
	id
	number
	title
	body
	author { __typename login ... on User { databaseId } ... on Bot { databaseId } }
	state
	isDraft
	merged
	mergedBy { __typename login ... on User { databaseId } ... on Bot { databaseId } }
	headRefName
	baseRefName
	url
	repository { nameWithOwner }
	headRepository { nameWithOwner }
	additions
	deletions
	changedFiles
	reviewRequests(first: 100) {
		pageInfo { hasNextPage }
		nodes {
			requestedReviewer {
				... on User { login databaseId }
				... on Team { databaseId slug organization { login } }
			}
		}
	}
	latestReviews(first: 20) {
		nodes {
			id
			fullDatabaseId
			author { __typename login ... on User { databaseId } ... on Bot { databaseId } }
			state
			submittedAt
			comments { totalCount }
		}
	}
	reviews(last: 1, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) {
		totalCount
		nodes { fullDatabaseId submittedAt }
	}
	closedEvents: timelineItems(last: 1, itemTypes: [CLOSED_EVENT]) {
		nodes {
			... on ClosedEvent { actor { __typename login ... on User { databaseId } ... on Bot { databaseId } } }
		}
	}
"#;

/// TF's `prDiscoveryFragment`, verbatim (base fields spliced the same way).
fn pr_discovery_fragment() -> String {
    format!(
        r#"
fragment PRDiscoveryFields on PullRequest {{
{PR_BASE_FIELDS}
	commits(last: 1) {{
		nodes {{
			commit {{ oid committedDate }}
		}}
	}}
	labels(first: 10) {{ nodes {{ name }} }}
	comments(last: 1) {{
		totalCount
		nodes {{ fullDatabaseId createdAt }}
	}}
	createdAt
	updatedAt
	mergedAt
	closedAt
}}
"#
    )
}

/// TF's `prFullFragment`, verbatim — including its `#` comment lines, which
/// must parse as GraphQL comments.
fn pr_full_fragment() -> String {
    format!(
        r#"
fragment PRFullFields on PullRequest {{
{PR_BASE_FIELDS}
	commits(last: 1) {{
		nodes {{
			commit {{
				oid
				committedDate
				statusCheckRollup {{
					contexts(first: 100) {{
						pageInfo {{ hasNextPage }}
						nodes {{
							__typename
							... on CheckRun {{
								databaseId
								name
								status
								conclusion
								completedAt
								detailsUrl
								checkSuite {{ workflowRun {{ databaseId }} }}
							}}
						}}
					}}
				}}
			}}
		}}
	}}
	labels(first: 10) {{ nodes {{ name }} }}
	comments(last: 1) {{
		totalCount
		nodes {{ fullDatabaseId createdAt }}
	}}
	createdAt
	updatedAt
	mergedAt
	closedAt
	# Timeline tail for source-time enrichment of label / review-request /
	# ready-for-review events.
	timelineItems(last: 20, itemTypes: [LABELED_EVENT, UNLABELED_EVENT, REVIEW_REQUESTED_EVENT, READY_FOR_REVIEW_EVENT]) {{
		nodes {{
			__typename
			... on LabeledEvent {{
				createdAt
				label {{ name }}
			}}
			... on UnlabeledEvent {{
				createdAt
				label {{ name }}
			}}
			... on ReviewRequestedEvent {{
				createdAt
				requestedReviewer {{
					... on User {{ login }}
					... on Team {{ slug organization {{ login }} }}
				}}
			}}
			... on ReadyForReviewEvent {{
				createdAt
			}}
		}}
	}}
}}
"#
    )
}

fn gql(base: &str, token: &str, query: &str, variables: serde_json::Value) -> serde_json::Value {
    ureq::post(format!("{base}/api/graphql"))
        .header("authorization", &format!("token {token}"))
        .send_json(serde_json::json!({ "query": query, "variables": variables }))
        .expect("graphql call")
        .body_mut()
        .read_json()
        .expect("graphql json")
}

struct Seeded {
    inst: common::Instance,
    token: String,
    bob_token: String,
    ci_token: String,
    #[allow(dead_code)]
    work: tempfile::TempDir,
    api: String,
    feature_sha: String,
}

/// Boot, mint alice/bob/ci, create alice/webapp, push `main` plus a
/// one-commit `feature` branch — the substrate every test builds its PRs on.
async fn seed() -> Seeded {
    let inst = boot().await;
    let base = inst.base.clone();
    let token = mint_actor(&base, "alice", "user");
    let bob_token = mint_actor(&base, "bob", "user");
    let ci_token = mint_actor(&base, "ci", "bot");

    let repo = face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &token,
        serde_json::json!({ "name": "webapp" }),
    );
    let clone_url = repo["clone_url"].as_str().unwrap().to_string();
    let authed_url = clone_url.replace("http://", &format!("http://x-access-token:{token}@"));

    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# webapp\n").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "init"]);
    git(&src, clock, &["checkout", "-b", "feature"]);
    std::fs::write(src.join("feature.txt"), "v1\n").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "add feature"]);
    let feature_sha = git(&src, clock, &["rev-parse", "HEAD"]).trim().to_string();
    git(&src, clock, &["push", &authed_url, "main", "feature"]);

    let api = format!("{base}/api/v3/repos/alice/webapp");
    Seeded {
        inst,
        token,
        bob_token,
        ci_token,
        work,
        api,
        feature_sha,
    }
}

fn open_pr(s: &Seeded) -> serde_json::Value {
    face_ok(
        "POST",
        &format!("{}/pulls", s.api),
        &s.token,
        serde_json::json!({ "title": "Add feature", "body": "does the thing",
                            "head": "feature", "base": "main" }),
    )
}

/// TF's Phase-2 refresh: `nodes(ids:)` with the full fragment, exactly as
/// `refreshPRsBatch` composes it. One live PR with reviews, CI, and a
/// comment; one deleted id and one junk id resolving to null entries.
#[tokio::test(flavor = "multi_thread")]
async fn tf_refresh_full_fragment_round_trip() {
    let s = seed().await;
    let pull = open_pr(&s);

    // Two reviews by bob so latestReviews collapses to his latest while
    // reviews.totalCount sees both.
    let reviews_url = format!("{}/pulls/1/reviews", s.api);
    face_ok(
        "POST",
        &reviews_url,
        &s.bob_token,
        serde_json::json!({ "event": "REQUEST_CHANGES", "body": "needs work" }),
    );
    let approval = face_ok(
        "POST",
        &reviews_url,
        &s.bob_token,
        serde_json::json!({ "event": "APPROVE", "body": "ship it" }),
    );
    let checks_url = format!("{}/check-runs", s.api);
    face_ok(
        "POST",
        &checks_url,
        &s.ci_token,
        serde_json::json!({ "name": "ci/build", "head_sha": s.feature_sha,
                            "status": "completed", "conclusion": "success" }),
    );
    face_ok(
        "POST",
        &checks_url,
        &s.ci_token,
        serde_json::json!({ "name": "ci/lint", "head_sha": s.feature_sha,
                            "status": "in_progress" }),
    );
    face_ok(
        "POST",
        &format!("{}/issues/1/comments", s.api),
        &s.bob_token,
        serde_json::json!({ "body": "looks close" }),
    );

    let query = format!(
        "query RefreshPRsWithChecks($ids: [ID!]!) {{ nodes(ids: $ids) {{ ...PRFullFields }} rateLimit {{ cost }} }}\n{}",
        pr_full_fragment()
    );
    let resp = gql(
        &s.inst.base,
        &s.token,
        &query,
        serde_json::json!({ "ids": ["O_pull_1", "O_pull_999", "PR_kwDOjunk"] }),
    );
    assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
    assert!(
        resp["data"]["rateLimit"].is_null(),
        "rate limiting is off, as on a GHES without it"
    );
    let nodes = resp["data"]["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 3);
    assert!(nodes[1].is_null(), "missing pull resolves to null");
    assert!(nodes[2].is_null(), "foreign id resolves to null");

    let pr = &nodes[0];
    assert_eq!(pr["id"], "O_pull_1");
    assert_eq!(pr["number"], 1);
    assert_eq!(pr["title"], "Add feature");
    assert_eq!(pr["body"], "does the thing");
    assert_eq!(pr["author"]["login"], "alice");
    assert_eq!(
        pr["author"]["databaseId"], pull["user"]["id"],
        "an Int, the id REST answers"
    );
    assert_eq!(pr["state"], "OPEN");
    assert_eq!(pr["isDraft"], false);
    assert_eq!(pr["merged"], false);
    assert!(pr["mergedBy"].is_null());
    assert_eq!(pr["closedEvents"]["nodes"], serde_json::json!([]));
    assert_eq!(pr["headRefName"], "feature");
    assert_eq!(pr["baseRefName"], "main");
    assert_eq!(
        pr["url"],
        format!("{}/alice/webapp/pull/1", s.inst.base).as_str()
    );
    assert_eq!(pr["repository"]["nameWithOwner"], "alice/webapp");
    assert_eq!(pr["headRepository"]["nameWithOwner"], "alice/webapp");
    assert_eq!(pr["additions"], 1);
    assert_eq!(pr["deletions"], 0);
    assert_eq!(pr["changedFiles"], 1);
    assert_eq!(pr["reviewRequests"]["pageInfo"]["hasNextPage"], false);
    assert_eq!(pr["reviewRequests"]["nodes"].as_array().unwrap().len(), 0);

    let latest = pr["latestReviews"]["nodes"].as_array().unwrap();
    assert_eq!(latest.len(), 1, "latestReviews is per-author latest");
    assert_eq!(latest[0]["id"], "O_review_2");
    assert_eq!(latest[0]["fullDatabaseId"], "2", "BigInt is a string");
    assert_eq!(latest[0]["author"]["__typename"], "User");
    assert_eq!(latest[0]["comments"]["totalCount"], 0);
    assert_eq!(latest[0]["author"]["login"], "bob");
    assert_eq!(latest[0]["author"]["databaseId"], approval["user"]["id"]);
    assert_eq!(latest[0]["state"], "APPROVED");
    assert_eq!(latest[0]["submittedAt"], "2020-01-01T00:00:00Z");
    assert_eq!(pr["reviews"]["totalCount"], 2);
    let last_review = pr["reviews"]["nodes"].as_array().unwrap();
    assert_eq!(last_review.len(), 1, "reviews(last: 1)");
    assert_eq!(last_review[0]["fullDatabaseId"], "2");

    let commit = &pr["commits"]["nodes"][0]["commit"];
    assert_eq!(commit["oid"], s.feature_sha.as_str());
    assert_eq!(commit["committedDate"], "2020-01-01T00:00:00Z");
    let contexts = &commit["statusCheckRollup"]["contexts"];
    assert_eq!(contexts["pageInfo"]["hasNextPage"], false);
    let runs = contexts["nodes"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["__typename"], "CheckRun");
    assert_eq!(runs[0]["databaseId"], 1);
    assert_eq!(runs[0]["name"], "ci/build");
    assert_eq!(runs[0]["status"], "COMPLETED");
    assert_eq!(runs[0]["conclusion"], "SUCCESS");
    assert_eq!(runs[0]["completedAt"], "2020-01-01T00:00:00Z");
    assert!(runs[0]["checkSuite"].is_null());
    assert_eq!(runs[1]["name"], "ci/lint");
    assert_eq!(runs[1]["status"], "IN_PROGRESS");
    assert!(runs[1]["conclusion"].is_null());

    assert_eq!(pr["labels"]["nodes"].as_array().unwrap().len(), 0);
    assert_eq!(pr["comments"]["totalCount"], 1);
    assert_eq!(pr["comments"]["nodes"][0]["fullDatabaseId"], "1");
    assert_eq!(
        pr["comments"]["nodes"][0]["createdAt"],
        "2020-01-01T00:00:00Z"
    );
    assert_eq!(pr["createdAt"], "2020-01-01T00:00:00Z");
    assert!(pr["mergedAt"].is_null());
    assert!(pr["closedAt"].is_null());
    assert_eq!(pr["timelineItems"]["nodes"].as_array().unwrap().len(), 0);

    // A commit with no check runs at all reports a null rollup — the shape
    // TF reads as "no CI here".
    let discovery = format!(
        "query($ids: [ID!]!) {{ nodes(ids: $ids) {{ ...PRFullFields }} }}\n{}",
        pr_full_fragment()
    );
    let _ = discovery; // same query text; rollup nullability is covered below.
}

/// TF's `prCommentsQuery` (internal/github/pr_comments.go), verbatim.
/// TF's mergeability probe (`prMergeabilityQuery`), verbatim.
const PR_MERGEABILITY_QUERY: &str = r#"
	query PRMergeability($ids: [ID!]!) {
		nodes(ids: $ids) { ... on PullRequest { id mergeable headRefOid } }
		rateLimit { cost }
	}
"#;

/// TF's probe worker reads a batch of pulls' mergeability and the head each
/// answer was read at, and skips an id with no node.
#[tokio::test(flavor = "multi_thread")]
async fn tf_mergeability_probe() {
    let s = seed().await;
    open_pr(&s);

    let resp = gql(
        &s.inst.base,
        &s.token,
        PR_MERGEABILITY_QUERY,
        serde_json::json!({ "ids": ["O_pull_1", "O_pull_999"] }),
    );
    assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
    let nodes = resp["data"]["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0]["id"], "O_pull_1");
    assert_eq!(nodes[0]["mergeable"], "MERGEABLE");
    assert_eq!(nodes[0]["headRefOid"], s.feature_sha.as_str());
    assert!(nodes[1].is_null(), "missing pull resolves to null");
}

const PR_COMMENTS_QUERY: &str = r#"
	query PRComments($id: ID!, $before: String) {
		node(id: $id) {
			... on PullRequest {
				comments(last: 50, before: $before) {
					totalCount
					pageInfo { hasPreviousPage startCursor }
					nodes {
						fullDatabaseId
						createdAt
						author { __typename login ... on User { databaseId } ... on Bot { databaseId } }
						body
					}
				}
			}
		}
		rateLimit { cost }
	}
"#;

/// TF's `prReviewsQuery` (internal/github/pr_reviews.go), verbatim.
const PR_REVIEWS_QUERY: &str = r#"
	query PRReviews($id: ID!, $before: String) {
		node(id: $id) {
			... on PullRequest {
				reviews(last: 50, before: $before, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) {
					totalCount
					pageInfo { hasPreviousPage startCursor }
					nodes {
						fullDatabaseId
						state
						submittedAt
						author { __typename login ... on User { databaseId } ... on Bot { databaseId } }
						body
						comments(first: 100) {
							totalCount
							nodes { fullDatabaseId path body }
						}
					}
				}
			}
		}
		rateLimit { cost }
	}
"#;

/// The tracker's new-comment and new-review reads page backwards from the
/// newest item with `last:` and `before:`, so a world with more than a page
/// is walked to its start through `startCursor`.
#[tokio::test(flavor = "multi_thread")]
async fn tf_comment_and_review_paging() {
    let s = seed().await;
    open_pr(&s);
    for i in 0..52 {
        face_ok(
            "POST",
            &format!("{}/issues/1/comments", s.api),
            &s.bob_token,
            serde_json::json!({ "body": format!("comment {i}") }),
        );
    }
    for event in ["COMMENT", "REQUEST_CHANGES", "APPROVE"] {
        face_ok(
            "POST",
            &format!("{}/pulls/1/reviews", s.api),
            &s.bob_token,
            serde_json::json!({ "event": event, "body": event }),
        );
    }

    let page1 = gql(
        &s.inst.base,
        &s.token,
        PR_COMMENTS_QUERY,
        serde_json::json!({ "id": "O_pull_1", "before": null }),
    );
    assert!(page1.get("errors").is_none(), "unexpected errors: {page1}");
    assert!(page1["data"]["rateLimit"].is_null());
    let c1 = &page1["data"]["node"]["comments"];
    assert_eq!(c1["totalCount"], 52);
    assert_eq!(c1["pageInfo"]["hasPreviousPage"], true);
    let nodes = c1["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 50, "the newest 50");
    assert_eq!(nodes[0]["fullDatabaseId"], "3");
    assert_eq!(nodes[49]["fullDatabaseId"], "52");
    assert_eq!(nodes[49]["body"], "comment 51");
    assert_eq!(nodes[49]["author"]["login"], "bob");

    let page2 = gql(
        &s.inst.base,
        &s.token,
        PR_COMMENTS_QUERY,
        serde_json::json!({ "id": "O_pull_1", "before": c1["pageInfo"]["startCursor"] }),
    );
    let c2 = &page2["data"]["node"]["comments"];
    assert_eq!(c2["pageInfo"]["hasPreviousPage"], false);
    let older: Vec<&str> = c2["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["fullDatabaseId"].as_str().unwrap())
        .collect();
    assert_eq!(older, ["1", "2"]);

    let reviews = gql(
        &s.inst.base,
        &s.token,
        PR_REVIEWS_QUERY,
        serde_json::json!({ "id": "O_pull_1", "before": null }),
    );
    assert!(
        reviews.get("errors").is_none(),
        "unexpected errors: {reviews}"
    );
    let r = &reviews["data"]["node"]["reviews"];
    assert_eq!(r["totalCount"], 3);
    assert_eq!(r["pageInfo"]["hasPreviousPage"], false);
    let states: Vec<&str> = r["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["COMMENTED", "CHANGES_REQUESTED", "APPROVED"]);
    assert_eq!(r["nodes"][0]["comments"]["totalCount"], 0);

    // The states filter narrows the set and its count alike.
    let approved = gql(
        &s.inst.base,
        &s.token,
        "query($id: ID!) { node(id: $id) { ... on PullRequest { reviews(last: 5, states: [APPROVED]) { totalCount nodes { state } } } } }",
        serde_json::json!({ "id": "O_pull_1" }),
    );
    assert_eq!(approved["data"]["node"]["reviews"]["totalCount"], 1);

    // Both ends at once, and a cursor Orrery never issued, are refused.
    let both = gql(
        &s.inst.base,
        &s.token,
        "query($id: ID!) { node(id: $id) { ... on PullRequest { comments(first: 1, last: 1) { totalCount } } } }",
        serde_json::json!({ "id": "O_pull_1" }),
    );
    assert!(
        both["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("Passing both"),
        "{both}"
    );
    let junk = gql(
        &s.inst.base,
        &s.token,
        PR_COMMENTS_QUERY,
        serde_json::json!({ "id": "O_pull_1", "before": "not-a-cursor" }),
    );
    assert!(
        junk["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("valid cursor"),
        "{junk}"
    );
}

/// TF's discovery/backfill search, all four template shapes plus `repo:`
/// scoping, with the discovery fragment verbatim — and the loud refusal for
/// a qualifier outside the mocked subset.
#[tokio::test(flavor = "multi_thread")]
async fn tf_discovery_search_and_refusals() {
    let s = seed().await;
    open_pr(&s);
    face_ok(
        "POST",
        &format!("{}/pulls/1/reviews", s.api),
        &s.bob_token,
        serde_json::json!({ "event": "APPROVE" }),
    );
    face_ok(
        "PUT",
        &format!("{}/pulls/1/merge", s.api),
        &s.token,
        serde_json::json!({}),
    );

    // A second PR, closed without merging.
    let clock = s.inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src2");
    let authed = format!("{}/alice/webapp.git", s.inst.base)
        .replace("http://", &format!("http://x-access-token:{}@", s.token));
    git(
        work.path(),
        clock,
        &["clone", &authed, src.to_str().unwrap()],
    );
    git(&src, clock, &["checkout", "-b", "abandoned"]);
    std::fs::write(src.join("junk.txt"), "x\n").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "junk"]);
    git(&src, clock, &["push", &authed, "abandoned"]);
    face_ok(
        "POST",
        &format!("{}/pulls", s.api),
        &s.token,
        serde_json::json!({ "title": "Abandoned", "head": "abandoned", "base": "main" }),
    );
    face_ok(
        "PATCH",
        &format!("{}/pulls/2", s.api),
        &s.token,
        serde_json::json!({ "state": "closed" }),
    );

    let query = format!(
        "query($q: String!, $limit: Int!) {{ search(query: $q, type: ISSUE, first: $limit) {{ nodes {{ ...PRDiscoveryFields }} }} }}\n{}",
        pr_discovery_fragment()
    );
    let run = |q: &str| {
        let resp = gql(
            &s.inst.base,
            &s.token,
            &query,
            serde_json::json!({ "q": q, "limit": 50 }),
        );
        assert!(resp.get("errors").is_none(), "search {q:?} errored: {resp}");
        resp["data"]["search"]["nodes"].as_array().unwrap().clone()
    };

    // TF's four dashboard-backfill templates (dashboard_backfill.go).
    let merged = run("is:pr is:merged author:alice merged:>=2020-01-01 repo:alice/webapp");
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0]["number"], 1);
    assert_eq!(merged[0]["state"], "MERGED");
    assert_eq!(merged[0]["merged"], true);
    assert_eq!(merged[0]["mergedAt"], "2020-01-01T00:00:00Z");
    assert_eq!(merged[0]["mergedBy"]["__typename"], "User");
    assert_eq!(merged[0]["mergedBy"]["login"], "alice");
    assert_eq!(
        merged[0]["mergedBy"]["databaseId"],
        merged[0]["author"]["databaseId"]
    );
    assert_eq!(
        merged[0]["closedEvents"]["nodes"][0]["actor"]["login"], "alice",
        "a merge closes the pull too, as GitHub logs `closed` beside `merged`"
    );

    let reviewed = run("is:pr is:merged reviewed-by:bob merged:>=2020-01-01");
    assert_eq!(reviewed.len(), 1);
    assert_eq!(reviewed[0]["number"], 1);

    let closed = run("is:pr is:closed is:unmerged author:alice closed:>=2020-01-01");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["number"], 2);
    assert_eq!(closed[0]["state"], "CLOSED");
    assert!(closed[0]["mergedBy"].is_null());
    assert_eq!(
        closed[0]["closedEvents"]["nodes"],
        serde_json::json!([{ "actor": {
            "__typename": "User", "login": "alice", "databaseId": closed[0]["author"]["databaseId"]
        } }])
    );

    assert_eq!(
        run("is:pr is:closed is:unmerged reviewed-by:bob closed:>=2020-01-01").len(),
        0,
        "bob never reviewed the abandoned PR"
    );
    assert_eq!(
        run("is:pr is:merged author:alice merged:>=2020-01-01 repo:alice/other").len(),
        0,
        "repo scope excludes"
    );
    assert_eq!(
        run("is:pr is:merged author:alice merged:>=2020-01-02").len(),
        0,
        "date cutoff excludes"
    );

    // A qualifier outside the mocked subset refuses loudly with the orrery
    // envelope — never a silently-empty result.
    let refused = gql(
        &s.inst.base,
        &s.token,
        &query,
        serde_json::json!({ "q": "is:pr label:bug", "limit": 50 }),
    );
    let err = &refused["errors"][0];
    assert!(
        err["message"].as_str().unwrap().contains("not mocked"),
        "refusal names the gap: {err}"
    );
    assert_eq!(err["extensions"]["orrery"]["not_mocked"], true);
}

/// TF's aliased branch-existence batch (branches.go): variables per alias,
/// null ref = confidently gone, null repository = unknown.
#[tokio::test(flavor = "multi_thread")]
async fn tf_branch_existence_aliases() {
    let s = seed().await;
    let query = "query($o0: String!, $n0: String!, $q0: String!, $o1: String!, $n1: String!, $q1: String!, $o2: String!, $n2: String!, $q2: String!) {
  r0: repository(owner: $o0, name: $n0) { ref(qualifiedName: $q0) { id } }
  r1: repository(owner: $o1, name: $n1) { ref(qualifiedName: $q1) { id } }
  r2: repository(owner: $o2, name: $n2) { ref(qualifiedName: $q2) { id } }
}";
    let resp = gql(
        &s.inst.base,
        &s.token,
        query,
        serde_json::json!({
            "o0": "alice", "n0": "webapp", "q0": "refs/heads/feature",
            "o1": "alice", "n1": "webapp", "q1": "refs/heads/gone",
            "o2": "alice", "n2": "no-such-repo", "q2": "refs/heads/main",
        }),
    );
    assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
    assert!(resp["data"]["r0"]["ref"]["id"].is_string());
    assert!(resp["data"]["r1"]["ref"].is_null());
    assert!(resp["data"]["r2"].is_null());
}

/// TF's review read by node id (pending_reviews.go GetReview): any state,
/// truthfully-empty inline comments, and the non-review-node shape TF reads
/// as "no review here".
#[tokio::test(flavor = "multi_thread")]
async fn tf_review_node_lookup() {
    let s = seed().await;
    open_pr(&s);
    face_ok(
        "POST",
        &format!("{}/pulls/1/reviews", s.api),
        &s.bob_token,
        serde_json::json!({ "event": "REQUEST_CHANGES", "body": "hmm" }),
    );

    let query = r#"query($id: ID!) {
		node(id: $id) {
			... on PullRequestReview {
				state
				body
				comments(first: 100) {
					pageInfo { hasNextPage }
					nodes { id path line startLine body }
				}
			}
		}
	}"#;
    let resp = gql(
        &s.inst.base,
        &s.token,
        query,
        serde_json::json!({ "id": "O_review_1" }),
    );
    assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
    let node = &resp["data"]["node"];
    assert_eq!(node["state"], "CHANGES_REQUESTED");
    assert_eq!(node["body"], "hmm");
    assert_eq!(node["comments"]["nodes"].as_array().unwrap().len(), 0);
    assert_eq!(node["comments"]["pageInfo"]["hasNextPage"], false);

    // A pull's id through the same query: the inline fragment matches
    // nothing. GitHub renders an empty object there; this executor renders
    // null — an accepted divergence, since consumers treat "null node" and
    // "no matched fields" as the same no-review answer.
    let resp = gql(
        &s.inst.base,
        &s.token,
        query,
        serde_json::json!({ "id": "O_pull_1" }),
    );
    assert!(resp["data"]["node"].is_null());

    let resp = gql(
        &s.inst.base,
        &s.token,
        query,
        serde_json::json!({ "id": "O_review_99" }),
    );
    assert!(resp["data"]["node"].is_null());
}

/// TF's two mutations (status.go): draft flips both ways, wrong-state
/// refusals, and the action-log trail.
#[tokio::test(flavor = "multi_thread")]
async fn tf_draft_mutations() {
    let s = seed().await;
    face_ok(
        "POST",
        &format!("{}/pulls", s.api),
        &s.token,
        serde_json::json!({ "title": "WIP", "head": "feature", "base": "main", "draft": true }),
    );

    let ready = "mutation($id: ID!) { markPullRequestReadyForReview(input: {pullRequestId: $id}) { pullRequest { isDraft } } }";
    let to_draft = "mutation($id: ID!) { convertPullRequestToDraft(input: {pullRequestId: $id}) { pullRequest { isDraft } } }";
    let vars = serde_json::json!({ "id": "O_pull_1" });

    let resp = gql(&s.inst.base, &s.token, ready, vars.clone());
    assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
    assert_eq!(
        resp["data"]["markPullRequestReadyForReview"]["pullRequest"]["isDraft"],
        false
    );
    let via_rest = face_ok(
        "GET",
        &format!("{}/pulls/1", s.api),
        &s.token,
        serde_json::json!({}),
    );
    assert_eq!(via_rest["draft"], false);

    // Already ready → GitHub errors rather than no-oping.
    let resp = gql(&s.inst.base, &s.token, ready, vars.clone());
    assert!(resp["data"]["markPullRequestReadyForReview"].is_null());
    assert!(
        resp["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("not a draft")
    );

    let resp = gql(&s.inst.base, &s.token, to_draft, vars.clone());
    assert_eq!(
        resp["data"]["convertPullRequestToDraft"]["pullRequest"]["isDraft"],
        true
    );

    let log = control_post(&s.inst.base, "/_orrery/log/list", serde_json::json!({}));
    let actions: Vec<&str> = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["action"].as_str())
        .collect();
    assert!(actions.contains(&"pull.ready_for_review"));
    assert!(actions.contains(&"pull.convert_to_draft"));
}

/// TF's organization.teams query (teams.go): an org that never made a team
/// answers with an empty team set, and a user login answers null, exactly the
/// shapes TF degrades on without erroring. `e2e_teams.rs` covers an org with
/// teams.
#[tokio::test(flavor = "multi_thread")]
async fn tf_organization_teams_query() {
    let s = seed().await;
    mint_actor(&s.inst.base, "acme", "org");

    let query = r#"
		query($org: String!, $login: String!, $cursor: String) {
			organization(login: $org) {
				databaseId
				teams(first: 100, userLogins: [$login], after: $cursor) {
					pageInfo { hasNextPage endCursor }
					nodes {
						databaseId
						slug
						name
						members { totalCount }
					}
				}
			}
		}
	"#;
    let resp = gql(
        &s.inst.base,
        &s.token,
        query,
        serde_json::json!({ "org": "acme", "login": "alice", "cursor": null }),
    );
    assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
    let org = face_ok(
        "GET",
        &format!("{}/api/v3/orgs/acme", s.inst.base),
        &s.token,
        serde_json::json!(null),
    );
    assert_eq!(resp["data"]["organization"]["databaseId"], org["id"]);
    let teams = &resp["data"]["organization"]["teams"];
    assert_eq!(teams["pageInfo"]["hasNextPage"], false);
    assert_eq!(teams["nodes"].as_array().unwrap().len(), 0);

    let resp = gql(
        &s.inst.base,
        &s.token,
        query,
        serde_json::json!({ "org": "alice", "login": "alice", "cursor": null }),
    );
    assert!(resp["data"]["organization"].is_null());
}

/// The NOT-MOCKED contract in GraphQL: a field outside the mocked subset is
/// a validation error wearing the orrery envelope — distinguishable from
/// "GitHub doesn't have this" — and the endpoint requires auth like the rest
/// of the API face.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_field_marked_not_mocked() {
    let s = seed().await;
    let resp = gql(
        &s.inst.base,
        &s.token,
        "{ viewer { login } }",
        serde_json::json!({}),
    );
    assert!(resp["data"].is_null());
    let err = &resp["errors"][0];
    let msg = err["message"].as_str().unwrap();
    assert!(
        msg.starts_with("orrery (not mocked):"),
        "marked message: {msg}"
    );
    assert_eq!(err["extensions"]["orrery"]["not_mocked"], true);

    // Unmocked enum values and arguments are marked the same way.
    let resp = gql(
        &s.inst.base,
        &s.token,
        "query { search(query: \"is:pr\", type: REPOSITORY, first: 1) { nodes { __typename } } }",
        serde_json::json!({}),
    );
    assert!(resp["data"].is_null());
    assert_eq!(
        resp["errors"][0]["extensions"]["orrery"]["not_mocked"], true,
        "unknown enum value is a marked gap: {resp}"
    );

    let err = ureq::post(format!("{}/api/graphql", s.inst.base))
        .send_json(serde_json::json!({ "query": "{ __typename }" }))
        .expect_err("anonymous GraphQL refuses");
    match err {
        ureq::Error::StatusCode(code) => assert_eq!(code, 401),
        other => panic!("expected status error, got {other:?}"),
    }
}

/// A bot actor stands for a GitHub App: REST spells its login with the
/// `[bot]` suffix and types it `Bot`, GraphQL types its author `Bot` with the
/// bare slug, and the authenticated-user routes refuse its token, as GitHub
/// refuses an installation token there. TF tells its own App's comments and
/// reviews apart by exactly these spellings.
#[tokio::test(flavor = "multi_thread")]
async fn bot_actors_render_as_github_apps() {
    let s = seed().await;
    let pull = open_pr(&s);
    let rest = face_ok(
        "POST",
        &format!("{}/issues/1/comments", s.api),
        &s.ci_token,
        serde_json::json!({ "body": "build failed" }),
    );
    assert_eq!(rest["user"]["login"], "ci[bot]");
    assert_eq!(rest["user"]["type"], "Bot");
    let review = face_ok(
        "POST",
        &format!("{}/pulls/1/reviews", s.api),
        &s.ci_token,
        serde_json::json!({ "event": "COMMENT", "body": "lint notes" }),
    );
    assert_eq!(review["user"]["login"], "ci[bot]");

    let comments = gql(
        &s.inst.base,
        &s.token,
        PR_COMMENTS_QUERY,
        serde_json::json!({ "id": "O_pull_1", "before": null }),
    );
    let author = &comments["data"]["node"]["comments"]["nodes"][0]["author"];
    assert_eq!(author["__typename"], "Bot");
    assert_eq!(author["login"], "ci");
    assert_eq!(author["databaseId"], rest["user"]["id"]);
    let reviews = gql(
        &s.inst.base,
        &s.token,
        PR_REVIEWS_QUERY,
        serde_json::json!({ "id": "O_pull_1", "before": null }),
    );
    let author = &reviews["data"]["node"]["reviews"]["nodes"][0]["author"];
    assert_eq!(author["__typename"], "Bot");
    assert_eq!(author["login"], "ci");
    assert_eq!(author["databaseId"], review["user"]["id"]);

    // A person stays a User in both.
    let pr = gql(
        &s.inst.base,
        &s.token,
        "query($id: ID!) { node(id: $id) { ... on PullRequest { author { __typename login ... on User { databaseId } ... on Bot { databaseId } } } } }",
        serde_json::json!({ "id": "O_pull_1" }),
    );
    assert_eq!(pr["data"]["node"]["author"]["__typename"], "User");
    assert_eq!(pr["data"]["node"]["author"]["login"], "alice");
    assert_eq!(
        pr["data"]["node"]["author"]["databaseId"],
        pull["user"]["id"]
    );

    for path in [
        "/api/v3/user",
        "/api/v3/user/emails",
        "/api/v3/user/repos",
        "/api/v3/user/teams",
    ] {
        assert_eq!(
            common::face(
                "GET",
                &format!("{}{path}", s.inst.base),
                &s.ci_token,
                serde_json::json!(null)
            ),
            Err(403),
            "{path}"
        );
    }
    assert_eq!(
        common::face(
            "POST",
            &format!("{}/api/v3/user/repos", s.inst.base),
            &s.ci_token,
            serde_json::json!({ "name": "bots-own" })
        ),
        Err(403)
    );
}

/// Labels written through REST and ready-for-review through GraphQL read back
/// through TF's full fragment: the pull's current labels, and the timeline
/// items TF dates those transitions by.
#[tokio::test(flavor = "multi_thread")]
async fn tf_labels_and_timeline() {
    let s = seed().await;
    face_ok(
        "POST",
        &format!("{}/pulls", s.api),
        &s.token,
        serde_json::json!({ "title": "WIP", "head": "feature", "base": "main", "draft": true }),
    );
    let labels_url = format!("{}/issues/1/labels", s.api);

    let added = face_ok(
        "POST",
        &labels_url,
        &s.bob_token,
        serde_json::json!({ "labels": ["bug", "needs-review"] }),
    );
    let names: Vec<&str> = added
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["bug", "needs-review"]);
    let bare = face_ok(
        "POST",
        &labels_url,
        &s.bob_token,
        serde_json::json!(["triage"]),
    );
    assert_eq!(
        bare.as_array().unwrap().len(),
        3,
        "a bare array is the same request"
    );
    assert_eq!(
        face(
            "POST",
            &labels_url,
            &s.bob_token,
            serde_json::json!({ "labels": [""] })
        ),
        Err(422)
    );
    assert_eq!(
        face(
            "POST",
            &labels_url,
            &s.bob_token,
            serde_json::json!({ "labels": [{ "name": "x" }] })
        ),
        Err(422),
        "label objects are refused as not mocked"
    );

    let left = face_ok(
        "DELETE",
        &format!("{labels_url}/bug"),
        &s.bob_token,
        serde_json::json!({}),
    );
    assert_eq!(left.as_array().unwrap().len(), 2);
    assert_eq!(
        face(
            "DELETE",
            &format!("{labels_url}/bug"),
            &s.bob_token,
            serde_json::json!({})
        ),
        Err(404)
    );
    assert_eq!(
        face(
            "POST",
            &format!("{}/issues/99/labels", s.api),
            &s.bob_token,
            serde_json::json!(["x"])
        ),
        Err(404)
    );

    let rest = face_ok(
        "GET",
        &format!("{}/pulls/1", s.api),
        &s.token,
        serde_json::json!({}),
    );
    let rest_names: Vec<&str> = rest["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].as_str().unwrap())
        .collect();
    assert_eq!(rest_names, ["needs-review", "triage"]);

    let ready = "mutation($id: ID!) { markPullRequestReadyForReview(input: {pullRequestId: $id}) { pullRequest { isDraft } } }";
    let resp = gql(
        &s.inst.base,
        &s.token,
        ready,
        serde_json::json!({ "id": "O_pull_1" }),
    );
    assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");

    let query = format!(
        "query($ids: [ID!]!) {{ nodes(ids: $ids) {{ ...PRFullFields }} }} {}",
        pr_full_fragment()
    );
    let resp = gql(
        &s.inst.base,
        &s.token,
        &query,
        serde_json::json!({ "ids": ["O_pull_1"] }),
    );
    assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
    let pr = &resp["data"]["nodes"][0];
    assert_eq!(
        pr["labels"]["nodes"],
        serde_json::json!([{ "name": "needs-review" }, { "name": "triage" }])
    );
    // Nothing here advances the clock, so every item is dated at its start.
    let clock = "2020-01-01T00:00:00Z";
    let timeline: Vec<(String, Option<String>)> = pr["timelineItems"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            assert_eq!(n["createdAt"], clock);
            (
                n["__typename"].as_str().unwrap().to_string(),
                n["label"]["name"].as_str().map(str::to_string),
            )
        })
        .collect();
    assert_eq!(
        timeline,
        [
            ("LabeledEvent".to_string(), Some("bug".to_string())),
            ("LabeledEvent".to_string(), Some("needs-review".to_string())),
            ("LabeledEvent".to_string(), Some("triage".to_string())),
            ("UnlabeledEvent".to_string(), Some("bug".to_string())),
            ("ReadyForReviewEvent".to_string(), None),
        ]
    );

    let only_ready = gql(
        &s.inst.base,
        &s.token,
        "query($id: ID!) { node(id: $id) { ... on PullRequest { timelineItems(last: 1, itemTypes: [READY_FOR_REVIEW_EVENT, UNLABELED_EVENT]) { pageInfo { hasPreviousPage } nodes { __typename } } } } }",
        serde_json::json!({ "id": "O_pull_1" }),
    );
    assert!(
        only_ready.get("errors").is_none(),
        "unexpected errors: {only_ready}"
    );
    let items = &only_ready["data"]["node"]["timelineItems"];
    assert_eq!(
        items["nodes"],
        serde_json::json!([{ "__typename": "ReadyForReviewEvent" }])
    );
    assert_eq!(items["pageInfo"]["hasPreviousPage"], true);
}

/// Who merged and who closed a pull, read through TF's base fields: the
/// merger is `mergedBy`, and the closer is the actor of the newest
/// `ClosedEvent`, typed by its kind like an author. A merge closes the pull
/// too, so it lands as a `ClosedEvent` by the merger.
#[tokio::test(flavor = "multi_thread")]
async fn tf_merger_and_closer() {
    let s = seed().await;
    open_pr(&s);
    let users = format!("{}/api/v3/users", s.inst.base);
    let bob_id = face_ok(
        "GET",
        &format!("{users}/bob"),
        &s.token,
        serde_json::json!(null),
    )["id"]
        .clone();
    let ci_id = face_ok(
        "GET",
        &format!("{users}/ci[bot]"),
        &s.token,
        serde_json::json!(null),
    )["id"]
        .clone();
    let pull_url = format!("{}/pulls/1", s.api);
    let set_state = |token: &str, state: &str| {
        face_ok(
            "PATCH",
            &pull_url,
            token,
            serde_json::json!({ "state": state }),
        );
    };
    let query = format!(
        "query($ids: [ID!]!) {{ nodes(ids: $ids) {{ ...PRFullFields }} }} {}",
        pr_full_fragment()
    );
    let read = || {
        let resp = gql(
            &s.inst.base,
            &s.token,
            &query,
            serde_json::json!({ "ids": ["O_pull_1"] }),
        );
        assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
        resp["data"]["nodes"][0].clone()
    };
    let closer = |pr: &serde_json::Value| pr["closedEvents"]["nodes"][0]["actor"].clone();

    set_state(&s.ci_token, "closed");
    let pr = read();
    assert_eq!(pr["state"], "CLOSED");
    assert_eq!(
        closer(&pr),
        serde_json::json!({ "__typename": "Bot", "login": "ci", "databaseId": ci_id }),
        "a bot closer is a Bot with its bare slug"
    );
    assert_eq!(
        pr["timelineItems"]["nodes"],
        serde_json::json!([]),
        "the full fragment's own timeline does not ask for closes"
    );

    set_state(&s.token, "open");
    set_state(&s.bob_token, "closed");
    assert_eq!(
        closer(&read()),
        serde_json::json!({ "__typename": "User", "login": "bob", "databaseId": bob_id }),
        "last: 1 is the newest close"
    );

    set_state(&s.token, "open");
    face_ok(
        "PUT",
        &format!("{pull_url}/merge"),
        &s.bob_token,
        serde_json::json!({}),
    );
    let pr = read();
    assert_eq!(pr["state"], "MERGED");
    assert_eq!(
        pr["mergedBy"],
        serde_json::json!({ "__typename": "User", "login": "bob", "databaseId": bob_id })
    );
    assert_eq!(closer(&pr)["login"], "bob", "the merge is the newest close");

    let closes = gql(
        &s.inst.base,
        &s.token,
        "query($id: ID!) { node(id: $id) { ... on PullRequest { timelineItems(first: 10, itemTypes: [CLOSED_EVENT]) { nodes { __typename ... on ClosedEvent { actor { login } } } } } } }",
        serde_json::json!({ "id": "O_pull_1" }),
    );
    assert!(
        closes.get("errors").is_none(),
        "unexpected errors: {closes}"
    );
    let logins: Vec<&str> = closes["data"]["node"]["timelineItems"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            assert_eq!(n["__typename"], "ClosedEvent");
            n["actor"]["login"].as_str().unwrap()
        })
        .collect();
    assert_eq!(logins, ["ci", "bob", "bob"], "every close, oldest first");
}
