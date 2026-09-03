#!/usr/bin/env bash
# Policy-author key tooling (docs/managed-mode.md, integral timed locks).
#
# The author key is the SECOND signature on config: betamacsd accepts a
# config only when it carries a valid author signature, and otactl never
# holds this key — so locking the private key (typeserver timed secret)
# cryptographically closes the config channel until the lock expires.
#
#   scripts/author-key.sh generate           # -> author-key.pem (PRIVATE,
#                                            #    gitignored) + author-pubkey.pem
#   scripts/author-key.sh sign <package.json> [ttl-seconds]
#                                            # -> <package.json>.authored
#                                            #    (the wrapper publish.sh uploads)
#
# Custody: after generating, store author-key.pem's contents in
# typeserver as a pasted secret. To lock config changes until T, use
# typeserver's LOCK_SECRET on it and delete/shred the local file; the
# drand timelock makes early recovery impossible for everyone.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KEY="${BETAMACS_AUTHOR_KEY:-$ROOT/author-key.pem}"
PUB="$ROOT/author-pubkey.pem"

case "${1:-}" in
  generate)
    [ -f "$KEY" ] && { echo "refusing to overwrite existing $KEY" >&2; exit 1; }
    openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out "$KEY"
    chmod 600 "$KEY"
    openssl pkey -in "$KEY" -pubout -out "$PUB"
    echo "private: $KEY   (store in typeserver, then shred locally to lock)"
    echo "public:  $PUB   (committed; baked into managed builds by make-app.sh)"
    ;;
  sign)
    FILE="${2:-}"
    [ -f "$FILE" ] || { echo "usage: author-key.sh sign <package.json> [ttl-seconds]" >&2; exit 1; }
    [ -f "$KEY" ] || { echo "author key not found at $KEY (BETAMACS_AUTHOR_KEY overrides)" >&2; exit 1; }
    TTL="${3:-3600}"
    python3 - "$FILE" "$KEY" "$TTL" <<'EOF'
import base64, json, subprocess, sys, tempfile, time
path, key, ttl = sys.argv[1], sys.argv[2], int(sys.argv[3])
package = open(path, "rb").read()
json.loads(package)  # must be valid JSON before it gets signed
fmt = "%Y-%m-%dT%H:%M:%SZ"
authored_at = time.strftime(fmt, time.gmtime())
not_after = time.strftime(fmt, time.gmtime(time.time() + ttl))
package_b64 = base64.b64encode(package).decode()
signing_input = f"betamacs-config-author-v1\n{authored_at}\n{not_after}\n{package_b64}"
with tempfile.NamedTemporaryFile() as msg, tempfile.NamedTemporaryFile() as sig:
    msg.write(signing_input.encode()); msg.flush()
    subprocess.run(["openssl", "dgst", "-sha256", "-sign", key,
                    "-out", sig.name, msg.name], check=True)
    signature = base64.b64encode(open(sig.name, "rb").read()).decode()
out = path + ".authored"
json.dump({"packageB64": package_b64, "authoredAt": authored_at,
           "notAfter": not_after, "authorSignature": signature},
          open(out, "w"))
print(f"authored wrapper: {out} (valid until {not_after})")
EOF
    ;;
  *)
    echo "usage: author-key.sh generate | author-key.sh sign <package.json> [ttl-seconds]" >&2
    exit 1
    ;;
esac
