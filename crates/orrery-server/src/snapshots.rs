//! Snapshot, restore, and diff routes on the control plane — engine
//! operations reality has no API for (spec/usage.md — door two).
//!
//! Create, restore, delete, and diff all take the git face's world write gate
//! exclusively: no push, merge, or fetch is in flight while a world directory
//! is copied, swapped, or compared, so every one of these reads or writes an
//! exact moment. The snapshot reads take its read side, so they never see a
//! snapshot half made or half removed. All filesystem work runs on the
//! blocking pool with the gate guard moved into the task.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{Response, StatusCode};
use axum::routing::{get, post};
use orrery_snapshot::{DiffSide, SnapshotError};
use orrery_world::{WorldError, WorldView};

use crate::control::{Fault, Fields, errors, internal, keyset_page, page_size};
use crate::{AppState, json};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/snapshots", post(create))
        .route(
            "/snapshots/list",
            post(list)
                .get(get_one_named_list)
                .delete(delete_one_named_list),
        )
        .route("/snapshots/{name}", get(get_one).delete(delete_one))
        .route("/snapshots/{name}/restore", post(restore))
        .route("/diff", post(diff))
}

/// The name is checked with the rest of the body, before the gate is taken,
/// so a bad one is refused without waiting on a push in flight.
async fn create(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let name: Option<String> = fields.required("name");
    if let Some(name) = &name
        && let Err(e) = orrery_snapshot::validate_name(name)
    {
        fields.fault(Fault::invalid(e.to_string(), "name"));
    }
    let name = match fields.finish(name) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    let gate = state.git.gate();
    let guard = gate.write_owned().await;
    let world = state.world.clone();
    let dir = state.snapshots_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _hold = guard;
        orrery_snapshot::create(&world, &dir, &name)
    })
    .await;
    match flatten(result) {
        Ok(info) => json(StatusCode::CREATED, to_value(&info)),
        Err(e) => snapshot_error(e, Some("name")),
    }
}

async fn get_one(State(state): State<AppState>, UrlPath(name): UrlPath<String>) -> Response<Body> {
    read_one(&state, name).await
}

/// `list` is a valid snapshot name, but the list's static path takes
/// `GET /snapshots/list` before `/snapshots/{name}` can, so the point read
/// is routed here too.
async fn get_one_named_list(State(state): State<AppState>) -> Response<Body> {
    read_one(&state, "list".to_string()).await
}

async fn read_one(state: &AppState, name: String) -> Response<Body> {
    let guard = state.git.gate().read_owned().await;
    let dir = state.snapshots_dir.clone();
    let read_name = name.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _hold = guard;
        orrery_snapshot::info(&dir, &read_name)
    })
    .await;
    match flatten(result) {
        Ok(Some(info)) => json(StatusCode::OK, to_value(&info)),
        Ok(None) => not_found(&name),
        Err(e) => snapshot_error(e, Some("name")),
    }
}

async fn list(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    // TODO(ORR-58): pages by the visible `after_name` cursor with no
    // `total_count`; moves to `list::read_window` and `list::answer`.
    let mut fields = Fields::parse(&body);
    let page_size = page_size(&mut fields);
    let after_name: Option<String> = fields.optional("after_name");
    if let Some(after) = &after_name
        && let Err(e) = orrery_snapshot::validate_name(after)
    {
        fields.fault(Fault::invalid(e.to_string(), "after_name"));
    }
    let page_size = match fields.finish(page_size) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    let guard = state.git.gate().read_owned().await;
    let dir = state.snapshots_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _hold = guard;
        orrery_snapshot::list(&dir)
    })
    .await;
    let all = match flatten(result) {
        Ok(a) => a,
        Err(e) => return snapshot_error(e, None),
    };
    // Keyset over the listing's name order; the set is bounded by what an
    // operator created, but the read contract is uniform anyway.
    let rows: Vec<_> = all
        .into_iter()
        .filter(|s| match &after_name {
            Some(after) => s.name.as_str() > after.as_str(),
            None => true,
        })
        .take(page_size + 1)
        .collect();
    let (items, next_after_name) = keyset_page(rows, page_size, |s| s.name.clone());
    json(
        StatusCode::OK,
        serde_json::json!({
            "items": items.iter().map(to_value).collect::<Vec<_>>(),
            "next_after_name": next_after_name,
        }),
    )
}

async fn delete_one(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    body: Bytes,
) -> Response<Body> {
    delete_named(&state, name, &body).await
}

/// The delete of a snapshot named `list`, routed here for the reason
/// [`get_one_named_list`] is.
async fn delete_one_named_list(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    delete_named(&state, "list".to_string(), &body).await
}

async fn delete_named(state: &AppState, name: String, body: &[u8]) -> Response<Body> {
    if let Err(resp) = Fields::parse(body).finish(Some(())) {
        return *resp;
    }
    let guard = state.git.gate().write_owned().await;
    let dir = state.snapshots_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _hold = guard;
        orrery_snapshot::delete(&dir, &name)
    })
    .await;
    match flatten(result) {
        Ok(()) => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .expect("static response parts are valid"),
        Err(e) => snapshot_error(e, Some("name")),
    }
}

async fn restore(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    body: Bytes,
) -> Response<Body> {
    if let Err(resp) = Fields::parse(&body).finish(Some(())) {
        return *resp;
    }
    let gate = state.git.gate();
    let guard = gate.write_owned().await;
    let world = state.world.clone();
    let dir = state.snapshots_dir.clone();
    let restored_name = name.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _hold = guard;
        orrery_snapshot::restore(&world, &dir, &restored_name)
    })
    .await;
    if let Err(e) = flatten(result) {
        return snapshot_error(e, Some("name"));
    }
    match state.world.view_meta() {
        Ok(meta) => json(
            StatusCode::OK,
            serde_json::json!({
                "restored": name,
                "clock_unix": meta.clock_unix,
                "last_seq": meta.last_seq,
            }),
        ),
        Err(e) => internal(e),
    }
}

async fn diff(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let from: Option<String> = fields.required("from");
    // Another snapshot, or absent for the live world.
    let to: Option<String> = fields.optional("to");
    for (name, field) in [(&from, "from"), (&to, "to")] {
        if let Some(name) = name
            && let Err(e) = orrery_snapshot::validate_name(name)
        {
            fields.fault(Fault::invalid(e.to_string(), field));
        }
    }
    let from = match fields.finish(from) {
        Ok(f) => f,
        Err(resp) => return *resp,
    };
    let gate = state.git.gate();
    let guard = gate.write_owned().await;
    let world = state.world.clone();
    let dir = state.snapshots_dir.clone();
    // A failure to open a snapshot is about the field that named it; one in
    // the comparison itself is about neither.
    let result = tokio::task::spawn_blocking(move || {
        let _hold = guard;
        let from = orrery_snapshot::open(&dir, &from).map_err(|e| (e, Some("from")))?;
        let diffed = match &to {
            Some(to_name) => {
                let to = orrery_snapshot::open(&dir, to_name).map_err(|e| (e, Some("to")))?;
                orrery_snapshot::diff::diff(from.side(), to.side())
            }
            None => orrery_snapshot::diff::diff(
                from.side(),
                DiffSide {
                    name: "live",
                    view: &*world,
                    repos_dir: &world.repos_dir(),
                },
            ),
        };
        diffed.map_err(|e| (e, None))
    })
    .await;
    match result {
        Ok(Ok(d)) => match serde_json::to_value(&d) {
            Ok(v) => json(StatusCode::OK, v),
            Err(e) => internal(e),
        },
        Ok(Err((e, field))) => snapshot_error(e, field),
        Err(e) => internal(format!("blocking task failed: {e}")),
    }
}

fn flatten<T>(
    joined: Result<Result<T, SnapshotError>, tokio::task::JoinError>,
) -> Result<T, SnapshotError> {
    match joined {
        Ok(r) => r,
        Err(e) => Err(SnapshotError::Copy(format!("blocking task failed: {e}"))),
    }
}

fn to_value(info: &orrery_snapshot::SnapshotInfo) -> serde_json::Value {
    serde_json::json!({
        "name": info.name,
        "clock_unix": info.clock_unix,
        "last_seq": info.last_seq,
    })
}

fn not_found(name: &str) -> Response<Body> {
    errors(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        &format!("no snapshot named {name:?}"),
        Some("name"),
    )
}

fn snapshot_error(e: SnapshotError, field: Option<&str>) -> Response<Body> {
    match e {
        SnapshotError::InvalidName(_) => errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            &e.to_string(),
            field,
        ),
        SnapshotError::NotFound(_) => {
            errors(StatusCode::NOT_FOUND, "NOT_FOUND", &e.to_string(), field)
        }
        SnapshotError::AlreadyExists(_) => errors(
            StatusCode::CONFLICT,
            "ALREADY_EXISTS",
            &e.to_string(),
            field,
        ),
        SnapshotError::World(WorldError::SchemaVersion { .. }) => errors(
            StatusCode::BAD_REQUEST,
            "FAILED_PRECONDITION",
            &e.to_string(),
            field,
        ),
        other => internal(other),
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use orrery_world::{DEFAULT_T0_UNIX, World, WorldSecret};

    use super::*;

    fn state(dir: &std::path::Path) -> AppState {
        let world = Arc::new(
            World::create(&dir.join("world"), DEFAULT_T0_UNIX, &WorldSecret::LOCAL).unwrap(),
        );
        let git = orrery_face_git::GitFace::new(world.clone());
        let github = orrery_face_github::GithubFace {
            world: world.clone(),
            git: git.clone(),
            base_url: "http://world.test".to_string(),
        };
        AppState {
            world,
            base_url: "http://world.test".to_string(),
            git,
            snapshots_dir: dir.join("snapshots"),
            schema: orrery_face_github::graphql::schema(github),
            anonymous_git_reads: true,
        }
    }

    /// Long enough for a read that does not wait on the gate to finish.
    async fn settle() {
        tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(200)))
            .await
            .unwrap();
    }

    /// The test holds the gate exclusively and removes the snapshot under
    /// it, as a delete does. Reads sent meanwhile see the snapshot either
    /// whole or gone, never half removed.
    #[tokio::test(flavor = "multi_thread")]
    async fn snapshot_reads_wait_out_a_delete() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        orrery_snapshot::create(&state.world, &state.snapshots_dir, "before").unwrap();

        let held = state.git.gate().write_owned().await;
        let point = tokio::spawn(get_one(State(state.clone()), UrlPath("before".to_string())));
        let listed = tokio::spawn(list(State(state.clone()), Bytes::new()));
        settle().await;
        assert!(!point.is_finished(), "the point read did not wait");
        assert!(!listed.is_finished(), "the list did not wait");

        orrery_snapshot::delete(&state.snapshots_dir, "before").unwrap();
        drop(held);
        assert_eq!(point.await.unwrap().status(), StatusCode::NOT_FOUND);
        let listed = listed.await.unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let body = axum::body::to_bytes(listed.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["items"], serde_json::json!([]));
    }

    /// The test holds the gate exclusively, as a push does. A create with a
    /// bad name is refused on its first poll, so it never reached the gate,
    /// and one with a good name waits.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_snapshot_name_does_not_wait_on_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());

        let held = state.git.gate().write_owned().await;
        let bad = std::pin::pin!(create(
            State(state.clone()),
            Bytes::from_static(br#"{"name": "../x"}"#),
        ))
        .poll(&mut Context::from_waker(Waker::noop()));
        match bad {
            Poll::Ready(resp) => assert_eq!(resp.status(), StatusCode::BAD_REQUEST),
            Poll::Pending => panic!("the bad name waited on the gate"),
        }

        let good = tokio::spawn(create(
            State(state.clone()),
            Bytes::from_static(br#"{"name": "before"}"#),
        ));
        settle().await;
        assert!(!good.is_finished(), "the good name did not wait");
        drop(held);
        assert_eq!(good.await.unwrap().status(), StatusCode::CREATED);
    }
}
