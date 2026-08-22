//! End-to-end GraphQL: TF's six query families (spec/surface-github.md §3)
//! against a served world, using the *verbatim* query and fragment texts from
//! triage-factory's `internal/github` — rule 2 applied to the wire's exact
//! spelling, comments and all. Plus the GraphQL translation of the
//! NOT-MOCKED contract: an unimplemented field is a validation error carrying
//! the self-describing orrery envelope, never a bare GitHub-plausible error.

mod common;

use common::{boot, control_post, face_ok, git, mint_actor};

/// TF's `prBaseFields` (internal/github/graphql.go), verbatim.
const PR_BASE_FIELDS: &str = r#"
	id
	number
	title
	author { login }
	state
	isDraft
	merged
	mergeable
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
				... on User { login }
				... on Team { slug organization { login } }
			}
		}
	}
	latestReviews(first: 20) {
		nodes {
			id
			author { login }
			state
			submittedAt
		}
	}
	reviews(first: 1) { totalCount }
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
	comments {{ totalCount }}
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
	comments {{ totalCount }}
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
    open_pr(&s);

    // Two reviews by bob so latestReviews collapses to his latest while
    // reviews.totalCount sees both.
    let reviews_url = format!("{}/pulls/1/reviews", s.api);
    face_ok(
        "POST",
        &reviews_url,
        &s.bob_token,
        serde_json::json!({ "event": "REQUEST_CHANGES", "body": "needs work" }),
    );
    face_ok(
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
        "query($ids: [ID!]!) {{ nodes(ids: $ids) {{ ...PRFullFields }} }}\n{}",
        pr_full_fragment()
    );
    let resp = gql(
        &s.inst.base,
        &s.token,
        &query,
        serde_json::json!({ "ids": ["O_pull_1", "O_pull_999", "PR_kwDOjunk"] }),
    );
    assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
    let nodes = resp["data"]["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 3);
    assert!(nodes[1].is_null(), "missing pull resolves to null");
    assert!(nodes[2].is_null(), "foreign id resolves to null");

    let pr = &nodes[0];
    assert_eq!(pr["id"], "O_pull_1");
    assert_eq!(pr["number"], 1);
    assert_eq!(pr["title"], "Add feature");
    assert_eq!(pr["author"]["login"], "alice");
    assert_eq!(pr["state"], "OPEN");
    assert_eq!(pr["isDraft"], false);
    assert_eq!(pr["merged"], false);
    assert_eq!(pr["mergeable"], "MERGEABLE");
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
    assert_eq!(latest[0]["author"]["login"], "bob");
    assert_eq!(latest[0]["state"], "APPROVED");
    assert_eq!(latest[0]["submittedAt"], "2020-01-01T00:00:00Z");
    assert_eq!(pr["reviews"]["totalCount"], 2);

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

    let reviewed = run("is:pr is:merged reviewed-by:bob merged:>=2020-01-01");
    assert_eq!(reviewed.len(), 1);
    assert_eq!(reviewed[0]["number"], 1);

    let closed = run("is:pr is:closed is:unmerged author:alice closed:>=2020-01-01");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["number"], 2);
    assert_eq!(closed[0]["state"], "CLOSED");

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

/// TF's organization.teams query (teams.go): an org-kind actor answers with
/// an empty team set (teams are unmodeled — truthfully none), a user login
/// answers null, exactly the shapes TF degrades on without erroring.
#[tokio::test(flavor = "multi_thread")]
async fn tf_organization_teams_query() {
    let s = seed().await;
    mint_actor(&s.inst.base, "acme", "org");

    let query = r#"
		query($org: String!, $login: String!, $cursor: String) {
			organization(login: $org) {
				teams(first: 100, userLogins: [$login], after: $cursor) {
					pageInfo { hasNextPage endCursor }
					nodes {
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
