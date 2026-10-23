#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

for tool in syft grype; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: $tool not found on PATH" >&2
    exit 1
  fi
done

sbom="sbom.syft.json"

syft scan dir:. --exclude './target' -o syft-json="$sbom"
grype "sbom:$sbom" -o table "$@"
