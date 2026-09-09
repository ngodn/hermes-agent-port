# Python Compatibility Review: Native Compression-Hook Checkpoint

## Executive Summary

This review audits the Python compatibility surface of the uncommitted native compression-hook checkpoint, focusing on [`hermes_cli/rust_hook_runner.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_hook_runner.py), [`tests/cli/test_rust_hook_runner.py`](file:///home/eins0fx/development/hermes-agent-port/tests/cli/test_rust_hook_runner.py), the Python gateway hook ABI in [`gateway/hooks.py`](file:///home/eins0fx/development/hermes-agent-port/gateway/hooks.py), and the `session:compress` emitter in [`agent/conversation_compression.py`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py).

The checkpoint adheres to the Python hook ABI and matches the runtime contracts of the legacy Python gateway and agent loop. No blocking compatibility or security flaws were identified.

Legend:
- **[VERIFIED]**: Confirmed directly by source inspection and passing test executions.
- **[NOTE]**: Non-blocking design observation or operational nuance.

---

## 1. Gateway Hook ABI Compatibility (`gateway/hooks.py` vs `hermes_cli/rust_hook_runner.py`)

### 1.1 Argument Order and Invocations

**[VERIFIED]**
- **Legacy Python contract**: Defined in [`gateway/hooks.py:7`](file:///home/eins0fx/development/hermes-agent-port/gateway/hooks.py#L7) and dispatched at [`gateway/hooks.py:195`](file:///home/eins0fx/development/hermes-agent-port/gateway/hooks.py#L195) and [`gateway/hooks.py:222`](file:///home/eins0fx/development/hermes-agent-port/gateway/hooks.py#L222) as `handle(event_type, context)`.
- **Runner implementation**: In [`hermes_cli/rust_hook_runner.py:35-41`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_hook_runner.py#L35-L41), the CLI expects `argv = [handler_path, event_type]`. At line 95, `handle_fn` is invoked with `handle_fn(event_type, context)`.
- **Caller alignment**: In [`rust/crates/hermes-gateway/src/hooks.rs:273-278`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/hooks.rs#L273-L278) and line 357, the command is constructed as `python -m hermes_cli.rust_hook_runner <handler> <event_type>`, streaming `context` via stdin.
- The argument order matches the Python contract across CLI and function call boundaries.

### 1.2 Sync and Async Behavior

**[VERIFIED]**
- **Legacy Python contract**: [`gateway/hooks.py:196-198`](file:///home/eins0fx/development/hermes-agent-port/gateway/hooks.py#L196-L198) and [`gateway/hooks.py:223-224`](file:///home/eins0fx/development/hermes-agent-port/gateway/hooks.py#L223-L224) handle both synchronous functions and coroutines (`if asyncio.iscoroutine(result): await result`).
- **Runner implementation**: In [`hermes_cli/rust_hook_runner.py:96-107`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_hook_runner.py#L96-L107), `inspect.isawaitable(result)` detects both coroutines and custom awaitables. If awaitable, it wraps general awaitables into coroutines and executes them in an isolated event loop via `asyncio.Runner()` (falling back to `asyncio.run()` on older runtimes).
- Synchronous handlers execute directly and return their values without entering asyncio machinery.
- Both synchronous and asynchronous handlers correctly emit compact JSON when returning non-None values, and emit nothing when returning None.

### 1.3 Module Import Semantics

**[VERIFIED]**
- **Legacy Python contract**: [`gateway/hooks.py:127-142`](file:///home/eins0fx/development/hermes-agent-port/gateway/hooks.py#L127-L142) registers `sys.modules[module_name] = module` before calling `spec.loader.exec_module(module)` to support forward reference resolution (for `from __future__ import annotations`, Pydantic models, and dataclasses).
- **Runner implementation**: [`hermes_cli/rust_hook_runner.py:64-85`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_hook_runner.py#L64-L85) uses a collision-safe dynamic module name `_hermes_rust_hook_<uuid>` and inserts `sys.modules[module_name] = module` before executing `spec.loader.exec_module(module)`.
- Forward reference resolution in handlers operates identically to the legacy in-process hook loader.

### 1.4 JSON Input / Output Handling

**[VERIFIED]**
- **Input**: [`hermes_cli/rust_hook_runner.py:43-57`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_hook_runner.py#L43-L57) reads stdin, parses with `json.loads`, and explicitly asserts `isinstance(context, dict)`. Malformed JSON or non-object payloads write a single diagnostic line to stderr and exit with status 1.
- **Output**: [`hermes_cli/rust_hook_runner.py:112-119`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_hook_runner.py#L112-L119) writes to stdout only if `result is not None`. The output is serialized using compact separators (`separators=(",", ":")`). If serialization fails, the error is written to stderr and exits with status 1.
- If `result is None`, stdout remains completely empty, matching the expectation in [`rust/crates/hermes-gateway/src/hooks.rs:404-406`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/hooks.rs#L404-L406).

### 1.5 Module Cleanup and Namespace Hygiene

**[VERIFIED]**
- **Runner implementation**: Lines 65-122 of [`hermes_cli/rust_hook_runner.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/rust_hook_runner.py#L65-L122) execute inside a `try ... finally` block where `sys.modules.pop(module_name, None)` is unconditionally executed upon exit.
- This ensures in-process test runs and direct imports do not leak dynamically generated module names across calls.
- Verified in [`tests/cli/test_rust_hook_runner.py:246-265`](file:///home/eins0fx/development/hermes-agent-port/tests/cli/test_rust_hook_runner.py#L246-L265) for both successful completions and exceptions raised during handler execution.

### 1.6 Error Containment and Diagnostic Safety

**[VERIFIED]**
- **Runner containment**:
  - Argument errors exit with status 2 and usage instructions on stderr.
  - Stdin read errors, JSON decode errors, non-dict payloads, missing handler files, import errors, missing/non-callable `handle` functions, runtime handler exceptions, and serialization errors all output concise error messages strictly to stderr and exit with status 1.
  - Raw exception tracebacks and runtime secrets are never written to stdout.
- **Gateway containment**:
  - In [`rust/crates/hermes-gateway/src/hooks.rs:206-218`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/hooks.rs#L206-L218) and line 365, handler stderr is redirected to `Stdio::null()` and non-zero exit codes are logged at `tracing::warn!` level without halting the gateway event pipeline.

---

## 2. Generic `session:compress` Event Emitter Compatibility

### 2.1 Exact Five-Key Payload Contract

**[VERIFIED]**
- **Legacy Python emitter**: Defined at [`agent/conversation_compression.py:5575-5581`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5575-L5581):
  ```python
  agent.event_callback("session:compress", {
      "platform": agent.platform or "",
      "session_id": agent.session_id,
      "old_session_id": _old_sid or "",
      "in_place": in_place,
      "compression_count": agent.context_compressor.compression_count,
  })
  ```
- **Native gateway emitter**: Defined at [`rust/crates/hermes-gateway/src/native_agent.rs:1991-1997`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1991-L1997):
  ```rust
  let context = json!({
      "platform": self.platform.as_ref(),
      "session_id": new_session_id,
      "old_session_id": if in_place { "" } else { old_session_id },
      "in_place": in_place,
      "compression_count": compression_count,
  });
  ```
- Comparison of fields:
  1. `platform`: String (`str`), fallback to empty string.
  2. `session_id`: Current/child session identifier string.
  3. `old_session_id`: Parent session identifier on rotation, empty string on in-place compaction.
  4. `in_place`: Boolean flag indicating whether compaction was in-place.
  5. `compression_count`: Integer indicating the cumulative compaction count.
- The field names, types, and structure match the legacy Python payload.

### 2.2 In-Place `old_session_id` Semantics

**[VERIFIED]**
- In [`agent/conversation_compression.py:5485`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5485), `_old_sid = locals().get("old_session_id")`. In in-place compaction (`in_place=True`), lines 4931-4999 execute and `old_session_id` is never bound in `locals()`, leaving `_old_sid` as `None`.
- Consequently, [`agent/conversation_compression.py:5578`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5578) evaluates `_old_sid or ""` to `""` (empty string).
- In [`rust/crates/hermes-gateway/src/native_agent.rs:1994`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1994), `if in_place { "" } else { old_session_id }` replicates this behavior.
- Documented in [`website/docs/user-guide/features/hooks.md:82`](file:///home/eins0fx/development/hermes-agent-port/website/docs/user-guide/features/hooks.md#L82) and [`website/docs/developer-guide/context-compression-and-caching.md:147`](file:///home/eins0fx/development/hermes-agent-port/website/docs/developer-guide/context-compression-and-caching.md#L147), which explicitly state that `old_session_id` is an empty string in in-place mode.

### 2.3 `compression_count` Monotonic Semantics

**[VERIFIED]**
- In [`agent/context_compressor.py:3610`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L3610), `self.compression_count` initializes to 0.
- In [`agent/context_compressor.py:8814`](file:///home/eins0fx/development/hermes-agent-port/agent/context_compressor.py#L8814), `self.compression_count += 1` increments immediately upon message compaction.
- When `session:compress` is emitted at [`agent/conversation_compression.py:5580`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5580), the count is 1 for the first compaction, 2 for the second, representing a 1-based completed compression counter.
- In [`rust/crates/hermes-gateway/src/native_agent.rs:1987-1990`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1987-L1990), `self.compression_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1` produces identical 1-based numbering starting at 1.

### 2.4 Event Emission Timing and Ordering

**[VERIFIED]**
- In [`agent/conversation_compression.py:5537-5584`](file:///home/eins0fx/development/hermes-agent-port/agent/conversation_compression.py#L5537-L5584), `agent._memory_manager.on_session_switch` completes before `session:compress` is dispatched.
- In [`rust/crates/hermes-gateway/src/native_agent.rs:1975-2003`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/native_agent.rs#L1975-L2003), `host.session_switch` is awaited before `hooks.emit("session:compress", ...)` is spawned.
- In [`gateway/run.py:5834-5837`](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L5834-L5837), `_event_callback_sync` dispatches `emit` via `asyncio.run_coroutine_threadsafe` asynchronously. In Rust, `tokio::spawn(async move { hooks.emit(...).await; })` mirrors this fire-and-forget asynchronous pattern.
- Verified by integration test in [`rust/crates/hermes-gateway/src/message.rs:2890-2917`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/message.rs#L2890-L2917), asserting `"memory_completed": true` inside the hook handler.

---

## 3. Test Coverage Verification

**[VERIFIED]**
The test suite in [`tests/cli/test_rust_hook_runner.py`](file:///home/eins0fx/development/hermes-agent-port/tests/cli/test_rust_hook_runner.py) contains 26 test cases:
1. `test_sync_handler_returns_structured_json`: Synchronous handlers returning dictionary payloads.
2. `test_sync_handler_returns_none`: Synchronous handlers returning None.
3. `test_async_handler_returns_structured_json`: Coroutine handlers returning dictionary payloads.
4. `test_async_handler_returns_none`: Coroutine handlers returning None.
5. `test_exact_arguments_delivered_to_handle`: Verifies exact types and structures for `(event_type, context)`.
6. `test_malformed_json_input`: Tests broken JSON on stdin (`"{bad json"`, `""`, `"{}{}"`).
7. `test_non_object_json_input`: Tests non-dict JSON primitives (`[1, 2, 3]`, `"string"`, `42`, `true`, `null`).
8. `test_missing_handle_symbol`: Handlers without `handle` defined.
9. `test_non_callable_handle`: Handlers where `handle` is not callable.
10. `test_import_error_syntax_error`: Handlers with syntax errors.
11. `test_import_error_missing_module`: Handlers with missing imports.
12. `test_import_error_nonexistent_file`: Nonexistent file paths.
13. `test_raised_handler_sync`: Exception propagation containment for sync handlers.
14. `test_raised_handler_async`: Exception propagation containment for async handlers.
15. `test_paths_containing_spaces`: Paths and directories containing spaces.
16. `test_malformed_argv`: Insufficient or extra command line arguments.
17. `test_imported_module_cleanup`: Confirms `sys.modules` cleanup after both successful runs and failures.
18. `test_subprocess_python_m_proof`: End-to-end execution of `python -m hermes_cli.rust_hook_runner`.

Test results:
- `pytest tests/cli/test_rust_hook_runner.py`: 26 passed in 0.48s.
- `ruff check hermes_cli/rust_hook_runner.py tests/cli/test_rust_hook_runner.py`: All checks passed.
- `cargo test --manifest-path rust/Cargo.toml -p hermes-gateway hooks`: 9 passed in 1.19s.
- `cargo test --manifest-path rust/Cargo.toml -p hermes-gateway message::tests::full_compression_commits_after_tools_before_the_same_turn_followup`: 1 passed in 1.29s.
- `cargo test --manifest-path rust/Cargo.toml -p hermes-gateway startup_tests`: 6 passed in 2.78s.

---

## 4. Security and Compatibility Analysis

### 4.1 Blocking Flaws
- None identified.

### 4.2 Non-Blocking Observations

**[NOTE] Stdout Pollution in Subprocess Architecture**:
- In the legacy in-process Python gateway, any debug `print(...)` statements in `handler.py` printed to the gateway stdout without corrupting the return value of `handle(...)` because Python returned the object in memory.
- In the subprocess model, stdout is the IPC channel for structured returns collected by `emit_collect`. If a user hook includes unbuffered `print(...)` statements before returning a dictionary, stdout will contain interleaved text and JSON.
- For `session:compress`, this does not cause issues because `emit("session:compress", ...)` discards stdout (`collect_output = false`, `Stdio::null()`). For decision hooks using `emit_collect`, handlers should direct diagnostic output to stderr or logging rather than stdout.

**[NOTE] Working Directory and Relative Imports**:
- The runner is invoked with `cwd = repo_root`. Hook handlers importing local auxiliary Python modules located inside their own hook directory should use relative package imports or append their directory to `sys.path`. This matches the legacy Python behavior where `sys.path` was not altered per hook.

---

## 5. Conclusion

The Python compatibility surface of the native compression-hook checkpoint satisfies all ABI requirements:
1. Argument order `(event_type, context)` is preserved.
2. Synchronous and asynchronous handler execution models operate correctly.
3. Import semantics and forward-reference support match `gateway/hooks.py`.
4. JSON input parsing and compact output formatting are robust.
5. In-process module cleanup leaves no residue in `sys.modules`.
6. Error handling isolates failures to stderr with non-zero exit codes.
7. The exact five-key payload (`platform`, `session_id`, `old_session_id`, `in_place`, `compression_count`) is identical to `agent/conversation_compression.py`.
8. In-place compaction sets `old_session_id` to an empty string `""`.
9. The 1-based monotonic compression count semantics match.

The Python compatibility layer is sound and ready for merge.
