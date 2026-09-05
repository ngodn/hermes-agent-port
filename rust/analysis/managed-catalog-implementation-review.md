# Managed Catalog Implementation Review

**Review Target**:
- Rust Catalog: [`ManagedCatalog`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L13-L77) in [`managed_catalog.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs)
- Rust Capabilities Integration: [`ManagedCapabilities`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L18-L177) in [`managed_capabilities.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs)
- Python Authority: [`catalog.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py), [`capabilities.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/capabilities.py), and [`bootstrap.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/bootstrap.py)

**Scope Constraints**: Review only; no code modifications to Rust or test tools. Full runner wiring and model recommendation are explicitly deferred to later work.

---

## Executive Summary

The Rust implementation in [`managed_catalog.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs) and its integration in [`managed_capabilities.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs) successfully replicates the runtime HTTP refresh protocol, failure-throttling TTL semantics, and non-blocking background dispatch of [`hermes_cli/local_runtime/catalog.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py).

However, the review identified several critical findings across four areas:
1. **Architectural Gap (Untyped JSON vs Domain Model)**: Rather than parsing into strongly-typed structs ([`CatalogEntry`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L125-L191), [`QuantVariant`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L99-L124), [`AssetFile`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L80-L98)), [`managed_catalog.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L14) stores catalog state as an untyped [`serde_json::Value`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L14). Consequently, helper methods like [`find_entry_for_model`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L458-L465) or [`local_name`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L94-L97) are absent, forcing consumers like [`ManagedCapabilities::catalog_vision`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L148-L176) to duplicate ad-hoc JSON tree traversals and clone the entire AST on every capability check.
2. **Permissive Malformed-Input Over-Engineering**: To satisfy synthetic golden cases in [`managed-catalog-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/managed-catalog-goldens.json), [`normalize_catalog`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L146-L193) re-implements Python duck-typing quirks (Arabic-Indic digits, underscores in numeric strings, converting 2-char string lists into key-value dictionaries, and string `"false"` being truthy). These are artifacts of Python's runtime coercions rather than intentional catalog specifications, obscuring the practical catalog contract.
3. **Packaged Data Drift Risk**: The packaged catalog is loaded from [`rust/tools/managed-catalog.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/managed-catalog.json) via [`include_str!("../../../tools/managed-catalog.json")`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L22). Changes to the canonical [`hermes_cli/local_runtime/catalog.json`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L402) do not automatically propagate without running the offline golden generator.
4. **Lifecycle & Sharing Encapsulation**: [`ManagedCapabilities`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L30-L58) instantiates its own isolated [`ManagedCatalog`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L13-L18) instance; its constructor accepting an existing [`Arc<ManagedCatalog>`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L46) is currently private, preventing shared catalog ownership across gateway subsystems.

---

## 1. Refresh, TTL, and Concurrency Analysis

### 1.1 Specification Comparison

| Feature | Python Authority ([`catalog.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py)) | Rust Implementation ([`managed_catalog.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs)) | Fidelity Assessment |
| :--- | :--- | :--- | :--- |
| **Refresh URL** | `https://raw.githubusercontent.com/NousResearch/hermes-agent/main/hermes_cli/local_runtime/catalog.json` ([`catalog.py:348`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L348)) | `https://raw.githubusercontent.com/NousResearch/hermes-agent/main/hermes_cli/local_runtime/catalog.json` ([`managed_catalog.rs:10`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L10)) | **Exact Match** |
| **Refresh TTL** | `6 * 3600` seconds (6 hours) ([`catalog.py:351`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L351)) | `Duration::from_secs(6 * 3600)` ([`managed_catalog.rs:11`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L11)) | **Exact Match** |
| **User-Agent Header** | `hermes-local-runtime` ([`catalog.py:425`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L425)) | `hermes-local-runtime` ([`managed_catalog.rs:58`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L58)) | **Exact Match** |
| **Network Timeout** | 10 seconds socket timeout ([`catalog.py:426`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L426)) | Connect 10s, Read 10s ([`managed_catalog.rs:35-36`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L35-L36)) | **Practical Match** |
| **TTL Reservation** | Set before network I/O under lock ([`catalog.py:422`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L422)) | Set before network I/O under mutex ([`managed_catalog.rs:54`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L54)) | **Exact Match** |
| **Failure Throttling** | Failures do not revert timestamp; retries throttled for 6h ([`catalog.py:428-430`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L428-L430)) | Failures do not revert timestamp; throttled for 6h ([`managed_catalog.rs:63`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L63)) | **Exact Match** |
| **Initial Attempt State** | `0.0` (subject to monotonic uptime edge cases) ([`catalog.py:353`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L353)) | `None` ([`managed_catalog.rs:32`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L32)) | **Improved in Rust** |
| **Identical Payloads** | Returns `True` even if payload is unchanged ([`catalog.py:431-434`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L431-L434)) | Returns `true` even if payload is unchanged ([`managed_catalog.rs:65`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L65)) | **Exact Match** |
| **Disk Mutability** | In-memory only; zero disk writes ([`catalog.py:343-345`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L343-L345)) | In-memory only; zero disk writes ([`managed_catalog.rs:64`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L64)) | **Exact Match** |

### 1.2 Concrete Refresh & Concurrency Findings

1. **Lock Scoping and I/O Decoupling**:
   - In [`ManagedCatalog::refresh`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L48-L66), the mutex lock [`self.attempted.lock()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L50) is held strictly in an isolated inner block (lines 49-55) to check the elapsed TTL and record `Some(Instant::now())`.
   - The asynchronous HTTP request (`client.get(&self.url)...send().await`) is executed completely outside of the lock.
   - Any concurrent callers invoking `refresh(false)` while network I/O is ongoing immediately observe `at.elapsed() < REFRESH_TTL` and return `false` without waiting or duplicating network calls.
2. **Background Dispatch via Tokio Task (`refresh_soon`)**:
   - In [`ManagedCatalog::refresh_soon`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L70-L76), the method returns immediately without blocking the caller's request.
   - It performs an initial non-blocking check on [`self.attempted`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L71). If within TTL, it skips dispatch entirely.
   - If TTL has lapsed, it clones the `Arc<Self>` and executes `tokio::spawn(async move { catalog.refresh(false).await; })`.
   - If multiple concurrent requests trigger `refresh_soon()` simultaneously, multiple tasks may be spawned, but the first spawned task to execute will commit `*attempted = Some(Instant::now())` inside `refresh()`. The remaining tasks immediately abort with `false`, preventing redundant GitHub network traffic.
3. **Startup Monotonic Clock Improvement**:
   - In Python, `_last_refresh_attempt = 0.0`. If a Linux host was booted within the last 6 hours, `time.monotonic() < 21600.0`, causing Python's `now - _last_refresh_attempt < _REFRESH_TTL_S` to evaluate to `True`, inadvertently suppressing initial catalog refresh.
   - In Rust, [`attempted`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L32) initializes to `None`. The condition `attempted.is_some_and(|at| at.elapsed() < REFRESH_TTL)` evaluates to `false`, guaranteeing that a fresh process is always immediately eligible for a remote refresh on startup regardless of system uptime.
4. **Failure Backoff Semantics**:
   - As in Python, any HTTP error (such as 404, 500, or DNS failure), network timeout, or schema failure leaves `attempted` stamped with `Instant::now()`.
   - Consequently, downstream consumers are shielded from tight retry loops against GitHub.

---

## 2. Packaged Catalog Loading and Data Integrity

### 2.1 Packaged Loading Architecture
- **Rust Implementation**: [`ManagedCatalog::packaged()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L21-L26) embeds [`rust/tools/managed-catalog.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/managed-catalog.json) via `include_str!("../../../tools/managed-catalog.json")` at compile time.
- **Normalization at Startup**: When `packaged()` is called, it deserializes the embedded JSON string into [`serde_json::Value`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L22) and passes it through [`normalize_catalog`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L146-L193).

### 2.2 Concrete Packaged Loading Findings

1. **Drift Risk with Python Canonical Catalog**:
   - The canonical local runtime catalog in Python is located at [`hermes_cli/local_runtime/catalog.json`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.json).
   - The Rust build embeds [`rust/tools/managed-catalog.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/managed-catalog.json), which is a detached duplicate.
   - Synchronizing this duplicate requires running [`rust/tools/gen_managed_catalog_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_managed_catalog_goldens.py). There is currently no `build.rs` or CI check enforcing that `rust/tools/managed-catalog.json` matches `hermes_cli/local_runtime/catalog.json`. If models are added, removed, or modified in the Python repository, Rust's packaged fallback will silently drift.
2. **Crate Boundary Leak via `include_str!`**:
   - In [`managed_catalog.rs:22`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L22), `include_str!("../../../tools/managed-catalog.json")` navigates outside the crate directory (`rust/crates/hermes-gateway`).
   - If `hermes-gateway` is ever packaged as a standalone crate or built outside the repository root, `cargo package` will reject paths outside the package root.
   - The packaged catalog should reside within the crate hierarchy (e.g. `crates/hermes-gateway/resources/catalog.json` or embedded directly from the root workspace asset).
3. **Fail-Fast Initialization**:
   - [`ManagedCatalog::packaged()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L21-L26) uses `.expect(...)` on both JSON parsing and catalog normalization. If the embedded payload fails schema verification, process startup panics immediately with a clear error. This correctly mirrors Python's behavior, where a corrupted packaged JSON raises a top-level exception at module import time.

---

## 3. Practical Catalog Behavior vs Permissive Malformed-Input Edge Cases

A key finding of this review is the distinction between **practical catalog behavior** (how curated models are authored, shipped, and consumed) and **permissive malformed-input edge cases** (how Python's loose typing was emulated in Rust to satisfy synthetic tests).

### 3.1 Field-by-Field Analysis

| Field | Practical Catalog Specification | Permissive Edge Cases in [`normalize_catalog`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L146-L193) | Practical Impact & Assessment |
| :--- | :--- | :--- | :--- |
| `schema_version` | Literal integer `1`. Reject any document where version != 1. | Coerces strings (`"1"`), floats (`1.0`), and boolean `true` to integer `1` via [`integer()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L90-L98). | **Harmless but unnecessary**: In reality, GitHub and packaged documents always supply integer `1`. |
| `id`, `display_name`, `description`, `repo` | Non-empty strings identifying the model and repository. | Cloned directly as [`Value`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L154). Missing fields cause normalization to return `None`. | **Practical**: Required fields are properly gated. |
| `n_ctx_train`, `full_layers`, `recurrent_layers`, `per_layer_f16` | Standard positive integers (e.g. `131072`, `64`, `0`, `4096`). | Supports underscore separators (`"1_024"`), Arabic-Indic digits (`"١٢"`), truncating floats (`12.9 -> 12`), and booleans (`true -> 1`). | **Over-engineered emulation**: Python's `int()` accepts these representations. Real production catalogs never author context windows or layer counts with Arabic-Indic digits or underscores. |
| `swa_layers`, `swa_window`, `mtp_draft_depth`, `n_vocab`, `quality` | Optional integers with defaults (`0`, `0`, `3`, `0`, `0`). | Defaults applied when missing; otherwise subjected to the same loose [`integer()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L90-L98) parsing. | **Practical defaults match Python**: Default values match [`CatalogEntry`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L139-L164). |
| `moe`, `mtp`, `validated` | Booleans indicating MoE architecture, MTP heads, or test verification. | Uses [`truthy()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L79-L88): `"false"` evaluates to `true`! Non-empty arrays (`[0]`) evaluate to `true`. | **Subtle Hazard**: In JSON, an author writing `"moe": "false"` will have `moe` evaluated as `true`. In Python `bool("false")` is `True`. In a typed system, this should strictly accept boolean `true`/`false`. |
| `sampling` | JSON Object specifying launch flags (e.g. `{"temp": 0.7}`). | [`dictionary()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L120-L133) accepts array of pairs `[["x", 1]]` and array of 2-char strings `["ab"] -> {"a": "b"}`. | **Extreme edge case**: Replicates Python `dict(["ab"])`. No catalog author writes sampling configurations as `["ab"]`. |
| `min_engine` | String release tag (e.g. `"b10678"`) or empty string `""`. | If non-string, calls [`crate::image_routing::python_repr`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L171) to format `["x", true]` as `"['x', True]"`. | **Permissive edge case**: Driven solely by Python `str(val)`. In practical catalogs, engine tags are strings. |
| `decode_fraction` | Float between `0.0` and `1.0`, default `1.0`. | Parses `"1_0.5"`, boolean `true -> 1.0`. | **Harmless emulation**: Matches Python `float()`. |
| `mmproj`, `draft` | Asset object `{"path": ..., "size_bytes": ...}` or `null`. | [`asset()`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L135-L142) converts falsy values (`{}`, `[]`, `""`) to `null`. | **Practical**: Safely handles absent assets. |
| Unknown Fields | Future properties added upstream. | Unknown fields at document, model, variant, and asset levels are ignored. | **Forward Compatibility Guarantee**: Matches Python contract; older clients survive new catalog fields. |

### 3.2 Evaluation of Edge-Case Complexity
The normalization code in [`managed_catalog.rs:79-143`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L79-L143) devotes substantial logic ([`truthy`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L79), [`integer`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L90), [`numeric_text`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L101), [`dictionary`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L120)) to mimicking Python's dynamic runtime conversions.

While this allows Rust to pass synthetic fixture assertions generated by [`gen_managed_catalog_goldens.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/gen_managed_catalog_goldens.py#L30-L45), it introduces unnecessary baggage. In real catalog usage, inputs are well-formed JSON documents authored by repository maintainers or fetched from the official repository. Enforcing standard JSON types (`i64`, `f64`, `bool`, `String`, `Map<String, Value>`) would be cleaner, safer, and more idiomatic in Rust.

---

## 4. Managed Capabilities Integration

### 4.1 Integration Boundary
[`ManagedCapabilities`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L18-L22) integrates the catalog to resolve vision support for models that are staged on disk but not currently loaded by the engine.

The lookup order in [`managed_model_supports_vision`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L72-L82) mirrors [`capabilities.py:82-118`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/capabilities.py#L82-L118):
1. Verify model is in [`staged_model_ids`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L251-L285).
2. Query live server modalities via [`props_modalities`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L124-L146).
3. Fall back to catalog check via [`catalog_vision`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L148-L176).

### 4.2 Concrete Integration Findings

1. **Snapshot Deep-Cloning Overhead in `catalog_vision`**:
   - In [`managed_capabilities.rs:149`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L149):
     ```rust
     let catalog = self.catalog.snapshot();
     ```
   - [`ManagedCatalog::snapshot`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L42-L44) acquires the read lock and calls `.clone()` on the entire `Value` document.
   - For every staged vision lookup where the model is not currently live, the full catalog JSON AST is cloned in memory, allocating strings, vectors, and maps.
   - In contrast, Python's `find_entry_for_model` iterates the frozen module-level tuple [`CATALOG`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L407) without copying anything.
2. **Missing Catalog Query API Forces Ad-Hoc Traversal**:
   - In Python, `capabilities.py:107` simply calls `hit = find_entry_for_model(model_id)` and inspects `entry.mmproj`.
   - Because [`ManagedCatalog`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L13-L18) provides no domain query API, [`ManagedCapabilities::catalog_vision`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L148-L176) must manually re-implement model lookup:
     - Iterates `catalog["models"]` and `entry["variants"]`.
     - Extracts the first file from `variant["files"]`.
     - Extracts posix filename, strips `.gguf`, and applies `strip_part_suffix(stem)`.
     - Checks whether `mmproj` exists, resolves `local` or `path`, and checks `models/assets/{name}` on disk.
   - This leads to logic duplication: both [`managed_capabilities.rs:240-248`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L240-L248) and [`catalog.py:108-112`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L108-L112) define `PART_SUFFIX` regex logic independently.
3. **Encapsulation and Instance Decoupling**:
   - [`ManagedCapabilities::from_packaged`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L30-L32) instantiates a fresh `ManagedCatalog::packaged()`.
   - [`ManagedCapabilities::with_catalog`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_capabilities.rs#L46-L58) is private.
   - Consequently, there is no public way to share an existing `Arc<ManagedCatalog>` between `ManagedCapabilities` and other gateway subsystems (such as a router or future runner). If another component triggers `catalog.refresh()`, `ManagedCapabilities` retains its own separate catalog instance and will not observe the refreshed models.

---

## 5. Architectural Comparison: Untyped `Value` vs Domain Model

### 5.1 Structural Comparison

In Python, [`catalog.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py) is a domain module defining strongly-typed entities:
- [`AssetFile`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L80-L98): Encapsulates `path`, `size_bytes`, `local`, and derived `local_name`.
- [`QuantVariant`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L99-L124): Encapsulates `quant`, `files`, `validated`, `model_id`, `size_bytes`, and `weights_bytes`.
- [`CatalogEntry`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L125-L191): Encapsulates architecture parameters, `download_files()`, `download_bytes()`, and `profile()`.
- Public functions: [`find_entry_for_model`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L458-L465), [`find_variant`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L451-L456), and [`catalog_by_id`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/local_runtime/catalog.py#L447-L449).

In Rust, [`managed_catalog.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs) currently implements only:
- Storage of raw [`Value`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L14) in `RwLock<Value>`.
- Raw normalization [`normalize_catalog(&Value) -> Option<Value>`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/managed_catalog.rs#L146-L193).
- HTTP fetch and atomic swap.

### 5.2 Implications for Future Tasks (Runner Wiring & Recommendations)
As noted in the user instructions, full runner wiring and model recommendation are explicitly later work. However, the current untyped architecture will impact that work:
1. **Model Downloader**: Needs to know which files to fetch for a variant (`download_files`). Without typed structs, the downloader will have to repeat JSON navigation across `variants` and `mmproj`/`draft`.
2. **Estimator & Hardware Fit**: `select_variant` and `recommended_entry` in Python require layer counts, KV cache size per layer (`per_layer_f16`), SWA parameters, and MTP settings. Operating on raw `Value` will require repeated type checks and conversions rather than clean struct field access.
3. **Lookup Indices**: In Python, `catalog_by_id()` constructs a hash map for $O(1)$ lookups. The Rust implementation performs linear scans over JSON arrays.

---

## 6. Concrete Recommendations for Future Work

Without modifying Rust code or tools in this turn, the following concrete improvements are recommended for subsequent implementation tasks:

1. **Introduce Strongly-Typed Domain Structs**:
   - Define `AssetFile`, `QuantVariant`, and `CatalogEntry` structs with `serde::Deserialize` in `managed_catalog.rs`.
   - Store `Arc<Vec<CatalogEntry>>` (or `Arc<Catalog>`) inside `ManagedCatalog` rather than `RwLock<Value>`.
   - Implement `find_entry_for_model(&self, model_id: &str) -> Option<(CatalogEntry, QuantVariant)>` and `find_variant(&self, entry_id: &str, model_id: &str) -> Option<QuantVariant>` directly on `ManagedCatalog`.
   - Make `snapshot()` return an `Arc<Catalog>` reference (cheap pointer clone) instead of deep-cloning a `Value` tree.
2. **Expose Public Constructor for Shared Catalog in `ManagedCapabilities`**:
   - Change `fn with_catalog(...)` to `pub fn with_catalog(root: PathBuf, catalog: Arc<ManagedCatalog>) -> Self` in `managed_capabilities.rs:46`.
   - This allows the gateway server to instantiate a single `Arc<ManagedCatalog>` and share it across `ManagedCapabilities`, the image router, and the model runner.
3. **Consolidate `model_id` and Asset Name Derivation**:
   - Move `strip_part_suffix` and `local_name` logic into the catalog domain types where they naturally belong, eliminating redundant regex definitions in `managed_capabilities.rs`.
4. **Harden Packaged Asset Packaging**:
   - Place the packaged `catalog.json` under `crates/hermes-gateway/resources/catalog.json` so it complies with crate boundary constraints.
   - Add a cargo test or build check verifying that `hermes_cli/local_runtime/catalog.json` and the Rust packaged asset remain byte-identical.
5. **Normalize Validation to Idiomatic Rust**:
   - Transition from permissive duck-typing (`numeric_text`, `truthy`, `dictionary` list-of-pairs) to strict JSON schema validation, as real catalogs never produce these edge cases.
