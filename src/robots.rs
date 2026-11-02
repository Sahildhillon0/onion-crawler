//! Minimal robots.txt support: per-origin cache, group selection by user-agent
//! token, and longest-match Allow/Disallow evaluation with `*` and `$` wildcards.

use reqwest::Client;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;
use url::Url;

/// A robots.txt group: the user-agent tokens it names, and its (is_allow, pattern) rules.
type Group = (Vec<String>, Vec<(bool, String)>);

#[derive(Default, Debug)]
pub struct Rules {
    /// (is_allow, pattern) pairs that apply to our user-agent.
    entries: Vec<(bool, String)>,
}

impl Rules {
    pub fn parse(text: &str, agent_token: &str) -> Self {
        // Each group: the user-agent tokens it names, and its allow/disallow rules.
        let mut groups: Vec<Group> = Vec::new();
        let mut in_rules = false;

        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            let Some((key, val)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let val = val.trim();
            match key.as_str() {
                "user-agent" => {
                    // Consecutive user-agent lines share a group; a user-agent line
                    // after rules starts a new one.
                    if in_rules || groups.is_empty() {
                        groups.push((Vec::new(), Vec::new()));
                        in_rules = false;
                    }
                    if let Some(g) = groups.last_mut() {
                        g.0.push(val.to_ascii_lowercase());
                    }
                }
                "allow" | "disallow" => {
                    in_rules = true;
                    if let Some(g) = groups.last_mut()
                        && !val.is_empty() {
                            g.1.push((key == "allow", val.to_string()));
                        }
                }
                _ => {}
            }
        }

        let agent = agent_token.to_ascii_lowercase();
        let specific: Vec<_> = groups
            .iter()
            .filter(|(agents, _)| agents.iter().any(|a| a != "*" && agent.contains(a.as_str())))
            .collect();
        let chosen: Vec<_> = if specific.is_empty() {
            groups
                .iter()
                .filter(|(agents, _)| agents.iter().any(|a| a == "*"))
                .collect()
        } else {
            specific
        };

        let entries = chosen
            .into_iter()
            .flat_map(|(_, rules)| rules.iter().cloned())
            .collect();
        Rules { entries }
    }

    /// Longest matching rule wins; on a tie, Allow wins. No match means allowed.
    pub fn allows(&self, path_and_query: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;
        for (allow, pattern) in &self.entries {
            if !glob_match(pattern, path_and_query) {
                continue;
            }
            let len = pattern.len();
            match best {
                Some((l, a)) if l > len || (l == len && (a || !allow)) => {}
                _ => best = Some((len, *allow)),
            }
        }
        best.is_none_or(|(_, allowed)| allowed)
    }
}

/// Prefix match by default; `*` matches any run of characters; trailing `$` anchors the end.
fn glob_match(pattern: &str, path: &str) -> bool {
    let (pat, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let parts: Vec<&str> = pat.split('*').collect();
    let first = parts[0];
    if !path.starts_with(first) {
        return false;
    }
    let mut pos = first.len();
    let last = parts.len() - 1;
    if last == 0 {
        return !anchored || pos == path.len();
    }
    for (i, part) in parts.iter().enumerate().skip(1) {
        if i == last && anchored {
            return path.len() >= pos + part.len() && path.ends_with(part);
        }
        if part.is_empty() {
            continue;
        }
        match path[pos..].find(part) {
            Some(off) => pos += off + part.len(),
            None => return false,
        }
    }
    true
}

/// Lazily fetches and caches robots.txt once per origin. Concurrent callers for the
/// same origin share one request via `OnceCell`.
pub struct Cache {
    client: Client,
    agent_token: String,
    enabled: bool,
    map: Mutex<HashMap<String, Arc<OnceCell<Rules>>>>,
}

impl Cache {
    pub fn new(client: Client, agent_token: String, enabled: bool) -> Self {
        Self {
            client,
            agent_token,
            enabled,
            map: Mutex::new(HashMap::new()),
        }
    }

    pub async fn allowed(&self, url: &Url) -> bool {
        if !self.enabled {
            return true;
        }
        let origin = url.origin().ascii_serialization();
        let cell = {
            let mut map = self.map.lock().expect("robots cache poisoned");
            map.entry(origin).or_default().clone()
        };
        let rules = cell.get_or_init(|| self.load(url)).await;
        let path = match url.query() {
            Some(q) => format!("{}?{}", url.path(), q),
            None => url.path().to_string(),
        };
        rules.allows(&path)
    }

    async fn load(&self, url: &Url) -> Rules {
        let mut robots_url = url.clone();
        robots_url.set_path("/robots.txt");
        robots_url.set_query(None);
        robots_url.set_fragment(None);
        match self.client.get(robots_url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.text().await {
                Ok(text) => Rules::parse(&text, &self.agent_token),
                Err(_) => Rules::default(),
            },
            // Missing or unreachable robots.txt means no restrictions.
            _ => Rules::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disallow_prefix_and_longest_match() {
        let r = Rules::parse(
            "User-agent: *\nDisallow: /private\nAllow: /private/ok\n",
            "RustCrawler",
        );
        assert!(!r.allows("/private/x"));
        assert!(r.allows("/private/ok/page"));
        assert!(r.allows("/public"));
    }

    #[test]
    fn specific_agent_group_overrides_star() {
        let txt = "User-agent: *\nDisallow: /\n\nUser-agent: RustCrawler\nDisallow: /tmp\n";
        let r = Rules::parse(txt, "RustCrawler/0.1");
        assert!(r.allows("/docs"));
        assert!(!r.allows("/tmp/a"));
    }

    #[test]
    fn wildcards_and_anchors() {
        let r = Rules::parse("User-agent: *\nDisallow: /*.pdf$\nDisallow: /search*q=\n", "X");
        assert!(!r.allows("/files/a.pdf"));
        assert!(r.allows("/files/a.pdf?x=1"));
        assert!(!r.allows("/search?q=rust"));
    }

    #[test]
    fn empty_disallow_allows_everything() {
        let r = Rules::parse("User-agent: *\nDisallow:\n", "X");
        assert!(r.allows("/anything"));
    }
}
