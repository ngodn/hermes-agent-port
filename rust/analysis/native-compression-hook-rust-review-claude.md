# Rust safety and integration audit: native compression-hook checkpoint

Scope: the uncommitted changes wiring a native `session:compress` lifecycle hook
into the Rust gateway. Reviewed files:

- `rust/crates/hermes-gateway/src/hooks.rs`
- `rust/crates/hermes-gateway/src/native_agent.rs`
- `rust/crates/hermes-gateway/src/main.rs`
- `rust/crates/hermes-gateway/src/message.rs` (changed tests)

Out of scope by instruction: the Python runner implementation itself. It was read
only far enough to confirm the command ABI Rust depends on
(`hermes_cli/rust_hook_runner.py`).

Method: read-only source audit plus focused test runs. No files were edited.

## Important caveat: the tree moved during review

A peer session (`bca-97`, the implementation lane) was actively editing these
files while I audited. Between my first and second read, `native_agent.rs` grew
from 52 to 91 inserted lines: the inline payload block was refactored into a
`compression_hook_context` helper and a new unit test was added. Everything below
reflects the on-disk state at the time of writing. Line numbers in
`native_agent.rs` in particular may drift if the lane keeps moving. Re-run the
cited greps before acting on any single line reference.

## Verdict

No blocking findings. The checkpoint is safe and correctly integrated. The
isolation, timeout, cleanup, fire-and-forget, ordering, payload, and counter
requirements all hold. Remaining items are nonblocking parity notes and a few
test gaps.

Focused tests run green in my checkout:

- `hooks::` unit tests: 9 passed
- `startup_tests::conversation_prompt_is_persisted_before_model_io_and_reused_verbatim`: passed (production discovery, in-place payload, prompt-cache byte stability)
- `message::tests::full_compression_commits_after_tools_before_the_same_turn_followup`: passed (runner path, memory ordering, in-place payload)

## Checklist findings

### Profile-secret isolation: PASS

`apply_isolated_env` (`hooks.rs:305`) does `env_clear()`, re-inherits only names
accepted by `crate::secret_scope::is_global_env`, layers the prepared
`profile_env`, then forces `HERMES_HOME` last (`hooks.rs` around the `env_clear`
plus `HERMES_HOME` block). Provider credentials (`ANTHROPIC_API_KEY`,
`OPENROUTER_API_KEY`, relay auth, `API_SERVER_KEY`, etc.) are deliberately absent
from `GLOBAL_ENV_EXACT`/`GLOBAL_ENV_PREFIXES` (`secret_scope.rs:107-160`), so a
foreign profile secret sitting in the ambient process env cannot reach a handler.
A `profile_env` entry cannot override profile identity because `HERMES_HOME` is
written after it. `HERMES_HOOK_EVENT` is set after `apply_isolated_env`
(`hooks.rs:347-352`), so `env_clear` cannot wipe it. Covered by
`profile_env_isolation_and_home_identity`.

Note: isolation runs only when a `HookRuntime` is installed. Production always
installs one (`main.rs:962`), so this is only a test-mode caveat.

### Program and working-directory resolution: PASS

With a runtime, a `.py` handler runs as `python -m hermes_cli.rust_hook_runner
<handler> <event>` from `rt.repo_root` (`hooks.rs:259` onward, runner branch),
using `rt.python`. In production those come from `config.agent_python` and
`config.agent_cwd` (`main.rs:965-966`), the same interpreter and cwd the
extension host already spawns Python from (`main.rs:831`), so the `-m` package
import resolves consistently. Runner invocation shape is pinned by
`python_handler_routes_through_runner_when_runtime_set`, and the runner ABI
(`argv=[handler, event]`, JSON object on stdin, compact JSON or empty on stdout,
nonzero exit on error) matches what Rust sends and how it reads the result
(`hooks.rs:397-402`).

Minor: non-Python handlers under a runtime do not get `current_dir` set, so they
inherit the gateway cwd. Consistent with prior behavior; not a defect.

### Child timeout and cleanup: PASS

Every child is `kill_on_drop(true)` (`hooks.rs:362`) and wrapped in
`tokio::time::timeout(timeout, child.wait_with_output())` (`hooks.rs:376`). On
timeout the wait future is dropped, which drops the child, which triggers the
OS kill and tokio reaping; the function returns `ErrorKind::TimedOut`. stdin is
written from a detached task (`hooks.rs` stdin-writer spawn) so a handler that
never reads stdin cannot wedge the write on a full pipe buffer. `wait_with_output`
drains stdout concurrently, so there is no stdout deadlock either. Covered by
`timeout_kills_hung_handler`, which proves the post-sleep side effect never
lands. The 30s `HANDLER_TIMEOUT` (`hooks.rs:46`) is a reasonable fixed ceiling;
because the emit is spawned, even a full 30s hang never delays compression.

### Fire-and-forget behavior: PASS

The emit is `tokio::spawn`ed and never awaited (`native_agent.rs:2007-2013`).
`notify_compression_boundary` returns `memory_result` independently. A slow or
broken handler cannot delay a committed boundary.

### Ordering after durable publication and memory notification: PASS

Durable publication happens first in `automatic_compression.rs`
(`publish_in_place_compression` / `publish_compression` at lines 1109 and 1127);
`notify_compression_boundary` is only called on a committed result
(`automatic_compression.rs:1119`, `1149`). Inside the native client, the memory
`session_switch` is awaited before the hook context is built and spawned
(`native_agent.rs:1995-2013`). The integration test asserts
`memory_completed: true` (`message.rs:2723`, `2917`), proving the handler observes
the memory boundary file already written. The event cannot fire before the memory
notification because the spawn happens after the await.

### Event delivery despite memory transport failure: PASS (code), test gap

The emit block is outside the `memory_result` match and unconditional on it
(`native_agent.rs:1995-2013`): the hook fires even when `session_switch` returned
`Err`. The `Err` is still returned to the caller, where `conversation_agent`
retires the stale client via `finish_compression_boundary(..., result.is_ok())`
(`conversation_agent.rs:908-914`), but the event for this boundary already fired
with the correct count. No unit test exercises the memory-failure path (the
integration tests use a succeeding provider). See test gaps below.

### Exact rotation and in-place payload construction: PASS

`compression_hook_context` (`native_agent.rs:546`) emits the five-key Python ABI
payload: `platform`, `session_id = new_session_id`, `old_session_id = "" when
in_place else old_session_id`, `in_place`, `compression_count`. The in-place
empty `old_session_id` intentionally diverges from the real id passed to the
memory `session_switch`, matching the Python contract and the prior boundary
review's "do not fix this up" guidance. Both shapes are now unit-tested by
`compression_hook_payload_uses_python_ids_and_clone_shared_count`
(`native_agent.rs:2365`): in-place gives `old_session_id: ""`, rotation gives
`old_session_id: "parent"` with `in_place: false`. In-place is also covered
end-to-end in `message.rs:2913` and `main.rs:1754`.

### compression_count lifetime across clones and cache rekeys: PASS, with parity notes

`compression_count` is `Arc<AtomicU32>` (`native_agent.rs:457`), so the derived
`Clone` shares one counter across all clones. The new unit test builds a client,
clones it, and observes `1` then `2` across the clone, confirming shared state.
Across a cache rekey, `finish_compression_boundary` moves the same `CacheEntry`
(hence the same `ClientCell` and the same client `Arc`, hence the same counter)
from the old session key to the new one on success
(`conversation_agent.rs:181-208`); for in-place the key is unchanged. So the
counter survives both in-place boundaries and split rekeys within a conversation.

Nonblocking parity notes (both flagged as open decisions in the prior boundary
review, not regressions here):

1. Rust increments the counter inside `notify_compression_boundary`, which runs
   only after a committed boundary, so it counts commits. Python increments in
   `compress()` before the durable split, so it counts attempts including
   caught/rolled-back ones. The values can diverge after a failed attempt.
2. On the memory-failure path the client is retired
   (`conversation_agent.rs:186-194`); the next boundary rebuilds a fresh client
   with the counter reset to 0. Python's in-memory counter would not reset there.

Minor style note: `compression_hook_context` increments the counter as a side
effect of building the payload. It is called exactly once per boundary so this is
correct, but a "build context" method mutating shared state is slightly
surprising. Not a defect.

### Production discovery: PASS

`build_conversation_client` builds the runtime with `profile_env` from the active
secret scope or `home/.env` (`main.rs:959-967`), discovers `home/hooks`
(`main.rs:968`), and stores `Some(Arc<HookRegistry>)` only when at least one hook
loaded, else `None` (`main.rs:969-974`). The registry and platform are threaded
through `NativeConversationState` and `with_hooks` (`main.rs:736`). Exercised by
the startup test, which discovers a real hook and observes its output after a live
`notify_compression_boundary`.

### Prompt-cache stability: PASS

`hooks`, `platform`, and `compression_count` are independent client fields; none
feed prompt assembly, the tool snapshot, or the cache key. The byte-stability
assertion in `conversation_prompt_is_persisted_before_model_io_and_reused_verbatim`
still passes with hooks present, confirming a reinitialized client restores the
stored prompt bytes without consulting changed sources.

### Test adequacy: GOOD, with gaps

Strong coverage: env isolation and forced home, nonzero-exit isolation with the
next handler still running, timeout kill, runner invocation shape, gated real
runner execution, both payload shapes, clone-shared counter, production discovery
plus live emit, and memory-ordering end-to-end through the real runner.

Nonblocking gaps:

1. No test that the hook still fires when the memory `session_switch` returns
   `Err` (the "event delivery despite memory transport failure" property is only
   verified by code inspection).
2. The clone test approximates counter persistence but nothing exercises the
   actual `conversation_agent` rekey moving the counter across a split
   (`finish_compression_boundary`).
3. Malformed handler stdout is silently dropped to `None`
   (`hooks.rs:402`, `serde_json::from_str(...).ok()`); low risk, untested.

## Design notes worth surfacing (nonblocking)

- Under a runtime, Python routing takes precedence over the executable bit: a
  `handler.py` shipped with a shebang and a `__main__` entrypoint but no
  `handle()` function will fail through the runner (logged, non-fatal) instead of
  running directly. This is the intended ABI shift, but it is a behavior change
  for any existing executable `handler.py`.
- `platform` is frozen at client-build time from the first message
  (`main.rs:782`), whereas Python computes it at emit time. Stable per
  conversation, so a divergence would require a client shared across platforms,
  which the `(home, session_id)` cache key makes unlikely.
- `compression_count` increments even when the loaded hooks contain no
  `session:compress` handler (the counter advances before `emit` finds no match).
  Harmless, and Python increments unconditionally too.
