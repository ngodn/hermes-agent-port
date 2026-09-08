# Frozen tool/plugin conversation-state checkpoint: source review

Scope: the uncommitted diff on branch `rust-rewrite` touching
`conversation_prompt.rs`, `main.rs`, `native_agent.rs`, `native_tools.rs`,
`plugin_prompt.rs`, and `session_db.rs`. Compared directly against
`agent/conversation_loop.py` (`_restore_or_build_system_prompt`),
`tools/mcp_tool.py` (`persist_agent_tool_names`, `restore_agent_tool_prefix`,
`_merge_preserving_prefix`), `agent/system_prompt.py`, `hermes_state.py`
(`update_session_tool_names`), and `hermes_state_common.py` (schema).

No production, test, or documentation file was modified during this review.

## Verdict

No blocking issues. The checkpoint is sound and matches the Python behavior it
targets on every path that can actually occur in production (clean JSON string
arrays, unique tool names, single-tool native catalog). The headline freeze
works and is genuinely proven by the integration test: a config flap from
`agent_tools = true` to `false` across two process-level initializers still
sends `current_time` on the second turn, and both turns send byte-identical
`tools` and `system_prompt` (`main.rs:1075`, `1099-1101`).

The findings below are all low or trivial severity. Two are behavioral
divergences from Python that only fire on a corrupted `tool_names` DB row, which
neither the Rust nor the Python writer can produce. One is a mislabeled test
that bakes in the divergent behavior. The rest are cosmetic. I would ship this
as-is and track the divergences as comments, but I list the smallest correction
for each so the choice is explicit.

## Findings

### 1. Duplicate saved name resolves to a different schema than Python (low)

`native_tools.rs:76-83`. For a saved name that appears more than once and is
present in the fresh surface, the first occurrence consumes the fresh tool
(`fresh.remove`), and every later occurrence falls through to `registered.get`,
so it picks up the registered (possibly stale) schema instead of the fresh one.

Python does the opposite. `restore_agent_tool_prefix` builds `saved_defs` with a
non-consuming `fresh.get(name)` lookup (`mcp_tool.py:8945-8954`), so both
duplicate slots carry the fresh def. `_merge_preserving_prefix` then pops the
fresh dict once and, for the exhausted second slot, keeps the saved entry, which
was already the fresh def (`mcp_tool.py:8990-8997`). Net: Python emits the fresh
schema for both duplicate slots; Rust emits fresh for the first and registered
for the rest.

Failure scenario: a `tool_names` row of `["a","a"]` where tool `a` is both fresh
and registered with differing schemas. Python sends the fresh schema twice; Rust
sends fresh then registered. This requires a hand-edited or corrupted row, since
every writer (`persist_agent_tool_names`, `update_session_tool_names`) emits a
de-duplicated string array. Practical impact today is zero because the only
native tool is `current_time` and fresh and registered share one definition.

Smallest correction: resolve each saved name against the fresh map without
removing it, then de-duplicate at the end the way Python's downstream request
builder does, or simply document that duplicate saved names are treated as
distinct slots taking the registered schema after the first. No production path
reaches this, so a comment is the proportionate fix.

### 2. Mislabeled merge test asserts the divergent behavior as Python-equivalent (low)

`native_tools.rs:794-822`, test `restored_tool_prefix_matches_python_merge_rules`.
The saved list is `["a","flapped","removed","a"]` with `a` present in fresh
(description `new`) and also registered (description `old`). The test asserts
`merged[2].spec().description == "old"` (`native_tools.rs:817`). Traced through
the Python functions above, the second `a` slot is the fresh def, so Python
yields description `new` there, not `old`. The test therefore encodes finding 1's
divergence while claiming to match Python's merge rules, so a green run here is
not evidence of Python parity for the duplicate case.

The non-duplicate assertions in the same test (fresh override at slot 0,
registered carry-forward for `flapped` at slot 1, `removed` dropped, new `b`
appended, and the stable-input `!changed` case) are correct and do match Python.

Smallest correction: rename the test to state it pins the Rust duplicate-slot
rule, or fix finding 1 and flip the slot-2 expectation to `new`. Either keeps the
test honest about what it proves.

### 3. Malformed-partial JSON discards the whole prefix instead of the bad element (low)

`conversation_prompt.rs:588-597`. Parsing uses
`serde_json::from_str::<Vec<String>>(raw)`, so a row like `["current_time", 7]`
fails whole and yields `saved_tool_names = None`, and `main.rs:531` then falls to
`fresh_tools`. Python's `json.loads` accepts the mixed array, and
`restore_agent_tool_prefix` salvages the valid entries element by element while
the non-string is skipped by the registry lookup (`mcp_tool.py:8947-8954`).

Failure scenario: a corrupted row `["current_time", 7]` on a turn where
`agent_tools` is now off. Python keeps `current_time` frozen from the registry;
Rust drops it because the whole list was rejected and fresh is empty. Again only
reachable from a corrupted row. The all-or-nothing rule is arguably safer and
simpler; the divergence is worth a one-line note, not a rewrite. The
`reused_prompt_recovers_only_valid_ordered_tool_name_json` test (`conversation_prompt.rs:783`)
correctly proves the whole-list rejection, so the tested behavior is the intended
one.

### 4. `tool_names` DB text is not byte-identical to Python (trivial)

`session_db.rs:1061-1072` uses `serde_json::to_string`, which emits
`["a","b"]` (no space after the comma). Python's `update_session_tool_names`
uses `json.dumps(list(...))`, which emits `["a", "b"]` (with a space)
(`hermes_state.py:9645`, confirmed by running it). The doc comment at
`session_db.rs:1059` says "The JSON text shape is shared with Python". The array
schema is shared; the exact bytes are not.

This is harmless: `tool_names` is only ever parsed back (`serde_json::from_str` /
`json.loads`), never compared for cache identity and never hashed, unlike
`system_prompt`. During the strangler window a row written by one runtime parses
cleanly in the other. Smallest correction: soften the comment to say the JSON
shape, not the exact bytes, is shared.

### 5. Redundant re-persist when the saved prefix is `"[]"` (trivial)

`main.rs:513-528`. A reused row storing `"[]"` parses to `Some(vec![])`, so
`restore_tool_prefix(&[], fresh_tools, ...)` appends all fresh tools and reports
`changed = true` whenever fresh is non-empty, triggering an
`update_session_tool_names` write. Python treats an empty saved list as falsy and
returns early from `restore_agent_tool_prefix` (`mcp_tool.py:8940`), so it neither
re-installs nor re-persists. The installed tool surface is identical in both
(fresh tools), so this is only one extra idempotent DB write before the request,
not a tool-surface difference. No action needed.

### 6. Unused snapshot is cloned on every turn (trivial)

`native_agent.rs:475` clones the whole `NativeAgentClient`, including the new
`_plugin_prompt: Snapshot` (now `Clone`, `plugin_prompt.rs:241`), once per turn.
The snapshot is never read on any turn or compression path yet, so the per-turn
`Vec<Section>` clone is pure overhead. It is tiny (empty on the fresh path, a
handful of restored sections on the reused path) and disappears once a real
consumer arrives. No action needed now.

## What the tests actually prove

- `conversation_prompt_is_persisted_before_model_io_and_reused_verbatim`
  (`main.rs:957`) is the strong one. The fixture model handler reads the session
  row and asserts `tool_names == ["current_time"]` before recording the request
  (`main.rs:975`), which proves persistence-before-request for the tool pin, not
  just the prompt. It flips `agent_tools` to `false` and rebuilds under a fresh
  initializer, then asserts the second request still carries `current_time` and
  that `requests[0]["tools"] == requests[1]["tools"]`. If the freeze or the
  registered-catalog carry-forward were broken, the empty fresh surface on turn
  two would drop the tool and the assertion at `main.rs:1099` would fail. This
  genuinely covers freeze, byte-for-byte prompt reuse, and ordering.
- The handler was switched from an SSE stream to a JSON completion
  (`main.rs:965-977`) because installing a tool routes the turn through the
  non-streaming tool loop (`native_agent.rs:490-493`). That is a legitimate
  adjustment, and it means the test now exercises the tool-loop path rather than
  the streaming path.
- `session_tool_names_round_trip_in_wire_order` (`session_db.rs:1801`) proves the
  three-state contract: ordered array, `Some(&[])` stores `"[]"`, `None` clears
  to SQL NULL. It does not (and cannot) prove Python byte-parity, see finding 4.
- `reused_prompt_recovers_only_valid_ordered_tool_name_json`
  (`conversation_prompt.rs:783`) proves valid ordered JSON is recovered and both
  a type-mismatched element and non-JSON reject to `None`. It pins the
  whole-list rejection of finding 3.
- `native_client_owns_callback_free_restored_plugin_snapshot`
  (`native_agent.rs:696`) proves the snapshot is retained on the client with no
  callback execution. It only reaches the field through a `#[cfg(test)]`
  accessor (`plugin_prompt.rs:258`); nothing in production reads it yet.
- `restored_tool_prefix_matches_python_merge_rules` (`native_tools.rs:794`)
  proves the fresh-override, registered-carry-forward, drop-removed, and
  append-new rules, plus the stable `!changed` case. Its duplicate-slot
  assertion does not match Python, see finding 2.

## Residual deferred boundaries (intentional, not gaps to fix here)

These are correctly scoped out and each is keyed to a subsystem that is not yet
ported. They are why the plugin half of the checkpoint is inert rather than
wrong.

- Plugin sections are never rendered into the native fresh prompt
  (`build_fresh` emits no plugin block), so `plugin_prompt.restore` on the reused
  path (`main.rs:511`) currently extracts nothing. The seam is real and the
  restore is a no-op until a native plugin manager writes sections into the
  prompt. Matches the deferral recorded in the map.
- `_plugin_prompt` on `NativeAgentClient` (`native_agent.rs:208`) is dead
  ownership on purpose: it exists so a later compression rebuild reuses frozen
  bytes instead of consulting live plugin state. There is no compression path in
  the native client yet, so it is retained but unread. The underscore name and
  the comment make the intent explicit.
- `reconstruct_static_prefix` (Python `conversation_loop.py:1153`) has no native
  counterpart. The native prompt is a single reused block, not a segmented
  static-plus-volatile layout, so there is no two-block cache breakpoint to
  reconstruct. Deferred until the prompt is segmented.
- The native tool catalog is the single static `CurrentTimeTool`
  (`main.rs:205-213`). There is no live `check_fn` probing, no MCP registration,
  and no between-turns availability refresh, so `_merge_preserving_prefix`'s
  flap and late-append cases exist in code but cannot be driven by real tool
  churn yet. The merge is ported ahead of the surface it will eventually fold.
- `saved_tool_names` restoration reads and writes correctly, but with one native
  tool the freeze is close to degenerate. Its value grows only when the catalog
  does.

## Bottom line

Ship it. Persistence order (prompt then tool names, both before provider I/O),
byte-for-byte prompt reuse, the tool freeze, three-state pin semantics, and
profile isolation all hold and are tested. The only real divergences from Python
are on a corrupted `tool_names` row that no writer produces, and the one test
that overstates Python parity should be renamed or its duplicate expectation
fixed. Everything plugin-side is correctly deferred behind the native plugin
manager and native compression, neither of which exists yet.
