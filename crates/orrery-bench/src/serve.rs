//! One world, served by one engine build: `orrery serve` on a free port, with
//! an actor, `alice`, and her public repository, `alice/bench`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

pub struct Server {
    /// The engine build's name in the report.
    pub engine: String,
    /// The faces' origin.
    pub base: String,
    pub token: String,
    /// `alice/bench`'s clone URL, with her token in it.
    pub url: String,
    /// `alice/bench`'s bare repository.
    pub bare: PathBuf,
    _child: Reaped,
    _stdout: BufReader<ChildStdout>,
    _dir: tempfile::TempDir,
}

impl Server {
    pub fn start(engine: &str, binary: &Path, work: &Path) -> Result<Server> {
        let instances = work.join("instances");
        std::fs::create_dir_all(&instances)?;
        let dir = tempfile::Builder::new()
            .prefix(&format!("{engine}-"))
            .tempdir_in(&instances)?;
        let instance = dir.path().join("instance");
        let config = dir.path().join("orrery.toml");
        std::fs::write(
            &config,
            format!(
                "[instance]\ndir = {:?}\n\n[server]\nlisten = \"127.0.0.1:0\"\n",
                instance.display().to_string()
            ),
        )?;
        let log = std::fs::File::create(dir.path().join("serve.log"))?;
        let mut child = Reaped(
            Command::new(binary)
                .args(["serve", "--config"])
                .arg(&config)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(log)
                .spawn()
                .with_context(|| format!("starting {}", binary.display()))?,
        );
        let mut stdout = BufReader::new(child.0.stdout.take().expect("stdout is piped"));
        let mut ready = String::new();
        stdout.read_line(&mut ready)?;
        let Ok(ready) = serde_json::from_str::<Value>(&ready) else {
            bail!(
                "{} printed no ready line; its log is in {}",
                binary.display(),
                dir.path().display()
            );
        };
        let base = ready["base_url"]
            .as_str()
            .context("ready line has no base_url")?
            .to_string();
        let mut server = Server {
            engine: engine.to_string(),
            token: String::new(),
            url: String::new(),
            bare: instance.join("world/repos/alice/bench.git"),
            base,
            _child: child,
            _stdout: stdout,
            _dir: dir,
        };
        let actor = server.send(
            "POST",
            "/_orrery/actors",
            serde_json::json!({ "login": "alice", "kind": "user" }),
        )?;
        server.token = actor["token"]
            .as_str()
            .context("minted actor has no token")?
            .to_string();
        server.send(
            "POST",
            "/api/v3/user/repos",
            serde_json::json!({ "name": "bench" }),
        )?;
        let host = server.base.trim_start_matches("http://");
        server.url = format!(
            "http://x-access-token:{}@{host}/alice/bench.git",
            server.token
        );
        Ok(server)
    }

    /// A REST path under `alice/bench`.
    pub fn api(&self, rest: &str) -> String {
        format!("/api/v3/repos/alice/bench{rest}")
    }

    /// Send `body` to `path` with alice's token, and read the JSON answer.
    pub fn send(&self, method: &str, path: &str, body: Value) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let auth = format!("token {}", self.token);
        let sent = match method {
            "POST" => ureq::post(&url)
                .header("authorization", &auth)
                .send_json(&body),
            "PUT" => ureq::put(&url)
                .header("authorization", &auth)
                .send_json(&body),
            _ => bail!("no {method} in the benches"),
        };
        let mut resp = sent.with_context(|| format!("{method} {path}"))?;
        Ok(resp.body_mut().read_json()?)
    }

    /// How many bytes `alice/bench` holds in its object store.
    pub fn repo_bytes(&self) -> Result<u64> {
        fn walk(dir: &Path) -> std::io::Result<u64> {
            let mut total = 0;
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let meta = entry.metadata()?;
                total += if meta.is_dir() {
                    walk(&entry.path())?
                } else {
                    meta.len()
                };
            }
            Ok(total)
        }
        Ok(walk(&self.bare.join("objects"))?)
    }
}

/// A served world's process, stopped when this is dropped, so a start that
/// fails partway leaves no server running.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `binary --version`'s first line, for the report's header.
pub fn version(binary: &Path) -> String {
    Command::new(binary)
        .arg("--version")
        .output()
        .ok()
        .map(|o| {
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout.lines().next().unwrap_or_default().trim().to_string()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "(no version)".to_string())
}
