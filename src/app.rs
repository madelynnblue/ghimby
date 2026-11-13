//! Shared state and the background sync loop.

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering::Relaxed};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Result, bail};
use futures::StreamExt;
use tokio::sync::{Notify, watch};

use crate::changes::Changes;
use crate::checkout::{self, Checkouts};
use crate::item::{Item, Value};
use crate::source::github::GitHub;
use crate::rules::{self, Compiled, FieldDoc, Schema};
use crate::source::{Source, Status};
use crate::prpage::{self, PrPage};
use crate::source::set_message;
use crate::store::{Mark, Store, now};

/// Fields the app sets on every item, whatever its source.
const APP_FIELDS: &[FieldDoc] = &[
    FieldDoc {
        name: "ignored",
        help: "You clicked Ignore, and no human has named you since (mention, review request, assignment)",
    },
    FieldDoc {
        name: "dismissed",
        help: "You clicked Dismiss, and no other human has commented, reviewed or pushed since",
    },
];

/// Sets `ignored` and `dismissed` from the item's mark, if it's still in force.
fn apply_mark(it: &mut Item, mark: Option<&(Mark, i64)>) {
    let after = |field: &str, at: i64| matches!(it.get(field), Value::Time(t) if *t > at);
    let (ignored, dismissed) = match mark {
        Some(&(Mark::Ignore, at)) => (!after("last_named_me", at), false),
        // Items without PR or issue details only have the notification time to go on.
        Some(&(Mark::Dismiss, at)) => match it.get("last_activity") {
            Value::Null => (false, !after("updated", at)),
            _ => (false, !after("last_others_activity", at)),
        },
        None => (false, false),
    };
    it.set("ignored", Value::Bool(ignored));
    it.set("dismissed", Value::Bool(dismissed));
}

pub struct App {
    pub store: Store,
    pub sources: Vec<Arc<dyn Source>>,
    pub items: RwLock<Arc<Vec<Item>>>,
    pub status: Status,
    pub kick: Notify,
    pub github: Arc<GitHub>,
    pub checkouts: Checkouts,
    pub changes: Changes,
    /// Inbox tabs open and visible; background syncing waits for one.
    pub viewers: watch::Sender<usize>,
    pub synced_at: AtomicI64,
    /// Open Claude desktop sessions, re-read every few seconds while an inbox tab is visible.
    pub claude: RwLock<Arc<Vec<crate::claude::Session>>>,
}

/// Without a visible inbox tab, sync only this often, so auto-update and checkout cleanup still run.
const IDLE_SECS: i64 = 86400;

/// Counts as a viewer until dropped.
pub struct Viewer(Arc<App>);

impl Drop for Viewer {
    fn drop(&mut self) {
        self.0.viewers.send_modify(|n| *n -= 1);
    }
}

impl App {
    pub fn schema(&self) -> Schema {
        let mut s = Schema::default();
        s.extend(APP_FIELDS);
        for src in &self.sources {
            s.extend(src.fields());
        }
        s
    }

    pub fn compile(&self, text: &str) -> Result<Compiled, Vec<(usize, String)>> {
        rules::compile(text, &self.schema())
    }

    pub fn current_rules(&self) -> Result<String> {
        Ok(self.store.rules()?.unwrap_or_default())
    }

    pub fn rebuild_items(&self) -> Result<()> {
        let mut all = Vec::new();
        for src in &self.sources {
            all.extend(src.items()?);
        }
        let marks = self.store.marks()?;
        for it in &mut all {
            apply_mark(it, marks.get(&it.url));
        }
        let mut items = self.items.write().unwrap();
        if **items != all {
            *items = Arc::new(all);
            self.changes.bump();
        }
        Ok(())
    }

    /// Records an Ignore or Dismiss click and updates items in place, avoiding a full rebuild.
    pub fn set_mark(&self, url: &str, label: &str, mark: Option<Mark>) -> Result<()> {
        self.store.set_mark(url, label, mark)?;
        let marks = self.store.marks()?;
        let mut items = self.items.write().unwrap();
        let mut next = (**items).clone();
        for it in next.iter_mut().filter(|it| it.url == url) {
            apply_mark(it, marks.get(url));
        }
        *items = Arc::new(next);
        self.changes.bump();
        Ok(())
    }

    pub fn items(&self) -> Arc<Vec<Item>> {
        self.items.read().unwrap().clone()
    }

    async fn sync_once(&self) {
        {
            let mut s = self.status.lock().unwrap();
            s.running = true;
            s.message = "starting".into();
        }
        let before = (format!("{:?}", self.store.checkouts().ok()), self.status.lock().unwrap().last_error.clone());
        let mut err = None;
        for src in &self.sources {
            if let Err(e) = src.sync(&self.status).await {
                err = Some(format!("{}: {e:#}", src.name()));
            }
        }
        if let Err(e) = self.rebuild_items() {
            err.get_or_insert(format!("building items: {e:#}"));
        }
        if let Err(e) = self.refetch_shown().await {
            err.get_or_insert(format!("checking shown PRs: {e:#}"));
        }
        if let Err(e) = self.refresh_pr_pages().await {
            err.get_or_insert(format!("fetching PR pages: {e:#}"));
        }
        if let Err(e) = self.sweep_checkouts().await {
            err.get_or_insert(format!("cleaning up checkouts: {e:#}"));
        }
        {
            let mut s = self.status.lock().unwrap();
            s.running = false;
            s.message.clear();
            match err {
                Some(e) => {
                    tracing::warn!("sync failed: {e}");
                    s.last_error = Some(e);
                }
                None => {
                    s.last_error = None;
                    s.last_ok = Some(now());
                }
            }
        }
        // Item changes bump on rebuild; also reload pages for sync errors and checkout cleanup.
        let after = (format!("{:?}", self.store.checkouts().ok()), self.status.lock().unwrap().last_error.clone());
        if before != after {
            self.changes.bump();
        }
        if let Err(e) = crate::update::maybe_update(self).await {
            tracing::warn!("auto-update: {e:#}");
        }
    }

    /// Re-fetches shown PRs that changed on GitHub without notifying you (e.g. your own review),
    /// using a light updatedAt check, then rebuilds if any did.
    async fn refetch_shown(&self) -> Result<()> {
        // Rule errors are reported elsewhere; without rules nothing is shown to check.
        let Ok(compiled) = self.compile(&self.current_rules()?) else { return Ok(()) };
        let now = now();
        let prs: Vec<(String, i64, i64)> = self
            .items()
            .iter()
            .filter(|it| prpage::item_key(it).is_some() && compiled.decide(it, now).show)
            .filter_map(|it| match (it.get("repo"), it.get("number"), it.get("last_activity")) {
                (Value::Str(r), Value::Int(n), Value::Time(t)) => Some((r.clone(), *n, *t)),
                _ => None,
            })
            .collect();
        if self.github.refetch_changed(&prs).await? > 0 {
            self.rebuild_items()?;
        }
        Ok(())
    }

    /// Fetches uncached or stale pages of shown PRs, and drops pages of PRs no longer in the inbox.
    async fn refresh_pr_pages(&self) -> Result<()> {
        let items = self.items();
        let compiled = self.compile(&self.current_rules()?).ok();
        let times = self.store.pr_page_times()?;
        let now = now();
        let mut want = Vec::new();
        let mut keep = HashSet::new();
        for it in items.iter() {
            let Some(key) = prpage::item_key(it) else { continue };
            if !keep.insert(key.clone()) {
                continue;
            }
            // Hidden PRs are fetched on click by pr_page instead.
            let shown = compiled.as_ref().is_some_and(|c| c.decide(it, now).show);
            if shown && times.get(&key).is_none_or(|t| prpage::stale(*t, it)) {
                let (Value::Str(repo), Value::Int(n)) = (it.get("repo"), it.get("number")) else { continue };
                want.push((repo.clone(), *n));
            }
        }
        for key in times.keys().filter(|k| !keep.contains(*k)) {
            self.store.delete_pr_page(key)?;
        }
        let total = want.len();
        let mut done = 0;
        let mut errors = Vec::new();
        let mut stream = futures::stream::iter(want)
            .map(|(repo, n)| async move {
                prpage::fetch_and_store(&self.github, &self.store, &repo, n).await.map_err(|e| format!("{repo}#{n}: {e:#}"))
            })
            .buffer_unordered(4);
        while let Some(r) = stream.next().await {
            done += 1;
            if let Err(e) = r {
                errors.push(e);
            }
            set_message(&self.status, format!("fetched PR pages {done}/{total}"));
        }
        if let Some(e) = errors.first() {
            bail!("{} of {total} failed, first: {e}", errors.len());
        }
        Ok(())
    }

    /// The cached page for a PR, re-fetched first if `force` or the inbox knows of newer changes.
    pub async fn pr_page(&self, repo: &str, number: i64, force: bool) -> (Option<PrPage>, Option<String>) {
        let key = prpage::key(repo, number);
        let cached = prpage::load(&self.store, &key);
        let items = self.items();
        let item = items.iter().find(|it| prpage::item_key(it).as_deref() == Some(key.as_str()));
        let fresh = cached.as_ref().is_some_and(|p| item.is_none_or(|it| !prpage::stale((p.updated, p.fetched_at), it)));
        if fresh && !force {
            return (cached, None);
        }
        match prpage::fetch_and_store(&self.github, &self.store, repo, number).await {
            Ok(p) => (Some(p), None),
            Err(e) => (cached, Some(format!("{e:#}"))),
        }
    }

    /// Cleanup isn't urgent, so it checks GitHub at most hourly.
    async fn sweep_checkouts(&self) -> Result<()> {
        const KEY: &str = "checkouts_swept_at";
        if now() - self.store.setting(KEY)?.parse::<i64>().unwrap_or(0) < 3600 {
            return Ok(());
        }
        let prs: Vec<(String, i64)> = self
            .store
            .checkouts()?
            .into_iter()
            .filter(|c| !c.clone.is_empty())
            .map(|c| (c.repo, c.number))
            .collect();
        let states = self.github.pr_states(&prs).await?;
        checkout::sweep(&self.store, &states).await?;
        self.store.set_setting(KEY, &now().to_string())
    }

    /// Clears every source's synced data and starts a fresh sync.
    pub fn reset_sync(&self) -> Result<()> {
        for src in &self.sources {
            src.reset_sync()?;
        }
        self.rebuild_items()?;
        self.kick.notify_one();
        Ok(())
    }

    /// Registers a visible inbox tab, syncing now if the last sync is older than `stale_secs`.
    pub fn viewer(self: &Arc<Self>) -> Viewer {
        let stale = self.store.setting_i64("stale_secs").unwrap_or(120);
        if now() - self.synced_at.load(Relaxed) >= stale && !self.status.lock().unwrap().running {
            self.kick.notify_one();
        }
        self.viewers.send_modify(|n| *n += 1);
        Viewer(self.clone())
    }

    pub fn claude_sessions(&self) -> Arc<Vec<crate::claude::Session>> {
        self.claude.read().unwrap().clone()
    }

    /// Re-reads the Claude app's session records, bumping the change counter if they differ.
    pub fn claude_enabled(&self) -> bool {
        self.store.setting("claude_section").is_ok_and(|v| v.trim() != "false")
    }

    pub fn rescan_claude(&self) {
        let dir = self.store.setting("claude_sessions_dir").unwrap_or_default();
        let mut sessions = match dir.trim() {
            d if d.is_empty() || !self.claude_enabled() => vec![],
            d => crate::claude::scan(&checkout::expand_home(d)),
        };
        crate::claude::apply_dismissed(&mut sessions, &self.store.claude_dismissed().unwrap_or_default());
        // An empty scan may be a transient read failure; keep dismissals until sessions show up.
        if !sessions.is_empty() {
            let open: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
            if let Err(e) = self.store.prune_claude_dismissed(&open) {
                tracing::warn!("pruning Claude dismissals: {e:#}");
            }
        }
        let mut cur = self.claude.write().unwrap();
        if **cur != sessions {
            *cur = Arc::new(sessions);
            self.changes.bump();
        }
    }

    /// The files are local and small, so polling is cheap; it pauses with no inbox tab visible.
    pub async fn claude_loop(self: Arc<Self>) {
        let mut viewers = self.viewers.subscribe();
        loop {
            self.rescan_claude();
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = viewers.changed() => {}
            }
            while *viewers.borrow_and_update() == 0 {
                if viewers.changed().await.is_err() {
                    return;
                }
            }
        }
    }

    pub async fn sync_loop(self: Arc<Self>) {
        let mut viewers = self.viewers.subscribe();
        loop {
            self.sync_once().await;
            self.synced_at.store(now(), Relaxed);
            loop {
                let every = if *viewers.borrow_and_update() > 0 {
                    self.store.setting_i64("poll_secs").unwrap_or(300).max(60)
                } else {
                    IDLE_SECS
                };
                let wait = (self.synced_at.load(Relaxed) + every - now()).max(0) as u64;
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(wait)) => break,
                    _ = self.kick.notified() => break,
                    // A tab appeared or went away: recompute the deadline.
                    _ = viewers.changed() => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(fields: &[(&str, Value)]) -> Item {
        let mut it = Item { id: "1".into(), url: "u".into(), title: "t".into(), label: "l".into(), fields: Default::default() };
        for (k, v) in fields {
            it.set(k, v.clone());
        }
        it
    }

    fn marked(it: &mut Item, m: Option<(Mark, i64)>) -> (bool, bool) {
        apply_mark(it, m.as_ref());
        (it.get("ignored") == &Value::Bool(true), it.get("dismissed") == &Value::Bool(true))
    }

    #[test]
    fn marks() {
        let pr = |others: i64, named: i64| item(&[
            ("last_activity", Value::Time(500)),
            ("last_others_activity", Value::Time(others)),
            ("last_named_me", Value::Time(named)),
        ]);
        assert_eq!(marked(&mut pr(200, 50), None), (false, false));
        // Others acting doesn't lift an ignore; being named does.
        assert_eq!(marked(&mut pr(200, 50), Some((Mark::Ignore, 100))), (true, false));
        assert_eq!(marked(&mut pr(200, 150), Some((Mark::Ignore, 100))), (false, false));
        assert_eq!(marked(&mut pr(50, 50), Some((Mark::Dismiss, 100))), (false, true));
        assert_eq!(marked(&mut pr(200, 50), Some((Mark::Dismiss, 100))), (false, false));
        // No subject details: dismiss falls back to the notification time.
        let mut rel = item(&[("updated", Value::Time(200))]);
        assert_eq!(marked(&mut rel, Some((Mark::Dismiss, 100))), (false, false));
        assert_eq!(marked(&mut rel, Some((Mark::Dismiss, 300))), (false, true));
    }

    #[test]
    fn default_rules_compile() {
        let mut s = rules::Schema::default();
        s.extend(APP_FIELDS);
        s.extend(crate::source::github::FIELDS);
        if let Err(errs) = rules::compile(crate::store::DEFAULT_RULES, &s) {
            panic!("{errs:?}");
        }
    }
}
