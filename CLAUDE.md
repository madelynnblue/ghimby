# ghimby

Rust web app that filters GitHub notifications with rules stored in SQLite. Design is in DESIGN.md.

## Building

- cargo is at `~/.cargo/bin` (`export PATH=$HOME/.cargo/bin:$PATH`). rustfmt and clippy aren't installed.
- `cargo run -- --db ghimby.db` serves on 127.0.0.1:7878 (`.claude/launch.json` has a preview config).

## Source control

- jj, colocated with git. Remote `origin` is git@github.com:madelynnblue/ghimby.git.
- Don't commit or push on your own. When a change is done, ask the user whether to commit (offer a
  "Yes, commit" button). Once they confirm, describe it, `jj new`, then push:
  `jj bookmark set main -r @- && jj git push --bookmark main`, then deploy with `scripts/deploy.sh`.
- Releases: lightweight `vX.Y.Z` tags on a pushed `main` commit, e.g.
  `git tag v0.2.0 <commit> && git push origin v0.2.0`. Bump minor for features, patch for fixes.
  The tag push runs `.github/workflows/release.yml`, which builds binaries with `GHIMBY_VERSION`
  set; installed release builds self-update to it. Local `scripts/deploy.sh` builds show "dev".
- The deployed service (launchd agent `io.ghimby`) serves on 7880 with its own database. The dev
  preview on 7878 uses `./ghimby.db`.
