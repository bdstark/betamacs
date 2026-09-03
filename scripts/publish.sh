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
    # Validate before signing anything: the file must parse as JSON, and
    # must be a raw package — never an already-authored wrapper. Passing a
    # *.authored file here would double-wrap it (author-key.sh signs the
    # bytes it is given), producing a wrapper-inside-a-wrapper the fleet
    # unwraps one layer at a time and then rejects. Feed the raw
    # package.json; the signing happens here, exactly once.
    python3 - "$FILE" <<'PYEOF'
import json, sys
d = json.load(open(sys.argv[1]))
if isinstance(d, dict) and "authorSignature" in d and "packageB64" in d:
    sys.exit("refusing to publish an already-authored wrapper (%s); pass the raw package.json" % sys.argv[1])
PYEOF
    # With author signing available — remote (typeserver oracle, when
    # BETAMACS_AUTHOR_SECRET is set) or a local key — upload the
    # author-signed wrapper; a pinned fleet refuses anything else.
    if [ -n "${BETAMACS_AUTHOR_SECRET:-}" ] || [ -f "${BETAMACS_AUTHOR_KEY:-$ROOT/author-key.pem}" ]; then
      "$ROOT/scripts/author-key.sh" sign "$FILE" "${BETAMACS_AUTHOR_TTL:-3600}"
      FILE="$FILE.authored"
    elif [ -f "$ROOT/author-pubkey.pem" ]; then
      echo "WARNING: author-pubkey.pem exists but no author key found —" >&2
      echo "  pinned installs will REFUSE this unsigned config." >&2
      echo "  (BETAMACS_AUTHOR_KEY points at the key; typeserver may hold it.)" >&2
    fi
    echo "publishing app=betamacs-config $VERSION ($CHANNEL) from $FILE"
    "$OTACTL" boot-usb upload \
      --artifact "$FILE" --app betamacs-config --arch "$ARCH" --channel "$CHANNEL" \
      --version "$VERSION" --role runtime --format betamacs-package-json \
      --build-dtm "$BUILD_DTM" --git-hash "$GIT_HASH"
    ;;
  tasks)
    # The challenge task bank — a SEPARATE signed artifact from config, so
    # questions version and swap (per kid, or as kids age) independently of
    # policy. Same author-signing and anti-wrapper guard as config; only the
    # app name, default file, and format differ. Per-kid variants are just
    # different channels (BETAMACS_CHANNEL), resolved per device by otactl.
    VERSION="${2:-}"
    [ -n "$VERSION" ] || { echo "usage: publish.sh tasks <version> [tasks.json]" >&2; exit 1; }
    FILE="${3:-$ROOT/config/tasks.json}"
    [ -f "$FILE" ] || { echo "no task bank at $FILE" >&2; exit 1; }
    python3 - "$FILE" <<'PYEOF'
import json, sys
d = json.load(open(sys.argv[1]))
if isinstance(d, dict) and "authorSignature" in d and "packageB64" in d:
    sys.exit("refusing to publish an already-authored wrapper (%s); pass the raw tasks.json" % sys.argv[1])
if not (isinstance(d, dict) and isinstance(d.get("tasks"), list)):
    sys.exit("%s is not a task bank ({version, tasks:[...]})" % sys.argv[1])
PYEOF
    if [ -n "${BETAMACS_AUTHOR_SECRET:-}" ] || [ -f "${BETAMACS_AUTHOR_KEY:-$ROOT/author-key.pem}" ]; then
      "$ROOT/scripts/author-key.sh" sign "$FILE" "${BETAMACS_AUTHOR_TTL:-3600}"
      FILE="$FILE.authored"
    elif [ -f "$ROOT/author-pubkey.pem" ]; then
      echo "WARNING: author-pubkey.pem exists but no author key found —" >&2
      echo "  pinned installs will REFUSE this unsigned task bank." >&2
    fi
    echo "publishing app=betamacs-tasks $VERSION ($CHANNEL) from $FILE"
    "$OTACTL" boot-usb upload \
      --artifact "$FILE" --app betamacs-tasks --arch "$ARCH" --channel "$CHANNEL" \
      --version "$VERSION" --role runtime --format betamacs-taskbank-json \
      --build-dtm "$BUILD_DTM" --git-hash "$GIT_HASH"
    ;;
  *)
    echo "usage: publish.sh app | publish.sh config <version> [file] | publish.sh tasks <version> [file]" >&2
    exit 1
    ;;
esac
