#!/usr/bin/env bash
# Wrapper for the Antigravity CLI (agy) used as a headless mapping/analysis
# helper for the Rust port. Model is pinned to gemini-3.7-flash-high (high
# effort baked into the slug). Prompts must instruct agy where to write.
#
# Usage: rust/tools/agy.sh <prompt-file>
set -euo pipefail

REPO=/home/eins0fx/development/hermes-agent-port
MODEL=gemini-3.7-flash-high
PROMPT_FILE="$1"

cd "$REPO"
agy -p "$(cat "$PROMPT_FILE")" \
    --model "$MODEL" \
    --output-format text \
    --dangerously-skip-permissions \
    --print-timeout 15m
