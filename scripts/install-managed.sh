#!/usr/bin/env bash
# Managed (tamper-resistant) install — run with sudo. Lays down the
# root-owned layout from docs/managed-mode.md:
#
#   /Applications/betamacs.app            root:wheel, incl. betamacsd
#   /Library/LaunchAgents/…betamacs.plist root-owned per-user agent
#   /Library/LaunchDaemons/…betamacsd.plist  root watchdog daemon
#   /Library/Application Support/betamacs    managed config dir
#
# It also removes a previous per-user (self-installed) LaunchAgent for
# the invoking user so the app isn't loaded twice.
#
# Usage: sudo scripts/install-managed.sh [path/to/betamacs.app]
set -euo pipefail

[ "$(id -u)" -eq 0 ] || { echo "run with sudo" >&2; exit 1; }
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_SRC="${1:-$ROOT/dist/betamacs.app}"
[ -d "$APP_SRC" ] || { echo "app bundle not found: $APP_SRC (run scripts/make-app.sh)" >&2; exit 1; }
[ -x "$APP_SRC/Contents/MacOS/betamacsd" ] || { echo "bundle has no betamacsd — rebuild with current make-app.sh" >&2; exit 1; }

APP=/Applications/betamacs.app
MANAGED="/Library/Application Support/betamacs"
AGENT_PLIST=/Library/LaunchAgents/com.bdstark.betamacs.plist
DAEMON_PLIST=/Library/LaunchDaemons/com.bdstark.betamacsd.plist

# The user who invoked sudo — their per-user install gets cleaned up.
CALLER="${SUDO_USER:-}"

echo "installing app (root-owned)"
rm -rf "$APP"
ditto "$APP_SRC" "$APP"
chown -R root:wheel "$APP"
chmod -R go-w "$APP"

echo "creating managed config dir"
mkdir -p "$MANAGED"
chown root:wheel "$MANAGED"
chmod 755 "$MANAGED"

echo "writing launchd plists"
cat > "$AGENT_PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.bdstark.betamacs</string>
	<key>ProgramArguments</key>
	<array>
		<string>$APP/Contents/MacOS/betamacs</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
		<key>BETAMACS_LAUNCHD</key>
		<string>1</string>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
</dict>
</plist>
EOF
cat > "$DAEMON_PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.bdstark.betamacsd</string>
	<key>ProgramArguments</key>
	<array>
		<string>$APP/Contents/MacOS/betamacsd</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
</dict>
</plist>
EOF
chown root:wheel "$AGENT_PLIST" "$DAEMON_PLIST"
chmod 644 "$AGENT_PLIST" "$DAEMON_PLIST"

if [ -n "$CALLER" ]; then
  USER_PLIST="$(eval echo ~"$CALLER")/Library/LaunchAgents/com.bdstark.betamacs.plist"
  if [ -f "$USER_PLIST" ]; then
    echo "removing $CALLER's per-user agent (superseded by global agent)"
    CALLER_UID="$(id -u "$CALLER")"
    launchctl bootout "gui/$CALLER_UID/com.bdstark.betamacs" 2>/dev/null || true
    rm -f "$USER_PLIST"
  fi
fi

echo "starting daemon"
launchctl bootout system/com.bdstark.betamacsd 2>/dev/null || true
launchctl bootstrap system "$DAEMON_PLIST"

if [ -n "$CALLER" ]; then
  echo "starting agent for $CALLER"
  CALLER_UID="$(id -u "$CALLER")"
  launchctl bootstrap "gui/$CALLER_UID" "$AGENT_PLIST" 2>/dev/null || \
    echo "  (agent will start at next login)"
fi

echo "done — see docs/managed-mode.md for the enforcement model"
