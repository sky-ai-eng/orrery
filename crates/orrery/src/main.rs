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
    listen: Option<String>,
    /// External base URL rendered into clone/html URLs; defaults to
    /// `http://<bound address>`.
    base_url: Option<String>,
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

    let listen = config.server.listen.as_deref().unwrap_or("127.0.0.1:4180");
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let bound = listener.local_addr()?;
    let base_url = config
        .server
        .base_url
        .clone()
        .unwrap_or_else(|| format!("http://{bound}"));

    tracing::info!(
        world = %world_root.display(),
        %bound,
        %base_url,
        clock_unix = world.clock()?,
        "orrery serving"
    );
    let app = orrery_server::app(world, base_url, config.instance.dir.join("snapshots"));
    orrery_server::run(listener, app).await?;
    Ok(())
}
