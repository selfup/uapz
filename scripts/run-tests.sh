#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

cargo test

# The live tests are ignored by default: they need a model server and a PostgreSQL
# server (see README.md). Override these from the environment.
export UAPZ_LIVE_MODEL="${UAPZ_LIVE_MODEL:-mistralai/ministral-3-3b}"
export UAPZ_LIVE_EMBED_MODEL="${UAPZ_LIVE_EMBED_MODEL:-text-embedding-nomic-embed-text-v1.5}"
export UAPZ_LIVE_PG_URL="${UAPZ_LIVE_PG_URL:-postgres://localhost/postgres}"

cargo test live_ -- --ignored
