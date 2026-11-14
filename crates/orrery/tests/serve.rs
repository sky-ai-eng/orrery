//! `orrery serve` the way a supervisor runs it: the real binary, a generated
//! config, and its ready line read from stdout to learn where it listens.

mod common;

use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{
    Ready, Served, git, orrery_serve, orrery_serve_passing, pass_sockets, refused, request, serve,
    start, write_config,
};
use sha2::{Digest as _, Sha256};

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

/// Listening sockets bound at `dir/<name>.sock`, as a supervisor makes them
/// to pass to `serve`, with their paths.
fn bind_sockets(dir: &Path, names: &[&str]) -> Vec<(UnixListener, PathBuf)> {
    names
        .iter()
        .map(|name| {
            let path = dir.join(format!("{name}.sock"));
            (UnixListener::bind(&path).unwrap(), path)
        })
        .collect()
}

/// A config whose faces and control plane listen on the passed sockets
/// `faces` and `control`.
fn passed_config(dir: &Path) -> PathBuf {
    write_config(dir, "", PASSED_SERVER)
}

/// The `[server]` lines of [`passed_config`].
const PASSED_SERVER: &str =
    "listen = \"fd:faces\"\ncontrol_listen = \"fd:control\"\nbase_url = \"http://world.test\"\n";

#[test]
fn split_listeners_serve_over_passed_sockets() {
    let dir = tempfile::tempdir().unwrap();
    let sockets = bind_sockets(dir.path(), &["faces", "control"]);
    let fds: Vec<_> = sockets.iter().map(|(s, _)| s.as_raw_fd()).collect();
    let config = passed_config(dir.path());
    let (_served, ready) = start(orrery_serve_passing(&config, &fds, "faces:control"));
    assert_eq!(
        ready,
        Ready {
            base_url: "http://world.test".to_string(),
            listen: format!("unix:{}", sockets[0].1.display()),
            control: format!("unix:{}", sockets[1].1.display()),
        }
    );

    let (status, body) = request(&ready.control, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 200, "{body}");
    let (status, body) = request(&ready.listen, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 501, "{body}");
    assert!(body.contains("\"not_mocked\":true"), "{body}");
}

/// A supervisor that holds a world's sockets keeps them open across its
/// restart: a connection made while no `serve` runs waits in the socket's
/// backlog, and the next `serve` answers it.
#[test]
fn a_passed_socket_holds_connections_while_no_serve_runs() {
    let dir = tempfile::tempdir().unwrap();
    let sockets = bind_sockets(dir.path(), &["faces", "control"]);
    let fds: Vec<_> = sockets.iter().map(|(s, _)| s.as_raw_fd()).collect();
    let config = passed_config(dir.path());
    let (mut first, _) = start(orrery_serve_passing(&config, &fds, "faces:control"));
    first.signal("TERM");
    let (status, _, stderr) = first.exited(Duration::from_secs(10));
    assert!(status.success(), "{status}: {stderr}");
    for (_, path) in &sockets {
        let meta =
            std::fs::symlink_metadata(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(meta.file_type().is_socket(), "{}", path.display());
    }

    let mut waiting = UnixStream::connect(&sockets[1].1).unwrap();
    waiting
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        waiting,
        "GET /_orrery/healthz HTTP/1.1\r\nhost: world.test\r\nconnection: close\r\n\r\n"
    )
    .unwrap();
    let (_second, _) = start(orrery_serve_passing(&config, &fds, "faces:control"));
    let mut response = String::new();
    waiting.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

/// A passed socket `serve` can't use, or two listeners on one socket, stops
/// the world before its ready line, with an error naming the listener.
#[test]
fn an_unusable_passed_socket_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let sockets = bind_sockets(dir.path(), &["faces"]);
    let faces = sockets[0].0.as_raw_fd();
    let faces_path = format!("unix:{}", sockets[0].1.display());
    let (connected, _peer) = UnixStream::pair().unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    // Each case serves an instance of its own, so none reads another's config.
    let config = |case: &str, server: &str| {
        let case = dir.path().join(case);
        std::fs::create_dir(&case).unwrap();
        write_config(&case, "", server)
    };
    let faces_only = "listen = \"fd:faces\"\nbase_url = \"http://world.test\"\n";

    let parent = std::process::id().to_string();
    let mut another_pid = orrery_serve(
        &config("another-pid", faces_only),
        &[("LISTEN_PID", &parent)],
    );
    pass_sockets(&mut another_pid, &[faces], "faces");
    // Each case: the command, the listener its error names, and the cause.
    let cases = [
        (another_pid, "fd:faces", "names another process"),
        (
            orrery_serve_passing(&config("unnamed", faces_only), &[faces], "other"),
            "fd:faces",
            "does not name it",
        ),
        (
            orrery_serve_passing(
                &config("named-twice", faces_only),
                &[faces, faces],
                "faces:faces",
            ),
            "fd:faces",
            "names more than one descriptor",
        ),
        (
            orrery_serve_passing(&config("past-count", faces_only), &[faces], "other:faces"),
            "fd:faces",
            "names it in position 2, but LISTEN_FDS is 1",
        ),
        (
            orrery_serve_passing(&config("tcp", faces_only), &[tcp.as_raw_fd()], "faces"),
            "fd:faces",
            "is not a Unix socket",
        ),
        (
            orrery_serve_passing(
                &config("not-listening", faces_only),
                &[connected.as_raw_fd()],
                "faces",
            ),
            "fd:faces",
            "is not listening",
        ),
        (
            orrery_serve_passing(
                &config(
                    "one-socket",
                    "listen = \"fd:faces\"\ncontrol_listen = \"fd:faces\"\nbase_url = \"http://world.test\"\n",
                ),
                &[faces],
                "faces",
            ),
            "fd:faces",
            "both name fd:faces",
        ),
        (
            orrery_serve_passing(
                &config("passed-twice", PASSED_SERVER),
                &[faces, faces],
                "faces:control",
            ),
            &faces_path,
            "each needs a socket of its own",
        ),
        (
            orrery_serve_passing(
                &config(
                    "empty-name",
                    "listen = \"fd:\"\nbase_url = \"http://world.test\"\n",
                ),
                &[faces],
                "faces",
            ),
            "\"fd:\"",
            "a passed socket's name is 1 to 255 characters",
        ),
        (
            orrery_serve_passing(
                &config(
                    "colon",
                    "listen = \"fd:faces:control\"\nbase_url = \"http://world.test\"\n",
                ),
                &[faces],
                "faces",
            ),
            "\"fd:faces:control\"",
            "a passed socket's name is 1 to 255 characters",
        ),
    ];
    for (cmd, listener, cause) in cases {
        let out = refused(cmd);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{cause}: {stderr}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "", "{cause}");
        assert!(stderr.contains(listener), "{cause}: {stderr}");
        assert!(stderr.contains(cause), "{cause}: {stderr}");
    }
}

/// A git process holding a passed socket would keep it open after the world
/// stops, and anything it runs could accept the connections meant for the
/// world's next `serve`. So every passed descriptor is marked close-on-exec,
/// the ones the config doesn't name and the unnamed one among them. The faces
/// listen on TCP, since git cannot push over a Unix socket.
#[cfg(target_os = "linux")]
#[test]
fn no_git_process_holds_a_passed_socket() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let sockets = bind_sockets(dir.path(), &["control", "spare", "unnamed"]);
    let fds: Vec<_> = sockets.iter().map(|(s, _)| s.as_raw_fd()).collect();
    let config = write_config(
        dir.path(),
        "",
        "listen = \"127.0.0.1:0\"\ncontrol_listen = \"fd:control\"\n",
    );
    let (_served, ready) = start(orrery_serve_passing(&config, &fds, "control:spare"));
    let token = common::mint(&ready.control, "alice");
    common::call(
        &ready.listen,
        "POST",
        "/api/v3/user/repos",
        Some(&token),
        r#"{"name":"webapp"}"#,
        201,
    );

    let held = dir.path().join("held");
    let hooks = dir
        .path()
        .join("instance/world/repos/alice/webapp.git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-receive");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nfor fd in /proc/$$/fd/*; do readlink \"$fd\"; done > '{}'\n",
            held.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main", "."]);
    git(&src, &["commit", "-q", "--allow-empty", "-m", "first"]);
    let remote = format!(
        "http://x-access-token:{token}@{}/alice/webapp.git",
        ready.listen
    );
    git(&src, &["push", "-q", &remote, "main"]);

    let held = std::fs::read_to_string(&held).unwrap();
    assert!(!held.is_empty(), "the hook listed no descriptors");
    for (socket, path) in &sockets {
        let target = std::fs::read_link(format!("/proc/self/fd/{}", socket.as_raw_fd())).unwrap();
        let target = target.to_str().unwrap();
        assert!(target.starts_with("socket:["), "{target}");
        assert!(
            !held.lines().any(|line| line == target),
            "the hook holds {} ({target}):\n{held}",
            path.display()
        );
    }
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

/// `serve` reads its config once, so a supervisor that rewrites a world's
/// config learns whether the running process has read the new file by
/// comparing healthz's digest with the file's.
#[test]
fn healthz_reports_the_digest_of_the_config_serve_read() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        dir.path(),
        "",
        "# a comment is part of the file's bytes\nlisten = \"127.0.0.1:0\"\n",
    );
    let read = std::fs::read(&config).unwrap();
    let digest = format!("{:x}", Sha256::digest(&read));
    let (_served, ready) = serve(&config, &[]);
    let health = |control: &str| {
        let (status, body) = request(control, "GET", "/_orrery/healthz", None, "");
        assert_eq!(status, 200, "{body}");
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["config_sha256"].clone()
    };
    assert_eq!(health(&ready.control), digest.as_str());
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "{digest}"
    );

    std::fs::write(&config, [&read[..], b"# rewritten\n"].concat()).unwrap();
    assert_ne!(
        format!("{:x}", Sha256::digest(std::fs::read(&config).unwrap())),
        digest
    );
    assert_eq!(health(&ready.control), digest.as_str());
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

/// What a world answers for pull request `number` that its git works out:
/// the diff media type, the line and file counts GraphQL serves, and the
/// REST files with theirs.
#[derive(Debug, PartialEq)]
struct PullReads {
    diff: String,
    counts: serde_json::Value,
    files: serde_json::Value,
}

fn pull_reads(addr: &str, token: &str, number: u64) -> PullReads {
    let path = format!("/api/v3/repos/alice/webapp/pulls/{number}");
    let (status, diff) = common::request_accepting(
        addr,
        "GET",
        &path,
        Some(token),
        Some("application/vnd.github.v3.diff"),
        "",
    );
    assert_eq!(status, 200, "{diff}");
    let pull = common::call(addr, "GET", &path, Some(token), "", 200);
    let query = serde_json::json!({
        "query": "query($id: ID!) { node(id: $id) { ... on PullRequest { additions deletions changedFiles } } }",
        "variables": { "id": pull["node_id"] },
    });
    let answer = common::call(
        addr,
        "POST",
        "/api/graphql",
        Some(token),
        &query.to_string(),
        200,
    );
    assert!(answer.get("errors").is_none(), "{answer}");
    PullReads {
        diff,
        counts: answer["data"]["node"].clone(),
        files: common::call(addr, "GET", &format!("{path}/files"), Some(token), "", 200),
    }
}

/// The variables an operator's shell can carry that would each change a
/// world's answers if its git saw them: config hiding every branch from a
/// fetch, an external diff, and a home whose attributes file marks every
/// file binary.
const HOSTILE_ENV: [&str; 5] = [
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_KEY_0",
    "GIT_CONFIG_VALUE_0",
    "GIT_EXTERNAL_DIFF",
    "HOME",
];

/// The world's git sees only the environment the engine gives it, so a
/// world answers alike whatever environment `serve` was started in: a clone
/// sees the branches, the diff is git's own, and the counts and files are
/// those the same world gives when served without those variables.
#[test]
fn the_worlds_git_takes_nothing_from_the_environment_serve_runs_in() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), "", "listen = \"127.0.0.1:0\"\n");
    let marker = "an external diff ran";
    let external_diff = dir.path().join("external-diff");
    std::fs::write(&external_diff, format!("#!/bin/sh\necho '{marker}'\n")).unwrap();
    std::fs::set_permissions(&external_diff, std::fs::Permissions::from_mode(0o755)).unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".config/git")).unwrap();
    std::fs::write(home.join(".config/git/attributes"), "* -diff\n").unwrap();
    let values = [
        "1",
        "uploadpack.hideRefs",
        "refs/heads",
        external_diff.to_str().unwrap(),
        home.to_str().unwrap(),
    ];
    let hostile: Vec<_> = HOSTILE_ENV.into_iter().zip(values).collect();
    let mut cmd = orrery_serve(&config, &hostile);
    // Unset, so git would look for the attributes file under `HOME`.
    cmd.env_remove("XDG_CONFIG_HOME");
    let (served, ready) = start(cmd);

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
    std::fs::write(src.join("a.txt"), "one\ntwo\n").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-q", "-m", "first"]);
    git(&src, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(src.join("a.txt"), "one\nthree\n").unwrap();
    std::fs::write(src.join("b.txt"), "new\n").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-q", "-m", "change"]);
    let remote = format!(
        "http://x-access-token:{token}@{}/alice/webapp.git",
        ready.listen
    );
    git(&src, &["push", "-q", &remote, "main", "topic"]);
    let pull = common::call(
        &ready.listen,
        "POST",
        "/api/v3/repos/alice/webapp/pulls",
        Some(&token),
        r#"{"title":"topic","head":"topic","base":"main"}"#,
        201,
    );
    let number = pull["number"].as_u64().unwrap();

    git(dir.path(), &["clone", "-q", &remote, "clone"]);
    let branches = git(
        &dir.path().join("clone"),
        &["for-each-ref", "--format=%(refname)", "refs/remotes/origin"],
    );
    for branch in ["main", "topic"] {
        let name = format!("refs/remotes/origin/{branch}");
        assert!(branches.lines().any(|b| b == name), "{branches}");
    }
    let reads = pull_reads(&ready.listen, &token, number);
    assert!(!reads.diff.contains(marker), "{}", reads.diff);
    drop(served);

    let mut cmd = orrery_serve(&config, &[]);
    for var in HOSTILE_ENV {
        cmd.env_remove(var);
    }
    let (_served, ready) = start(cmd);
    let clean = pull_reads(&ready.listen, &token, number);
    assert_eq!(
        clean.counts,
        serde_json::json!({ "additions": 2, "deletions": 1, "changedFiles": 2 })
    );
    assert_eq!(reads, clean);
}

/// A world given egress through a proxy starts as any other: the proxy is
/// reached only when there is something to send, so it need not be up yet.
#[test]
fn a_world_starts_with_egress_through_a_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        dir.path(),
        "",
        &format!(
            "listen = \"127.0.0.1:0\"\n[outbound]\negress = \"unix:{}\"\n",
            dir.path().join("egress.sock").display()
        ),
    );
    let (_served, ready) = serve(&config, &[]);
    let (status, _) = request(&ready.listen, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 200);
}

/// A world told to send nothing starts as any other, and `send` takes a
/// boolean alone.
#[test]
fn a_world_starts_that_sends_no_deliveries() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        dir.path(),
        "",
        "listen = \"127.0.0.1:0\"\n[outbound]\nsend = false\n",
    );
    let (_served, ready) = serve(&config, &[]);
    let (status, _) = request(&ready.listen, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 200);
}

/// An outbound setting the client can't use stops `serve` before it makes
/// the world, so fixing the config and starting again is all it takes.
#[test]
fn an_unusable_outbound_setting_stops_serve_before_the_world_is_made() {
    for (case, outbound, expected) in [
        ("name", "egress = \"localhost:3128\"", "\"localhost:3128\""),
        (
            "relative",
            "egress = \"unix:egress.sock\"",
            "must be absolute",
        ),
        (
            "missing",
            "extra_ca_certs = \"/nonexistent/ca.pem\"",
            "/nonexistent/ca.pem",
        ),
        ("unknown", "egres = \"direct\"", "unknown field `egres`"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(
            dir.path(),
            "",
            &format!("listen = \"127.0.0.1:0\"\n[outbound]\n{outbound}\n"),
        );
        let out = orrery_serve(&config, &[]).output().unwrap();
        assert!(!out.status.success(), "{case}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(expected), "{case}: {stderr}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "", "{case}");
        assert!(!dir.path().join("instance/world").exists(), "{case}");
    }
}
