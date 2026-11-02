//! Fetching one URL, extracting visible text and outgoing links, and applying policy.

use crate::policy::Policy;
use crate::robots;
use reqwest::header::CONTENT_TYPE;
use reqwest::redirect::Policy as RedirectPolicy;
use reqwest::{Client, Proxy};
use scraper::{Html, Selector};
use serde_json::json;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use url::Url;

/// Hard cap on how much of a response body we read.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Hard cap on stored visible text per page.
const MAX_TEXT_CHARS: usize = 8 * 1024;

static ANCHOR: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());
static TITLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("title").unwrap());

/// Shared, read-only state for all fetch tasks.
pub struct Ctx {
    pub client: Client,
    pub robots: robots::Cache,
    pub policy: Policy,
}

pub struct Page {
    pub url: Url,
    pub depth: u32,
    pub status: Option<u16>,
    pub title: Option<String>,
    /// Visible text, truncated. `None` for non-HTML, errors, and withheld pages.
    pub text: Option<String>,
    pub links: Vec<Url>,
    pub bytes: usize,
    pub security_score: u32,
    /// Matched the withhold list: no text or links kept.
    pub withheld: bool,
    pub elapsed: Duration,
    pub error: Option<String>,
}

// Outcomes are short-lived and passed by value once per URL; boxing the page would add
// an allocation for every fetch without a real size benefit.
#[allow(clippy::large_enum_variant)]
pub enum Outcome {
    Fetched(Page),
    /// Skipped because robots.txt disallows it; not written to output.
    Blocked,
}

impl Page {
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "url": self.url.as_str(),
            "depth": self.depth,
            "status": self.status,
            "title": self.title,
            "text": self.text,
            "security_score": self.security_score,
            "withheld": self.withheld,
            "bytes": self.bytes,
            "elapsed_ms": (self.elapsed.as_secs_f64() * 1000.0 * 10.0).round() / 10.0,
            "links": self.links.iter().map(Url::as_str).collect::<Vec<_>>(),
            "error": self.error,
        })
    }

    fn bare(url: Url, depth: u32, status: Option<u16>, elapsed: Duration) -> Self {
        Page {
            url,
            depth,
            status,
            title: None,
            text: None,
            links: Vec::new(),
            bytes: 0,
            security_score: 0,
            withheld: false,
            elapsed,
            error: None,
        }
    }
}

/// Builds the HTTP client. With `proxy` set (e.g. `socks5h://127.0.0.1:9050`), all
/// traffic goes through it; `socks5h` makes the proxy resolve DNS, so no lookups
/// leak to the local resolver.
pub fn build_client(user_agent: &str, timeout: Duration, proxy: Option<&str>) -> anyhow::Result<Client> {
    let mut b = Client::builder()
        .user_agent(user_agent)
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(30)))
        .redirect(RedirectPolicy::limited(5))
        .pool_max_idle_per_host(16)
        .tcp_nodelay(true);
    if let Some(p) = proxy {
        b = b.proxy(Proxy::all(p)?);
    }
    Ok(b.build()?)
}

pub async fn fetch_page(ctx: &Ctx, url: Url, depth: u32) -> Outcome {
    if !ctx.robots.allowed(&url).await {
        return Outcome::Blocked;
    }

    let started = Instant::now();
    let mut resp = match ctx.client.get(url.clone()).send().await {
        Ok(r) => r,
        Err(e) => {
            let mut p = Page::bare(url, depth, None, started.elapsed());
            p.error = Some(e.to_string());
            return Outcome::Fetched(p);
        }
    };

    let status = resp.status().as_u16();
    let is_html = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("html"));
    let final_url = resp.url().clone();

    // Non-HTML bodies are never read: no binaries, media or downloads.
    if !resp.status().is_success() || !is_html {
        return Outcome::Fetched(Page::bare(url, depth, Some(status), started.elapsed()));
    }

    let mut body = Vec::with_capacity(64 * 1024);
    let mut error = None;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(&chunk);
                if body.len() >= MAX_BODY_BYTES {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                error = Some(e.to_string());
                break;
            }
        }
    }

    let bytes = body.len();
    let extracted = tokio::task::spawn_blocking(move || extract(&body, &final_url))
        .await
        .unwrap_or_default();

    let mut page = Page::bare(url, depth, Some(status), started.elapsed());
    page.bytes = bytes;
    page.error = error;
    page.title = extracted.title;

    let lower = extracted.text.to_lowercase();
    if ctx.policy.is_withheld(&lower) {
        page.withheld = true;
        return Outcome::Fetched(page);
    }

    page.security_score = ctx.policy.focus_score(&lower);
    page.text = Some(truncate_chars(extracted.text, MAX_TEXT_CHARS));
    page.links = extracted.links;
    Outcome::Fetched(page)
}

#[derive(Default)]
struct Extracted {
    title: Option<String>,
    /// Whitespace-collapsed visible text, not yet truncated.
    text: String,
    links: Vec<Url>,
}

fn extract(body: &[u8], base: &Url) -> Extracted {
    let html = String::from_utf8_lossy(body);
    let doc = Html::parse_document(&html);

    let title = doc.select(&TITLE).next().and_then(|t| {
        let text = collapse(&t.text().collect::<Vec<_>>().join(" "));
        (!text.is_empty()).then_some(text)
    });

    let mut links = Vec::new();
    for a in doc.select(&ANCHOR) {
        let Some(href) = a.value().attr("href") else {
            continue;
        };
        let href = href.trim();
        if href.is_empty()
            || href.starts_with('#')
            || href.starts_with("javascript:")
            || href.starts_with("mailto:")
            || href.starts_with("tel:")
        {
            continue;
        }
        if let Ok(mut u) = base.join(href)
            && matches!(u.scheme(), "http" | "https")
        {
            u.set_fragment(None);
            links.push(u);
        }
    }

    Extracted {
        title,
        text: visible_text(&doc),
        links,
    }
}

/// Text nodes that are not inside script, style, noscript or template elements.
fn visible_text(doc: &Html) -> String {
    let mut raw = String::new();
    for node in doc.tree.root().descendants() {
        let Some(text) = node.value().as_text() else {
            continue;
        };
        let hidden = node.ancestors().any(|a| {
            a.value()
                .as_element()
                .is_some_and(|e| matches!(e.name(), "script" | "style" | "noscript" | "template"))
        });
        if !hidden {
            raw.push_str(text);
            raw.push(' ');
        }
    }
    collapse(&raw)
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(mut s: String, max_bytes: usize) -> String {
    if s.len() > max_bytes {
        let mut end = max_bytes;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/a/b").unwrap()
    }

    #[test]
    fn extracts_title_and_resolves_links() {
        let html = b"<html><head><title>  Hi\n there </title></head>
            <body><a href='/x'>x</a><a href='c#frag'>c</a><a href='mailto:a@b'>m</a>
            <a href='#top'>t</a><a href='https://other.org/y'>o</a></body></html>";
        let e = extract(html, &base());
        assert_eq!(e.title.as_deref(), Some("Hi there"));
        let got: Vec<&str> = e.links.iter().map(Url::as_str).collect();
        assert_eq!(
            got,
            vec![
                "https://example.com/x",
                "https://example.com/a/c",
                "https://other.org/y"
            ]
        );
    }

    #[test]
    fn visible_text_skips_script_and_style() {
        let html = b"<html><head><style>.a{color:red}</style></head><body>
            <p>Hello <b>exploit</b> world</p><script>var secret=1;</script>
            <noscript>enable js</noscript></body></html>";
        let e = extract(html, &base());
        assert_eq!(e.text, "Hello exploit world");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let s = "é".repeat(10); // 2 bytes each
        let t = truncate_chars(s, 5);
        assert_eq!(t, "éé");
    }
}
