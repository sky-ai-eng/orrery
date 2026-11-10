//! `orrery serve` the way a supervisor runs it: the real binary, a generated
//! config, and its ready line read from stdout to learn where it listens.

mod common;

use std::os::unix::net::UnixListener;
use std::path::Path;

use common::{Ready, Served, git, orrery_serve, request, serve, write_config};

#[test]
fn split_listeners_serve_over_unix_sockets() {
    let dir = tempfile::tempdir().unwrap();
    let faces = dir.path().join("faces.sock");
    let control = dir.path().join("control.sock");
    // A killed world process leaves its socket file behind.
    drop(UnixListener::bind(&faces).unwrap());
    assert!(faces.exists());
    let config = write_config(
        dir.path(),
        "",
        &format!(
            "listen = \"unix:{}\"\ncontrol_listen = \"unix:{}\"\nbase_url = \"http://world.test\"\n",
            faces.display(),
            control.display()
        ),
    );
    let (_served, ready) = serve(&config, &[]);
    assert_eq!(
        ready,
        Ready {
            base_url: "http://world.test".to_string(),
            listen: format!("unix:{}", faces.display()),
            control: format!("unix:{}", control.display()),
        }
    );
    let (faces, control) = (&ready.listen, &ready.control);

    let (status, body) = request(control, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 200, "{body}");
    let (status, body) = request(faces, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 501, "{body}");
    assert!(body.contains("\"not_mocked\":true"), "{body}");

    let (status, body) = request(
        control,
        "POST",
        "/_orrery/actors",
        None,
        r#"{"login":"alice","kind":"user"}"#,
    );
    assert_eq!(status, 201, "{body}");
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let token = created["token"].as_str().unwrap();
    let (status, body) = request(faces, "GET", "/api/v3/user", Some(token), "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"login\":\"alice\""), "{body}");
    let (status, body) = request(control, "GET", "/api/v3/user", Some(token), "");
    assert_eq!(status, 501, "{body}");
}

/// Port 0 lets a caller start many worlds without picking free ports
/// itself; the ready line is how it learns the ports they got.
#[test]
fn the_ready_line_reports_the_port_bound_for_port_zero() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), "", "listen = \"127.0.0.1:0\"\n");
    let (_served, ready) = serve(&config, &[]);
    let port: u16 = ready
        .listen
        .strip_prefix("127.0.0.1:")
        .unwrap_or_else(|| panic!("{ready:?}"))
        .parse()
        .unwrap();
    assert_ne!(port, 0);
    assert_eq!(ready.base_url, format!("http://127.0.0.1:{port}"));
    assert_eq!(ready.control, ready.listen);

    let host = ready.base_url.strip_prefix("http://").unwrap();
    let (status, body) = request(host, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 200, "{body}");
}

#[test]
fn the_ready_line_reports_a_control_socket() {
    let dir = tempfile::tempdir().unwrap();
    let control = dir.path().join("control.sock");
    let config = write_config(
        dir.path(),
        "",
        &format!(
            "listen = \"127.0.0.1:0\"\ncontrol_listen = \"unix:{}\"\n",
            control.display()
        ),
    );
    let (_served, ready) = serve(&config, &[]);
    assert_eq!(ready.control, format!("unix:{}", control.display()));
    assert_eq!(ready.base_url, format!("http://{}", ready.listen));

    let (status, body) = request(&ready.control, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 200, "{body}");
    let (status, body) = request(&ready.listen, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 501, "{body}");
}

#[test]
fn nothing_but_the_ready_line_reaches_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), "", "listen = \"127.0.0.1:0\"\n");
    let (mut served, ready) = serve(&config, &[("RUST_LOG", "debug")]);
    // Traffic that logs: a control-plane write and unmocked surface.
    let (status, body) = request(
        &ready.control,
        "POST",
        "/_orrery/actors",
        None,
        r#"{"login":"alice","kind":"user"}"#,
    );
    assert_eq!(status, 201, "{body}");
    let (status, body) = request(&ready.listen, "GET", "/not/mocked", None, "");
    assert_eq!(status, 501, "{body}");

    let (stdout, stderr) = served.stop();
    assert_eq!(stdout, "", "stdout after the ready line");
    assert!(stderr.contains("orrery serving"), "{stderr}");
    assert!(stderr.contains("NOT MOCKED"), "{stderr}");
}

/// A caller that reads a ready line may send traffic to every address in it,
/// so a listener that fails to bind must stop the world before it prints one.
#[test]
fn no_ready_line_until_every_listener_is_bound() {
    let dir = tempfile::tempdir().unwrap();
    let control = dir.path().join("control.sock");
    std::fs::write(&control, "not a socket").unwrap();
    let config = write_config(
        dir.path(),
        "",
        &format!(
            "listen = \"127.0.0.1:0\"\ncontrol_listen = \"unix:{}\"\n",
            control.display()
        ),
    );
    let out = orrery_serve(&config, &[]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("exists and is not a socket"), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

#[test]
fn a_live_socket_is_not_taken_over() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("faces.sock");
    let config = write_config(
        dir.path(),
        "",
        &format!(
            "listen = \"unix:{}\"\nbase_url = \"http://world.test\"\n",
            socket.display()
        ),
    );
    let (_first, ready) = serve(&config, &[]);
    // A ref the serving process has moved and not yet recorded, as
    // receive-pack leaves one partway through a push.
    let token = common::mint(&ready.listen, "alice");
    common::call(
        &ready.listen,
        "POST",
        "/api/v3/user/repos",
        Some(&token),
        r#"{"name":"webapp"}"#,
        201,
    );
    let bare = dir.path().join("instance/world/repos/alice/webapp.git");
    let tree = git(&bare, &["mktree"]);
    let pushed = git(&bare, &["commit-tree", &tree, "-m", "in flight"]);
    git(&bare, &["update-ref", "refs/heads/topic", &pushed]);

    let second = orrery_serve(&config, &[]).output().unwrap();
    assert!(!second.status.success());
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("in use by another process"), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&second.stdout), "");
    assert_eq!(git(&bare, &["rev-parse", "refs/heads/topic"]), pushed);
    let (status, _) = request(&ready.listen, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 200);
}

#[test]
fn a_socket_listener_needs_an_explicit_base_url() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        dir.path(),
        "",
        &format!(
            "listen = \"unix:{}\"\n",
            dir.path().join("faces.sock").display()
        ),
    );
    let out = orrery_serve(&config, &[]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("server.base_url must be set"), "{stderr}");
}

/// A supervisor running many worlds starts each with
/// `TOKIO_WORKER_THREADS=2`. `#[tokio::main]` reads it with no code of
/// ours, which is exactly what a change to the runtime setup could drop.
#[cfg(target_os = "linux")]
#[test]
fn tokio_worker_threads_sizes_the_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("faces.sock");
    let config = write_config(
        dir.path(),
        "",
        &format!(
            "listen = \"unix:{}\"\nbase_url = \"http://world.test\"\n",
            socket.display()
        ),
    );
    let (served, _) = serve(&config, &[("TOKIO_WORKER_THREADS", "2")]);
    let threads = std::fs::read_dir(format!("/proc/{}/task", served.child.id()))
        .unwrap()
        .count();
    assert_eq!(threads, 3, "the main thread and two workers");
}

/// Serve a fresh instance whose `[instance]` table adds `instance`, and
/// return the token it mints for alice and the running server.
fn alice_token(dir: &Path, instance: &str) -> (String, Served) {
    let (addr, served) = common::serve_instance(dir, instance);
    (common::mint(&addr, "alice"), served)
}

#[test]
fn a_random_secret_makes_each_new_worlds_tokens_unguessable() {
    let dirs: Vec<_> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let (default, _) = alice_token(dirs[0].path(), "");
    let (local, _) = alice_token(dirs[1].path(), "secret = \"local\"\n");
    let (random, served) = alice_token(dirs[2].path(), "secret = \"random\"\n");
    let (other_random, _) = alice_token(dirs[3].path(), "secret = \"random\"\n");

    // Local worlds share the well-known secret, so they mint the same token.
    assert_eq!(default, local);
    assert_ne!(random, local);
    assert_ne!(random, other_random);

    // The secret is the world's, not the config's: a restart keeps it, so
    // the token alice already holds still works.
    drop(served);
    let config = dirs[2].path().join("orrery.toml");
    let (_served, ready) = serve(&config, &[]);
    let (status, body) = request(&ready.listen, "GET", "/api/v3/user", Some(&random), "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"login\":\"alice\""), "{body}");
}

#[test]
fn an_unknown_secret_choice_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        dir.path(),
        "secret = \"sometimes\"\n",
        "listen = \"127.0.0.1:0\"\n",
    );
    let out = orrery_serve(&config, &[]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown variant `sometimes`"), "{stderr}");
    assert!(
        !dir.path().join("instance").exists(),
        "no world was created"
    );
}

/// A branch moved and another created in the bare repository while the
/// world is stopped, as a push cut off by its process dying leaves them, are
/// put back when the world is next served. The next push of the branch is
/// recorded moving it from where the log left it.
#[test]
fn start_up_puts_back_refs_the_log_does_not_describe() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), "", "listen = \"127.0.0.1:0\"\n");
    let (served, ready) = serve(&config, &[]);
    let token = common::mint(&ready.control, "alice");
    common::call(
        &ready.listen,
        "POST",
        "/api/v3/user/repos",
        Some(&token),
        r#"{"name":"webapp"}"#,
        201,
    );
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main", "."]);
    git(&src, &["commit", "-q", "--allow-empty", "-m", "first"]);
    let first = git(&src, &["rev-parse", "HEAD"]);
    let remote = format!(
        "http://x-access-token:{token}@{}/alice/webapp.git",
        ready.listen
    );
    git(&src, &["push", "-q", &remote, "main"]);
    drop(served);

    let bare = dir.path().join("instance/world/repos/alice/webapp.git");
    let tree = git(&bare, &["rev-parse", "main^{tree}"]);
    let unlogged = git(
        &bare,
        &["commit-tree", &tree, "-p", &first, "-m", "unlogged"],
    );
    git(&bare, &["update-ref", "refs/heads/main", &unlogged]);
    git(&bare, &["update-ref", "refs/heads/stray", &unlogged]);

    let (mut served, ready) = serve(&config, &[]);
    let refs = git(
        &bare,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    );
    assert_eq!(refs, format!("refs/heads/main {first}"));

    git(&src, &["commit", "-q", "--allow-empty", "-m", "second"]);
    let second = git(&src, &["rev-parse", "HEAD"]);
    let remote = format!(
        "http://x-access-token:{token}@{}/alice/webapp.git",
        ready.listen
    );
    git(&src, &["push", "-q", &remote, "main"]);
    let log = common::call(
        &ready.control,
        "POST",
        "/_orrery/log/list",
        None,
        r#"{"sort_key":"seq","sort_dir":"desc"}"#,
        200,
    );
    let moved = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "ref:updated")
        .unwrap_or_else(|| panic!("{log}"));
    assert_eq!(moved["subject"]["name"], "refs/heads/main");
    assert_eq!(moved["changes"]["sha"]["from"], first.as_str());
    assert_eq!(moved["changes"]["sha"]["to"], second.as_str());

    let (_, stderr) = served.stop();
    assert!(stderr.contains("refs/heads/stray"), "{stderr}");
}
