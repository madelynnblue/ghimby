# ghimby design

A local web app that pulls GitHub notifications, enriches them, and runs user-editable rules to decide
what's worth looking at. All config lives in a SQLite file passed at startup (`ghimby --db ghimby.db`).

## Motivation

Broad CODEOWNERS rules request reviews from whole teams, so most notifications are team review requests
for PRs you don't need to see.

- Nearly all unread notifications were `review_requested`, and most of those PRs were already closed.
- One rule separated the wanted PRs from the rest: **show if any changed file's last-matching CODEOWNERS
  rule names your team and lists ≤ 2 owners**.
  - Team-request status was not useful: wanted PRs sometimes had no pending request, and unwanted ones
    often had the team requested via a many-team fallback `*` rule.
  - Bot PRs leak through via team-owned paths, so they need their own hide rule.

## Rules language

Text is the source of truth; the form builder reads and writes the same text. Rules are evaluated top to
bottom. `show`/`hide` stop at the first match; `tag` adds a label and keeps going.

```
hide "finished"         if state != "open"
hide "bots"             if author in ["dependabot", "acme-ci"]
hide "already reviewed" if reviewed_by_me and not new_since_my_review
tag  "draft"            if draft
show "storage"           if owned_by("@acme/storage", max_owners: 2)
default hide
```

Hidden items stay viewable in the app with the rule that hid them.

### Fields (per notification, after enrichment)

| field | type |
|---|---|
| `repo`, `author`, `title`, `reason`, `state` | string |
| `draft`, `reviewed_by_me`, `new_since_my_review` | bool |
| `labels`, `paths`, `requested_teams`, `requested_users` | list of strings |
| `changed_files`, `additions`, `deletions` | int |
| `updated`, `created` | timestamp (use `age(updated) > 14d`) |
| `codeowners` | per changed file, the owners of its last-matching CODEOWNERS line |

`new_since_my_review` is true when, after your latest review, there was a new commit or force-push,
a comment or review by another human, or a review re-request for you or one of your teams.

A comparison against a field the item doesn't have (e.g. `state` on a release notification) is false.

### Operators and functions

- `==`, `!=`, `<`, `>`, `in [...]`, `contains`, `matches "glob"`, `and`, `or`, `not`, parentheses.
- `any(paths, "storage/**")`: a list element matches a glob.
- `owned_by(team, max_owners: N)`: some changed file's last-matching CODEOWNERS rule includes `team` and
  has at most N owners.
- `requested(team_or_user)`.

New fields and functions are added by registering them with the evaluator; the parser doesn't change.

## Architecture

```
src/
  main.rs          CLI args (db path, listen addr), wiring
  store/           rusqlite, migrations, typed accessors for settings, rules, caches
  source/          trait Source { fetch() -> Vec<Item> }
    github/        notifications polling (Last-Modified / X-Poll-Interval), GraphQL PR enrichment
  codeowners.rs    parse + last-match lookup, cached per repo + default-branch sha
  item.rs          generic Item { source, id, url, fields: map<String, Value> }
  rules/           lexer, parser, AST, evaluator, function registry
  web/             axum + server-rendered HTML (maud) + htmx
```

`Item` is source-agnostic, so a future source (Jira, other GitHub queries) produces Items and the same
rules engine applies.

### SQLite tables

- `settings(key, value)`: GitHub token or token command (default `gh auth token`), poll interval, login.
- `rules(id, text, updated_at)`: the rules program (one row; history kept by `updated_at`).
- `notifications(id, json, fetched_at)`, `pr_facts(key, json, fetched_at)`,
  `codeowners(repo, sha, content)`: caches so restarts don't re-fetch everything.

### Pages

- **Inbox**: shown items (with tags), then a collapsed hidden section grouped by the rule that hid each.
- **Rules**: textarea with parse errors inline, a form view of the same rules, and a dry-run showing
  which items would change bucket before saving.
- **Settings**: token source, poll interval.

## Not in v1

- Marking notifications read/done on GitHub (write access to the account).
- Sources other than GitHub notifications.
