//! Path globs: `*` and `?` stay within a path segment, `**` crosses segments.

use regex::Regex;

/// Translates glob syntax to a regex body (no anchors).
pub fn to_regex_body(glob: &str) -> String {
    let mut s = String::new();
    let mut rest = glob;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("**/") {
            s.push_str("(?:.*/)?");
            rest = r;
        } else if let Some(r) = rest.strip_prefix("**") {
            s.push_str(".*");
            rest = r;
        } else {
            let c = rest.chars().next().unwrap();
            match c {
                '*' => s.push_str("[^/]*"),
                '?' => s.push_str("[^/]"),
                _ => s.push_str(&regex::escape(&c.to_string())),
            }
            rest = &rest[c.len_utf8()..];
        }
    }
    s
}

pub fn compile(glob: &str) -> Result<Regex, regex::Error> {
    Regex::new(&format!("^{}$", to_regex_body(glob)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        let g = compile("storage/**").unwrap();
        assert!(g.is_match("storage/a/b.go"));
        assert!(!g.is_match("platform/storage/a.go"));
        let g = compile("**/storage-*").unwrap();
        assert!(g.is_match("storage-x"));
        assert!(g.is_match("a/b/storage-x"));
        assert!(!g.is_match("a/storage/x"));
        let g = compile("*.go").unwrap();
        assert!(g.is_match("a.go"));
        assert!(!g.is_match("a/b.go"));
    }
}
