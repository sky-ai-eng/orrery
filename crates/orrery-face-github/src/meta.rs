//! `GET /meta`, where a GitHub Enterprise Server client learns which release
//! it is talking to (spec/surface-github.md §1.9).

use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Response, StatusCode};

use crate::{GithubFace, json_response, media};

/// Clients choose which requests to send by this release, so it names the
/// one whose requests the face serves; §1.9 says why this one.
const INSTALLED_VERSION: &str = "3.17.0";

/// github.com's `/meta` lists its network addresses and keys, and names no
/// release, so under its layout the route is surface Orrery does not serve,
/// whichever host the request reached it through.
pub(crate) async fn get_meta(
    State(face): State<GithubFace>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response<Body> {
    if face.layout.is_github_com() {
        return crate::not_mocked("GET", uri.path());
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "Meta") {
        return refused;
    }
    json_response(
        StatusCode::OK,
        serde_json::json!({
            // No actor has a password to verify (spec/README.md rule 12).
            "verifiable_password_authentication": false,
            "installed_version": INSTALLED_VERSION,
        }),
    )
}
