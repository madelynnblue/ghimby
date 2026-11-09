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
hide "ignored"          if ignored
hide "bots"             if author in ["dependabot", "acme-ci"]
hide "already reviewed" if reviewed_by_me and not new_since_my_review
hide "draft"           if draft
show "storage"          if owned_by("@acme/storage", max_owners: 2)
default hide
```

Hidden items stay viewable in the app with the rule that hid them.

### Fields (per notification, after enrichment)

| field | type |
|---|---|
| `repo`, `author`, `title`, `reason`, `state`, `review_decision` | string |
| `draft`, `ignored`, `requested_for_me`, `reviewed_by_me`, `new_since_my_review` | bool |
| `labels`, `paths`, `requested_teams`, `requested_users`, `approved_by`, `changes_requested_by` | list of strings |
| `changed_files`, `additions`, `deletions` | int |
| `updated`, `created`, `last_activity` | timestamp (use `age(updated) > 14d`) |
| `codeowners` | per changed file, the owners of its last-matching CODEOWNERS line |

`new_since_my_review` is true when, after your latest review, there was a new commit or force-push,
a comment or review by another human, or a review re-request for you or one of your teams.

`ignored` is set by the Ignore button on an inbox row (keyed by the item's URL, so it survives
re-syncs). Ignoring only sets the field; the `hide "ignored"` rule is what hides it.

`review_decision` is `approved`, `changes_requested`, `review_required` or `none`: GitHub's
`reviewDecision` where branch protection requires reviews, otherwise derived from each reviewer's
latest approve or request-changes review (any request for changes wins). The inbox shows approved and
changes-requested PRs as a badge next to the title, naming the reviewers on hover.

PR details refresh when a newer notification arrives and every `subject_refresh_secs` for open PRs.
Each sync also checks open drafts with a light state-and-draft query and re-fetches any marked ready
for review, so it leaves the `draft` rule within one poll. After rules run, it also asks GitHub for
each shown PR's `updatedAt` and re-fetches any that moved, catching changes that send you no
notification, such as your own review. Your login and teams are re-fetched hourly or when the token
changes.

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
  Each row has an Ignore/Unignore button. The page stays current on its own: see Live updates.
- **Rules**: textarea with parse errors inline, a form view of the same rules, and a dry-run showing
  which items would change bucket before saving.
- **Settings**: token source, poll interval.
- **PR** (`/pr/<owner>/<repo>/<N>`, linked from each PR title): read-only description, conversation,
  and the diff with review threads inline. See below.

## Security

There's no login, and the app can run commands and approve PRs as you, so:

- It only listens on a loopback address (`--listen` refuses anything else).
- Requests whose `Host` isn't `127.0.0.1`, `localhost` or `[::1]` on our port are refused, which
  defeats DNS rebinding. POSTs must also have no `Origin` or our own.
- Responses carry a CSP with `script-src 'self'` (scripts are served from `/js`, so GitHub's HTML on
  the PR page can't run any) and `frame-ancestors 'none'` against clickjacking.
- The database, which may hold a token, is created mode 0600.

## Live updates

The server keeps a version counter, bumped when items actually change after a sync (compared, not
just rebuilt), on Ignore, on rules save, when a checkout job starts, progresses or ends, and when a
sync's error or checkout cleanup changes. `/events` is a Server-Sent Events stream that sends the
version on change and the sync status line when it changes.

The inbox embeds its version. On a newer one it reloads only when that can't move things under the
pointer: right away if the tab is hidden or the window is unfocused, otherwise the moment the window
regains focus. Never while the pointer is on a link, button or summary; it waits until the pointer
moves off. Open hidden groups and the scroll position survive the reload. The status line updates in
place without a reload.

## PR page

Shows a PR's description, conversation (comments and reviews), and per-file diffs with review
threads under the line they're anchored to. Resolved threads start collapsed; outdated threads, and
threads on lines missing from the patch, sit above their file's diff with the comment's hunk.

- Data: one GraphQL query (`bodyHTML`, last 100 comments and reviews, first 100 review threads) plus
  REST `pulls/N/files` for patches. Bodies are GitHub's rendered, sanitized HTML. Truncation is noted
  on the page with a link to GitHub. Files with no patch (binary, too large) link to GitHub.
- Cache: `pr_pages(key, json, updated_at, fetched_at)`. A page is stale when the PR's `updatedAt`
  (the item's `last_activity`, which moves on commits, comments and reviews) is newer than the cached
  one, or a notification arrived after it was fetched.
- Sync fetches pages only for shown PRs that are uncached or stale, and drops pages of PRs no longer
  in the notification window. Hidden PRs are fetched when viewed: opening a stale or uncached page
  fetches it first.
- A Refresh button at the top re-fetches it on demand.
- Approve (open PRs by others, after a confirm) posts an APPROVE review pinned to the page's
  `headRefOid`, so it covers exactly the diff shown, then re-fetches the PR. Once you've approved, it
  shows "you approved" instead.

## Open a PR locally

An "Open" button on each PR row checks the PR out in its own git worktree and opens it in an editor,
set up so VS Code's GitHub Pull Requests extension treats it as that PR (description, inline review
comments, adding comments and reviews).

### Finding the local clone

- Setting `repo_roots`: folders to scan (default `~/code, ~/code/github.com/*`), a few levels deep.
- A clone matches `owner/repo` by its `origin` remote, normalized across `git@github.com:`,
  `org-…@github.com:` and `https://` forms. Git worktrees and jj workspaces are skipped (`.git` is a
  file, or `.jj/repo` is a file).
- Setting `repo_paths`: `owner/repo = path` lines that win over scanning, for duplicates such as two
  clones of the same repo.
- Scan results are cached in SQLite and listed on the settings page with a rescan button.
- Not cloned: `gh repo clone` (plain git, not jj) into the `clone_path` setting
  (default `~/code/github.com/{owner}/{repo}`), then continue.

### One worktree per PR

Layout mirrors Claude Code's `claude --worktree "#1234"` (`.claude/worktrees/pr-1234`), under `.ghimby` so the two tools stay separate:

- Path: `<clone>/.ghimby/worktrees/pr-<N>`.
- Branch: `worktree-pr-<N>`.
- `.ghimby/worktrees/` is added to the clone's `.git/info/exclude` (local only, never committed).
  jj already skips nested worktrees.

The path and branch depend only on the clone and PR number, so every click lands in the same place. A
`checkouts(repo, number, path, branch, head_oid, updated_at)` table records them, and a per-PR lock
serializes concurrent clicks.

On click:

1. Look up the PR's current `headRefOid` with GraphQL.
2. `git fetch origin pull/<N>/head`. This works for fork PRs too.
3. If no worktree exists at the path (checked against `git worktree list`), run
   `git worktree add -B worktree-pr-<N> <path> FETCH_HEAD`.
4. If it exists and is behind: when it has no uncommitted changes and no commits beyond the last head
   ghimby checked out, `git reset --hard FETCH_HEAD` (handles force-pushes). Otherwise leave it and
   report "not updated: local changes".
5. Set `branch.worktree-pr-<N>.github-pr-owner-number = <owner>#<repo>#<N>` and
   `branch.worktree-pr-<N>.remote = origin`. The PR extension reads these to link the branch to the PR.
6. Run the `editor_cmd` setting (default `code {path}`, `cursor {path}` also works). Empty uses the
   system opener (`open` on macOS, `xdg-open` on Linux).

Using git rather than a jj workspace is deliberate: the PR extension expects git. jj sees the local
`worktree-pr-<N>` branch as a bookmark, which keeps the PR commits from being abandoned by later
`jj git fetch` runs.

### UI and safety

- The button is a form POST, not a link, since it runs commands. Requests whose `Origin` isn't the app
  itself are rejected, so other sites can't trigger it.
- The row shows progress and the result (opened, updated, not updated, error).
- Settings page lists existing checkouts with a remove button
  (`git worktree remove` + delete the branch).

### Cleanup

A sweep runs at most hourly, after a sync, and removes a checkout only when nothing would be lost:

- The PR has been merged or closed for longer than `checkout_cleanup_days` (default 1).
- The worktree has no uncommitted changes and no commits beyond the last head ghimby checked out.

Removal runs `git worktree remove`, deletes the `worktree-pr-<N>` branch and its two
`github-pr-*`/`remote` config keys, and drops the record. Checkouts that fail the safety check stay and
are flagged on the settings page ("PR closed, has local changes"). If the folder was deleted by hand,
the sweep runs `git worktree prune` and drops the record. Open PRs are never removed automatically.

## Not in v1

- Marking notifications read/done on GitHub (write access to the account).
- Sources other than GitHub notifications.

## Versions and auto-update

Release builds get their tag (`v0.1.3`) baked in at compile time; local builds report `dev`. The top
bar shows it, and `ghimby --version` prints it.

After a sync, at most once a day, a release build run by launchd or systemd (`XPC_SERVICE_NAME` or
`INVOCATION_ID` is set) asks GitHub for the latest release. If it's newer, it downloads this
platform's binary next to itself, checks that `--version` reports the new tag, renames it over the
running binary and exits; the service manager starts the new one. It skips the check while a checkout
is running, and the `auto_update` setting turns it off. Open pages reload on their own, since the
restarted server's change counter starts from the clock.
