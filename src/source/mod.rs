//! Sources fetch external data into the store and turn it into Items.

pub mod github;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures::future::BoxFuture;

use crate::item::Item;
use crate::rules::FieldDoc;

#[derive(Default, Clone)]
pub struct SyncStatus {
    pub running: bool,
    pub message: String,
    pub last_ok: Option<i64>,
    pub last_error: Option<String>,
}

pub type Status = Arc<Mutex<SyncStatus>>;

pub fn set_message(status: &Status, msg: impl Into<String>) {
    status.lock().unwrap().message = msg.into();
}

pub trait Source: Send + Sync {
    fn name(&self) -> &'static str;
    fn fields(&self) -> &'static [FieldDoc];
    /// Fetches new data into the store.
    fn sync<'a>(&'a self, status: &'a Status) -> BoxFuture<'a, Result<()>>;
    /// Forgets synced data so the next sync starts from scratch. Caches may be kept.
    fn reset_sync(&self) -> Result<()>;
    /// Builds items from what's in the store.
    fn items(&self) -> Result<Vec<Item>>;
}
