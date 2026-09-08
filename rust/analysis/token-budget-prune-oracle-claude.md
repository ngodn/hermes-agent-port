# Token-budget tool-result prune oracle, Claude lane

Golden fixtures for the Rust port of the token-budget-aware tool-result prune.
The generator runs the real CPython implementation and freezes its
inputs/outputs so the Rust port is checked against actual behaviour, not a
paraphrase.

## Authoritative source

- `agent/context_compressor.py`
  - `_estimate_msg_budget_tokens` (per-message budget estimator)
  - `_prune_old_tool_results` (the boundary walk plus Passes 1-4)
  - supporting helpers: `_content_length_for_budget`,
    `_serialized_length_for_budget`, `_reasoning_details_text_chars`,
    `_collect_protected_skill_names`, `_retire_stale_tool_result_images`,
    `_strip_images_from_tool_msg`, `_summarize_tool_result`,
    `_stale_thinking_on_wire`
  - constants: `_CHARS_PER_TOKEN`, `_IMAGE_TOKEN_ESTIMATE`,
    `_IMAGE_CHAR_EQUIVALENT`, `_PRUNE_MIN_CHARS`, `_MAX_TAIL_MESSAGE_FLOOR`,
    `_PRESSURE_KEEP_RECENT_MESSAGES`, `_MAX_KEEP_TOOL_IMAGES`,
    `_SKILL_PRUNE_RECENT_WINDOW`
- `agent/model_metadata.py::estimate_tokens_rough` (ASCII vs CJK vs byte-count
  token math that the estimator sums)
- `agent/turn_context.py::drop_stale_api_content` (sidecar drop on rewrite)

## Generator

`rust/tools/token-budget-prune-oracle.py` builds a `ContextCompressor` with
`get_model_context_length` patched to a fixed 100k window (mirroring
`tests/agent/test_context_compressor.py`), so construction never touches the
network and no environment-dependent value enters a golden. It writes
`rust/tools/token-budget-prune-goldens.json`. Run from the repo root:

    .venv/bin/python rust/tools/token-budget-prune-oracle.py

`--check` re-derives the fixture from Python and fails if the checked-in file
drifted, so CI can flag a stale golden after any estimator or prune tweak.

Every case computes its expected output by calling the real functions on a deep
copy of the input. Budgets for the equality and floor cases are derived FROM
the real estimator and then frozen to a concrete integer; the generator asserts
each case actually exercises its target branch before writing, so a silent
behaviour change breaks the run rather than baking a wrong golden.

## Sections

`estimator` (17 cases) drives `_estimate_msg_budget_tokens` directly: ASCII vs
CJK-dense vs mixed vs Cyrillic/emoji byte-counted text, empty content, the full
`str(tool_call)` envelope (single and parallel), image part lists and the
native multimodal dict, the always-charged Codex sidecars (on with charge both
True and False), the newest-turn-only `reasoning`/`reasoning_content` keys with
`reasoning_content`-wins dedup, and `reasoning_details` text-only charging.

`prune` (14 cases) drives `_prune_old_tool_results`:

- `boundary-equality-exact` / `boundary-equality-minus-one`: the walk breaks on
  strict `>`, so a message whose inclusion makes the running total exactly
  equal to the budget stays protected; one token less demotes it. The tail is
  kept light so Pass 4 does not re-prune the protected tool and mask the flip.
- `eight-message-floor-cap`: `protect_tail_count=20` on a 12-message transcript
  is capped at `_MAX_TAIL_MESSAGE_FLOOR` (8), so the two head tools demote where
  an uncapped count would protect everything.
- `all-content-fits`: a budget larger than the whole transcript leaves the
  boundary at 0; output equals input, pruned 0.
- `stale-thinking-newest-only-generic` / `-charge-every-turn-deepseek`: same
  input and budget, different route. The generic route charges the stale
  reasoning turn's thinking as free (newest-turn-only) and protects the older
  tool; the deepseek echo-back route charges it every turn, exhausts the budget
  one message sooner, and demotes that tool.
- `codex-sidecar-fills-tail` / `codex-sidecar-absent-control`: same budget; the
  always-replayed `codex_reasoning_items` / `codex_message_items` weight pushes
  the boundary earlier and prunes more than the sidecar-free control.
- `pass4a-pressure-loop` / `pass4b-demote-all-but-newest` /
  `pass4c-last-resort-newest-tool`: the three protected-tail pressure stages
  (loop inside `[boundary, len - keep_recent)`, then demote every protected tool
  but the newest, then the last-resort demotion of the newest tool body).
- `protected-skill-spared-normal-prune` / `protected-skill-pass4-override`: a
  `skill_view` body referenced by a tail user message survives the ordinary
  Pass 2 demotion but is demoted under Pass 4 pressure
  (`spare_protected_skills=False`).
- `stale-api-content-image-demotion`: Pass 3.5 retires all but the newest three
  image-bearing tool results; each retired message loses its `api_content`
sidecar so replay cannot resend the pre-strip bytes, while the newest three
keep image and sidecar.

`tail_cut` (3 cases) drives `_find_tail_cut_by_tokens` directly. It covers the
full parallel tool-call envelope, the all-fit raw-budget fallback, complete
tool-group alignment, and the latest user/assistant reply anchors.

## Verification

- `.venv/bin/python rust/tools/token-budget-prune-oracle.py` writes the golden
  (17 estimator + 14 prune + 3 tail-cut cases); `--check` returns OK against
  the checked-in file.
- Relevant Python suites pass (200 tests): `test_context_compressor.py`,
  `test_compressor_tool_call_budget.py`, `test_cjk_token_estimation.py`,
  `test_compressor_stale_tool_images.py`,
  `test_budget_reasoning_details_exclusion.py`, `test_ghost_skill_pruning.py`,
  `test_protected_tail_pressure_61932.py`, `test_replay_budget_accounting.py`.

## Note for the Rust port

Passes 2 through 4 all run after the boundary walk, so a protected but bulky
tool body can still be demoted by the Pass 4 pressure cascade. Several golden
cases are deliberately shaped with a light protected tail to isolate the
boundary behaviour from Pass 4; the Rust port must keep the pass ordering
(boundary walk, dedup, summarize, arg-truncate, image retire, pressure) and the
strict `>` boundary comparison for these goldens to hold.
