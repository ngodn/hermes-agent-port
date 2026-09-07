# Plugin Registration, Prompt Section, and Unload Lifecycle Source Review

This document specifies the exact runtime semantics, validation rules, ordering guarantees, memory limits, and disposal invariants of the plugin registration subsystem in [`hermes_cli/plugins.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py). It provides authoritative requirements for implementing plugin registration, system prompt section injection, ownership tracking, and owner unloading in the Rust port without divergence.

---

## 1. Scope, Authority, and Source Mapping

### 1.1 Source Files and Line Ranges
- **Primary Registration & Lifecycle Implementation**: [`hermes_cli/plugins.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py)
  - System prompt section constants & validation regex: [lines 558-588](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L558-L588)
    - `SYSTEM_PROMPT_SECTION_POSITIONS`: [line 558](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L558)
    - `DEFAULT_SYSTEM_PROMPT_SECTION_MAX_CHARS` / `MAX_SYSTEM_PROMPT_SECTION_CHARS` (4,000): [lines 559-560](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L559-L560)
    - `MAX_SYSTEM_PROMPT_SECTIONS` (32): [line 561](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L561)
    - `MAX_SYSTEM_PROMPT_SECTIONS_TOTAL_CHARS` (8,000): [line 562](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L562)
    - `_SYSTEM_PROMPT_SECTION_ID_RE` (`^[a-z0-9][a-z0-9._-]{0,127}$`): [line 563](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L563)
    - Framing markers (`PLUGIN_SECTIONS_START`, `PLUGIN_SECTIONS_END`): [lines 565-566](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L565-L566)
    - `is_valid_system_prompt_section_id`: [lines 569-571](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L569-L571)
    - `format_system_prompt_section` & `format_system_prompt_sections`: [lines 574-588](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L574-L588)
  - Data classes: [lines 1172-1271](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L1172-L1271)
    - `PluginSystemPromptSection`: [lines 1172-1181](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L1172-L1181)
    - `RenderedPluginSystemPromptSection`: [lines 1183-1191](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L1183-L1191)
    - `PluginRegistration`: [lines 1230-1271](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L1230-L1271)
  - Plugin context registration APIs:
    - `PluginContext._track`: [lines 1600-1617](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L1600-L1617)
    - `PluginContext._track_replacement`: [lines 1619-1638](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L1619-L1638)
    - `PluginContext.on_unload`: [lines 1701-1714](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L1701-L1714)
    - `PluginContext.spawn_task`: [lines 1715-1730](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L1715-L1730)
    - `PluginContext.register_system_prompt_section`: [lines 3412-3488](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L3412-L3488)
  - Manager ownership ledger and teardown:
    - Container declarations (`_system_prompt_sections`, `_ownership_ledger`, `_registration_order`, `_persistent_carryover`): [lines 3756, 3812-3820](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L3756-L3820)
    - `PluginManager._track_registration`: [lines 3843-3875](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L3843-L3875)
    - `PluginManager._evict_stale_persistent_registrations`: [lines 3877-3918](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L3877-L3918)
    - `PluginManager._restore_mapping`: [lines 3941-3956](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L3941-L3956)
    - `PluginManager._forget_registrations`: [lines 3987-4009](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L3987-L4009)
    - `PluginManager._dispose_registrations`: [lines 4010-4026](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L4010-L4026)
    - `PluginManager.unload` & `_unload_scoped`: [lines 4037-4195](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L4037-L4195)
    - `PluginManager.render_system_prompt_sections`: [lines 5917-6005](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L5917-L6005)
    - Module-level `render_system_prompt_sections`: [lines 6486-6490](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L6486-L6490)

- **Ownership Lease & Generational Replacement**: [`registration_lifecycle.py`](file:///home/eins0fx/development/hermes-agent-port/registration_lifecycle.py)
  - `same_registration`: [lines 17-24](file:///home/eins0fx/development/hermes-agent-port/registration_lifecycle.py#L17-L24)
  - `ReplacementLease`: [lines 26-41](file:///home/eins0fx/development/hermes-agent-port/registration_lifecycle.py#L26-L41)
  - `ReplacementCoordinator`: [lines 43-128](file:///home/eins0fx/development/hermes-agent-port/registration_lifecycle.py#L43-L128)

- **Prompt Construction and Session Metadata Consumer**: [`agent/system_prompt.py`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py)
  - `_plugin_session_info`: [lines 162-189](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L162-L189)
  - `_frozen_plugin_prompt_sections`: [lines 191-226](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L191-L226)
  - `_restore_plugin_prompt_sections`: [lines 229-272](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L229-L272)
  - `restore_plugin_prompt_sections`: [lines 274-277](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L274-L277)
  - `_plugin_section_blocks`: [lines 279-285](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L279-L285)

- **Existing Rust Port Status**:
  - [`rust/crates/hermes-gateway/src/plugin_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs): Implements `render_sections`, `Snapshot`, `format`, and `restore`. Contains golden test coverage against Python outputs. Currently lacks the registration API, input validation, `PluginRegistration` handles, ownership tracking, and owner unload.
  - [`rust/crates/hermes-gateway/src/system_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs): Integrates `restore_plugin_sections` into system prompt assembly ([lines 435-445](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/system_prompt.rs#L435-L445)).

- **Relevant Test Suites**:
  - Prompt section unit tests: [`tests/hermes_cli/test_plugin_prompt_sections.py`](file:///home/eins0fx/development/hermes-agent-port/tests/hermes_cli/test_plugin_prompt_sections.py)
  - Prompt section integration & resumption: [`tests/agent/test_plugin_prompt_sections.py`](file:///home/eins0fx/development/hermes-agent-port/tests/agent/test_plugin_prompt_sections.py)
  - Ownership ledger & unload coverage: [`tests/hermes_cli/test_plugin_ownership_ledger.py`](file:///home/eins0fx/development/hermes-agent-port/tests/hermes_cli/test_plugin_ownership_ledger.py)

---

## 2. `register_system_prompt_section` Specification

### 2.1 Public Signature and Semantics
Defined on `PluginContext` in [`hermes_cli/plugins.py:3412-3488`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L3412-L3488):

```python
def register_system_prompt_section(
    self,
    id: str,
    content: Union[str, Callable[[Mapping[str, Any]], str]],
    *,
    position: str = "after_memory",
    max_chars: int = DEFAULT_SYSTEM_PROMPT_SECTION_MAX_CHARS,
) -> PluginRegistration:
```

### 2.2 Rejected Inputs and Validation Rules
All registration validation is **fail-closed** and raises exceptions immediately before any manager mutation occurs:

| Input Field | Permitted Values / Type | Rejected Values (Examples) | Error Type | Exact Exception Message |
| :--- | :--- | :--- | :--- | :--- |
| `id` | `str`, 1-128 chars, matches `^[a-z0-9][a-z0-9._-]{0,127}$` | `""`, `None`, `123`, `"UPPER.case"`, `"has space"`, `"line\nbreak"`, `"-start-hyphen"`, `".dot"`, `"x" * 129` | `ValueError` | `"system prompt section id must be 1-128 lowercase characters using letters, numbers, '.', '_', or '-'"` |
| `content` | `isinstance(content, str) or callable(content)` | `123`, `None`, `{"dict": 1}`, `["list"]`, booleans | `TypeError` | `"system prompt section content must be a string or callable"` |
| `position` | `position in SYSTEM_PROMPT_SECTION_POSITIONS` (`{"after_memory"}`) | `"before_memory"`, `"priority-17"`, `""`, `None` | `ValueError` | `"system prompt section position must be one of: after_memory"` |
| `max_chars` | `int` (strictly excluding `bool`), `1 <= max_chars <= 4000` | `True`, `False`, `0`, `-1`, `4001`, `100.5`, `"4000"`, `None` | `ValueError` | `"system prompt section max_chars must be between 1 and 4000"` |
| Duplicate `id` | Must not be present in `_manager._system_prompt_sections` | Any `id` already registered by the same or another plugin | `ValueError` | `f"system prompt section {id!r} is already registered by plugin {existing.plugin!r}"` |

> [!IMPORTANT]
> **Boolean Guard Requirement**: In Python, `isinstance(True, int)` evaluates to `True`. The Python code explicitly guards `isinstance(max_chars, bool) or not isinstance(max_chars, int)`. The Rust port must ensure strongly-typed integer ingestion (e.g. `usize` or `u32`) that rejects boolean or float deserialization, with `1 <= max_chars <= 4000`.

### 2.3 Registration Storage and Lease Acquisition
Upon passing validation:
1. `plugin_id = self.manifest.key or self.manifest.name`.
2. A `PluginSystemPromptSection` dataclass instance is constructed:
   ```python
   section = PluginSystemPromptSection(
       id=id,
       content=content,
       position=position,
       max_chars=max_chars,
       plugin=plugin_id,
   )
   ```
3. Stored in manager mapping: `self._manager._system_prompt_sections[id] = section`.
4. Replaceable lease acquired via `_track_replacement`:
   - `kind`: `"system_prompt_section"`.
   - `key`: `id`.
   - `slot`: `("manager_mapping", builtins.id(self._manager._system_prompt_sections), id)`.
   - `current`: `section`.
   - `previous`: `None` (guaranteed by duplicate rejection).
   - `restore`: closure invoking `self._manager._restore_mapping(self._manager._system_prompt_sections, id, section, replacement)`.
5. Enrolled in `self._ownership_ledger[plugin_id]` and `self._registration_order`.
6. Live `PluginRegistration` handle is returned.

---

## 3. Render-Time Execution, Budgeting, and Invalidation Invariants

Prompt sections are rendered deterministically and lazily when the session prompt is built. The render pipeline is specified in [`hermes_cli/plugins.py:5917-6005`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L5917-L6005) and mirrored in [`rust/crates/hermes-gateway/src/plugin_prompt.rs:19-68`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs#L19-L68).

### 3.1 Ordering and Determinism
- **Lexicographical ID Sorting**: Sections are evaluated strictly in ascending alphabetical order of `section.id` (`sorted(self._system_prompt_sections)`). Discovery order and registration order have no effect on prompt section ordering.
- In Rust, storing or collecting into `BTreeMap<String, ...>` preserves this invariant.

### 3.2 Render-Time Filter and Budget Pipeline
Unlike registration-time checks (which raise exceptions), render-time violations operate **fail-open**: broken or oversized sections are logged and skipped, allowing healthy sections to proceed.

```
Registered Sections (Sorted by ID)
  │
  ├─► [1] Section Count >= 32? ─────────────────────► Skip (Warning logged)
  │
  ├─► [2] Evaluate Callable (or read string)
  │         └─► Raises exception? ──────────────────► Skip (Warning logged)
  │
  ├─► [3] Value is str? ────────────────────────────► Skip if not str (Warning logged)
  │
  ├─► [4] text = value.strip(); text is empty? ─────► Skip silently (No warning)
  │
  ├─► [5] text contains START or END markers? ──────► Skip (Warning logged)
  │
  ├─► [6] char_count(text) > section.max_chars? ────► Skip entirely (Warning logged; no truncate)
  │
  ├─► [7] total_chars + formatted_size > 8,000? ────► Skip (Warning logged)
  │
  └─► Valid Section Emitted; total_chars += formatted_size
```

1. **Section Count Limit (`MAX_SYSTEM_PROMPT_SECTIONS = 32`)**: If `len(rendered) >= 32`, log warning: `"Plugin system prompt section %s exceeded the section-count budget (%d) and was skipped"`, and skip.
2. **Callback Exception Isolation**: If `section.content(frozen_info)` raises an exception, catch `Exception`, log warning: `"Plugin system prompt section %s (%s) raised and was skipped: %s"`, and skip.
3. **Return Type Enforcement**: If return value is not `str`, log warning: `"Plugin system prompt section %s (%s) returned %s, not str; skipped"`, and skip.
4. **Whitespace Stripping & Empty Omission**: `text = value.strip()`. If `text` is empty, skip silently without warning.
5. **Persistence Marker Anti-Spoofing**: If `<!-- hermes-plugin-sections:start -->` or `<!-- hermes-plugin-sections:end -->` appears in `text`, log warning: `"Plugin system prompt section %s (%s) contained a reserved persistence marker and was skipped"`, and skip.
6. **Per-Section Character Limit**: If `len(text) > section.max_chars`, log warning: `"Plugin system prompt section %s (%s) exceeded max_chars (%d > %d) and was skipped"`, and skip. (The section is completely omitted, never truncated).
7. **Aggregate Character Limit (`MAX_SYSTEM_PROMPT_SECTIONS_TOTAL_CHARS = 8_000`)**:
   - Initial `total_chars` baseline: `len(PLUGIN_SECTIONS_START) + len(PLUGIN_SECTIONS_END) + 2` (72 characters).
   - Formatted section size: `len(format_system_prompt_section(section.id, text))`.
   - Inter-section separator: `+ 2` characters (`\n\n`) for all sections after the first.
   - If `total_chars + rendered_chars > 8_000`, log warning: `"Plugin system prompt section %s (%s) exceeded the aggregate session budget (%d chars) and was skipped"`, and skip.
   - **Greedy Fitting Invariant**: Skipping an oversized section does **not** terminate rendering; subsequent smaller sections that fit within the remaining aggregate budget must still be evaluated and included.

### 3.3 Prompt Lifecycle and Resumption Semantics
Specified in [`agent/system_prompt.py:191-285`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L191-L285):
- **Frozen Per-Session Snapshot**: On a new session, sections are rendered once and stashed on the agent as `_plugin_system_prompt_sections_snapshot`. Subsequent turns reuse this frozen snapshot without re-executing plugin code.
- **Rebuild Boundary Re-rendering**: When `invalidate_system_prompt(agent)` is invoked (e.g. during context compaction), `_plugin_system_prompt_sections_previous` captures the previous snapshot and `_plugin_system_prompt_sections_snapshot` is cleared. A subsequent prompt assembly re-renders the sections. If re-rendering fails, it falls back to `_plugin_system_prompt_sections_previous` (fail-open) rather than omitting sections.
- **Restored Session Immunity**: When resuming an existing session from `SessionDB`, `_restore_plugin_prompt_sections` extracts the exact bytes from the stored prompt. **Plugin callbacks are never executed during session resumption.**

---

## 4. Callback Session Metadata Semantics

When dynamic callables are registered (`Callable[[Mapping[str, Any]], str]`), they receive a session metadata dictionary at render time.

### 4.1 Immutability Contract
- In Python, `render_system_prompt_sections` wraps the input dictionary with `types.MappingProxyType(dict(session_info))`.
- **Read-Only Invariant**: Any attempt by a plugin callback to mutate the session dictionary (e.g. `info["session_id"] = "tampered"`) raises `TypeError`.
- A defensive copy (`dict(session_info)`) is made before wrapping, ensuring caller state cannot be modified through shared references.
- In Rust, this maps to passing an immutable reference (`&SessionMetadata` or `&BTreeMap<String, String>`).

### 4.2 Canonical Metadata Keys
Extracted by [`agent/system_prompt.py:_plugin_session_info`](file:///home/eins0fx/development/hermes-agent-port/agent/system_prompt.py#L162-L189):

| Metadata Key | Rust Type | Source / Resolution Logic | Fallback Value |
| :--- | :--- | :--- | :--- |
| `session_id` | `String` | `getattr(agent, "session_id", None)` | `""` |
| `model` | `String` | `getattr(agent, "model", None)` | `""` |
| `provider` | `String` | `getattr(agent, "provider", None)` | `""` |
| `platform` | `String` | `getattr(agent, "platform", None)` | `""` |
| `profile_name` | `String` | Agent home resolution (`_agent_home(agent)`); fallback to `get_active_profile_name()` | `"default"` |
| `cwd` | `String` | `resolve_context_cwd()` | `""` |

All values are guaranteed to be strings. `None` values are normalized to empty strings or default values.

---

## 5. `PluginRegistration` Lifecycle and Ownership Ledger

### 5.1 The `PluginRegistration` Dataclass
Defined in [`hermes_cli/plugins.py:1230-1271`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L1230-L1271):

- `kind: str`: Registration category (`"system_prompt_section"`, `"tool"`, `"platform"`, `"hook"`, `"middleware"`, `"command"`, `"cli_command"`, `"skill"`, `"approval_transport"`, `"on_unload"`, `"background_task"`).
- `key: str`: Unique identifier of the registered entity.
- `release: Callable[[], None]`: Host-owned inverse cleanup function.
- `plugin_key: str`: Canonical key of the declaring plugin (`manifest.key or manifest.name`).
- `persistent: bool`: If `True`, process-global host infrastructure (e.g. dashboard auth provider) that survives routine per-home manager unloads. Prompt sections have `persistent=False`.
- `_disposed: bool`: Initialized to `False`. Tracks whether disposal has executed.
- `_on_dispose: Optional[Callable[[PluginRegistration], None]]`: Internal notification hook installed by the manager.
- `active: bool`: Read-only property returning `not self._disposed`.

### 5.2 Disposal Invariants
- **Idempotency**:
  ```python
  def dispose(self) -> None:
      if self._disposed:
          return
      self._disposed = True
      try:
          self.release()
      finally:
          if self._on_dispose is not None:
              self._on_dispose(self)
  ```
  Repeated calls to `dispose()` on the same handle are safe no-ops.
- **Cleanup Guarantee**: `_on_dispose` executes inside a `finally` block, ensuring ledger tracking is forgotten even if `release()` raises.
- **Exception Isolation**: In bulk unloads ([lines 4010-4026](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L4010-L4026)), individual disposal failures are caught and logged at `WARNING` (with traceback if `_PLUGINS_DEBUG`), never aborting teardown for remaining registrations.

### 5.3 Ownership Tracking Structures
The `PluginManager` maintains two synchronized structures:
1. `_ownership_ledger: Dict[str, List[PluginRegistration]]`: Maps `plugin_key` to all active registrations owned by that plugin. Used for targeted unloads.
2. `_registration_order: List[PluginRegistration]`: Flat list of all non-persistent registrations in chronological acquisition order across all plugins. Used for global reverse-order teardown.
3. `_persistent_carryover: List[PluginRegistration]`: Holds active `persistent=True` registrations that survived an unload-all, waiting for re-discovery to reconcile or evict them.

When a registration is disposed, `_forget_registrations` removes its exact object identity from `_registration_order` and `_ownership_ledger`. If a plugin has zero remaining registrations, its key is popped from `_ownership_ledger`.

---

## 6. Stale Handles After Same-ID Re-Registration

A critical invariant of the registration system is that an older, stale handle must never evict, mutate, or resurrect state when a newer registration with the same identifier is active.

### 6.1 Collision and Re-Registration Scenarios
1. **Concurrent Duplicate Registration**:
   - If Plugin A registers section `"metrics"` and later Plugin B (or Plugin A again) attempts to register section `"metrics"` while the first is still active:
   - `register_system_prompt_section` checks `existing = self._manager._system_prompt_sections.get(id)`.
   - `existing is not None` -> Raises `ValueError("system prompt section 'metrics' is already registered by plugin '...'")`.
   - The second registration is rejected before acquiring any lease or altering ledger state.
2. **Re-Registration After Unload / Disposal**:
   - Plugin A registers section `"metrics"` -> `section_v1`, `handle_v1`, `lease_v1`.
   - Plugin A unloads or `handle_v1.dispose()` is called:
     - `_system_prompt_sections.pop("metrics")` removes `section_v1`.
     - `handle_v1._disposed` becomes `True`.
     - `lease_v1.active` becomes `False`.
   - Plugin A (or Plugin B) registers section `"metrics"` again -> `section_v2`, `handle_v2`, `lease_v2`.
   - Now an external caller invokes `handle_v1.dispose()` (the stale handle).

### 6.2 The Four Layers of Stale Handle Protection
The architecture employs four distinct defense layers to guarantee that `handle_v1.dispose()` cannot corrupt `section_v2`:

```
Caller calls stale handle_v1.dispose()
  │
  ▼
[Layer 1: Handle Idempotency Guard]
  ├─► handle_v1._disposed == True? ──────────► Early return. No-op.
  │
  ▼ (If bypassed or uninitialized)
[Layer 2: Lease Active Guard]
  ├─► lease_v1.active == False? ─────────────► Early return. No-op.
  │
  ▼ (If lease appeared active)
[Layer 3: Generational Top-of-Stack Check]
  ├─► latest live lease in slot is lease_v2.
  ├─► latest is lease_v1 == False!
  └─► lease_v1.restore(...) is NEVER CALLED.
      lease_v1 is marked inactive and pruned.
  │
  ▼ (Even if restore callback was somehow invoked)
[Layer 4: CAS / Object Identity Verification]
  ├─► _restore_mapping(mapping, "metrics", current=section_v1, previous=None)
  ├─► mapping.get("metrics") is section_v2 (NOT section_v1).
  └─► Identity comparison fails: returns False. Mapping untouched.
```

1. **Layer 1: Handle-Level Idempotency**:
   `handle_v1._disposed` is set to `True` during its first disposal. Any subsequent call returns immediately at [line 1263](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L1263).
2. **Layer 2: Lease-Level Active Check**:
   [`ReplacementCoordinator.dispose`](file:///home/eins0fx/development/hermes-agent-port/registration_lifecycle.py#L91): `if not lease.active: return`. An already-retired lease returns immediately without acquiring coordinator locks or invoking restore hooks.
3. **Layer 3: Generational Predecessor Chain**:
   In [`ReplacementCoordinator.dispose`](file:///home/eins0fx/development/hermes-agent-port/registration_lifecycle.py#L94-L115):
   ```python
   latest = next((candidate for candidate in reversed(leases) if candidate.active), None)
   lease.active = False
   if latest is lease:
       # Only the latest live generation may mutate the slot!
       ...
       lease.restore(replacement)
   ```
   If an older lease is disposed while a newer generation is active, `latest is lease` evaluates to `False`. The coordinator **never invokes the restore callback** for superseded generations; it merely marks the old lease inactive and prunes it from the slot's generation history.
4. **Layer 4: Compare-And-Swap (CAS) Identity Guard**:
   In [`PluginManager._restore_mapping`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L3941-L3956):
   ```python
   def _restore_mapping(self, mapping, key, current, previous):
       if mapping.get(key) is not current:
           return False
       if previous is None:
           mapping.pop(key, None)
       else:
           mapping[key] = previous
       return True
   ```
   Even if the coordinator invoked the restore closure, `current` is captured as `section_v1`. The current mapping entry is `section_v2`. Because `section_v2 is not section_v1`, the CAS check fails, and `mapping.pop` is aborted.

---

## 7. Owner Unload Invariants (Targeted vs. Unload-All)

The unload subsystem handles single plugin teardown and full manager resets via `PluginManager.unload(plugin)` ([lines 4037-4195](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L4037-L4195)).

### 7.1 Reverse Acquisition Order
Registrations must be disposed in **strict reverse order of acquisition**:
```python
for registration in reversed(registrations):
    registration.dispose()
```
- Ensures dependent registrations clean up before their prerequisites.
- Overridden entities are restored before the base entity is removed.
- `on_unload` callbacks and background task cancellations are tracked directly as registrations in `_registration_order`. They execute interleaved with tool, platform, hook, and prompt section cleanups at the precise reverse moment of their registration.

### 7.2 Unload-All (`plugin is None`)
Used during full manager destruction and `discover_and_load(force=True)`:
1. Gathers all registrations from `_registration_order`.
2. **Persistent Preservation**: Registrations with `persistent=True` (e.g. process-global dashboard auth providers) are excluded from `_registration_order` and parked in `_persistent_carryover` ([lines 4157-4167](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L4157-L4167)).
3. Reverse disposal of all non-persistent registrations.
4. Clears manager-local maps:
   `_ownership_ledger`, `_plugins`, `_hooks`, `_middleware`, `_plugin_tool_names`, `_plugin_platform_names`, `_cli_commands`, `_plugin_commands`, `_plugin_skills`, `_portable_mcp_servers`, `_aux_tasks`, `_system_prompt_sections`, `_approval_transports`, `_slack_action_handlers`, `_predeclared_modules`, `_predeclared_tools`, `_platform_handler_factories`.
5. Sweeps pre-ledger tools and platform names from process-global registries to prevent leaks.
6. Sets `_discovered = False`.

### 7.3 Targeted Unload (`plugin is not None`)
Used when an individual plugin is disabled or uninstalled:
1. Target key resolution: resolves `plugin` argument via exact match in `_ownership_ledger` or `_plugins`, falling back to `manifest.name` or `plugin_key`.
2. Gathers registrations matching `registration.plugin_key in target_keys` from `_registration_order`.
3. **Targeted Persistent Eviction**: Unlike unload-all, targeted unload **does** collect active `persistent=True` registrations owned by the target plugin from `_ownership_ledger` ([lines 4094-4099](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L4094-L4099)), because an uninstalled/disabled plugin must not leave behind process-global auth providers.
4. Disposes gathered registrations in reverse acquisition order.
5. Forgets gathered registrations from `_registration_order` and `_ownership_ledger`.
6. Removes target keys from `self._plugins`.
7. **Isolation Invariant**: Targeted unload never clears un-targeted plugin entries, does not clear manager maps, and does not sweep un-targeted process-global registrations.

### 7.4 Re-Discovery Reconciliation of Persistent Entries
In `discover_and_load(force=True)`, after `unload()` parks persistent handles in `_persistent_carryover`:
- `_evict_stale_persistent_registrations` ([lines 3877-3918](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/plugins.py#L3877-L3918)) runs after re-discovery.
- If a re-discovered plugin re-registered the same `(kind, key)`, the new registration rotated the provider in place. The old parked handle is discarded **without calling dispose** (avoiding unregistering the shared singleton).
- If the plugin is no longer present (disabled or removed), the parked handle is disposed, cleanly unregistering the provider.

---

## 8. Rust Registry Preservation Blueprint

The existing Rust implementation in [`rust/crates/hermes-gateway/src/plugin_prompt.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/plugin_prompt.rs) already provides `render_sections`, `Snapshot`, `format`, and `restore`. To complete the port of the plugin registration and ownership lifecycle, the following architectural invariants must be preserved:

### 8.1 Required Type Contracts

```rust
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

pub type ReleaseFn = Box<dyn FnOnce() + Send + Sync>;
pub type SectionCallback = Box<dyn Fn(&SessionMetadata) -> Result<String, String> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct SessionMetadata {
    pub session_id: String,
    pub model: String,
    pub provider: String,
    pub platform: String,
    pub profile_name: String,
    pub cwd: String,
}

pub enum SectionContent {
    Static(String),
    Dynamic(SectionCallback),
}

pub struct SystemPromptSection {
    pub id: String,
    pub content: SectionContent,
    pub position: String,
    pub max_chars: usize,
    pub plugin_key: String,
    pub generation: u64,
}

pub struct RegistrationHandle {
    pub kind: &'static str,
    pub key: String,
    pub plugin_key: String,
    pub generation: u64,
    pub persistent: bool,
    disposed: AtomicBool,
    release: Option<ReleaseFn>,
}

impl RegistrationHandle {
    pub fn is_active(&self) -> bool {
        !self.disposed.load(Ordering::Acquire)
    }

    pub fn dispose(&mut self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return; // Idempotent: already disposed
        }
        if let Some(release) = self.release.take() {
            release();
        }
    }
}
```

### 8.2 Invariant Mapping Summary

| Subsystem Requirement | Python Behavior | Rust Implementation Strategy |
| :--- | :--- | :--- |
| **Section ID Validation** | Regex `^[a-z0-9][a-z0-9._-]{0,127}$` | Pre-compiled `Regex` validating length 1..=128 and charset. Return `Result<..., RegistrationError::InvalidId>`. |
| **Duplicate ID Rejection** | `existing is not None` -> `ValueError` | Check `map.contains_key(&id)`. Return `Err(RegistrationError::DuplicateId)`. |
| **Position & Max Chars** | `"after_memory"`, `1 <= max_chars <= 4000` | Match `enum Position { AfterMemory }` and check `(1..=4000).contains(&max_chars)`. |
| **Generational Monotonicity** | `ReplacementCoordinator` generations | Increment a monotonic `AtomicU64` generation counter per registration slot. |
| **Stale Handle Disposal** | Disposing older handle cannot pop newer entry | When `RegistrationHandle::dispose()` runs, verify `stored_section.generation == handle.generation`. If mismatched, do not remove. |
| **Idempotent Handle Disposal** | Repeated `.dispose()` is no-op | Atomic `disposed.swap(true, Ordering::AcqRel)` ensures `release` executes at most once. |
| **Reverse Unload Order** | `for reg in reversed(registrations)` | Maintain `Vec<Arc<Mutex<RegistrationHandle>>>` in registration order; iterate `.iter().rev()`. |
| **Exception Isolation** | `try ... except Exception` around each dispose | Wrap `release()` invocation in `std::panic::catch_unwind` and log errors at `warn!`. |
| **Session Metadata Immutability** | `MappingProxyType(dict(session_info))` | Pass immutable borrowed reference `&SessionMetadata` to callbacks. |
| **Render-Time Budgeting** | Bounded count (32), chars (8000), fail-open | Retain existing logic in `plugin_prompt::render_sections`. |
| **Resumption Immunity** | Restores from prompt bytes; never calls plugins | Retain existing logic in `plugin_prompt::restore` and `Snapshot`. |
