use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::{Arc, LazyLock};

use anyhow::Context;
use clap::{ArgGroup, Parser, Subcommand};
use orrery_snapshot::SnapshotError;
use orrery_world::{ClockMode, NewClock, World, WorldError, WorldSecret};
use sha2::{Digest as _, Sha256};

static VERSION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{} ({})",
        env!("CARGO_PKG_VERSION"),
        orrery_server::BUILD_COMMIT
    )
});

mod control;
mod passed;

#[derive(Parser)]
#[command(
    name = "orrery",
    version = VERSION.as_str(),
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
    /// Create a world at `<instance>/world`, exactly as `serve` does on first
    /// run, and exit without serving it. Writes no config file.
    Create {
        /// The new instance directory; must not already hold a world.
        #[arg(long)]
        instance: PathBuf,
        /// RFC3339 T0, the new world's clock. Defaults to
        /// 2020-01-01T00:00:00Z. Refused with `--clock real`.
        #[arg(long)]
        t0: Option<String>,
        /// How the new world's clock moves.
        #[arg(long, value_enum, default_value_t)]
        clock: ClockChoice,
        /// The secret the new world derives its credentials from.
        #[arg(long, value_enum, default_value_t)]
        secret: SecretChoice,
        /// Print one JSON object on stdout: the instance on success, the
        /// control plane's error shape on failure.
        #[arg(long)]
        json: bool,
    },
    /// Copy a world into a fresh instance directory as its live world,
    /// re-keyed under the chosen secret.
    ///
    /// The source is a snapshot (`--instance` with `--snapshot`) or any world
    /// directory (`--world`). It is only read, so any number of forks may copy
    /// it at once. A snapshot never changes, so forking one is safe while its
    /// instance is served. A world that `orrery serve` is running can be
    /// copied mid-write, so `--world` is for stopped instances and built
    /// templates: to copy a live world, snapshot it and fork the snapshot.
    #[command(group(ArgGroup::new("source").required(true).args(["instance", "world"])))]
    Fork {
        /// The source instance directory (the one holding `snapshots/`).
        #[arg(long, requires = "snapshot")]
        instance: Option<PathBuf>,
        /// The snapshot name to fork from.
        #[arg(long, requires = "instance")]
        snapshot: Option<String>,
        /// A world directory to fork from, such as a stopped instance's
        /// `world/`. Never one a running `orrery serve` is using.
        #[arg(long, conflicts_with_all = ["instance", "snapshot"])]
        world: Option<PathBuf>,
        /// The new instance directory; must not already hold a world.
        #[arg(long)]
        dest: PathBuf,
        /// The secret the fork is re-keyed under.
        #[arg(long, value_enum, default_value_t)]
        secret: SecretChoice,
        /// Print one JSON object on stdout: the instance on success, the
        /// control plane's error shape on failure.
        #[arg(long)]
        json: bool,
    },
    /// Print `export` lines that date git commits at the world clock, read
    /// with `GET /_orrery/clock`. Run `eval "$(orrery env)"` before each
    /// commit, and again after advancing the clock.
    Env {
        #[command(flatten)]
        world: control::Address,
    },
    /// Read a served world's log of events.
    Log {
        #[command(subcommand)]
        command: LogCommand,
    },
}

#[derive(Subcommand)]
enum LogCommand {
    /// Print each entry after `--after-seq` in a world's log, then each one as
    /// it is appended, as one line of JSON: the stream of
    /// `GET /_orrery/log/stream`.
    ///
    /// It never exits 0, and never reconnects. A snapshot restore ends it with
    /// status 3, naming the restored log's newest seq. An error answer, a
    /// connection that can't be made or drops, or 45 seconds with nothing
    /// received ends it with status 1, naming the seq to resume after.
    Follow {
        /// Print the entries after this position: 0, or the `seq` of an entry
        /// in the log.
        #[arg(long)]
        after_seq: u64,
        #[command(flatten)]
        world: control::Address,
    },
}

/// How `orrery log follow` exits when a restore ends the stream, distinct
/// from every failure's 1 and a usage error's 2.
const RESTORED: u8 = 3;

/// An instance is one config file plus one seed (spec/README.md rule 7).
/// Strictly parsed: unknown keys are rejected, not ignored.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    instance: InstanceConfig,
    #[serde(default)]
    server: ServerConfig,
    #[serde(default)]
    outbound: OutboundConfig,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceConfig {
    /// Directory holding this instance's `world/` (and later `snapshots/`).
    dir: PathBuf,
    /// RFC3339 T0 for a newly created world. Ignored when the world already
    /// exists — the clock lives in the world, not the config.
    t0: Option<String>,
    /// How a newly created world's clock moves. Ignored when the world
    /// already exists, like `t0`: the mode lives in the world.
    #[serde(default)]
    clock: ClockChoice,
    /// The secret a newly created world derives its credentials from. Ignored
    /// when the world already exists, like `t0`: the secret lives in the world.
    #[serde(default)]
    secret: SecretChoice,
}

#[derive(serde::Deserialize, clap::ValueEnum, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum SecretChoice {
    /// A fixed, well-known secret: a local world needs no unguessable
    /// credentials, and its runs mint the same tokens every time.
    #[default]
    Local,
    /// Unguessable, as a world on the public internet needs.
    Random,
}

impl SecretChoice {
    fn secret(&self) -> anyhow::Result<WorldSecret> {
        match self {
            SecretChoice::Local => Ok(WorldSecret::LOCAL),
            SecretChoice::Random => {
                let mut bytes = [0; 32];
                // The one random input in the tree. It is safe for replay
                // because the world stores the secret at creation and every
                // later derivation reads it from there.
                #[allow(clippy::disallowed_methods)]
                getrandom::fill(&mut bytes)
                    .map_err(|e| anyhow::anyhow!("generating the world secret: {e}"))?;
                Ok(WorldSecret::from_bytes(bytes))
            }
        }
    }
}

#[derive(serde::Deserialize, clap::ValueEnum, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum ClockChoice {
    /// Moved only by `POST /_orrery/clock/advance`, from T0.
    #[default]
    Manual,
    /// The host's time, from the moment the world is created.
    Real,
}

impl ClockChoice {
    fn mode(self) -> ClockMode {
        match self {
            ClockChoice::Manual => ClockMode::Manual,
            ClockChoice::Real => ClockMode::Real,
        }
    }
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

impl ServerConfig {
    /// The rules that span fields, which serde checks none of.
    fn check(&self) -> anyhow::Result<()> {
        if let (Some(Listen::Fd(listen)), Some(Listen::Fd(control))) =
            (&self.listen, &self.control_listen)
        {
            anyhow::ensure!(
                listen != control,
                "server.listen and server.control_listen both name fd:{listen}: each needs a socket of its own"
            );
        }
        Ok(())
    }
}

/// How the one thing a world sends, a webhook delivery, is sent
/// (spec/usage.md, "What a world sends").
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct OutboundConfig {
    /// How a delivery's connection leaves the machine; `direct` when absent.
    #[serde(default, deserialize_with = "egress")]
    egress: orrery_outbound::Egress,
    /// A PEM file of certificate authorities to trust as well as the public
    /// roots the binary ships.
    extra_ca_certs: Option<PathBuf>,
}

fn egress<'de, D: serde::Deserializer<'de>>(raw: D) -> Result<orrery_outbound::Egress, D::Error> {
    let raw = <String as serde::Deserialize>::deserialize(raw)?;
    raw.parse().map_err(serde::de::Error::custom)
}

/// A listener address: `host:port`; `unix:/absolute/path` for a Unix socket
/// `serve` binds, which needs no port; or `fd:<name>` for a listening Unix
/// socket a supervisor passed to the process (`passed`).
#[derive(serde::Deserialize)]
#[serde(try_from = "String")]
enum Listen {
    Tcp(String),
    Unix(PathBuf),
    Fd(String),
}

impl TryFrom<String> for Listen {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if let Some(name) = raw.strip_prefix("fd:") {
            if !passed::name_is_valid(name) {
                return Err(format!(
                    "{raw:?}: a passed socket's name is 1 to 255 characters, with no `:` and no control characters, as in fd:faces"
                ));
            }
            return Ok(Listen::Fd(name.to_string()));
        }
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
            Listen::Fd(name) => {
                let (listener, path) =
                    passed::take(name).with_context(|| format!("listening on fd:{name}"))?;
                Ok(Listener::Unix(listener, path))
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

/// The one line `serve` writes to stdout, so a caller that started a world
/// learns where it listens without reading logs, and a world told to bind
/// port 0 can report the port it got.
#[derive(serde::Serialize)]
struct Ready<'a> {
    base_url: &'a str,
    listen: String,
    control: String,
}

async fn run(
    listener: Listener,
    app: axum::Router,
    stop: &orrery_server::Stop,
) -> std::io::Result<()> {
    match listener {
        Listener::Tcp(listener, _) => orrery_server::run(listener, app, stop.requested()).await,
        Listener::Unix(listener, _) => orrery_server::run(listener, app, stop.requested()).await,
    }
}

/// How long a stopping world gives the requests it is serving, and its git
/// processes, to finish (spec/surface-git.md, "Starting a world"). Short of
/// the ten seconds a container runtime commonly waits before it sends
/// SIGKILL, which no process can handle.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Resolves with the name of the first SIGTERM or SIGINT the process gets.
/// The handlers are installed when this is called, not when it is awaited.
fn stop_signal() -> anyhow::Result<impl Future<Output = &'static str>> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = signal(SignalKind::terminate()).context("handling SIGTERM")?;
    let mut int = signal(SignalKind::interrupt()).context("handling SIGINT")?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = int.recv() => "SIGINT",
        }
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    // stdout carries a command's output, which a shell may evaluate
    // (`eval "$(orrery env)"`) or a caller may parse (`--json`, the ready
    // line), so no log line may reach it.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve { config } => {
            serve(&config).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Create {
            instance,
            t0,
            clock,
            secret,
            json,
        } => report(
            json,
            create(&instance, t0.as_deref(), clock, secret),
            |made| {
                format!(
                    "created a world at {} with its {} clock at {} — serve it with a config whose instance.dir is {}",
                    made.world.display(),
                    clock.mode().as_str(),
                    rfc3339(made.clock_unix),
                    made.instance.display(),
                )
            },
        ),
        Command::Fork {
            instance,
            snapshot,
            world,
            dest,
            secret,
            json,
        } => {
            let source = match (instance, snapshot, world) {
                (_, _, Some(world)) => Source::World(world),
                (Some(instance), Some(snapshot), None) => Source::Snapshot { instance, snapshot },
                _ => unreachable!("clap requires --world, or --instance with --snapshot"),
            };
            let outcome = fork(&source, &dest, secret);
            report(json, outcome, |made| {
                format!(
                    "forked {source} into {} — serve it with a config whose instance.dir is {}",
                    made.world.display(),
                    made.instance.display(),
                )
            })
        }
        Command::Env { world } => {
            env(&world)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Log {
            command: LogCommand::Follow { after_seq, world },
        } => {
            let restored = world.follow_log(after_seq, &mut std::io::stdout().lock())?;
            eprintln!(
                "a snapshot restore ended the stream; the restored log's newest seq is {}",
                restored.newest_seq
            );
            Ok(ExitCode::from(RESTORED))
        }
    }
}

/// `@<seconds> <offset>` is git's internal date format, which every git
/// version reads as exactly that instant.
fn env(world: &control::Address) -> anyhow::Result<()> {
    let clock = world.clock()?;
    println!("export GIT_AUTHOR_DATE='@{clock} +0000'");
    println!("export GIT_COMMITTER_DATE='@{clock} +0000'");
    Ok(())
}

/// A new instance holding a world, as `create` and `fork` report it.
struct Made {
    instance: PathBuf,
    world: PathBuf,
    clock_unix: i64,
}

/// A `create` or `fork` failure, in the control plane's error terms. `field`
/// names the flag whose value it is about.
struct Failure {
    reason: &'static str,
    message: String,
    field: Option<&'static str>,
}

impl Failure {
    fn new(reason: &'static str, e: impl std::fmt::Display, field: &'static str) -> Failure {
        Failure {
            reason,
            message: format!("{e:#}"),
            field: Some(field),
        }
    }

    fn internal(e: impl std::fmt::Display) -> Failure {
        Failure {
            reason: "INTERNAL",
            message: format!("{e:#}"),
            field: None,
        }
    }
}

/// With `--json`, stdout holds exactly one object and a failure exits 1;
/// without it, a person reads the result.
fn report(
    json: bool,
    outcome: Result<Made, Failure>,
    describe: impl FnOnce(&Made) -> String,
) -> anyhow::Result<ExitCode> {
    match outcome {
        Ok(made) if json => println!(
            "{}",
            serde_json::json!({
                "instance": made.instance.to_string_lossy(),
                "world": made.world.to_string_lossy(),
                "clock_unix": made.clock_unix,
            })
        ),
        Ok(made) => println!("{}", describe(&made)),
        Err(failure) if json => {
            let mut error =
                serde_json::json!({ "reason": failure.reason, "message": failure.message });
            if let Some(field) = failure.field {
                error["field"] = field.into();
            }
            println!("{}", serde_json::json!({ "errors": [error] }));
            return Ok(ExitCode::FAILURE);
        }
        Err(failure) => anyhow::bail!(failure.message),
    }
    Ok(ExitCode::SUCCESS)
}

fn create(
    instance: &Path,
    t0: Option<&str>,
    clock: ClockChoice,
    secret: SecretChoice,
) -> Result<Made, Failure> {
    let clock = new_clock(clock, t0, ["--t0", "--clock real"])
        .map_err(|e| Failure::new("INVALID_ARGUMENT", e, "t0"))?;
    let instance = std::path::absolute(instance).map_err(Failure::internal)?;
    let world_root = instance.join("world");
    let secret = secret.secret().map_err(Failure::internal)?;
    let world = World::create_with_clock(&world_root, clock, &secret).map_err(|e| match e {
        WorldError::AlreadyExists(_) => Failure::new("ALREADY_EXISTS", e, "instance"),
        e => Failure::internal(e),
    })?;
    Ok(Made {
        instance,
        world: world_root,
        clock_unix: world.clock().map_err(Failure::internal)?,
    })
}

enum Source {
    Snapshot { instance: PathBuf, snapshot: String },
    World(PathBuf),
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Snapshot { instance, snapshot } => {
                write!(f, "snapshot {snapshot:?} of {}", instance.display())
            }
            Source::World(world) => write!(f, "{}", world.display()),
        }
    }
}

fn fork(source: &Source, dest: &Path, secret: SecretChoice) -> Result<Made, Failure> {
    let dest = std::path::absolute(dest).map_err(Failure::internal)?;
    let secret = secret.secret().map_err(Failure::internal)?;
    let (forked, source_field) = match source {
        Source::Snapshot { instance, snapshot } => (
            orrery_snapshot::fork(&instance.join("snapshots"), snapshot, &dest, &secret),
            "snapshot",
        ),
        Source::World(world) => (orrery_snapshot::fork_world(world, &dest, &secret), "world"),
    };
    let world_root = forked.map_err(|e| match e {
        SnapshotError::InvalidName(_) => Failure::new("INVALID_ARGUMENT", e, "snapshot"),
        SnapshotError::NotFound(_) | SnapshotError::World(WorldError::NotFound(_)) => {
            Failure::new("NOT_FOUND", e, source_field)
        }
        SnapshotError::DestinationExists(_) => Failure::new("ALREADY_EXISTS", e, "dest"),
        e => Failure::internal(e),
    })?;
    let clock_unix = World::open(&world_root)
        .and_then(|world| world.clock())
        .map_err(Failure::internal)?;
    Ok(Made {
        instance: dest,
        world: world_root,
        clock_unix,
    })
}

/// A new world's clock: `real`, starting at the host's time, or `manual`,
/// starting at `t0`. `names` are what the caller calls `t0` and the choice of
/// `real`: a real world has no T0 to set, so the two are refused together.
fn new_clock(clock: ClockChoice, t0: Option<&str>, names: [&str; 2]) -> anyhow::Result<NewClock> {
    let [t0_name, real_name] = names;
    match (clock, t0) {
        (ClockChoice::Real, Some(_)) => anyhow::bail!(
            "{t0_name} is refused with {real_name}: a real world's clock starts at the host's time"
        ),
        (ClockChoice::Real, None) => Ok(NewClock::Real),
        (ClockChoice::Manual, t0) => Ok(NewClock::Manual {
            t0_unix: t0_unix(t0, t0_name)?,
        }),
    }
}

/// A new world's T0: the RFC3339 instant `t0`, which the caller calls `name`,
/// or the fixed default.
fn t0_unix(t0: Option<&str>, name: &str) -> anyhow::Result<i64> {
    match t0 {
        Some(t0) => Ok(jiff::Timestamp::from_str(t0)
            .with_context(|| format!("{name} {t0:?} is not RFC3339"))?
            .as_second()),
        None => Ok(orrery_world::DEFAULT_T0_UNIX),
    }
}

fn rfc3339(clock_unix: i64) -> String {
    jiff::Timestamp::from_second(clock_unix)
        .map(|t| t.to_string())
        .unwrap_or_else(|_| format!("{clock_unix} (unix)"))
}

async fn serve(config_path: &std::path::Path) -> anyhow::Result<()> {
    // Before anything starts a process, which would inherit the passed
    // sockets.
    passed::close_on_exec();
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;
    // Of the bytes parsed here and never of the file again, so it identifies
    // what this process read however the file changes later.
    let config_sha256 = format!("{:x}", Sha256::digest(raw.as_bytes()));
    let config: Config = toml::from_str(&raw)
        .with_context(|| format!("parsing config {}", config_path.display()))?;
    config
        .server
        .check()
        .with_context(|| format!("checking config {}", config_path.display()))?;
    // Made before the world is opened, so that a certificate file it can't
    // use stops `serve` before it opens or creates one.
    // TODO(ORR-7): hand the client to the webhook sender, which posts each delivery with it.
    let outbound = orrery_outbound::Client::new(
        config.outbound.egress,
        config.outbound.extra_ca_certs.as_deref(),
    )
    .with_context(|| format!("checking config {}", config_path.display()))?;

    let clock = new_clock(
        config.instance.clock,
        config.instance.t0.as_deref(),
        ["instance.t0", "instance.clock = \"real\""],
    )
    .with_context(|| format!("checking config {}", config_path.display()))?;
    let world_root = config.instance.dir.join("world");
    let world = Arc::new(World::open_or_create(
        &world_root,
        clock,
        &config.instance.secret.secret()?,
    )?);

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
    // Two passed descriptors can be one socket, and the faces and the
    // control plane would then accept from one queue.
    if let (Listener::Unix(_, listen), Some(Listener::Unix(_, control))) = (&listener, &control) {
        anyhow::ensure!(
            listen != control,
            "server.listen and server.control_listen are both unix:{}: each needs a socket of its own",
            listen.display()
        );
    }
    // A push is recorded only by the process serving it, so a process that
    // died mid-push can have left refs its log does not describe. This runs
    // once every listener is bound, because a second `serve` of an instance
    // that is already served fails to bind a port or socket path the first
    // holds, and it must fail before it moves a ref the serving process has
    // moved and not yet recorded.
    orrery_face_git::restore_refs(&world)
        .context("putting the world's refs where its log puts them")?;

    tracing::info!(
        world = %world_root.display(),
        listen = %listener,
        control = %control.as_ref().map_or_else(|| format!("{listener}/_orrery"), ToString::to_string),
        %base_url,
        egress = %outbound.egress(),
        clock_unix = world.clock()?,
        "orrery serving"
    );
    let ready = serde_json::to_string(&Ready {
        base_url: &base_url,
        listen: listener.to_string(),
        control: control.as_ref().unwrap_or(&listener).to_string(),
    })?;
    let routers = orrery_server::routers(
        world,
        base_url,
        config.instance.dir.join("snapshots"),
        orrery_server::Options {
            anonymous_git_reads: config.server.anonymous_git_reads.unwrap_or(true),
            config_sha256: Some(config_sha256),
        },
    );
    let stop = routers.stop();
    // Installed before the ready line, so a caller that signals the world as
    // soon as it has read the line stops it the same way as any later.
    let signal = stop_signal()?;
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{ready}")
        .and_then(|()| stdout.flush())
        .context("writing the ready line to stdout")?;
    let serving = async {
        match control {
            None => run(listener, routers.combined(), &stop).await,
            Some(control) => {
                let (faces, control_plane) = routers.split();
                tokio::try_join!(
                    run(listener, faces, &stop),
                    run(control, control_plane, &stop)
                )
                .map(|_| ())
            }
        }
    };
    let mut serving = std::pin::pin!(serving);
    let signal = tokio::select! {
        served = &mut serving => return served.context("serving"),
        signal = signal => signal,
    };

    tracing::info!(signal, grace_secs = STOP_GRACE.as_secs(), "orrery stopping");
    // Both start now, so both end within the grace period, except that git
    // killed at its end still moves back the pushes it cut off. A connection
    // still open then is dropped as the process exits.
    let (_, drained) = tokio::join!(
        stop.stop(STOP_GRACE),
        tokio::time::timeout(STOP_GRACE, &mut serving)
    );
    match drained {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(error = %e, "a listener failed while stopping"),
        Err(_) => tracing::warn!("connections still open after the grace period were dropped"),
    }
    tracing::info!("orrery stopped");
    Ok(())
}
