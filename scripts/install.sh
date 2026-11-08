#!/bin/sh
# Installs ghimby and (re)starts it as a background service (launchd on macOS, a systemd user
# service on Linux). Safe to run repeatedly; rerun to update.
#   curl -fsSL https://raw.githubusercontent.com/madelynnblue/ghimby/main/scripts/install.sh | sh
# Downloads the latest release (or GHIMBY_VERSION, e.g. v0.2.0), or installs the binary given as $1.
set -eu

REPO=madelynnblue/ghimby
PORT=${GHIMBY_PORT:-7880}
BIN=$HOME/.local/bin/ghimby
URL=http://127.0.0.1:$PORT

case "$(uname -s)/$(uname -m)" in
    Darwin/arm64) OS=mac ASSET=ghimby-darwin-arm64 ;;
    Linux/x86_64) OS=linux ASSET=ghimby-linux-x86_64 ;;
    Linux/aarch64 | Linux/arm64) OS=linux ASSET=ghimby-linux-aarch64 ;;
    *)
        echo "ghimby has no build for $(uname -s) $(uname -m); it supports Apple silicon Macs and x86_64 or aarch64 Linux" >&2
        exit 1
        ;;
esac
command -v gh >/dev/null || echo "warning: gh not found; ghimby gets its GitHub token from 'gh auth token'" >&2

if [ $OS = mac ]; then
    LABEL=io.ghimby
    DATA="$HOME/Library/Application Support/ghimby"
    LOG=$HOME/Library/Logs/ghimby.log
    SERVICE=$HOME/Library/LaunchAgents/$LABEL.plist
else
    DATA=${XDG_DATA_HOME:-$HOME/.local/share}/ghimby
    SERVICE=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ghimby.service
fi
FIRST=$([ -e "$SERVICE" ] || echo 1)

mkdir -p "$(dirname "$BIN")" "$DATA" "$(dirname "$SERVICE")"
chmod 700 "$DATA"
if [ $# -gt 0 ]; then
    cp "$1" "$BIN.new"
else
    if [ -n "${GHIMBY_VERSION:-}" ]; then
        DL=https://github.com/$REPO/releases/download/$GHIMBY_VERSION/$ASSET
    else
        DL=https://github.com/$REPO/releases/latest/download/$ASSET
    fi
    echo "downloading $DL"
    curl -fsSL "$DL" -o "$BIN.new"
    chmod +x "$BIN.new"
fi
# Rename into place: overwriting a running binary in place can get it killed.
mv "$BIN.new" "$BIN"

if [ $OS = mac ]; then
    cat > "$SERVICE.new" <<EOF
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
    if cmp -s "$SERVICE.new" "$SERVICE" && launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
        rm "$SERVICE.new"
        launchctl kickstart -k "$DOMAIN/$LABEL"
    else
        mv "$SERVICE.new" "$SERVICE"
        launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
        launchctl bootstrap "$DOMAIN" "$SERVICE"
    fi
    echo "ghimby running at $URL (log: $LOG)"
else
    # The user manager's PATH is minimal, so keep the installer's, which finds gh and git.
    cat > "$SERVICE" <<EOF
[Unit]
Description=ghimby: rule-filtered GitHub notifications

[Service]
ExecStart="$BIN" --db "$DATA/ghimby.db" --listen 127.0.0.1:$PORT
WorkingDirectory=$DATA
Environment="PATH=$PATH"
Restart=always

[Install]
WantedBy=default.target
EOF
    systemctl --user daemon-reload
    systemctl --user enable ghimby >/dev/null 2>&1
    systemctl --user restart ghimby
    echo "ghimby running at $URL (log: journalctl --user -u ghimby)"
fi

# Open the browser on first install only, once the server answers (up to 10s).
if [ -n "$FIRST" ]; then
    for _ in $(seq 50); do
        curl -sf -o /dev/null "$URL/favicon.svg" && break
        sleep 0.2
    done
    if [ $OS = mac ]; then
        open "$URL"
    elif command -v xdg-open >/dev/null && [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
        xdg-open "$URL" >/dev/null 2>&1 || true
    fi
fi
