# Managed State and Local Vision Capabilities Source Review

This document specifies the exact runtime semantics and behavioral contracts of the Python managed runtime implementation across `endpoint.py`, `capabilities.py`, `bootstrap.py`, `catalog.py`, `binaries.py`, `supervisor.py`, and `growth.py`. It provides verified findings for implementing `managed_capabilities.rs` in Rust without relying on proposed API names or invented manifest behaviors.

Scope and authority:
- `hermes_cli/local_runtime/endpoint.py`: lines 28-84 (`_pid_alive`, `_state_endpoint`)
- `hermes_cli/local_runtime/capabilities.py`: lines 30-118 (`ACCEPTED_IMAGE_MIMES`, `is_managed_provider`, `_props_modalities`, `managed_model_supports_vision`)
- `hermes_cli/local_runtime/bootstrap.py`: lines 51-97 (`models_dir`, `assets_dir`, `staged_models`, `staged_model_ids`)
- `hermes_cli/local_runtime/catalog.py`: lines 80-124, 356-396, 458-464 (`AssetFile`, `QuantVariant`, `find_entry_for_model`)
- `hermes_cli/local_runtime/binaries.py`: lines 70-79 (`runtimes_root`)
- `hermes_cli/local_runtime/supervisor.py`: lines 42-45, 203-225 (`state_path`, `_write_state`)
- `hermes_cli/local_runtime/growth.py`: lines 61-72 (`is_managed_endpoint`)
- `hermes_constants.py`: lines 53-59, 218-262 (`get_default_hermes_root`)

---

## 1. Directory Hierarchy and State File Location

### 1.1 Machine-Scoped Root Resolution
- Root directory: `get_default_hermes_root()` in `hermes_constants.py:218`.
  - POSIX default: `~/.hermes`
  - Windows default: `%LOCALAPPDATA%/hermes` (falling back to `~/AppData/Local/hermes`)
  - If `HERMES_HOME` is set and points outside native home: uses `HERMES_HOME` (or grandparent if under a `profiles/<name>` subdirectory).
- Local runtime binary root: `runtimes_root()` in `binaries.py:70` resolves to `<default_hermes_root>/runtimes/llamacpp`.
- Models directory: `models_dir()` in `bootstrap.py:51` resolves to `<default_hermes_root>/models`.
- Companion assets directory: `assets_dir()` in `bootstrap.py:60` resolves to `<default_hermes_root>/models/assets`.

### 1.2 State File Path and Structure
- State file path: `state_path()` in `supervisor.py:42` resolves to `<default_hermes_root>/runtimes/llamacpp/server.json`.
- Written by: `LlamaServerSupervisor._write_state()` in `supervisor.py:218`.
- JSON schema:
  - `base_url` (string): e.g. `"http://127.0.0.1:18434/v1"`
  - `api_key` (string): e.g. 24-byte urlsafe secret token
  - `pid` (integer or null): PID of the supervised child `llama-server` process

---

## 2. State Endpoint Resolution and Process Liveness

### 2.1 Process Liveness Probe (`_pid_alive`)
- Signature: `_pid_alive(pid: int) -> bool` (`endpoint.py:28-42`)
- Input validation: If `not pid or pid < 0`, immediately returns `False` (PID 0 or negative is dead/invalid).
- Python behavior: Uses `psutil.pid_exists(pid)`. If `psutil` raises an exception or is unavailable, falls back optimistically to `True`.
- Platform warning: On Windows, `os.kill(pid, 0)` terminates the target process, so it must never be used as a probe. On POSIX, sending signal 0 via `libc::kill(pid, 0)` is safe.

### 2.2 State Endpoint Resolution (`_state_endpoint`)
- Implementation: `endpoint.py:45-84`
- Early exits before any network call:
  1. If `server.json` does not exist: returns `None`.
  2. If file reading or JSON decoding fails (`json.JSONDecodeError`, `OSError`): returns `None`.
  3. If `state.get("base_url")` is empty or missing: returns `None`.
- Endpoint candidate dictionary: `{"base_url": base_url, "api_key": state.get("api_key", "")}`.
- PID evaluation: `pid_ok = _pid_alive(int(state.get("pid") or 0))`.

### 2.3 Request Order with Dead PID
- Exact execution sequence:
  1. `pid_ok` is computed.
  2. An HTTP GET request to `health = base_url.rsplit("/v1", 1)[0] + "/health"` is issued with `timeout=3`.
  3. If `/health` responds with HTTP 200:
     - Returns `endpoint if pid_ok else None`.
     - When `pid_ok` is `False`, this branch returns `None`.
  4. If `/health` fails with `URLError`, `OSError`, or `TimeoutError`:
     - Exception is caught and ignored.
     - Fall-through: `if pid_ok: return endpoint; return None`.
     - When `pid_ok` is `False`, this branch returns `None`.
- Verified finding: When `pid_ok` is `False`, `_state_endpoint()` unconditionally returns `None` regardless of `/health` outcome. The `/health` network request is executed prior to the final dead-PID branch. Its design purpose is an ownership tiebreaker: an HTTP 200 from a server whose recorded PID is dead indicates that an independent process or another install owns that port, so the state file cannot be trusted.

### 2.4 Healthy versus Starting Server
- **Healthy Server**:
  - `pid_ok` is `True` and GET `/health` returns HTTP status 200 within 3 seconds.
  - `_state_endpoint()` returns the endpoint dictionary.
  - In `capabilities.py`, `_props_modalities(model_id)` connects to `{base}/props?model={model_id}`.
  - The response modalities block (`props.get("modalities")`) is evaluated. If present and containing `"vision"`, returns `bool(modalities["vision"])`.
  - Crucial rule: Live verdict is authoritative. If `/props` returns `false`, `managed_model_supports_vision` returns `False` directly. It does not fall through to the catalog.
- **Starting Server**:
  - `server.json` is written at spawn time in `_spawn()` before `_wait_health()` passes.
  - When probed while starting:
    - `/health` fails (connection refused or connect timeout).
    - Exception is caught.
    - Fall-through checks `if pid_ok: return endpoint`.
    - Because `pid_ok` is `True`, `_state_endpoint()` returns `endpoint` optimistically.
  - In `capabilities.py`:
    - `_props_modalities(model_id)` tries to connect to `{base}/props?model={model_id}`.
    - The connection fails because `llama-server` is still initializing and not yet listening.
    - Exception is caught and `_props_modalities()` returns `None`.
    - Because `live is None`, `managed_model_supports_vision` falls back to the catalog entry and companion asset file check in `assets_dir()`.
- Contract summary:
  - Healthy server: `/props` modality check wins for both `True` and `False`.
  - Starting server: Optimistically detected via live PID; `/props` fails; vision support is answered from catalog declaration plus `assets/` disk existence.

---

## 3. Provider and Capability Verification

### 3.1 Managed Provider Predicate (`is_managed_provider`)
- Implementation: `capabilities.py:41-55`
- Alias set: `_LLAMACPP_ALIASES = frozenset({"llamacpp", "llama.cpp", "llama-cpp"})`.
- Normalization: `p = (provider or "").strip().lower()`.
- Branch 1: If `p in _LLAMACPP_ALIASES`: returns `True` immediately, regardless of `base_url` (even if `base_url` is empty, None, or unparseable).
- Branch 2: If `p == "custom"` and `base_url` is non-empty:
  - Calls `growth.is_managed_endpoint(base_url)`.
  - In `is_managed_endpoint(base_url)` (`growth.py:61-72`):
    - Obtains state from `_state_endpoint()`.
    - Returns `False` if `state is None`.
    - Compares `(base_url or "").rstrip("/") == str(state.get("base_url", "")).rstrip("/")`.
    - Trailing slashes are stripped from both URLs; casing is preserved.
- Branch 3: All other cases (e.g. `p == "custom"` with empty `base_url`, `"ollama"`, `"openai"`): returns `False`.

### 3.2 Live Probe Query Construction (`_props_modalities`)
- Implementation: `capabilities.py:58-79`
- Target URL: `f"{base}/props?model={model_id}"` where `base = state["base_url"].rsplit("/v1", 1)[0]`.
  - If `base_url` is `"http://127.0.0.1:18434/v1"`, `base` is `"http://127.0.0.1:18434"`.
- Raw query string semantics:
  - `model_id` is interpolated directly into the query string without `urllib.parse.quote` or percent-encoding.
  - Query parameter key is `model`.
- Headers: `{"Authorization": f"Bearer {state.get('api_key', '')}"}`.
  - If `api_key` is empty, sends `"Bearer "`.
- Timeout: 3 seconds.
- JSON extraction:
  - Reads `props = json.load(r)`.
  - Reads `modalities = props.get("modalities")`.
  - If `isinstance(modalities, dict) and "vision" in modalities`: returns `bool(modalities["vision"])`.
  - Otherwise returns `None`.

### 3.3 Managed Vision Resolution Flow (`managed_model_supports_vision`)
- Implementation: `capabilities.py:82-118`
- Execution waterfall:
  1. Input validation:
     - `if not model_id: return None`.
  2. Staging gate:
     - Checks `if model_id not in staged_model_ids(): return None`.
     - Staging check occurs before any network request. If model is not staged, returns `None` without contacting `/health` or `/props`.
  3. Live inspection:
     - `live = _props_modalities(model_id)`.
     - `if live is not None: return live`.
     - Explicit `False` wins: If `live` is `False`, returns `False` immediately.
  4. Catalog fallback (executed only when `live is None`):
     - `hit = find_entry_for_model(model_id)`.
     - If `hit is None`: returns `None` (model unknown to catalog, fall through to caller).
     - `entry = hit[0]`.
     - If `entry.mmproj is None`: returns `False` (known model declares no vision projector).
     - If `entry.mmproj` exists: returns `(assets_dir() / entry.mmproj.local_name).exists()`.
     - Returns `True` if the companion projector file exists on disk, `False` if missing.

---

## 4. Filesystem Staging and Glob Semantics

### 4.1 Shallow Glob and Directory / Symlink Semantics (`staged_models`)
- Implementation: `bootstrap.py:67-91`
- Directory scan: `files = sorted(models_dir().glob("*.gguf"))`.
- Shallow search: `Path.glob("*.gguf")` only searches the direct children of `models_dir()`. Subdirectories such as `assets/` and `nested/` are ignored.
- Directory handling:
  - `Path.glob("*.gguf")` yields all filesystem directory entries whose names end with `.gguf`.
  - `staged_models()` does not call `p.is_file()` or `not p.is_dir()`.
  - A directory named `dir.gguf` (or `dir.gguf/`) is matched by `glob("*.gguf")` and treated as a staged model with stem `dir`. (Verified in test fixture: `["dir.gguf/"] -> ["dir"]`).
- Symlink handling:
  - Symlinks matching `*.gguf` (pointing to files or directories, including dangling links) are returned by `Path.glob("*.gguf")`.
- Hidden files:
  - A file named `.hidden.gguf` matches `*.gguf` and yields stem `".hidden"`.
- Sorting and Duplicates:
  - Glob results are sorted lexicographically by full Path before filtering.
  - Duplicates are preserved in list output (e.g. `a.gguf` and `a-00001-of-00001.gguf` both produce `"a"` in `staged_model_ids()`).
- Missing directory:
  - If `models_dir()` does not exist, `Path.glob()` yields an empty sequence without raising an exception.

### 4.2 Split GGUF Suffix Rules and Unicode Digits
- Split regex in `bootstrap.py:75`:
  `part = re.compile(r"-(\d{5})-of-(\d{5})\.gguf$")`
- Split regex in `catalog.py:77` and `bootstrap.py:97`:
  `_PART_SUFFIX = re.compile(r"-\d{5}-of-\d{5}$")`
- Unicode digit matching:
  - In Python 3 `re`, `\d` matches all Unicode characters in category `Nd` (Decimal Number), including Arabic-Indic digits `\u0660-\u0669` (e.g. `٢` = `\u0662`) and fullwidth digits `\uff10-\uff19`.
- Split verification rules:
  1. Part 1 index check:
     - `if m.group(1) != "00001": continue`
     - Uses exact ASCII string comparison `!= "00001"`. If group 1 consists of non-ASCII Unicode digits (e.g. `\u0660\u0660\u0660\u0660\u0661`), it is rejected.
  2. Total count parsing:
     - `total = int(m.group(2))`
     - Python's `int()` successfully parses Unicode decimal digits (e.g. `int("0000٢") == 2`).
  3. Continuation part lookup:
     - Iterates `i in range(2, total + 1)`.
     - Template: `f"{stem}-{i:05d}-of-{m.group(2)}.gguf" in names`.
     - `i:05d` produces ASCII digits (`00002`), while `m.group(2)` preserves the original string from part 1 (e.g. `"0000٢"`).
     - Verified: `["a-00001-of-0000٢.gguf", "a-00002-of-0000٢.gguf"]` forms a valid complete split set.
  4. Total count zero:
     - For `a-00001-of-00000.gguf`, `total = 0`.
     - `range(2, 1)` is empty; `all(...)` returns `True`. The single file is accepted as a complete split.
  5. Incomplete splits:
     - If any required part in `2..=total` is missing from `names`, the split is omitted entirely from `staged_models()`.
  6. Non-split files:
     - If `m is None`, the file is appended to `out` unconditionally.

### 4.3 Staged Model ID Derivation (`staged_model_ids`)
- Implementation: `bootstrap.py:94-97`
  `[re.sub(r"-\d{5}-of-\d{5}$", "", p.stem) for p in staged_models()]`
- For single-file GGUF `model.gguf`: `p.stem` is `model`; regex does not match; ID is `model`.
- For split GGUF `model-00001-of-00004.gguf`: `p.stem` is `model-00001-of-00004`; regex strips `-00001-of-00004`; ID is `model`.
- For split GGUF with Unicode digits `a-00001-of-0000٢.gguf`: regex matches and strips `-00001-of-0000٢`; ID is `a`.

---

## 5. Catalog Matching Semantics

### 5.1 AssetFile Local Name Resolution
- Implementation: `catalog.py:81-97`
- Property `local_name`: `self.local or PurePosixPath(self.path).name`.
- If `local` is explicitly set (e.g. `{"local": "chosen.gguf"}`), returns `chosen.gguf`.
- If `local` is `None` or empty, extracts filename from `path` (e.g. `"path/to/p.gguf" -> "p.gguf"`).

### 5.2 QuantVariant Model ID Resolution
- Implementation: `catalog.py:100-112`
- Property `model_id`:
  1. Takes first file in variant: `self.files[0].path`.
  2. Extracts filename and strips `.gguf` suffix via `removesuffix(".gguf")`.
  3. Strips split suffix using `_PART_SUFFIX.sub("", stem)` where `_PART_SUFFIX = re.compile(r"-\d{5}-of-\d{5}$")`.
- Matches the output of `staged_model_ids()` for the corresponding staged files.

### 5.3 Catalog Lookup (`find_entry_for_model`)
- Implementation: `catalog.py:458-464`
- Sequentially scans `CATALOG`. For each `CatalogEntry`, inspects each `QuantVariant` in `entry.variants`.
- Matching condition: `variant.model_id == model_id` (case-sensitive exact string match).
- Returns `(entry, variant)` tuple on first match; returns `None` if no variant matches.

---

## 6. Truthiness and Explicit Boolean Contracts

Summary of explicit boolean versus `None` contracts:

| Context | Condition | Python Expression | Return Value | Notes |
| :--- | :--- | :--- | :--- | :--- |
| `_props_modalities` | Vision true | `bool(modalities["vision"])` | `True` | Live vision confirmed |
| `_props_modalities` | Vision false | `bool(modalities["vision"])` | `False` | Explicit false returned, not None |
| `_props_modalities` | No modalities / network fail | Exception or missing key | `None` | Triggers catalog fallback |
| `managed_model_supports_vision` | Unstaged model | `model_id not in staged_model_ids()` | `None` | Falls through to next source |
| `managed_model_supports_vision` | Live returns `True` | `live is not None` | `True` | Live true wins |
| `managed_model_supports_vision` | Live returns `False` | `live is not None` | `False` | Live false wins; catalog skipped |
| `managed_model_supports_vision` | Unknown model | `find_entry_for_model == None` | `None` | Falls through to next source |
| `managed_model_supports_vision` | Catalog has no mmproj | `entry.mmproj is None` | `False` | Known model without vision |
| `managed_model_supports_vision` | Catalog mmproj exists on disk | `projector.exists()` | `True` | Staged companion file found |
| `managed_model_supports_vision` | Catalog mmproj missing on disk | `not projector.exists()` | `False` | Staged companion file missing |
| `is_managed_provider` | llamacpp alias | `p in _LLAMACPP_ALIASES` | `True` | Matches regardless of base_url |
| `is_managed_provider` | custom provider | `p == "custom" and base_url` | `bool` | Evaluates is_managed_endpoint |
| `is_managed_provider` | custom without base_url | `p == "custom" and not base_url` | `False` | Missing base_url rejects custom |
| `is_managed_provider` | other providers | `p not in aliases` | `False` | Rejects foreign providers |
