//! Read-only PR page: description, conversation, and the diff with review threads inline.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, PreEscaped, html};

use super::{Local, S, ago, local_cell, page};
use crate::prpage::{self, PrPage, Thread, parse_patch, thread_anchor};
use crate::store::now;

/// Files with longer diffs start collapsed.
const COLLAPSE_LINES: usize = 400;

pub fn href(repo: &str, number: i64) -> String {
    format!("/pr/{repo}/{number}")
}

pub async fn show(State(app): S, Path((owner, repo, number)): Path<(String, String, i64)>) -> Response {
    let repo = format!("{owner}/{repo}");
    let (p, err) = app.pr_page(&repo, number, false).await;
    render(&app, &repo, number, p, err).into_response()
}

pub async fn refresh(State(app): S, Path((owner, repo, number)): Path<(String, String, i64)>) -> Response {
    let repo = format!("{owner}/{repo}");
    let (p, err) = app.pr_page(&repo, number, true).await;
    match err {
        None => Redirect::to(&href(&repo, number)).into_response(),
        Some(e) => render(&app, &repo, number, p, Some(e)).into_response(),
    }
}

fn render(app: &crate::app::App, repo: &str, number: i64, p: Option<PrPage>, err: Option<String>) -> Markup {
    let now = now();
    let label = format!("{repo}#{number}");
    let key = prpage::key(repo, number);
    let items = app.items();
    let item = items.iter().find(|it| prpage::item_key(it).as_deref() == Some(key.as_str()));
    let local = Local::load(app);
    let top = html! {
        div.card {
            form method="post" action=(format!("{}/refresh", href(repo, number))) style="display:inline" {
                button title="Fetch this PR from GitHub now" { "Refresh" }
            }
            " "
            @if let Some(p) = &p { span.muted { "Fetched " (ago(now - p.fetched_at)) " ago" } }
            @if let Some(it) = item { div style="display:inline-block; margin-left:12px" { (local_cell(it, &local)) } }
            @if let Some(e) = &err { div.bad { "Fetching from GitHub failed: " (e) } }
        }
    };
    let Some(p) = p else {
        return page(&label, html! { (top) p { a href=(format!("https://github.com/{repo}/pull/{number}")) target="_blank" { "View on GitHub" } } });
    };
    let mut by_path: HashMap<&str, Vec<&Thread>> = HashMap::new();
    for t in &p.threads {
        by_path.entry(t.path.as_str()).or_default().push(t);
    }
    let orphans: Vec<&Thread> = p.threads.iter().filter(|t| !p.files.iter().any(|f| f.path == t.path)).collect();
    page(
        &label,
        html! {
            (top)
            h2 {
                a href=(p.url) target="_blank" { (label) } " " (p.title)
            }
            p.muted {
                span.tag class=(state_class(&p.state)) { (p.state) }
                @if p.draft { span.tag { "draft" } }
                b { (p.author) } " wants to merge " code { (p.head) } " into " code { (p.base) }
                " · opened " (ago(now - p.created)) " ago · "
                span.good { "+" (p.additions) } " " span.bad { "−" (p.deletions) }
                " in " (p.files.len()) " files"
            }
            @for n in &p.notes { div.card.muted { (n) " " a href=(p.url) target="_blank" { "See GitHub for the rest." } } }
            div.card.md { @if p.body_html.is_empty() { span.muted { "No description." } } @else { (PreEscaped(&p.body_html)) } }

            h3 { "Conversation " span.muted { "(" (p.events.len()) ")" } }
            @for e in &p.events {
                div.card {
                    div.muted.small {
                        b { (e.author) } " " (event_verb(&e.kind)) " · "
                        a href=(e.url) target="_blank" { (ago(now - e.at)) " ago" }
                    }
                    @if !e.body_html.is_empty() { div.md { (PreEscaped(&e.body_html)) } }
                }
            }

            h3 { "Files " span.muted { "(" (p.files.len()) ")" } }
            table.small {
                @for (i, f) in p.files.iter().enumerate() {
                    @let n = by_path.get(f.path.as_str()).map_or(0, Vec::len);
                    tr {
                        td { a href=(format!("#f{i}")) { (f.path) } }
                        td.nowrap { span.good { "+" (f.additions) } " " span.bad { "−" (f.deletions) } }
                        td.nowrap.muted { @if n > 0 { (n) " thread" @if n > 1 { "s" } } }
                    }
                }
            }
            @for (i, f) in p.files.iter().enumerate() {
                @let threads = by_path.get(f.path.as_str()).cloned().unwrap_or_default();
                @let lines = f.patch.as_deref().map(parse_patch).unwrap_or_default();
                @let anchors: Vec<Option<usize>> = threads.iter().map(|t| thread_anchor(&lines, t)).collect();
                details.file id=(format!("f{i}")) open[lines.len() <= COLLAPSE_LINES] {
                    summary {
                        code { @if let Some(prev) = &f.previous { (prev) " → " } (f.path) }
                        " " span.good { "+" (f.additions) } " " span.bad { "−" (f.deletions) }
                        @if !threads.is_empty() { " " span.tag { (threads.len()) " thread" @if threads.len() > 1 { "s" } } }
                    }
                    @for (t, _) in threads.iter().zip(&anchors).filter(|(_, a)| a.is_none()) {
                        (thread(t, now, true))
                    }
                    @if f.patch.is_none() {
                        p.muted { "No diff available (binary or too large). " a href=(format!("{}/files", p.url)) target="_blank" { "View on GitHub" } }
                    } @else {
                        table.diff {
                            @for (li, l) in lines.iter().enumerate() {
                                tr class=(line_class(l.kind)) {
                                    td.ln { (l.old.map(|n| n.to_string()).unwrap_or_default()) }
                                    td.ln { (l.new.map(|n| n.to_string()).unwrap_or_default()) }
                                    td.code { @if matches!(l.kind, '+' | '-') { (l.kind) } @else if l.kind == ' ' { " " } (l.text) }
                                }
                                @for (t, _) in threads.iter().zip(&anchors).filter(|(_, a)| **a == Some(li)) {
                                    tr { td colspan="3" { (thread(t, now, false)) } }
                                }
                            }
                        }
                    }
                }
            }
            @if !orphans.is_empty() {
                h3 { "Threads on files not in this diff" }
                @for t in &orphans { (thread(t, now, true)) }
            }
        },
    )
}

fn state_class(state: &str) -> &'static str {
    match state {
        "open" => "good",
        "merged" => "merged",
        _ => "bad",
    }
}

fn event_verb(kind: &str) -> &str {
    match kind {
        "APPROVED" => "approved",
        "CHANGES_REQUESTED" => "requested changes",
        "COMMENTED" => "reviewed",
        "DISMISSED" => "reviewed (dismissed)",
        k => k,
    }
}

fn line_class(kind: char) -> &'static str {
    match kind {
        '+' => "add",
        '-' => "del",
        '@' | '\\' => "hunk",
        _ => "",
    }
}

/// A review thread. `detached` threads aren't placed on a diff line, so they show their path and hunk.
fn thread(t: &Thread, now: i64, detached: bool) -> Markup {
    let first = t.comments.first();
    let range = match (t.start_line, t.line) {
        (Some(s), Some(e)) if s != e => format!("lines {s}–{e}"),
        (_, Some(e)) => format!("line {e}"),
        _ => "outdated".into(),
    };
    let body = html! {
        @if detached {
            div.muted.small { code { (t.path) } " · " (range) @if t.outdated { " · outdated" } }
            @if let Some(c) = first { @if !c.diff_hunk.is_empty() { pre.hunk { (last_lines(&c.diff_hunk, 6)) } } }
        }
        @for c in &t.comments {
            div.comment {
                div.muted.small { b { (c.author) } " · " a href=(c.url) target="_blank" { (ago(now - c.at)) " ago" } }
                div.md { (PreEscaped(&c.body_html)) }
            }
        }
    };
    if t.resolved {
        html! {
            details.thread {
                summary.muted.small {
                    "Resolved thread"
                    @if let Some(c) = first { " by " (c.author) }
                    " (" (t.comments.len()) ")"
                }
                (body)
            }
        }
    } else {
        html! { div.thread { (body) } }
    }
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
