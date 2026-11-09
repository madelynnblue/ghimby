//! Server-rendered web UI.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde::Deserialize;

use crate::app::App;
use crate::checkout::{self, key};
use crate::store::Checkout;
use crate::item::{Item, Value};
use crate::rules::{self, Action, Compiled, Decision, FUNCTIONS};
use crate::store::{SETTINGS, now};

mod pr;

type S = State<Arc<App>>;

/// `port` is the one we listen on; requests naming any other host are refused.
pub fn router(app: Arc<App>, port: u16) -> Router {
    Router::new()
        .route("/", get(inbox))
        .route("/favicon.svg", get(favicon))
        .route("/favicon.ico", get(|| async { Redirect::permanent("/favicon.svg") }))
        .route("/sync", post(sync))
        .route("/events", get(events))
        .route("/item/{id}", get(item_page))
        .route("/rules", get(rules_page).post(rules_text))
        .route("/rules/form", post(rules_form))
        .route("/settings", get(settings_page).post(settings_save))
        .route("/settings/reset-sync", post(reset_sync))
        .route("/settings/rescan", post(rescan))
        .route("/item/{id}/open", post(open_item))
        .route("/item/{id}/ignore", post(ignore_item))
        .route("/checkouts/remove", post(remove_checkout))
        .route("/pr/{owner}/{repo}/{number}", get(pr::show))
        .route("/pr/{owner}/{repo}/{number}/refresh", post(pr::refresh))
        .route("/pr/{owner}/{repo}/{number}/approve", post(pr::approve))
        .route("/js/{name}", get(js))
        .layer(middleware::from_fn_with_state(port, guard))
        .with_state(app)
}

async fn favicon() -> impl IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/svg+xml"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("favicon.svg"),
    )
}

async fn js(Path(name): Path<String>) -> Response {
    let body = match name.as_str() {
        "confirm.js" => CONFIRM_JS,
        "live.js" => LIVE_JS,
        "sort.js" => SORT_JS,
        "drag.js" => DRAG_JS,
        _ => return axum::http::StatusCode::NOT_FOUND.into_response(),
    };
    ([(axum::http::header::CONTENT_TYPE, "text/javascript")], body).into_response()
}

/// Scripts load from /js so the CSP can forbid inline ones; GitHub's HTML gets no script at all.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src * data:; media-src *; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// Whether a request may proceed. The Host check defeats DNS rebinding; the Origin check stops other
/// sites' POSTs, which run commands on this machine.
fn allowed(post: bool, host: &str, origin: &str, port: u16) -> bool {
    let ok_host = [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")]
        .iter()
        .any(|h| h == host);
    ok_host && (!post || origin.is_empty() || origin == format!("http://{host}"))
}

async fn guard(State(port): State<u16>, req: Request, next: Next) -> Response {
    let ok = {
        let header = |n: &str| req.headers().get(n).and_then(|v| v.to_str().ok()).unwrap_or("");
        allowed(req.method() == axum::http::Method::POST, header("host"), header("origin"), port)
    };
    if !ok {
        return (axum::http::StatusCode::FORBIDDEN, "request from another site rejected").into_response();
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    use axum::http::HeaderValue;
    h.insert("content-security-policy", HeaderValue::from_static(CSP));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    // Not no-referrer: that makes browsers send "Origin: null" on form posts, failing the check above.
    h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    resp
}

/// Asks before submitting forms marked data-confirm (inline onsubmit is blocked by the CSP).
const CONFIRM_JS: &str = r#"
document.addEventListener("submit", e => {
  const m = e.target.dataset.confirm;
  if (m && !confirm(m)) e.preventDefault();
});
"#;

#[cfg(test)]
mod tests {
    use super::allowed;

    #[test]
    fn host_and_origin() {
        assert!(allowed(false, "127.0.0.1:7880", "", 7880));
        assert!(allowed(false, "localhost:7880", "", 7880));
        assert!(allowed(true, "[::1]:7880", "http://[::1]:7880", 7880));
        // DNS rebinding: the attacker's name resolves here, so Origin and Host agree but Host is foreign.
        assert!(!allowed(true, "evil.com:7880", "http://evil.com:7880", 7880));
        assert!(!allowed(false, "evil.com:7880", "", 7880));
        assert!(!allowed(false, "127.0.0.1:7878", "", 7880));
        assert!(!allowed(true, "127.0.0.1:7880", "http://evil.com", 7880));
        assert!(!allowed(true, "127.0.0.1:7880", "null", 7880));
    }
}

/// Checkout records and in-progress jobs, keyed by "owner/repo#N".
struct Local {
    checkouts: HashMap<String, Checkout>,
    running: HashMap<String, String>,
}

impl Local {
    fn load(app: &App) -> Local {
        Local {
            checkouts: app
                .store
                .checkouts()
                .unwrap_or_default()
                .into_iter()
                .map(|c| (key(&c.repo, c.number), c))
                .collect(),
            running: app.checkouts.running.lock().unwrap().clone(),
        }
    }
}

fn local_cell(it: &Item, local: &Local) -> Markup {
    let Some(k) = crate::prpage::item_key(it) else { return html! {} };
    let checkout = local.checkouts.get(&k);
    html! {
        form method="post" action=(format!("/item/{}/open", it.id)) style="display:inline" {
            button title="Check out in a worktree and open the editor" { "Open" }
        }
        @if let Some(msg) = local.running.get(&k) {
            div.muted { (msg) "…" }
        } @else if let Some(c) = checkout.filter(|c| !matches!(c.status.as_str(), "" | "created" | "updated" | "up to date")) {
            // Only problems are worth showing; a successful Open speaks for itself.
            div.muted.small title=(c.path) class=[c.status.starts_with("error").then_some("bad")] { (c.status) }
        }
    }
}

/// Sorts PRs with a local checkout first.
fn local_sort(it: &Item, local: &Local) -> &'static str {
    match crate::prpage::item_key(it) {
        Some(k) if local.checkouts.contains_key(&k) || local.running.contains_key(&k) => "0",
        _ => "1",
    }
}

/// A pill for a PR's current approval or requested changes, naming who in the tooltip.
fn review_badge(it: &Item) -> Markup {
    let who = |f: &str| match it.get(f) {
        Value::List(xs) => xs.iter().filter_map(|x| if let Value::Str(s) = x { Some(s.as_str()) } else { None }).collect::<Vec<_>>().join(", "),
        _ => String::new(),
    };
    match str_field(it, "review_decision") {
        "approved" => html! { span.tag.good title=(format!("Approved by {}", who("approved_by"))) { "✓ approved" } },
        "changes_requested" => {
            html! { span.tag.bad title=(format!("Changes requested by {}", who("changes_requested_by"))) { "✗ changes requested" } }
        }
        _ => html! {},
    }
}

/// Ignoring returns to the inbox; unignoring returns to `back`, a local path.
fn ignore_button(it: &Item, back: &str) -> Markup {
    let ignored = matches!(it.get("ignored"), Value::Bool(true));
    html! {
        form method="post" action=(format!("/item/{}/ignore", it.id)) style="display:inline" {
            @if ignored {
                input type="hidden" name="back" value=(back);
                input type="hidden" name="undo" value="1";
                button title="Stop setting the ignored field on this item" { "Unignore" }
            } @else {
                button title="Set the ignored field on this item so a rule can hide it" { "Ignore" }
            }
        }
    }
}

const CSS: &str = r#"
th[data-sort] { cursor: pointer; user-select: none; white-space: nowrap; }
th[data-sort]:hover { text-decoration: underline; }
th.asc::after { content: " ▲"; font-size: 0.8em; }
th.desc::after { content: " ▼"; font-size: 0.8em; }
:root { --fg:#1f2328; --muted:#656d76; --bg:#fff; --card:#f6f8fa; --border:#d0d7de; --accent:#0969da; --bad:#cf222e; --good:#1a7f37; }
@media (prefers-color-scheme: dark) { :root { --fg:#e6edf3; --muted:#8d96a0; --bg:#0d1117; --card:#161b22; --border:#30363d; --accent:#4493f8; --bad:#f85149; --good:#3fb950; } }
* { box-sizing: border-box; }
body { font: 14px/1.5 -apple-system, system-ui, sans-serif; color: var(--fg); background: var(--bg); margin: 0 auto; padding: 16px; max-width: 1200px; }
a { color: var(--accent); text-decoration: none; } a:hover { text-decoration: underline; }
nav { display: flex; gap: 16px; align-items: center; border-bottom: 1px solid var(--border); padding-bottom: 8px; margin-bottom: 16px; }
nav b { margin-right: 8px; }
nav .gh { margin-left: auto; color: var(--muted); }
table { border-collapse: collapse; width: 100%; }
td, th { text-align: left; padding: 4px 8px; border-bottom: 1px solid var(--border); vertical-align: top; }
th { color: var(--muted); font-weight: 500; }
.muted { color: var(--muted); } .bad { color: var(--bad); } .good { color: var(--good); }
.tag.good { border-color: var(--good); } .tag.bad { border-color: var(--bad); }
.tag { display: inline-block; font-size: 12px; padding: 0 6px; border: 1px solid var(--border); border-radius: 10px; margin-right: 4px; }
.card { background: var(--card); border: 1px solid var(--border); border-radius: 6px; padding: 8px 12px; margin: 8px 0; }
details > summary { cursor: pointer; padding: 4px 0; }
textarea, input[type=text], input[type=password], select { font: 13px ui-monospace, monospace; color: var(--fg); background: var(--bg); border: 1px solid var(--border); border-radius: 4px; padding: 4px 6px; }
textarea { width: 100%; min-height: 260px; }
button { font: inherit; padding: 4px 12px; border: 1px solid var(--border); border-radius: 6px; background: var(--card); color: var(--fg); cursor: pointer; }
code { font: 12px ui-monospace, monospace; }
.nowrap { white-space: nowrap; }
.small { font-size: 12px; }
.handle { cursor: grab; border: none; background: none; padding: 0 4px; color: var(--muted); font-size: 16px; }
tr.dragging { opacity: 0.4; }
:root { --add:#e6ffec; --del:#ffebe9; --merged:#8250df; }
@media (prefers-color-scheme: dark) { :root { --add:rgba(46,160,67,0.15); --del:rgba(248,81,73,0.15); --merged:#ab7df8; } }
.merged { color: var(--merged); }
.md img { max-width: 100%; }
.md pre, pre.hunk { background: var(--card); padding: 8px; overflow: auto; border-radius: 6px; font: 12px ui-monospace, monospace; }
.md blockquote { color: var(--muted); border-left: 3px solid var(--border); margin: 0; padding-left: 8px; }
.md table td, .md table th { border: 1px solid var(--border); }
details.file { border: 1px solid var(--border); border-radius: 6px; margin: 12px 0; }
details.file > summary { background: var(--card); padding: 6px 8px; position: sticky; top: 0; z-index: 1; }
table.diff { font: 12px/1.45 ui-monospace, monospace; }
table.diff td { border: none; padding: 0 6px; }
table.diff td.code { white-space: pre-wrap; word-break: break-all; width: 100%; }
table.diff td.ln { color: var(--muted); text-align: right; user-select: none; white-space: nowrap; }
table.diff tr.add td { background: var(--add); }
table.diff tr.del td { background: var(--del); }
table.diff tr.hunk td { color: var(--muted); background: var(--card); }
.thread { margin: 6px 0; padding: 4px 10px; border: 1px solid var(--border); border-radius: 6px; background: var(--bg); font: 14px/1.5 -apple-system, system-ui, sans-serif; white-space: normal; word-break: normal; }
.comment + .comment { border-top: 1px solid var(--border); margin-top: 6px; padding-top: 6px; }
"#;

fn page(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · ghimby" }
                link rel="icon" type="image/svg+xml" href="/favicon.svg";
                style { (PreEscaped(CSS)) }
                script src="/js/confirm.js" {}
            }
            body {
                nav {
                    b { "ghimby" }
                    a href="/" { "Inbox" }
                    a href="/rules" { "Rules" }
                    a href="/settings" { "Settings" }
                    a.gh href="https://github.com/madelynnblue/ghimby" target="_blank" rel="noopener" { "GitHub" }
                }
                (body)
            }
        }
    }
}

fn ago(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

fn fmt_value(v: &Value, now: i64) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Str(s) => rules::quote(s),
        Value::Time(t) => format!("{} ({} ago)", chrono::DateTime::from_timestamp(*t, 0).map(|d| d.to_rfc3339()).unwrap_or_default(), ago(now - t)),
        Value::Duration(d) => ago(*d),
        Value::List(xs) => format!("[{}]", xs.iter().map(|x| fmt_value(x, now)).collect::<Vec<_>>().join(", ")),
        Value::Sets(sets) => {
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for s in sets {
                *counts.entry(s.join(" ")).or_default() += 1;
            }
            counts
                .iter()
                .map(|(k, n)| format!("{n} file(s): {}", if k.is_empty() { "(no owners)" } else { k }))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

fn updated(it: &Item) -> i64 {
    match it.get("updated") {
        Value::Time(t) => *t,
        _ => 0,
    }
}

/// "+a −d" for PRs, sorting by total lines changed.
fn size_cell(it: &Item) -> Markup {
    let int = |f: &str| match it.get(f) {
        Value::Int(n) => Some(*n),
        _ => None,
    };
    match (int("additions"), int("deletions")) {
        (Some(a), Some(d)) => html! {
            td.nowrap.small data-v=(a + d) title=[int("changed_files").map(|n| format!("{n} files"))] {
                span.good { "+" (a) } " " span.bad { "−" (d) }
            }
        },
        _ => html! { td data-v="" {} },
    }
}

fn str_field<'a>(it: &'a Item, f: &str) -> &'a str {
    match it.get(f) {
        Value::Str(s) => s,
        _ => "",
    }
}

fn item_rows(items: &[(&Item, Decision)], limit: usize, now: i64, local: &Local) -> Markup {
    html! {
        table.items {
            tr {
                th data-sort="pr" { "PR" } th data-sort="title" { "Title" } th data-sort="author" { "Author" }
                th data-sort="size" title="Lines added and removed" { "Size" } th data-sort="rule" { "Rule" } th data-sort="updated" { "Updated" } th data-sort="local" { "Local" } th { "" }
            }
            @for (it, d) in items.iter().take(limit) {
                tr {
                    td.nowrap { a href=(it.url) target="_blank" { (it.label) } }
                    td data-v=(it.title.to_lowercase()) {
                        @match (crate::prpage::item_key(it), it.get("repo"), it.get("number")) {
                            (Some(_), Value::Str(r), Value::Int(n)) => a href=(pr::href(r, *n)) { (it.title) },
                            _ => (it.title),
                        }
                        " "
                        (review_badge(it))
                        @for t in &d.tags { span.tag { (t) } }
                        @if !d.errors.is_empty() { span.tag.bad title=(d.errors.join("\n")) { "rule error" } }
                    }
                    td.nowrap.muted { (str_field(it, "author")) }
                    (size_cell(it))
                    td.nowrap.muted title=(format!("GitHub reason: {}", str_field(it, "reason"))) { (d.rule) }
                    td.nowrap.muted data-v=(updated(it)) { a href=(format!("/item/{}", it.id)) title="Why?" { (ago(now - updated(it))) } }
                    td style="max-width:220px" data-v=(local_sort(it, local)) { (local_cell(it, local)) }
                    td { (ignore_button(it, "/")) }
                }
            }
        }
        @if items.len() > limit { p.muted { "…and " (items.len() - limit) " more" } }
    }
}

fn status_bar(app: &App) -> Markup {
    let s = app.status.lock().unwrap().clone();
    let now = now();
    html! {
        div.card #status {
            form method="post" action="/sync" style="display:inline" { button { "Sync now" } }
            " "
            @if s.running { span { "Syncing: " (s.message) } }
            @else if let Some(t) = s.last_ok { span.muted { "Last synced " (ago(now - t)) " ago" } }
            @else { span.muted { "Not synced yet" } }
            @if let Some(e) = &s.last_error { div.bad { "Last sync failed: " (e) } }
        }
    }
}

fn compile_current(app: &App) -> Result<Compiled, Markup> {
    let text = app.current_rules().unwrap_or_default();
    app.compile(&text).map_err(|errs| {
        html! { div.card.bad { "Saved rules don't compile; everything is shown. " a href="/rules" { "Fix them" } ul { @for (l, e) in errs { li { "line " (l) ": " (e) } } } } }
    })
}

async fn inbox(State(app): S) -> Markup {
    let now = now();
    // Read before the items so a change in between still triggers a reload.
    let version = app.changes.version();
    let local = Local::load(&app);
    let items = app.items();
    let compiled = compile_current(&app);
    let decided: Vec<(&Item, Decision)> = items
        .iter()
        .map(|it| {
            let d = match &compiled {
                Ok(c) => c.decide(it, now),
                Err(_) => Decision { show: true, rule: "default".into(), tags: vec![], errors: vec![] },
            };
            (it, d)
        })
        .collect();
    let mut shown = Vec::new();
    let mut hidden: BTreeMap<String, Vec<(&Item, Decision)>> = BTreeMap::new();
    for (it, d) in decided {
        if d.show {
            shown.push((it, d));
        } else {
            hidden.entry(d.rule.clone()).or_default().push((it, d));
        }
    }
    shown.sort_by_key(|(it, _)| -updated(it));
    let mut hidden: Vec<_> = hidden.into_iter().collect();
    hidden.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    let errored = shown.iter().chain(hidden.iter().flat_map(|(_, v)| v)).filter(|(_, d)| !d.errors.is_empty()).count();
    page(
        "Inbox",
        html! {
            (status_bar(&app))
            @if let Err(m) = &compiled { (m) }
            @if errored > 0 { div.card.bad { (errored) " item(s) hit rule errors; hover the \"rule error\" tag for details." } }
            h2 { "Shown " span.muted { "(" (shown.len()) ")" } }
            (item_rows(&shown, usize::MAX, now, &local))
            h2 { "Hidden " span.muted { "(" (hidden.iter().map(|(_, v)| v.len()).sum::<usize>()) ")" } }
            @for (rule, v) in &hidden {
                details data-k=(rule) {
                    summary { "by " b { (rule) } " " span.muted { "(" (v.len()) ")" } }
                    (item_rows(v, 300, now, &local))
                }
            }
            script src="/js/live.js" data-version=(version) {}
            script src="/js/sort.js" {}
        },
    )
}

/// Pushes "version" when the inbox changes and "status" when the sync status line changes.
async fn events(State(app): S) -> impl IntoResponse {
    let rx = app.changes.subscribe();
    let tick = tokio::time::interval(std::time::Duration::from_secs(1));
    let stream = futures::stream::unfold((app, rx, tick, None::<u64>, String::new()), |(app, mut rx, mut tick, sent, status)| async move {
        loop {
            let v = *rx.borrow_and_update();
            if sent != Some(v) {
                let ev = Event::default().event("version").data(v.to_string());
                return Some((Ok::<_, Infallible>(ev), (app, rx, tick, Some(v), status)));
            }
            let now_status = status_bar(&app).into_string().replace('\n', " ");
            if now_status != status {
                let ev = Event::default().event("status").data(now_status.clone());
                return Some((Ok(ev), (app, rx, tick, sent, now_status)));
            }
            tokio::select! {
                r = rx.changed() => r.ok()?,
                _ = tick.tick() => {}
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Reloads the inbox when it changes, but only while the window is in the background or right as it
/// regains focus, never while the pointer is on a link or button. Keeps scroll and open groups.
const LIVE_JS: &str = r#"
(() => {
  const KEY = "ghimby-inbox";
  try {
    const s = JSON.parse(sessionStorage.getItem(KEY) || "null");
    sessionStorage.removeItem(KEY);
    if (s) {
      document.querySelectorAll("details[data-k]").forEach(d => { if (s.open.includes(d.dataset.k)) d.open = true; });
      window.scrollTo(0, s.y);
    }
  } catch (e) {}
  const version = document.currentScript.dataset.version;
  let stale = false, overLink = false;
  const interactive = "a, button, summary, input, select, textarea, label";
  document.addEventListener("mouseover", e => { overLink = !!e.target.closest(interactive); maybe(false); });
  document.documentElement.addEventListener("mouseleave", () => { overLink = false; maybe(false); });
  function reload() {
    try {
      const open = [...document.querySelectorAll("details[data-k]")].filter(d => d.open).map(d => d.dataset.k);
      sessionStorage.setItem(KEY, JSON.stringify({ open, y: window.scrollY }));
    } catch (e) {}
    location.reload();
  }
  // focusing: the window just came forward, the one moment a focused page may reload.
  function maybe(focusing) {
    if (!stale) return;
    if (document.hidden) return reload();
    if (overLink) return;
    if (focusing || !document.hasFocus()) reload();
  }
  window.addEventListener("focus", () => maybe(true));
  document.addEventListener("visibilitychange", () => maybe(!document.hidden));
  const es = new EventSource("/events");
  es.addEventListener("version", e => { if (e.data !== version) { stale = true; maybe(false); } });
  es.addEventListener("status", e => {
    const el = document.getElementById("status");
    if (!el || el.matches(":hover")) return;
    const t = document.createElement("template");
    t.innerHTML = e.data;
    const next = t.content.firstElementChild;
    if (next && next.innerHTML !== el.innerHTML) el.innerHTML = next.innerHTML;
  });
})();
"#;

/// Click a column title to sort every item table by it; click again to reverse. Remembered per browser.
/// Cells sort by `data-v` when present, else their text; numbers compare numerically.
const SORT_JS: &str = r#"
(() => {
  const KEY = "ghimby-sort";
  let s = null;
  try { s = JSON.parse(localStorage.getItem(KEY)); } catch (e) {}
  s = s || { col: "updated", dir: -1 };
  const val = td => td.dataset.v ?? td.textContent.trim().toLowerCase();
  const cmp = (x, y) => {
    const nx = Number(x), ny = Number(y);
    if (x !== "" && y !== "" && !isNaN(nx) && !isNaN(ny)) return nx - ny;
    return x.localeCompare(y, undefined, { numeric: true });
  };
  function apply() {
    document.querySelectorAll("table.items").forEach(t => {
      const head = [...t.rows[0].cells];
      head.forEach(th => {
        th.classList.toggle("asc", th.dataset.sort === s.col && s.dir > 0);
        th.classList.toggle("desc", th.dataset.sort === s.col && s.dir < 0);
      });
      const i = head.findIndex(th => th.dataset.sort === s.col);
      if (i < 0) return;
      const rows = [...t.rows].slice(1);
      rows.sort((a, b) => cmp(val(a.cells[i]), val(b.cells[i])) * s.dir);
      rows.forEach(r => r.parentNode.appendChild(r));
    });
  }
  document.addEventListener("click", e => {
    const th = e.target.closest("table.items th[data-sort]");
    if (!th) return;
    const col = th.dataset.sort;
    // Newest first is the useful start for dates; A–Z for everything else.
    s = s.col === col ? { col, dir: -s.dir } : { col, dir: col === "updated" ? -1 : 1 };
    try { localStorage.setItem(KEY, JSON.stringify(s)); } catch (e) {}
    apply();
  });
  apply();
})();
"#;

async fn sync(State(app): S) -> Redirect {
    app.kick.notify_one();
    Redirect::to("/")
}

async fn item_page(State(app): S, Path(id): Path<String>) -> Response {
    let now = now();
    let items = app.items();
    let Some(it) = items.iter().find(|it| it.id == id) else {
        return (axum::http::StatusCode::NOT_FOUND, "no such item").into_response();
    };
    let decision = compile_current(&app).ok().map(|c| c.decide(it, now));
    page(
        &it.label,
        html! {
            h2 { a href=(it.url) target="_blank" { (it.label) } " " (it.title) }
            @if let Some(d) = decision {
                div.card {
                    @if d.show { span.good { "Shown" } } @else { span.bad { "Hidden" } }
                    " by rule " b { (d.rule) }
                    @if !d.tags.is_empty() { ", tags: " @for t in &d.tags { span.tag { (t) } } }
                    @for e in &d.errors { div.bad { (e) } }
                }
            }
            p.muted { "Fields available to rules:" }
            table {
                @for (k, v) in &it.fields {
                    tr { td { code { (k) } } td { pre style="margin:0;white-space:pre-wrap" { code { (fmt_value(v, now)) } } } }
                }
            }
        },
    )
    .into_response()
}

struct Preview {
    errors: Vec<(usize, String)>,
    before_shown: usize,
    after_shown: usize,
    changed: Vec<(Item, Decision, Decision)>,
}

fn preview(app: &App, text: &str) -> Preview {
    let now = now();
    let new = match app.compile(text) {
        Ok(c) => c,
        Err(errors) => return Preview { errors, before_shown: 0, after_shown: 0, changed: vec![] },
    };
    let old = compile_current(app).ok();
    let mut p = Preview { errors: vec![], before_shown: 0, after_shown: 0, changed: vec![] };
    for it in app.items().iter() {
        let a = old.as_ref().map(|c| c.decide(it, now));
        let b = new.decide(it, now);
        let a_show = a.as_ref().is_none_or(|a| a.show);
        p.before_shown += a_show as usize;
        p.after_shown += b.show as usize;
        if let Some(a) = a
            && (a.show != b.show || a.tags != b.tags)
        {
            p.changed.push((it.clone(), a, b));
        }
    }
    p
}

fn rules_editor(app: &App, text: &str, pv: Option<Preview>, saved: bool) -> Markup {
    let parsed = app.compile(text).ok();
    let blank_rows = 2;
    html! {
        @if saved { div.card.good { "Saved." } }
        @if let Some(p) = &pv {
            @if !p.errors.is_empty() {
                div.card.bad { "Not saved, the rules have errors:" ul { @for (l, e) in &p.errors { li { "line " (l) ": " (e) } } } }
            } @else {
                div.card {
                    b { "Preview: " } (p.before_shown) " shown now → " (p.after_shown) " shown with these rules. "
                    (p.changed.len()) " item(s) change."
                    @if !p.changed.is_empty() {
                        table {
                            tr { th { "" } th { "Title" } th { "Now" } th { "With these rules" } }
                            @for (it, a, b) in p.changed.iter().take(300) {
                                tr {
                                    td.nowrap { a href=(it.url) target="_blank" { (it.label) } }
                                    td { (it.title) }
                                    td.nowrap { (if a.show { "shown" } else { "hidden" }) " by " (a.rule) @for t in &a.tags { " " span.tag { (t) } } }
                                    td.nowrap { (if b.show { "shown" } else { "hidden" }) " by " (b.rule) @for t in &b.tags { " " span.tag { (t) } } }
                                }
                            }
                        }
                    }
                }
            }
        }
        h2 { "Rules as text" }
        p.muted { "One rule per line: " code { "show|hide|tag \"name\" if <condition>" } ", then " code { "default show|hide" } ". The first show or hide that matches decides; tags accumulate. " code { "#" } " starts a comment." }
        form method="post" action="/rules" {
            textarea name="text" spellcheck="false" { (text) }
            p { button name="op" value="preview" { "Preview" } " " button name="op" value="save" { "Save" } }
        }
        @if let Some(c) = &parsed {
            h2 { "Rules as a form" }
            p.muted { "Same rules, edited row by row. Drag the handle to reorder (or focus it and use the arrow keys). Saving from here drops comments." }
            form #rule-form method="post" action="/rules/form" {
                table {
                    thead { tr { th { "" } th { "Action" } th { "Name" } th { "Condition" } th { "Delete" } } }
                    tbody #rule-rows {
                        @for (i, r) in c.program.rules.iter().enumerate() {
                            (form_row(i, Some(r.action), &r.name, &r.cond_src))
                        }
                        @for j in 0..blank_rows {
                            (form_row(c.program.rules.len() + j, None, "", ""))
                        }
                    }
                    tfoot { tr {
                        td {} td { "default" }
                        td colspan="3" {
                            select name="default" {
                                option value="show" selected[c.program.default == Action::Show] { "show" }
                                option value="hide" selected[c.program.default == Action::Hide] { "hide" }
                            }
                        }
                    } }
                }
                p { button name="op" value="preview" { "Preview" } " " button name="op" value="save" { "Save" } }
            }
            script src="/js/drag.js" {}
        }
        h2 { "Reference" }
        details {
            summary { "Fields" }
            table { @for f in app.schema().fields { tr { td { code { (f.name) } } td.muted { (f.help) } } } }
        }
        details {
            summary { "Functions and operators" }
            table { @for f in FUNCTIONS { tr { td { code { (f.name) } } td.muted { (f.help) } } } }
            p.muted {
                "Operators: " code { "== != < > <= >= in contains matches and or not" } ". "
                "Literals: " code { "\"string\" 42 true [\"a\", \"b\"] 14d 3h 30m" } ". "
                "Globs: " code { "*" } " stays within a path segment, " code { "**" } " crosses them. "
                "A comparison involving a field the item doesn't have is false."
            }
        }
    }
}

/// Reorders rule rows by dragging the handle; positions are written into hidden inputs on submit.
const DRAG_JS: &str = r#"
(() => {
  const form = document.getElementById('rule-form');
  const tb = document.getElementById('rule-rows');
  let dragged = null;
  tb.addEventListener('mousedown', e => {
    const h = e.target.closest('.handle');
    if (h) h.closest('tr').draggable = true;
  });
  tb.addEventListener('dragstart', e => {
    dragged = e.target.closest('tr');
    dragged.classList.add('dragging');
    e.dataTransfer.effectAllowed = 'move';
    e.dataTransfer.setData('text/plain', '');
  });
  tb.addEventListener('dragover', e => {
    if (!dragged) return;
    e.preventDefault();
    const tr = e.target.closest('tr');
    if (!tr || tr === dragged) return;
    const r = tr.getBoundingClientRect();
    tb.insertBefore(dragged, e.clientY > r.top + r.height / 2 ? tr.nextSibling : tr);
  });
  tb.addEventListener('dragend', () => {
    if (!dragged) return;
    dragged.classList.remove('dragging');
    dragged.draggable = false;
    dragged = null;
  });
  tb.addEventListener('keydown', e => {
    const h = e.target.closest('.handle');
    if (!h || (e.key !== 'ArrowUp' && e.key !== 'ArrowDown')) return;
    e.preventDefault();
    const tr = h.closest('tr');
    if (e.key === 'ArrowUp' && tr.previousElementSibling) tb.insertBefore(tr, tr.previousElementSibling);
    if (e.key === 'ArrowDown' && tr.nextElementSibling) tb.insertBefore(tr.nextElementSibling, tr);
    h.focus();
  });
  form.addEventListener('submit', () => {
    [...tb.rows].forEach((tr, i) => { tr.querySelector('input.pos').value = i; });
  });
})();
"#;

fn form_row(i: usize, action: Option<Action>, name: &str, cond: &str) -> Markup {
    html! {
        tr {
            td {
                button.handle type="button" title="Drag to reorder" aria-label="Reorder rule" { "⠿" }
                input.pos type="hidden" name=(format!("pos.{i}")) value=(i);
            }
            td {
                select name=(format!("action.{i}")) {
                    @for a in [Action::Show, Action::Hide, Action::Tag] {
                        option value=(a.as_str()) selected[action == Some(a)] { (a.as_str()) }
                    }
                }
            }
            td { input type="text" name=(format!("name.{i}")) value=(name) size="18"; }
            td style="width:60%" { input type="text" name=(format!("cond.{i}")) value=(cond) style="width:100%" placeholder=(if cond.is_empty() { "new rule condition" } else { "" }); }
            td { input type="checkbox" name=(format!("del.{i}")); }
        }
    }
}

#[derive(Deserialize)]
struct SavedQuery {
    saved: Option<String>,
}

async fn rules_page(State(app): S, Query(q): Query<SavedQuery>) -> Markup {
    let text = app.current_rules().unwrap_or_default();
    page("Rules", rules_editor(&app, &text, None, q.saved.is_some()))
}

fn submit_rules(app: &App, text: String, op: &str) -> Response {
    let pv = preview(app, &text);
    if op == "save" && pv.errors.is_empty() {
        if let Err(e) = app.store.save_rules(&text) {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
        app.changes.bump();
        return Redirect::to("/rules?saved=1").into_response();
    }
    page("Rules", rules_editor(app, &text, Some(pv), false)).into_response()
}

#[derive(Deserialize)]
struct RulesText {
    text: String,
    op: String,
}

async fn rules_text(State(app): S, Form(f): Form<RulesText>) -> Response {
    submit_rules(&app, f.text.replace("\r\n", "\n"), &f.op)
}

async fn rules_form(State(app): S, Form(fields): Form<Vec<(String, String)>>) -> Response {
    let mut op = String::new();
    let mut default = "show".to_string();
    let mut rows: BTreeMap<usize, BTreeMap<String, String>> = BTreeMap::new();
    for (k, v) in fields {
        match k.split_once('.') {
            Some((f, i)) => {
                if let Ok(i) = i.parse() {
                    rows.entry(i).or_default().insert(f.to_string(), v);
                }
            }
            None if k == "op" => op = v,
            None if k == "default" => default = v,
            None => {}
        }
    }
    let mut rows: Vec<_> = rows
        .into_values()
        .filter(|r| !r.contains_key("del") && r.get("cond").is_some_and(|c| !c.trim().is_empty()))
        .collect();
    rows.sort_by_key(|r| r.get("pos").and_then(|p| p.trim().parse::<i64>().ok()).unwrap_or(i64::MAX));
    let mut text = String::new();
    for (n, r) in rows.iter().enumerate() {
        let action = r.get("action").and_then(|a| Action::parse(a)).unwrap_or(Action::Show);
        let name = r.get("name").map(|s| s.trim()).filter(|s| !s.is_empty()).map(String::from).unwrap_or_else(|| format!("rule {}", n + 1));
        text += &rules::format_rule(action, &name, r.get("cond").map_or("", |s| s.as_str()));
        text.push('\n');
    }
    text += &format!("default {default}\n");
    submit_rules(&app, text, &op)
}

async fn settings_page(State(app): S) -> Markup {
    page(
        "Settings",
        html! {
            form method="post" action="/settings" {
                table {
                    @for s in SETTINGS {
                        @let v = app.store.setting(s.key).unwrap_or_default();
                        tr {
                            td { code { (s.key) } }
                            td {
                                @if s.key == "github_token" {
                                    input type="password" name=(s.key) value="" placeholder=(if v.is_empty() { "not set" } else { "set (leave empty to keep)" }) size="40";
                                    " " label { input type="checkbox" name="clear_token"; " clear" }
                                } @else if s.multiline {
                                    textarea name=(s.key) rows="3" style="min-height:0" { (v) }
                                } @else {
                                    input type="text" name=(s.key) value=(v) size="40";
                                }
                            }
                            td.muted { (s.help) }
                        }
                    }
                }
                p { button { "Save" } }
            }
            p.muted { "Signed in as " code { (app.store.setting("github.login").unwrap_or_default()) } }
            h2 { "Reset sync" }
            p.muted { "Forget synced notifications and re-fetch everything updated in the last " code { "sync_days" } " days. Cached PR details and CODEOWNERS are kept." }
            form method="post" action="/settings/reset-sync" data-confirm="Clear synced notifications and re-sync?" {
                button { "Reset sync" }
            }
            h2 { "PR checkouts" }
            p.muted { "Worktrees made by the Open button. Merged or closed PRs are removed automatically after " code { "checkout_cleanup_days" } " unless they have local changes." }
            @let checkouts = app.store.checkouts().unwrap_or_default();
            @if checkouts.is_empty() { p.muted { "None yet." } }
            table {
                @for c in &checkouts {
                    tr {
                        td.nowrap { (c.repo) "#" (c.number) }
                        td { code.small { (c.path) } }
                        td class=[(c.status.starts_with("error") || c.status.contains("kept")).then_some("bad")] { (c.status) }
                        td.nowrap {
                            form method="post" action="/checkouts/remove" style="display:inline" data-confirm="Remove this worktree?" {
                                input type="hidden" name="repo" value=(c.repo);
                                input type="hidden" name="number" value=(c.number);
                                button { "Remove" }
                            }
                            " "
                            form method="post" action="/checkouts/remove" style="display:inline" data-confirm="Remove this worktree and its branch even if it has local changes?" {
                                input type="hidden" name="repo" value=(c.repo);
                                input type="hidden" name="number" value=(c.number);
                                input type="hidden" name="force" value="1";
                                button { "Force remove" }
                            }
                        }
                    }
                }
            }
            h2 { "Local clones" }
            p.muted { "Found by scanning " code { "repo_roots" } ", plus " code { "repo_paths" } " overrides." }
            form method="post" action="/settings/rescan" { button { "Rescan" } }
            table {
                @for r in app.store.local_repos().unwrap_or_default() {
                    tr {
                        td.nowrap { (r.repo) }
                        td { code.small { (r.path) } }
                        td.muted.small { @if !r.others.is_empty() { "also at: " (r.others.join(", ")) } }
                    }
                }
            }
        },
    )
}

async fn rescan(State(app): S) -> Response {
    match checkout::scan(&app.store) {
        Ok(_) => Redirect::to("/settings").into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn open_item(State(app): S, Path(id): Path<String>) -> Response {
    let items = app.items();
    let Some(it) = items.iter().find(|it| it.id == id) else {
        return (axum::http::StatusCode::NOT_FOUND, "no such item").into_response();
    };
    let (Value::Str(repo), Value::Int(number)) = (it.get("repo").clone(), it.get("number").clone()) else {
        return (axum::http::StatusCode::BAD_REQUEST, "not a pull request").into_response();
    };
    // Show progress right away, before the task gets scheduled.
    app.checkouts.running.lock().unwrap().insert(key(&repo, number), "starting".into());
    let app2 = app.clone();
    tokio::spawn(async move { app2.checkouts.open(&app2.store, &repo, number).await });
    Redirect::to("/").into_response()
}

#[derive(Deserialize)]
struct IgnoreForm {
    undo: Option<String>,
    back: Option<String>,
}

async fn ignore_item(State(app): S, Path(id): Path<String>, Form(f): Form<IgnoreForm>) -> Response {
    let items = app.items();
    let Some(it) = items.iter().find(|it| it.id == id) else {
        return (axum::http::StatusCode::NOT_FOUND, "no such item").into_response();
    };
    match app.set_ignored(&it.url, &it.label, f.undo.is_none()) {
        Ok(()) => {
            // Only same-site paths, so the form can't redirect elsewhere.
            let back = f.back.filter(|b| b.starts_with('/') && !b.starts_with("//"));
            Redirect::to(back.as_deref().unwrap_or("/")).into_response()
        }
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct RemoveForm {
    repo: String,
    number: i64,
    force: Option<String>,
}

async fn remove_checkout(State(app): S, Form(f): Form<RemoveForm>) -> Response {
    let Ok(Some(c)) = app.store.checkout(&f.repo, f.number) else {
        return Redirect::to("/settings").into_response();
    };
    if let Err(e) = checkout::remove(&app.store, &c, f.force.is_some()).await {
        let mut c = c;
        c.status = format!("not removed: {e:#}");
        let _ = app.store.put_checkout(&c);
    }
    app.changes.bump();
    Redirect::to("/settings").into_response()
}

async fn reset_sync(State(app): S) -> Response {
    match app.reset_sync() {
        Ok(()) => Redirect::to("/").into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn settings_save(State(app): S, Form(fields): Form<Vec<(String, String)>>) -> Response {
    let clear = fields.iter().any(|(k, _)| k == "clear_token");
    for (k, v) in &fields {
        let Some(s) = SETTINGS.iter().find(|s| s.key == k) else { continue };
        if s.key == "github_token" && v.is_empty() && !clear {
            continue;
        }
        let v = v.replace("\r\n", "\n");
        if let Err(e) = app.store.set_setting(s.key, v.trim()) {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    if clear {
        let _ = app.store.set_setting("github_token", "");
    }
    app.kick.notify_one();
    Redirect::to("/settings").into_response()
}
