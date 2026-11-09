//! What a repository's lock costs: how long a fetch request holds it for
//! read, how long the pushes and merges behind it wait, how clones started
//! together fare, and how long fetches wait behind a large push. Each scenario
//! runs every `--engine` in turn, alone on the machine, against a world seeded
//! from the same payload. See docs/contributors/benchmarks/lock-bench.md.

use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use orrery_bench::git;
use orrery_bench::payload::{self, Rng};
use orrery_bench::serve::{self, Server};
use orrery_bench::stats::{Summary, Table, ms, timed};

#[derive(Parser)]
#[command(about = "Benchmark the per-repository lock against real git")]
struct Args {
    /// An engine build to serve worlds with, as NAME=PATH. Repeat it to
    /// compare builds; each scenario runs every engine in turn.
    #[arg(long = "engine", value_name = "NAME=PATH", required = true, value_parser = parse_engine)]
    engines: Vec<(String, PathBuf)>,
    #[arg(long, value_enum, default_value = "all")]
    scenario: Scenario,
    /// Repositories to seed, by name: `tiny`, `src<MB>`, `bin<MB>`, or both
    /// joined by `+`.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "tiny,src5,src40,src168,src168+bin150"
    )]
    payloads: Vec<String>,
    /// Clones per engine and payload in `hold`.
    #[arg(long, default_value_t = 5)]
    runs: usize,
    /// Rounds of push, open, and merge per phase in `writers`.
    #[arg(long, default_value_t = 20)]
    rounds: usize,
    /// Clients cloning back to back during `writers`' busy phase.
    #[arg(long, default_value_t = 2)]
    cloners: usize,
    /// Clones started together in `fanout`.
    #[arg(long, default_value_t = 4)]
    parallel: usize,
    /// Megabytes of random data in `big-push`'s push.
    #[arg(long, default_value_t = 150)]
    push_mb: usize,
    /// Seed for the payloads and the writers' pauses.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Where payloads are built and kept, and worlds are served from.
    #[arg(long, default_value = "target/lock-bench")]
    work: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Scenario {
    Hold,
    Writers,
    Fanout,
    BigPush,
    All,
}

fn parse_engine(arg: &str) -> Result<(String, PathBuf), String> {
    let (name, path) = arg
        .split_once('=')
        .ok_or_else(|| format!("{arg:?} is not NAME=PATH"))?;
    if name.is_empty() {
        return Err(format!("{arg:?} has no name"));
    }
    let path = std::path::absolute(path).map_err(|e| e.to_string())?;
    if !path.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    Ok((name.to_string(), path))
}

fn main() -> Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.work)?;
    let work = std::path::absolute(&args.work)?;
    header(&args)?;
    let run = |s: Scenario| args.scenario == s || args.scenario == Scenario::All;
    if run(Scenario::Hold) {
        hold(&args, &work)?;
    }
    if run(Scenario::Writers) {
        writers(&args, &work)?;
    }
    if run(Scenario::Fanout) {
        fanout(&args, &work)?;
    }
    if run(Scenario::BigPush) {
        big_push(&args, &work)?;
    }
    Ok(())
}

fn header(args: &Args) -> Result<()> {
    println!("lock-bench");
    for (name, path) in &args.engines {
        println!(
            "  engine {name}: {} ({})",
            path.display(),
            serve::version(path)
        );
    }
    let cpus = std::thread::available_parallelism().map_or(0, |n| n.get());
    let git = git::run(Path::new("."), &["--version"])?;
    println!("  {cpus} CPUs, {git}, seed {}", args.seed);
    println!();
    Ok(())
}

/// A served world for `engine`, with `payload` pushed as its `main`.
fn seeded(engine: &(String, PathBuf), payload: &Path, work: &Path) -> Result<Server> {
    let server = Server::start(&engine.0, &engine.1, work)?;
    git::run(payload, &["push", "-q", &server.url, "main"])
        .with_context(|| format!("seeding {}", payload.display()))?;
    Ok(server)
}

/// A bare clone over HTTP. Its wall time, and its longest POST from the
/// request going out to the response's status line. The server buffers each
/// answer whole before sending it, so that span is the pack request's time
/// under the repository's lock, plus sending the request and any wait for a
/// write to finish.
fn clone(server: &Server, dest: &Path) -> Result<(Duration, Duration)> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    let parent = dest
        .parent()
        .expect("a clone goes under the work directory");
    let (out, wall) = timed(|| {
        git::output(
            git::command(parent)
                .args(["clone", "-q", "--bare", &server.url])
                .arg(dest)
                .env("GIT_TRACE_CURL", "1")
                .env("GIT_TRACE_CURL_NO_DATA", "1"),
        )
    });
    let out = out?;
    std::fs::remove_dir_all(dest)?;
    Ok((wall, longest_post(&String::from_utf8_lossy(&out.stderr))))
}

fn longest_post(trace: &str) -> Duration {
    let stamp = |line: &str| -> Option<f64> {
        let (h, rest) = line.get(..15)?.split_once(':')?;
        let (m, s) = rest.split_once(':')?;
        Some(
            h.parse::<f64>().ok()? * 3600.0
                + m.parse::<f64>().ok()? * 60.0
                + s.parse::<f64>().ok()?,
        )
    };
    let mut longest = 0.0f64;
    let mut sent = None;
    for line in trace.lines() {
        let Some(t) = stamp(line) else { continue };
        if line.contains("=> Send header: POST ") {
            sent = Some(t);
        } else if line.contains("<= Recv header: HTTP/")
            && let Some(s) = sent.take()
        {
            longest = longest.max(t - s);
        }
    }
    Duration::from_secs_f64(longest.max(0.0))
}

fn summary(sample: &[Duration]) -> String {
    Summary::of(sample).map_or_else(|| "-".to_string(), |s| s.to_string())
}

fn hold(args: &Args, work: &Path) -> Result<()> {
    println!("== hold: one clone at a time");
    println!(
        "   pack request = the time a clone holds the repository's lock for read, no writes running"
    );
    let mut table = Table::new(&[
        "payload",
        "engine",
        "repo MB",
        "clone p50",
        "pack request p50 / p90 / max",
    ]);
    for name in &args.payloads {
        let payload = payload::build(work, name, args.seed)?;
        for engine in &args.engines {
            let server = seeded(engine, &payload, work)?;
            let mut walls = Vec::new();
            let mut packs = Vec::new();
            for _ in 0..args.runs {
                let (wall, pack) = clone(&server, &work.join("hold-clone"))?;
                walls.push(wall);
                packs.push(pack);
            }
            table.row(vec![
                name.clone(),
                engine.0.clone(),
                format!("{:.1}", server.repo_bytes()? as f64 / f64::from(1 << 20)),
                Summary::of(&walls).map_or("-".into(), |s| ms(s.p50)),
                summary(&packs),
            ]);
        }
    }
    table.print();
    Ok(())
}

/// Each round's three writes, timed.
#[derive(Default)]
struct Writes {
    push: Vec<Duration>,
    open: Vec<Duration>,
    merge: Vec<Duration>,
}

/// `rounds` rounds, each pushing a one-file branch off `main` as it was
/// seeded, opening a pull request for it, and merging it, after a pause of up
/// to 300 ms so the writes land at every point of a concurrent clone.
fn write_rounds(
    server: &Server,
    work: &Path,
    rounds: std::ops::Range<usize>,
    rng: &mut Rng,
) -> Result<Writes> {
    let dir = tempfile::tempdir_in(work)?;
    let repo = dir.path().join("writer.git");
    // Blobless and bare: each commit is the seeded tree plus one file, built
    // with plumbing, so the writer never fetches the payload's contents.
    git::output(
        git::command(dir.path())
            .args(["clone", "-q", "--bare", "--filter=blob:none", &server.url])
            .arg(&repo),
    )?;
    let base = git::run(&repo, &["rev-parse", "main"])?;
    let index = dir.path().join("index");
    let with_index =
        |args: &[&str]| git::stdout(git::command(&repo).args(args).env("GIT_INDEX_FILE", &index));
    let mut writes = Writes::default();
    for i in rounds {
        with_index(&["read-tree", &base])?;
        let blob = git::hash_blob(&repo, format!("{i}\n").as_bytes())?;
        with_index(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},w{i}.txt"),
        ])?;
        let tree = with_index(&["write-tree", "--missing-ok"])?;
        let commit = git::run(
            &repo,
            &["commit-tree", &tree, "-p", &base, "-m", &format!("w{i}")],
        )?;
        std::thread::sleep(Duration::from_millis(rng.below(300)));

        let (pushed, t) = timed(|| {
            git::run(
                &repo,
                &["push", "-q", "origin", &format!("{commit}:refs/heads/w{i}")],
            )
        });
        pushed?;
        writes.push.push(t);
        let (pull, t) = timed(|| {
            server.send(
                "POST",
                &server.api("/pulls"),
                serde_json::json!({ "title": format!("w{i}"), "head": format!("w{i}"), "base": "main" }),
            )
        });
        let number = pull?["number"]
            .as_i64()
            .context("opened pull has no number")?;
        writes.open.push(t);
        let (merged, t) = timed(|| {
            server.send(
                "PUT",
                &server.api(&format!("/pulls/{number}/merge")),
                serde_json::json!({}),
            )
        });
        merged?;
        writes.merge.push(t);
    }
    Ok(writes)
}

/// Run `f` while `n` clients clone the repository back to back; what it
/// returned, and each of those clones' pack-request time.
fn under_clones<T>(
    server: &Server,
    work: &Path,
    n: usize,
    f: impl FnOnce() -> Result<T>,
) -> Result<(T, Vec<Duration>)> {
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let stop = &stop;
        let cloners: Vec<_> = (0..n)
            .map(|i| {
                scope.spawn(move || -> Result<Vec<Duration>> {
                    let mut packs = Vec::new();
                    while !stop.load(Ordering::Relaxed) {
                        packs.push(clone(server, &work.join(format!("busy-clone-{i}")))?.1);
                    }
                    Ok(packs)
                })
            })
            .collect();
        let out = f();
        stop.store(true, Ordering::Relaxed);
        let mut packs = Vec::new();
        for cloner in cloners {
            packs.extend(cloner.join().expect("a cloner panicked")?);
        }
        Ok((out?, packs))
    })
}

fn writers(args: &Args, work: &Path) -> Result<()> {
    println!(
        "== writers: {} rounds of push, open, and merge, idle and while {} clients clone back to back",
        args.rounds, args.cloners
    );
    println!("   p50 / p90 / max, ms");
    let mut table = Table::new(&[
        "payload",
        "engine",
        "push idle",
        "push busy",
        "open busy",
        "merge busy",
        "clones' pack request, with waits",
    ]);
    for name in &args.payloads {
        let payload = payload::build(work, name, args.seed)?;
        for engine in &args.engines {
            let server = seeded(engine, &payload, work)?;
            let mut rng = Rng::new(args.seed);
            let idle = write_rounds(&server, work, 0..args.rounds, &mut rng)?;
            let (busy, packs) = under_clones(&server, work, args.cloners, || {
                write_rounds(&server, work, args.rounds..2 * args.rounds, &mut rng)
            })?;
            table.row(vec![
                name.clone(),
                engine.0.clone(),
                summary(&idle.push),
                summary(&busy.push),
                summary(&busy.open),
                summary(&busy.merge),
                format!("{} ({} clones)", summary(&packs), packs.len()),
            ]);
        }
    }
    table.print();
    Ok(())
}

fn fanout(args: &Args, work: &Path) -> Result<()> {
    println!("== fanout: {} clones started together", args.parallel);
    let mut table = Table::new(&[
        "payload",
        "engine",
        "all done, ms",
        "each clone p50 / p90 / max",
    ]);
    for name in &args.payloads {
        let payload = payload::build(work, name, args.seed)?;
        for engine in &args.engines {
            let server = seeded(engine, &payload, work)?;
            clone(&server, &work.join("fanout-warm"))?;
            let start = Barrier::new(args.parallel);
            let (walls, total) = timed(|| {
                std::thread::scope(|scope| {
                    let (server, start) = (&server, &start);
                    let clones: Vec<_> = (0..args.parallel)
                        .map(|i| {
                            scope.spawn(move || {
                                start.wait();
                                clone(server, &work.join(format!("fanout-{i}"))).map(|c| c.0)
                            })
                        })
                        .collect();
                    clones
                        .into_iter()
                        .map(|c| c.join().expect("a clone panicked"))
                        .collect::<Result<Vec<_>>>()
                })
            });
            let walls = walls?;
            table.row(vec![
                name.clone(),
                engine.0.clone(),
                ms(total),
                summary(&walls),
            ]);
        }
    }
    table.print();
    Ok(())
}

fn big_push(args: &Args, work: &Path) -> Result<()> {
    println!(
        "== big-push: `git ls-remote` back to back while one push of {} MB of random data runs",
        args.push_mb
    );
    let mut table = Table::new(&[
        "engine",
        "push s",
        "ls-remote idle",
        "ls-remote during push",
        "n",
    ]);
    let tiny = payload::build(work, "tiny", args.seed)?;
    let big = payload::build(work, &format!("bin{}", args.push_mb), args.seed)?;
    for engine in &args.engines {
        let server = seeded(engine, &tiny, work)?;
        let ls_remote = || -> Result<Duration> {
            let (out, t) = timed(|| git::run(work, &["ls-remote", &server.url]));
            out?;
            Ok(t)
        };
        let idle = (0..10).map(|_| ls_remote()).collect::<Result<Vec<_>>>()?;
        let done = AtomicBool::new(false);
        let (pushed, during) = std::thread::scope(|scope| {
            let reader = scope.spawn(|| -> Result<Vec<Duration>> {
                let mut lats = Vec::new();
                while !done.load(Ordering::Relaxed) {
                    lats.push(ls_remote()?);
                }
                Ok(lats)
            });
            let pushed =
                timed(|| git::run(&big, &["push", "-q", &server.url, "main:refs/heads/big"]));
            done.store(true, Ordering::Relaxed);
            (pushed, reader.join().expect("the reader panicked"))
        });
        let (out, push_time) = pushed;
        out?;
        let during = during?;
        if during.is_empty() {
            bail!("no ls-remote ran during the push");
        }
        table.row(vec![
            engine.0.clone(),
            format!("{:.1}", push_time.as_secs_f64()),
            summary(&idle),
            summary(&during),
            during.len().to_string(),
        ]);
    }
    table.print();
    Ok(())
}
