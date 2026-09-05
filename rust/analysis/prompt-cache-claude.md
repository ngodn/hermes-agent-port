# Prompt-cache key routing port

Ports the prompt-cache routing logic shared by the two OpenAI-compatible
transports into `crates/hermes-gateway/src/prompt_cache.rs`, with inline tests
driven by Python-executed goldens.

## What was ported

From `agent/transports/codex.py`:
- `_cache_scope_from_session_id` -> `cache_scope_from_session_id`
- `_bounded_prompt_cache_key` -> `bounded_prompt_cache_key`
- `_content_cache_key` -> `content_cache_key`

From `agent/transports/chat_completions.py`:
- `_static_prompt_instructions` -> `static_prompt_instructions`
- `_add_prompt_cache_key` -> public `apply`

`apply` is the transport entry point. It mutates SDK-shaped `api_kwargs`
(a `serde_json::Value` object) in place BEFORE `extra_body` is flattened onto the
wire request, exactly like the Python transport. Signature:

```rust
pub fn apply(
    api_kwargs: &mut Value,
    messages: &[Value],
    tools: Option<&Value>,
    supports: bool,
    session_id: Option<&str>,
    cache_scope_id: Option<&str>,
)
```

## Shared helpers reused

From `python_value`: `python_repr`, `python_number`, `python_whitespace`,
`decimal_digit`, `truthy`. No new scalar coercions were added. `python_str`
(Python `str()`, which differs from `repr()` only for bare strings) is a thin
local wrapper over `python_repr`.

## Behaviours pinned

### Canonical JSON hash

The content hash must match CPython byte-for-byte, so `canonical_json`
reproduces `json.dumps(v, sort_keys=True, ensure_ascii=False,
separators=(",", ":"))`:

- No whitespace between tokens; `:` and `,` separators only.
- Object keys sorted by Unicode code point. Rust `str` `Ord` is UTF-8 byte
  order, which equals code-point order, so a plain `sort()` is faithful.
- `ensure_ascii=False`: non-ASCII emitted literally; only `"`, `\`, and the C0
  control range are escaped, with short escapes for `\b \t \n \f \r`. 0x7f and
  all non-ASCII pass through unescaped (matches CPython `c_encode_basestring`).
- Float spelling via `python_number` (Python `repr`): the exponent switch at
  `abs < 1e-4` / `abs >= 1e16`, and the `e{+03}` exponent formatting
  (`1e+16`, `1e-05`). Integer vs float is preserved from the parsed value
  (`1` stays `1`, `1.0` stays `1.0`), matching `json.loads`/`json.dumps`.

### Tool sorting

Tools are filtered to objects (`isinstance(t, dict)`), then STABLY sorted by the
string form of `t.get("name") or t.get("type") or ""` (Python truthiness via
`truthy`, string form via `python_str`). This is the TOP-level `name`/`type`,
never a nested `function.name`. A pinned test uses tools whose nested
`function.name` order is the reverse of their top-level `name` order to prove
the top-level key is what decides the hash input. `sort_by` is stable, matching
CPython `sorted`.

### Cron scope regex

`cache_scope_from_session_id` reproduces
`re.match(r"^(cron_.+)_\d{8}_\d{6}$", str(session_id or ""))` by hand, since the
`regex` crate's `$` and `.`/`\d` semantics differ from CPython's and adding a
dependency is out of scope. CPython behaviours preserved:

- `.` never crosses a newline (a newline inside the `.+` region fails the match).
- `\d` matches any Unicode decimal digit (`decimal_digit`), not just ASCII.
  Tested with Arabic-Indic digits.
- `$` (non-MULTILINE) matches at end of string OR immediately before a single
  trailing `\n`. Two trailing newlines do not match.
- No `main`/`child` stripping: only the cron per-fire timestamp is removed; every
  other session id passes through unchanged.

The suffix `_\d{8}_\d{6}` is 16 chars and is anchored at `$`, so it is forced to
the tail and greedy `.+` becomes deterministic. That lets the matcher compute the
capture without full backtracking.

### apply / _add_prompt_cache_key

- A caller-supplied `prompt_cache_key` is authoritative and is honored even when
  `supports` is false (the caller branch returns before the `supports` check).
- Top-level and `extra_body` keys are bounded (or removed when they strip to
  empty) SEPARATELY, and both are handled when present.
- No autogeneration after an explicit key was removed for being empty: the
  caller branch always returns.
- `cache_scope_id or session_id`: a non-empty `cache_scope_id` takes precedence,
  else the physical `session_id`, else `""`.
- `extra_body` present but not an object, with no top-level key, falls through to
  the autogenerate path (it is not treated as a caller-supplied key).

## Limitations recorded

- **Unbounded integers.** CPython ints are arbitrary precision. `serde_json`
  numbers are bounded to `i64`/`u64` (this crate does not enable
  `arbitrary_precision`). `u64` is preserved where it fits; an integer larger
  than `u64::MAX` cannot be represented as a `serde_json::Number` and so cannot
  round-trip through a caller cache key or a tool schema. The goldens therefore
  do not exercise such values. `python_number` renders any representable
  `i64`/`u64` via `to_string`, matching Python `repr(int)`.
- **Lone surrogates.** Python's `str.encode("utf-8", errors="replace")` exists to
  handle lone surrogate code points (U+D800..U+DFFF), which a Rust `String`
  cannot hold at all. That replacement path is unreachable in Rust, and the
  goldens cannot cover surrogate inputs. For all valid Rust strings the SHA-256
  input is just the string's UTF-8 bytes, identical to CPython.
- **NaN / Infinity.** `json.dumps` emits `NaN`/`Infinity`/`-Infinity` by default,
  but such values cannot exist in a `serde_json::Value` (they parse to `Null`),
  so they are not a concern for JSON-sourced request bodies and are not modelled.

## Goldens

`rust/tools/gen_prompt_cache_goldens.py` extracts the five functions (plus the
`_CRON_SESSION_ID_RE` assignment) from the two transport source files with `ast`
and execs just those into one namespace, so the fixture tracks the real source
rather than a paraphrase. `_add_prompt_cache_key` lazily imports the three codex
helpers, so a stub `agent.transports.codex` module (backed by the exec'd
functions) is registered in `sys.modules`. Runs under mise Python 3.12.13:

```
mise x python@3.12.13 -- python rust/tools/gen_prompt_cache_goldens.py         # write
mise x python@3.12.13 -- python rust/tools/gen_prompt_cache_goldens.py --check # verify
```

`rust/tools/prompt-cache-goldens.json` holds 65 cases across five sections
(`scope`, `bounded`, `static`, `content`, `apply`). `--check` round-trips
cleanly. The inline tests in `prompt_cache.rs` replay every section and add
focused unit tests for the behaviours above.

## Integration note

The main agent registered this module and wired apply into native request
projection. The temporary dead-code allowance was removed. See
[prompt-cache-verification.md](prompt-cache-verification.md) for integration
coverage and remaining session-resolution boundaries.
