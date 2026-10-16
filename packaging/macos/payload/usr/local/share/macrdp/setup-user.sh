#!/bin/bash
# Sets up macrdp-server for one user: config, log folder, login agent.
# Run as root: setup-user.sh <username>
# Keeps an existing config; a new config gets a random password.
set -euo pipefail

USER_NAME="${1:?usage: setup-user.sh <username>}"
LABEL="com.macrdp.server"
BIN="/Applications/macrdp.app/Contents/MacOS/macrdp-server"
SHARE="/usr/local/share/macrdp"

USER_ID=$(id -u "$USER_NAME")
USER_GROUP=$(id -gn "$USER_NAME")
USER_HOME=$(dscl . -read "/Users/$USER_NAME" NFSHomeDirectory | awk '{print $2}')

CONFIG_DIR="$USER_HOME/.config/macrdp"
CONFIG="$CONFIG_DIR/config.toml"
LOG_DIR="$USER_HOME/Library/Logs/macrdp"
AGENT_DIR="$USER_HOME/Library/LaunchAgents"
PLIST="$AGENT_DIR/$LABEL.plist"

install -d -o "$USER_NAME" -g "$USER_GROUP" -m 755 "$USER_HOME/.config"
install -d -o "$USER_NAME" -g "$USER_GROUP" -m 700 "$CONFIG_DIR" "$LOG_DIR"
install -d -o "$USER_NAME" -g "$USER_GROUP" -m 755 "$AGENT_DIR"

NEW_PASSWORD=""
if [ ! -f "$CONFIG" ]; then
    NEW_PASSWORD=$(LC_ALL=C tr -dc 'A-HJ-NP-Za-km-z2-9' </dev/urandom | head -c 16)
    sed -e "s/__USERNAME__/$USER_NAME/" -e "s/__PASSWORD__/$NEW_PASSWORD/" \
        "$SHARE/config.template.toml" > "$CONFIG"
    chown "$USER_NAME:$USER_GROUP" "$CONFIG"
    chmod 600 "$CONFIG"
fi

cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$LABEL</string>
	<key>ProgramArguments</key>
	<array>
		<string>$BIN</string>
	</array>
	<key>WorkingDirectory</key>
	<string>$LOG_DIR</string>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ThrottleInterval</key>
	<integer>10</integer>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardOutPath</key>
	<string>$LOG_DIR/stdout.log</string>
	<key>StandardErrorPath</key>
	<string>$LOG_DIR/stderr.log</string>
</dict>
</plist>
EOF
chown "$USER_NAME:$USER_GROUP" "$PLIST"
chmod 644 "$PLIST"

# Start it now if the user is logged in; otherwise it starts at their next login.
if launchctl print "gui/$USER_ID" >/dev/null 2>&1; then
    launchctl bootout "gui/$USER_ID/$LABEL" 2>/dev/null || true
    launchctl bootstrap "gui/$USER_ID" "$PLIST"
fi

if [ -n "$NEW_PASSWORD" ]; then
    echo "macrdp: created $CONFIG for $USER_NAME (password is in that file)"
fi
# Hand the new password to the caller (postinstall shows it in a dialog).
(umask 077; printf '%s' "$NEW_PASSWORD" > "/var/tmp/macrdp-newpw.$USER_ID") 2>/dev/null || true
