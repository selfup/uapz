#!/usr/bin/env bash
set -euo pipefail

# Install project dependencies. Assumes Node.js 24, npm, and stable Rust are already
# on PATH (same prerequisites as .github/workflows/ci.yml).

cd "$(dirname "$0")/.."

for tool in npm npx cargo; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: $tool not found on PATH" >&2
    exit 1
  fi
done

npm ci
npx playwright install chromium
cargo fetch --locked
