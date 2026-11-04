//! SQLite-backed config and caches.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

pub const DEFAULT_RULES: &str = r#"# Rules run top to bottom. show/hide stop at the first match; tag keeps going.
hide "finished" if state != "open"
hide "bots" if author in ["dependabot", "renovate"]
hide "already reviewed" if reviewed_by_me and not new_since_my_review
tag "draft" if draft
default show
"#;

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE rules (id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE notifications (id TEXT PRIMARY KEY, json TEXT NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE subjects (key TEXT PRIMARY KEY, json TEXT NOT NULL, fetched_at INTEGER NOT NULL);
CREATE TABLE codeowners (repo TEXT PRIMARY KEY, content TEXT, fetched_at INTEGER NOT NULL);
"#];

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

pub struct Setting {
    pub key: &'static str,
    pub default: &'static str,
    pub help: &'static str,
}

/// Settings editable in the app, with defaults used when unset.
pub const SETTINGS: &[Setting] = &[
    Setting {
        key: "github_token",
        default: "",
        help: "GitHub token. Leave empty to use the token command.",
    },
    Setting {
        key: "github_token_cmd",
        default: "gh auth token",
        help: "Shell command that prints a GitHub token.",
    },
    Setting {
        key: "poll_secs",
        default: "300",
        help: "Seconds between notification polls.",
    },
    Setting {
        key: "subject_refresh_secs",
        default: "900",
        help: "Re-fetch details of open PRs/issues older than this many seconds.",
    },
    Setting {
        key: "codeowners_refresh_secs",
        default: "21600",
        help: "Re-fetch CODEOWNERS files older than this many seconds.",
    },
];

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        for (i, m) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            conn.execute_batch(m)?;
            conn.pragma_update(None, "user_version", (i + 1) as i64)?;
        }
        let store = Store {
            conn: Arc::new(Mutex::new(conn)),
        };
        if store.rules()?.is_none() {
            store.save_rules(DEFAULT_RULES)?;
        }
        Ok(store)
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap();
        Ok(f(&conn)?)
    }

    pub fn setting(&self, key: &str) -> Result<String> {
        let v: Option<String> = self.with(|c| {
            c.query_row("SELECT value FROM settings WHERE key = ?", [key], |r| {
                r.get(0)
            })
            .optional()
        })?;
        Ok(v.unwrap_or_else(|| {
            SETTINGS
                .iter()
                .find(|s| s.key == key)
                .map(|s| s.default.to_string())
                .unwrap_or_default()
        }))
    }

    pub fn setting_i64(&self, key: &str) -> Result<i64> {
        Ok(self.setting(key)?.trim().parse()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO settings (key, value) VALUES (?, ?)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
        })?;
        Ok(())
    }

    pub fn rules(&self) -> Result<Option<String>> {
        self.with(|c| {
            c.query_row("SELECT text FROM rules ORDER BY id DESC LIMIT 1", [], |r| {
                r.get(0)
            })
            .optional()
        })
    }

    pub fn save_rules(&self, text: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO rules (text, created_at) VALUES (?, ?)",
                params![text, now()],
            )
        })?;
        Ok(())
    }

    /// Replaces the full set of notifications.
    pub fn replace_notifications(&self, items: &[(String, String, i64)]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM notifications", [])?;
        {
            let mut stmt =
                tx.prepare("INSERT INTO notifications (id, json, updated_at) VALUES (?, ?, ?)")?;
            for (id, json, updated) in items {
                stmt.execute(params![id, json, updated])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn notifications(&self) -> Result<Vec<String>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT json FROM notifications ORDER BY updated_at DESC")?;
            stmt.query_map([], |r| r.get(0))?.collect()
        })
    }

    pub fn put_subject(&self, key: &str, json: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO subjects (key, json, fetched_at) VALUES (?, ?, ?)
                 ON CONFLICT(key) DO UPDATE SET json = excluded.json, fetched_at = excluded.fetched_at",
                params![key, json, now()],
            )
        })?;
        Ok(())
    }

    /// Returns (json, fetched_at) for every cached subject.
    pub fn subjects(&self) -> Result<Vec<(String, String, i64)>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT key, json, fetched_at FROM subjects")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect()
        })
    }

    pub fn put_codeowners(&self, repo: &str, content: Option<&str>) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO codeowners (repo, content, fetched_at) VALUES (?, ?, ?)
                 ON CONFLICT(repo) DO UPDATE SET content = excluded.content, fetched_at = excluded.fetched_at",
                params![repo, content, now()],
            )
        })?;
        Ok(())
    }

    /// Returns (repo, content, fetched_at).
    pub fn codeowners(&self) -> Result<Vec<(String, Option<String>, i64)>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT repo, content, fetched_at FROM codeowners")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect()
        })
    }
}
