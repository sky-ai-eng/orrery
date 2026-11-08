//! Snapshot, restore, and diff routes on the control plane — engine
//! operations reality has no API for (spec/usage.md — door two).
//!
//! Create, restore, delete, and diff all take the git face's world write gate
//! exclusively: no push, merge, or fetch is in flight while a world directory
//! is copied, swapped, or compared, so every one of these reads or writes an
//! exact moment. The filesystem work runs on the blocking pool with the gate
//! guard moved into the task.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{Response, StatusCode};
use axum::routing::{get, post};
use orrery_snapshot::{DiffSide, SnapshotError};
use orrery_world::{WorldError, WorldView};

use crate::control::{Fault, Fields, errors, internal};
use crate::{AppState, json};

const LIST_PAGE_MAX: usize = 500;
const LIST_PAGE_DEFAULT: usize = 100;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/snapshots", post(create))
        .route("/snapshots/list", post(list))
        .route("/snapshots/{name}", get(get_one).delete(delete_one))
        .route("/snapshots/{name}/restore", post(restore))
        .route("/diff", post(diff))
}

async fn create(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let name: Option<String> = fields.required("name");
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
    match orrery_snapshot::info(&state.snapshots_dir, &name) {
        Ok(Some(info)) => json(StatusCode::OK, to_value(&info)),
        Ok(None) => not_found(&name),
        Err(e) => snapshot_error(e, Some("name")),
    }
}

// TODO(ORR-37): a last page that is exactly full answers a non-null
// `next_after_name`, and the page-size bounds are a second copy of
// control.rs's.
async fn list(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let page_size = fields.optional("page_size").unwrap_or(LIST_PAGE_DEFAULT);
    if page_size == 0 || page_size > LIST_PAGE_MAX {
        fields.fault(Fault::out_of_range(
            format!("page_size must be 1..={LIST_PAGE_MAX}"),
            "page_size",
        ));
    }
    let after_name: Option<String> = fields.optional("after_name");
    let (page_size, after_name) = match fields.finish(Some((page_size, after_name))) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let all = match orrery_snapshot::list(&state.snapshots_dir) {
        Ok(a) => a,
        Err(e) => return snapshot_error(e, None),
    };
    // Keyset over the listing's name order; the set is bounded by what an
    // operator created, but the read contract is uniform anyway.
    let items: Vec<_> = all
        .into_iter()
        .filter(|s| match &after_name {
            Some(after) => s.name.as_str() > after.as_str(),
            None => true,
        })
        .take(page_size)
        .collect();
    let next_after_name = (items.len() == page_size)
        .then(|| items.last().map(|s| s.name.clone()))
        .flatten();
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
) -> Response<Body> {
    let gate = state.git.gate();
    let _guard = gate.write().await;
    match orrery_snapshot::delete(&state.snapshots_dir, &name) {
        Ok(()) => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .expect("static response parts are valid"),
        Err(e) => snapshot_error(e, Some("name")),
    }
}

async fn restore(State(state): State<AppState>, UrlPath(name): UrlPath<String>) -> Response<Body> {
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
