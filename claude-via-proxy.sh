#!/usr/bin/env bash
set -euo pipefail

PROXY_URL="${PROXY_URL:-http://localhost:9876}"

if ! curl -fsS -o /dev/null --max-time 2 "$PROXY_URL/v1/models"; then
  echo "error: proxy not reachable at $PROXY_URL" >&2
  echo "  start it with: cargo run -- server" >&2
  exit 1
fi

unset ANTHROPIC_API_KEY

export ANTHROPIC_BASE_URL="$PROXY_URL"
export ANTHROPIC_AUTH_TOKEN="${ANTHROPIC_AUTH_TOKEN:-copilot-proxy-local}"
export ANTHROPIC_DEFAULT_OPUS_MODEL="claude-opus-4.7-1m-internal"
export ANTHROPIC_DEFAULT_SONNET_MODEL="claude-sonnet-4.6"
export ANTHROPIC_DEFAULT_HAIKU_MODEL="claude-sonnet-4.6"

exec claude "$@"
