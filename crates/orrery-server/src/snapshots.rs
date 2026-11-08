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

use crate::control::{decode_strict, errors, faults, internal, keyset_page, page_size};
use crate::{AppState, json};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/snapshots", post(create))
        .route("/snapshots/list", post(list))
        .route("/snapshots/{name}", get(get_one).delete(delete_one))
        .route("/snapshots/{name}/restore", post(restore))
        .route("/diff", post(diff))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    name: String,
}

async fn create(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let parsed: CreateBody = match decode_strict(&body) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let gate = state.git.gate();
    let guard = gate.write_owned().await;
    let world = state.world.clone();
    let dir = state.snapshots_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _hold = guard;
        orrery_snapshot::create(&world, &dir, &parsed.name)
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ListBody {
    after_name: Option<String>,
    page_size: Option<usize>,
}

async fn list(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let parsed: ListBody = match decode_strict(&body) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let page_size = match page_size(parsed.page_size) {
        Ok(n) => n,
        Err(fault) => return faults(StatusCode::BAD_REQUEST, vec![fault]),
    };
    let all = match orrery_snapshot::list(&state.snapshots_dir) {
        Ok(a) => a,
        Err(e) => return snapshot_error(e, None),
    };
    // Keyset over the listing's name order; the set is bounded by what an
    // operator created, but the read contract is uniform anyway.
    let rows: Vec<_> = all
        .into_iter()
        .filter(|s| match &parsed.after_name {
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffBody {
    from: String,
    /// Another snapshot, or absent for the live world.
    to: Option<String>,
}

async fn diff(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let parsed: DiffBody = match decode_strict(&body) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let gate = state.git.gate();
    let guard = gate.write_owned().await;
    let world = state.world.clone();
    let dir = state.snapshots_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _hold = guard;
        let from = orrery_snapshot::open(&dir, &parsed.from)?;
        match &parsed.to {
            Some(to_name) => {
                let to = orrery_snapshot::open(&dir, to_name)?;
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
        }
    })
    .await;
    match flatten(result) {
        Ok(d) => match serde_json::to_value(&d) {
            Ok(v) => json(StatusCode::OK, v),
            Err(e) => internal(e),
        },
        Err(e) => snapshot_error(e, None),
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
