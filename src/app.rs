//! Shared state and the background sync loop.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Notify;

use crate::item::Item;
use crate::rules::{self, Compiled, Schema};
use crate::source::{Source, Status};
use crate::store::{Store, now};

pub struct App {
    pub store: Store,
    pub sources: Vec<Arc<dyn Source>>,
    pub items: RwLock<Arc<Vec<Item>>>,
    pub status: Status,
    pub kick: Notify,
}

impl App {
    pub fn schema(&self) -> Schema {
        let mut s = Schema::default();
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
