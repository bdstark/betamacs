#!/usr/bin/env bash
# Package betamacs as a signed .app bundle so it can run as a login item.
# The binary detects the bundle at runtime and switches to
# ~/Library/Application Support/betamacs for config/ and logging, with
# models/ and the webapp served from Contents/Resources (see src/main.rs).
#
# Usage: scripts/make-app.sh [output-dir]   (default: dist/)
# Signing identity override: BETAMACS_SIGN_IDENTITY (default picks the
# first "Developer ID Application" identity, falling back to ad-hoc).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/dist}"
APP="$OUT/betamacs.app"
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -1)

BIN="$ROOT/target/release/betamacs"
DAEMON="$ROOT/target/release/betamacsd"
[ -x "$BIN" ] || { echo "binary missing — run: cargo build --release" >&2; exit 1; }
[ -x "$DAEMON" ] || { echo "betamacsd missing — run: cargo build --release" >&2; exit 1; }
[ -f "$ROOT/models/320n.onnx" ] || { echo "models missing — run: scripts/fetch-model.sh 320n" >&2; exit 1; }
[ -f "$ROOT/webapp/dist/index.html" ] || { echo "webapp not built — run: cd webapp && npm install && npm run build" >&2; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/models" "$APP/Contents/Resources/webapp" \
  "$APP/Contents/Library/LaunchDaemons"

# SMAppService daemon: `betamacs install-daemon` registers this plist so
# the privileged bootstrap is one System Settings approval, no sudo.
cat > "$APP/Contents/Library/LaunchDaemons/com.bdstark.betamacsd.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.bdstark.betamacsd</string>
	<key>BundleProgram</key>
	<string>Contents/MacOS/betamacsd</string>
	<key>AssociatedBundleIdentifiers</key>
	<array>
		<string>com.bdstark.betamacs</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
</dict>
</plist>
EOF
cp "$BIN" "$APP/Contents/MacOS/betamacs"
cp "$DAEMON" "$APP/Contents/MacOS/betamacsd"
cp "$ROOT"/models/*.onnx "$APP/Contents/Resources/models/"
cp -R "$ROOT/webapp/dist/." "$APP/Contents/Resources/webapp/"

# A pinned otactl root turns the build into a MANAGED build: settings
# are then accepted only as fleet-signed envelopes (docs/managed-mode.md).
PIN="${BETAMACS_OTACTL_ROOT:-$ROOT/otactl-root.pem}"
if [ -f "$PIN" ]; then
  cp "$PIN" "$APP/Contents/Resources/otactl-root.pem"
  echo "managed build: pinned otactl root from $PIN"
fi
# An author public key beside it makes config author-signature-required:
# only the policy author's key (which otactl never holds, and which
# typeserver can time-lock) can change policy on such installs.
if [ -f "$ROOT/author-pubkey.pem" ]; then
  cp "$ROOT/author-pubkey.pem" "$APP/Contents/Resources/author-pubkey.pem"
  echo "managed build: author-signed config REQUIRED (author-pubkey.pem)"
fi

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>com.bdstark.betamacs</string>
	<key>CFBundleName</key>
	<string>betamacs</string>
	<key>CFBundleExecutable</key>
	<string>betamacs</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>${VERSION}</string>
	<key>CFBundleVersion</key>
	<string>${VERSION}</string>
	<key>LSMinimumSystemVersion</key>
	<string>13.0</string>
	<key>LSUIElement</key>
	<true/>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
EOF

if [ -z "${BETAMACS_SIGN_IDENTITY:-}" ]; then
  if security find-identity -v -p codesigning | grep -q "Developer ID Application"; then
    BETAMACS_SIGN_IDENTITY="Developer ID Application"
  else
    BETAMACS_SIGN_IDENTITY="-"  # ad-hoc; TCC grants won't survive rebuilds
  fi
fi
# Nested executables must be signed before the bundle seal.
codesign --force --sign "$BETAMACS_SIGN_IDENTITY" \
  --identifier com.bdstark.betamacsd "$APP/Contents/MacOS/betamacsd"
codesign --force --sign "$BETAMACS_SIGN_IDENTITY" \
  --identifier com.bdstark.betamacs "$APP"
codesign --verify --strict "$APP"
echo "built $APP (version $VERSION, signed: $BETAMACS_SIGN_IDENTITY)"
