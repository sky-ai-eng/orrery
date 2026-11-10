//! The media types a REST route answers in (spec/grammar.md, "Requests").
//! GitHub picks a rendering from a parameter in its own media type:
//! `application/vnd.github.v3.full+json` asks for `body_text` and
//! `body_html` beside `body`, and `application/vnd.github.v3.diff` for the
//! diff instead of the object. The face renders JSON, and the diff on the
//! routes that list it, so a GitHub media type asking for anything else is
//! refused by name rather than answered with JSON that lacks what it asked
//! for (rule 1).

use axum::body::Body;
use axum::http::{HeaderMap, Response, header};

use crate::orrery_unsupported_field;

/// The not-mocked refusal naming `Accept`, when it lists a GitHub media type
/// asking for a rendering of `resource` other than JSON.
///
/// Every REST handler calls this, [`refuse_unrendered_except`], or
/// [`refuse_unrendered_unless_diff`] once, as a statement of its own, after
/// it has found what its path names and before it reads its body or writes
/// anything. That is GitHub's order: something
/// that doesn't exist is a `404` whatever the request accepts, and a refused
/// write must land nothing.
pub(crate) fn refuse_unrendered(headers: &HeaderMap, resource: &str) -> Option<Response<Body>> {
    refuse_unrendered_except(headers, resource, &[])
}

/// As [`refuse_unrendered`], for a route that also answers the renderings
/// `renders` names (`object` in `application/vnd.github.object`).
pub(crate) fn refuse_unrendered_except(
    headers: &HeaderMap,
    resource: &str,
    renders: &[&str],
) -> Option<Response<Body>> {
    unrendered(headers, renders).then(|| orrery_unsupported_field(resource, "Accept"))
}

/// As [`refuse_unrendered`], for a route that also answers in the diff media
/// type. GitHub answers the diff whenever `Accept` lists it, whatever it
/// lists beside it, so a request that lists it is never refused.
pub(crate) fn refuse_unrendered_unless_diff(
    headers: &HeaderMap,
    resource: &str,
) -> Option<Response<Body>> {
    if diff_media(headers).is_some() {
        return None;
    }
    refuse_unrendered(headers, resource)
}

/// The first diff media type `Accept` lists, lowercased, which the answer
/// names as its content type; `None` when it lists none.
pub(crate) fn diff_media(headers: &HeaderMap) -> Option<String> {
    listed(headers)
        .into_iter()
        .find_map(|(media, param)| (param == "diff").then_some(media))
}

/// Whether any media type `Accept` lists is a GitHub media type asking for a
/// rendering that is neither JSON nor one of `renders`.
fn unrendered(headers: &HeaderMap, renders: &[&str]) -> bool {
    listed(headers)
        .iter()
        .any(|(_, param)| !param.is_empty() && !renders.contains(&param.as_str()))
}

/// Each GitHub media type `Accept` lists, lowercased and without its
/// parameters, with the rendering it asks for.
fn listed(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_ascii_lowercase)
        .flat_map(|value| {
            value
                .split(',')
                .filter_map(|range| {
                    let media = range.split(';').next().unwrap_or("").trim();
                    let param = rendering_param(media)?;
                    Some((media.to_string(), param.to_string()))
                })
                .collect::<Vec<_>>()
        })
        .collect()
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
