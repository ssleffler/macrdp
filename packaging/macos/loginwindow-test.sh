#!/bin/bash
# Feasibility test: can macrdp serve the macOS login window (nobody logged in)?
# Runs the installed macrdp-server at the login screen on two extra ports,
# next to the normal per-user service (port 3389), which it doesn't touch:
#   3390  a display at the client's size (macrdp's default)
#   3391  the Mac's own display (match_client_resolution = false)
#
#   sudo bash loginwindow-test.sh install [username]   then log out and connect
#   sudo bash loginwindow-test.sh report               after logging back in
#   sudo bash loginwindow-test.sh uninstall
#
# Needs macrdp installed and working for <username> (both permissions allowed),
# FileVault off, and another way into the Mac while it sits at the login screen.
set -euo pipefail

PREFIX="${MACRDP_TEST_PREFIX:-}" # dry runs only: write under this folder, no root needed
BIN="/Applications/macrdp.app/Contents/MacOS/macrdp-server"
DIR="$PREFIX/Library/Application Support/macrdp-loginwindow-test"
AGENTS="$PREFIX/Library/LaunchAgents"
LABEL="com.macrdp.loginwindow-test"
VARIANTS="3390 3391"

[ -n "$PREFIX" ] || [ "$(id -u)" = 0 ] || { echo "Run with sudo." >&2; exit 1; }

case "${1:-}" in
install)
    user="${2:-${SUDO_USER:-}}"
    [ -n "$user" ] || { echo "usage: loginwindow-test.sh install <username>" >&2; exit 1; }
    home=$(dscl . -read "/Users/$user" NFSHomeDirectory | awk '{print $2}')
    config="$home/.config/macrdp/config.toml"
    [ -x "$BIN" ] || { echo "macrdp isn't installed ($BIN)." >&2; exit 1; }
    [ -f "$config" ] || { echo "No macrdp config for $user ($config)." >&2; exit 1; }
    fdesetup status | grep -q "FileVault is On" && echo "Warning: FileVault is on. Nothing runs before the disk is unlocked at boot; log out instead of restarting for this test."

    mkdir -p "$AGENTS"
    for port in $VARIANTS; do
        dir="$DIR/$port"
        mkdir -p "$dir"
        # The user's config (same RDP login), on this variant's port
        (umask 077; awk -v port="$port" '
            /^port *=/ { print "port = " port
                         if (port == 3391) print "match_client_resolution = false"
                         done = 1; next }
            /^match_client_resolution *=/ { next }
            { print }
            END { if (!done) exit 1 }' "$config" > "$dir/config.toml") \
            || { echo "No port line in $config." >&2; exit 1; }
        cat > "$AGENTS/$LABEL-$port.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$LABEL-$port</string>
	<key>LimitLoadToSessionType</key>
	<string>LoginWindow</string>
	<key>ProgramArguments</key>
	<array>
		<string>$BIN</string>
		<string>--config</string>
		<string>$dir/config.toml</string>
	</array>
	<key>WorkingDirectory</key>
	<string>$dir</string>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ThrottleInterval</key>
	<integer>10</integer>
	<key>StandardOutPath</key>
	<string>$dir/stdout.log</string>
	<key>StandardErrorPath</key>
	<string>$dir/stderr.log</string>
</dict>
</plist>
EOF
        chmod 644 "$AGENTS/$LABEL-$port.plist"
    done
    cat <<EOF
Installed. Now:
  1. Log out (Apple menu > Log Out), or restart if automatic login is off.
  2. From another computer, connect RDP to this Mac on port 3390, then 3391
     (same username and password as usual). Note for each: connects? picture
     of the login screen? can you click a user and type the Mac password?
  3. Log back in and run:  sudo bash $0 report
EOF
    ;;
report)
    echo "macOS $(sw_vers -productVersion), $(uname -m), macrdp $(defaults read /Applications/macrdp.app/Contents/Info.plist CFBundleShortVersionString 2>/dev/null)"
    echo "Still running after login: $(pgrep -f "$DIR" | wc -l | tr -d ' ') process(es)"
    for port in $VARIANTS; do
        echo "===== port $port ====="
        log="$DIR/$port/macrdp.log"
        [ -f "$log" ] || { echo "no log: macrdp never started at the login window"; tail -n 5 "$DIR/$port/stderr.log" 2>/dev/null; continue; }
        echo "log lines: $(grep -c '' "$log"), starts: $(grep -c 'RDP server listening' "$log")"
        grep -E "permission|preflight|listening|Virtual display|Screen capture|SCStream|encoder created|negotiated|Client gone|WARN|ERROR" "$log" \
            | grep -v "Illegal SNI" | cut -c 1-220 | tail -n 40
        echo "--- stderr"
        tail -n 8 "$DIR/$port/stderr.log" 2>/dev/null | cut -c 1-220
    done
    ;;
uninstall)
    rm -f "$AGENTS/$LABEL"-*.plist
    pkill -f "$DIR" 2>/dev/null || true
    rm -rf "$DIR"
    echo "Removed. (Agents already loaded at the login window stop at the next logout or restart.)"
    ;;
*)
    sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
