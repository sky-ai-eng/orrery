//! CGI bridge to real `git http-backend`. Serving actual git server-side code
//! (rather than reimplementing the pack protocol) removes an entire fidelity
//! class — rule 2: real tools judge realness.

use std::path::Path;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;

use crate::GitFaceError;

/// One request's worth of CGI input, already normalized by the handler.
pub struct CgiRequest<'a> {
    pub method: &'a str,
    /// Always the `.git`-suffixed form: `/{owner}/{name}.git/{service-path}`.
    pub path_info: String,
    pub query: &'a str,
    pub content_type: Option<&'a str>,
    pub content_encoding: Option<&'a str>,
    /// The `Git-Protocol` request header; forwarding it is what enables
    /// protocol v2 through http-backend.
    pub git_protocol: Option<&'a str>,
    /// Absent on anonymous (public-repo fetch) requests.
    pub remote_user: Option<&'a str>,
    pub body: &'a [u8],
}

pub struct CgiResponse {
    pub status: u16,
    /// (name, value) pairs from the CGI header block, minus `Status`.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub async fn run_http_backend(
    repos_dir: &Path,
    req: CgiRequest<'_>,
) -> Result<CgiResponse, GitFaceError> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("http-backend")
        .env("GIT_PROJECT_ROOT", repos_dir)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GIT_HTTP_MAX_REQUEST_BUFFER", "100M")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("REQUEST_METHOD", req.method)
        .env("PATH_INFO", &req.path_info)
        .env("QUERY_STRING", req.query)
        .env("REMOTE_ADDR", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(user) = req.remote_user {
        cmd.env("REMOTE_USER", user);
    }
    if let Some(ct) = req.content_type {
        cmd.env("CONTENT_TYPE", ct);
    }
    if req.method == "POST" {
        cmd.env("CONTENT_LENGTH", req.body.len().to_string());
    }
    if let Some(ce) = req.content_encoding {
        cmd.env("HTTP_CONTENT_ENCODING", ce);
    }
    if let Some(gp) = req.git_protocol {
        cmd.env("HTTP_GIT_PROTOCOL", gp);
    }

    // TODO(ORR-78): this http-backend, and the receive-pack it starts,
    // outlive an `orrery serve` stopped by a signal to its process alone, and
    // can still move refs after it exits.
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().expect("stdin piped");
    let body = req.body.to_vec();
    // Write the request body concurrently with collecting output: http-backend
    // may start emitting sideband while still reading a large pack.
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&body).await;
        // Dropping stdin closes it, signalling EOF to the CGI.
    });
    let out = child.wait_with_output().await?;
    let _ = writer.await;

    if !out.status.success() {
        return Err(GitFaceError::Cgi(format!(
            "git http-backend exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    parse_cgi_output(out.stdout)
}

/// CGI output is a CRLF header block, a blank line, then the body. `Status:`
/// carries the HTTP status (absent means 200).
fn parse_cgi_output(raw: Vec<u8>) -> Result<CgiResponse, GitFaceError> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| GitFaceError::Cgi("http-backend output had no CGI header block".into()))?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = raw[split + 4..].to_vec();

    let mut status = 200u16;
    let mut headers = Vec::new();
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            status = value
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(200);
        } else {
            headers.push((name.to_string(), value.to_string()));
        }
    }
    Ok(CgiResponse {
        status,
        headers,
        body,
    })
}
