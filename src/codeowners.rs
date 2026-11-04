//! CODEOWNERS parsing and last-match lookup.

use regex::Regex;

use crate::glob;

pub struct Rule {
    re: Regex,
    /// Lowercased, without the leading `@`.
    pub owners: Vec<String>,
}

pub struct CodeOwners {
    rules: Vec<Rule>,
}

pub fn normalize_owner(s: &str) -> String {
    s.trim().trim_start_matches('@').to_lowercase()
}

fn pattern_regex(pat: &str) -> Option<Regex> {
    // No slash except a trailing one means the pattern matches at any depth.
    let anchored = pat.starts_with('/') || pat.trim_end_matches('/').contains('/');
    let p = pat.trim_start_matches('/');
    let dir = p.ends_with('/');
    let p = p.trim_end_matches('/');
    if p == "*" {
        return Regex::new("^.*$").ok();
    }
    let prefix = if anchored { "^" } else { "^(?:.*/)?" };
    let suffix = if dir { "/.*$" } else { "(?:/.*)?$" };
    Regex::new(&format!("{prefix}{}{suffix}", glob::to_regex_body(p))).ok()
}

impl CodeOwners {
    pub fn parse(text: &str) -> CodeOwners {
        let rules = text
            .lines()
            .filter_map(|line| {
                let line = line.split('#').next()?.trim();
                let mut parts = line.split_whitespace();
                let pattern = parts.next()?;
                Some(Rule {
                    re: pattern_regex(pattern)?,
                    owners: parts.map(normalize_owner).collect(),
                })
            })
            .collect();
        CodeOwners { rules }
    }

    /// The last rule matching `path`, which is the one GitHub applies.
    pub fn lookup(&self, path: &str) -> Option<&Rule> {
        self.rules.iter().rev().find(|r| r.re.is_match(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "
* @acme/Platform @acme/Cloud
/platform/ @acme/Platform
storage/ @acme/Storage
storage/go.mod
**/storage_* @acme/Storage
/blobs/ @acme/Storage
components/storage-* @acme/storage
";

    fn owners(co: &CodeOwners, p: &str) -> Vec<String> {
        co.lookup(p).map(|r| r.owners.clone()).unwrap_or_default()
    }

    #[test]
    fn last_match_wins() {
        let co = CodeOwners::parse(SAMPLE);
        assert_eq!(owners(&co, "README.md"), ["acme/platform", "acme/cloud"]);
        assert_eq!(owners(&co, "platform/x/y.go"), ["acme/platform"]);
        assert_eq!(owners(&co, "storage/api/x.go"), ["acme/storage"]);
        assert_eq!(owners(&co, "platform/db/storage/x.go"), ["acme/storage"]);
        assert!(owners(&co, "storage/go.mod").is_empty());
        assert_eq!(owners(&co, "platform/config/storage_x.go"), ["acme/storage"]);
        assert_eq!(owners(&co, "blobs/b.go"), ["acme/storage"]);
        assert_eq!(owners(&co, "components/storage-node/x.yaml"), ["acme/storage"]);
        assert_eq!(owners(&co, "x/blobs/b.go"), ["acme/platform", "acme/cloud"]);
    }
}
