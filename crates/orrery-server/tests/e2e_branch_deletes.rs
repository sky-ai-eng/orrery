//! End-to-end for what a push that deletes or restores a branch does to the
//! pull requests on it (spec/surface-git.md, "What a push does"): real `git`
//! pushes, and the GraphQL timeline (spec/surface-github.md §3.3), the REST
//! pull, and the log read the result.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::{Instance, boot, control_post, face_answer, face_ok, git, mint_actor, snapshot};
use orrery_face_git::store;
use orrery_world::{Event, Pull, TimelineKind};

const TIMELINE: &str = r#"
    query($id: ID!, $types: [PullRequestTimelineItemsItemType!]) {
        node(id: $id) { ... on PullRequest {
            timelineItems(first: 100, itemTypes: $types) { nodes {
                __typename
                ... on ClosedEvent { actor { login } }
                ... on HeadRefDeletedEvent { actor { login } headRefName }
                ... on BaseRefDeletedEvent { actor { login } baseRefName }
                ... on HeadRefRestoredEvent { actor { login } }
            } }
        } }
    }
"#;

/// A served world holding `alice/webapp`, and a repository to commit and
/// push from.
struct Scene {
    inst: Instance,
    alice: String,
    bob: String,
    work: tempfile::TempDir,
}

impl Scene {
    async fn new() -> Scene {
        let inst = boot().await;
        let alice = mint_actor(&inst.control, "alice", "user");
        let bob = mint_actor(&inst.control, "bob", "user");
        face_ok(
            "POST",
            &format!("{}/api/v3/user/repos", inst.base),
            &alice,
            serde_json::json!({ "name": "webapp" }),
        );
        let work = tempfile::tempdir().unwrap();
        let scene = Scene {
            inst,
            alice,
            bob,
            work,
        };
        let clock = scene.clock();
        git(&scene.src(), clock, &["init", "-b", "main", "."]);
        std::fs::write(scene.src().join("README.md"), "# webapp\n").unwrap();
        git(&scene.src(), clock, &["add", "."]);
        git(&scene.src(), clock, &["commit", "-m", "init"]);
        scene
    }

    fn src(&self) -> PathBuf {
        self.work.path().to_path_buf()
    }

    fn clock(&self) -> i64 {
        self.inst.world.clock().unwrap()
    }

    fn advance(&self) {
        control_post(
            &self.inst.control,
            "/_orrery/clock/advance",
            serde_json::json!({ "seconds": 60 }),
        );
    }

    fn api(&self) -> String {
        format!("{}/api/v3/repos/alice/webapp", self.inst.base)
    }

    fn bare(&self) -> PathBuf {
        store::bare_dir(&self.inst.world.repos_dir(), "alice", "webapp")
    }

    /// A commit adding `file` on a new `branch` from `from`; returns its sha.
    fn branch(&self, branch: &str, from: &str, file: &str) -> String {
        let clock = self.clock();
        git(&self.src(), clock, &["checkout", "-q", "-b", branch, from]);
        std::fs::write(self.src().join(file), format!("{file}\n")).unwrap();
        git(&self.src(), clock, &["add", "."]);
        git(&self.src(), clock, &["commit", "-q", "-m", file]);
        git(&self.src(), clock, &["rev-parse", "HEAD"])
            .trim()
            .to_string()
    }

    fn push(&self, token: &str, refspecs: &[&str]) {
        push_to(&self.inst, &self.src(), token, refspecs);
    }

    fn open(&self, head: &str, base: &str, draft: bool) -> serde_json::Value {
        face_ok(
            "POST",
            &format!("{}/pulls", self.api()),
            &self.alice,
            serde_json::json!({ "title": head, "head": head, "base": base, "draft": draft }),
        )
    }

    fn pull(&self, number: i64) -> serde_json::Value {
        face_ok(
            "GET",
            &format!("{}/pulls/{number}", self.api()),
            &self.alice,
            serde_json::json!(null),
        )
    }

    /// The kernel's row for the pull, for the clocks it was stamped with.
    fn row(&self, number: i64) -> Pull {
        let repo = self
            .inst
            .world
            .repo_by_owner_name("alice", "webapp")
            .unwrap()
            .unwrap();
        self.inst
            .world
            .pull_by_number(repo.id, number)
            .unwrap()
            .unwrap()
    }

    fn set_state(&self, number: i64, state: &str) -> (u16, serde_json::Value) {
        face_answer(
            "PATCH",
            &format!("{}/pulls/{number}", self.api()),
            &self.alice,
            &[],
            Some(serde_json::json!({ "state": state })),
        )
    }

    fn merge(&self, number: i64) {
        face_ok(
            "PUT",
            &format!("{}/pulls/{number}/merge", self.api()),
            &self.alice,
            serde_json::json!({}),
        );
    }

    /// The pull's GraphQL timeline, oldest first, each item as its type, the
    /// branch it names if any, and its actor.
    fn timeline(&self, number: i64) -> Vec<String> {
        self.timeline_of(number, serde_json::json!(null))
    }

    /// The timeline items of `types` alone.
    fn timeline_of(&self, number: i64, types: serde_json::Value) -> Vec<String> {
        let answer: serde_json::Value = ureq::post(format!("{}/api/graphql", self.inst.base))
            .header("authorization", &format!("token {}", self.alice))
            .send_json(serde_json::json!({
                "query": TIMELINE,
                "variables": { "id": self.pull(number)["node_id"], "types": types },
            }))
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap();
        assert!(answer.get("errors").is_none(), "{answer}");
        answer["data"]["node"]["timelineItems"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| {
                let branch = n
                    .get("headRefName")
                    .or_else(|| n.get("baseRefName"))
                    .and_then(|b| b.as_str())
                    .map(|b| format!(" {b}"))
                    .unwrap_or_default();
                format!(
                    "{}{branch} by {}",
                    n["__typename"].as_str().unwrap(),
                    n["actor"]["login"].as_str().unwrap()
                )
            })
            .collect()
    }

    /// The events after `seq`, as type, actor, and what they are about: a
    /// ref by name, a pull by number, a timeline item by its pull's number
    /// and its kind. The clock's are left out.
    fn events_after(&self, seq: i64) -> Vec<(String, String, String)> {
        self.inst
            .world
            .log_after(seq, 1000)
            .unwrap()
            .into_iter()
            .filter(|e| e.body.type_name() != "world:clock_advanced")
            .map(|e| {
                let body = serde_json::to_value(&e.body).unwrap();
                let subject = &body["subject"];
                let about = match subject["type"].as_str().unwrap() {
                    "ref" | "repo" => subject["name"].as_str().unwrap().to_string(),
                    "pull" => format!("#{}", subject["number"]),
                    "timeline_item" => format!(
                        "#{} {}",
                        subject["pull"]["number"],
                        body["object"]["kind"].as_str().unwrap()
                    ),
                    other => other.to_string(),
                };
                (e.body.type_name().to_string(), e.actor.login, about)
            })
            .collect()
    }

    fn last_seq(&self) -> i64 {
        self.inst
            .world
            .log_after(0, 10_000)
            .unwrap()
            .last()
            .unwrap()
            .seq
    }
}

fn push_to(inst: &Instance, src: &Path, token: &str, refspecs: &[&str]) {
    let url = format!("{}/alice/webapp.git", inst.base)
        .replace("http://", &format!("http://x-access-token:{token}@"));
    let mut args = vec!["push", "-q", url.as_str()];
    args.extend(refspecs);
    git(src, inst.world.clock().unwrap(), &args);
}

fn event(kind: &str, actor: &str, about: &str) -> (String, String, String) {
    (kind.into(), actor.into(), about.into())
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_head_closes_its_open_pulls() {
    let s = Scene::new().await;
    s.branch("release", "main", "release.txt");
    let feature = s.branch("feature", "main", "feature.txt");
    s.push(&s.alice, &["main", "release", "feature"]);
    s.open("feature", "main", true);
    s.open("feature", "release", false);
    snapshot(&s.inst.control, "before");
    s.advance();
    let mark = s.last_seq();

    s.push(&s.bob, &[":feature"]);

    let now = s.clock();
    for (number, draft) in [(1, true), (2, false)] {
        let pull = s.pull(number);
        assert_eq!(pull["state"], "closed", "#{number}");
        assert_eq!(pull["merged"], false);
        assert_eq!(pull["draft"], draft, "a draft stays a draft");
        assert_eq!(pull["head"]["ref"], "feature");
        assert_eq!(pull["head"]["sha"], feature.as_str());
        let row = s.row(number);
        assert_eq!(row.closed_clock_unix, Some(now));
        assert_eq!(row.updated_clock_unix, now);
        assert_eq!(
            store::rev_parse(&s.bare(), &store::pull_head_ref(number)).unwrap(),
            Some(feature.clone())
        );
        assert_eq!(
            s.timeline(number),
            ["ClosedEvent by bob", "HeadRefDeletedEvent feature by bob"]
        );
    }
    // GitHub's order across the pulls too: both closes, then both deletions.
    let mut items: Vec<_> = [1, 2]
        .into_iter()
        .flat_map(|n| s.inst.world.list_pull_timeline(s.row(n).id).unwrap())
        .collect();
    items.sort_by_key(|i| i.id);
    let (one, two) = (s.row(1).id, s.row(2).id);
    let deleted = TimelineKind::HeadRefDeleted("feature".into());
    assert_eq!(
        items
            .iter()
            .map(|i| (i.pull_id, &i.kind))
            .collect::<Vec<_>>(),
        [
            (one, &TimelineKind::Closed),
            (two, &TimelineKind::Closed),
            (one, &deleted),
            (two, &deleted),
        ]
    );
    assert_eq!(
        s.events_after(mark),
        [
            event("repo:pushed", "bob", "webapp"),
            event("ref:deleted", "bob", "refs/heads/feature"),
            event("pull:closed", "bob", "#1"),
            event("timeline_item:created", "bob", "#1 closed"),
            event("pull:closed", "bob", "#2"),
            event("timeline_item:created", "bob", "#2 closed"),
            event("timeline_item:created", "bob", "#1 head_ref_deleted"),
            event("timeline_item:created", "bob", "#2 head_ref_deleted"),
        ]
    );

    let diff = control_post(
        &s.inst.control,
        "/_orrery/diff",
        serde_json::json!({ "from": "before" }),
    );
    let closed: Vec<i64> = diff["pulls_closed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["number"].as_i64().unwrap())
        .collect();
    assert_eq!(closed, [1, 2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_base_closes_its_open_pulls_without_retargeting_them() {
    let s = Scene::new().await;
    let release = s.branch("release", "main", "release.txt");
    s.branch("feature", "release", "feature.txt");
    s.branch("fix", "release", "fix.txt");
    s.push(&s.alice, &["main", "release", "feature", "fix"]);
    // `release` is the head of a merged pull, the base of an open one, and
    // the base of a closed one.
    s.open("release", "main", false);
    s.merge(1);
    s.open("feature", "release", false);
    s.open("fix", "release", false);
    assert_eq!(s.set_state(3, "closed").0, 200);
    let closed_before = s.row(3);
    s.advance();
    let mark = s.last_seq();

    s.push(&s.bob, &[":release"]);

    let pull = s.pull(2);
    assert_eq!(pull["state"], "closed");
    assert_eq!(pull["base"]["ref"], "release", "not retargeted");
    assert_eq!(pull["base"]["sha"], release.as_str());
    assert_eq!(
        s.timeline(2),
        ["BaseRefDeletedEvent release by bob", "ClosedEvent by bob"]
    );
    assert_eq!(s.row(3), closed_before, "a closed pull records nothing");
    assert_eq!(s.timeline(3), ["ClosedEvent by alice"]);
    let merged = s.pull(1);
    assert_eq!(merged["merged"], true);
    assert_eq!(
        s.timeline(1),
        ["ClosedEvent by alice", "HeadRefDeletedEvent release by bob"]
    );
    let branch_items = serde_json::json!([
        "HEAD_REF_DELETED_EVENT",
        "BASE_REF_DELETED_EVENT",
        "HEAD_REF_RESTORED_EVENT"
    ]);
    assert_eq!(
        s.timeline_of(1, branch_items.clone()),
        ["HeadRefDeletedEvent release by bob"]
    );
    assert_eq!(
        s.timeline_of(2, branch_items),
        ["BaseRefDeletedEvent release by bob"]
    );
    assert_eq!(
        s.events_after(mark),
        [
            event("repo:pushed", "bob", "webapp"),
            event("ref:deleted", "bob", "refs/heads/release"),
            event("pull:updated", "bob", "#1"),
            event("timeline_item:created", "bob", "#1 head_ref_deleted"),
            event("timeline_item:created", "bob", "#2 base_ref_deleted"),
            event("pull:closed", "bob", "#2"),
            event("timeline_item:created", "bob", "#2 closed"),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_pulls_head_is_deleted_and_restored_and_the_pull_reopens() {
    let s = Scene::new().await;
    let feature = s.branch("feature", "main", "feature.txt");
    s.branch("fix", "main", "fix.txt");
    s.push(&s.alice, &["main", "feature", "fix"]);
    s.open("feature", "main", false);
    assert_eq!(s.set_state(1, "closed").0, 200);
    s.open("fix", "main", false);
    s.merge(2);
    s.advance();
    let mark = s.last_seq();

    // Deleting a closed or merged pull's head records it and changes no
    // state.
    s.push(&s.bob, &[":feature", ":fix"]);
    for (number, branch, merged) in [(1, "feature", false), (2, "fix", true)] {
        let pull = s.pull(number);
        assert_eq!(pull["state"], "closed");
        assert_eq!(pull["merged"], merged);
        assert_eq!(s.row(number).updated_clock_unix, s.clock());
        assert_eq!(
            s.timeline(number),
            [
                "ClosedEvent by alice".to_string(),
                format!("HeadRefDeletedEvent {branch} by bob")
            ]
        );
    }
    assert_eq!(
        s.events_after(mark),
        [
            event("repo:pushed", "bob", "webapp"),
            event("ref:deleted", "bob", "refs/heads/feature"),
            event("ref:deleted", "bob", "refs/heads/fix"),
            event("pull:updated", "bob", "#1"),
            event("timeline_item:created", "bob", "#1 head_ref_deleted"),
            event("pull:updated", "bob", "#2"),
            event("timeline_item:created", "bob", "#2 head_ref_deleted"),
        ]
    );

    // Pushing the head back records its restoration, and the pull stays
    // closed until it is reopened.
    s.advance();
    let restore = format!("{feature}:refs/heads/feature");
    s.push(&s.alice, &[&restore]);
    assert_eq!(s.pull(1)["state"], "closed");
    let restored = [
        "ClosedEvent by alice",
        "HeadRefDeletedEvent feature by bob",
        "HeadRefRestoredEvent by alice",
    ];
    assert_eq!(s.timeline(1), restored);
    let (status, reopened) = s.set_state(1, "open");
    assert_eq!(status, 200, "{reopened}");
    assert_eq!(reopened["state"], "open");
    assert_eq!(reopened["head"]["sha"], feature.as_str());
    assert_eq!(s.timeline(1), restored, "a reopen adds no item");
}

/// Re-execute `log` against `into` through the same doors that made it: the
/// control plane for actors and the clock, real `git` from `objects` for
/// pushes, and the REST face for repositories and pull requests. Only each
/// write's operation is re-executed, the first of its events: the engine
/// makes the rest again. A push re-pushes every ref its write moved.
fn replay(log: &[Event], into: &Instance, objects: &Path) {
    let mut tokens: BTreeMap<String, String> = BTreeMap::new();
    let api = format!("{}/api/v3/repos/alice/webapp", into.base);
    for e in log.iter().filter(|e| e.seq == e.write.seq) {
        let b = serde_json::to_value(&e.body).unwrap();
        let token = tokens.get(&e.actor.login).cloned().unwrap_or_default();
        let set_state = |state: &str| {
            let url = format!("{api}/pulls/{}", b["subject"]["number"]);
            face_ok("PATCH", &url, &token, serde_json::json!({ "state": state }));
        };
        match e.body.type_name() {
            "world:created" => {}
            "actor:created" => {
                let login = b["subject"]["login"].as_str().unwrap();
                let minted =
                    mint_actor(&into.control, login, b["subject"]["kind"].as_str().unwrap());
                tokens.insert(login.to_string(), minted);
            }
            "world:clock_advanced" => {
                let clock = &b["changes"]["clock_unix"];
                let seconds = clock["to"].as_i64().unwrap() - clock["from"].as_i64().unwrap();
                control_post(
                    &into.control,
                    "/_orrery/clock/advance",
                    serde_json::json!({ "seconds": seconds }),
                );
            }
            "repo:created" => {
                face_ok(
                    "POST",
                    &format!("{}/api/v3/user/repos", into.base),
                    &token,
                    serde_json::json!({ "name": b["subject"]["name"] }),
                );
            }
            "repo:pushed" => {
                let refspecs: Vec<String> = log
                    .iter()
                    .filter(|m| m.write == e.write)
                    .filter_map(|m| {
                        let m = serde_json::to_value(&m.body).unwrap();
                        let name = m["subject"]["name"].as_str()?.to_string();
                        match m["type"].as_str().unwrap() {
                            "ref:created" => Some(format!("{}:{name}", m["object"]["sha"])),
                            "ref:updated" => Some(format!(
                                "{}{}:{name}",
                                if m["forced"] == true { "+" } else { "" },
                                m["changes"]["sha"]["to"]
                            )),
                            "ref:deleted" => Some(format!(":{name}")),
                            _ => None,
                        }
                    })
                    .map(|spec| spec.replace('"', ""))
                    .collect();
                let refspecs: Vec<&str> = refspecs.iter().map(String::as_str).collect();
                push_to(into, objects, &token, &refspecs);
            }
            "pull:opened" => {
                let o = &b["object"];
                let opened = face_ok(
                    "POST",
                    &format!("{api}/pulls"),
                    &token,
                    serde_json::json!({
                        "title": o["title"], "head": o["head"]["ref"], "base": o["base"]["ref"],
                        "draft": o["draft"],
                    }),
                );
                assert_eq!(opened["number"], o["number"]);
            }
            "pull:closed" => set_state("closed"),
            "pull:reopened" => set_state("open"),
            other => panic!("the replay does not re-execute {other}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn replaying_the_log_rebuilds_the_closes_and_the_timeline() {
    let s = Scene::new().await;
    let feature = s.branch("feature", "main", "feature.txt");
    s.branch("release", "main", "release.txt");
    s.branch("fix", "release", "fix.txt");
    s.push(&s.alice, &["main", "feature", "release", "fix"]);
    s.open("feature", "main", false);
    s.open("feature", "release", true);
    s.open("fix", "release", false);
    s.advance();
    // One push deletes the head of #1 and #2 and the base of #2 and #3.
    s.push(&s.bob, &[":feature", ":release"]);
    s.advance();
    s.push(&s.alice, &[&format!("{feature}:refs/heads/feature")]);
    assert_eq!(s.set_state(1, "open").0, 200);

    let replayed = boot().await;
    let log = s.inst.world.log_after(0, 10_000).unwrap();
    replay(&log, &replayed, &s.src());

    // Every event the re-executed operations made, consequences included,
    // is the one the original made, but for the real time it was appended.
    let events = |inst: &Instance| -> Vec<Event> {
        inst.world
            .log_after(0, 10_000)
            .unwrap()
            .into_iter()
            .map(|e| Event {
                wall_unix_ms: 0,
                ..e
            })
            .collect()
    };
    assert_eq!(events(&replayed), events(&s.inst));
    let repo = |inst: &Instance| {
        inst.world
            .repo_by_owner_name("alice", "webapp")
            .unwrap()
            .unwrap()
            .id
    };
    let pulls = |inst: &Instance| inst.world.list_pulls_for_repo(repo(inst), None).unwrap();
    assert_eq!(pulls(&replayed), pulls(&s.inst));
    let states: Vec<&str> = pulls(&s.inst)
        .iter()
        .rev()
        .map(|p| p.state.as_str())
        .collect();
    assert_eq!(states, ["open", "closed", "closed"]);
    for pull in pulls(&s.inst) {
        assert_eq!(
            replayed.world.list_pull_timeline(pull.id).unwrap(),
            s.inst.world.list_pull_timeline(pull.id).unwrap(),
            "#{}",
            pull.number
        );
    }
    let bare = |inst: &Instance| store::bare_dir(&inst.world.repos_dir(), "alice", "webapp");
    assert_eq!(
        store::list_refs(&bare(&replayed)).unwrap(),
        store::list_refs(&bare(&s.inst)).unwrap()
    );
}
