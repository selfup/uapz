#!/usr/bin/env bash
set -euo pipefail

profile=release
case "${1:-}" in
  --debug) profile=dev; shift ;;
  --release) shift ;;
  -h|--help)
    cat <<'USAGE'
Usage: ./scripts/run-app.sh [--debug|--release] [app arguments]

Build the frontend and native app, then run with embedded assets.
Defaults to a release build and the repository workspace.
Use --debug for a faster build, or --workspace PATH to open another workspace.
UAPZ_WORKSPACE_DIR also selects a workspace; --workspace takes precedence.
USAGE
    exit 0
    ;;
esac

for tool in npm cargo; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: $tool not found on PATH" >&2
    exit 1
  fi
done

app_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$app_root"
export UAPZ_WORKSPACE_DIR="${UAPZ_WORKSPACE_DIR:-$app_root}"

if [[ ! -d node_modules ]]; then
  npm ci
fi

cargo fmt --all --check
npm run build
cargo check --locked -p uapz-desktop --profile "$profile" --features custom-protocol
exec cargo run --locked -p uapz-desktop --profile "$profile" --features custom-protocol -- "$@"
