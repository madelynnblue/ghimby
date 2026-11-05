#!/bin/sh
# Builds a release binary and (re)starts it as a launchd agent. Safe to run repeatedly.
set -eu

LABEL=io.ghimby
PORT=${GHIMBY_PORT:-7880}
BIN=$HOME/.local/bin/ghimby
DATA="$HOME/Library/Application Support/ghimby"
LOG=$HOME/Library/Logs/ghimby.log
PLIST=$HOME/Library/LaunchAgents/$LABEL.plist
REPO=$(cd "$(dirname "$0")/.." && pwd)

export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release --manifest-path "$REPO/Cargo.toml"

mkdir -p "$(dirname "$BIN")" "$DATA" "$(dirname "$PLIST")"
# Rename into place: overwriting a running binary in place can get it killed on macOS.
cp "$REPO/target/release/ghimby" "$BIN.new"
mv "$BIN.new" "$BIN"

# First install starts from the dev database so rules and ignores carry over.
if [ ! -e "$DATA/ghimby.db" ] && [ -e "$REPO/ghimby.db" ]; then
    sqlite3 "$REPO/ghimby.db" ".backup '$DATA/ghimby.db'"
fi

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
