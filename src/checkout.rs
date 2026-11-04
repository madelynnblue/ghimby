//! Local PR checkouts: one git worktree per PR under `<clone>/.ghimby/worktrees/pr-<N>`, set up for
//! VS Code's GitHub Pull Requests extension.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::store::{Checkout, LocalRepo, Store, now};

const WORKTREE_DIR: &str = ".ghimby/worktrees";

#[derive(Default)]
pub struct Checkouts {
    /// In-progress messages, keyed by "owner/repo#N".
    pub running: Mutex<HashMap<String, String>>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

pub fn key(repo: &str, number: i64) -> String {
    format!("{repo}#{number}")
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

pub fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if p == "~" => home(),
        None => PathBuf::from(p),
    }
}

/// Expands `*` path components to every subdirectory.
fn expand_glob(p: &str) -> Vec<PathBuf> {
    let mut out = vec![PathBuf::new()];
    for comp in expand_home(p).components() {
        let c = comp.as_os_str();
        if c == "*" {
            out = out
                .iter()
                .flat_map(|base| {
                    std::fs::read_dir(base)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .filter(|e| e.path().is_dir())
                        .map(|e| e.path())
                        .collect::<std::collections::BTreeSet<_>>()
                })
                .collect();
        } else {
            for o in &mut out {
                o.push(c);
            }
        }
    }
    out
}

/// Turns a git remote URL into a lowercase "owner/repo".
pub fn remote_to_repo(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/').trim_end_matches(".git");
    if !url.contains("github.com") {
        return None;
    }
    let mut parts = url.rsplit(['/', ':']);
    let repo = parts.next()?;
    let owner = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}").to_lowercase())
}

/// Reads the origin URL straight from `.git/config` to avoid spawning git per folder.
fn origin_url(clone: &Path) -> Option<String> {
    let cfg = std::fs::read_to_string(clone.join(".git/config")).ok()?;
    let mut in_origin = false;
    for line in cfg.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_origin = l == "[remote \"origin\"]";
        } else if in_origin && let Some(v) = l.strip_prefix("url") {
            return Some(v.trim_start().trim_start_matches('=').trim().to_string());
        }
    }
    None
}

/// Scans the configured roots for main clones (a `.git` directory, so worktrees and jj
/// workspaces are skipped) and stores the result.
pub fn scan(store: &Store) -> Result<Vec<LocalRepo>> {
    let mut found: Vec<(String, String)> = Vec::new();
    for root in store.setting("repo_roots")?.lines().map(str::trim).filter(|l| !l.is_empty()) {
        for dir in expand_glob(root) {
            // Sorted so duplicates resolve the same way every scan.
            let mut children: Vec<PathBuf> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .collect();
            children.sort();
            let mut candidates = vec![dir.clone()];
            candidates.extend(children);
            for c in candidates {
                if !c.join(".git").is_dir() {
                    continue;
                }
                if let Some(repo) = origin_url(&c).as_deref().and_then(remote_to_repo) {
                    let path = c.to_string_lossy().to_string();
                    if !found.iter().any(|(_, p)| *p == path) {
                        found.push((repo, path));
                    }
                }
            }
        }
    }
    let overrides = overrides(store)?;
    let mut repos: Vec<LocalRepo> = Vec::new();
    for (repo, path) in found {
        match repos.iter_mut().find(|r| r.repo == repo) {
            Some(r) => r.others.push(path),
            None => repos.push(LocalRepo { repo, path, others: vec![] }),
        }
    }
    for (repo, path) in overrides {
        match repos.iter_mut().find(|r| r.repo == repo) {
            Some(r) if r.path != path => {
                let old = std::mem::replace(&mut r.path, path.clone());
                r.others.retain(|o| *o != path);
                r.others.push(old);
            }
            Some(_) => {}
            None => repos.push(LocalRepo { repo, path, others: vec![] }),
        }
    }
    repos.sort_by(|a, b| a.repo.cmp(&b.repo));
    store.put_local_repos(&repos)?;
    Ok(repos)
}

fn overrides(store: &Store) -> Result<Vec<(String, String)>> {
    Ok(store
        .setting("repo_paths")?
        .lines()
        .filter_map(|l| {
            let (r, p) = l.split_once('=')?;
            Some((r.trim().to_lowercase(), expand_home(p.trim()).to_string_lossy().to_string()))
        })
        .filter(|(r, p)| !r.is_empty() && !p.is_empty())
        .collect())
}

async fn run(dir: &Path, prog: &str, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new(prog)
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .with_context(|| format!("running {prog}"))?;
    if !out.status.success() {
        bail!(
            "{prog} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    run(dir, "git", args).await
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl Checkouts {
    fn lock(&self, k: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().unwrap().entry(k.to_string()).or_default().clone()
    }

    fn progress(&self, k: &str, msg: &str) {
        self.running.lock().unwrap().insert(k.to_string(), msg.to_string());
    }

    /// Creates or updates the PR's worktree and opens it in the editor. The outcome is stored as
    /// the checkout's status.
    pub async fn open(&self, store: &Store, repo: &str, number: i64) {
        let k = key(repo, number);
        let lock = self.lock(&k);
        let _guard = lock.lock().await;
        self.progress(&k, "starting");
        let result = self.open_locked(store, repo, number, &k).await;
        self.running.lock().unwrap().remove(&k);
        if let Err(e) = result {
            let mut c = store.checkout(repo, number).ok().flatten().unwrap_or(Checkout {
                repo: repo.to_string(),
                number,
                clone: String::new(),
                path: String::new(),
                branch: String::new(),
                head_oid: String::new(),
                status: String::new(),
                closed_seen_at: None,
                updated_at: now(),
            });
            c.status = format!("error: {e:#}");
            c.updated_at = now();
            if let Err(e) = store.put_checkout(&c) {
                tracing::warn!("saving checkout status: {e:#}");
            }
        }
    }

    async fn find_or_clone(&self, store: &Store, repo: &str, k: &str) -> Result<PathBuf> {
        let lower = repo.to_lowercase();
        let lookup = |repos: Vec<LocalRepo>| {
            repos
                .into_iter()
                .find(|r| r.repo == lower && Path::new(&r.path).join(".git").is_dir())
                .map(|r| PathBuf::from(r.path))
        };
        if let Some(p) = lookup(store.local_repos()?) {
            return Ok(p);
        }
        self.progress(k, "scanning for a local clone");
        if let Some(p) = lookup(scan(store)?) {
            return Ok(p);
        }
        let (owner, name) = repo.split_once('/').ok_or_else(|| anyhow!("bad repo {repo}"))?;
        let dest = expand_home(
            &store
                .setting("clone_path")?
                .replace("{owner}", owner)
                .replace("{repo}", name),
        );
        if dest.exists() {
            bail!("{} exists but isn't a clone of {repo}", dest.display());
        }
        let parent = dest.parent().ok_or_else(|| anyhow!("bad clone_path"))?;
        std::fs::create_dir_all(parent)?;
        self.progress(k, &format!("cloning into {}", dest.display()));
        let dest_s = dest.to_string_lossy().to_string();
        run(parent, "gh", &["repo", "clone", repo, &dest_s]).await?;
        scan(store)?;
        Ok(dest)
    }

    async fn open_locked(&self, store: &Store, repo: &str, number: i64, k: &str) -> Result<()> {
        let clone = self.find_or_clone(store, repo, k).await?;
        let branch = format!("worktree-pr-{number}");
        let path = clone.join(WORKTREE_DIR).join(format!("pr-{number}"));
        let path_s = path.to_string_lossy().to_string();
        exclude_worktrees(&clone)?;

        self.progress(k, "fetching PR head");
        git(&clone, &["fetch", "--quiet", "origin", &format!("pull/{number}/head")]).await?;
        let new = git(&clone, &["rev-parse", "FETCH_HEAD"]).await?;

        let prev = store.checkout(repo, number)?;
        let status = if is_worktree(&clone, &path).await? {
            let cur = git(&path, &["rev-parse", "HEAD"]).await?;
            if cur == new {
                "up to date".to_string()
            } else if let Some(why) = local_work(&path, prev.as_ref().map(|c| c.head_oid.as_str())).await? {
                format!("not updated: {why}")
            } else {
                self.progress(k, "updating worktree");
                git(&path, &["reset", "--quiet", "--hard", &new]).await?;
                "updated".to_string()
            }
        } else {
            if path.exists() {
                bail!("{path_s} exists but isn't a worktree of {}", clone.display());
            }
            self.progress(k, "creating worktree");
            git(&clone, &["worktree", "prune"]).await?;
            git(&clone, &["worktree", "add", "--quiet", "-B", &branch, &path_s, &new]).await?;
            "created".to_string()
        };
        let head_oid = if status.starts_with("not updated") {
            prev.as_ref().map(|c| c.head_oid.clone()).unwrap_or_default()
        } else {
            new.clone()
        };

        // The PR extension links a branch to its PR through these keys.
        let owner_repo = repo.replacen('/', "#", 1);
        git(&clone, &["config", &format!("branch.{branch}.github-pr-owner-number"), &format!("{owner_repo}#{number}")]).await?;
        git(&clone, &["config", &format!("branch.{branch}.remote"), "origin"]).await?;

        store.put_checkout(&Checkout {
            repo: repo.to_string(),
            number,
            clone: clone.to_string_lossy().to_string(),
            path: path_s.clone(),
            branch,
            head_oid,
            status: status.clone(),
            closed_seen_at: prev.and_then(|c| c.closed_seen_at),
            updated_at: now(),
        })?;

        self.progress(k, "opening editor");
        open_editor(store, &path).await?;
        Ok(())
    }
}

fn exclude_worktrees(clone: &Path) -> Result<()> {
    let info = clone.join(".git/info");
    std::fs::create_dir_all(&info)?;
    let f = info.join("exclude");
    let cur = std::fs::read_to_string(&f).unwrap_or_default();
    let line = format!("/{WORKTREE_DIR}/");
    if !cur.lines().any(|l| l.trim() == line) {
        let sep = if cur.is_empty() || cur.ends_with('\n') { "" } else { "\n" };
        std::fs::write(&f, format!("{cur}{sep}{line}\n"))?;
    }
    Ok(())
}

async fn is_worktree(clone: &Path, path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let want = std::fs::canonicalize(path)?;
    let list = git(clone, &["worktree", "list", "--porcelain"]).await?;
    Ok(list
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .any(|p| std::fs::canonicalize(p).is_ok_and(|p| p == want)))
}

/// Why the worktree can't be moved without losing something, if anything.
async fn local_work(path: &Path, recorded: Option<&str>) -> Result<Option<String>> {
    if !git(path, &["status", "--porcelain"]).await?.is_empty() {
        return Ok(Some("uncommitted changes".into()));
    }
    let head = git(path, &["rev-parse", "HEAD"]).await?;
    match recorded {
        Some(r) if r == head => Ok(None),
        _ => Ok(Some("local commits".into())),
    }
}

async fn open_editor(store: &Store, path: &Path) -> Result<()> {
    let cmd = store.setting("editor_cmd")?;
    let quoted = shell_quote(&path.to_string_lossy());
    let cmd = if cmd.trim().is_empty() {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        format!("{opener} {quoted}")
    } else if cmd.contains("{path}") {
        cmd.replace("{path}", &quoted)
    } else {
        format!("{cmd} {quoted}")
    };
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("sh").arg("-c").arg(&cmd).output(),
    )
    .await
    .map_err(|_| anyhow!("editor command {cmd:?} took over 30s"))??;
    if !out.status.success() {
        bail!("editor command {cmd:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Removes a checkout's worktree, branch, and PR config. Unless `force`, refuses when it holds
/// local work.
pub async fn remove(store: &Store, c: &Checkout, force: bool) -> Result<()> {
    let clone = Path::new(&c.clone);
    let path = Path::new(&c.path);
    if clone.join(".git").is_dir() {
        if is_worktree(clone, path).await? {
            if !force && let Some(why) = local_work(path, Some(&c.head_oid)).await? {
                bail!("has {why}");
            }
            let mut args = vec!["worktree", "remove"];
            if force {
                args.push("--force");
            }
            args.push(&c.path);
            git(clone, &args).await?;
        } else {
            git(clone, &["worktree", "prune"]).await?;
        }
        let tip = git(clone, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", c.branch)]).await;
        if let Ok(tip) = tip
            && (force || tip == c.head_oid)
        {
            git(clone, &["branch", "-D", &c.branch]).await?;
        }
        // Fails harmlessly when the section is already gone.
        let _ = git(clone, &["config", "--remove-section", &format!("branch.{}", c.branch)]).await;
    }
    store.delete_checkout(&c.repo, c.number)?;
    Ok(())
}

/// Removes checkouts whose PR has been merged or closed for `checkout_cleanup_days`, when they hold
/// no local work. `states` maps "owner/repo#N" to OPEN, CLOSED, or MERGED.
pub async fn sweep(store: &Store, states: &HashMap<String, String>) -> Result<()> {
    let days = store.setting_i64("checkout_cleanup_days")?.max(0);
    for mut c in store.checkouts()? {
        if c.clone.is_empty() {
            // Only an error record; nothing on disk.
            if c.status.starts_with("error") && now() - c.updated_at > 7 * 86400 {
                store.delete_checkout(&c.repo, c.number)?;
            }
            continue;
        }
        let Some(state) = states.get(&key(&c.repo, c.number)) else {
            continue;
        };
        if state == "OPEN" {
            if c.closed_seen_at.take().is_some() {
                store.put_checkout(&c)?;
            }
            continue;
        }
        let seen = *c.closed_seen_at.get_or_insert_with(now);
        if now() - seen < days * 86400 {
            store.put_checkout(&c)?;
            continue;
        }
        match remove(store, &c, false).await {
            Ok(()) => tracing::info!("removed checkout {}#{}", c.repo, c.number),
            Err(e) => {
                c.status = format!("PR {}, kept: {e:#}", state.to_lowercase());
                store.put_checkout(&c)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes() {
        for (url, want) in [
            ("git@github.com:acme/monorepo.git", "acme/monorepo"),
            ("org-12345@github.com:acme/deploy.git", "acme/deploy"),
            ("https://github.com/Acme/Widgets", "acme/widgets"),
            ("ssh://git@github.com/o/r.git", "o/r"),
        ] {
            assert_eq!(remote_to_repo(url).as_deref(), Some(want), "{url}");
        }
        assert_eq!(remote_to_repo("git@gitlab.com:o/r.git"), None);
    }
}
