#!/bin/sh
# Stops and removes the launchd agent. Leaves the database in ~/Library/Application Support/ghimby.
set -eu
LABEL=io.ghimby
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$LABEL.plist" "$HOME/.local/bin/ghimby"
