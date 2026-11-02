# crawler

A fast, polite, concurrent web crawler written in Rust (tokio + reqwest + scraper).

## Build

```sh
cargo build --release
./target/release/crawler --help
```

## Usage

```sh
# Crawl from a seed, staying on the seed's host, writing JSON Lines
crawler https://example.com --max-pages 500 --max-depth 4 -o pages.jsonl

# Tune throughput vs. politeness
crawler https://example.com -c 256 --delay-ms 100

# Several seeds, follow links to any host
crawler https://a.org https://b.org --any-host -o out.jsonl

# Pipe to stdout
crawler https://example.com -o - | jq .url
```

Each output line is one fetched URL:

```json
{"url":"https://example.com/","depth":0,"status":200,"title":"Example","bytes":1256,"elapsed_ms":84.3,"links":["https://www.iana.org/..."],"error":null}
```

## Design

- **Coordinator loop** owns the frontier (BFS queue), the dedup set and the per-host
  schedule. It does no network or parsing work itself.
- **One task per URL** does the robots check, the HTTP GET, a size-capped body read,
  and HTML parsing on the blocking pool so async workers stay free.
- **Politeness without locks:** each origin gets a reserved start slot
  (`last_slot + delay`). Hosts are throttled independently, so a slow host never
  stalls the others.
- **robots.txt:** fetched once per origin (shared `OnceCell`), group selection by
  user-agent token, longest-match Allow/Disallow with `*` and `$`. Missing robots.txt
  means no restrictions.
- **Output** goes through one buffered writer task, so workers never contend on I/O.
- Responses are capped at 4 MiB, redirects at 5, and every request has a timeout.

## Options

| Flag | Default | Meaning |
|---|---|---|
| `-c, --concurrency` | 128 | Max requests in flight across all hosts |
| `--max-pages` | 1000 | Stop dispatching after this many requests |
| `--max-depth` | 3 | Link depth from a seed |
| `--delay-ms` | 200 | Minimum gap between requests to the same host |
| `--timeout` | 10 | Per-request timeout (seconds) |
| `--any-host` | off | Follow links off the seed hosts |
| `--ignore-robots` | off | Skip robots.txt (only where you are authorized) |
| `-o, --output` | `crawl.jsonl` | Output file, or `-` for stdout |
| `--user-agent` | `RustCrawler/0.1 (...)` | User-Agent; first token is used for robots matching |

## Notes and limits

- The default `--delay-ms 200` is deliberate. Against a real site, politeness dominates
  wall-clock time: 1000 pages on one host takes at least 200 s. Use `--delay-ms 0` only
  for local or self-owned targets.
- The dedup set holds every discovered URL in memory. Very large crawls will need a
  bounded or on-disk set.
- JavaScript-rendered pages are not executed; only server-rendered HTML is parsed.

## Tests

```sh
cargo test          # robots.txt rules, link extraction
cargo clippy --all-targets
```

## Tor mode (`.onion` sites)

```sh
# Requires a running Tor SOCKS proxy (system service, or any listener on 9050)
sudo systemctl start tor
./target/release/crawler http://<address>.onion/ --tor -o onion.jsonl

# Custom proxy, with a withhold list and a security-focus threshold
./target/release/crawler http://<address>.onion/ --tor 127.0.0.1:9150 \
    --withhold-file withhold.txt --focus-file focus.txt --min-score 2
```

- `--tor` routes everything through SOCKS5 with DNS resolved by Tor (`socks5h`), and
  accepts only `.onion` seeds. Links to other hosts are never followed.
- Defaults shift to suit Tor: concurrency 8, 1500 ms per-host delay, 60 s timeout.
- The crawler checks that the proxy is listening before starting.
- It fetches HTML only and stores visible text (capped at 8 KB), never raw HTML,
  media, or files. Cookies are not stored and no forms are submitted.
- `--withhold-file` is an operator-supplied list. Matching pages are recorded as
  `withheld: true` with no text and no outgoing links followed. The crawler ships no
  built-in list; write one that fits your legal review.
- `--focus-file` adds terms to the built-in security list. `security_score` counts
  matches per page, and `--min-score` drops lower-scoring pages from the output while
  still traversing through them.
- Matching is substring-based and coarse. Review the output before relying on it.
