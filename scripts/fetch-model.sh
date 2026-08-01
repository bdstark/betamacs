#!/usr/bin/env bash
# Download the NudeNet v3 detector ONNX models.
#
# Note: plain browser_download_url links on notAI-tech/NudeNet redirect to a
# GitHub sign-in page (the repo's downloads are login-gated), so this uses
# the GitHub API asset endpoint with an octet-stream Accept header, which
# still serves the file anonymously.
#
# Usage: scripts/fetch-model.sh [320n|640m]   (default: 320n)
set -euo pipefail

MODEL="${1:-320n}"
case "$MODEL" in
  320n|640m) ;;
  *) echo "unknown model '$MODEL' (expected 320n or 640m)" >&2; exit 1 ;;
esac

DIR="$(cd "$(dirname "$0")/.." && pwd)/models"
mkdir -p "$DIR"

ASSET_ID=$(curl -s "https://api.github.com/repos/notAI-tech/NudeNet/releases/tags/v3.4-weights" |
  python3 -c "import json,sys; r=json.load(sys.stdin); print(next(a['id'] for a in r['assets'] if a['name']=='${MODEL}.onnx'))")

echo "Downloading ${MODEL}.onnx (asset $ASSET_ID) -> $DIR/$MODEL.onnx"
curl -L --fail -H "Accept: application/octet-stream" \
  -o "$DIR/$MODEL.onnx" \
  "https://api.github.com/repos/notAI-tech/NudeNet/releases/assets/$ASSET_ID"

# Sanity check: a login page is ~47KB of HTML; real models are 12MB/99MB.
SIZE=$(stat -f%z "$DIR/$MODEL.onnx")
if [ "$SIZE" -lt 1000000 ]; then
  echo "Downloaded file is suspiciously small ($SIZE bytes) — likely not the model." >&2
  echo "Fallback for 320n: it is bundled in the 'nudenet' PyPI wheel (nudenet/320n.onnx)." >&2
  exit 1
fi
echo "Done ($SIZE bytes)."
