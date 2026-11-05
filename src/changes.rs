//! A counter bumped whenever what the inbox shows changes, so open pages know to reload.

use tokio::sync::watch;

#[derive(Clone)]
pub struct Changes(watch::Sender<u64>);

impl Default for Changes {
    fn default() -> Self {
        // Start from the clock so a restarted server never repeats a version a page already has.
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        Changes(watch::channel(start).0)
    }
}

impl Changes {
    pub fn bump(&self) {
        self.0.send_modify(|v| *v += 1);
    }

    pub fn version(&self) -> u64 {
        *self.0.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.subscribe()
    }
}
