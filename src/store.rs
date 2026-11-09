//! SQLite-backed config and caches.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

pub const DEFAULT_RULES: &str = r#"# Rules run top to bottom. show/hide stop at the first match; tag keeps going.
hide "finished" if state != "open"
hide "ignored" if ignored
hide "bots" if author in ["dependabot", "renovate"]
hide "already reviewed" if reviewed_by_me and not new_since_my_review
hide "draft" if draft
default show
"#;

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE rules (id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE notifications (id TEXT PRIMARY KEY, json TEXT NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE subjects (key TEXT PRIMARY KEY, json TEXT NOT NULL, fetched_at INTEGER NOT NULL);
CREATE TABLE codeowners (repo TEXT PRIMARY KEY, content TEXT, fetched_at INTEGER NOT NULL);
"#, r#"
CREATE TABLE local_repos (repo TEXT PRIMARY KEY, path TEXT NOT NULL, others TEXT NOT NULL);
CREATE TABLE checkouts (
  repo TEXT NOT NULL, number INTEGER NOT NULL, clone TEXT NOT NULL, path TEXT NOT NULL,
  branch TEXT NOT NULL, head_oid TEXT NOT NULL, status TEXT NOT NULL,
  closed_seen_at INTEGER, updated_at INTEGER NOT NULL,
  PRIMARY KEY (repo, number)
);
"#, r#"
CREATE TABLE ignored (url TEXT PRIMARY KEY, label TEXT NOT NULL, created_at INTEGER NOT NULL);
"#, r#"
CREATE TABLE pr_pages (key TEXT PRIMARY KEY, json TEXT NOT NULL, updated_at INTEGER NOT NULL, fetched_at INTEGER NOT NULL);
"#, r#"
-- The subject query gained updatedAt and review decisions; re-fetch cached subjects.
UPDATE subjects SET fetched_at = 0;
"#];

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

pub struct Setting {
    pub key: &'static str,
    pub default: &'static str,
    pub help: &'static str,
    pub multiline: bool,
}

#[derive(Debug, Clone)]
pub struct Checkout {
    pub repo: String,
    pub number: i64,
    pub clone: String,
    pub path: String,
    pub branch: String,
    /// The PR head ghimby last put in the worktree.
    pub head_oid: String,
    pub status: String,
    pub closed_seen_at: Option<i64>,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct LocalRepo {
    pub repo: String,
    pub path: String,
    /// Other clones of the same repo that lost to `path`.
    pub others: Vec<String>,
}

/// Settings editable in the app, with defaults used when unset.
pub const SETTINGS: &[Setting] = &[
    Setting {
        key: "github_token",
        default: "",
        help: "GitHub token. Leave empty to use the token command.",
        multiline: false,
    },
    Setting {
        key: "github_token_cmd",
        default: "gh auth token",
        help: "Shell command that prints a GitHub token.",
        multiline: false,
    },
    Setting {
        key: "sync_days",
        default: "7",
        help: "Keep notifications updated in the last this many days, read or unread.",
        multiline: false,
    },
    Setting {
        key: "poll_secs",
        default: "300",
        help: "Seconds between syncs while an inbox tab is visible. With none, ghimby syncs once a day.",
        multiline: false,
    },
    Setting {
        key: "stale_secs",
        default: "120",
        help: "When an inbox tab becomes visible, sync right away if the last sync is older than this many seconds.",
        multiline: false,
    },
    Setting {
        key: "subject_refresh_secs",
        default: "900",
        help: "Re-fetch details of open PRs/issues older than this many seconds.",
        multiline: false,
    },
    Setting {
        key: "codeowners_refresh_secs",
        default: "21600",
        help: "Re-fetch CODEOWNERS files older than this many seconds.",
        multiline: false,
    },
    Setting {
        key: "editor_cmd",
        default: "code {path}",
        help: "Command that opens a PR checkout; {path} is replaced with the folder. Empty uses the system opener (open on macOS, xdg-open on Linux).",
        multiline: false,
    },
    Setting {
        key: "repo_roots",
        default: "~/code\n~/code/github.com/*",
        help: "Folders whose direct subfolders are scanned for clones, one per line. A * component matches every subfolder.",
        multiline: true,
    },
    Setting {
        key: "repo_paths",
        default: "",
        help: "owner/repo = path lines that override scanning, e.g. for repos cloned twice.",
        multiline: true,
    },
    Setting {
        key: "clone_path",
        default: "~/code/github.com/{owner}/{repo}",
        help: "Where to git clone a repo that isn't found locally.",
        multiline: false,
    },
    Setting {
        key: "checkout_cleanup_days",
        default: "1",
        help: "Remove a PR checkout this many days after the PR merges or closes, if it has no local changes.",
        multiline: false,
    },
    Setting {
        key: "auto_update",
        default: "true",
        help: "Once a day, install the newest release and restart. Only installed release builds update; set to false to turn off.",
        multiline: false,
    },
];

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        restrict(path)?;
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

    /// URLs of items the user ignored.
    pub fn ignored(&self) -> Result<std::collections::HashSet<String>> {
        self.with(|c| {
            c.prepare("SELECT url FROM ignored")?
                .query_map([], |r| r.get(0))?
                .collect()
        })
    }

    pub fn set_ignored(&self, url: &str, label: &str, ignored: bool) -> Result<()> {
        self.with(|c| {
            if ignored {
                c.execute(
                    "INSERT OR REPLACE INTO ignored (url, label, created_at) VALUES (?, ?, ?)",
                    params![url, label, now()],
                )
            } else {
                c.execute("DELETE FROM ignored WHERE url = ?", [url])
            }
        })?;
        Ok(())
    }

    /// A cached PR page, keyed by "owner/repo#N".
    pub fn pr_page(&self, key: &str) -> Result<Option<String>> {
        self.with(|c| {
            c.query_row("SELECT json FROM pr_pages WHERE key = ?", [key], |r| r.get(0))
                .optional()
        })
    }

    pub fn put_pr_page(&self, key: &str, json: &str, updated_at: i64, fetched_at: i64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO pr_pages (key, json, updated_at, fetched_at) VALUES (?, ?, ?, ?)",
                params![key, json, updated_at, fetched_at],
            )
        })?;
        Ok(())
    }

    /// (PR updatedAt, fetched_at) of every cached PR page.
    pub fn pr_page_times(&self) -> Result<std::collections::HashMap<String, (i64, i64)>> {
        self.with(|c| {
            c.prepare("SELECT key, updated_at, fetched_at FROM pr_pages")?
                .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
                .collect()
        })
    }

    pub fn delete_pr_page(&self, key: &str) -> Result<()> {
        self.with(|c| c.execute("DELETE FROM pr_pages WHERE key = ?", [key]))?;
        Ok(())
    }

    pub fn upsert_notifications(&self, items: &[(String, String, i64)]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO notifications (id, json, updated_at) VALUES (?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
            )?;
            for (id, json, updated) in items {
                stmt.execute(params![id, json, updated])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Deletes notifications last updated before `before` (unix seconds).
    pub fn prune_notifications(&self, before: i64) -> Result<()> {
        self.with(|c| c.execute("DELETE FROM notifications WHERE updated_at < ?", [before]))?;
        Ok(())
    }

    pub fn clear_notifications(&self) -> Result<()> {
        self.with(|c| c.execute("DELETE FROM notifications", []))?;
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

    /// Drops cached subjects whose key isn't in `keep`.
    pub fn prune_subjects(&self, keep: &std::collections::HashSet<String>) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let keys: Vec<String> =
            tx.prepare("SELECT key FROM subjects")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for k in keys.iter().filter(|k| !keep.contains(*k)) {
            tx.execute("DELETE FROM subjects WHERE key = ?", [k])?;
        }
        tx.commit()?;
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

    pub fn put_local_repos(&self, repos: &[LocalRepo]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM local_repos", [])?;
        for r in repos {
            tx.execute(
                "INSERT INTO local_repos (repo, path, others) VALUES (?, ?, ?)",
                params![r.repo, r.path, serde_json::to_string(&r.others).unwrap()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn local_repos(&self) -> Result<Vec<LocalRepo>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT repo, path, others FROM local_repos ORDER BY repo")?;
            stmt.query_map([], |r| {
                let others: String = r.get(2)?;
                Ok(LocalRepo {
                    repo: r.get(0)?,
                    path: r.get(1)?,
                    others: serde_json::from_str(&others).unwrap_or_default(),
                })
            })?
            .collect()
        })
    }

    pub fn put_checkout(&self, c: &Checkout) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO checkouts (repo, number, clone, path, branch, head_oid, status, closed_seen_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(repo, number) DO UPDATE SET clone = excluded.clone, path = excluded.path,
                   branch = excluded.branch, head_oid = excluded.head_oid, status = excluded.status,
                   closed_seen_at = excluded.closed_seen_at, updated_at = excluded.updated_at",
                params![c.repo, c.number, c.clone, c.path, c.branch, c.head_oid, c.status, c.closed_seen_at, c.updated_at],
            )
        })?;
        Ok(())
    }

    pub fn checkouts(&self) -> Result<Vec<Checkout>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT repo, number, clone, path, branch, head_oid, status, closed_seen_at, updated_at
                 FROM checkouts ORDER BY repo, number",
            )?;
            stmt.query_map([], |r| {
                Ok(Checkout {
                    repo: r.get(0)?,
                    number: r.get(1)?,
                    clone: r.get(2)?,
                    path: r.get(3)?,
                    branch: r.get(4)?,
                    head_oid: r.get(5)?,
                    status: r.get(6)?,
                    closed_seen_at: r.get(7)?,
                    updated_at: r.get(8)?,
                })
            })?
            .collect()
        })
    }

    pub fn checkout(&self, repo: &str, number: i64) -> Result<Option<Checkout>> {
        Ok(self
            .checkouts()?
            .into_iter()
            .find(|c| c.repo == repo && c.number == number))
    }

    pub fn delete_checkout(&self, repo: &str, number: i64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM checkouts WHERE repo = ? AND number = ?",
                params![repo, number],
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

/// The database can hold a GitHub token, so only its owner may read it. SQLite gives the -wal and
/// -shm files the main file's mode when it creates them.
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        if std::path::Path::new(&p).exists() {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}
