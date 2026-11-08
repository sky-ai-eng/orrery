//! End-to-end for organization membership, teams and review requests:
//! the authoring writes a scenario uses to build them, the REST reads TF's
//! poller and settings make (spec/surface-github.md §1.1 and §1.2), and TF's GraphQL
//! selections over teams, pending requests and request timeline items, using
//! the query texts from triage-factory's `internal/github`.

mod common;

use common::{boot, face_ok, git, mint_actor};

/// Any call, answered with its status and its body (`null` when empty).
fn call(
    method: &str,
    url: &str,
    token: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let auth = format!("token {token}");
    let result = match (method, body) {
        ("GET", _) => agent.get(url).header("authorization", &auth).call(),
        ("DELETE", None) => agent.delete(url).header("authorization", &auth).call(),
        ("DELETE", Some(b)) => agent
            .delete(url)
            .header("authorization", &auth)
            .force_send_body()
            .send_json(b),
        ("POST", b) => agent
            .post(url)
            .header("authorization", &auth)
            .send_json(b.unwrap_or(serde_json::json!({}))),
        ("PUT", None) => agent.put(url).header("authorization", &auth).send_empty(),
        ("PUT", Some(b)) => agent.put(url).header("authorization", &auth).send_json(b),
        (other, _) => panic!("unsupported method {other}"),
    };
    let mut resp = result.unwrap_or_else(|e| panic!("{method} {url}: {e:?}"));
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().expect("body");
    let json = if text.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text).expect("json body")
    };
    (status, json)
}

fn gql(base: &str, token: &str, query: &str, variables: serde_json::Value) -> serde_json::Value {
    let resp = gql_raw(base, token, query, variables);
    assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
    resp
}

/// A GraphQL call whose errors are the point: the first error's message.
fn gql_error(base: &str, token: &str, query: &str, variables: serde_json::Value) -> String {
    let resp = gql_raw(base, token, query, variables);
    resp["errors"][0]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an error: {resp}"))
        .to_string()
}

fn gql_raw(
    base: &str,
    token: &str,
    query: &str,
    variables: serde_json::Value,
) -> serde_json::Value {
    let (status, resp) = call(
        "POST",
        &format!("{base}/api/graphql"),
        token,
        Some(serde_json::json!({ "query": query, "variables": variables })),
    );
    assert_eq!(status, 200);
    resp
}

/// TF's `ListUserTeamsInOrg` query (internal/github/teams.go), verbatim.
const USER_TEAMS_IN_ORG: &str = r#"
		query UserTeamsInOrg($org: String!, $login: String!, $cursor: String) {
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
			rateLimit { cost }
		}
	"#;

/// TF's `ListOrgTeamsDetailed` query (internal/github/teams.go), verbatim.
const ORG_TEAMS: &str = r#"
		query OrgTeams($org: String!, $cursor: String) {
			organization(login: $org) {
				databaseId
				teams(first: 100, after: $cursor) {
					pageInfo { hasNextPage endCursor }
					nodes {
						databaseId
						slug
						name
						members { totalCount }
					}
				}
			}
			rateLimit { cost }
		}
	"#;

/// The review-request selections of TF's `prBaseFields` and `prFullFragment`
/// (internal/github/graphql.go), verbatim.
const PR_REQUESTS: &str = r#"
	query($id: ID!) { node(id: $id) { ... on PullRequest {
	reviewRequests(first: 100) {
		pageInfo { hasNextPage }
		nodes {
			requestedReviewer {
				... on User { login databaseId }
				... on Team { databaseId slug organization { login } }
			}
		}
	}
	timelineItems(last: 20, itemTypes: [LABELED_EVENT, UNLABELED_EVENT, REVIEW_REQUESTED_EVENT, READY_FOR_REVIEW_EVENT]) {
		nodes {
			__typename
			... on ReviewRequestedEvent {
				createdAt
				requestedReviewer {
					... on User { login }
					... on Team { slug organization { login } }
				}
			}
		}
	}
	} } }
"#;

struct Seeded {
    inst: common::Instance,
    alice: String,
    bob: String,
    carol: String,
    #[allow(dead_code)]
    work: tempfile::TempDir,
}

impl Seeded {
    fn api(&self, path: &str) -> String {
        format!("{}/api/v3{path}", self.inst.base)
    }
}

/// An `acme` org with alice as admin, and alice's `acme/webapp` with `main`
/// and a `feature` branch pushed and a pull open from it. bob and carol
/// exist but belong to nothing.
async fn seed() -> Seeded {
    let inst = boot().await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    let bob = mint_actor(&base, "bob", "user");
    let carol = mint_actor(&base, "carol", "user");
    mint_actor(&base, "acme", "org");
    let s = Seeded {
        inst,
        alice,
        bob,
        carol,
        work: tempfile::tempdir().unwrap(),
    };
    face_ok(
        "PUT",
        &s.api("/orgs/acme/memberships/alice"),
        &s.alice,
        serde_json::json!({ "role": "admin" }),
    );
    let repo = face_ok(
        "POST",
        &s.api("/orgs/acme/repos"),
        &s.alice,
        serde_json::json!({ "name": "webapp" }),
    );
    let clone_url = repo["clone_url"].as_str().unwrap();
    let authed = clone_url.replace("http://", &format!("http://x-access-token:{}@", s.alice));
    let clock = s.inst.world.clock().unwrap();
    let src = s.work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# webapp\n").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "init"]);
    git(&src, clock, &["checkout", "-b", "feature"]);
    std::fs::write(src.join("feature.txt"), "v1\n").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "add feature"]);
    git(&src, clock, &["push", &authed, "main", "feature"]);
    face_ok(
        "POST",
        &s.api("/repos/acme/webapp/pulls"),
        &s.alice,
        serde_json::json!({ "title": "Add feature", "head": "feature", "base": "main" }),
    );
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn org_and_team_membership() {
    let s = seed().await;

    let (status, m) = call("PUT", &s.api("/orgs/acme/memberships/bob"), &s.alice, None);
    assert_eq!(status, 200, "{m}");
    assert_eq!(m["state"], "active");
    assert_eq!(m["role"], "member", "role defaults to member");
    assert_eq!(m["user"]["login"], "bob");
    assert_eq!(m["organization"]["login"], "acme");
    for (path, body, want) in [
        ("/orgs/acme/memberships/ghost", None, 404),
        ("/orgs/alice/memberships/bob", None, 404),
        (
            "/orgs/acme/memberships/bob",
            Some(serde_json::json!({ "role": "owner" })),
            422,
        ),
    ] {
        assert_eq!(call("PUT", &s.api(path), &s.alice, body).0, want, "{path}");
    }

    let (status, team) = call(
        "POST",
        &s.api("/orgs/acme/teams"),
        &s.alice,
        Some(serde_json::json!({ "name": "Platform Team", "description": "infra" })),
    );
    assert_eq!(status, 201, "{team}");
    assert_eq!(team["slug"], "platform-team");
    assert_eq!(team["name"], "Platform Team");
    assert_eq!(team["members_count"], 1, "alice joins the team she made");
    assert_eq!(team["organization"]["login"], "acme");
    assert_eq!(team["privacy"], "closed");
    assert!(team["node_id"].as_str().unwrap().starts_with("O_team_"));

    for (body, want) in [
        (serde_json::json!({ "name": "platform team" }), 422),
        (serde_json::json!({}), 422),
        (
            serde_json::json!({ "name": "x", "maintainers": ["bob"] }),
            422,
        ),
        (serde_json::json!({ "name": "x", "privacy": "secret" }), 422),
    ] {
        assert_eq!(
            call(
                "POST",
                &s.api("/orgs/acme/teams"),
                &s.alice,
                Some(body.clone())
            )
            .0,
            want,
            "{body}"
        );
    }
    assert_eq!(
        call(
            "POST",
            &s.api("/orgs/alice/teams"),
            &s.alice,
            Some(serde_json::json!({ "name": "Mine" }))
        )
        .0,
        404
    );

    let (status, tm) = call(
        "PUT",
        &s.api("/orgs/acme/teams/platform-team/memberships/bob"),
        &s.alice,
        Some(serde_json::json!({ "role": "maintainer" })),
    );
    assert_eq!(status, 200, "{tm}");
    assert_eq!(tm["role"], "maintainer");
    assert_eq!(tm["state"], "active");
    assert_eq!(
        call(
            "PUT",
            &s.api("/orgs/acme/teams/platform-team/memberships/carol"),
            &s.alice,
            None
        )
        .0,
        422,
        "carol is not in acme"
    );
    assert_eq!(
        call(
            "PUT",
            &s.api("/orgs/acme/teams/nope/memberships/bob"),
            &s.alice,
            None
        )
        .0,
        404
    );

    let (status, listed) = call(
        "GET",
        &s.api("/orgs/acme/teams?per_page=100&page=1"),
        &s.alice,
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(listed[0]["slug"], "platform-team");
    assert_eq!(listed[0]["name"], "Platform Team");

    let (status, mine) = call(
        "GET",
        &s.api("/user/teams?per_page=100&page=1"),
        &s.bob,
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(mine[0]["slug"], "platform-team");
    assert_eq!(mine[0]["organization"]["login"], "acme");
    assert_eq!(mine[0]["members_count"], 2);
    let (_, carols) = call("GET", &s.api("/user/teams"), &s.carol, None);
    assert_eq!(carols, serde_json::json!([]));

    let resp = gql(
        &s.inst.base,
        &s.alice,
        USER_TEAMS_IN_ORG,
        serde_json::json!({ "org": "acme", "login": "bob", "cursor": null }),
    );
    // GraphQL's databaseIds are the ids REST answers with.
    assert_eq!(
        resp["data"]["organization"]["databaseId"],
        m["organization"]["id"]
    );
    let teams = &resp["data"]["organization"]["teams"];
    assert_eq!(
        teams["nodes"],
        serde_json::json!([{ "databaseId": team["id"], "slug": "platform-team",
                             "name": "Platform Team", "members": { "totalCount": 2 } }])
    );
    let resp = gql(
        &s.inst.base,
        &s.alice,
        USER_TEAMS_IN_ORG,
        serde_json::json!({ "org": "acme", "login": "carol", "cursor": null }),
    );
    assert_eq!(
        resp["data"]["organization"]["teams"]["nodes"],
        serde_json::json!([])
    );

    let (status, _) = call(
        "DELETE",
        &s.api("/orgs/acme/teams/platform-team/memberships/bob"),
        &s.alice,
        None,
    );
    assert_eq!(status, 204);
    assert_eq!(
        call(
            "DELETE",
            &s.api("/orgs/acme/teams/platform-team/memberships/bob"),
            &s.alice,
            None
        )
        .0,
        204,
        "removing someone not on the team succeeds"
    );
    for path in [
        "/orgs/acme/teams/platform-team/memberships/ghost",
        "/orgs/acme/teams/nope/memberships/bob",
    ] {
        assert_eq!(
            call("DELETE", &s.api(path), &s.alice, None).0,
            404,
            "{path}"
        );
    }
}

/// An empty body or a missing name is GitHub's `missing_field`, and a name
/// that is not a string is `invalid` — never a JSON parse error.
#[tokio::test(flavor = "multi_thread")]
async fn create_team_body_faults() {
    let s = seed().await;
    let url = s.api("/orgs/acme/teams");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .post(&url)
        .header("authorization", &format!("token {}", s.alice))
        .send_empty()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 422);
    let body: serde_json::Value = resp.body_mut().read_json().unwrap();
    assert_eq!(body["errors"][0]["code"], "missing_field");
    assert_eq!(body["errors"][0]["field"], "name");

    for (body, code) in [
        (serde_json::json!({}), "missing_field"),
        (serde_json::json!({ "name": null }), "missing_field"),
        (serde_json::json!({ "name": "  " }), "missing_field"),
        (serde_json::json!({ "name": 7 }), "invalid"),
        (serde_json::json!({ "name": ["x"] }), "invalid"),
    ] {
        let (status, err) = call("POST", &url, &s.alice, Some(body.clone()));
        assert_eq!(status, 422, "{body}");
        assert_eq!(err["errors"][0]["code"], code, "{body}");
        assert_eq!(err["errors"][0]["field"], "name", "{body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn org_teams_page_by_cursor() {
    let s = seed().await;
    for name in ["One", "Two", "Three"] {
        face_ok(
            "POST",
            &s.api("/orgs/acme/teams"),
            &s.alice,
            serde_json::json!({ "name": name }),
        );
    }
    let page = |cursor: serde_json::Value| {
        let q = ORG_TEAMS.replace("first: 100", "first: 2").replace(
            "pageInfo { hasNextPage endCursor }",
            "pageInfo { hasNextPage startCursor endCursor }",
        );
        gql(
            &s.inst.base,
            &s.alice,
            &q,
            serde_json::json!({ "org": "acme", "cursor": cursor }),
        )["data"]["organization"]["teams"]
            .clone()
    };
    let first = page(serde_json::Value::Null);
    assert_eq!(first["pageInfo"]["hasNextPage"], true);
    let slugs = |t: &serde_json::Value| -> Vec<String> {
        t["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["slug"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(slugs(&first), ["one", "two"]);
    assert!(first["pageInfo"]["startCursor"].is_string());
    assert_ne!(
        first["pageInfo"]["startCursor"],
        first["pageInfo"]["endCursor"]
    );
    let second = page(first["pageInfo"]["endCursor"].clone());
    assert_eq!(second["pageInfo"]["hasNextPage"], false);
    assert_eq!(slugs(&second), ["three"]);
    assert_eq!(
        second["pageInfo"]["startCursor"], second["pageInfo"]["endCursor"],
        "a one-item page starts and ends on the same item"
    );

    // Exactly as many as asked for: no next page.
    let exact = gql(
        &s.inst.base,
        &s.alice,
        &ORG_TEAMS.replace("first: 100", "first: 3"),
        serde_json::json!({ "org": "acme", "cursor": null }),
    );
    let teams = &exact["data"]["organization"]["teams"];
    assert_eq!(slugs(teams), ["one", "two", "three"]);
    assert_eq!(teams["pageInfo"]["hasNextPage"], false);

    // `after` composes with the `userLogins` filter: bob is on one and three.
    face_ok(
        "PUT",
        &s.api("/orgs/acme/memberships/bob"),
        &s.alice,
        serde_json::json!({}),
    );
    for slug in ["one", "three"] {
        face_ok(
            "PUT",
            &s.api(&format!("/orgs/acme/teams/{slug}/memberships/bob")),
            &s.alice,
            serde_json::json!({}),
        );
    }
    let bobs = |cursor: serde_json::Value| {
        gql(
            &s.inst.base,
            &s.alice,
            &USER_TEAMS_IN_ORG.replace("first: 100", "first: 1"),
            serde_json::json!({ "org": "acme", "login": "bob", "cursor": cursor }),
        )["data"]["organization"]["teams"]
            .clone()
    };
    let one = bobs(serde_json::Value::Null);
    assert_eq!(slugs(&one), ["one"]);
    assert_eq!(one["pageInfo"]["hasNextPage"], true);
    let three = bobs(one["pageInfo"]["endCursor"].clone());
    assert_eq!(slugs(&three), ["three"]);
    assert_eq!(three["pageInfo"]["hasNextPage"], false);

    let msg = gql_error(
        &s.inst.base,
        &s.alice,
        ORG_TEAMS,
        serde_json::json!({ "org": "acme", "cursor": "not-a-cursor" }),
    );
    assert_eq!(msg, "`not-a-cursor` does not appear to be a valid cursor.");
}

/// GitHub's page-size bounds on the connections TF pages: one of
/// `first`/`last` required, never negative, at most 100.
#[tokio::test(flavor = "multi_thread")]
async fn connections_bound_their_page_size() {
    let s = seed().await;
    let teams = |args: &str| {
        gql_error(
            &s.inst.base,
            &s.alice,
            &format!(
                "{{ organization(login: \"acme\") {{ teams{args} {{ nodes {{ slug }} }} }} }}"
            ),
            serde_json::json!({}),
        )
    };
    assert_eq!(
        teams(""),
        "You must provide a `first` or `last` value to properly paginate the `teams` connection."
    );
    assert_eq!(
        teams("(first: 101)"),
        "Requesting 101 records on the `teams` connection exceeds the `first` limit of 100 records."
    );
    assert_eq!(
        teams("(first: -1)"),
        "`first` on the `teams` connection cannot be less than zero."
    );
    let requests = gql_error(
        &s.inst.base,
        &s.alice,
        "query($id: ID!) { node(id: $id) { ... on PullRequest { reviewRequests { nodes { __typename } } } } }",
        serde_json::json!({ "id": "O_pull_1" }),
    );
    assert_eq!(
        requests,
        "You must provide a `first` or `last` value to properly paginate the `reviewRequests` connection."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn review_requests_round_trip() {
    let s = seed().await;
    face_ok(
        "PUT",
        &s.api("/orgs/acme/memberships/bob"),
        &s.alice,
        serde_json::json!({}),
    );
    face_ok(
        "POST",
        &s.api("/orgs/acme/teams"),
        &s.alice,
        serde_json::json!({ "name": "Platform" }),
    );
    let reviewers = s.api("/repos/acme/webapp/pulls/1/requested_reviewers");

    let (status, pull) = call(
        "POST",
        &reviewers,
        &s.alice,
        Some(serde_json::json!({ "reviewers": ["bob", "carol"], "team_reviewers": ["platform"] })),
    );
    assert_eq!(status, 201, "{pull}");
    let logins: Vec<&str> = pull["requested_reviewers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["login"].as_str().unwrap())
        .collect();
    assert_eq!(logins, ["bob", "carol"]);
    assert_eq!(pull["requested_teams"][0]["slug"], "platform");
    let listed = face_ok(
        "GET",
        &s.api("/repos/acme/webapp/pulls?state=open&per_page=100&page=1"),
        &s.alice,
        serde_json::json!(null),
    );
    assert_eq!(listed[0]["requested_teams"][0]["slug"], "platform");

    let (status, err) = call(
        "POST",
        &reviewers,
        &s.alice,
        Some(serde_json::json!({ "reviewers": ["alice"] })),
    );
    assert_eq!(status, 422);
    assert_eq!(
        err["errors"][0],
        "Review cannot be requested from pull request author."
    );
    let (status, err) = call(
        "POST",
        &reviewers,
        &s.alice,
        Some(serde_json::json!({ "reviewers": ["ghost"] })),
    );
    assert_eq!(status, 422);
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .starts_with("Reviews may only be requested from collaborators."),
        "{err}"
    );
    assert_eq!(
        call("POST", &reviewers, &s.alice, Some(serde_json::json!({}))).0,
        422
    );

    let resp = gql(
        &s.inst.base,
        &s.alice,
        PR_REQUESTS,
        serde_json::json!({ "id": "O_pull_1" }),
    );
    let node = &resp["data"]["node"];
    // Each reviewer's databaseId is the id the REST pull answered with.
    let (bob_id, carol_id) = (
        &pull["requested_reviewers"][0]["id"],
        &pull["requested_reviewers"][1]["id"],
    );
    let platform_id = &pull["requested_teams"][0]["id"];
    assert_eq!(
        node["reviewRequests"]["nodes"],
        serde_json::json!([
            { "requestedReviewer": { "login": "bob", "databaseId": bob_id } },
            { "requestedReviewer": { "login": "carol", "databaseId": carol_id } },
            { "requestedReviewer": { "databaseId": platform_id, "slug": "platform",
                                     "organization": { "login": "acme" } } },
        ])
    );
    let requested: Vec<&serde_json::Value> = node["timelineItems"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            assert_eq!(n["__typename"], "ReviewRequestedEvent");
            assert_eq!(n["createdAt"], "2020-01-01T00:00:00Z");
            &n["requestedReviewer"]
        })
        .collect();
    assert_eq!(requested.len(), 3);
    assert_eq!(requested[2]["slug"], "platform");

    let two = gql(
        &s.inst.base,
        &s.alice,
        "query($id: ID!) { node(id: $id) { ... on PullRequest { reviewRequests(first: 2) { pageInfo { hasNextPage } nodes { requestedReviewer { ... on User { login } } } } } } }",
        serde_json::json!({ "id": "O_pull_1" }),
    );
    let two = &two["data"]["node"]["reviewRequests"];
    assert_eq!(
        two["pageInfo"]["hasNextPage"], true,
        "truncation is reported"
    );
    assert_eq!(
        two["nodes"],
        serde_json::json!([
            { "requestedReviewer": { "login": "bob" } },
            { "requestedReviewer": { "login": "carol" } },
        ])
    );

    // bob's review clears his request; withdrawing carol's lands on the
    // timeline as a removal, which TF's itemTypes filter leaves out.
    face_ok(
        "POST",
        &s.api("/repos/acme/webapp/pulls/1/reviews"),
        &s.bob,
        serde_json::json!({ "event": "APPROVE" }),
    );
    let (status, pull) = call(
        "DELETE",
        &reviewers,
        &s.alice,
        Some(serde_json::json!({ "reviewers": ["carol"] })),
    );
    assert_eq!(status, 200, "{pull}");
    assert_eq!(pull["requested_reviewers"], serde_json::json!([]));
    assert_eq!(pull["requested_teams"][0]["slug"], "platform");
    assert_eq!(
        call("DELETE", &reviewers, &s.alice, Some(serde_json::json!({}))).0,
        422,
        "reviewers is required"
    );
    let all = gql(
        &s.inst.base,
        &s.alice,
        "query($id: ID!) { node(id: $id) { ... on PullRequest { timelineItems(first: 10) { nodes { __typename } } } } }",
        serde_json::json!({ "id": "O_pull_1" }),
    );
    let kinds: Vec<&str> = all["data"]["node"]["timelineItems"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["__typename"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "ReviewRequestedEvent",
            "ReviewRequestedEvent",
            "ReviewRequestedEvent",
            "ReviewRequestRemovedEvent"
        ]
    );
    let filtered = gql(
        &s.inst.base,
        &s.alice,
        "query($id: ID!) { node(id: $id) { ... on PullRequest { timelineItems(first: 10, itemTypes: [REVIEW_REQUESTED_EVENT]) { nodes { __typename } } } } }",
        serde_json::json!({ "id": "O_pull_1" }),
    );
    assert_eq!(
        filtered["data"]["node"]["timelineItems"]["nodes"],
        serde_json::json!([
            { "__typename": "ReviewRequestedEvent" },
            { "__typename": "ReviewRequestedEvent" },
            { "__typename": "ReviewRequestedEvent" },
        ])
    );

    // Deleting the team drops its request; the timeline keeps the request
    // with the team gone, as GitHub shows a deleted reviewer.
    let (status, _) = call(
        "DELETE",
        &s.api("/orgs/acme/teams/platform"),
        &s.alice,
        None,
    );
    assert_eq!(status, 204);
    assert_eq!(
        call(
            "DELETE",
            &s.api("/orgs/acme/teams/platform"),
            &s.alice,
            None
        )
        .0,
        404
    );
    let resp = gql(
        &s.inst.base,
        &s.alice,
        PR_REQUESTS,
        serde_json::json!({ "id": "O_pull_1" }),
    );
    let node = &resp["data"]["node"];
    assert_eq!(node["reviewRequests"]["nodes"], serde_json::json!([]));
    let reviewers_on_timeline = |node: &serde_json::Value| -> Vec<serde_json::Value> {
        node["timelineItems"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["requestedReviewer"].clone())
            .collect()
    };
    assert_eq!(
        reviewers_on_timeline(node),
        [
            serde_json::json!({ "login": "bob" }),
            serde_json::json!({ "login": "carol" }),
            serde_json::Value::Null,
        ]
    );
    let (_, teams) = call("GET", &s.api("/orgs/acme/teams"), &s.alice, None);
    assert_eq!(teams, serde_json::json!([]));

    // A new team that takes the slug is a different team: the old request
    // still names nobody, and the new team's own request names it.
    face_ok(
        "POST",
        &s.api("/orgs/acme/teams"),
        &s.alice,
        serde_json::json!({ "name": "Platform" }),
    );
    face_ok(
        "POST",
        &reviewers,
        &s.alice,
        serde_json::json!({ "team_reviewers": ["platform"] }),
    );
    let resp = gql(
        &s.inst.base,
        &s.alice,
        PR_REQUESTS,
        serde_json::json!({ "id": "O_pull_1" }),
    );
    let timeline = reviewers_on_timeline(&resp["data"]["node"]);
    assert_eq!(timeline.len(), 4);
    assert!(timeline[2].is_null(), "the deleted team stays gone");
    assert_eq!(timeline[3]["slug"], "platform");
}
