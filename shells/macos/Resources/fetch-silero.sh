#!/usr/bin/env bash
#
# Re-download the Silero v5 VAD ONNX model into this directory.
#
# We pin to a specific release tag rather than `main` so the bundled
# weights match the framing + thresholds the Rust core was tested
# against. Bump SILERO_TAG and the expected hash in lockstep when
# upgrading. (`silero_vad.onnx` lives in `shells/macos/Resources/`
# and is copied into the .app bundle by XcodeGen — see project.yml.)

set -euo pipefail

SILERO_TAG="v5.1.2"
EXPECTED_SHA256="2623a2953f6ff3d2c1e61740c6cdb7168133479b267dfef114a4a3cc5bdd788f"
URL="https://raw.githubusercontent.com/snakers4/silero-vad/${SILERO_TAG}/src/silero_vad/data/silero_vad.onnx"

cd "$(dirname "$0")"

echo "fetching silero_vad.onnx from ${SILERO_TAG}…"
curl -fSL --max-time 60 -o silero_vad.onnx "$URL"

ACTUAL_SHA256=$(shasum -a 256 silero_vad.onnx | awk '{print $1}')
echo "sha256: ${ACTUAL_SHA256}"
if [ "$ACTUAL_SHA256" != "$EXPECTED_SHA256" ]; then
  echo "ERROR: sha256 mismatch — expected ${EXPECTED_SHA256}" >&2
  echo "  upstream retagged ${SILERO_TAG} or the URL changed; investigate." >&2
  exit 1
fi
echo "ok"
