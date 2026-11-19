//! End-to-end for Dependabot alerts (spec/surface-github.md §1.13): the
//! switch, the list with its cursor and its `ETag`, one alert, a person's
//! dismissal and reopening, what the control plane does as GitHub, and the
//! GraphQL read of what Dependabot's updater did.

mod common;

use common::{boot, control_post, face_ok, faults, mint_actor};

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

struct Answer {
    status: u16,
    etag: Option<String>,
    link: Option<String>,
    json: serde_json::Value,
}

/// One call, with a JSON body when given one and `If-None-Match` when given
/// a tag.
fn call(
    method: &str,
    url: &str,
    token: &str,
    body: Option<serde_json::Value>,
    if_none_match: Option<&str>,
) -> Answer {
    let http = agent();
    let auth = format!("Bearer {token}");
    let result = match (method, body) {
        ("GET", _) => {
            let mut req = http.get(url).header("authorization", &auth);
            if let Some(tag) = if_none_match {
                req = req.header("if-none-match", tag);
            }
            req.call()
        }
        ("DELETE", _) => http.delete(url).header("authorization", &auth).call(),
        ("PUT", None) => http.put(url).header("authorization", &auth).send_empty(),
        ("PUT", Some(body)) => http.put(url).header("authorization", &auth).send_json(body),
        ("PATCH", Some(body)) => http
            .patch(url)
            .header("authorization", &auth)
            .send_json(body),
        ("POST", Some(body)) => http
            .post(url)
            .header("authorization", &auth)
            .send_json(body),
        (other, _) => panic!("unsupported call {other}"),
    };
    let mut resp = result.unwrap_or_else(|e| panic!("{method} {url}: {e:?}"));
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let (status, etag, link) = (resp.status().as_u16(), header("etag"), header("link"));
    let text = resp.body_mut().read_to_string().unwrap();
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    Answer {
        status,
        etag,
        link,
        json,
    }
}

fn numbers(answer: &Answer) -> Vec<i64> {
    answer
        .json
        .as_array()
        .unwrap_or_else(|| panic!("not a list: {}", answer.json))
        .iter()
        .map(|a| a["number"].as_i64().unwrap())
        .collect()
}

/// The value of `name` in the link whose `rel` is `rel`.
fn link_param(link: &str, rel: &str, name: &str) -> Option<String> {
    let part = link
        .split(", ")
        .find(|part| part.ends_with(&format!("rel=\"{rel}\"")))?;
    let url = part.strip_prefix('<')?.split('>').next()?;
    url.split_once('?')?
        .1
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
        .map(str::to_string)
}

/// A world with alice, the organization acme, and its repositories `web`,
/// whose alerts are on, and `api`, whose are off.
async fn seeded() -> (common::Instance, String) {
    let inst = boot().await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    mint_actor(&base, "acme", "org");
    for name in ["web", "api"] {
        face_ok(
            "POST",
            &format!("{base}/api/v3/orgs/acme/repos"),
            &alice,
            serde_json::json!({ "name": name, "private": true }),
        );
    }
    let on = call(
        "PUT",
        &format!("{base}/api/v3/repos/acme/web/vulnerability-alerts"),
        &alice,
        None,
        None,
    );
    assert_eq!(on.status, 204);
    (inst, alice)
}

fn lodash() -> serde_json::Value {
    serde_json::json!({
        "ecosystem": "npm",
        "package": "lodash",
        "manifest_path": "package-lock.json",
        "scope": "runtime",
        "version_requirement": "4.17.15",
        "ghsa_id": "GHSA-35jh-r3h4-6jhm",
        "cve_id": "CVE-2021-23337",
        "summary": "Command Injection in lodash",
        "description": "`lodash` versions prior to 4.17.21 are vulnerable.",
        "severity": "high",
        "vulnerable_version_range": "< 4.17.21",
        "first_patched_version": "4.17.21",
    })
}

/// Three alerts on acme/web, as GitHub would make them, a second apart.
fn three_alerts(base: &str) {
    let alerts = "/_orrery/repos/acme/web/dependabot-alerts";
    let made = control_post(base, alerts, lodash());
    assert_eq!(made["number"], 1);
    assert_eq!(made["state"], "open");
    assert_eq!(made["update"], serde_json::Value::Null);
    for (n, more) in [
        serde_json::json!({
            "ecosystem": "npm", "package": "minimist", "manifest_path": "package-lock.json",
            "scope": "development", "ghsa_id": "GHSA-vh95-rmgr-6w4m", "summary": "Prototype Pollution in minimist",
            "severity": "medium", "vulnerable_version_range": "< 0.2.1", "first_patched_version": "0.2.1",
        }),
        serde_json::json!({
            "ecosystem": "pip", "package": "django", "manifest_path": "requirements.txt",
            "ghsa_id": "GHSA-0000-0000-0001", "summary": "A flaw with no fix",
            "severity": "critical", "vulnerable_version_range": ">= 2.2, < 3",
        }),
    ]
    .into_iter()
    .enumerate()
    {
        control_post(base, "/_orrery/clock/advance", serde_json::json!({ "seconds": 1 }));
        assert_eq!(control_post(base, alerts, more)["number"], n as i64 + 2);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repositorys_alerts_are_listed_only_while_they_are_on() {
    let (inst, alice) = seeded().await;
    let api = format!("{}/api/v3", inst.base);
    let switch = format!("{api}/repos/acme/api/vulnerability-alerts");
    let list = format!("{api}/repos/acme/api/dependabot/alerts?state=open&per_page=1");

    // Off: GitHub says so in these words, which a caller tells from a
    // refused credential by.
    assert_eq!(call("GET", &switch, &alice, None, None).status, 404);
    for url in [&list, &format!("{api}/repos/acme/api/dependabot/alerts/1")] {
        let off = call("GET", url, &alice, None, None);
        assert_eq!(off.status, 403, "{url}");
        assert_eq!(
            off.json["message"],
            "Dependabot alerts are disabled for this repository."
        );
    }
    // GitHub makes no alert for a repository whose alerts are off.
    let mut refused = agent()
        .post(&format!(
            "{}/_orrery/repos/acme/api/dependabot-alerts",
            inst.base
        ))
        .send_json(lodash())
        .unwrap();
    assert_eq!(refused.status(), 400);
    let refused: serde_json::Value = refused.body_mut().read_json().unwrap();
    assert_eq!(faults(&refused)[0].0, "FAILED_PRECONDITION");

    // On, and on again; then readable, and empty.
    for _ in 0..2 {
        assert_eq!(call("PUT", &switch, &alice, None, None).status, 204);
    }
    assert_eq!(call("GET", &switch, &alice, None, None).status, 204);
    let empty = call("GET", &list, &alice, None, None);
    assert_eq!((empty.status, &empty.json), (200, &serde_json::json!([])));
    assert!(empty.link.is_none());

    // Off again keeps the alerts for when they are on again.
    control_post(
        &inst.base,
        "/_orrery/repos/acme/api/dependabot-alerts",
        lodash(),
    );
    for _ in 0..2 {
        assert_eq!(call("DELETE", &switch, &alice, None, None).status, 204);
    }
    assert_eq!(call("GET", &list, &alice, None, None).status, 403);
    assert_eq!(call("PUT", &switch, &alice, None, None).status, 204);
    assert_eq!(numbers(&call("GET", &list, &alice, None, None)), [1]);

    assert_eq!(
        call(
            "PUT",
            &format!("{api}/repos/acme/nope/vulnerability-alerts"),
            &alice,
            None,
            None
        )
        .status,
        404
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_list_pages_by_cursor_and_answers_an_unchanged_page_304() {
    let (inst, alice) = seeded().await;
    let base = &inst.base;
    let api = format!("{base}/api/v3");
    three_alerts(base);
    let list = format!("{api}/repos/acme/web/dependabot/alerts");
    let get = |query: &str| call("GET", &format!("{list}?{query}"), &alice, None, None);

    // Newest first, each as GitHub's alert object.
    let all = get("state=open&per_page=100");
    assert_eq!(numbers(&all), [3, 2, 1]);
    assert!(all.link.is_none());
    let lodash = &all.json[2];
    assert_eq!(lodash["state"], "open");
    assert_eq!(
        lodash["dependency"],
        serde_json::json!({
            "package": { "ecosystem": "npm", "name": "lodash" },
            "manifest_path": "package-lock.json",
            "version_requirement": "4.17.15",
            "scope": "runtime",
            "relationship": "unknown",
        })
    );
    assert_eq!(
        lodash["security_advisory"]["ghsa_id"],
        "GHSA-35jh-r3h4-6jhm"
    );
    assert_eq!(lodash["security_advisory"]["cve_id"], "CVE-2021-23337");
    assert_eq!(
        lodash["security_advisory"]["summary"],
        "Command Injection in lodash"
    );
    assert_eq!(lodash["security_advisory"]["severity"], "high");
    assert_eq!(
        lodash["security_vulnerability"],
        serde_json::json!({
            "package": { "ecosystem": "npm", "name": "lodash" },
            "severity": "high",
            "vulnerable_version_range": "< 4.17.21",
            "first_patched_version": { "identifier": "4.17.21" },
        })
    );
    assert_eq!(
        lodash["url"],
        format!("{api}/repos/acme/web/dependabot/alerts/1")
    );
    assert_eq!(
        lodash["html_url"],
        format!("{base}/acme/web/security/dependabot/1")
    );
    for key in [
        "dismissed_at",
        "dismissed_by",
        "dismissed_reason",
        "fixed_at",
        "auto_dismissed_at",
    ] {
        assert!(lodash[key].is_null(), "{key}");
    }
    // An alert given neither has neither.
    let django = &all.json[0];
    assert!(django["security_vulnerability"]["first_patched_version"].is_null());
    assert!(django["security_advisory"]["cve_id"].is_null());
    assert!(django["dependency"]["scope"].is_null());

    // A page names the next by a cursor, at the repository's id, and a page
    // reached by one names the page before it too.
    let first = get("state=open&per_page=2");
    assert_eq!(numbers(&first), [3, 2]);
    let link = first.link.clone().unwrap();
    let repo_id = face_ok(
        "GET",
        &format!("{api}/repos/acme/web"),
        &alice,
        serde_json::json!(null),
    )["id"]
        .clone();
    assert!(
        link.starts_with(&format!(
            "<{api}/repositories/{repo_id}/dependabot/alerts?state=open&per_page=2&after="
        )),
        "{link}"
    );
    assert!(link_param(&link, "prev", "before").is_none(), "{link}");
    let after = link_param(&link, "next", "after").unwrap();
    let second = get(&format!("state=open&per_page=2&after={after}"));
    assert_eq!(numbers(&second), [1]);
    let link = second.link.unwrap();
    assert!(link_param(&link, "next", "after").is_none(), "{link}");
    let before = link_param(&link, "prev", "before").unwrap();
    assert_eq!(
        numbers(&get(&format!("state=open&per_page=2&before={before}"))),
        [3, 2]
    );
    // The link itself is served.
    let followed = link.split('>').next().unwrap().trim_start_matches('<');
    assert_eq!(numbers(&call("GET", followed, &alice, None, None)), [3, 2]);

    // An unchanged page costs a 304, and a changed one does not.
    let tag = first.etag.unwrap();
    assert!(tag.starts_with('"') && tag.len() == 66, "{tag}");
    let same = call(
        "GET",
        &format!("{list}?state=open&per_page=2"),
        &alice,
        None,
        Some(&tag),
    );
    assert_eq!(same.status, 304);
    assert_eq!(same.etag.as_deref(), Some(tag.as_str()));
    control_post(
        base,
        "/_orrery/repos/acme/web/dependabot-alerts",
        serde_json::json!({
            "ecosystem": "go", "package": "example.com/mod", "manifest_path": "go.mod",
            "ghsa_id": "GHSA-0000-0000-0002", "summary": "Another", "severity": "low",
            "vulnerable_version_range": "< 1.2.3",
        }),
    );
    let changed = call(
        "GET",
        &format!("{list}?state=open&per_page=2"),
        &alice,
        None,
        Some(&tag),
    );
    assert_eq!(changed.status, 200);
    assert_eq!(numbers(&changed), [4, 3]);

    // The filters, and the default page.
    assert_eq!(numbers(&get("")), [4, 3, 2, 1]);
    assert_eq!(numbers(&get("severity=high,critical")), [3, 1]);
    assert_eq!(numbers(&get("severity=bogus")), [4, 3, 2, 1], "no filter");
    assert_eq!(numbers(&get("state=bogus")), [] as [i64; 0]);
    assert_eq!(numbers(&get("state=OPEN,dismissed")), [4, 3, 2, 1]);
    assert_eq!(numbers(&get("ecosystem=npm")), [2, 1]);
    assert_eq!(numbers(&get("package=django,lodash")), [3, 1]);
    assert_eq!(numbers(&get("manifest=go.mod")), [4]);
    assert_eq!(numbers(&get("scope=development")), [2]);
    assert_eq!(numbers(&get("sort=created&direction=asc")), [1, 2, 3, 4]);
    assert_eq!(
        numbers(&get("first=1&zzz=1")),
        [4, 3, 2, 1],
        "ignored, as on GitHub"
    );

    // What GitHub refuses.
    for (query, status, message) in [
        ("per_page=101", 400, ":per_page must be between 0 and 100"),
        (
            "page=2",
            400,
            "Pagination using the `page` parameter is not supported.",
        ),
        (
            "after=garbage",
            400,
            ":after does not appear to be a valid cursor",
        ),
        (
            "before=garbage",
            400,
            ":before does not appear to be a valid cursor",
        ),
    ] {
        let refused = get(query);
        assert_eq!(refused.status, status, "{query}");
        assert_eq!(refused.json["message"], message, "{query}");
    }
    // And a filter a world cannot apply is refused by name.
    let unsupported = get("has=patch");
    assert_eq!(unsupported.status, 422);
    assert_eq!(unsupported.json["orrery"]["field"], "has");
    // A page of no size is empty and names itself as the next.
    let none = get("per_page=0");
    assert_eq!((none.status, numbers(&none)), (200, vec![]));
    assert!(none.link.unwrap().ends_with("?per_page=0>; rel=\"next\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_person_dismisses_and_reopens_an_alert_and_github_fixes_one() {
    let (inst, alice) = seeded().await;
    let base = &inst.base;
    let api = format!("{base}/api/v3");
    three_alerts(base);
    let one = format!("{api}/repos/acme/web/dependabot/alerts/1");
    let patch = |url: &str, body: serde_json::Value| call("PATCH", url, &alice, Some(body), None);

    let read = call("GET", &one, &alice, None, None);
    assert_eq!(
        (read.status, &read.json["number"]),
        (200, &serde_json::json!(1))
    );
    for (number, status, message) in [
        ("99", 404, "No alert found for alert number 99"),
        ("0", 404, "No alert found for alert number 0"),
        ("abc", 422, "The alert_number parameter must be an integer."),
    ] {
        let url = format!("{api}/repos/acme/web/dependabot/alerts/{number}");
        let refused = call("GET", &url, &alice, None, None);
        assert_eq!(
            (refused.status, &refused.json["message"]),
            (status, &message.into())
        );
    }

    // What GitHub's schema and its rules refuse, in its words.
    for (body, status, message) in [
        (
            serde_json::json!({ "state": "dismissed" }),
            400,
            "Setting an alert to 'dismissed' requires a 'dismissed_reason'",
        ),
        (
            serde_json::json!({ "state": "dismissed", "dismissed_reason": "bogus" }),
            422,
            "Invalid request.\n\nInvalid property /dismissed_reason: `bogus` is not a possible value. Must be one of the following: fix_started, inaccurate, no_bandwidth, not_used, tolerable_risk.",
        ),
        (
            serde_json::json!({ "state": "fixed" }),
            422,
            "Invalid request.\n\nInvalid property /state: `fixed` is not a possible value. Must be one of the following: dismissed, open.",
        ),
        (
            serde_json::json!({}),
            422,
            "Invalid request.\n\nInvalid input: data matches no possible input. See `documentation_url`.",
        ),
        (
            serde_json::json!({ "zzz": 1 }),
            422,
            "Invalid request.\n\nInvalid input: data matches no possible input. See `documentation_url`.\nInvalid input: \"zzz\" is not a permitted key.",
        ),
        (
            serde_json::json!({ "state": "open" }),
            409,
            "Alert 1 is already open",
        ),
        (
            serde_json::json!({ "state": "open", "dismissed_reason": "not_used" }),
            400,
            "Can't set 'dismissed_reason' when setting an alert to 'open'",
        ),
    ] {
        let refused = patch(&one, body.clone());
        assert_eq!(refused.status, status, "{body}");
        assert_eq!(refused.json["message"], message, "{body}");
    }

    // Dismissed, by the person, with their reason.
    control_post(
        base,
        "/_orrery/clock/advance",
        serde_json::json!({ "seconds": 60 }),
    );
    let dismissed = patch(
        &one,
        serde_json::json!({
            "state": "dismissed", "dismissed_reason": "tolerable_risk", "dismissed_comment": "fine",
        }),
    );
    assert_eq!(dismissed.status, 200, "{}", dismissed.json);
    let alert = &dismissed.json;
    assert_eq!(alert["state"], "dismissed");
    assert_eq!(alert["dismissed_by"]["login"], "alice");
    assert_eq!(alert["dismissed_reason"], "tolerable_risk");
    assert_eq!(alert["dismissed_comment"], "fine");
    assert_eq!(alert["dismissed_at"], alert["updated_at"]);
    assert_ne!(alert["updated_at"], alert["created_at"]);
    assert_eq!(call("GET", &one, &alice, None, None).json, *alert);
    let again = patch(
        &one,
        serde_json::json!({ "state": "dismissed", "dismissed_reason": "not_used" }),
    );
    assert_eq!(
        (again.status, &again.json["message"]),
        (409, &"Alert 1 is already dismissed".into())
    );
    let list = format!("{api}/repos/acme/web/dependabot/alerts");
    assert_eq!(
        numbers(&call(
            "GET",
            &format!("{list}?state=dismissed"),
            &alice,
            None,
            None
        )),
        [1]
    );
    assert_eq!(
        numbers(&call(
            "GET",
            &format!("{list}?state=open"),
            &alice,
            None,
            None
        )),
        [3, 2]
    );

    // Reopened: the dismissal is gone.
    let reopened = patch(&one, serde_json::json!({ "state": "open" }));
    assert_eq!(reopened.status, 200);
    for key in [
        "dismissed_at",
        "dismissed_by",
        "dismissed_reason",
        "dismissed_comment",
    ] {
        assert!(reopened.json[key].is_null(), "{key}");
    }

    // GitHub fixes an alert itself, and a fixed one is nobody's to move.
    let control = |number: i64, body: serde_json::Value| {
        let mut resp = agent()
            .patch(&format!(
                "{base}/_orrery/repos/acme/web/dependabot-alerts/{number}"
            ))
            .send_json(body)
            .unwrap();
        let status = resp.status().as_u16();
        let json: serde_json::Value = resp.body_mut().read_json().unwrap();
        (status, json)
    };
    let (status, fixed) = control(1, serde_json::json!({ "state": "fixed" }));
    assert_eq!((status, &fixed["state"]), (200, &"fixed".into()));
    let read = call("GET", &one, &alice, None, None).json;
    assert_eq!(read["state"], "fixed");
    assert_eq!(read["fixed_at"], read["updated_at"]);
    for body in [
        serde_json::json!({ "state": "open" }),
        serde_json::json!({ "state": "dismissed", "dismissed_reason": "not_used" }),
    ] {
        let refused = patch(&one, body);
        assert_eq!(refused.status, 409);
        assert_eq!(
            refused.json["message"],
            "Fixed alerts cannot be dismissed or reopened"
        );
    }
    // It can come back, and a rule can dismiss one.
    let (status, back) = control(1, serde_json::json!({ "state": "open" }));
    assert_eq!((status, &back["state"]), (200, &"open".into()));
    assert!(call("GET", &one, &alice, None, None).json["fixed_at"].is_null());
    let (status, auto) = control(2, serde_json::json!({ "state": "auto_dismissed" }));
    assert_eq!((status, &auto["state"]), (200, &"auto_dismissed".into()));
    let two = call("GET", &format!("{list}/2"), &alice, None, None).json;
    assert_eq!(two["auto_dismissed_at"], two["updated_at"]);

    // What the control plane refuses: a person's move, a state that does not
    // follow, an unknown state, field, alert, and repository.
    for (number, body, status, field) in [
        (
            3,
            serde_json::json!({ "state": "dismissed" }),
            400,
            Some("state"),
        ),
        (
            3,
            serde_json::json!({ "state": "open" }),
            400,
            Some("state"),
        ),
        (
            3,
            serde_json::json!({ "state": "nope" }),
            400,
            Some("state"),
        ),
        (3, serde_json::json!({ "state": null }), 400, Some("state")),
        (3, serde_json::json!({ "zzz": 1 }), 400, Some("zzz")),
        (99, serde_json::json!({ "state": "fixed" }), 404, None),
    ] {
        let (got, refused) = control(number, body.clone());
        assert_eq!(got, status, "{body}: {refused}");
        assert_eq!(faults(&refused)[0].1.as_deref(), field, "{body}");
    }
    let (status, unchanged) = control(3, serde_json::json!({}));
    assert_eq!((status, &unchanged["state"]), (200, &"open".into()));

    // Every move is in the log, by who made it.
    let log = control_post(
        base,
        "/_orrery/log/list",
        serde_json::json!({ "page_size": 500 }),
    );
    let moves: Vec<(String, String)> = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"].as_str().unwrap().starts_with("dependency_alert:"))
        .skip(3)
        .map(|e| {
            (
                e["type"].as_str().unwrap().to_string(),
                e["actor"]["login"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let want: Vec<(String, String)> = [
        ("dependency_alert:dismissed", "alice"),
        ("dependency_alert:reopened", "alice"),
        ("dependency_alert:fixed", "orrery"),
        ("dependency_alert:reintroduced", "orrery"),
        ("dependency_alert:auto_dismissed", "orrery"),
    ]
    .iter()
    .map(|(t, a)| (t.to_string(), a.to_string()))
    .collect();
    assert_eq!(moves, want);
}

#[tokio::test(flavor = "multi_thread")]
async fn graphql_says_what_the_updater_did_about_each_open_alert() {
    let (inst, alice) = seeded().await;
    let base = &inst.base;
    three_alerts(base);
    let update = |number: i64, method: &str, body: Option<serde_json::Value>| {
        let url = format!("{base}/_orrery/repos/acme/web/dependabot-alerts/{number}/update");
        let mut resp = match (method, body) {
            ("PUT", Some(body)) => agent().put(&url).send_json(body),
            _ => agent().delete(&url).call(),
        }
        .unwrap();
        let status = resp.status().as_u16();
        let json: serde_json::Value = resp.body_mut().read_json().unwrap();
        (status, json)
    };
    // One has started, one failed, and one has had nothing done.
    let (status, pending) = update(1, "PUT", Some(serde_json::json!({})));
    assert_eq!(status, 200, "{pending}");
    assert_eq!(
        pending["update"],
        serde_json::json!({ "pull_request": null, "error": null })
    );
    let failed = serde_json::json!({
        "error": { "type": "dependency_file_not_found", "title": "Dependabot couldn't find a package.json" },
    });
    let (status, errored) = update(2, "PUT", Some(failed));
    assert_eq!(status, 200, "{errored}");
    assert_eq!(
        errored["update"]["error"]["type"],
        "dependency_file_not_found"
    );

    const QUERY: &str = "query($owner: String!, $name: String!, $after: String) {
        repository(owner: $owner, name: $name) {
            vulnerabilityAlerts(states: [OPEN], first: 2, after: $after) {
                pageInfo { hasNextPage endCursor }
                nodes { number dependabotUpdate { pullRequest { number url } error { errorType title } } }
            }
        }
        rateLimit { cost }
    }";
    let ask = |after: serde_json::Value| {
        let answer = call(
            "POST",
            &format!("{base}/api/graphql"),
            &alice,
            Some(serde_json::json!({
                "query": QUERY,
                "variables": { "owner": "acme", "name": "web", "after": after },
            })),
            None,
        );
        assert_eq!(answer.status, 200, "{}", answer.json);
        assert!(answer.json["errors"].is_null(), "{}", answer.json);
        answer.json["data"]["repository"]["vulnerabilityAlerts"].clone()
    };
    let first = ask(serde_json::Value::Null);
    assert_eq!(first["pageInfo"]["hasNextPage"], true);
    assert_eq!(
        first["nodes"],
        serde_json::json!([
            { "number": 1, "dependabotUpdate": { "pullRequest": null, "error": null } },
            { "number": 2, "dependabotUpdate": { "pullRequest": null, "error": {
                "errorType": "dependency_file_not_found",
                "title": "Dependabot couldn't find a package.json",
            } } },
        ])
    );
    let second = ask(first["pageInfo"]["endCursor"].clone());
    assert_eq!(second["pageInfo"]["hasNextPage"], false);
    assert_eq!(
        second["nodes"],
        serde_json::json!([{ "number": 3, "dependabotUpdate": null }])
    );

    // Taken back, nothing was done. An alert that is not open is not listed.
    let (status, cleared) = update(2, "DELETE", None);
    assert_eq!(
        (status, &cleared["update"]),
        (200, &serde_json::Value::Null)
    );
    agent()
        .patch(&format!(
            "{base}/_orrery/repos/acme/web/dependabot-alerts/1"
        ))
        .send_json(serde_json::json!({ "state": "fixed" }))
        .unwrap();
    assert_eq!(
        ask(serde_json::Value::Null)["nodes"],
        serde_json::json!([
            { "number": 2, "dependabotUpdate": null },
            { "number": 3, "dependabotUpdate": null },
        ])
    );

    // An update names a pull request of the repository, or an error.
    for (body, field) in [
        (serde_json::json!({ "pull_request": 99 }), "pull_request"),
        (
            serde_json::json!({ "pull_request": 1, "error": { "type": "t", "title": "x" } }),
            "error",
        ),
        (serde_json::json!({ "error": { "type": "t" } }), "error"),
        (serde_json::json!({ "zzz": 1 }), "zzz"),
    ] {
        let (status, refused) = update(3, "PUT", Some(body.clone()));
        assert_eq!(status, 400, "{body}: {refused}");
        assert_eq!(faults(&refused)[0].1.as_deref(), Some(field), "{body}");
    }
    assert_eq!(update(99, "PUT", Some(serde_json::json!({}))).0, 404);

    // A repository whose alerts are off lists none.
    let off = call(
        "POST",
        &format!("{base}/api/graphql"),
        &alice,
        Some(serde_json::json!({
            "query": QUERY,
            "variables": { "owner": "acme", "name": "api", "after": null },
        })),
        None,
    );
    assert_eq!(
        off.json["data"]["repository"]["vulnerabilityAlerts"]["nodes"],
        serde_json::json!([])
    );
}
