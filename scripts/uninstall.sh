#!/bin/sh
# Stops and removes the launchd agent and binary. --purge also deletes the database and log.
#   curl -fsSL https://raw.githubusercontent.com/madelynnblue/ghimby/main/scripts/uninstall.sh | sh -s -- --purge
set -eu
LABEL=io.ghimby
DATA="$HOME/Library/Application Support/ghimby"
LOG=$HOME/Library/Logs/ghimby.log

launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$LABEL.plist" "$HOME/.local/bin/ghimby"
if [ "${1:-}" = --purge ]; then
    rm -rf "$DATA" "$LOG"
    echo "ghimby removed, including its database and log"
else
    echo "ghimby removed; its database is kept in $DATA (rerun with --purge to delete it)"
fi
