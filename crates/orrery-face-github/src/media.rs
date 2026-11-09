//! The media types a REST route answers in (spec/grammar.md, "Requests").
//! GitHub picks a rendering from a parameter in its own media type:
//! `application/vnd.github.v3.full+json` asks for `body_text` and
//! `body_html` beside `body`, and `application/vnd.github.v3.diff` for the
//! diff instead of the object. The face renders JSON, so a GitHub media type
//! asking for anything else is refused by name rather than answered with JSON
//! that lacks what it asked for (rule 1).

use axum::extract::{Request, State};
use axum::http::{HeaderMap, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::MethodRouter;

use crate::{GithubFace, orrery_unsupported_field};

#[derive(Clone, Copy)]
struct Renders {
    resource: &'static str,
    params: &'static [&'static str],
}

/// `methods`, which answer JSON about `resource`, behind the refusal of every
/// GitHub media type asking for another rendering.
pub(crate) fn json(
    resource: &'static str,
    methods: MethodRouter<GithubFace>,
) -> MethodRouter<GithubFace> {
    rendering(resource, &[], methods)
}

/// As [`json`], for routes that also answer the renderings `params` name
/// (`object` in `application/vnd.github.object`).
pub(crate) fn rendering(
    resource: &'static str,
    params: &'static [&'static str],
    methods: MethodRouter<GithubFace>,
) -> MethodRouter<GithubFace> {
    // A route layer, so a method the path doesn't serve still falls through
    // to the not-mocked `501`, whatever it accepts.
    methods.route_layer(middleware::from_fn_with_state(
        Renders { resource, params },
        refuse_unrendered,
    ))
}

async fn refuse_unrendered(
    State(renders): State<Renders>,
    request: Request,
    next: Next,
) -> Response {
    if unrendered(request.headers(), renders.params) {
        return orrery_unsupported_field(renders.resource, "Accept");
    }
    next.run(request).await
}

/// Whether any media type `Accept` lists is a GitHub media type asking for a
/// rendering that is neither JSON nor one of `params`.
fn unrendered(headers: &HeaderMap, params: &[&str]) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_ascii_lowercase)
        .any(|value| {
            value
                .split(',')
                .filter_map(|range| rendering_param(range.split(';').next().unwrap_or("").trim()))
                .any(|param| !param.is_empty() && !params.contains(&param))
        })
}

/// The rendering a lowercased GitHub media type asks for: its parameter
/// (`full` in `application/vnd.github.v3.full+json`), or `""` for JSON.
/// `None` when the media type isn't GitHub's.
fn rendering_param(media: &str) -> Option<&str> {
    let rest = media.strip_prefix("application/vnd.github")?;
    let rest = rest.strip_suffix("+json").unwrap_or(rest);
    if rest.is_empty() {
        return Some("");
    }
    let rest = rest.strip_prefix('.')?;
    let param = match rest.strip_prefix("v3") {
        Some("") => "",
        Some(after) => after.strip_prefix('.').unwrap_or(rest),
        None => rest,
    };
    // A preview asks for JSON: none of the face's routes has a preview left
    // on GitHub, and gh's HTTP client sends two previews on every request.
    Some(if param.ends_with("-preview") {
        ""
    } else {
        param
    })
}

#[cfg(test)]
mod tests {
    use super::rendering_param;

    #[test]
    fn rendering_params() {
        for (media, param) in [
            ("application/vnd.github+json", Some("")),
            ("application/vnd.github.v3+json", Some("")),
            ("application/vnd.github.v3", Some("")),
            ("application/vnd.github.merge-info-preview+json", Some("")),
            ("application/vnd.github.nebula-preview", Some("")),
            ("application/vnd.github.v3.squirrel-girl-preview", Some("")),
            ("application/vnd.github.v3.diff", Some("diff")),
            ("application/vnd.github.patch", Some("patch")),
            ("application/vnd.github.full+json", Some("full")),
            ("application/vnd.github.v3.raw", Some("raw")),
            ("application/vnd.github.object+json", Some("object")),
            ("application/vnd.github.v3x+json", Some("v3x")),
            ("application/vnd.githubx+json", None),
            ("application/json", None),
            ("*/*", None),
        ] {
            assert_eq!(rendering_param(media), param, "{media}");
        }
    }
}
