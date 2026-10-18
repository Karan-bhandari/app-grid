#!/usr/bin/env bash
# Antigravity PostToolUse hook for auto-formatting edited files.
# Reads JSON payload from stdin, formats the codebase silently, and outputs `{}` to stdout.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT" || exit 0

# Drain stdin per Antigravity hook contract
cat >/dev/null || true

# Run multi-language formatter, sending all logs to stderr so stdout remains pure JSON `{}`
"$REPO_ROOT/scripts/format.sh" >/dev/null 2>&1 || true

printf '{}\n'
