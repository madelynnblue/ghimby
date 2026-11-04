//! Shared state and the background sync loop.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Notify;

use crate::checkout::{self, Checkouts};
use crate::item::{Item, Value};
use crate::source::github::GitHub;
use crate::rules::{self, Compiled, FieldDoc, Schema};
use crate::source::{Source, Status};
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
        *self.items.write().unwrap() = Arc::new(all);
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
        let mut err = None;
        for src in &self.sources {
            if let Err(e) = src.sync(&self.status).await {
                err = Some(format!("{}: {e:#}", src.name()));
            }
        }
        if let Err(e) = self.rebuild_items() {
            err.get_or_insert(format!("building items: {e:#}"));
        }
        if let Err(e) = self.sweep_checkouts().await {
            err.get_or_insert(format!("cleaning up checkouts: {e:#}"));
        }
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
