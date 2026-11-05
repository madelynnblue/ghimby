//! GitHub notifications, enriched with PR/issue details and CODEOWNERS.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use chrono::DateTime;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use reqwest::header::HeaderMap;
use serde_json::{Value as J, json};

use super::{Source, Status, set_message};
use crate::codeowners::{CodeOwners, normalize_owner};
use crate::item::{Item, Value};
use crate::rules::FieldDoc;
use crate::store::{Store, now};

pub const API: &str = "https://api.github.com";
const SUBJECT_BATCH: usize = 25;
const SUBJECT_CONCURRENCY: usize = 4;
const CODEOWNERS_BATCH: usize = 20;

pub const FIELDS: &[FieldDoc] = &[
    FieldDoc { name: "source", help: "\"github\"" },
    FieldDoc { name: "type", help: "Notification subject type: PullRequest, Issue, Release, ..." },
    FieldDoc { name: "reason", help: "Why you were notified: review_requested, mention, team_mention, comment, ..." },
    FieldDoc { name: "unread", help: "bool" },
    FieldDoc { name: "repo", help: "owner/name" },
    FieldDoc { name: "number", help: "PR or issue number" },
    FieldDoc { name: "title", help: "string" },
    FieldDoc { name: "updated", help: "Time of the latest notification update" },
    FieldDoc { name: "created", help: "Time the PR or issue was opened" },
    FieldDoc { name: "last_activity", help: "Time the PR or issue last changed on GitHub (commits, comments, reviews)" },
    FieldDoc { name: "state", help: "open, closed, or merged" },
    FieldDoc { name: "draft", help: "bool" },
    FieldDoc { name: "author", help: "Login of the PR or issue author" },
    FieldDoc { name: "labels", help: "List of label names" },
    FieldDoc { name: "paths", help: "Changed file paths (first 100)" },
    FieldDoc { name: "changed_files", help: "int" },
    FieldDoc { name: "additions", help: "int" },
    FieldDoc { name: "deletions", help: "int" },
    FieldDoc { name: "requested_users", help: "Logins with a pending review request (lowercase)" },
    FieldDoc { name: "requested_teams", help: "org/team slugs with a pending review request (lowercase)" },
    FieldDoc { name: "requested_for_me", help: "A review is currently requested from you by name (not via a team)" },
    FieldDoc { name: "reviewed_by_me", help: "You have submitted a review" },
    FieldDoc {
        name: "review_decision",
        help: "approved, changes_requested, review_required, or none. GitHub's decision under branch protection, else from each reviewer's latest approve/request-changes review",
    },
    FieldDoc { name: "approved_by", help: "Logins whose latest opinionated review approves (lowercase)" },
    FieldDoc { name: "changes_requested_by", help: "Logins whose latest opinionated review requests changes (lowercase)" },
    FieldDoc {
        name: "new_since_my_review",
        help: "Since your latest review: a new commit or force-push, a comment or review by another human, or a review re-request for you or your teams",
    },
    FieldDoc { name: "codeowners", help: "Per changed file, the owners of its last-matching CODEOWNERS line. Use owned_by()." },
];

pub struct GitHub {
    store: Store,
    http: reqwest::Client,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SubjectRef {
    owner: String,
    repo: String,
    number: u64,
}

impl SubjectRef {
    fn key(&self) -> String {
        format!("{}/{}#{}", self.owner, self.repo, self.number)
    }

    /// Parses an API subject URL like https://api.github.com/repos/o/r/pulls/1.
    fn from_api_url(url: &str) -> Option<SubjectRef> {
        let rest = url.strip_prefix("https://api.github.com/repos/")?;
        let parts: Vec<&str> = rest.split('/').collect();
        match parts.as_slice() {
            [owner, repo, "pulls" | "issues", n] => Some(SubjectRef {
                owner: owner.to_string(),
                repo: repo.to_string(),
                number: n.parse().ok()?,
            }),
            _ => None,
        }
    }
}

fn ts(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp())
}

pub(crate) fn jts(v: &J) -> Option<i64> {
    v.as_str().and_then(ts)
}

fn login(v: &J) -> Option<String> {
    v.get("login").and_then(J::as_str).map(str::to_lowercase)
}

fn is_bot(actor: &J) -> bool {
    actor.get("__typename").and_then(J::as_str) == Some("Bot")
}

fn next_link(h: &HeaderMap) -> Option<String> {
    let link = h.get("link")?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"")
            .then(|| url.trim().trim_start_matches('<').trim_end_matches('>').to_string())
    })
}

impl GitHub {
    pub fn new(store: Store) -> GitHub {
        GitHub {
            store,
            http: reqwest::Client::builder()
                .user_agent("ghimby")
                .build()
                .expect("http client"),
        }
    }

    pub(crate) async fn token(&self) -> Result<String> {
        let t = self.store.setting("github_token")?;
        if !t.trim().is_empty() {
            return Ok(t.trim().to_string());
        }
        let cmd = self.store.setting("github_token_cmd")?;
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .await
            .with_context(|| format!("running token command {cmd:?}"))?;
        if !out.status.success() {
            bail!(
                "token command {cmd:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let t = String::from_utf8(out.stdout)?.trim().to_string();
        if t.is_empty() {
            bail!("token command {cmd:?} printed nothing");
        }
        Ok(t)
    }

    async fn get(&self, token: &str, url: &str, extra: HeaderMap) -> Result<reqwest::Response> {
        let resp = self
            .http
            .get(url)
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json")
            .headers(extra)
            .send()
            .await?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_MODIFIED {
            let status = resp.status();
            bail!("GET {url}: {status}: {}", resp.text().await.unwrap_or_default());
        }
        Ok(resp)
    }

    pub(crate) async fn get_all(&self, token: &str, first: &str) -> Result<Vec<J>> {
        let mut out = Vec::new();
        let mut url = Some(first.to_string());
        while let Some(u) = url {
            let resp = self.get(token, &u, HeaderMap::new()).await?;
            url = next_link(resp.headers());
            let page: Vec<J> = resp.json().await?;
            out.extend(page);
        }
        Ok(out)
    }

    pub(crate) async fn graphql(&self, token: &str, query: &str) -> Result<J> {
        let resp = self
            .http
            .post(format!("{API}/graphql"))
            .bearer_auth(token)
            .json(&json!({ "query": query }))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        let Ok(body) = serde_json::from_str::<J>(&text) else {
            bail!("graphql: {status}: {}", text.chars().take(200).collect::<String>());
        };
        if !status.is_success() {
            bail!("graphql: {status}: {body}");
        }
        if body.get("data").is_none_or(J::is_null) {
            bail!("graphql: {}", body.get("errors").unwrap_or(&body));
        }
        Ok(body)
    }

    /// Updates the stored notification list. Returns false if nothing changed.
    /// Merges notifications updated since the last sync (or the last `sync_days` days on the first
    /// sync), then drops ones older than that window.
    async fn sync_notifications(&self, token: &str) -> Result<()> {
        let days = self.store.setting_i64("sync_days")?.max(1);
        let started = chrono::Utc::now();
        let window_start = started - chrono::Duration::days(days);
        let cursor = self.store.setting("github.notifications_since")?;
        // Overlap the previous sync slightly so nothing updated mid-request is missed.
        let since = DateTime::parse_from_rfc3339(&cursor)
            .map(|c| c.to_utc() - chrono::Duration::minutes(1))
            .unwrap_or(window_start)
            .max(window_start);
        let first = format!(
            "{API}/notifications?all=true&per_page=50&since={}",
            since.format("%Y-%m-%dT%H:%M:%SZ")
        );
        let all = self.get_all(token, &first).await?;
        let rows: Vec<(String, String, i64)> = all
            .iter()
            .map(|n| {
                (
                    n["id"].as_str().unwrap_or_default().to_string(),
                    n.to_string(),
                    jts(&n["updated_at"]).unwrap_or(0),
                )
            })
            .collect();
        self.store.upsert_notifications(&rows)?;
        self.store.prune_notifications(window_start.timestamp())?;
        self.store
            .set_setting("github.notifications_since", &started.to_rfc3339())?;
        Ok(())
    }

    async fn sync_identity(&self, token: &str) -> Result<()> {
        let user: J = self.get(token, &format!("{API}/user"), HeaderMap::new()).await?.json().await?;
        let me = login(&user).ok_or_else(|| anyhow!("no login in /user"))?;
        let teams: Vec<String> = self
            .get_all(token, &format!("{API}/user/teams?per_page=100"))
            .await?
            .iter()
            .filter_map(|t| {
                Some(format!(
                    "{}/{}",
                    t["organization"]["login"].as_str()?,
                    t["slug"].as_str()?
                ))
            })
            .map(|s| s.to_lowercase())
            .collect();
        self.store.set_setting("github.login", &me)?;
        self.store
            .set_setting("github.teams", &serde_json::to_string(&teams)?)?;
        Ok(())
    }

    fn subject_query(me: &str, refs: &[SubjectRef]) -> String {
        let me = serde_json::to_string(me).unwrap();
        let mut q = String::from("query {");
        for (i, r) in refs.iter().enumerate() {
            q += &format!(
                r#" s{i}: repository(owner: {}, name: {}) {{ issueOrPullRequest(number: {}) {{ {SUBJECT_FRAGMENT} }} }}"#,
                serde_json::to_string(&r.owner).unwrap(),
                serde_json::to_string(&r.repo).unwrap(),
                r.number,
            );
        }
        q.push('}');
        q.replace("$ME", &me)
    }

    async fn fetch_subjects(&self, token: &str, me: &str, refs: Vec<SubjectRef>) -> Result<()> {
        let q = Self::subject_query(me, &refs);
        let body = self.graphql(token, &q).await?;
        for (i, r) in refs.iter().enumerate() {
            let node = &body["data"][format!("s{i}")]["issueOrPullRequest"];
            self.store.put_subject(&r.key(), &node.to_string())?;
        }
        Ok(())
    }

    async fn sync_subjects(&self, token: &str, status: &Status) -> Result<()> {
        let me = self.store.setting("github.login")?;
        let refresh = self.store.setting_i64("subject_refresh_secs")?;
        let cached: HashMap<String, (J, i64)> = self
            .store
            .subjects()?
            .into_iter()
            .map(|(k, j, t)| (k, (serde_json::from_str(&j).unwrap_or(J::Null), t)))
            .collect();
        let now = now();
        let mut want = Vec::new();
        let mut seen = HashSet::new();
        for n in self.store.notifications()? {
            let n: J = serde_json::from_str(&n)?;
            let Some(r) = n["subject"]["url"].as_str().and_then(SubjectRef::from_api_url) else {
                continue;
            };
            if !seen.insert(r.clone()) {
                continue;
            }
            let updated = jts(&n["updated_at"]).unwrap_or(0);
            let stale = match cached.get(&r.key()) {
                None => true,
                Some((j, fetched)) => {
                    updated > *fetched
                        || (j["prState"] == "OPEN" || j["issueState"] == "OPEN")
                            && now - fetched > refresh
                }
            };
            if stale {
                want.push(r);
            }
        }
        let total = want.len();
        let batches: Vec<Vec<SubjectRef>> = want.chunks(SUBJECT_BATCH).map(<[_]>::to_vec).collect();
        let mut done = 0;
        let mut errors = Vec::new();
        let mut stream = futures::stream::iter(batches)
            .map(|b| {
                let n = b.len();
                self.fetch_subjects(token, &me, b).map(move |r| (n, r))
            })
            .buffer_unordered(SUBJECT_CONCURRENCY);
        while let Some((n, r)) = stream.next().await {
            done += n;
            if let Err(e) = r {
                errors.push(e.to_string());
            }
            set_message(status, format!("fetched PR/issue details {done}/{total}"));
        }
        if let Some(e) = errors.first() {
            bail!("{} of the detail batches failed, first: {e}", errors.len());
        }
        Ok(())
    }

    async fn sync_codeowners(&self, token: &str) -> Result<()> {
        let refresh = self.store.setting_i64("codeowners_refresh_secs")?;
        let cached: HashMap<String, i64> = self
            .store
            .codeowners()?
            .into_iter()
            .map(|(r, _, t)| (r, t))
            .collect();
        let now = now();
        let mut repos: Vec<String> = self
            .store
            .notifications()?
            .iter()
            .filter_map(|n| serde_json::from_str::<J>(n).ok())
            .filter(|n| n["subject"]["type"] == "PullRequest")
            .filter_map(|n| n["repository"]["full_name"].as_str().map(String::from))
            .collect::<HashSet<_>>()
            .into_iter()
            .filter(|r| cached.get(r).is_none_or(|t| now - t > refresh))
            .collect();
        repos.sort();
        for chunk in repos.chunks(CODEOWNERS_BATCH) {
            let mut q = String::from("query {");
            for (i, full) in chunk.iter().enumerate() {
                let (o, n) = full.split_once('/').unwrap_or((full, ""));
                q += &format!(
                    r#" r{i}: repository(owner: {}, name: {}) {{
                        a: object(expression: "HEAD:.github/CODEOWNERS") {{ ... on Blob {{ text }} }}
                        b: object(expression: "HEAD:CODEOWNERS") {{ ... on Blob {{ text }} }}
                        c: object(expression: "HEAD:docs/CODEOWNERS") {{ ... on Blob {{ text }} }}
                    }}"#,
                    serde_json::to_string(o)?,
                    serde_json::to_string(n)?,
                );
            }
            q.push('}');
            let body = self.graphql(token, &q).await?;
            for (i, full) in chunk.iter().enumerate() {
                let r = &body["data"][format!("r{i}")];
                // GitHub uses the first of these that exists.
                let text = ["a", "b", "c"]
                    .iter()
                    .find_map(|k| r[k]["text"].as_str());
                self.store.put_codeowners(full, text)?;
            }
        }
        Ok(())
    }

    /// Current state (OPEN, CLOSED, MERGED) of each PR, keyed by "owner/repo#N".
    pub async fn pr_states(&self, prs: &[(String, i64)]) -> Result<HashMap<String, String>> {
        let mut out = HashMap::new();
        if prs.is_empty() {
            return Ok(out);
        }
        let token = self.token().await?;
        for chunk in prs.chunks(50) {
            let mut q = String::from("query {");
            for (i, (repo, n)) in chunk.iter().enumerate() {
                let (o, r) = repo.split_once('/').unwrap_or((repo, ""));
                q += &format!(
                    " p{i}: repository(owner: {}, name: {}) {{ pullRequest(number: {n}) {{ state }} }}",
                    serde_json::to_string(o)?,
                    serde_json::to_string(r)?,
                );
            }
            q.push('}');
            let body = self.graphql(&token, &q).await?;
            for (i, (repo, n)) in chunk.iter().enumerate() {
                if let Some(s) = body["data"][format!("p{i}")]["pullRequest"]["state"].as_str() {
                    out.insert(format!("{repo}#{n}"), s.to_string());
                }
            }
        }
        Ok(out)
    }

    async fn run_sync(&self, status: &Status) -> Result<()> {
        set_message(status, "getting token");
        let token = self.token().await?;
        set_message(status, "fetching identity");
        self.sync_identity(&token).await?;
        set_message(status, "fetching notifications");
        self.sync_notifications(&token).await?;
        set_message(status, "fetching CODEOWNERS");
        self.sync_codeowners(&token).await?;
        set_message(status, "fetching PR/issue details");
        self.sync_subjects(&token, status).await?;
        Ok(())
    }
}

/// The PR's review decision and who currently approves or requests changes.
/// Without branch protection GitHub's reviewDecision is null, so derive it from latest reviews.
fn review_decision(pr: &J) -> (&'static str, Vec<String>, Vec<String>) {
    let (mut approved, mut changes) = (Vec::new(), Vec::new());
    for r in pr["latestOpinionatedReviews"]["nodes"].as_array().into_iter().flatten() {
        let Some(l) = login(&r["author"]) else { continue };
        match r["state"].as_str() {
            Some("APPROVED") => approved.push(l),
            Some("CHANGES_REQUESTED") => changes.push(l),
            _ => {}
        }
    }
    let decision = match pr["reviewDecision"].as_str() {
        Some("APPROVED") => "approved",
        Some("CHANGES_REQUESTED") => "changes_requested",
        Some("REVIEW_REQUIRED") => "review_required",
        _ if !changes.is_empty() => "changes_requested",
        _ if !approved.is_empty() => "approved",
        _ => "none",
    };
    (decision, approved, changes)
}

/// Whether `me` reviewed, and whether anything new happened since `me`'s latest review.
fn review_activity(pr: &J, me: &str, my_teams: &[String]) -> (bool, bool) {
    let Some(last) = pr["myReviews"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| jts(&r["submittedAt"]))
        .max()
    else {
        return (false, false);
    };
    let others = |actor: &J| !is_bot(actor) && login(actor).is_some_and(|l| l != me);
    let new = pr["timelineItems"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|ev| {
            let after = |field: &J| jts(field).is_some_and(|t| t > last);
            match ev["__typename"].as_str().unwrap_or("") {
                "PullRequestCommit" => {
                    after(&ev["commit"]["committedDate"])
                        && login(&ev["commit"]["author"]["user"]).as_deref() != Some(me)
                }
                "HeadRefForcePushedEvent" => after(&ev["createdAt"]) && others(&ev["actor"]),
                "IssueComment" => after(&ev["createdAt"]) && others(&ev["author"]),
                "PullRequestReview" => after(&ev["submittedAt"]) && others(&ev["author"]),
                "ReviewRequestedEvent" => {
                    let rr = &ev["requestedReviewer"];
                    let target = login(rr).or_else(|| {
                        rr["combinedSlug"].as_str().map(str::to_lowercase)
                    });
                    after(&ev["createdAt"])
                        && target.is_some_and(|t| t == me || my_teams.contains(&t))
                }
                _ => false,
            }
        });
    (true, new)
}

fn strs_at(v: &J, list: &str, field: &str) -> Vec<String> {
    v[list]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n[field].as_str().map(String::from))
        .collect()
}

fn html_url(api_url: &str) -> String {
    api_url
        .replace("https://api.github.com/repos/", "https://github.com/")
        .replace("/pulls/", "/pull/")
}

impl Source for GitHub {
    fn name(&self) -> &'static str {
        "github"
    }

    fn fields(&self) -> &'static [FieldDoc] {
        FIELDS
    }

    fn sync<'a>(&'a self, status: &'a Status) -> BoxFuture<'a, Result<()>> {
        self.run_sync(status).boxed()
    }

    fn reset_sync(&self) -> Result<()> {
        self.store.clear_notifications()?;
        self.store.set_setting("github.notifications_since", "")
    }

    fn items(&self) -> Result<Vec<Item>> {
        let me = self.store.setting("github.login")?;
        let teams: Vec<String> =
            serde_json::from_str(&self.store.setting("github.teams")?).unwrap_or_default();
        let subjects: HashMap<String, J> = self
            .store
            .subjects()?
            .into_iter()
            .map(|(k, j, _)| (k, serde_json::from_str(&j).unwrap_or(J::Null)))
            .collect();
        let owners: HashMap<String, CodeOwners> = self
            .store
            .codeowners()?
            .into_iter()
            .filter_map(|(r, t, _)| Some((r, CodeOwners::parse(&t?))))
            .collect();
        let mut items = Vec::new();
        for n in self.store.notifications()? {
            let n: J = serde_json::from_str(&n)?;
            let repo = n["repository"]["full_name"].as_str().unwrap_or("").to_string();
            let short = n["repository"]["name"].as_str().unwrap_or("").to_string();
            let subject_url = n["subject"]["url"].as_str().unwrap_or("");
            let r = SubjectRef::from_api_url(subject_url);
            let mut it = Item {
                id: n["id"].as_str().unwrap_or("").to_string(),
                url: if subject_url.is_empty() {
                    format!("https://github.com/{repo}")
                } else {
                    html_url(subject_url)
                },
                title: n["subject"]["title"].as_str().unwrap_or("").to_string(),
                label: match &r {
                    Some(r) => format!("{short}#{}", r.number),
                    None => short.clone(),
                },
                fields: BTreeMap::new(),
            };
            let ty = n["subject"]["type"].as_str().unwrap_or("").to_string();
            it.set("source", Value::Str("github".into()));
            it.set("type", Value::Str(ty));
            it.set("reason", Value::Str(n["reason"].as_str().unwrap_or("").into()));
            it.set("unread", Value::Bool(n["unread"].as_bool().unwrap_or(false)));
            it.set("repo", Value::Str(repo.clone()));
            it.set("title", Value::Str(it.title.clone()));
            if let Some(t) = jts(&n["updated_at"]) {
                it.set("updated", Value::Time(t));
            }
            if let Some(r) = &r {
                it.set("number", Value::Int(r.number as i64));
            }
            let s = r.as_ref().and_then(|r| subjects.get(&r.key()));
            if let Some(s) = s.filter(|s| !s.is_null()) {
                if let Some(u) = s["url"].as_str() {
                    it.url = u.to_string();
                }
                let state = s["prState"].as_str().or(s["issueState"].as_str());
                if let Some(state) = state {
                    it.set("state", Value::Str(state.to_lowercase()));
                }
                if let Some(a) = login(&s["author"]) {
                    it.set("author", Value::Str(a));
                }
                if let Some(t) = jts(&s["createdAt"]) {
                    it.set("created", Value::Time(t));
                }
                if let Some(t) = jts(&s["updatedAt"]) {
                    it.set("last_activity", Value::Time(t));
                }
                it.set("labels", Value::strs(strs_at(s, "labels", "name")));
                if s["__typename"] == "PullRequest" {
                    it.set("draft", Value::Bool(s["isDraft"].as_bool().unwrap_or(false)));
                    for (f, k) in [
                        ("changed_files", "changedFiles"),
                        ("additions", "additions"),
                        ("deletions", "deletions"),
                    ] {
                        it.set(f, Value::Int(s[k].as_i64().unwrap_or(0)));
                    }
                    let paths = strs_at(s, "files", "path");
                    let co = owners.get(&repo);
                    let sets = paths
                        .iter()
                        .filter_map(|p| co?.lookup(p).map(|r| r.owners.clone()))
                        .collect();
                    it.set("codeowners", Value::Sets(sets));
                    it.set("paths", Value::strs(paths));
                    let (mut users, mut rteams) = (Vec::new(), Vec::new());
                    for rr in s["reviewRequests"]["nodes"].as_array().into_iter().flatten() {
                        let rr = &rr["requestedReviewer"];
                        if let Some(l) = login(rr) {
                            users.push(l);
                        } else if let Some(t) = rr["combinedSlug"].as_str() {
                            rteams.push(normalize_owner(t));
                        }
                    }
                    it.set("requested_for_me", Value::Bool(users.contains(&me)));
                    it.set("requested_users", Value::strs(users));
                    it.set("requested_teams", Value::strs(rteams));
                    let (decision, approved, changes) = review_decision(s);
                    it.set("review_decision", Value::Str(decision.into()));
                    it.set("approved_by", Value::strs(approved));
                    it.set("changes_requested_by", Value::strs(changes));
                    let (reviewed, new) = review_activity(s, &me, &teams);
                    it.set("reviewed_by_me", Value::Bool(reviewed));
                    it.set("new_since_my_review", Value::Bool(new));
                }
            }
            items.push(it);
        }
        Ok(items)
    }
}

const SUBJECT_FRAGMENT: &str = r#"
__typename
... on Issue {
  issueState: state url createdAt updatedAt
  author { login }
  labels(first: 30) { nodes { name } }
}
... on PullRequest {
  prState: state url createdAt updatedAt isDraft additions deletions changedFiles
  author { login }
  labels(first: 30) { nodes { name } }
  files(first: 100) { nodes { path } }
  reviewRequests(first: 30) { nodes { requestedReviewer { __typename ... on User { login } ... on Team { combinedSlug } } } }
  reviewDecision
  latestOpinionatedReviews(first: 30) { nodes { state author { login } } }
  myReviews: reviews(last: 5, author: $ME) { nodes { submittedAt } }
  timelineItems(last: 100, itemTypes: [PULL_REQUEST_COMMIT, HEAD_REF_FORCE_PUSHED_EVENT, ISSUE_COMMENT, PULL_REQUEST_REVIEW, REVIEW_REQUESTED_EVENT]) {
    nodes {
      __typename
      ... on PullRequestCommit { commit { committedDate author { user { login } } } }
      ... on HeadRefForcePushedEvent { createdAt actor { __typename login } }
      ... on IssueComment { createdAt author { __typename login } }
      ... on PullRequestReview { submittedAt author { __typename login } }
      ... on ReviewRequestedEvent { createdAt requestedReviewer { __typename ... on User { login } ... on Team { combinedSlug } } }
    }
  }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(timeline: J) -> J {
        json!({
            "myReviews": { "nodes": [{ "submittedAt": "2026-11-01T00:00:00Z" }] },
            "timelineItems": { "nodes": timeline },
        })
    }

    fn act(timeline: J) -> (bool, bool) {
        review_activity(&pr(timeline), "me", &["org/team".to_string()])
    }

    #[test]
    fn not_reviewed() {
        let j = json!({ "myReviews": { "nodes": [] }, "timelineItems": { "nodes": [] } });
        assert_eq!(review_activity(&j, "me", &[]), (false, false));
    }

    #[test]
    fn nothing_new() {
        assert_eq!(
            act(json!([
                { "__typename": "IssueComment", "createdAt": "2026-10-30T00:00:00Z", "author": { "login": "bob" } },
                { "__typename": "IssueComment", "createdAt": "2026-11-02T00:00:00Z", "author": { "login": "me" } },
                { "__typename": "IssueComment", "createdAt": "2026-11-02T00:00:00Z", "author": { "__typename": "Bot", "login": "ci" } },
                { "__typename": "ReviewRequestedEvent", "createdAt": "2026-11-02T00:00:00Z", "requestedReviewer": { "login": "carol" } },
            ])),
            (true, false)
        );
    }

    #[test]
    fn new_activity() {
        for ev in [
            json!({ "__typename": "IssueComment", "createdAt": "2026-11-02T00:00:00Z", "author": { "login": "Bob" } }),
            json!({ "__typename": "PullRequestReview", "submittedAt": "2026-11-02T00:00:00Z", "author": { "login": "bob" } }),
            json!({ "__typename": "PullRequestCommit", "commit": { "committedDate": "2026-11-02T00:00:00Z", "author": { "user": { "login": "bob" } } } }),
            json!({ "__typename": "HeadRefForcePushedEvent", "createdAt": "2026-11-02T00:00:00Z", "actor": { "login": "bob" } }),
            json!({ "__typename": "ReviewRequestedEvent", "createdAt": "2026-11-02T00:00:00Z", "requestedReviewer": { "login": "me" } }),
            json!({ "__typename": "ReviewRequestedEvent", "createdAt": "2026-11-02T00:00:00Z", "requestedReviewer": { "combinedSlug": "org/team" } }),
        ] {
            assert_eq!(act(json!([ev])), (true, true), "{ev}");
        }
    }

    #[test]
    fn decision() {
        let pr = json!({ "reviewDecision": null, "latestOpinionatedReviews": { "nodes": [
            { "state": "APPROVED", "author": { "login": "A" } },
            { "state": "CHANGES_REQUESTED", "author": { "login": "b" } },
        ] } });
        assert_eq!(review_decision(&pr), ("changes_requested", vec!["a".into()], vec!["b".into()]));
        let pr = json!({ "reviewDecision": "REVIEW_REQUIRED", "latestOpinionatedReviews": { "nodes": [
            { "state": "APPROVED", "author": { "login": "a" } },
        ] } });
        assert_eq!(review_decision(&pr).0, "review_required");
        assert_eq!(review_decision(&json!({})).0, "none");
    }

    #[test]
    fn subject_ref() {
        let r = SubjectRef::from_api_url("https://api.github.com/repos/o/r/pulls/12").unwrap();
        assert_eq!(r.key(), "o/r#12");
        assert!(SubjectRef::from_api_url("https://api.github.com/repos/o/r/releases/1").is_none());
    }
}
