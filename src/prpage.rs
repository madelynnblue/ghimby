//! A read-only snapshot of one PR (description, conversation, files, review threads), cached in SQLite.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value as J;

use crate::item::{Item, Value};
use crate::source::github::{API, GitHub, jts};
use crate::store::{Store, now};

#[derive(Serialize, Deserialize, Clone)]
pub struct PrPage {
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub draft: bool,
    pub author: String,
    pub base: String,
    pub head: String,
    /// Commit the diff was fetched at, so an approval covers exactly what was shown.
    #[serde(default)]
    pub head_oid: String,
    pub created: i64,
    /// GitHub's updatedAt, which moves on commits, comments, and reviews.
    pub updated: i64,
    pub body_html: String,
    pub additions: i64,
    pub deletions: i64,
    pub events: Vec<Event>,
    pub files: Vec<File>,
    pub threads: Vec<Thread>,
    /// Places where GitHub returned less than everything.
    pub notes: Vec<String>,
    pub fetched_at: i64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Event {
    /// "commented", or a review state such as APPROVED.
    pub kind: String,
    pub author: String,
    pub at: i64,
    pub body_html: String,
    pub url: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct File {
    pub path: String,
    pub previous: Option<String>,
    pub status: String,
    pub additions: i64,
    pub deletions: i64,
    /// Missing for binary files and diffs too large for the API.
    pub patch: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Thread {
    pub path: String,
    /// Line in the current diff; None when outdated.
    pub line: Option<i64>,
    pub start_line: Option<i64>,
    /// Anchored to the old side of the diff.
    pub left: bool,
    pub resolved: bool,
    pub outdated: bool,
    pub comments: Vec<Comment>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Comment {
    pub author: String,
    pub at: i64,
    pub body_html: String,
    pub url: String,
    pub diff_hunk: String,
}

pub fn key(repo: &str, number: i64) -> String {
    format!("{repo}#{number}")
}

/// The "owner/repo#N" key of a PR item.
pub fn item_key(it: &Item) -> Option<String> {
    match (it.get("type"), it.get("repo"), it.get("number")) {
        (Value::Str(t), Value::Str(r), Value::Int(n)) if t == "PullRequest" => Some(key(r, *n)),
        _ => None,
    }
}

/// Whether a page cached with these (updated, fetched_at) times misses changes the item knows of.
pub fn stale(times: (i64, i64), it: &Item) -> bool {
    let (updated, fetched) = times;
    let newer = |f: &str, than: i64| matches!(it.get(f), Value::Time(t) if *t > than);
    newer("last_activity", updated) || newer("updated", fetched)
}

pub fn load(store: &Store, key: &str) -> Option<PrPage> {
    serde_json::from_str(&store.pr_page(key).ok()??).ok()
}

pub async fn fetch_and_store(gh: &GitHub, store: &Store, repo: &str, number: i64) -> Result<PrPage> {
    let p = fetch(gh, repo, number).await?;
    store.put_pr_page(&key(repo, number), &serde_json::to_string(&p)?, p.updated, p.fetched_at)?;
    Ok(p)
}

const QUERY: &str = r#"
title url state isDraft author { login } baseRefName headRefName headRefOid createdAt updatedAt bodyHTML additions deletions
comments(last: 100) { totalCount nodes { author { login } createdAt bodyHTML url } }
reviews(last: 100) { totalCount nodes { author { login } submittedAt state bodyHTML url } }
reviewThreads(first: 100) {
  totalCount
  nodes {
    path line startLine diffSide isResolved isOutdated
    comments(first: 50) { totalCount nodes { author { login } createdAt bodyHTML url diffHunk } }
  }
}
"#;

fn author(v: &J) -> String {
    v["author"]["login"].as_str().unwrap_or("ghost").to_string()
}

fn s(v: &J) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn truncated(conn: &J, what: &str, notes: &mut Vec<String>) {
    let total = conn["totalCount"].as_i64().unwrap_or(0);
    let got = conn["nodes"].as_array().map_or(0, Vec::len) as i64;
    if total > got {
        notes.push(format!("Showing {got} of {total} {what}."));
    }
}

async fn fetch(gh: &GitHub, repo: &str, number: i64) -> Result<PrPage> {
    let (owner, name) = repo.split_once('/').ok_or_else(|| anyhow!("bad repo {repo:?}"))?;
    let token = gh.token().await?;
    let q = format!(
        "query {{ repository(owner: {}, name: {}) {{ pullRequest(number: {number}) {{ {QUERY} }} }} }}",
        serde_json::to_string(owner)?,
        serde_json::to_string(name)?,
    );
    let files_url = format!("{API}/repos/{repo}/pulls/{number}/files?per_page=100");
    let (body, files) = tokio::try_join!(gh.graphql(&token, &q), gh.get_all(&token, &files_url))?;
    let pr = &body["data"]["repository"]["pullRequest"];
    if pr.is_null() {
        return Err(anyhow!("{repo}#{number} is not a pull request"));
    }
    let mut notes = Vec::new();

    let mut events = Vec::new();
    for c in pr["comments"]["nodes"].as_array().into_iter().flatten() {
        events.push(Event {
            kind: "commented".into(),
            author: author(c),
            at: jts(&c["createdAt"]).unwrap_or(0),
            body_html: s(&c["bodyHTML"]),
            url: s(&c["url"]),
        });
    }
    for r in pr["reviews"]["nodes"].as_array().into_iter().flatten() {
        let body_html = s(&r["bodyHTML"]);
        let kind = s(&r["state"]);
        // Empty COMMENTED reviews only wrap inline comments, which show in the diff.
        if body_html.is_empty() && (kind == "COMMENTED" || kind == "PENDING") {
            continue;
        }
        events.push(Event { kind, author: author(r), at: jts(&r["submittedAt"]).unwrap_or(0), body_html, url: s(&r["url"]) });
    }
    events.sort_by_key(|e| e.at);
    truncated(&pr["comments"], "comments", &mut notes);
    truncated(&pr["reviews"], "reviews", &mut notes);

    let mut threads = Vec::new();
    for t in pr["reviewThreads"]["nodes"].as_array().into_iter().flatten() {
        truncated(&t["comments"], "comments in a thread", &mut notes);
        threads.push(Thread {
            path: s(&t["path"]),
            line: t["line"].as_i64(),
            start_line: t["startLine"].as_i64(),
            left: t["diffSide"] == "LEFT",
            resolved: t["isResolved"].as_bool().unwrap_or(false),
            outdated: t["isOutdated"].as_bool().unwrap_or(false),
            comments: t["comments"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| Comment {
                    author: author(c),
                    at: jts(&c["createdAt"]).unwrap_or(0),
                    body_html: s(&c["bodyHTML"]),
                    url: s(&c["url"]),
                    diff_hunk: s(&c["diffHunk"]),
                })
                .collect(),
        });
    }
    truncated(&pr["reviewThreads"], "review threads", &mut notes);

    let files = files
        .iter()
        .map(|f| File {
            path: s(&f["filename"]),
            previous: f["previous_filename"].as_str().map(str::to_string),
            status: s(&f["status"]),
            additions: f["additions"].as_i64().unwrap_or(0),
            deletions: f["deletions"].as_i64().unwrap_or(0),
            patch: f["patch"].as_str().map(str::to_string),
        })
        .collect();

    Ok(PrPage {
        repo: repo.to_string(),
        number,
        title: s(&pr["title"]),
        url: s(&pr["url"]),
        state: s(&pr["state"]).to_lowercase(),
        draft: pr["isDraft"].as_bool().unwrap_or(false),
        author: author(pr),
        base: s(&pr["baseRefName"]),
        head: s(&pr["headRefName"]),
        head_oid: s(&pr["headRefOid"]),
        created: jts(&pr["createdAt"]).unwrap_or(0),
        updated: jts(&pr["updatedAt"]).unwrap_or(0),
        body_html: s(&pr["bodyHTML"]),
        additions: pr["additions"].as_i64().unwrap_or(0),
        deletions: pr["deletions"].as_i64().unwrap_or(0),
        events,
        files,
        threads,
        notes,
        fetched_at: now(),
    })
}

#[derive(Debug, PartialEq)]
pub struct DiffLine {
    /// ' ', '+', '-', '@' for a hunk header, or '\\' for "No newline at end of file".
    pub kind: char,
    pub old: Option<i64>,
    pub new: Option<i64>,
    pub text: String,
}

/// Splits a unified diff patch into lines numbered on each side.
pub fn parse_patch(patch: &str) -> Vec<DiffLine> {
    let (mut old, mut new) = (0, 0);
    let mut out = Vec::new();
    for l in patch.lines() {
        let mut chars = l.chars();
        let kind = chars.next().unwrap_or(' ');
        let rest = chars.as_str();
        let line = match kind {
            '@' => {
                // "@@ -a,b +c,d @@ context"
                let mut parts = l.split(' ').skip(1);
                let start = |p: Option<&str>| p.and_then(|p| p[1..].split(',').next()?.parse().ok()).unwrap_or(1);
                old = start(parts.next());
                new = start(parts.next());
                DiffLine { kind, old: None, new: None, text: l.to_string() }
            }
            '+' => {
                new += 1;
                DiffLine { kind, old: None, new: Some(new - 1), text: rest.to_string() }
            }
            '-' => {
                old += 1;
                DiffLine { kind, old: Some(old - 1), new: None, text: rest.to_string() }
            }
            '\\' => DiffLine { kind, old: None, new: None, text: l.to_string() },
            _ => {
                old += 1;
                new += 1;
                DiffLine { kind: ' ', old: Some(old - 1), new: Some(new - 1), text: rest.to_string() }
            }
        };
        out.push(line);
    }
    out
}

/// Index of the diff line a thread is anchored to, if it's in the patch.
pub fn thread_anchor(lines: &[DiffLine], t: &Thread) -> Option<usize> {
    let n = t.line?;
    lines.iter().position(|l| if t.left { l.kind != '+' && l.old == Some(n) } else { l.kind != '-' && l.new == Some(n) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_numbers() {
        let p = "@@ -10,3 +10,4 @@ fn x\n a\n-b\n+c\n+d\n e\n\\ No newline at end of file";
        let l = parse_patch(p);
        let nums: Vec<_> = l.iter().map(|l| (l.kind, l.old, l.new)).collect();
        assert_eq!(
            nums,
            vec![
                ('@', None, None),
                (' ', Some(10), Some(10)),
                ('-', Some(11), None),
                ('+', None, Some(11)),
                ('+', None, Some(12)),
                (' ', Some(12), Some(13)),
                ('\\', None, None),
            ]
        );
        let t = |line, left| Thread { path: String::new(), line: Some(line), start_line: None, left, resolved: false, outdated: false, comments: vec![] };
        assert_eq!(thread_anchor(&l, &t(12, false)), Some(4));
        assert_eq!(thread_anchor(&l, &t(11, true)), Some(2));
        assert_eq!(thread_anchor(&l, &t(99, false)), None);
    }
}
