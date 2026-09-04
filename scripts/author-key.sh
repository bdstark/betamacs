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
# Preferred custody: create the key INSIDE typeserver (Secrets UI →
# "Signing Key" type — the private half never exists outside the
# server), save the shown public key as author-pubkey.pem, and sign
# remotely by setting:
#   TYPESERVER_URL       e.g. https://typeserver.docker.newton.haus
#   TYPESERVER_SESSION   the ts_session cookie value of a signed-in browser
#   BETAMACS_AUTHOR_SECRET  the secret's name in typeserver
# Locking config changes is then just typeserver's LOCK_SECRET on that
# secret — the drand timelock refuses the signing operation until T.
# The local author-key.pem path below remains for offline/dev use.
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
    TTL="${3:-604800}"
    if [ -n "${BETAMACS_AUTHOR_SECRET:-}" ]; then
      : "${TYPESERVER_URL:?TYPESERVER_URL required for remote signing}"
      : "${TYPESERVER_SESSION:?TYPESERVER_SESSION (ts_session cookie) required for remote signing}"
    elif [ ! -f "$KEY" ]; then
      echo "no author key at $KEY and no BETAMACS_AUTHOR_SECRET set" >&2; exit 1
    fi
    python3 - "$FILE" "$TTL" "$KEY" <<'EOF'
import base64, json, os, subprocess, sys, tempfile, time, urllib.request
path, key = sys.argv[1], sys.argv[3]  # sys.argv[2] (ttl) is ignored: v2 has no expiry
package = open(path, "rb").read()
json.loads(package)  # must be valid JSON before it gets signed
fmt = "%Y-%m-%dT%H:%M:%SZ"
authored_at = time.strftime(fmt, time.gmtime())
package_b64 = base64.b64encode(package).decode()
# v2 wrapper: no notAfter. Authorship doesn't expire; anti-rollback is the
# epoch plus the daemon's authoredAt high-water. betamacs >= 0.2.19 verifies it.
signing_input = f"betamacs-config-author-v2\n{authored_at}\n{package_b64}"

secret = os.environ.get("BETAMACS_AUTHOR_SECRET")
if secret:
    # Remote: typeserver's signing oracle. The key never leaves the
    # server; a LOCK_SECRET on it refuses this call until the lock ends.
    req = urllib.request.Request(
        os.environ["TYPESERVER_URL"].rstrip("/") + "/api/secrets/sign",
        data=json.dumps({
            "name": secret,
            "payload_b64": base64.b64encode(signing_input.encode()).decode(),
            "passphrase": os.environ.get("BETAMACS_AUTHOR_PASSPHRASE", ""),
        }).encode(),
        headers={"Content-Type": "application/json",
                 "Cookie": "ts_session=" + os.environ["TYPESERVER_SESSION"]},
        method="POST")
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            signature = json.load(resp)["signature_b64"]
    except urllib.error.HTTPError as e:
        sys.exit(f"typeserver refused to sign: {e.read().decode().strip()}")
    print(f"signed remotely with typeserver secret {secret!r}")
else:
    with tempfile.NamedTemporaryFile() as msg, tempfile.NamedTemporaryFile() as sig:
        msg.write(signing_input.encode()); msg.flush()
        subprocess.run(["openssl", "dgst", "-sha256", "-sign", key,
                        "-out", sig.name, msg.name], check=True)
        signature = base64.b64encode(open(sig.name, "rb").read()).decode()

out = path + ".authored"
json.dump({"packageB64": package_b64, "authoredAt": authored_at,
           "authorSignature": signature},
          open(out, "w"))
print(f"authored wrapper: {out} (valid until {not_after})")
EOF
    ;;
  *)
    echo "usage: author-key.sh generate | author-key.sh sign <package.json> [ttl-seconds]" >&2
    exit 1
    ;;
esac
