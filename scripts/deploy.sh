#!/bin/sh
# Builds from this checkout and installs it as the launchd agent. Safe to run repeatedly.
set -eu

REPO=$(cd "$(dirname "$0")/.." && pwd)
DATA="$HOME/Library/Application Support/ghimby"

export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release --manifest-path "$REPO/Cargo.toml"

# First install starts from the dev database so rules and ignores carry over.
if [ ! -e "$DATA/ghimby.db" ] && [ -e "$REPO/ghimby.db" ]; then
    mkdir -p "$DATA"
    sqlite3 "$REPO/ghimby.db" ".backup '$DATA/ghimby.db'"
fi

exec "$REPO/scripts/install.sh" "$REPO/target/release/ghimby"
