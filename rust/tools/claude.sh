#!/usr/bin/env bash
# Run one bounded Claude helper task. Usage: rust/tools/claude.sh <prompt-file>
# The caller owns integration and validation; the prompt assigns output files.
set -euo pipefail

if [[ $# -ne 1 || ! -f "$1" || ! -r "$1" || ! -s "$1" ]]; then
  echo "Usage: $0 <nonempty-readable-prompt-file>" >&2
  exit 2
fi

# Open before changing directory so relative prompt paths work from any cwd.
exec 3< "$1"
REPO=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$REPO"

# Resolve mise's installed binary directly. The desktop's claude launcher runs
# `mise use -g` on every call, which changes global config and pollutes stdout.
if command -v mise >/dev/null && CLAUDE_BIN=$(mise which claude 2>/dev/null); then
  :
else
  CLAUDE_BIN=$(command -v claude)
fi

# Keep auth in the installed CLI. JSON includes result/error and model usage.
# Never fall back to another model silently. No interactive permission prompts.
exec timeout --kill-after=10s 15m \
  "$CLAUDE_BIN" --print --model claude-opus-4-8 --effort medium \
    --output-format json --dangerously-skip-permissions <&3
