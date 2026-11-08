#!/bin/sh
# Stops and removes the background service, binary, database and log.
#   curl -fsSL https://raw.githubusercontent.com/madelynnblue/ghimby/main/scripts/uninstall.sh | sh
set -eu
if [ "$(uname -s)" = Darwin ]; then
    LABEL=io.ghimby
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/$LABEL.plist" "$HOME/Library/Logs/ghimby.log"
    rm -rf "$HOME/Library/Application Support/ghimby"
else
    systemctl --user disable --now ghimby 2>/dev/null || true
    rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ghimby.service"
    systemctl --user daemon-reload 2>/dev/null || true
    rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/ghimby"
fi
rm -f "$HOME/.local/bin/ghimby"
echo "ghimby removed, including its database and log"
