//! A fast, polite, concurrent web crawler, with an optional Tor mode for `.onion`
//! sites and a text-only content policy.
//!
//! Architecture:
//! - One coordinator loop owns the frontier (BFS queue), the dedup set and the
//!   per-host politeness schedule. It never does network or parsing work itself.
//! - Each dispatched URL runs as its own task: robots check, HTTP GET, streamed
//!   body read (HTML only), and text/link extraction on the blocking pool.
//! - Results flow back to the coordinator, which applies the link scope, enqueues
//!   new links, and forwards records to a single writer task that emits JSON Lines.

mod fetch;
mod policy;
mod robots;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use url::Url;

use fetch::{Ctx, Outcome, Page};

const DEFAULT_TOR_PROXY: &str = "socks5h://127.0.0.1:9050";

#[derive(Parser, Debug, Clone)]
#[command(name = "crawler", version, about = "A fast, polite, concurrent web crawler")]
struct Args {
    /// Seed URLs to start from (must be .onion URLs when --tor is set)
    #[arg(required = true)]
    seeds: Vec<Url>,

    /// Route all traffic through a SOCKS5 proxy and restrict the crawl to .onion hosts.
    /// Without a value, uses the local Tor daemon at socks5h://127.0.0.1:9050.
    #[arg(long, num_args = 0..=1, default_missing_value = DEFAULT_TOR_PROXY, value_name = "PROXY_URL")]
    tor: Option<String>,

    /// Maximum requests in flight [default: 128, or 8 with --tor]
    #[arg(short, long)]
    concurrency: Option<usize>,

    /// Stop dispatching after this many pages
    #[arg(long, default_value_t = 1000)]
    max_pages: usize,

    /// Maximum link depth from a seed (0 = seeds only)
    #[arg(long, default_value_t = 3)]
    max_depth: u32,

    /// Minimum delay between requests to the same host, in ms [default: 200, or 1500 with --tor]
    #[arg(long)]
    delay_ms: Option<u64>,

    /// Per-request timeout in seconds [default: 10, or 60 with --tor]
    #[arg(long)]
    timeout: Option<u64>,

    /// Follow links to any host instead of only the seed hosts (ignored with --tor)
    #[arg(long)]
    any_host: bool,

    /// Skip robots.txt checks (only where you are authorized to crawl)
    #[arg(long)]
    ignore_robots: bool,

    /// File of terms, one per line. Pages containing any of them are withheld:
    /// their URL is recorded, but no text or links are kept.
    #[arg(long, value_name = "FILE")]
    withhold_file: Option<PathBuf>,

    /// File of extra security-focus terms, one per line, added to the built-in list
    #[arg(long, value_name = "FILE")]
    focus_file: Option<PathBuf>,

    /// Only write pages whose security-focus score is at least this (0 = write all)
    #[arg(long, default_value_t = 0)]
    min_score: u32,

    /// Output file for JSON Lines records ("-" for stdout)
    #[arg(short, long, default_value = "crawl.jsonl")]
    output: String,

    /// User-Agent header; the first token is also used for robots.txt matching
    #[arg(long, default_value = "RustCrawler/0.1 (+https://example.com/bot)")]
    user_agent: String,
}

/// Effective settings after applying Tor-specific defaults.
struct Settings {
    concurrency: usize,
    delay: Duration,
    timeout: Duration,
    onion_only: bool,
}

impl Args {
    fn settings(&self) -> Settings {
        let tor = self.tor.is_some();
        Settings {
            concurrency: self.concurrency.unwrap_or(if tor { 8 } else { 128 }),
            delay: Duration::from_millis(self.delay_ms.unwrap_or(if tor { 1500 } else { 200 })),
            timeout: Duration::from_secs(self.timeout.unwrap_or(if tor { 60 } else { 10 })),
            onion_only: tor,
        }
    }
}

#[derive(Default)]
struct Stats {
    dispatched: usize,
    ok: usize,
    failed: usize,
    blocked: usize,
    withheld: usize,
    below_min_score: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let settings = args.settings();
    if settings.concurrency == 0 {
        bail!("--concurrency must be at least 1");
    }
    if settings.onion_only {
        for seed in &args.seeds {
            if !is_onion(seed) {
                bail!("--tor mode only accepts .onion seeds; got {seed}");
            }
        }
        if let Some(proxy) = args.tor.as_deref() {
            check_proxy_reachable(proxy).await?;
        }
    }

    let client = fetch::build_client(&args.user_agent, settings.timeout, args.tor.as_deref())
        .context("building HTTP client")?;
    let agent_token = args
        .user_agent
        .split_whitespace()
        .next()
        .unwrap_or("crawler")
        .to_string();
    let policy = policy::Policy::load(args.withhold_file.as_deref(), args.focus_file.as_deref())?;
    let ctx = Arc::new(Ctx {
        robots: robots::Cache::new(client.clone(), agent_token, !args.ignore_robots),
        client,
        policy,
    });

    let (tx, rx) = mpsc::channel::<Page>(1024);
    let writer = tokio::spawn(write_output(args.output.clone(), rx));

    let started = Instant::now();
    let stats = crawl(&args, &settings, ctx, tx).await;
    let written = writer.await.context("writer task panicked")??;

    let secs = started.elapsed().as_secs_f64();
    eprintln!(
        "done: {} fetched ok, {} failed, {} blocked by robots.txt, {} withheld, {} below min score, {} records written in {:.2}s ({:.1} pages/s)",
        stats.ok,
        stats.failed,
        stats.blocked,
        stats.withheld,
        stats.below_min_score,
        written,
        secs,
        (stats.ok + stats.failed) as f64 / secs.max(f64::EPSILON),
    );
    Ok(())
}

fn is_onion(url: &Url) -> bool {
    url.host_str().is_some_and(|h| h.ends_with(".onion"))
}

/// Fails fast with a clear message if the SOCKS proxy is not listening.
async fn check_proxy_reachable(proxy: &str) -> Result<()> {
    let url = Url::parse(proxy).with_context(|| format!("invalid --tor proxy URL {proxy}"))?;
    let host = url.host_str().context("--tor proxy URL has no host")?;
    let port = url.port().unwrap_or(9050);
    match tokio::time::timeout(Duration::from_secs(3), TcpStream::connect((host, port))).await {
        Ok(Ok(_)) => Ok(()),
        _ => bail!(
            "Tor SOCKS proxy not reachable at {host}:{port}. Start Tor (e.g. `sudo systemctl start tor`) or pass --tor <URL>."
        ),
    }
}

async fn crawl(args: &Args, settings: &Settings, ctx: Arc<Ctx>, out: mpsc::Sender<Page>) -> Stats {
    let allowed_hosts: HashSet<String> = args
        .seeds
        .iter()
        .filter_map(|u| u.host_str().map(str::to_owned))
        .collect();

    let mut seen: HashSet<String, ahash::RandomState> = HashSet::default();
    let mut frontier: VecDeque<(Url, u32)> = VecDeque::new();
    for seed in &args.seeds {
        if seen.insert(seed.as_str().to_owned()) {
            frontier.push_back((seed.clone(), 0));
        }
    }

    // Next free start time per origin. Reserving a slot at dispatch time spaces
    // requests to one host without holding any lock or blocking other hosts.
    let mut next_slot: HashMap<String, Instant> = HashMap::new();
    let delay = settings.delay;

    let mut set: JoinSet<Outcome> = JoinSet::new();
    let mut stats = Stats::default();

    loop {
        while set.len() < settings.concurrency && stats.dispatched < args.max_pages {
            let Some((url, depth)) = frontier.pop_front() else {
                break;
            };
            let origin = url.origin().ascii_serialization();
            let now = Instant::now();
            let slot = next_slot.get(&origin).copied().unwrap_or(now).max(now);
            next_slot.insert(origin, slot + delay);
            stats.dispatched += 1;

            let ctx = ctx.clone();
            set.spawn(async move {
                tokio::time::sleep_until(slot.into()).await;
                fetch::fetch_page(&ctx, url, depth).await
            });
        }

        let Some(joined) = set.join_next().await else {
            break; // nothing in flight and nothing left to dispatch
        };
        let page = match joined {
            Ok(Outcome::Fetched(page)) => page,
            Ok(Outcome::Blocked) => {
                stats.blocked += 1;
                continue;
            }
            Err(e) => {
                eprintln!("worker task failed: {e}");
                stats.failed += 1;
                continue;
            }
        };

        if page.withheld {
            stats.withheld += 1;
        } else if page.error.is_none() && page.status.is_some_and(|s| (200..300).contains(&s)) {
            stats.ok += 1;
        } else {
            stats.failed += 1;
        }

        if page.depth < args.max_depth && !page.withheld {
            for link in &page.links {
                let in_scope = if settings.onion_only {
                    is_onion(link)
                } else {
                    args.any_host || link.host_str().is_some_and(|h| allowed_hosts.contains(h))
                };
                if !in_scope {
                    continue;
                }
                if seen.insert(link.as_str().to_owned()) {
                    frontier.push_back((link.clone(), page.depth + 1));
                }
            }
        }

        // Withheld pages are always recorded (URL and flag only); others must meet --min-score.
        if !page.withheld && page.security_score < args.min_score {
            stats.below_min_score += 1;
            continue;
        }
        if out.send(page).await.is_err() {
            eprintln!("output writer stopped; halting crawl");
            break;
        }
    }

    stats
}

async fn write_output(path: String, mut rx: mpsc::Receiver<Page>) -> Result<usize> {
    let sink: Box<dyn AsyncWrite + Unpin + Send> = if path == "-" {
        Box::new(tokio::io::stdout())
    } else {
        Box::new(
            tokio::fs::File::create(&path)
                .await
                .with_context(|| format!("creating {path}"))?,
        )
    };
    let mut out = BufWriter::with_capacity(64 * 1024, sink);

    let mut written = 0usize;
    while let Some(page) = rx.recv().await {
        let mut line = page.to_json().to_string();
        line.push('\n');
        out.write_all(line.as_bytes()).await?;
        written += 1;
    }
    out.flush().await?;
    Ok(written)
}
