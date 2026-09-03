#!/usr/bin/env bash
# Publish betamacs artifacts into otactl's firmware pipeline, from where
# the hausmeister BetamacsPlugin delivers them to every entitled Mac
# (docs/managed-mode.md).
#
#   scripts/publish.sh app                        # dist/betamacs.app -> app=betamacs
#   scripts/publish.sh config <version> [file]    # package.json -> app=betamacs-config
#
# Needs the otactl CLI on PATH (or OTACTL_BIN) and the publisher
# environment it documents: OTACTL_BACKEND_URL, OTACTL_PUBLISHER_ID
# (plus OTACTL_TOKEN for first-time publisher enrollment).
# BETAMACS_CHANNEL overrides the channel (default stable).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OTACTL="${OTACTL_BIN:-otactl}"
CHANNEL="${BETAMACS_CHANNEL:-stable}"
ARCH=arm64
BUILD_DTM="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
GIT_HASH="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"

case "${1:-}" in
  app)
    APP_BUNDLE="$ROOT/dist/betamacs.app"
    [ -d "$APP_BUNDLE" ] || { echo "build first: scripts/make-app.sh" >&2; exit 1; }
    [ -f "$APP_BUNDLE/Contents/Resources/otactl-root.pem" ] || {
      echo "refusing to publish an UNMANAGED build (no pinned otactl root)" >&2; exit 1; }
    VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -1)"
    ZIP="$ROOT/dist/betamacs-$VERSION.zip"
    # --keepParent: the archive must hold betamacs.app itself, not its
    # contents — betamacsd looks for the .app at the archive root.
    ditto -c -k --keepParent "$APP_BUNDLE" "$ZIP"
    echo "publishing app=betamacs $VERSION ($CHANNEL) from $ZIP"
    "$OTACTL" boot-usb upload \
      --artifact "$ZIP" --app betamacs --arch "$ARCH" --channel "$CHANNEL" \
      --version "$VERSION" --role runtime --format macos-app-zip \
      --build-dtm "$BUILD_DTM" --git-hash "$GIT_HASH"
    ;;
  config)
    VERSION="${2:-}"
    [ -n "$VERSION" ] || { echo "usage: publish.sh config <version> [package.json]" >&2; exit 1; }
    FILE="${3:-$ROOT/config/package.json}"
    [ -f "$FILE" ] || { echo "no package file at $FILE" >&2; exit 1; }
    # Validate before signing anything: the file must parse as JSON.
    python3 -c "import json,sys; json.load(open(sys.argv[1]))" "$FILE"
    echo "publishing app=betamacs-config $VERSION ($CHANNEL) from $FILE"
    "$OTACTL" boot-usb upload \
      --artifact "$FILE" --app betamacs-config --arch "$ARCH" --channel "$CHANNEL" \
      --version "$VERSION" --role runtime --format betamacs-package-json \
      --build-dtm "$BUILD_DTM" --git-hash "$GIT_HASH"
    ;;
  *)
    echo "usage: publish.sh app | publish.sh config <version> [file]" >&2
    exit 1
    ;;
esac
