# Managed Curated Catalog Source Review

This document specifies the exact runtime semantics, validation rules, field coercions, defaults, and concurrency/TTL behaviors of the managed runtime curated model catalog in `hermes_cli/local_runtime/catalog.py`. It provides verified findings for implementing native curated catalog loading and refresh in Rust without relying on invented APIs or external `models.dev` mechanisms.

Scope and authority:
- `hermes_cli/local_runtime/catalog.py`: lines 80-98 (`AssetFile`)
- `hermes_cli/local_runtime/catalog.py`: lines 99-124 (`QuantVariant`)
- `hermes_cli/local_runtime/catalog.py`: lines 125-191 (`CatalogEntry`)
- `hermes_cli/local_runtime/catalog.py`: lines 192-200 (`VariantChoice`)
- `hermes_cli/local_runtime/catalog.py`: lines 348-354 (catalog constants and concurrency primitives)
- `hermes_cli/local_runtime/catalog.py`: lines 356-361 (`_asset_from`)
- `hermes_cli/local_runtime/catalog.py`: lines 363-397 (`_load_catalog`)
- `hermes_cli/local_runtime/catalog.py`: lines 399-408 (`_packaged_catalog` and initial `CATALOG`)
- `hermes_cli/local_runtime/catalog.py`: lines 410-435 (`refresh_catalog`)
- `hermes_cli/local_runtime/catalog.py`: lines 437-445 (`refresh_catalog_soon`)
- `hermes_cli/local_runtime/catalog.json`: packaged curated catalog payload (schema version 1)

---

## 1. Dataclasses and Derived Properties

All catalog dataclasses are defined with `@dataclass(frozen=True)` to ensure in-memory immutability once instantiated.

### 1.1 `AssetFile` (`catalog.py:80-98`)
Represents a single downloadable artifact file (model GGUF part, mmproj projector, or speculative draft model).
- Fields:
  - `path`: `str` (required). Repository-relative path, which may include subdirectories (for example, `"UD-Q4_K_XL/Qwen3.8-Flash-Next-UD-Q4_K_XL-00001-of-00004.gguf"` or `"mmproj-BF16.gguf"`).
  - `size_bytes`: `int` (required). Exact file size in bytes from Hugging Face LFS metadata. Used for memory/hardware estimation and download progress bars. No checksum or hash validation is performed at download time by design.
  - `local`: `str | None = None` (optional). On-disk filename override to prevent collision when upstream repositories reuse generic filenames such as `"mmproj-BF16.gguf"`.
- Properties:
  - `local_name -> str`: Returns `self.local` if truthy; otherwise returns `PurePosixPath(self.path).name`.

### 1.2 `QuantVariant` (`catalog.py:99-124`)
Represents one concrete downloadable quantization build of a model.
- Fields:
  - `quant`: `str` (required). Quantization class identifier (for example, `"UD-Q4_K_M"` or `"UD-Q4_K_XL"`).
  - `files`: `tuple` (required). Tuple of `AssetFile` instances. The first file (`files[0]`) is the target passed to `llama-server` during launch.
  - `validated`: `bool = False` (optional). Explicit flag indicating whether this build has been verified end-to-end on real hardware. Unvalidated builds represent day-0 catalog entries.
- Properties:
  - `model_id -> str`: Extracts the base model ID by taking the filename of `files[0].path`, stripping the `".gguf"` suffix via `.removesuffix(".gguf")`, and removing split part suffixes matching the regular expression `_PART_SUFFIX = re.compile(r"-\d{5}-of-\d{5}$")`. For split GGUFs, all parts share the same derived `model_id`.
  - `size_bytes -> int`: Sum of `f.size_bytes` across all assets in `self.files`.
  - `weights_bytes -> int`: Returns `self.size_bytes`. Used as a safe, slightly conservative estimate for tensor weights until `profile_from_gguf` parses actual file headers.

### 1.3 `CatalogEntry` (`catalog.py:125-191`)
Represents a curated model family entry carrying estimator inputs, architectural parameters, and download asset metadata.
- Fields:
  - `id`: `str` (required). Stable family ID (for example, `"qwen3.8-27b"`).
  - `display_name`: `str` (required). Human-facing model name.
  - `description`: `str` (required). Single-line plain language summary.
  - `repo`: `str` (required). Hugging Face repository path (for example, `"unsloth/Qwen3.8-27B-GGUF"`).
  - `variants`: `tuple` (required). Tuple of `QuantVariant` instances (exactly one Q4-class variant in production catalog).
  - `n_ctx_train`: `int` (required). Native training context window in tokens.
  - `full_layers`: `int` (required). Count of full-attention layers.
  - `recurrent_layers`: `int` (required). Count of recurrent/linear-attention layers paying zero KV bytes per token.
  - `per_layer_f16`: `int` (required). Key-value cache bytes per token per full-attention layer at FP16.
  - `swa_layers`: `int = 0` (optional). Sliding window attention layer count.
  - `swa_window`: `int = 0` (optional). Sliding window token size.
  - `moe`: `bool = False` (optional). Whether the architecture is mixture-of-experts.
  - `mtp`: `bool = False` (optional). Whether the model integrates multi-token prediction heads.
  - `mtp_draft_depth`: `int = 3` (optional). Speculative draft depth for MTP models.
  - `n_vocab`: `int = 0` (optional). Vocabulary size used to price GPU logits buffers.
  - `mmproj`: `AssetFile | None = None` (optional). Vision multimodal projector asset.
  - `draft`: `AssetFile | None = None` (optional). External speculative decode draft model asset.
  - `sampling`: `dict = field(default_factory=dict)` (optional). Dictionary of INI-style launch defaults (for example, `temp`, `top-p`, `min-p`).
  - `min_engine`: `str = ""` (optional). Minimum required `llama.cpp` release tag (for example, `"b10678"`). Empty string indicates compatibility with any engine.
  - `quality`: `int = 0` (optional). Editorial quality score used for sorting recommendations.
  - `decode_fraction`: `float = 1.0` (optional). Fraction of model weights read per decoded token (1.0 for dense models, active expert fraction for MoE).
- Helper Methods:
  - `download_files(variant) -> tuple`: Returns `tuple(variant.files) + tuple(a for a in (self.mmproj, self.draft) if a is not None)`.
  - `download_bytes(variant) -> int`: Sum of `f.size_bytes` across all files returned by `download_files(variant)`.
  - `profile(variant) -> ModelProfile`: Builds `ModelProfile` with full, SWA, and recurrent layers, setting `kv_scale = 1.2` if `self.mtp` else `1.0`.

### 1.4 `VariantChoice` (`catalog.py:192-200`)
Represents the result of fitting a model variant to a given machine hardware budget.
- Fields:
  - `variant`: `QuantVariant` (required).
  - `zero_spill`: `bool` (required). True if weights and required context fit fully in usable VRAM.
  - `reason_key`: `str` (required). UI copy discriminator key: `"best-large-window"`, `"best-fits"`, or `"smallest-fits-spilled"`.

---

## 2. Ingestion, Validation, Defaults, and Type Coercion

### 2.1 Asset Parsing (`_asset_from`, `catalog.py:356-361`)
```python
def _asset_from(d: "dict | None") -> "AssetFile | None":
    if not d:
        return None
    return AssetFile(path=d["path"], size_bytes=int(d["size_bytes"]),
                     local=d.get("local"))
```
- Falsy Input Handling:
  - If `d` is `None`, `{}` (empty dictionary), or any falsy value, `_asset_from` returns `None`.
- Required Fields:
  - `path`: Looked up via direct indexing `d["path"]`. If absent, raises `KeyError: 'path'`.
  - `size_bytes`: Looked up via direct indexing `d["size_bytes"]` and coerced with `int(...)`. If absent, raises `KeyError: 'size_bytes'`. If the value cannot be parsed as an integer, raises `ValueError` or `TypeError`.
- Optional Fields:
  - `local`: Looked up via `d.get("local")`. If key is omitted or explicitly set to `None`, returns `None`. Otherwise returns the string value.
- Unknown Fields:
  - Any extra keys in `d` (such as `"sha256"`, `"comment"`) are ignored.

### 2.2 Catalog Document Parsing (`_load_catalog`, `catalog.py:363-397`)
`_load_catalog(doc: dict) -> tuple[CatalogEntry, ...]` enforces exact document validation and field coercion.

#### Schema Version Check
- `int(doc.get("schema_version", 0)) != _SCHEMA_VERSION`:
  - `_SCHEMA_VERSION` is `1`.
  - If `schema_version` is missing, `doc.get("schema_version", 0)` returns `0`.
  - Any value other than `1` raises `ValueError(f"catalog schema {doc.get('schema_version')!r} (this build reads 1)")`.
  - Unknown fields at document root are ignored.

#### Models Array
- Looked up via direct indexing `doc["models"]`. If absent, raises `KeyError: 'models'`. Must be an iterable of dictionaries.

#### Variant Parsing within Models
For each variant dictionary `v` in `m["variants"]`:
- `quant`: Looked up via direct indexing `v["quant"]`. Raises `KeyError` if absent.
- `files`: Looked up via direct indexing `v["files"]`. Raises `KeyError` if absent. Each element is parsed via `_asset_from(f)`. Returns a tuple of `AssetFile`.
- `validated`: Looked up via `v.get("validated")` and coerced with `bool(...)`. Defaults to `False` if missing or `None`.

#### Model Field Coercion and Default Matrix
For each model dictionary `m` in `doc["models"]`:

| Field Name | Lookup Method | Type Coercion | Default Value | Error on Missing |
| :--- | :--- | :--- | :--- | :--- |
| `id` | `m["id"]` | None (expects `str`) | None | `KeyError: 'id'` |
| `display_name` | `m["display_name"]` | None (expects `str`) | None | `KeyError: 'display_name'` |
| `description` | `m["description"]` | None (expects `str`) | None | `KeyError: 'description'` |
| `repo` | `m["repo"]` | None (expects `str`) | None | `KeyError: 'repo'` |
| `variants` | `m["variants"]` | `tuple(QuantVariant, ...)` | None | `KeyError: 'variants'` |
| `n_ctx_train` | `m["n_ctx_train"]` | `int(...)` | None | `KeyError: 'n_ctx_train'` |
| `full_layers` | `m["full_layers"]` | `int(...)` | None | `KeyError: 'full_layers'` |
| `recurrent_layers` | `m["recurrent_layers"]` | `int(...)` | None | `KeyError: 'recurrent_layers'` |
| `per_layer_f16` | `m["per_layer_f16"]` | `int(...)` | None | `KeyError: 'per_layer_f16'` |
| `swa_layers` | `m.get("swa_layers", 0)` | `int(...)` | `0` | None |
| `swa_window` | `m.get("swa_window", 0)` | `int(...)` | `0` | None |
| `moe` | `m.get("moe")` | `bool(...)` | `False` | None |
| `mtp` | `m.get("mtp")` | `bool(...)` | `False` | None |
| `mtp_draft_depth` | `m.get("mtp_draft_depth", 3)` | `int(...)` | `3` | None |
| `n_vocab` | `m.get("n_vocab", 0)` | `int(...)` | `0` | None |
| `mmproj` | `m.get("mmproj")` | `_asset_from(...)` | `None` | None |
| `draft` | `m.get("draft")` | `_asset_from(...)` | `None` | None |
| `sampling` | `m.get("sampling", {})` | `dict(...)` | `{}` | None |
| `min_engine` | `m.get("min_engine", "")` | `str(...)` | `""` | None |
| `quality` | `m.get("quality", 0)` | `int(...)` | `0` | None |
| `decode_fraction` | `m.get("decode_fraction", 1.0)` | `float(...)` | `1.0` | None |

#### Forward Compatibility Policy
Unknown fields at any nesting level (root, model object, variant object, asset object) are silently ignored. Newer catalog payloads carrying future properties remain fully readable by older versions unless the `schema_version` integer is bumped.

---

## 3. Packaged Catalog Loading (`_packaged_catalog`)

Implementation (`catalog.py:399-408`):
```python
def _packaged_catalog() -> "tuple[CatalogEntry, ...]":
    from importlib.resources import files

    raw = files("hermes_cli.local_runtime").joinpath("catalog.json").read_text(
        encoding="utf-8")
    return _load_catalog(json.loads(raw))

CATALOG: "tuple[CatalogEntry, ...]" = _packaged_catalog()
```
- Source of Truth: Reads `catalog.json` packaged alongside `catalog.py`.
- Module Import Guarantee: Loaded synchronously at Python import time.
- Offline Guarantee: Absolutely no network I/O occurs during import or packaged catalog initialization.
- Fallback Baseline: The packaged catalog forms the immutable offline truth. If network refresh is unavailable, unconfigured, or fails, the application operates entirely from this baseline.

---

## 4. Remote Refresh Semantics (`refresh_catalog`)

Implementation (`catalog.py:410-435`):
```python
_CATALOG_URL = ("https://raw.githubusercontent.com/NousResearch/hermes-agent"
                "/main/hermes_cli/local_runtime/catalog.json")
_SCHEMA_VERSION = 1
_REFRESH_TTL_S = 6 * 3600
_refresh_lock = threading.Lock()
_last_refresh_attempt = 0.0

def refresh_catalog(force: bool = False) -> bool:
    global CATALOG, _last_refresh_attempt

    now = time.monotonic()
    with _refresh_lock:
        if not force and now - _last_refresh_attempt < _REFRESH_TTL_S:
            return False
        _last_refresh_attempt = now
    try:
        req = urllib.request.Request(
            _CATALOG_URL, headers={"User-Agent": "hermes-local-runtime"})
        with urllib.request.urlopen(req, timeout=10) as r:
            fetched = _load_catalog(json.load(r))
    except Exception as exc:
        logger.debug("catalog refresh skipped: %s", exc)
        return False
    if fetched != CATALOG:
        logger.info("catalog refreshed from repo (%d models)", len(fetched))
    CATALOG = fetched
    return True
```

### 4.1 Endpoint and Request Specifications
- URL: `https://raw.githubusercontent.com/NousResearch/hermes-agent/main/hermes_cli/local_runtime/catalog.json`
- HTTP Headers: `{"User-Agent": "hermes-local-runtime"}`
- Network Timeout: 10 seconds (`timeout=10` on `urllib.request.urlopen`).

### 4.2 Refresh Concurrency and Lock Scoping
- Module-level lock: `_refresh_lock = threading.Lock()`.
- Scope of lock: The lock is held strictly during the TTL evaluation and timestamp reservation.
- Lock release before I/O: The lock is released immediately after `_last_refresh_attempt = now` before `urllib.request.urlopen` executes.
- Concurrency behavior:
  - Standard (non-forced) calls: The first thread entering the block updates `_last_refresh_attempt = now`. Any subsequent threads calling `refresh_catalog(force=False)` within the next 6 hours observe `now - _last_refresh_attempt < _REFRESH_TTL_S` and exit immediately returning `False`.
  - Force calls: Multiple concurrent calls with `force=True` can each pass the check, update `_last_refresh_attempt`, and initiate parallel HTTP fetches outside the lock.
  - Catalog swap: The global assignment `CATALOG = fetched` replaces the in-memory pointer.

### 4.3 TTL Semantics and Monotonic Clock
- TTL Interval: `_REFRESH_TTL_S = 6 * 3600` (21,600 seconds, exactly 6 hours).
- Monotonic Time: Uses `time.monotonic()` to measure elapsed time. System wall-clock shifts or NTP adjustments do not disrupt refresh cadence.
- Initial State: `_last_refresh_attempt` starts at `0.0`. On process startup, `time.monotonic()` exceeds 21,600 seconds (unless the host booted less than 6 hours prior), enabling immediate refresh eligibility on first demand.

### 4.4 Force Bypass
- Signature: `refresh_catalog(force: bool = False) -> bool`.
- Behavior: When `force=True`, the TTL condition `now - _last_refresh_attempt < _REFRESH_TTL_S` is bypassed. The attempt timestamp `_last_refresh_attempt` is updated to `now`, and the network request proceeds immediately regardless of when the previous attempt took place.

### 4.5 Failures Update Attempt Timestamp
- Verified finding: Failures DO update `_last_refresh_attempt`.
- Mechanism: `_last_refresh_attempt = now` is committed inside `with _refresh_lock:` before entering the `try...except` block.
- Error Handling:
  - If network I/O fails (such as DNS failure, connection refused, HTTP 404/500, or socket timeout), or if JSON decoding fails, or if `_load_catalog` raises `ValueError` due to a schema mismatch:
  - The exception is caught by `except Exception as exc:`.
  - The failure is logged at debug level: `logger.debug("catalog refresh skipped: %s", exc)`.
  - The function returns `False`.
  - `_last_refresh_attempt` is NOT reverted.
  - In-memory `CATALOG` remains completely untouched.
  - Consequently, a failed refresh attempt throttles further non-forced attempts for the entire 6-hour TTL window, preventing retry storms against GitHub.

### 4.6 Identical Payloads Count as Successful Refresh
- Verified finding: Identical payloads DO count as a successful refresh and return `True`.
- Code analysis:
  ```python
  if fetched != CATALOG:
      logger.info("catalog refreshed from repo (%d models)", len(fetched))
  CATALOG = fetched
  return True
  ```
  - `fetched != CATALOG` controls solely whether the informational message is logged (`"catalog refreshed from repo (%d models)"`).
  - `CATALOG = fetched` executes unconditionally.
  - `return True` executes unconditionally upon valid document ingestion.
  - A fetched document that matches existing catalog entries in value and order still replaces the in-memory reference and reports success (`True`).

### 4.7 Disk Writes
- Verified finding: ZERO disk writes.
- Refresh operations are strictly in-memory swaps.
- No cache files, temporary files, or updated `catalog.json` files are written to disk.
- Rationale: Preserves git checkout cleanliness (avoiding dirty working trees on developer machines) and avoids filesystem permission or corruption issues in production environments.

---

## 5. Background Scheduling (`refresh_catalog_soon`)

Implementation (`catalog.py:437-445`):
```python
def refresh_catalog_soon() -> None:
    if time.monotonic() - _last_refresh_attempt < _REFRESH_TTL_S:
        return
    threading.Thread(target=refresh_catalog, daemon=True,
                     name="catalog-refresh").start()
```
- Non-Blocking Contract: Returns immediately (`None`). The caller serves the currently loaded in-memory catalog for the active request.
- Optimistic Pre-Check: Checks `time.monotonic() - _last_refresh_attempt < _REFRESH_TTL_S` without acquiring `_refresh_lock`. If within TTL, no thread is spawned.
- Thread Spawning: If TTL has expired, launches a daemon thread (`daemon=True`, thread name `"catalog-refresh"`) targeting `refresh_catalog(force=False)`.
- Race Prevention: If multiple calls to `refresh_catalog_soon()` pass the optimistic pre-check simultaneously, the background threads serialize on `_refresh_lock` inside `refresh_catalog()`. The first thread updates `_last_refresh_attempt`, causing all other racing threads to exit immediately with `False`.

---

## 6. Rust Implementation Directives

1. Data Structures:
   - Port `AssetFile`, `QuantVariant`, and `CatalogEntry` as Rust structs.
   - For `model_id` derivation, match Python behavior: strip trailing `".gguf"`, then strip regex `-\d{5}-of-\d{5}$`.
   - Implement `local_name`: `local.as_deref().unwrap_or(filename)`.
2. Packaged Catalog:
   - Embed packaged catalog bytes using `include_str!` or embedded assets.
   - Parse and validate schema version 1 on startup.
3. Ingestion and Coercion:
   - Reject any document where `schema_version != 1`.
   - Reject any document where required fields (`id`, `display_name`, `description`, `repo`, `variants`, `n_ctx_train`, `full_layers`, `recurrent_layers`, `per_layer_f16`, `quant`, `files`, `path`, `size_bytes`) are missing or unparseable.
   - Apply explicit defaults for optional fields: `swa_layers = 0`, `swa_window = 0`, `moe = false`, `mtp = false`, `mtp_draft_depth = 3`, `n_vocab = 0`, `mmproj = None`, `draft = None`, `sampling = {}`, `min_engine = ""`, `quality = 0`, `decode_fraction = 1.0`, `validated = false`.
   - Ignore unknown keys across all levels.
4. Concurrency and TTL:
   - Maintain an `Option<Instant>` for `last_refresh_attempt` protected by a `Mutex`.
   - Maintain active catalog state inside an `RwLock<Arc<Catalog>>` or `ArcSwap`.
   - Check and reserve `last_refresh_attempt = Some(Instant::now())` inside the lock before starting network I/O.
   - Ensure failures do not reset `last_refresh_attempt`.
   - Support `force: bool` to bypass the 6-hour TTL check.
   - Return `true` on successful fetch even when payloads are identical.
   - Perform zero disk writes during remote refresh.
