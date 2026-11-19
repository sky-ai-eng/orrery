//! `GET /meta`, where a GitHub Enterprise Server client learns which release
//! it is talking to (spec/surface-github.md §1.9).

use axum::body::Body;
use axum::http::{HeaderMap, Response, StatusCode};

use crate::{json_response, media};

/// Clients choose which requests to send by this release, so it names the
/// one whose requests the face serves; §1.9 says why this one.
const INSTALLED_VERSION: &str = "3.17.0";

pub(crate) async fn get_meta(headers: HeaderMap) -> Response<Body> {
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
