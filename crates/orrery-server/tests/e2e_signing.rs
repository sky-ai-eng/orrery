//! End-to-end for commit signing (spec/surface-github.md §1.10): signing keys
//! registered through the face, commits signed by real `git` with real
//! `ssh-keygen`, and the verdict GitHub's commit read gives each one. Every
//! test needs `ssh-keygen` on `PATH`, as a world that verifies does.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{boot, face, face_answer, face_ok, git, mint_actor};

/// A key pair `ssh-keygen` writes into `dir`: the private key's path, and
/// the public key as OpenSSH writes it, comment included.
fn keypair(dir: &Path, name: &str) -> (PathBuf, String) {
    let path = dir.join(name);
    let out = Command::new("ssh-keygen")
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            &format!("{name}@laptop"),
            "-f",
        ])
        .arg(&path)
        .output()
        .expect("ssh-keygen runs; a world that verifies needs it on PATH");
    assert!(out.status.success(), "ssh-keygen: {out:?}");
    let public = std::fs::read_to_string(path.with_extension("pub")).unwrap();
    (path, public.trim().to_string())
}

/// A public key as a world stores it: its type and blob, no comment.
fn bare_key(public: &str) -> String {
    public.split(' ').take(2).collect::<Vec<_>>().join(" ")
}

/// An empty commit on HEAD, committed as `email` at `clock_unix`, signed
/// with the SSH key at `key` when there is one. Returns its sha.
fn commit(work: &Path, clock_unix: i64, email: &str, key: Option<&Path>, message: &str) -> String {
    let date = format!("{clock_unix} +0000");
    let mut cmd = Command::new("git");
    cmd.current_dir(work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Alice")
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_COMMITTER_NAME", "Alice")
        .env("GIT_COMMITTER_EMAIL", email)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date);
    if let Some(key) = key {
        cmd.args(["-c", "gpg.format=ssh", "-c"])
            .arg(format!("user.signingkey={}", key.display()));
    }
    cmd.args(["commit", "--allow-empty", "-q", "-m", message]);
    if key.is_some() {
        cmd.arg("-S");
    }
    let out = cmd.output().expect("spawn git");
    assert!(out.status.success(), "git commit: {out:?}");
    git(work, clock_unix, &["rev-parse", "HEAD"])
        .trim()
        .to_string()
}

/// A commit object written as `raw` says, which `git commit` would never
/// write: a signature over other content, or one that does not parse.
fn raw_commit(work: &Path, raw: &str) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .current_dir(work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["hash-object", "-t", "commit", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(raw.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// `sha`'s commit with its message replaced, its signature kept.
fn retold(work: &Path, sha: &str, message: &str) -> String {
    let raw = git(work, 0, &["cat-file", "commit", sha]);
    let (headers, _) = raw.split_once("\n\n").unwrap();
    raw_commit(work, &format!("{headers}\n\n{message}\n"))
}

/// `sha`'s commit with its signature header replaced by `armor`, each line
/// after the first indented as git continues a header.
fn resigned(work: &Path, sha: &str, armor: &str) -> String {
    let raw = git(work, 0, &["cat-file", "commit", sha]);
    let (headers, message) = raw.split_once("\n\n").unwrap();
    let kept: Vec<&str> = headers
        .lines()
        .take_while(|l| !l.starts_with("gpgsig"))
        .collect();
    let signature = armor.lines().collect::<Vec<_>>().join("\n ");
    raw_commit(
        work,
        &format!("{}\ngpgsig {signature}\n\n{message}", kept.join("\n")),
    )
}

struct Fixture {
    inst: common::Instance,
    alice: String,
    api: String,
    work: tempfile::TempDir,
    alice_key: (PathBuf, String),
    other_key: (PathBuf, String),
    clock: i64,
}

/// alice holds `alice@corp.test`, verified, and `alice@old.test`, not; bob
/// holds `bob@corp.test`, verified. alice owns `alice/webapp`, and a work
/// tree whose `main` holds one unsigned commit.
async fn fixture() -> Fixture {
    let inst = boot().await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    mint_actor(&base, "bob", "user");
    let emails = |login: &str, emails: serde_json::Value| {
        ureq::put(format!("{base}/_orrery/actors/{login}/emails"))
            .send_json(serde_json::json!({ "emails": emails }))
            .expect("set emails");
    };
    emails(
        "alice",
        serde_json::json!([
            { "email": "alice@corp.test", "primary": true, "verified": true },
            { "email": "alice@old.test", "primary": false, "verified": false },
        ]),
    );
    emails(
        "bob",
        serde_json::json!([{ "email": "bob@corp.test", "primary": true, "verified": true }]),
    );
    let api = format!("{base}/api/v3");
    face_ok(
        "POST",
        &format!("{api}/user/repos"),
        &alice,
        serde_json::json!({ "name": "webapp" }),
    );
    let work = tempfile::tempdir().unwrap();
    let clock = inst.world.clock().unwrap();
    git(work.path(), clock, &["init", "-q", "-b", "main", "."]);
    commit(work.path(), clock, "alice@corp.test", None, "init");
    let keys = work.path().join("keys");
    std::fs::create_dir(&keys).unwrap();
    Fixture {
        alice_key: keypair(&keys, "alice"),
        other_key: keypair(&keys, "other"),
        inst,
        alice,
        api,
        work,
        clock,
    }
}

impl Fixture {
    fn src(&self) -> &Path {
        self.work.path()
    }

    /// A branch `name` off `main`, holding one commit.
    fn branch(&self, name: &str, email: &str, key: Option<&Path>) -> String {
        git(
            self.src(),
            self.clock,
            &["checkout", "-q", "-b", name, "main"],
        );
        commit(self.src(), self.clock, email, key, name)
    }

    /// Point branch `name` at `sha`.
    fn branch_at(&self, name: &str, sha: &str) {
        git(self.src(), self.clock, &["branch", "-f", name, sha]);
    }

    fn push_all(&self) {
        let url = format!(
            "{}/alice/webapp.git",
            self.inst
                .base
                .replace("http://", &format!("http://x-access-token:{}@", self.alice))
        );
        git(self.src(), self.clock, &["push", "-q", &url, "--all"]);
    }

    fn add_key(&self, public: &str) -> serde_json::Value {
        face_ok(
            "POST",
            &format!("{}/user/ssh_signing_keys", self.api),
            &self.alice,
            serde_json::json!({ "title": "laptop", "key": public }),
        )
    }

    /// `DELETE /user/ssh_signing_keys/{id}` as `token`, answered with its
    /// status.
    fn delete_key(&self, id: &serde_json::Value, token: &str) -> u16 {
        let resp = ureq::delete(format!("{}/user/ssh_signing_keys/{id}", self.api))
            .header("authorization", &format!("token {token}"))
            .call();
        match resp {
            Ok(r) => r.status().as_u16(),
            Err(ureq::Error::StatusCode(code)) => code,
            Err(e) => panic!("{e:?}"),
        }
    }

    fn verification(&self, sha: &str) -> serde_json::Value {
        let c = face_ok(
            "GET",
            &format!("{}/repos/alice/webapp/commits/{sha}", self.api),
            &self.alice,
            serde_json::Value::Null,
        );
        assert_eq!(c["sha"], sha);
        c["commit"]["verification"].clone()
    }

    /// `(verified, reason, committer email)` as the commit read gives them.
    fn verdict(&self, sha: &str) -> (bool, String, String) {
        let c = face_ok(
            "GET",
            &format!("{}/repos/alice/webapp/commits/{sha}", self.api),
            &self.alice,
            serde_json::Value::Null,
        );
        let v = &c["commit"]["verification"];
        (
            v["verified"].as_bool().unwrap(),
            v["reason"].as_str().unwrap().to_string(),
            c["commit"]["committer"]["email"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    }

    /// The events of the newest write in the log, by type.
    fn last_write(&self) -> Vec<serde_json::Value> {
        let log = self.inst.world.log_after(0, 10_000).unwrap();
        let last = log.last().unwrap().write.seq;
        log.into_iter()
            .filter(|e| e.write.seq == last)
            .map(|e| serde_json::to_value(e).unwrap())
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_commit_reads_with_the_verdict_github_gives() {
    let f = fixture().await;
    let alice_key = Some(f.alice_key.0.as_path());
    let added = f.add_key(&f.alice_key.1);

    let unsigned = git(f.src(), f.clock, &["rev-parse", "main"])
        .trim()
        .to_string();
    let valid = f.branch("valid", "alice@corp.test", alice_key);
    let unknown = f.branch("unknown", "alice@corp.test", Some(&f.other_key.0));
    let unverified = f.branch("unverified", "ALICE@old.test", alice_key);
    let elsewhere = f.branch("elsewhere", "bob@corp.test", alice_key);
    let nobody = f.branch("nobody", "nobody@nowhere.test", alice_key);
    let tampered = retold(f.src(), &valid, "not what was signed");
    f.branch_at("tampered", &tampered);
    let garbled = resigned(
        f.src(),
        &valid,
        "-----BEGIN SSH SIGNATURE-----\nU1NIU0lHAAAAAQ==\n-----END SSH SIGNATURE-----",
    );
    f.branch_at("garbled", &garbled);
    let pgp = resigned(
        f.src(),
        &valid,
        "-----BEGIN PGP SIGNATURE-----\n\niQEzBAABCAAdFiEE\n-----END PGP SIGNATURE-----",
    );
    f.branch_at("pgp", &pgp);
    f.push_all();

    let cases = [
        (&unsigned, false, "unsigned", "alice@corp.test"),
        (&valid, true, "valid", "alice@corp.test"),
        (&unknown, false, "unknown_key", "alice@corp.test"),
        (&unverified, false, "unverified_email", "ALICE@old.test"),
        (&elsewhere, false, "bad_email", "bob@corp.test"),
        (&nobody, false, "no_user", "nobody@nowhere.test"),
        (&tampered, false, "invalid", "alice@corp.test"),
        (&garbled, false, "malformed_signature", "alice@corp.test"),
        (&pgp, false, "unknown_key", "alice@corp.test"),
    ];
    for (sha, verified, reason, email) in cases {
        assert_eq!(
            f.verdict(sha),
            (verified, reason.to_string(), email.to_string()),
            "{reason}"
        );
    }

    // The push stored the one passing verdict, after its ref events.
    let push = f.last_write();
    let verified: Vec<&serde_json::Value> = push
        .iter()
        .filter(|e| e["type"] == "commit:verified")
        .collect();
    assert_eq!(verified.len(), 1, "{push:#?}");
    assert_eq!(push.last().unwrap()["type"], "commit:verified");
    assert_eq!(verified[0]["subject"]["sha"], valid.as_str());
    assert_eq!(verified[0]["subject"]["repo"]["name"], "webapp");
    assert_eq!(verified[0]["object"]["signer"]["login"], "alice");
    assert_eq!(verified[0]["object"]["signing_key"]["id"], added["id"]);
    assert_eq!(verified[0]["object"]["verified_unix"], f.clock);
    let stored = f.verification(&valid);
    assert_eq!(stored["verified_at"], "2020-01-01T00:00:00Z");
    assert!(
        stored["signature"]
            .as_str()
            .unwrap()
            .starts_with("-----BEGIN SSH SIGNATURE-----\n")
    );
    assert!(stored["payload"].as_str().unwrap().contains("\n\nvalid\n"));
    for sha in [&unsigned, &unverified] {
        assert_eq!(f.verification(sha)["verified_at"], serde_json::Value::Null);
    }
    assert_eq!(
        f.verification(&unsigned)["signature"],
        serde_json::Value::Null
    );
    assert_eq!(
        f.verification(&unsigned)["payload"],
        serde_json::Value::Null
    );

    // With the key gone, the stored verdict stands, and a failing one is
    // reached again on the next read.
    assert_eq!(f.delete_key(&added["id"], &f.alice), 204);
    assert_eq!(f.verdict(&valid).1, "valid");
    assert_eq!(f.verdict(&unverified).1, "unknown_key");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_verdict_the_push_stored_none_for_is_reached_on_every_read() {
    let f = fixture().await;
    let signed = f.branch("early", "alice@corp.test", Some(&f.alice_key.0));
    f.push_all();
    assert_eq!(f.verdict(&signed).1, "unknown_key");
    assert!(
        f.last_write()
            .iter()
            .all(|e| e["type"] != "commit:verified")
    );

    // Registered after the push, the key verifies the commit on read, with
    // no time a push stored it at.
    f.add_key(&f.alice_key.1);
    assert_eq!(
        f.verdict(&signed),
        (true, "valid".to_string(), "alice@corp.test".to_string())
    );
    assert_eq!(
        f.verification(&signed)["verified_at"],
        serde_json::Value::Null
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_commit_reads_by_sha_or_branch_with_its_parts() {
    let f = fixture().await;
    let tip = f.branch("feature", "alice@corp.test", None);
    f.push_all();
    let main = git(f.src(), f.clock, &["rev-parse", "main"])
        .trim()
        .to_string();
    let api = &f.api;
    let by_branch = face_ok(
        "GET",
        &format!("{api}/repos/alice/webapp/commits/feature"),
        &f.alice,
        serde_json::Value::Null,
    );
    let base = &f.inst.base;
    assert_eq!(
        by_branch,
        serde_json::json!({
            "sha": tip,
            "url": format!("{api}/repos/alice/webapp/commits/{tip}"),
            "html_url": format!("{base}/alice/webapp/commit/{tip}"),
            "commit": {
                "url": format!("{api}/repos/alice/webapp/git/commits/{tip}"),
                "author": { "name": "Alice", "email": "alice@corp.test", "date": "2020-01-01T00:00:00Z" },
                "committer": { "name": "Alice", "email": "alice@corp.test", "date": "2020-01-01T00:00:00Z" },
                "message": "feature",
                "tree": {
                    "sha": "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                    "url": format!("{api}/repos/alice/webapp/git/trees/4b825dc642cb6eb9a060e54bf8d69288fbee4904"),
                },
                "verification": {
                    "verified": false, "reason": "unsigned",
                    "signature": null, "payload": null, "verified_at": null,
                },
            },
            "parents": [{
                "sha": main,
                "url": format!("{api}/repos/alice/webapp/commits/{main}"),
                "html_url": format!("{base}/alice/webapp/commit/{main}"),
            }],
        })
    );
    assert_eq!(
        face_ok(
            "GET",
            &format!("{api}/repos/alice/webapp/commits/{tip}"),
            &f.alice,
            serde_json::Value::Null,
        ),
        by_branch
    );
    let missing = "1234567890123456789012345678901234567890";
    assert_eq!(
        face_answer(
            "GET",
            &format!("{api}/repos/alice/webapp/commits/{missing}"),
            &f.alice,
            &[],
            None
        ),
        (
            422,
            serde_json::json!({ "message": format!("No commit found for SHA: {missing}") })
        )
    );
    assert_eq!(
        face(
            "GET",
            &format!("{api}/repos/alice/nothing/commits/{tip}"),
            &f.alice,
            serde_json::Value::Null,
        ),
        Err(404)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn signing_keys_are_registered_listed_and_deleted_through_the_face() {
    let f = fixture().await;
    let api = &f.api;
    let base = &f.inst.base;
    let bob = ureq::post(format!("{base}/_orrery/actors/bob/tokens"))
        .send_json(serde_json::json!({}))
        .unwrap()
        .body_mut()
        .read_json::<serde_json::Value>()
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let bot = mint_actor(base, "ci-bot", "bot");

    let added = f.add_key(&f.alice_key.1);
    assert_eq!(
        added,
        serde_json::json!({
            "id": added["id"],
            "key": bare_key(&f.alice_key.1),
            "title": "laptop",
            "created_at": "2020-01-01T00:00:00Z",
        })
    );
    let untitled = face_ok(
        "POST",
        &format!("{api}/user/ssh_signing_keys"),
        &f.alice,
        serde_json::json!({ "key": f.other_key.1, "ignored": true }),
    );
    assert_eq!(untitled["title"], serde_json::Value::Null);
    assert!(untitled["id"].as_i64() > added["id"].as_i64());

    let listed = serde_json::json!([added, untitled]);
    for (url, token) in [
        (format!("{api}/users/alice/ssh_signing_keys"), &bob),
        (format!("{api}/users/ALICE/ssh_signing_keys"), &bob),
        (format!("{api}/user/ssh_signing_keys"), &f.alice),
    ] {
        assert_eq!(
            face_ok("GET", &url, token, serde_json::Value::Null),
            listed,
            "{url}"
        );
    }
    assert_eq!(
        face_ok(
            "GET",
            &format!("{api}/users/alice/ssh_signing_keys?per_page=1&page=2"),
            &bob,
            serde_json::Value::Null
        ),
        serde_json::json!([untitled])
    );
    assert_eq!(
        face_ok(
            "GET",
            &format!("{api}/users/ci-bot[bot]/ssh_signing_keys"),
            &bob,
            serde_json::Value::Null
        ),
        serde_json::json!([])
    );
    for missing in ["nobody", "ci-bot", "orrery"] {
        assert_eq!(
            face(
                "GET",
                &format!("{api}/users/{missing}/ssh_signing_keys"),
                &bob,
                serde_json::Value::Null
            ),
            Err(404),
            "{missing}"
        );
    }

    let refused = |token: &str, body: serde_json::Value| {
        face_answer(
            "POST",
            &format!("{api}/user/ssh_signing_keys"),
            token,
            &[],
            Some(body),
        )
    };
    let custom = |message: &str| {
        serde_json::json!({
            "message": "Validation Failed",
            "errors": [{ "resource": "SshSigningKey", "code": "custom", "field": "key",
                         "message": message }],
        })
    };
    assert_eq!(
        refused(&bob, serde_json::json!({ "title": "x" })),
        (
            422,
            serde_json::json!({
                "message": "Validation Failed",
                "errors": [{ "resource": "SshSigningKey", "code": "missing_field", "field": "key" }],
            })
        )
    );
    assert_eq!(
        refused(&bob, serde_json::json!({ "key": "ssh-ed25519 AAAA" })),
        (
            422,
            custom("key is invalid. You must supply a key in OpenSSH public key format")
        )
    );
    assert_eq!(
        refused(&bob, serde_json::json!({ "key": f.alice_key.1 })),
        (422, custom("key is already in use"))
    );
    assert_eq!(
        refused(&bob, serde_json::json!({ "key": 7 })),
        (
            400,
            serde_json::json!({ "message": "Problems parsing JSON" })
        )
    );
    assert_eq!(
        refused(&bot, serde_json::json!({ "key": f.alice_key.1 })).0,
        403
    );
    assert_eq!(
        face(
            "GET",
            &format!("{api}/user/ssh_signing_keys"),
            &bot,
            serde_json::Value::Null
        ),
        Err(403)
    );

    let delete = |id: &serde_json::Value, token: &str| f.delete_key(id, token);
    assert_eq!(delete(&added["id"], &bob), 404);
    assert_eq!(delete(&serde_json::json!("x"), &f.alice), 404);
    assert_eq!(delete(&added["id"], &f.alice), 204);
    assert_eq!(delete(&added["id"], &f.alice), 404);
    assert_eq!(
        face_ok(
            "GET",
            &format!("{api}/user/ssh_signing_keys"),
            &f.alice,
            serde_json::Value::Null
        ),
        serde_json::json!([untitled])
    );
    let types: Vec<String> = f
        .inst
        .world
        .log_after(0, 1000)
        .unwrap()
        .iter()
        .map(|e| e.body.type_name().to_string())
        .filter(|t| t.starts_with("ssh_signing_key:"))
        .collect();
    assert_eq!(
        types,
        [
            "ssh_signing_key:created",
            "ssh_signing_key:created",
            "ssh_signing_key:deleted"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn repositories_and_accounts_read_by_id() {
    let f = fixture().await;
    let api = &f.api;
    let base = &f.inst.base;
    mint_actor(base, "ci-bot", "bot");
    mint_actor(base, "acme", "org");
    let get = |path: &str| {
        face(
            "GET",
            &format!("{api}{path}"),
            &f.alice,
            serde_json::Value::Null,
        )
    };

    let repo = get("/repos/alice/webapp").unwrap();
    assert_eq!(get(&format!("/repositories/{}", repo["id"])).unwrap(), repo);
    for missing in ["/repositories/999", "/repositories/x", "/repositories/-1"] {
        assert_eq!(get(missing), Err(404), "{missing}");
    }

    for login in ["alice", "ci-bot[bot]", "acme"] {
        let account = get(&format!("/users/{login}")).unwrap();
        assert_eq!(account["login"], login);
        assert_eq!(
            get(&format!("/user/{}", account["id"])).unwrap(),
            account,
            "{login}"
        );
    }
    assert_eq!(get("/user/1"), Err(404), "the system actor");
    assert_eq!(get("/user/999"), Err(404));

    // GitHub serves other endpoints beside an account id, so a segment that
    // is not one is surface Orrery does not serve, not a missing account.
    for unserved in ["keys", "gpg_keys", "orgs"] {
        let (status, body) = face_answer(
            "GET",
            &format!("{api}/user/{unserved}"),
            &f.alice,
            &[],
            None,
        );
        assert_eq!(status, 501, "{unserved}");
        assert_eq!(body["orrery"]["path"], format!("/api/v3/user/{unserved}"));
    }
}

/// Two worlds made by the same calls, each pushed the same signed commits,
/// record the same events (rule 5): the verdict a push stores reads nothing
/// but its commits and the world.
#[tokio::test(flavor = "multi_thread")]
async fn a_signed_push_replays_to_the_same_events() {
    let first = fixture().await;
    let second = fixture().await;
    let signed = first.branch("signed", "alice@corp.test", Some(&first.alice_key.0));
    for f in [&first, &second] {
        f.add_key(&first.alice_key.1);
        let url = format!(
            "{}/alice/webapp.git",
            f.inst
                .base
                .replace("http://", &format!("http://x-access-token:{}@", f.alice))
        );
        git(first.src(), first.clock, &["push", "-q", &url, "--all"]);
    }
    let events = |f: &Fixture| -> Vec<serde_json::Value> {
        f.inst
            .world
            .log_after(0, 10_000)
            .unwrap()
            .into_iter()
            .map(|e| {
                let mut e = serde_json::to_value(e).unwrap();
                e.as_object_mut().unwrap().remove("wall_unix_ms");
                e
            })
            .collect()
    };
    assert_eq!(events(&first), events(&second));
    assert_eq!(first.verdict(&signed).1, "valid");
    assert_eq!(second.verdict(&signed).1, "valid");
}
