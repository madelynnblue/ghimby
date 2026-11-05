//! Shared state and the background sync loop.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Result, bail};
use futures::StreamExt;
use tokio::sync::Notify;

use crate::changes::Changes;
use crate::checkout::{self, Checkouts};
use crate::item::{Item, Value};
use crate::source::github::GitHub;
use crate::rules::{self, Compiled, FieldDoc, Schema};
use crate::source::{Source, Status};
use crate::prpage::{self, PrPage};
use crate::source::set_message;
use crate::store::{Store, now};

/// Fields the app sets on every item, whatever its source.
const APP_FIELDS: &[FieldDoc] = &[FieldDoc {
    name: "ignored",
    help: "You clicked Ignore on this item in the inbox",
}];

pub struct App {
    pub store: Store,
    pub sources: Vec<Arc<dyn Source>>,
    pub items: RwLock<Arc<Vec<Item>>>,
    pub status: Status,
    pub kick: Notify,
    pub github: Arc<GitHub>,
    pub checkouts: Checkouts,
    pub changes: Changes,
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
        let ignored = self.store.ignored()?;
        for it in &mut all {
            it.set("ignored", Value::Bool(ignored.contains(&it.url)));
        }
        let mut items = self.items.write().unwrap();
        if **items != all {
            *items = Arc::new(all);
            self.changes.bump();
        }
        Ok(())
    }

    /// Records an Ignore click and updates items in place, avoiding a full rebuild.
    pub fn set_ignored(&self, url: &str, label: &str, ignored: bool) -> Result<()> {
        self.store.set_ignored(url, label, ignored)?;
        let mut items = self.items.write().unwrap();
        let mut next = (**items).clone();
        for it in next.iter_mut().filter(|it| it.url == url) {
            it.set("ignored", Value::Bool(ignored));
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
    }

    /// Re-fetches cached PR pages that changed, prefetches shown PRs, and drops pages of PRs
    /// no longer in the inbox.
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
            let fetch = match times.get(&key) {
                Some(t) => prpage::stale(*t, it),
                None => compiled.as_ref().is_some_and(|c| c.decide(it, now).show),
            };
            if fetch {
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

    async fn sweep_checkouts(&self) -> Result<()> {
        let prs: Vec<(String, i64)> = self
            .store
            .checkouts()?
            .into_iter()
            .filter(|c| !c.clone.is_empty())
            .map(|c| (c.repo, c.number))
            .collect();
        let states = self.github.pr_states(&prs).await?;
        checkout::sweep(&self.store, &states).await
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

    pub async fn sync_loop(self: Arc<Self>) {
        loop {
            self.sync_once().await;
            let secs = self.store.setting_i64("poll_secs").unwrap_or(300).max(60);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(secs as u64)) => {}
                _ = self.kick.notified() => {}
            }
        }
    }
}
