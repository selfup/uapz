#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

for tool in syft grype; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: $tool not found on PATH" >&2

    exit 1
  fi
done

grype db update

sbom="uapz.sbom.syft.json"

syft scan dir:. --exclude './target' --source-name uapz --source-version 0.1.0 -o syft-json="$sbom"

grype "sbom:$sbom" -o table "$@"

# grype only sees advisories that reach the GitHub Advisory feed. cargo audit
# adds RustSec's unmaintained, unsound and yanked notices, which grype never shows.
if cargo audit --version >/dev/null 2>&1; then
  cargo audit
else
  echo "note: cargo-audit not installed, skipping RustSec check (cargo install cargo-audit)" >&2
fi
