# Local Server Probe and Vision Capability Source Review

This document specifies the exact runtime semantics of `agent/model_metadata.py`
for local server detection (`detect_local_server_type`), Ollama vision probing
(`query_ollama_supports_vision`), cache persistence, and HTTP client behaviors.
It provides concrete, easy-to-miss behavioral contracts for `local_probe.rs`
and the Python test oracle.

Scope and authority:
- `agent/model_metadata.py`: lines 160-322, 762-780, 866-874, 1049-1196, 2142-2190
- `utils.py`: lines 311-408 (`atomic_json_write`)

---

## 1. URL Normalization and Key Derivation

### 1.1 Base URL Normalization (`_normalize_base_url`)
- Implementation: `(base_url or "").strip().rstrip("/")`
- Whitespace is stripped from both ends, followed by stripping trailing forward slashes.
- Casing is preserved: scheme and host are not lowercased by this helper.
- Passing `None` or an empty string yields an empty string `""`.

### 1.2 Localhost to IPv4 Rewrite (`_localhost_to_ipv4`)
- Implementation: `re.sub(r"^(https?://)localhost(?=[:/]|$)", r"\g<1>127.0.0.1", url, count=1)`
- Anchored match: Only rewrites if the URL explicitly starts with `http://` or `https://`.
- Unanchored URLs without a scheme (for example `"localhost:11434"`) are not rewritten.
- Strict case sensitivity: Python regex does not pass `re.IGNORECASE`. Only lowercase `localhost` is rewritten; `Localhost` or `LOCALHOST` is left untouched.
- Lookahead delimiter `(?=[:/]|$)`: Matches `localhost:` (port), `localhost/` (path), or end of string. Subdomains such as `localhost.internal` or `localhost.domain.com` are not matched.
- Replacement limit: `count=1` replaces only the leading host occurrence. Embedded occurrences (for example in query parameters like `?upstream=http://localhost:8000`) remain untouched.
- Non-string inputs: If the input is not a string or is empty, it returns the input unchanged without raising an error.

### 1.3 LM Studio Server Root (`_lmstudio_server_root`)
- Input: `normalized` base URL (after `_normalize_base_url` and `_localhost_to_ipv4`).
- Suffix stripping order: Iterates sequentially over `("/api/v1", "/api", "/v1")`.
- Breaks on the first matching suffix:
  - `http://127.0.0.1:1234/api/v1` matches `/api/v1` first and yields `http://127.0.0.1:1234`.
  - `http://127.0.0.1:1234/v1` matches `/v1` and yields `http://127.0.0.1:1234`.
  - `http://127.0.0.1:1234/api` matches `/api` and yields `http://127.0.0.1:1234`.
- If no suffix matches, the stripped root is returned directly.

### 1.4 General Server URL (`server_url`)
- In `detect_local_server_type`:
  - `server_url` takes `normalized` and strips `/v1` only if `server_url.endswith("/v1")`: `server_url = server_url[:-3]`.
  - Notice difference from LM Studio: `server_url` does not strip `/api` or `/api/v1`.
  - Example: `http://localhost:1234/api/v1` results in:
    - `lmstudio_url`: `http://127.0.0.1:1234`
    - `server_url`: `http://127.0.0.1:1234/api`

### 1.5 Host Key for Blackhole Tracking (`_endpoint_host_key`)
- Implementation: Normalizes URL, parses with `urllib.parse.urlparse` (prepending `http://` if `://` is missing).
- Extracts `parsed.hostname` and `parsed.port or (443 if parsed.scheme == "https" else 80)`.
- Format: `"{host}:{port}"` (for example `127.0.0.1:11434`).
- Used exclusively for TCP connect timeout tracking (`_endpoint_blackhole_cache`). It is not used for memory verdict caching or disk caching.

### 1.6 Provider Prefix Stripping Boundary
- In `agent/model_metadata.py:2151`, `_strip_provider_prefix(model)` relies on dynamic imports from `providers.get_provider_profile` and regex matching on Ollama tags (`_OLLAMA_TAG_PATTERN`).
- For the Rust gateway port: `local_probe.rs` must receive an already normalized bare model name, or document that model prefix resolution is handled by the caller before invoking the probe. Do not hardcode a static provider registry in `local_probe.rs`.

---

## 2. Server Type Detection Waterfall (`detect_local_server_type`)

### 2.1 Authorization Headers (`_auth_headers`)
- Token processing: `token = str(api_key or "").strip()`.
- If token is non-empty: sets `{"Authorization": f"Bearer {token}"}`.
- If token is empty or whitespace only: returns `{}`.
- Forwarded across all probe requests, including Ollama, llama.cpp, and vLLM.

### 2.2 Probe Legs and Response Predicates
Detection executes up to four HTTP GET legs in strict waterfall order using a shared `httpx.Client(timeout=2.0, headers=headers)`:

1. **LM Studio**
   - URL: `{lmstudio_url}/api/v1/models`
   - Predicate: `r.status_code == 200`
   - Body evaluation: None. LM Studio matches on status 200 alone, even if the body is `"not-json"` or malformed.
   - Result: `"lm-studio"`.

2. **Ollama** (executed only if `result is None`)
   - URL: `{server_url}/api/tags`
   - Predicate: `r.status_code == 200` AND `"models" in data` where `data = r.json()`.
   - Crucial detail: LM Studio returns HTTP 200 with `{"error": "Unexpected endpoint"}` on `/api/tags`. The `"models" in data` check prevents LM Studio from being falsely classified as Ollama.
   - Malformed data handling:
     - If `data` is a JSON dict: checks key existence (`"models"` in dict keys).
     - If `data` is a JSON list: checks element equality (`["models"]` contains `"models"`).
     - If `data` is a JSON string: checks substring (`"models"` in `"models"` is True).
     - If `data` is a JSON primitive (null, number, boolean): `in` raises a `TypeError`, which is swallowed by `except Exception: pass`, leaving `result = None`.
   - Result: `"ollama"`.

3. **llama.cpp** (executed only if `result is None`)
   - URL: First attempts `{server_url}/v1/props`.
   - Fallback trigger: Executes `{server_url}/props` if and only if `r.status_code != 200`.
   - Critical easy-to-miss behavior: If `/v1/props` returns HTTP 200, but its body does not contain `"default_generation_settings"`, the legacy fallback `/props` is NOT called. Status 200 suppresses the fallback.
   - Predicate: `r.status_code == 200 and "default_generation_settings" in r.text`.
   - Body evaluation: Substring check on the raw body string (`r.text`), not JSON deserialization.
   - Result: `"llamacpp"`.

4. **vLLM** (executed only if `result is None`)
   - URL: `{server_url}/version`
   - Predicate: `r.status_code == 200` AND `"version" in data` where `data = r.json()`.
   - Malformed data handling:
     - If `data` is a JSON dict: checks key existence. Notice that `{"version": null}` evaluates to True because `"version"` is a key in the dict.
     - If `data` is a JSON list: `["version"]` evaluates to True.
     - If `data` is a JSON string: `"version"` evaluates to True.
     - If JSON decoding fails or `data` is not iterable (e.g. number, null): `TypeError` or `JSONDecodeError` escapes to the outer handler, where `_is_connect_timeout` is False, so it is swallowed, leaving `result = None`.
   - Result: `"vllm"`.

---

## 3. Ollama Vision Capability Probe (`query_ollama_supports_vision`)

### 3.1 Pre-conditions and Setup
- If `bare_model` is empty or `base_url` is empty: returns `None`.
- Server gate: Calls `detect_local_server_type(base_url, api_key=api_key)`. If the detected type is not `"ollama"`, returns `None` immediately without sending any request.
- URL: `server_url = _localhost_to_ipv4(base_url.rstrip("/"))`. If `server_url.endswith("/v1")`, strips `/v1`. Target is `{server_url}/api/show`.
- Method: HTTP POST with JSON body `{"name": bare_model}`.
- Headers: `Authorization: Bearer <token>` when `api_key` is non-empty.
- Timeout: 3.0 seconds (unlike the 2.0 second detection timeout).

### 3.2 Response Handling and Capabilities Precedence
- Status code must be 200. Any other status code returns `None`.
- JSON parse failure or network error returns `None`.
- If response is valid JSON:
  1. Inspect `caps = data.get("capabilities")`.
     - When `caps` is a JSON list (`isinstance(caps, list)`):
       - If any element satisfies `str(cap).lower() == "vision"`: returns `True` (`Some(true)`). Notice that whitespace is not stripped, so `" vision "` does not match `"vision"`.
       - If no element satisfies `"vision"` AND `caps` is non-empty (`if caps:`):
         Returns `False` (`Some(false)`) immediately.
         Precedence rule: A non-empty capabilities list definitively asserts the model's capabilities. It overrides and preempts `model_info`. Even if `model_info` contains `"vision.block_count"`, `capabilities: ["completion"]` causes the function to return `False`.
       - If `caps` is an empty list `[]`: `if caps:` is false. Execution falls through to `model_info`.
     - When `caps` is not a list (for example null, missing, string, or number): Execution falls through to `model_info`.
  2. Inspect `model_info = data.get("model_info")`.
     - When `model_info` is a JSON object/dict (`isinstance(model_info, dict)`):
       - Iterates over all keys. If `"vision.block_count"` is a substring in `str(key).lower()`: returns `True` (`Some(true)`).
       - The value associated with the key is completely ignored. Even if the value is `0`, `false`, or `null`, having `"vision.block_count"` in the key name returns `True`.
       - If no key matches `"vision.block_count"`: Does NOT return `False`. It falls through to return `None`.
     - When `model_info` is not a dict (for example a list or missing): Falls through to return `None`.
  3. Default return:
     - Returns `None` (`None` / unknown).
- Summary of return states:
  - `Some(true)`: Capabilities contains `"vision"`, or (capabilities is empty/absent and `model_info` contains a `"vision.block_count"` key).
  - `Some(false)`: Capabilities is a non-empty list that does not contain `"vision"`.
  - `None`: Unreachable server, non-Ollama server, non-200 status, or capabilities empty/absent with no `"vision.block_count"` key in `model_info`.

---

## 4. Cache Failure, Expiration, and Multi-tier Storage

Three distinct caching mechanisms exist with different keys, lifecycles, and persistence rules.

### 4.1 In-Memory Detection Cache (`_endpoint_probe_path_cache`)
- Key: `server_url` (normalized URL with IPv4 rewrite and `/v1` removed).
- Value: `(server_type, timestamp)` using monotonic clock (`time.monotonic()`).
- Positive verdict TTL: `_ENDPOINT_PROBE_TTL_SECONDS = 3600.0` (1 hour).
- Negative verdict TTL: `_ENDPOINT_PROBE_FAILURE_TTL_SECONDS = 300.0` (5 minutes).
- Docstring discrepancy: The Python function docstring claims the result is cached for the process lifetime. The code overrides this with a 3600-second positive TTL and 300-second negative TTL.
- Cache consultation: If an entry exists and `(time.monotonic() - cached_time) < ttl`, returns `cached_verdict`. Expired entries fall through to blackhole and disk checks.

### 4.2 TCP Connect Blackhole Cache (`_endpoint_blackhole_cache`)
- Purpose: Prevents hangs on dead or unreachable hosts that blackhole SYN packets.
- Key: `_endpoint_host_key(server_url)` -> `"{host}:{port}"`.
- Value: Monotonic timestamp of the last observed connect timeout.
- TTL: `_ENDPOINT_BLACKHOLE_TTL_SECONDS = 30.0` (30 seconds).
- Trigger condition (`_is_connect_timeout`):
  - Only triggered by connect-phase timeouts (`httpx.ConnectTimeout` or `requests.exceptions.ConnectTimeout`).
  - Read timeouts, write timeouts, pool timeouts, and connection refused errors do NOT trigger blackholing.
  - Rationale in code: A read timeout proves the server accepted the TCP connection, which is the opposite of an unreachable blackhole.
- Behavior on connect timeout:
  - Inside `_probe_failed`: Calls `_note_endpoint_blackholed(server_url)` and re-raises `exc`.
  - Re-raising immediately aborts the `with httpx.Client` block, terminating the waterfall early and skipping all subsequent probe legs.
  - The negative verdict `(None, time.monotonic())` is recorded in `_endpoint_probe_path_cache`.
- Blackhole check (`_endpoint_blackholed`):
  - Checks if `host:port` exists in `_endpoint_blackhole_cache`.
  - If age >= 30.0s: deletes the entry from `_endpoint_blackhole_cache` and returns `False`.
  - If active (< 30.0s): returns `True`.
  - In `detect_local_server_type`: If `_endpoint_blackholed` is True, returns `None` immediately without network I/O. Crucially, this early exit does NOT refresh or write to `_endpoint_probe_path_cache`.

### 4.3 Disk L2 Cache (`local_endpoint_probes.json`)
- Path: `$HERMES_HOME/cache/local_endpoint_probes.json` (defaults to `~/.hermes/cache/local_endpoint_probes.json`).
- TTL: `_LOCAL_PROBE_DISK_TTL_SECONDS = 300.0` (5 minutes).
- Key: `f"{kind}:{key}"`, specifically `"server_type:{server_url}"`.
- Value schema: `{"value": server_type, "ts": epoch_seconds}` using wall-clock time (`time.time()`).
- Positive verdicts only: Negative verdicts (`None`) are NEVER persisted to disk.
- Read path (`_local_probe_disk_get`):
  - Loads JSON. If the file does not exist, is not a dict, or fails parsing, returns `{}`.
  - Reads `entry = data.get("server_type:{server_url}")`.
  - Validates `isinstance(entry, dict)` and `(time.time() - float(entry["ts"])) < 300.0`.
  - If valid, populates the in-memory cache `_endpoint_probe_path_cache[server_url] = (disk_hit, time.monotonic())` and returns `disk_hit`.
- Write path (`_local_probe_disk_put`):
  - Called only when a detection probe successfully identifies a server.
  - Reads existing cache entries and prunes entries where `(now - float(ts)) >= 300.0` or entry is not a dict.
  - Appends `data[f"{kind}:{key}"] = {"value": value, "ts": now}`.
  - Atomic write via `atomic_json_write` in `utils.py`:
    - Writes to a temporary file in the same directory (`.local_endpoint_probes_*.tmp`).
    - Flushes and executes `os.fsync`.
    - Atomically replaces the target file via `os.replace` (handling symlinks and Windows file locks).
  - Error isolation: Any disk failure is caught and logged at debug level; it never crashes the probe.

---

## 5. Timeouts

### 5.1 Detection Waterfall Timeout
- `httpx.Client(timeout=2.0, headers=headers)`
- In HTTPX, passing a float timeout configures all operations (connect, read, write, and pool acquisition) to 2.0 seconds.
- Reference: HTTPX Timeouts configuration, https://www.python-httpx.org/advanced/timeouts/
- Worst-case timing:
  - If a host drops SYN packets (blackhole), Leg 1 burns 2.0s on connect timeout, marks the endpoint blackholed, and aborts legs 2-4. Total time spent: ~2.0 seconds.
  - If a host accepts TCP connections but stalls on responses (read timeout), each leg burns 2.0s because read timeouts do not abort the waterfall. Total time spent across four legs: up to 8.0 seconds (or 10.0 seconds if `/props` fallback is attempted).

### 5.2 Ollama Show Timeout
- `httpx.Client(timeout=3.0, headers=headers)`
- Configures all timeout phases to 3.0 seconds.
- Fails safe to `None` on any timeout or connection failure.

---

## 6. Redirect Behavior and Client Defaults

A critical discrepancy exists between HTTP client libraries regarding redirect handling.

### 6.1 Python HTTPX Defaults
- In `agent/model_metadata.py`, both `detect_local_server_type` and `query_ollama_supports_vision` construct clients as:
  - `httpx.Client(timeout=2.0, headers=headers)`
  - `httpx.Client(timeout=3.0, headers=headers)`
- Default redirect behavior: HTTPX does NOT follow redirects by default.
  - `follow_redirects` defaults to `False`.
  - Official documentation citation: https://www.python-httpx.org/quickstart/#redirection-and-history
    "By default, HTTPX will not follow redirects for all HTTP methods, although this can be explicitly enabled."
- Consequence: If an endpoint responds with HTTP 301, 302, 307, or 308 (such as redirecting a path without a trailing slash), HTTPX returns the 3xx response directly. Because `r.status_code == 200` is required, redirects fail the probe leg.

### 6.2 Comparison with Python Requests and Urllib
- Python `requests`:
  - `requests.get()` follows redirects by default (`allow_redirects=True`).
  - Official documentation citation: https://requests.readthedocs.io/en/latest/user/quickstart/#redirection-and-history
- Python `urllib.request`:
  - `urllib.request.urlopen()` follows redirects automatically using `HTTPRedirectHandler` for 301, 302, 303, and 307 responses.
  - Official documentation citation: https://docs.python.org/3/library/urllib.request.html#urllib.request.HTTPRedirectHandler

### 6.3 Rust Reqwest Defaults and Porting Requirement
- Rust `reqwest::Client`:
  - By default, `reqwest::Client` follows redirects up to 10 hops (`reqwest::redirect::Policy::default()`).
  - Official documentation citation: https://docs.rs/reqwest/latest/reqwest/redirect/struct.Policy.html
    "The default policy is to follow up to 10 redirects."
- Porting Trap: If `reqwest::Client::new()` is used without custom redirect configuration, the Rust probe will follow redirects while the Python reference implementation rejects them.
- Requirement for `local_probe.rs`:
  - The `reqwest::ClientBuilder` must explicitly disable redirects using `.redirect(reqwest::redirect::Policy::none())` to ensure parity with Python's HTTPX probes.

---

## 7. Connect Timeout Classification in Rust

In `local_probe.rs`, differentiating connect timeouts from read timeouts is essential for blackhole tracking:
- In Python: `_is_connect_timeout(exc)` checks `isinstance(exc, httpx.ConnectTimeout)`.
- In Rust (`reqwest::Error`):
  - `err.is_connect()` returns true if the error occurred during the connect phase.
  - `err.is_timeout()` returns true if a timeout occurred.
  - Official documentation citation: https://docs.rs/reqwest/latest/reqwest/struct.Error.html
  - Identification: A connect timeout in `reqwest` is identified when `err.is_connect() && err.is_timeout()`.
  - Trap: Checking `err.is_timeout()` alone would classify read timeouts as connect timeouts, erroneously triggering the 30-second blackhole suppression on healthy, slow servers.

---

## 8. Concurrency and Synchronization Boundaries for Rust

- The `LocalProbe` instance should own its in-memory state:
  - `probe_cache: Mutex<HashMap<String, (Option<String>, Instant)>>`
  - `blackhole_cache: Mutex<HashMap<String, Instant>>`
- Lock safety rule: Locks must never be held across asynchronous `.await` network calls. Cache lookups must acquire the lock, check or update state, and drop the lock before initiating any HTTP requests.
- Clocks: Use `std::time::Instant` for in-memory TTL calculations (monotonic) and `std::time::SystemTime` for disk cache Unix timestamps.
