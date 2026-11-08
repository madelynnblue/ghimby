#!/bin/sh
# Stops and removes the launchd agent, binary, database and log.
#   curl -fsSL https://raw.githubusercontent.com/madelynnblue/ghimby/main/scripts/uninstall.sh | sh
set -eu
LABEL=io.ghimby
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$LABEL.plist" "$HOME/.local/bin/ghimby" "$HOME/Library/Logs/ghimby.log"
rm -rf "$HOME/Library/Application Support/ghimby"
echo "ghimby removed, including its database and log"
