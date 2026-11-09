//! End-to-end: what a call answers when no route serves its path or its
//! method. On a face it is the loud not-mocked 501; on the control plane it
//! is the control plane's own error shape. Both hold whether the control
//! plane shares the faces' listener or has its own.

mod common;

use common::{Instance, boot, boot_split, fault, faults, mint_actor};
use serde_json::Value;

/// `(status, Allow header, json)`, whatever the status.
fn call(url: &str, method: &str, token: Option<&str>) -> (u16, Option<String>, Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let request = ureq::http::Request::builder()
        .method(method)
        .uri(url)
        .header("content-type", "application/json");
    let request = match token {
        Some(token) => request.header("authorization", format!("token {token}")),
        None => request,
    };
    let mut resp = agent.run(request.body(()).unwrap()).unwrap();
    let allow = resp
        .headers()
        .get("allow")
        .map(|v| v.to_str().unwrap().to_string());
    let body = resp.body_mut().read_to_string().unwrap();
    let json = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("{method} {url}: {e}: body {body:?}"));
    (resp.status().as_u16(), allow, json)
}

async fn layouts() -> Vec<(&'static str, Instance)> {
    vec![("combined", boot().await), ("split", boot_split().await)]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_face_path_called_with_a_method_it_does_not_serve_is_not_mocked() {
    for (layout, inst) in layouts().await {
        let token = mint_actor(&inst.control, "alice", "user");
        for (method, path) in [
            ("PATCH", "/api/v3/repos/alice/webapp"),
            ("DELETE", "/api/v3/repos/alice/webapp"),
            ("GET", "/api/graphql"),
            ("GET", "/login/oauth/access_token"),
            ("DELETE", "/alice/webapp.git/info/refs"),
        ] {
            let url = format!("{}{path}", inst.base);
            let (status, _, answer) = call(&url, method, Some(&token));
            assert_eq!(status, 501, "{layout} {method} {path}: {answer}");
            assert_eq!(answer["orrery"]["not_mocked"], true, "{layout} {path}");
            assert_eq!(answer["orrery"]["method"], method, "{layout} {path}");
            assert_eq!(answer["orrery"]["path"], path, "{layout} {method}");
        }

        // Face auth comes first, as it does for an unmatched path under
        // `/api/v3`.
        let url = format!("{}/api/v3/repos/alice/webapp", inst.base);
        let (status, _, answer) = call(&url, "PATCH", None);
        assert_eq!(status, 401, "{layout}: {answer}");
        assert_eq!(answer["message"], "Bad credentials");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_control_path_called_with_a_method_it_does_not_take_is_405() {
    for (layout, inst) in layouts().await {
        mint_actor(&inst.control, "alice", "user");
        for (method, path, allow) in [
            ("DELETE", "/_orrery/actors/alice", "GET,HEAD"),
            ("GET", "/_orrery/clock/advance", "POST"),
            ("PATCH", "/_orrery/snapshots/before", "GET,HEAD,DELETE"),
            ("PUT", "/_orrery/log/list", "POST"),
        ] {
            let url = format!("{}{path}", inst.control);
            let (status, got_allow, answer) = call(&url, method, None);
            assert_eq!(status, 405, "{layout} {method} {path}: {answer}");
            assert_eq!(got_allow.as_deref(), Some(allow), "{layout} {path}");
            assert_eq!(faults(&answer), [fault("METHOD_NOT_ALLOWED", None)]);
            let message = answer["errors"][0]["message"].as_str().unwrap();
            assert!(message.contains(path), "{layout}: {message}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_control_path_is_404() {
    for (layout, inst) in layouts().await {
        for (method, path) in [
            ("POST", "/_orrery/bogus"),
            ("GET", "/_orrery"),
            ("GET", "/_orrery/"),
            ("GET", "/_orrery/actors/alice/bogus"),
            // Shaped like git face routes, which take any first segment.
            ("GET", "/_orrery/x/info/refs"),
            ("GET", "/_orrery/actors/info/refs?service=git-upload-pack"),
            ("DELETE", "/_orrery/x/info/refs"),
            ("POST", "/_orrery/x/git-upload-pack"),
            ("POST", "/_orrery/x/git-receive-pack"),
        ] {
            let url = format!("{}{path}", inst.control);
            let (status, _, answer) = call(&url, method, None);
            assert_eq!(status, 404, "{layout} {method} {path}: {answer}");
            assert_eq!(faults(&answer), [fault("NOT_FOUND", None)]);
        }

        // Any other path is face surface, on whichever listener it reaches.
        let url = format!("{}/_orreryx", inst.control);
        let (status, _, answer) = call(&url, "GET", None);
        assert_eq!(status, 501, "{layout}: {answer}");
        assert_eq!(answer["orrery"]["not_mocked"], true);
    }

    // A faces listener with no control plane holds no control paths.
    let inst = boot_split().await;
    let url = format!("{}/_orrery/bogus", inst.base);
    let (status, _, answer) = call(&url, "POST", None);
    assert_eq!(status, 501, "{answer}");
    assert_eq!(answer["orrery"]["not_mocked"], true);
}
