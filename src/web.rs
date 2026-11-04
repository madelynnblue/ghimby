//! Server-rendered web UI.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde::Deserialize;

use crate::app::App;
use crate::item::{Item, Value};
use crate::rules::{self, Action, Compiled, Decision, FUNCTIONS};
use crate::store::{SETTINGS, now};

type S = State<Arc<App>>;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(inbox))
        .route("/sync", post(sync))
        .route("/item/{id}", get(item_page))
        .route("/rules", get(rules_page).post(rules_text))
        .route("/rules/form", post(rules_form))
        .route("/settings", get(settings_page).post(settings_save))
        .with_state(app)
}

const CSS: &str = r#"
:root { --fg:#1f2328; --muted:#656d76; --bg:#fff; --card:#f6f8fa; --border:#d0d7de; --accent:#0969da; --bad:#cf222e; --good:#1a7f37; }
@media (prefers-color-scheme: dark) { :root { --fg:#e6edf3; --muted:#8d96a0; --bg:#0d1117; --card:#161b22; --border:#30363d; --accent:#4493f8; --bad:#f85149; --good:#3fb950; } }
* { box-sizing: border-box; }
body { font: 14px/1.5 -apple-system, system-ui, sans-serif; color: var(--fg); background: var(--bg); margin: 0 auto; padding: 16px; max-width: 1200px; }
a { color: var(--accent); text-decoration: none; } a:hover { text-decoration: underline; }
nav { display: flex; gap: 16px; align-items: center; border-bottom: 1px solid var(--border); padding-bottom: 8px; margin-bottom: 16px; }
nav b { margin-right: 8px; }
table { border-collapse: collapse; width: 100%; }
td, th { text-align: left; padding: 4px 8px; border-bottom: 1px solid var(--border); vertical-align: top; }
th { color: var(--muted); font-weight: 500; }
.muted { color: var(--muted); } .bad { color: var(--bad); } .good { color: var(--good); }
.tag { display: inline-block; font-size: 12px; padding: 0 6px; border: 1px solid var(--border); border-radius: 10px; margin-right: 4px; }
.card { background: var(--card); border: 1px solid var(--border); border-radius: 6px; padding: 8px 12px; margin: 8px 0; }
details > summary { cursor: pointer; padding: 4px 0; }
textarea, input[type=text], input[type=password], select { font: 13px ui-monospace, monospace; color: var(--fg); background: var(--bg); border: 1px solid var(--border); border-radius: 4px; padding: 4px 6px; }
textarea { width: 100%; min-height: 260px; }
button { font: inherit; padding: 4px 12px; border: 1px solid var(--border); border-radius: 6px; background: var(--card); color: var(--fg); cursor: pointer; }
code { font: 12px ui-monospace, monospace; }
.nowrap { white-space: nowrap; }
"#;

fn page(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · ghimby" }
                style { (PreEscaped(CSS)) }
            }
            body {
                nav {
                    b { "ghimby" }
                    a href="/" { "Inbox" }
                    a href="/rules" { "Rules" }
                    a href="/settings" { "Settings" }
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

fn str_field<'a>(it: &'a Item, f: &str) -> &'a str {
    match it.get(f) {
        Value::Str(s) => s,
        _ => "",
    }
}

fn item_rows(items: &[(&Item, Decision)], limit: usize, now: i64) -> Markup {
    html! {
        table {
            tr { th { "" } th { "Title" } th { "Author" } th { "Reason" } th { "Updated" } }
            @for (it, d) in items.iter().take(limit) {
                tr {
                    td.nowrap { a href=(it.url) target="_blank" { (it.label) } }
                    td {
                        (it.title) " "
                        @for t in &d.tags { span.tag { (t) } }
                        @if !d.errors.is_empty() { span.tag.bad title=(d.errors.join("\n")) { "rule error" } }
                    }
                    td.nowrap.muted { (str_field(it, "author")) }
                    td.nowrap.muted { (str_field(it, "reason")) }
                    td.nowrap.muted { a href=(format!("/item/{}", it.id)) title="Why?" { (ago(now - updated(it))) } }
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
        div.card {
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
            (item_rows(&shown, usize::MAX, now))
            h2 { "Hidden " span.muted { "(" (hidden.iter().map(|(_, v)| v.len()).sum::<usize>()) ")" } }
            @for (rule, v) in &hidden {
                details {
                    summary { "by " b { (rule) } " " span.muted { "(" (v.len()) ")" } }
                    (item_rows(v, 300, now))
                }
            }
        },
    )
}

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
            p.muted { "Same rules, edited row by row. Saving from here drops comments. Rows are ordered by the # column." }
            form method="post" action="/rules/form" {
                table {
                    tr { th { "#" } th { "Action" } th { "Name" } th { "Condition" } th { "Delete" } }
                    @for (i, r) in c.program.rules.iter().enumerate() {
                        (form_row(i, (i + 1) * 10, Some(r.action), &r.name, &r.cond_src))
                    }
                    @for j in 0..blank_rows {
                        @let i = c.program.rules.len() + j;
                        (form_row(i, (i + 1) * 10, None, "", ""))
                    }
                    tr {
                        td {} td { "default" }
                        td colspan="3" {
                            select name="default" {
                                option value="show" selected[c.program.default == Action::Show] { "show" }
                                option value="hide" selected[c.program.default == Action::Hide] { "hide" }
                            }
                        }
                    }
                }
                p { button name="op" value="preview" { "Preview" } " " button name="op" value="save" { "Save" } }
            }
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

fn form_row(i: usize, pos: usize, action: Option<Action>, name: &str, cond: &str) -> Markup {
    html! {
        tr {
            td { input type="text" name=(format!("pos.{i}")) value=(pos) size="3"; }
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
        },
    )
}

async fn settings_save(State(app): S, Form(fields): Form<Vec<(String, String)>>) -> Response {
    let clear = fields.iter().any(|(k, _)| k == "clear_token");
    for (k, v) in &fields {
        let Some(s) = SETTINGS.iter().find(|s| s.key == k) else { continue };
        if s.key == "github_token" && v.is_empty() && !clear {
            continue;
        }
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
