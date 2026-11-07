//! `orrery create` and `orrery fork` the way a supervisor runs them: the real
//! binary with `--json`, then `orrery serve` on what they made.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::{call, mint, orrery_json, request, serve_instance};

/// 2026-03-04T05:06:07Z.
const T0: i64 = 1_772_600_767;
/// 2020-01-01T00:00:00Z, a world's T0 when none is given.
const DEFAULT_T0: i64 = 1_577_836_800;

/// The success object names absolute paths, whatever the caller passed.
fn assert_made(made: &serde_json::Value, instance: &Path, clock_unix: i64) {
    let reported = PathBuf::from(made["instance"].as_str().unwrap());
    assert!(reported.is_absolute(), "{made}");
    assert_eq!(
        reported.canonicalize().unwrap(),
        instance.canonicalize().unwrap()
    );
    assert_eq!(
        made["world"].as_str().unwrap(),
        reported.join("world").to_str().unwrap()
    );
    assert_eq!(made["clock_unix"], clock_unix, "{made}");
    assert_eq!(made.as_object().unwrap().len(), 3, "{made}");
}

fn assert_failed(outcome: (i32, serde_json::Value), reason: &str, field: Option<&str>) {
    let (code, body) = outcome;
    assert_eq!(code, 1, "{body}");
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{body}");
    assert_eq!(errors[0]["reason"], reason, "{body}");
    assert!(!errors[0]["message"].as_str().unwrap().is_empty(), "{body}");
    assert_eq!(errors[0]["field"].as_str(), field, "{body}");
}

/// Every file under `dir`, by path, with its bytes.
fn contents(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
    }
    files
}

#[test]
fn a_created_world_serves_from_t0_as_one_serve_created() {
    let created = tempfile::tempdir().unwrap();
    // A relative `--instance`, so the reported path is made absolute.
    let (code, made) = orrery_json(
        created.path(),
        &[
            "create",
            "--instance",
            "instance",
            "--t0",
            "2026-03-04T05:06:07Z",
        ],
    );
    assert_eq!(code, 0, "{made}");
    assert_made(&made, &created.path().join("instance"), T0);
    let made: Vec<_> = std::fs::read_dir(created.path().join("instance"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        made,
        ["world"],
        "create writes the world and nothing beside it"
    );

    // The config names no t0: the clock comes from the world.
    let (socket, _served) = serve_instance(created.path(), "");
    let health = call(&socket, "GET", "/_orrery/healthz", None, "", 200);
    assert_eq!(health["clock_unix"], T0, "{health}");
    let token = mint(&socket, "alice");
    let log = call(&socket, "POST", "/_orrery/log/list", None, "{}", 200);

    // `serve` creating the same world on first run makes the same one.
    let first_run = tempfile::tempdir().unwrap();
    let (socket, _served) = serve_instance(first_run.path(), "t0 = \"2026-03-04T05:06:07Z\"\n");
    assert_eq!(mint(&socket, "alice"), token);
    assert_eq!(
        call(&socket, "POST", "/_orrery/log/list", None, "{}", 200),
        log
    );
}

#[test]
fn a_random_secret_gives_each_created_world_its_own_tokens() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    for dir in &dirs {
        let instance = dir.path().join("instance");
        let (code, made) = orrery_json(
            dir.path(),
            &[
                "create",
                "--instance",
                instance.to_str().unwrap(),
                "--secret",
                "random",
            ],
        );
        assert_eq!(code, 0, "{made}");
        assert_made(&made, &instance, DEFAULT_T0);
    }

    let (socket, served) = serve_instance(dirs[0].path(), "");
    let first = mint(&socket, "alice");
    drop(served);

    let (socket, _served) = serve_instance(dirs[1].path(), "");
    let second = mint(&socket, "alice");
    assert_ne!(first, second);
    let (status, body) = request(&socket, "GET", "/api/v3/user", Some(&first), "");
    assert_eq!(status, 401, "{body}");
    let user = call(&socket, "GET", "/api/v3/user", Some(&second), "", 200);
    assert_eq!(user["login"], "alice");
}

#[test]
fn fork_world_copies_a_stopped_instance_and_never_writes_it() {
    let source = tempfile::tempdir().unwrap();
    let (socket, served) = serve_instance(source.path(), "");
    let token = mint(&socket, "alice");
    call(
        &socket,
        "POST",
        "/_orrery/clock/advance",
        None,
        r#"{"seconds":3600}"#,
        200,
    );
    drop(served);
    let world = source.path().join("instance/world");
    let before = contents(&world);

    // Under the local secret, as its source was, the copy keeps alice's token.
    let local = tempfile::tempdir().unwrap();
    let (code, made) = orrery_json(
        local.path(),
        &[
            "fork",
            "--world",
            world.to_str().unwrap(),
            "--dest",
            "instance",
        ],
    );
    assert_eq!(code, 0, "{made}");
    assert_made(&made, &local.path().join("instance"), DEFAULT_T0 + 3600);
    let (socket, _served) = serve_instance(local.path(), "");
    let health = call(&socket, "GET", "/_orrery/healthz", None, "", 200);
    assert_eq!(health["clock_unix"], DEFAULT_T0 + 3600, "{health}");
    let user = call(&socket, "GET", "/api/v3/user", Some(&token), "", 200);
    assert_eq!(user["login"], "alice");

    // Re-keyed under a random secret, the copy accepts none of the source's
    // tokens, and a token issued there works. The source is named through a
    // symlink, as a template's current version might be: the fork is a copy
    // of what it points at, and re-keying it leaves the source alone.
    let template = source.path().join("template");
    std::os::unix::fs::symlink(&world, &template).unwrap();
    let random = tempfile::tempdir().unwrap();
    let (code, made) = orrery_json(
        random.path(),
        &[
            "fork",
            "--world",
            template.to_str().unwrap(),
            "--dest",
            "instance",
            "--secret",
            "random",
        ],
    );
    assert_eq!(code, 0, "{made}");
    assert!(!random.path().join("instance/world").is_symlink());
    let (socket, _served) = serve_instance(random.path(), "");
    let (status, body) = request(&socket, "GET", "/api/v3/user", Some(&token), "");
    assert_eq!(status, 401, "{body}");
    let issued = call(
        &socket,
        "POST",
        "/_orrery/actors/alice/tokens",
        None,
        "",
        201,
    );
    let issued = issued["token"].as_str().unwrap();
    assert_ne!(issued, token);
    let user = call(&socket, "GET", "/api/v3/user", Some(issued), "", 200);
    assert_eq!(user["login"], "alice");

    assert!(before == contents(&world), "a fork wrote its source");
}

#[test]
fn a_snapshot_forks_while_its_instance_serves() {
    let source = tempfile::tempdir().unwrap();
    let (socket, _served) = serve_instance(source.path(), "");
    let token = mint(&socket, "alice");
    call(
        &socket,
        "POST",
        "/_orrery/snapshots",
        None,
        r#"{"name":"seed"}"#,
        201,
    );

    let dest = tempfile::tempdir().unwrap();
    let (code, made) = orrery_json(
        dest.path(),
        &[
            "fork",
            "--instance",
            source.path().join("instance").to_str().unwrap(),
            "--snapshot",
            "seed",
            "--dest",
            "instance",
        ],
    );
    assert_eq!(code, 0, "{made}");
    assert_made(&made, &dest.path().join("instance"), DEFAULT_T0);
    let (socket, _served) = serve_instance(dest.path(), "");
    let user = call(&socket, "GET", "/api/v3/user", Some(&token), "", 200);
    assert_eq!(user["login"], "alice");
}

#[test]
fn each_json_failure_names_its_reason() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();

    assert_failed(
        orrery_json(cwd, &["create", "--instance", "bad-t0", "--t0", "tomorrow"]),
        "INVALID_ARGUMENT",
        Some("t0"),
    );
    assert!(
        !cwd.join("bad-t0").exists(),
        "a refused create made nothing"
    );
    assert_failed(
        orrery_json(
            cwd,
            &[
                "fork",
                "--instance",
                "src",
                "--snapshot",
                "../escape",
                "--dest",
                "dest",
            ],
        ),
        "INVALID_ARGUMENT",
        Some("snapshot"),
    );

    assert_failed(
        orrery_json(
            cwd,
            &[
                "fork",
                "--instance",
                "src",
                "--snapshot",
                "missing",
                "--dest",
                "dest",
            ],
        ),
        "NOT_FOUND",
        Some("snapshot"),
    );
    assert_failed(
        orrery_json(cwd, &["fork", "--world", "missing", "--dest", "dest"]),
        "NOT_FOUND",
        Some("world"),
    );
    assert!(!cwd.join("dest").exists(), "a refused fork made nothing");

    let (code, made) = orrery_json(cwd, &["create", "--instance", "made"]);
    assert_eq!(code, 0, "{made}");
    assert_failed(
        orrery_json(cwd, &["create", "--instance", "made"]),
        "ALREADY_EXISTS",
        Some("instance"),
    );
    assert_failed(
        orrery_json(cwd, &["fork", "--world", "made/world", "--dest", "made"]),
        "ALREADY_EXISTS",
        Some("dest"),
    );

    // A file where the instance directory would go.
    std::fs::write(cwd.join("a-file"), "").unwrap();
    assert_failed(
        orrery_json(cwd, &["create", "--instance", "a-file"]),
        "INTERNAL",
        None,
    );

    // A copy that cannot be opened as a world fails to re-key, and leaves
    // nothing in its destination, so a retry from a sound source succeeds.
    std::fs::create_dir_all(cwd.join("broken/world")).unwrap();
    std::fs::write(cwd.join("broken/world/world.db"), "not a database").unwrap();
    assert_failed(
        orrery_json(cwd, &["fork", "--world", "broken/world", "--dest", "retry"]),
        "INTERNAL",
        None,
    );
    let left: Vec<_> = std::fs::read_dir(cwd.join("retry")).unwrap().collect();
    assert!(left.is_empty(), "a failed fork left {left:?}");
    let (code, made) = orrery_json(cwd, &["fork", "--world", "made/world", "--dest", "retry"]);
    assert_eq!(code, 0, "{made}");
}
