//! Content policy: a withhold list (operator-supplied) and a security-focus score.
//!
//! Both are plain case-insensitive substring matches over the visible text of a
//! page. They are coarse filters, not a guarantee: an operator is responsible for
//! reviewing what is crawled and stored.

use anyhow::{Context, Result};
use std::path::Path;

/// Terms that indicate security-research content. Scoring is substring-based,
/// so keep terms specific enough not to match everyday words.
const DEFAULT_FOCUS: &[&str] = &[
    "vulnerability",
    "vulnerabilities",
    "exploit",
    "cve-",
    "0day",
    "zero-day",
    "zero day",
    "malware",
    "ransomware",
    "rootkit",
    "pentest",
    "penetration test",
    "privilege escalation",
    "buffer overflow",
    "shellcode",
    "reverse engineering",
    "sql injection",
    "xss",
    "bug bounty",
    "infosec",
    "threat intel",
    "reverse shell",
    "fuzzing",
    "ctf",
];

pub struct Policy {
    withhold: Vec<String>,
    focus: Vec<String>,
}

impl Policy {
    /// `withhold_file` is one term per line; `extra_focus_file` adds to the built-in focus list.
    pub fn load(withhold_file: Option<&Path>, extra_focus_file: Option<&Path>) -> Result<Self> {
        let withhold = match withhold_file {
            Some(p) => read_terms(p)?,
            None => Vec::new(),
        };
        let mut focus: Vec<String> = DEFAULT_FOCUS.iter().map(|s| s.to_string()).collect();
        if let Some(p) = extra_focus_file {
            focus.extend(read_terms(p)?);
        }
        Ok(Self { withhold, focus })
    }

    /// `lower` must already be lowercased.
    pub fn is_withheld(&self, lower: &str) -> bool {
        self.withhold.iter().any(|t| lower.contains(t.as_str()))
    }

    /// Total occurrences of focus terms. `lower` must already be lowercased.
    pub fn focus_score(&self, lower: &str) -> u32 {
        self.focus
            .iter()
            .map(|t| lower.matches(t.as_str()).count() as u32)
            .sum()
    }
}

fn read_terms(path: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading term list {}", path.display()))?;
    Ok(text
        .lines()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(withhold: &[&str]) -> Policy {
        Policy {
            withhold: withhold.iter().map(|s| s.to_string()).collect(),
            focus: DEFAULT_FOCUS.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn focus_score_counts_matches() {
        let p = policy(&[]);
        assert_eq!(p.focus_score("new exploit for cve-2024-1 and another exploit"), 3);
        assert_eq!(p.focus_score("recipe for banana bread"), 0);
    }

    #[test]
    fn withhold_matches_any_term() {
        let p = policy(&["forbidden-term"]);
        assert!(p.is_withheld("this has a forbidden-term in it"));
        assert!(!p.is_withheld("nothing here"));
    }
}
