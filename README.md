# ghimby

A local web app for finding the GitHub notifications you actually need to look at, mostly review
requests that reach you through broad CODEOWNERS team rules.

It syncs your notifications, adds PR details and CODEOWNERS ownership, and runs rules you edit in the
app to decide what's shown. Hidden items stay one click away, grouped by the rule that hid them.

## Running

```
cargo run -- --db ghimby.db
```

Then open http://127.0.0.1:7878. All config (rules, settings, caches) lives in the SQLite file given by
`--db`. By default it gets a GitHub token from `gh auth token`.

## Rules

Rules run top to bottom. `show` and `hide` stop at the first match; `tag` adds a label and keeps going.

```
hide "finished" if state != "open"
hide "ignored" if ignored
hide "bots" if author in ["dependabot", "renovate"]
hide "already reviewed" if reviewed_by_me and not new_since_my_review
tag "draft" if draft
show "direct" if reason in ["mention", "author", "assign"] or requested_for_me
show "storage" if owned_by("@acme/storage", max_owners: 2)
default hide
```

The Rules page has a text editor, a drag-to-reorder form, a preview of what would change, and a
reference of every field and function.

## Features

- **Review badges**: approved or changes-requested PRs are marked next to the title.
- **Ignore**: hide a specific PR from the inbox.
- **PR page**: click a title for the description, conversation, and diff with review threads inline,
  cached locally and re-fetched when the PR changes.
- **Open**: check a PR out in its own git worktree (`<clone>/.ghimby/worktrees/pr-<N>`) and open it in
  your editor, linked for VS Code's GitHub Pull Requests extension. Worktrees of merged PRs are cleaned
  up automatically if they have no local changes.

See [DESIGN.md](DESIGN.md) for details.
