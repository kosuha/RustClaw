#!/usr/bin/env bash
set -euo pipefail

mkdir -p /home/node/.codex
if [ -d /workspace/codex-auth ]; then
  cp -R /workspace/codex-auth/. /home/node/.codex/ 2>/dev/null || true
fi

cat > /tmp/input.json
node /app/dist/index.js < /tmp/input.json
