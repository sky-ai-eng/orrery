//! CGI bridge to real `git http-backend`. Serving actual git server-side code
//! (rather than reimplementing the pack protocol) removes an entire fidelity
//! class — rule 2: real tools judge realness.

use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::watch;

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

/// Run one request through `git http-backend`. It runs in a process group of
/// its own, which the receive-pack or upload-pack it starts, and their hooks,
/// join. Once `kill` reads true, that whole group is killed, so nothing the
/// request started outlives it. Because the group is not the server's, a
/// signal sent to the server's group, as Ctrl-C in a terminal sends, reaches
/// none of these processes, and the server decides when they stop.
pub async fn run_http_backend(
    repos_dir: &Path,
    req: CgiRequest<'_>,
    kill: watch::Receiver<bool>,
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
        .process_group(0)
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

    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let body = req.body.to_vec();
    // Write the request body concurrently with collecting output: http-backend
    // may start emitting sideband while still reading a large pack.
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&body).await;
        // Dropping stdin closes it, signalling EOF to the CGI.
    });
    let exited = async {
        tokio::select! {
            status = child.wait() => status,
            () = killed(kill) => {
                // The group is killed before the child is reaped, so its id
                // still names this group and no other.
                if let Some(pgid) = child.id() {
                    kill_group(pgid);
                }
                child.wait().await
            }
        }
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let (status, read_out, read_err) = tokio::join!(
        exited,
        stdout.read_to_end(&mut out),
        stderr.read_to_end(&mut err)
    );
    let _ = writer.await;
    let status = status?;
    read_out?;
    read_err?;

    if !status.success() {
        return Err(GitFaceError::Cgi(format!(
            "git http-backend exited {status}: {}",
            String::from_utf8_lossy(&err)
        )));
    }
    parse_cgi_output(out)
}

/// Resolves once `kill` reads true, and never if its sender is gone.
async fn killed(mut kill: watch::Receiver<bool>) {
    if kill.wait_for(|kill| *kill).await.is_err() {
        std::future::pending::<()>().await;
    }
}

fn kill_group(pgid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        tracing::error!(pgid, "a process group id out of range was not killed");
        return;
    };
    // SAFETY: killpg takes no pointers and touches no memory of this process.
    if unsafe { libc::killpg(pgid, libc::SIGKILL) } != 0 {
        let e = std::io::Error::last_os_error();
        tracing::error!(error = %e, pgid, "killing a git process group failed");
    }
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
