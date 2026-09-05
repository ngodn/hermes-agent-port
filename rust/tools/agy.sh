#!/usr/bin/env bash
# Wrapper for the Antigravity CLI (agy) used as a headless mapping/analysis
# helper for the Rust port. Model is pinned; prompts must say where to write.
#
# Usage: rust/tools/agy.sh <prompt-file>
#
# SINGLE-FLIGHT AUTH GATE (do not remove).
# agy caches a Google login in ~/.gemini/oauth_creds.json. The access token
# expires roughly hourly, and Google ROTATES the refresh token on every refresh.
# So if two agy processes start at once against a cold token, both try to
# refresh, the first one wins, and the second is left holding a refresh token
# that was just invalidated. It then fails with "authentication required" and
# tries to pop a browser login, which in a headless session just times out.
# (Diagnosed in the dgnrt project; see its engine/src/ai/agy.rs AuthGate.)
#
# The fix is to serialize agy invocations behind an exclusive flock, so exactly
# one process ever performs the refresh. Concurrency here buys nothing anyway:
# these are long single-shot analysis runs.
set -euo pipefail

REPO=/home/eins0fx/development/hermes-agent-port
MODEL=gemini-3.8-flash-high
if [[ $# -ne 1 || ! -f "$1" || ! -r "$1" || ! -s "$1" ]]; then
  echo "Usage: $0 <nonempty-readable-prompt-file>" >&2
  exit 2
fi
# Resolve the prompt before changing directory. State the root in the prompt
# too, since the helper's filesystem tools may start outside the CLI cwd.
TASK_PROMPT=$(cat -- "$1")
TASK_PROMPT="Repository: $REPO. Resolve all task paths under this repository; use absolute paths when calling filesystem tools. Do not search all of the home directory to locate this checkout.

$TASK_PROMPT"
LOCK=/tmp/hermes-agy.lock
# Long enough for a full analysis run to finish and release the lock.
LOCK_WAIT_SECONDS=${AGY_LOCK_WAIT_SECONDS:-3600}

cd "$REPO"
exec flock --wait "$LOCK_WAIT_SECONDS" "$LOCK" \
  agy -p "$TASK_PROMPT" \
      --model "$MODEL" \
      --output-format text \
      --dangerously-skip-permissions \
      --print-timeout 15m
