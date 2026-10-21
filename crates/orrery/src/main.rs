use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use orrery_world::World;

#[derive(Parser)]
#[command(
    name = "orrery",
    about = "A deterministic mock of the software-development world"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve one world instance (creating it on first run).
    Serve {
        /// Instance config file (TOML). See `orrery.example.toml`.
        #[arg(long)]
        config: PathBuf,
    },
    /// Copy a snapshot into a fresh instance directory as its live world.
    /// Snapshots are static, so this is safe while the source instance is
    /// being served.
    Fork {
        /// The source instance directory (the one holding `snapshots/`).
        #[arg(long)]
        instance: PathBuf,
        /// The snapshot name to fork from.
        #[arg(long)]
        snapshot: String,
        /// The new instance directory; must not already hold a world.
        #[arg(long)]
        dest: PathBuf,
    },
}

/// An instance is one config file plus one seed (spec/README.md rule 7).
/// Strictly parsed: unknown keys are rejected, not ignored.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    instance: InstanceConfig,
    #[serde(default)]
    server: ServerConfig,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceConfig {
    /// Directory holding this instance's `world/` (and later `snapshots/`).
    dir: PathBuf,
    /// RFC3339 T0 for a newly created world. Ignored when the world already
    /// exists — the clock lives in the world, not the config.
    t0: Option<String>,
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ServerConfig {
    listen: Option<Listen>,
    /// External base URL rendered into clone/html URLs; defaults to
    /// `http://<bound address>`. Required when `listen` is a socket, since no
    /// URL can be derived from one.
    base_url: Option<String>,
    /// Serve the control plane only here. Absent, it stays at `/_orrery` on
    /// `listen`.
    control_listen: Option<Listen>,
    /// Defaults to true; see `orrery_server::Options::anonymous_git_reads`.
    anonymous_git_reads: Option<bool>,
}

/// A listener address: `host:port`, or `unix:/absolute/path` for a Unix
/// socket, which needs no port.
#[derive(serde::Deserialize)]
#[serde(try_from = "String")]
enum Listen {
    Tcp(String),
    Unix(PathBuf),
}

impl TryFrom<String> for Listen {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let Some(path) = raw.strip_prefix("unix:") else {
            return Ok(Listen::Tcp(raw));
        };
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(format!(
                "{raw:?}: a socket path must be absolute, as in unix:/run/orrery.sock"
            ));
        }
        Ok(Listen::Unix(path))
    }
}

enum Listener {
    Tcp(tokio::net::TcpListener, std::net::SocketAddr),
    Unix(tokio::net::UnixListener, PathBuf),
}

impl std::fmt::Display for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Listener::Tcp(_, addr) => write!(f, "{addr}"),
            Listener::Unix(_, path) => write!(f, "unix:{}", path.display()),
        }
    }
}

impl Listen {
    async fn bind(&self) -> anyhow::Result<Listener> {
        match self {
            Listen::Tcp(addr) => {
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .with_context(|| format!("binding {addr}"))?;
                let bound = listener.local_addr()?;
                Ok(Listener::Tcp(listener, bound))
            }
            Listen::Unix(path) => {
                remove_stale_socket(path)?;
                let listener = tokio::net::UnixListener::bind(path)
                    .with_context(|| format!("binding unix:{}", path.display()))?;
                Ok(Listener::Unix(listener, path.clone()))
            }
        }
    }
}

/// A killed process leaves its socket file behind, and binding over it fails,
/// so a restarted world would find its address taken. Only a socket nobody
/// answers on is stale: a live one is another process's, and a file that is
/// not a socket was never ours to delete.
fn remove_stale_socket(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::FileTypeExt as _;

    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("inspecting {}", path.display())),
    };
    anyhow::ensure!(
        meta.file_type().is_socket(),
        "{} exists and is not a socket",
        path.display()
    );
    anyhow::ensure!(
        std::os::unix::net::UnixStream::connect(path).is_err(),
        "{} is in use by another process",
        path.display()
    );
    std::fs::remove_file(path).with_context(|| format!("removing stale socket {}", path.display()))
}

async fn run(listener: Listener, app: axum::Router) -> std::io::Result<()> {
    match listener {
        Listener::Tcp(listener, _) => orrery_server::run(listener, app).await,
        Listener::Unix(listener, _) => orrery_server::run(listener, app).await,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve { config } => serve(&config).await,
        Command::Fork {
            instance,
            snapshot,
            dest,
        } => fork(&instance, &snapshot, &dest),
    }
}

fn fork(instance: &std::path::Path, snapshot: &str, dest: &std::path::Path) -> anyhow::Result<()> {
    let world = orrery_snapshot::fork(&instance.join("snapshots"), snapshot, dest)
        .with_context(|| format!("forking snapshot {snapshot:?} from {}", instance.display()))?;
    println!(
        "forked {snapshot:?} into {} — serve it with a config whose instance.dir is {}",
        world.display(),
        dest.display()
    );
    Ok(())
}

async fn serve(config_path: &std::path::Path) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;
    let config: Config = toml::from_str(&raw)
        .with_context(|| format!("parsing config {}", config_path.display()))?;

    let t0_unix = match &config.instance.t0 {
        Some(t0) => jiff::Timestamp::from_str(t0)
            .with_context(|| format!("instance.t0 {t0:?} is not RFC3339"))?
            .as_second(),
        None => orrery_world::DEFAULT_T0_UNIX,
    };
    let world_root = config.instance.dir.join("world");
    let world = Arc::new(World::open_or_create(&world_root, t0_unix)?);

    let listen = config
        .server
        .listen
        .unwrap_or_else(|| Listen::Tcp("127.0.0.1:4180".to_string()));
    let listener = listen.bind().await?;
    let base_url = match (config.server.base_url, &listener) {
        (Some(base_url), _) => base_url,
        (None, Listener::Tcp(_, bound)) => format!("http://{bound}"),
        (None, Listener::Unix(..)) => anyhow::bail!(
            "server.base_url must be set when server.listen is a Unix socket: no URL can be derived from one"
        ),
    };
    let control = match &config.server.control_listen {
        Some(control_listen) => Some(control_listen.bind().await?),
        None => None,
    };

    tracing::info!(
        world = %world_root.display(),
        listen = %listener,
        control = %control.as_ref().map_or_else(|| format!("{listener}/_orrery"), ToString::to_string),
        %base_url,
        clock_unix = world.clock()?,
        "orrery serving"
    );
    let routers = orrery_server::routers(
        world,
        base_url,
        config.instance.dir.join("snapshots"),
        orrery_server::Options {
            anonymous_git_reads: config.server.anonymous_git_reads.unwrap_or(true),
        },
    );
    match control {
        None => run(listener, routers.combined()).await?,
        Some(control) => {
            let (faces, control_plane) = routers.split();
            tokio::try_join!(run(listener, faces), run(control, control_plane))?;
        }
    }
    Ok(())
}
