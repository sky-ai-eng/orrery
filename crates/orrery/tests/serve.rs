//! `orrery serve` the way a supervisor runs it: the real binary, a generated
//! config, and Unix sockets for its listeners.

mod common;

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use common::{Served, orrery_serve, request, serve, write_config};

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
    let _served = serve(&config, &[], &[&faces, &control]);

    let (status, body) = request(&control, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 200, "{body}");
    let (status, body) = request(&faces, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 501, "{body}");
    assert!(body.contains("\"not_mocked\":true"), "{body}");

    let (status, body) = request(
        &control,
        "POST",
        "/_orrery/actors",
        None,
        r#"{"login":"alice","kind":"user"}"#,
    );
    assert_eq!(status, 201, "{body}");
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let token = created["token"].as_str().unwrap();
    let (status, body) = request(&faces, "GET", "/api/v3/user", Some(token), "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"login\":\"alice\""), "{body}");
    let (status, body) = request(&control, "GET", "/api/v3/user", Some(token), "");
    assert_eq!(status, 501, "{body}");
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
    let _first = serve(&config, &[], &[&socket]);
    let second = orrery_serve(&config, &[]).output().unwrap();
    assert!(!second.status.success());
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("in use by another process"), "{stderr}");
    let (status, _) = request(&socket, "GET", "/_orrery/healthz", None, "");
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
    let served = serve(&config, &[("TOKIO_WORKER_THREADS", "2")], &[&socket]);
    let threads = std::fs::read_dir(format!("/proc/{}/task", served.0.id()))
        .unwrap()
        .count();
    assert_eq!(threads, 3, "the main thread and two workers");
}

/// Serve a fresh instance whose `[instance]` table adds `instance`, and
/// return the token it mints for alice, its socket and the running server.
fn alice_token(dir: &Path, instance: &str) -> (String, PathBuf, Served) {
    let (socket, served) = common::serve_instance(dir, instance);
    let token = common::mint(&socket, "alice");
    (token, socket, served)
}

#[test]
fn a_random_secret_makes_each_new_worlds_tokens_unguessable() {
    let dirs: Vec<_> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let (default, _, _) = alice_token(dirs[0].path(), "");
    let (local, _, _) = alice_token(dirs[1].path(), "secret = \"local\"\n");
    let (random, socket, served) = alice_token(dirs[2].path(), "secret = \"random\"\n");
    let (other_random, _, _) = alice_token(dirs[3].path(), "secret = \"random\"\n");

    // Local worlds share the well-known secret, so they mint the same token.
    assert_eq!(default, local);
    assert_ne!(random, local);
    assert_ne!(random, other_random);

    // The secret is the world's, not the config's: a restart keeps it, so
    // the token alice already holds still works.
    drop(served);
    let config = dirs[2].path().join("orrery.toml");
    let _served = serve(&config, &[], &[&socket]);
    let (status, body) = request(&socket, "GET", "/api/v3/user", Some(&random), "");
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
