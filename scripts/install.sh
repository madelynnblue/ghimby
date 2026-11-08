#!/bin/sh
# Installs ghimby and (re)starts it as a launchd agent. Safe to run repeatedly; rerun to update.
#   curl -fsSL https://raw.githubusercontent.com/madelynnblue/ghimby/main/scripts/install.sh | sh
# Downloads the latest release (or GHIMBY_VERSION, e.g. v0.2.0), or installs the binary given as $1.
set -eu

REPO=madelynnblue/ghimby
LABEL=io.ghimby
PORT=${GHIMBY_PORT:-7880}
BIN=$HOME/.local/bin/ghimby
DATA="$HOME/Library/Application Support/ghimby"
LOG=$HOME/Library/Logs/ghimby.log
PLIST=$HOME/Library/LaunchAgents/$LABEL.plist

if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
    echo "ghimby's installer supports Apple silicon Macs only" >&2
    exit 1
fi
command -v gh >/dev/null || echo "warning: gh not found; ghimby gets its GitHub token from 'gh auth token'" >&2

mkdir -p "$(dirname "$BIN")" "$DATA" "$(dirname "$PLIST")"
chmod 700 "$DATA"
if [ $# -gt 0 ]; then
    cp "$1" "$BIN.new"
else
    if [ -n "${GHIMBY_VERSION:-}" ]; then
        URL=https://github.com/$REPO/releases/download/$GHIMBY_VERSION/ghimby-darwin-arm64
    else
        URL=https://github.com/$REPO/releases/latest/download/ghimby-darwin-arm64
    fi
    echo "downloading $URL"
    curl -fsSL "$URL" -o "$BIN.new"
    chmod +x "$BIN.new"
fi
# Rename into place: overwriting a running binary in place can get it killed on macOS.
mv "$BIN.new" "$BIN"

cat > "$PLIST.new" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>$BIN</string>
        <string>--db</string><string>$DATA/ghimby.db</string>
        <string>--listen</string><string>127.0.0.1:$PORT</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <!-- launchd's default PATH lacks Homebrew, which has gh and git. -->
        <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    </dict>
    <key>WorkingDirectory</key><string>$DATA</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>$LOG</string>
    <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
EOF

DOMAIN=gui/$(id -u)
if cmp -s "$PLIST.new" "$PLIST" && launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    rm "$PLIST.new"
    launchctl kickstart -k "$DOMAIN/$LABEL"
else
    mv "$PLIST.new" "$PLIST"
    launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
    launchctl bootstrap "$DOMAIN" "$PLIST"
fi
echo "ghimby running at http://127.0.0.1:$PORT (log: $LOG)"
