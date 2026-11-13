//! Claude desktop app sessions, read (never written) from the app's per-session JSON records.

use std::path::Path;

use serde::Deserialize;

/// One open session, as the inbox shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The app's id, `local_<uuid>`.
    pub id: String,
    pub title: String,
    pub cwd: String,
    /// Unix ms of the end of the last turn, or 0 while a turn runs.
    pub finished_at: i64,
    /// The last turn ended after you last opened the session.
    pub unseen: bool,
    /// The app's post-turn status, e.g. "completed", "blocked", "review_ready".
    pub status: String,
    /// What the app says you need to do, or else what happened.
    pub detail: String,
    /// "owner/repo#N" of PRs the app linked to the session.
    pub prs: Vec<String>,
}

impl Session {
    /// Opens the session in the desktop app.
    pub fn link(&self) -> String {
        format!("claude://claude.ai/epitaxy/{}", self.id)
    }

    /// A short project name: the repo a worktree belongs to, or the folder name.
    pub fn project(&self) -> &str {
        if self.cwd.contains("/scratch-workspaces/") {
            return "scratch";
        }
        let root = self.cwd.split("/.claude/worktrees/").next().unwrap_or(&self.cwd);
        let root = root.split("/.ghimby/worktrees/").next().unwrap_or(root);
        root.rsplit('/').next().unwrap_or(root)
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Record {
    session_id: String,
    title: String,
    cwd: String,
    is_archived: bool,
    last_focused_at: i64,
    latest_user_frame_at: i64,
    /// Set when a turn starts (just before the user's message) and when it ends.
    last_turn_edge_at: i64,
    post_turn_summary: Option<Summary>,
    turn_wrap_up: Option<WrapUp>,
    prs: Vec<Pr>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Summary {
    status_category: String,
    status_detail: String,
    needs_action: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WrapUp {
    description: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Pr {
    repo: String,
    pr_number: i64,
}

/// Reads `<root>/<account>/<org>/local_*.json`. Unreadable records are skipped, since the format
/// belongs to the app and may change.
pub fn scan(root: &Path) -> Vec<Session> {
    let mut records = Vec::new();
    for dir in subdirs(root).iter().flat_map(|d| subdirs(d)) {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("local_") && name.ends_with(".json")) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
            if let Ok(r) = serde_json::from_str::<Record>(&text) {
                records.push(r);
            }
        }
    }
    sessions(records)
}

/// Marks sessions seen whose dismissal covers their last turn; a later turn shows them again.
pub fn apply_dismissed(sessions: &mut [Session], dismissed: &std::collections::HashMap<String, i64>) {
    for s in sessions {
        if dismissed.get(&s.id).is_some_and(|t| *t >= s.finished_at) {
            s.unseen = false;
        }
    }
}

fn subdirs(p: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(p) else { return vec![] };
    entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()
}

fn sessions(records: Vec<Record>) -> Vec<Session> {
    // The app records only each session's latest focus, so the most recent one is on screen.
    let current = records.iter().filter(|r| r.last_focused_at > 0).max_by_key(|r| r.last_focused_at).map(|r| r.session_id.clone());
    let mut out: Vec<Session> = records
        .into_iter()
        .filter(|r| !r.is_archived && r.session_id.starts_with("local_"))
        .map(|r| {
            // A turn's start edge comes just before the user's message, so a later edge is its end.
            let finished_at = if r.last_turn_edge_at > r.latest_user_frame_at { r.last_turn_edge_at } else { 0 };
            let unseen = finished_at > r.last_focused_at && current.as_ref() != Some(&r.session_id);
            let s = r.post_turn_summary.unwrap_or_default();
            let wrap = r.turn_wrap_up.unwrap_or_default().description;
            let detail = [s.needs_action, s.status_detail, wrap].into_iter().find(|d| !d.is_empty()).unwrap_or_default();
            Session {
                id: r.session_id,
                title: r.title,
                cwd: r.cwd,
                finished_at,
                unseen,
                status: s.status_category,
                detail,
                prs: r.prs.iter().filter(|p| !p.repo.is_empty()).map(|p| crate::checkout::key(&p.repo, p.pr_number)).collect(),
            }
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.finished_at));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, focused: i64, user: i64, edge: i64) -> Record {
        Record {
            session_id: id.into(),
            last_focused_at: focused,
            latest_user_frame_at: user,
            last_turn_edge_at: edge,
            ..Default::default()
        }
    }

    #[test]
    fn unseen() {
        let got = sessions(vec![
            // Finished after you left it.
            rec("local_a", 100, 110, 200),
            // Finished, and you opened it since.
            rec("local_b", 300, 110, 200),
            // Still running: the edge is the turn's start.
            rec("local_c", 100, 401, 400),
            // Finished after its last focus, but it's the session on screen.
            rec("local_d", 500, 510, 600),
        ]);
        let unseen: Vec<_> = got.iter().filter(|s| s.unseen).map(|s| s.id.as_str()).collect();
        assert_eq!(unseen, ["local_a"]);
        assert_eq!(got.iter().find(|s| s.id == "local_c").unwrap().finished_at, 0);
    }

    #[test]
    fn dismissed_until_next_turn() {
        let dismissed = [("local_a".to_string(), 200)].into();
        let mut got = sessions(vec![rec("local_a", 100, 110, 200), rec("local_z", 700, 0, 0)]);
        apply_dismissed(&mut got, &dismissed);
        assert!(!got.iter().find(|s| s.id == "local_a").unwrap().unseen);
        let mut got = sessions(vec![rec("local_a", 100, 250, 300), rec("local_z", 700, 0, 0)]);
        apply_dismissed(&mut got, &dismissed);
        assert!(got.iter().find(|s| s.id == "local_a").unwrap().unseen);
    }

    #[test]
    fn parse() {
        let r: Record = serde_json::from_str(
            r#"{"sessionId":"local_x","title":"t","cwd":"/c/pebble/.claude/worktrees/w","isArchived":false,
            "postTurnSummary":{"status_category":"blocked","status_detail":"d","needs_action":"do it"},
            "prs":[{"prNumber":31,"repo":"o/r","state":"MERGED"}],"unknownField":1}"#,
        )
        .unwrap();
        let s = &sessions(vec![r])[0];
        assert_eq!((s.detail.as_str(), s.status.as_str(), s.project()), ("do it", "blocked", "pebble"));
        assert_eq!(s.prs, ["o/r#31"]);
        assert_eq!(s.link(), "claude://claude.ai/epitaxy/local_x");
    }
}
