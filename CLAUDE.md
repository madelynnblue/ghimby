# ghimby

Rust web app that filters GitHub notifications with rules stored in SQLite. Design is in DESIGN.md.

## Building

- cargo is at `~/.cargo/bin` (`export PATH=$HOME/.cargo/bin:$PATH`). rustfmt and clippy aren't installed.
- `cargo run -- --db ghimby.db` serves on 127.0.0.1:7878 (`.claude/launch.json` has a preview config).

## Source control

- jj, colocated with git. Remote `origin` is git@github.com:madelynnblue/ghimby.git.
- After a change is fully made and committed (described, with `jj new` on top), push it:
  `jj bookmark set main -r @- && jj git push --bookmark main`
