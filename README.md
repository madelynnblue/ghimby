# ghimby

A web app for triaging GitHub reviews.

Does your CODEOWNERS make it difficult to identify which PRs you need to review?
Have you failed to find the correct gmail filters to see when a PR has new comments or a push?
ghimby solves this with a configurable, fast way to see which PRs you need to review or re-review.

## Features

- So fast: most clicks are instant.
- Configurable rules with a DSL.
- See the full PR diff without opening an editor or the GitHub PR page.
- Open the PR in a dedicated workspace in your local editor.
- If that editor is VS Code and you have the GitHub Pull Requests extension installed, it works as expected.

## Installation

Install [gh](https://cli.github.com) and run (`gh auth login`).
Install ghimby:

```
curl -fsSL https://raw.githubusercontent.com/madelynnblue/ghimby/main/scripts/install.sh | sh
```

This will open http://127.0.0.1:7880.
Run it again to update.

Supports:

- macOS
- linux (amd64, arm64)

### Uninstallation

```
curl -fsSL https://raw.githubusercontent.com/madelynnblue/ghimby/main/scripts/uninstall.sh | sh
```
