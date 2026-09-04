# SQLite Schema & Session History Architecture for `state.db`

**Target Document**: `rust/analysis/session-db.md`  
**Date**: September 2026  
**Context**: Python-to-Rust port / Strangler Fig migration. Documenting the complete SQLite schema, WAL and PRAGMA configuration, primary read/write paths, FTS5 search subsystem, and connection lifecycle for conversation session history in `state.db`, so it can be reimplemented in Rust using `rusqlite`.

---

## Table of Contents
1. [Schema & DDL: Sessions, Messages, and Metadata Tables](#1-schema--ddl-sessions-messages-and-metadata-tables)
   - [1.1 The `messages` Table](#11-the-messages-table)
   - [1.2 The `sessions` Table](#12-the-sessions-table)
   - [1.3 Ancillary Metadata Tables](#13-ancillary-metadata-tables)
   - [1.4 Indexes (Base, Deferred, and Dynamic)](#14-indexes-base-deferred-and-dynamic)
   - [1.5 Declarative Schema Reconciliation & Migrations](#15-declarative-schema-reconciliation--migrations)
2. [WAL Mode & PRAGMA Strategy](#2-wal-mode--pragma-strategy)
   - [2.1 Initialization Sequence](#21-initialization-sequence)
   - [2.2 Detailed `apply_wal_with_fallback` Workflow](#22-detailed-apply_wal_with_fallback-workflow)
   - [2.3 WAL Sizing & Durability PRAGMAs](#23-wal-sizing--durability-pragmas)
   - [2.4 Read-Pool & Multi-Connection Concurrency](#24-read-pool--multi-connection-concurrency)
3. [Primary Write Path: Persisting Session & Message History](#3-primary-write-path-persisting-session--message-history)
   - [3.1 Function Signatures](#31-function-signatures)
   - [3.2 The Literal `INSERT INTO messages` SQL](#32-the-literal-insert-into-messages-sql)
   - [3.3 Parameter Serialization & Field Encodings](#33-parameter-serialization--field-encodings)
   - [3.4 Session Counter Updates](#34-session-counter-updates)
   - [3.5 Session Creation (`create_session`)](#35-session-creation-create_session)
   - [3.6 Write Transactions & Jitter Retry (`_execute_write`)](#36-write-transactions--jitter-retry-_execute_write)
   - [3.7 Critical Ordering Decision: `id` vs `timestamp`](#37-critical-ordering-decision-id-vs-timestamp)
4. [Primary Read Path: Loading Session Message History](#4-primary-read-path-loading-session-message-history)
   - [4.1 Function Signatures](#41-function-signatures)
   - [4.2 SELECT Queries & Paging Modes](#42-select-queries--paging-modes)
   - [4.3 In-Place Compaction Deduplication (`_dedupe_display_generations`)](#43-in-place-compaction-deduplication-_dedupe_display_generations)
   - [4.4 Deserialization Pipeline](#44-deserialization-pipeline)
5. [FTS5 Setup for Full-Text Search](#5-fts5-setup-for-full-text-search)
   - [5.1 Virtual Table DDL & Exclusion Views](#51-virtual-table-ddl--exclusion-views)
   - [5.2 Custom Tokenizer C Extension (`native/fts5_cjk`)](#52-custom-tokenizer-c-extension-nativefts5_cjk)
   - [5.3 Synchronization Triggers & Optimization](#53-synchronization-triggers--optimization)
   - [5.4 Query Routing & Session Search SQL](#54-query-routing--session-search-sql)
6. [Database Path, Construction & Caching Lifecycle](#6-database-path-construction--caching-lifecycle)
   - [6.1 Database Path Resolution](#61-database-path-resolution)
   - [6.2 Process-Wide Shared Registry (`hermes_state_registry.py`)](#62-process-wide-shared-registry-hermes_state_registrypy)
   - [6.3 Read Budget & Descriptor Bounds](#63-read-budget--descriptor-bounds)
7. [Rust Implementation Blueprint with `rusqlite`](#7-rust-implementation-blueprint-with-rusqlite)
   - [7.1 Rust Data Structures](#71-rust-data-structures)
   - [7.2 Connection Setup & Durability PRAGMAs](#72-connection-setup--durability-pragmas)
   - [7.3 Message Append & Paging Functions](#73-message-append--paging-functions)
8. [Summary of Critical Traps & Non-Obvious Invariants](#8-summary-of-critical-traps--non-obvious-invariants)

---

## 1. Schema & DDL: Sessions, Messages, and Metadata Tables

Schema definitions are declared primarily in [`hermes_state_common.py:409-675`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L409-L675) with initialization, reconciliation, and dynamic indexes defined in [`hermes_state_schema.py:1193-1642`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_schema.py#L1193-L1642).

### 1.1 The `messages` Table
The central transcript table. Every conversation turn (user prompt, assistant response, tool call, tool output, system notification) is recorded as a row here.

* **Definition**: [`hermes_state_common.py:482-507`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L482-L507)

```sql
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    role TEXT NOT NULL,
    content TEXT,
    tool_call_id TEXT,
    tool_calls TEXT,
    tool_name TEXT,
    effect_disposition TEXT,
    timestamp REAL NOT NULL,
    token_count INTEGER,
    finish_reason TEXT,
    reasoning TEXT,
    reasoning_content TEXT,
    reasoning_details TEXT,
    codex_reasoning_items TEXT,
    codex_message_items TEXT,
    platform_message_id TEXT,
    observed INTEGER DEFAULT 0,
    _compressed_summary INTEGER NOT NULL DEFAULT 0,
    active INTEGER NOT NULL DEFAULT 1,
    compacted INTEGER NOT NULL DEFAULT 0,
    api_content TEXT,
    display_kind TEXT,
    display_metadata TEXT
);
```

#### Column Definitions & Semantics:
* `id`: 64-bit monotonically increasing primary key (`INTEGER PRIMARY KEY AUTOINCREMENT`). **This is the canonical ordering key for conversation turns**, not `timestamp`.
* `session_id`: Foreign key referencing `sessions(id)` identifying the conversation session.
* `role`: String message role (`'system'`, `'user'`, `'assistant'`, `'tool'`).
* `content`: The text content of the message. For multimodal messages (e.g. image attachments), this column stores a JSON string prefixed with `\x00json:` (`_CONTENT_JSON_PREFIX`, [`hermes_state.py:12455`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L12455)).
* `tool_call_id`: The ID string matching an assistant tool invocation; populated on `role='tool'` responses.
* `tool_calls`: JSON array string of tool call objects `[{"id": "...", "type": "function", "function": {"name": "...", "arguments": "..."}}]`.
* `tool_name`: Name of the tool invoked (e.g. `'execute_code'`, `'web_search'`); populated on tool responses.
* `effect_disposition`: Execution safety tracking (e.g., read-only, state-mutating, speculative).
* `timestamp`: Floating-point Unix epoch timestamp (`time.time()`).
* `token_count`: Estimated or token-counter measured token length of this message.
* `finish_reason`: Provider finish reason (`'stop'`, `'tool_calls'`, `'length'`, `'content_filter'`).
* `reasoning`: Chain-of-thought internal reasoning string (e.g. DeepSeek R1 / OpenAI o-series / Anthropic thinking).
* `reasoning_content`: Dedicated reasoning channel text where providers distinguish it from final text.
* `reasoning_details` / `codex_reasoning_items` / `codex_message_items`: JSON strings storing structured reasoning blocks for Codex/specialized architectures.
* `platform_message_id`: External chat platform message ID (e.g. Telegram `update_id`, Slack `ts`, WhatsApp `msg_id`) used for idempotent delivery deduplication.
* `observed`: Integer boolean (`0` or `1`). Indicates whether the assistant loop has observed/processed this event.
* `_compressed_summary`: Integer boolean (`0` or `1`). Marks synthetic summary messages created during context compression.
* `active`: Integer boolean (`0` or `1`, default `1`). Soft-delete flag. Active conversation history is `active = 1`. Messages rewound via `/rewind` or `/undo` are marked `active = 0`.
* `compacted`: Integer boolean (`0` or `1`, default `0`). Distinguishes messages summarized away by in-place compression (`active = 0, compacted = 1`) from user rewinds (`active = 0, compacted = 0`). Compacted rows remain searchable via FTS and visible in UI history, but are omitted when constructing model context prompts.
* `api_content`: The exact verbatim byte string sent over the wire to the LLM provider. Used for prompt-cache stability across turns.
* `display_kind`: Presentation categorization (e.g., `'status'`, `'error'`, `'system_card'`) for UI renderers without altering model context.
* `display_metadata`: JSON string containing UI presentation parameters (e.g., collapsed state, task counts).

---

### 1.2 The `sessions` Table
Stores conversation metadata, execution parameters, token aggregates, and branch/compression lineage pointers.

* **Definition**: [`hermes_state_common.py:419-480`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L419-L480)

```sql
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    user_id TEXT,
    session_key TEXT,
    chat_id TEXT,
    chat_type TEXT,
    thread_id TEXT,
    display_name TEXT,
    origin_json TEXT,
    expiry_finalized INTEGER DEFAULT 0,
    model TEXT,
    model_config TEXT,
    system_prompt TEXT,
    system_prompt_hash TEXT,
    parent_session_id TEXT,
    started_at REAL NOT NULL,
    ended_at REAL,
    end_reason TEXT,
    message_count INTEGER DEFAULT 0,
    tool_call_count INTEGER DEFAULT 0,
    input_tokens INTEGER DEFAULT 0,
    output_tokens INTEGER DEFAULT 0,
    cache_read_tokens INTEGER DEFAULT 0,
    cache_write_tokens INTEGER DEFAULT 0,
    reasoning_tokens INTEGER DEFAULT 0,
    cwd TEXT,
    git_branch TEXT,
    git_repo_root TEXT,
    git_metadata_generation INTEGER NOT NULL DEFAULT 0,
    billing_provider TEXT,
    billing_base_url TEXT,
    billing_mode TEXT,
    estimated_cost_usd REAL,
    actual_cost_usd REAL,
    cost_status TEXT,
    cost_source TEXT,
    pricing_version TEXT,
    title TEXT,
    title_source TEXT,
    last_activity_at REAL,
    last_activity_description TEXT,
    last_activity_provenance TEXT,
    api_call_count INTEGER DEFAULT 0,
    handoff_state TEXT,
    handoff_platform TEXT,
    handoff_error TEXT,
    compression_failure_cooldown_until REAL,
    compression_failure_error TEXT,
    compression_fallback_streak INTEGER NOT NULL DEFAULT 0,
    compression_ineffective_count INTEGER NOT NULL DEFAULT 0,
    compression_recovery_deadline REAL,
    profile_name TEXT,
    rewind_count INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    pinned INTEGER NOT NULL DEFAULT 0,
    hidden INTEGER NOT NULL DEFAULT 0,
    last_read_at REAL,
    tool_names TEXT,
    FOREIGN KEY (parent_session_id) REFERENCES sessions(id),
    FOREIGN KEY (system_prompt_hash) REFERENCES system_prompts(hash)
);
```

#### Key Columns:
* `id`: Unique session string (e.g. UUIDv4 or timestamped slug `YYYYMMDD_HHMMSS_xxxxxx`).
* `source`: Client entrypoint (`'cli'`, `'gateway'`, `'telegram'`, `'discord'`, `'cron'`, `'subagent'`).
* `session_key`: Stable routing peer key (`<source>:<user_id>:<chat_id>:<thread_id>`) that outlives individual sessions.
* `model`: Default LLM model identifier (e.g. `'claude-3-7-sonnet'`, `'gpt-4o'`).
* `model_config`: JSON configuration dictionary (sampling parameters, system markers like `_branched_from`, `_delegate_from`, `_reset_from`).
* `system_prompt_hash`: SHA-256 reference to content-addressed `system_prompts(hash)`.
* `parent_session_id`: Recursive parent pointer for branches, forks, and compression continuations.
* `started_at` / `ended_at`: Timestamps for session lifetime.
* `end_reason`: Deliberate or accidental end marker (`'session_reset'`, `'session_switch'`, `'compression'`, `'agent_close'`, `'startup_orphan_reap'`).
* `message_count` / `tool_call_count`: Cached aggregates bumped synchronously on message append.
* `input_tokens` / `output_tokens` / `cache_read_tokens` / `cache_write_tokens` / `reasoning_tokens`: Aggregated token consumption.
* `last_activity_at`: Heartbeat timestamp of the most recent user turn or agent action.

---

### 1.3 Ancillary Metadata Tables

#### 1. `system_prompts`
Content-addressed deduplication table for session system prompts ([`hermes_state_common.py:414-417`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L414-L417)):
```sql
CREATE TABLE IF NOT EXISTS system_prompts (
    hash TEXT PRIMARY KEY,
    prompt TEXT NOT NULL
);
```
`hash` is standard hex SHA-256 (`hashlib.sha256(prompt.encode("utf-8")).hexdigest()`).

#### 2. `schema_version`
Stores the database schema version ([`hermes_state_common.py:410-412`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L410-L412)):
```sql
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);
```
Current target: `SCHEMA_VERSION = 30` ([`hermes_state_common.py:357`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L357)).

#### 3. `state_meta`
Key-value store for internal state, FTS rebuild high-water marks, and unique store instances ([`hermes_state_common.py:531-534`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L531-L534)):
```sql
CREATE TABLE IF NOT EXISTS state_meta (
    key TEXT PRIMARY KEY,
    value TEXT
);
```

#### 4. `session_model_usage`
Fine-grained multi-model token and cost accounting per session, model, provider, and task ([`hermes_state_common.py:509-529`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L509-L529)):
```sql
CREATE TABLE IF NOT EXISTS session_model_usage (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    model TEXT NOT NULL,
    billing_provider TEXT NOT NULL DEFAULT '',
    billing_base_url TEXT NOT NULL DEFAULT '',
    billing_mode TEXT NOT NULL DEFAULT '',
    task TEXT NOT NULL DEFAULT '',
    api_call_count INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
    estimated_cost_usd REAL NOT NULL DEFAULT 0,
    actual_cost_usd REAL NOT NULL DEFAULT 0,
    cost_status TEXT,
    cost_source TEXT,
    first_seen REAL,
    last_seen REAL,
    PRIMARY KEY (session_id, model, billing_provider, billing_base_url, billing_mode, task)
);
```

#### 5. Gateway Routing & Coordination Tables
* `gateway_routing` ([`hermes_state_common.py:536-542`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L536-L542)): Composite PK `(scope, session_key)` mapping inbound chats to active session IDs.
* `conversation_generations` ([`hermes_state_common.py:571-576`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L571-L576)): Monotonic generation counter per routing peer (`(source, session_key)`), never garbage collected to prevent ABA reuse.
* `gateway_heartbeats` ([`hermes_state_common.py:586-593`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L586-L593)): Backend process liveness registration (`backend_id`, `pid`, `last_heartbeat`).
* `compression_locks` ([`hermes_state_common.py:595-600`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L595-L600)): Distributed mutual exclusion during session compression rotation.
* `session_turn_leases` ([`hermes_state_common.py:602-607`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L602-L607)): Per-conversation turn fence preventing split-brain writes.
* `async_delegations` ([`hermes_state_common.py:609-628`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L609-L628)): Tracks asynchronous subagent execution tasks.

---

### 1.4 Indexes (Base, Deferred, and Dynamic)

All index DDL is quoted verbatim below.

```sql
-- Base Indexes (hermes_state_common.py:630-650)
CREATE INDEX IF NOT EXISTS idx_sessions_source ON sessions(source);
CREATE INDEX IF NOT EXISTS idx_sessions_source_id ON sessions(source, id);
CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_session_id);
CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at DESC);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id, id);
CREATE INDEX IF NOT EXISTS idx_messages_assistant_calls_by_session
    ON messages(session_id)
    WHERE role = 'assistant' AND tool_calls IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_compression_locks_expires ON compression_locks(expires_at);
CREATE INDEX IF NOT EXISTS idx_session_turn_leases_expires ON session_turn_leases(expires_at);
CREATE INDEX IF NOT EXISTS idx_session_model_usage_session ON session_model_usage(session_id);
CREATE INDEX IF NOT EXISTS idx_session_model_usage_model ON session_model_usage(model);
CREATE INDEX IF NOT EXISTS idx_async_delegations_delivery
    ON async_delegations(delivery_state, completed_at);

-- Deferred Indexes (hermes_state_common.py:657-675)
-- Created after declarative column reconciliation ensures reconciler-added columns exist.
CREATE INDEX IF NOT EXISTS idx_messages_session_active
    ON messages(session_id, active, timestamp);
CREATE INDEX IF NOT EXISTS idx_messages_active_null
    ON messages(active) WHERE active IS NULL;
CREATE INDEX IF NOT EXISTS idx_sessions_session_key
    ON sessions(session_key, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_gateway_peer
    ON sessions(source, user_id, chat_id, chat_type, thread_id, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_handoff_state
    ON sessions(handoff_state, started_at);
CREATE INDEX IF NOT EXISTS idx_sessions_system_prompt_hash
    ON sessions(system_prompt_hash);
CREATE INDEX IF NOT EXISTS idx_sessions_effective_activity
    ON sessions(COALESCE(last_activity_at, started_at) DESC, started_at DESC);

-- Dynamic / Conditional Indexes (hermes_state_schema.py:1247-1250, 1611-1612)
CREATE INDEX IF NOT EXISTS idx_messages_platform_msg_id
    ON messages(session_id, platform_message_id)
    WHERE platform_message_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_title_unique
    ON sessions(title)
    WHERE title IS NOT NULL;
```

---

### 1.5 Declarative Schema Reconciliation & Migrations
The Python codebase follows the Beets/sqlite-utils declarative reconciliation pattern ([`hermes_state_schema.py:933-999`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_schema.py#L933-L999)):
* `SCHEMA_SQL` is the canonical declaration.
* On startup, `_reconcile_columns()` inspects live tables via `PRAGMA table_info("<table_name>")`.
* Any missing columns are added automatically via `ALTER TABLE "<table_name>" ADD COLUMN "<col_name>" <col_type>`.
* Version-gated migrations in `schema_version` (target `v30`) handle data transformations that cannot be done via simple declarative `ADD COLUMN` (such as rewriting primary keys on `gateway_routing` and `session_model_usage`, and deduplicating legacy duplicate session titles).

---

## 2. WAL Mode & PRAGMA Strategy

SQLite configuration is governed by [`hermes_state.py:1419-1733`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L1419-L1733) (`apply_wal_with_fallback`) and [`hermes_state.py:1998-2071`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L1998-L2071) (`apply_database_pragmas`).

### 2.1 Initialization Sequence
When `SessionDB` opens a writer connection ([`hermes_state.py:5540-5565`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L5540-L5565)):
1. Open connection: `sqlite3.connect(path, timeout=1.0, isolation_level=None, check_same_thread=False)`.
2. Apply WAL mode with safety fallback: `apply_wal_with_fallback(conn, db_label="state.db")`.
3. Apply database sizing and performance PRAGMAs: `apply_database_pragmas(conn, db_label="state.db")`.
4. Enforce foreign keys: `PRAGMA foreign_keys = ON;`.
5. Optionally load CJK tokenizer extension (`load_fts5_cjk_extension`).
6. Execute schema scripts and column reconciliation.

### 2.2 Detailed `apply_wal_with_fallback` Workflow
1. **Operator Preference**: Reads `database.journal_mode` from `~/.hermes/config.yaml` (defaults to `"wal"`, or explicit `"delete"`).
2. **Vulnerability Gate** ([`hermes_state.py:1324-1336`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L1324-L1336)):
   * Linked SQLite libraries in versions 3.7.0 through 3.51.2 (without backports 3.50.7 or 3.44.6) contain the upstream SQLite WAL-reset bug (resetting transaction log can destroy data on crash). On fresh databases using vulnerable libraries, WAL is refused and `DELETE` mode is retained.
3. **Read-Only Mode Probe** ([`hermes_state.py:1488`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L1488)):
   * Executes `PRAGMA journal_mode;` without locking or unlinking sidecar files.
   * **Never Live-Downgrade Rule**: If the on-disk header reports `wal`, keep `wal`! Never downgrade a live database to `delete` while other concurrent processes may hold it open.
4. **Enabling WAL**:
   * Runs `PRAGMA journal_mode = WAL;`.
   * **Silent Refusal Detection**: On macOS NFS, SMB/CIFS, or Docker AgentFS overlays, SQLite returns `delete` from `PRAGMA journal_mode = WAL` without raising an error. The return value is inspected: if not `"wal"`, fallback to `DELETE` is triggered and logged as an error.
   * **Error Handling & EIO Retry**: Catches `sqlite3.OperationalError` containing `"locking protocol"`, `"disk i/o error"`, etc. For `"disk i/o error"`, retries twice with 50ms sleep to disambiguate transient filesystem pressure from permanent ZFS/NFS protocol incompatibility.
5. **Safe Switching to DELETE** ([`hermes_state.py:1625-1659`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L1625-L1659)):
   * When switching to `DELETE`, SQLite requires exclusive ownership. The helper temporarily sets `PRAGMA busy_timeout = 0;` so that if any concurrent connection holds the database, it immediately fails with `database is locked` instead of clobbering uncheckpointed WAL frames.

### 2.3 WAL Sizing & Durability PRAGMAs
When WAL mode is active, the following PRAGMAs are executed:
* **WAL Truncation Limit** ([`hermes_state.py:1220-1256`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L1220-L1256)):
  ```sql
  PRAGMA journal_size_limit = 67108864; -- 64 MiB
  ```
  SQLite's default `journal_size_limit` is -1 (unlimited). After large operations (e.g. VACUUM or FTS rebuilds), the `-wal` file retains its high-water mark indefinitely without shrinking. Bounding it to 64 MiB forces SQLite to truncate slack at each checkpoint.
* **macOS Darwin Durability Guards** ([`hermes_state.py:1259-1322`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L1259-L1322)):
  ```sql
  PRAGMA checkpoint_fullfsync = 1; -- Enforces F_FULLFSYNC on checkpoint on macOS
  PRAGMA synchronous = FULL;        -- Never NORMAL on macOS to prevent btree corruption
  ```
  On Darwin, `fsync()` does not guarantee platter durability across power loss or system shutdown unless `F_FULLFSYNC` is used.
* **Configurable Performance PRAGMAs** ([`hermes_state.py:2039-2070`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L2039-L2070)):
  * `PRAGMA cache_size = -65536;` (configured via `database.cache_size`, negative for KiB, e.g. 64MB).
  * `PRAGMA mmap_size = 0;` (configured via `database.mmap_size`).
  * `PRAGMA temp_store = MEMORY;` (configured via `database.temp_store`).
  * `PRAGMA wal_autocheckpoint = 1000;` (configured via `database.wal_autocheckpoint`).

### 2.4 Read-Pool & Multi-Connection Concurrency
* In WAL mode, reads do not block writes and writes do not block reads.
* `SessionDB` opens a dedicated writer connection and maintains a bounded pool of read-only connections (`mode=ro`, [`hermes_state.py:5676-5744`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L5676-L5744)) using `file:<path>?mode=ro`.
* Read permits are budgeted across the process to a maximum of `_READ_POOL_MAX = 8` per database path (`_PathReadBudget`, [`hermes_state.py:417`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L417)).
* If read permits are exhausted, queries cleanly fall back to running on the locked writer connection rather than hanging.

---

## 3. Primary Write Path: Persisting Session & Message History

### 3.1 Function Signatures
1. `append_message`: Persists a single message row and updates session counters.
   * **Location**: [`hermes_state.py:12693-12838`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L12693-L12838)
   ```python
   def append_message(
       self,
       session_id: str,
       role: str,
       content: str = None,
       tool_name: str = None,
       tool_calls: Any = None,
       tool_call_id: str = None,
       token_count: int = None,
       finish_reason: str = None,
       reasoning: str = None,
       reasoning_content: str = None,
       reasoning_details: Any = None,
       codex_reasoning_items: Any = None,
       codex_message_items: Any = None,
       platform_message_id: str = None,
       observed: bool = False,
       effect_disposition: Optional[str] = None,
       _compressed_summary: bool = False,
       timestamp: Any = None,
       api_content: Optional[str] = None,
       display_kind: Optional[str] = None,
       display_metadata: Optional[Dict[str, Any]] = None,
       compression_lock_holder: Optional[str] = None,
       turn_lease_holder: Optional[str] = None,
       turn_lease_ttl_seconds: float = 300.0,
   ) -> int
   ```
2. `append_messages_batch`: Inserts multiple messages atomically in **one** transaction.
   * **Location**: [`hermes_state.py:12840-12931`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L12840-L12931)
   ```python
   def append_messages_batch(
       self,
       session_id: str,
       messages: List[Dict[str, Any]],
       compression_lock_holder: Optional[str] = None,
       turn_lease_holder: Optional[str] = None,
       chunk_rows: Optional[int] = None,
       turn_lease_ttl_seconds: float = 300.0,
   ) -> int
   ```

### 3.2 The Literal `INSERT INTO messages` SQL
From [`hermes_state.py:12783-12812`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L12783-L12812) and [`hermes_state.py:13228-13257`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L13228-L13257):

```sql
INSERT INTO messages (
    session_id,
    role,
    content,
    tool_call_id,
    tool_calls,
    tool_name,
    effect_disposition,
    timestamp,
    token_count,
    finish_reason,
    reasoning,
    reasoning_content,
    reasoning_details,
    codex_reasoning_items,
    codex_message_items,
    platform_message_id,
    observed,
    _compressed_summary,
    active,
    api_content,
    display_kind,
    display_metadata
)
VALUES (
    ?, ?, ?, ?,
    ?, ?, ?, ?, ?, ?,
    ?, ?, ?, ?,
    ?, ?, ?, ?, 1,
    ?, ?, ?
);
```

### 3.3 Parameter Serialization & Field Encodings
| Param Position | Column Name | Type | Serialization / Encoding |
|---|---|---|---|
| 1 | `session_id` | `TEXT` | Session ID string |
| 2 | `role` | `TEXT` | `'system'`, `'user'`, `'assistant'`, or `'tool'` |
| 3 | `content` | `TEXT` | String as-is; structured list of parts encoded as JSON prefixed with `\x00json:` |
| 4 | `tool_call_id` | `TEXT` | Tool call ID string or `NULL` |
| 5 | `tool_calls` | `TEXT` | JSON array string `[{"id": ...}]` or `NULL` |
| 6 | `tool_name` | `TEXT` | Tool name or `NULL` |
| 7 | `effect_disposition` | `TEXT` | Effect disposition string or `NULL` |
| 8 | `timestamp` | `REAL` | Unix epoch seconds (`time.time()`) |
| 9 | `token_count` | `INTEGER` | Integer count or `NULL` |
| 10 | `finish_reason` | `TEXT` | `'stop'`, `'tool_calls'`, etc. or `NULL` |
| 11 | `reasoning` | `TEXT` | Chain-of-thought string or `NULL` |
| 12 | `reasoning_content`| `TEXT` | Reasoning text or `NULL` |
| 13 | `reasoning_details`| `TEXT` | JSON string or `NULL` |
| 14 | `codex_reasoning_items` | `TEXT` | JSON string or `NULL` |
| 15 | `codex_message_items`   | `TEXT` | JSON string or `NULL` |
| 16 | `platform_message_id`  | `TEXT` | Platform message ID or `NULL` |
| 17 | `observed` | `INTEGER` | `1` if observed else `0` |
| 18 | `_compressed_summary` | `INTEGER` | `1` if summary else `0` |
| 19 | `active` | `INTEGER` | Literal constant `1` |
| 20 | `api_content` | `TEXT` | Prompt-cache verbatim string or `NULL` |
| 21 | `display_kind` | `TEXT` | Presentation kind or `NULL` |
| 22 | `display_metadata` | `TEXT` | JSON string or `NULL` |

### 3.4 Session Counter Updates
In the same transaction following the message insert:
```sql
-- When tool_calls are present:
UPDATE sessions
SET message_count = message_count + ?,
    tool_call_count = tool_call_count + ?
WHERE id = ?;

-- When no tool_calls are present:
UPDATE sessions
SET message_count = message_count + ?
WHERE id = ?;
```

### 3.5 Session Creation (`create_session`)
From [`hermes_state.py:7171-7239`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L7171-L7239):
```sql
INSERT INTO sessions (
    id, source, user_id, session_key, chat_id, chat_type, thread_id,
    model, model_config, system_prompt, system_prompt_hash,
    parent_session_id, cwd, profile_name, git_repo_root,
    origin_json, display_name, started_at
)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(id) DO UPDATE SET
    model = COALESCE(sessions.model, excluded.model),
    model_config = CASE
        WHEN excluded.model_config IS NOT NULL
             AND json_type(sessions.model_config, '$._reset_from') IS NOT NULL
             AND json_remove(sessions.model_config, '$._reset_from') = '{}'
        THEN json_set(excluded.model_config, '$._reset_from', json_extract(sessions.model_config, '$._reset_from'))
        ELSE COALESCE(sessions.model_config, excluded.model_config)
    END,
    system_prompt_hash = COALESCE(sessions.system_prompt_hash, excluded.system_prompt_hash),
    system_prompt = CASE
        WHEN sessions.system_prompt_hash IS NULL AND excluded.system_prompt_hash IS NOT NULL THEN NULL
        ELSE sessions.system_prompt
    END,
    session_key = COALESCE(sessions.session_key, excluded.session_key),
    chat_id = COALESCE(sessions.chat_id, excluded.chat_id),
    chat_type = COALESCE(sessions.chat_type, excluded.chat_type),
    thread_id = COALESCE(sessions.thread_id, excluded.thread_id),
    parent_session_id = COALESCE(sessions.parent_session_id, excluded.parent_session_id),
    cwd = COALESCE(sessions.cwd, excluded.cwd),
    profile_name = COALESCE(sessions.profile_name, excluded.profile_name),
    git_repo_root = COALESCE(sessions.git_repo_root, excluded.git_repo_root),
    origin_json = COALESCE(sessions.origin_json, excluded.origin_json),
    display_name = COALESCE(sessions.display_name, excluded.display_name);
```

### 3.6 Write Transactions & Jitter Retry (`_execute_write`)
From [`hermes_state.py:6258-6360`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L6258-L6360):
* Write transactions are executed via `BEGIN IMMEDIATE` to acquire the write lock upfront and avoid lock upgrade deadlocks.
* Contention handling: On `database is locked` / `busy`, sleeps with randomized jitter (20–150ms, backing off to 250ms–1s).
* Retries until timeout: `_WRITE_PATIENCE_S = 20.0s` for routine writes, `_TRANSCRIPT_WRITE_PATIENCE_S = 60.0s` for transcript appends.
* Maintenance cadence:
  * Every 50 writes: `PRAGMA wal_checkpoint(PASSIVE);`.
  * Every 1000 writes: Incremental FTS merge (`INSERT INTO messages_fts(messages_fts, rank) VALUES('merge', 500);`).

### 3.7 Critical Ordering Decision: `id` vs `timestamp`
* **Evidence**: [`hermes_state.py:14062-14069`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L14062-L14069) and commit `c03acca50`.
* In production, timestamps produced by `time.time()` are non-monotonic due to NTP time step corrections, VM suspend/resume cycles, and WSL2 clock drift.
* Sorting by `timestamp` can sort an assistant tool call after the tool's result, corrupting OpenAI tool call alternation and causing HTTP 400 rejection on replay.
* **Invariant**: The ordering column is **always** `id ASC` (`INTEGER PRIMARY KEY AUTOINCREMENT`).

---

## 4. Primary Read Path: Loading Session Message History

### 4.1 Function Signatures
1. `get_messages`: Loads messages for REST API endpoints, TUI, and audit logs.
   * **Location**: [`hermes_state.py:13705-13814`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L13705-L13814)
   ```python
   def get_messages(
       self,
       session_id: str,
       include_inactive: bool = False,
       include_compacted: bool = False,
       limit: Optional[int] = None,
       offset: int = 0,
       latest: bool = False,
       after_id: Optional[int] = None,
   ) -> List[Dict[str, Any]]
   ```
2. `get_messages_as_conversation`: Loads messages formatted for model generation (OpenAI format).
   * **Location**: [`hermes_state.py:14014-14084`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L14014-L14084)
   ```python
   def get_messages_as_conversation(
       self,
       session_id: str,
       include_ancestors: bool = False,
       include_inactive: bool = False,
       repair_alternation: bool = False,
       include_row_ids: bool = False,
       include_compacted: bool = False,
   ) -> List[Dict[str, Any]]
   ```
3. `get_session`: Retrieves session metadata with resolved system prompt.
   * **Location**: [`hermes_state.py:10906-10923`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L10906-L10923)
   ```python
   def get_session(self, session_id: str) -> Optional[Dict[str, Any]]
   ```

### 4.2 SELECT Queries & Paging Modes

#### Scenario A: Active Messages (Default Turn Restoration)
```sql
SELECT * FROM messages
WHERE session_id = ? AND active = 1
ORDER BY id ASC;
```

#### Scenario B: Paginated / Keyset Seeking (`after_id`)
```sql
-- Keyset pagination (O(1) seek on massive transcripts):
SELECT * FROM messages
WHERE session_id = ? AND active = 1 AND id > ?
ORDER BY id ASC
LIMIT ?;

-- Offset pagination:
SELECT * FROM messages
WHERE session_id = ? AND active = 1
ORDER BY id ASC
LIMIT ? OFFSET ?;
```
*Note*: SQLite requires a `LIMIT` when `OFFSET` is present. If `limit` is omitted in Python, it supplies `-1` (unbounded limit).

#### Scenario C: Reverse Paging (`latest=True`)
```sql
SELECT * FROM messages
WHERE session_id = ? AND active = 1
ORDER BY id DESC
LIMIT ? OFFSET ?;
```
*Note*: Python reverses the rows back in memory before returning so the caller always receives them chronologically.

#### Scenario D: Model Conversation Replay (`get_messages_as_conversation`)
From [`hermes_state.py:14060-14072`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L14060-L14072):
```sql
SELECT id, role, content, tool_call_id, tool_calls, tool_name, effect_disposition,
       finish_reason, reasoning, reasoning_content, reasoning_details,
       codex_reasoning_items, codex_message_items, platform_message_id, observed,
       _compressed_summary, timestamp, active,
       api_content, display_kind, display_metadata
FROM messages
WHERE session_id IN (?) AND active = 1
ORDER BY id ASC;
```
If compression ancestors exist and `include_ancestors=True`, `session_ids` contains the lineage from root to tip.

#### Scenario E: UI Display with Context Compaction Deduplication (`include_compacted=True`)
```sql
SELECT * FROM messages
WHERE session_id = ? AND (active = 1 OR compacted = 1)
ORDER BY id ASC;
```
The returned rows are passed through `_dedupe_display_generations(rows)` ([`hermes_state.py:13655-13703`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L13655-L13703)), which groups by `(role, content, timestamp, tool_call_id, tool_calls, tool_name)` and selects `max(active, id)` to prevent duplicate cards for compacted history.

#### Scenario F: Loading Session Metadata (`get_session`)
From [`hermes_state.py:10914-10920`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L10914-L10920):
```sql
SELECT s.*,
       COALESCE(sp.prompt, s.system_prompt) AS _system_prompt_resolved
FROM sessions s
LEFT JOIN system_prompts sp ON sp.hash = s.system_prompt_hash
WHERE s.id = ?;
```

### 4.3 In-Place Compaction Deduplication (`_dedupe_display_generations`)
From [`hermes_state.py:13655-13703`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L13655-L13703):
* When in-place compaction runs, messages in the protected tail are preserved by copying them into the new generation, leaving identical `(role, content, timestamp)` rows with different `active` flags and `id`s.
* Deduplication key: `(role, content, timestamp, tool_call_id, tool_calls, tool_name)`.
* Winner selection: Prioritizes active row (`active=1`), then highest `id`.
* The deduplicated list is returned sorted by `id ASC`.

### 4.4 Deserialization Pipeline
1. `content`: If string begins with `\x00json:`, strip the prefix and parse with JSON; otherwise return string as-is.
2. `tool_calls`: If non-null, deserialize JSON into list of tool calls.
3. `display_metadata`: If non-null, deserialize JSON into dictionary.
4. `_compressed_summary`: Integer `1`/`0` converted to boolean `true`/`false`.

---

## 5. FTS5 Setup for Full-Text Search

FTS5 search is implemented in [`hermes_state_search.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_search.py) and [`hermes_state_common.py:678-875`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L678-L875).

### 5.1 Virtual Table DDL & Exclusion Views

#### 1. Primary FTS Index: `messages_fts` (External Content, `unicode61`)
[`hermes_state_common.py:698-704`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L698-L704)
```sql
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    content,
    tool_name,
    tool_calls,
    content='messages',
    content_rowid='id'
);
```

#### 2. Trigram Index: `messages_fts_trigram` (External View, `trigram`)
[`hermes_state_common.py:814-826`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L814-L826)
```sql
CREATE VIEW IF NOT EXISTS messages_fts_trigram_src AS
    SELECT m.id, m.role, m.content, m.tool_name
    FROM messages AS m
    JOIN sessions AS s ON s.id = m.session_id
    WHERE m.role <> 'tool'
      AND s.source NOT IN ('cron', 'subagent')
      AND json_extract(COALESCE(s.model_config, '{}'), '$._delegate_from') IS NULL;

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts_trigram USING fts5(
    content,
    tool_name,
    content='messages_fts_trigram_src',
    content_rowid='id',
    tokenize='trigram'
);
```

#### 3. CJK Bigram Index: `messages_fts_cjk` (Custom Tokenizer `cjk_unicode61`)
[`hermes_state.py:4217-4231`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L4217-L4231)
```sql
CREATE VIEW IF NOT EXISTS messages_fts_cjk_src AS
    SELECT id, role, content, tool_name, tool_calls
    FROM messages
    WHERE role <> 'tool';

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts_cjk USING fts5(
    content,
    tool_name,
    tool_calls,
    content='messages_fts_cjk_src',
    content_rowid='id',
    tokenize='cjk_unicode61'
);
```

---

### 5.2 Custom Tokenizer C Extension (`native/fts5_cjk`)
* **Purpose**: Standard `unicode61` treats CJK character runs as single unbroken tokens, preventing substring matches. Standard `trigram` requires ≥3 CJK characters (9 bytes) per token, failing on common 2-character Korean/Chinese/Japanese words (e.g., "구글", "우리", "일본", "修复") and forcing 3–6s LIKE table scans.
* **C Implementation**: [`native/fts5_cjk/fts5_cjk.c`](file:///home/eins0fx/development/hermes-agent-port/native/fts5_cjk/fts5_cjk.c).
* **Algorithm**: Wraps SQLite's `unicode61`. Tokenized tokens are inspected for CJK codepoints. Maximal CJK character runs are emitted as character bigrams (overlapping pairs, Lucene `CJKAnalyzer` semantics), lone CJK characters as unigrams, and non-CJK segments pass through unaltered.
* **Exported Symbols**: `sqlite3_ftscjk_init` and `sqlite3_fts5_cjk_init` ([`native/fts5_cjk/fts5_cjk.c:231-252`](file:///home/eins0fx/development/hermes-agent-port/native/fts5_cjk/fts5_cjk.c#L231-L252)).
* **Location**: Compiled to `libfts5_cjk.so`, installed in `~/.hermes/lib/libfts5_cjk.so` (or `HERMES_FTS5_CJK_SO`).
* **Loading**:
  ```python
  conn.enable_load_extension(True)
  conn.load_extension(str(so_path))
  conn.enable_load_extension(False)
  ```

---

### 5.3 Synchronization Triggers & Optimization
Triggers maintain FTS index consistency with the `messages` table.

```sql
CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages
WHEN (new.id > COALESCE((SELECT CAST(value AS INTEGER) FROM state_meta
                         WHERE key = 'fts_rebuild_high_water'), -1)
   OR new.id <= COALESCE((SELECT CAST(value AS INTEGER) FROM state_meta
                          WHERE key = 'fts_rebuild_progress'), -1))
BEGIN
    INSERT INTO messages_fts(rowid, content, tool_name, tool_calls)
    VALUES (
        new.id,
        CASE WHEN new.role = 'tool'
             AND new.id > COALESCE((SELECT CAST(value AS INTEGER) FROM state_meta
                                    WHERE key = 'fts_tool_full_content_high_water'), -1)
             THEN substr(COALESCE(new.content, ''), 1, 8192)
             ELSE new.content END,
        new.tool_name,
        new.tool_calls
    );
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages
WHEN (old.id > COALESCE((SELECT CAST(value AS INTEGER) FROM state_meta
                         WHERE key = 'fts_rebuild_high_water'), -1)
   OR old.id <= COALESCE((SELECT CAST(value AS INTEGER) FROM state_meta
                          WHERE key = 'fts_rebuild_progress'), -1))
BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content, tool_name, tool_calls)
    VALUES (
        'delete',
        old.id,
        CASE WHEN old.role = 'tool'
             AND old.id > COALESCE((SELECT CAST(value AS INTEGER) FROM state_meta
                                    WHERE key = 'fts_tool_full_content_high_water'), -1)
             THEN substr(COALESCE(old.content, ''), 1, 8192)
             ELSE old.content END,
        old.tool_name,
        old.tool_calls
    );
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_update
AFTER UPDATE OF content, tool_name, tool_calls, role ON messages
WHEN (old.content IS NOT new.content
    OR old.tool_name IS NOT new.tool_name
    OR old.tool_calls IS NOT new.tool_calls
    OR old.role IS NOT new.role)
   AND (old.id > COALESCE((SELECT CAST(value AS INTEGER) FROM state_meta
                           WHERE key = 'fts_rebuild_high_water'), -1)
     OR old.id <= COALESCE((SELECT CAST(value AS INTEGER) FROM state_meta
                            WHERE key = 'fts_rebuild_progress'), -1))
BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content, tool_name, tool_calls)
    VALUES (
        'delete',
        old.id,
        CASE WHEN old.role = 'tool'
             AND old.id > COALESCE((SELECT CAST(value AS INTEGER) FROM state_meta
                                    WHERE key = 'fts_tool_full_content_high_water'), -1)
             THEN substr(COALESCE(old.content, ''), 1, 8192)
             ELSE old.content END,
        old.tool_name,
        old.tool_calls
    );
    INSERT INTO messages_fts(rowid, content, tool_name, tool_calls)
    VALUES (
        new.id,
        CASE WHEN new.role = 'tool'
             AND new.id > COALESCE((SELECT CAST(value AS INTEGER) FROM state_meta
                                    WHERE key = 'fts_tool_full_content_high_water'), -1)
             THEN substr(COALESCE(new.content, ''), 1, 8192)
             ELSE new.content END,
        new.tool_name,
        new.tool_calls
    );
END;
```

*Key Trigger Characteristics*:
1. `8192` Prefix Limit (`FTS_TOOL_CONTENT_PREFIX_CHARS`): For tool responses, only the first 8192 characters are indexed to avoid SQLite write lock stalls on massive machine dumps.
2. `AFTER UPDATE OF`: Restricts update triggers to content columns (`content`, `tool_name`, `tool_calls`, `role`). Updating status or display metadata avoids FTS re-indexing entirely.
3. Deferred Rebuild Gating: The `WHEN` condition references `fts_rebuild_high_water` and `fts_rebuild_progress` in `state_meta` to keep incremental rebuilds consistent without corruption.

---

### 5.4 Query Routing & Session Search SQL
From [`hermes_state_search.py:1875-1892`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_search.py#L1875-L1892):

```sql
SELECT
    m.id,
    m.session_id,
    m.role,
    snippet(messages_fts, -1, '>>>', '<<<', '...', 40) AS snippet,
    m.timestamp,
    m.tool_name,
    s.source,
    s.model,
    s.started_at AS session_started
FROM messages_fts
JOIN messages m ON m.id = messages_fts.rowid
JOIN sessions s ON s.id = m.session_id
WHERE messages_fts MATCH ?
  AND (m.active = 1 OR m.compacted = 1)
  -- Optional source / role filters:
  -- AND s.source IN (?, ?)
  -- AND s.source NOT IN (?, ?)
  -- AND m.role IN (?, ?)
ORDER BY rank -- or "ORDER BY m.timestamp DESC, rank" / "ORDER BY m.timestamp ASC, rank"
LIMIT ? OFFSET ?;
```

#### Query Execution Routing:
1. **CJK Queries**:
   * If `messages_fts_cjk` is available and query has no isolated 1-character CJK terms: Executed against `messages_fts_cjk`.
   * Else if all tokens have ≥3 CJK chars and `messages_fts_trigram` is available: Executed against `messages_fts_trigram`.
   * Otherwise: Falls through to substring `LIKE '%...%'` scan over `messages`.
2. **Latin Queries**:
   * Executed against `messages_fts`.
   * If zero matches return, automatically retried on `messages_fts_cjk` / `messages_fts_trigram` to catch Latin terms adjacent to CJK characters.
3. **Stale Index Fallback**:
   * If `state_meta` contains `fts_stale`: FTS is detached and all queries run against the canonical messages table via `LIKE`.

---

## 6. Database Path, Construction & Caching Lifecycle

### 6.1 Database Path Resolution
* **Canonical Path Resolver**: [`hermes_constants.py:114-140`](file:///home/eins0fx/development/hermes-agent-port/hermes_constants.py#L114-L140) (`get_hermes_home()`).
  1. Thread-local override (`_HERMES_HOME_OVERRIDE`).
  2. Environment variable: `$HERMES_HOME`.
  3. Platform default: `~/.hermes` (`Path.home() / ".hermes"`).
* **Database File Path** ([`hermes_state.py:694-712`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L694-L712)):
  * Default profile: `$HERMES_HOME/state.db`.
  * Named profiles: `$HERMES_HOME/profiles/<profile_name>/state.db`.

### 6.2 Process-Wide Shared Registry (`hermes_state_registry.py`)
To prevent lock storms and file descriptor leaks across concurrent threads:
* Refcounted singleton registry per path ([`hermes_state_registry.py:120-200`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_registry.py#L120-L200)):
  * `acquire(db_path)` resolves the path and returns a shared `SessionDB` instance, incrementing its refcount.
  * `release(db)` decrements the refcount and tears down the connection only on reaching zero.
  * Calling `close()` on a shared instance is an explicit no-op (`_shared_registry_owned = True`).
* Inode Replacement Tracking:
  * On every `acquire()`, `_stat_db_file_identity(path)` checks `(st_dev, st_ino)`.
  * If the inode changed on disk (e.g. from `hermes sessions recover` or snapshot restore), the current generation is marked retired and a fresh instance is constructed.

### 6.3 Read Budget & Descriptor Bounds
* Bounded reader pool: Each instance maintains up to `_READ_POOL_MAX = 8` read-only connections opened via `file:<path>?mode=ro` ([`hermes_state.py:5676-5744`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L5676-L5744)).
* `_PathReadBudget`: A process-wide semaphore budgets file descriptors per database path to avoid hitting OS `RLIMIT_NOFILE`. If all permits are held, reads degrade to executing on the primary writer connection under the writer lock.

---

## 7. Rust Implementation Blueprint with `rusqlite`

### 7.1 Rust Data Structures

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub source: String,
    pub user_id: Option<String>,
    pub session_key: Option<String>,
    pub chat_id: Option<String>,
    pub chat_type: Option<String>,
    pub thread_id: Option<String>,
    pub display_name: Option<String>,
    pub origin_json: Option<String>,
    pub model: Option<String>,
    pub model_config: Option<String>,
    pub system_prompt: Option<String>,
    pub system_prompt_hash: Option<String>,
    pub parent_session_id: Option<String>,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    pub end_reason: Option<String>,
    pub message_count: i64,
    pub tool_call_count: i64,
    pub cwd: Option<String>,
    pub profile_name: Option<String>,
    pub git_repo_root: Option<String>,
    pub last_activity_at: Option<f64>,
    pub archived: i64,
    pub pinned: i64,
    pub hidden: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRow {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_name: Option<String>,
    pub effect_disposition: Option<String>,
    pub timestamp: f64,
    pub token_count: Option<i64>,
    pub finish_reason: Option<String>,
    pub reasoning: Option<String>,
    pub reasoning_content: Option<String>,
    pub reasoning_details: Option<String>,
    pub codex_reasoning_items: Option<String>,
    pub codex_message_items: Option<String>,
    pub platform_message_id: Option<String>,
    pub observed: i64,
    pub _compressed_summary: i64,
    pub active: i64,
    pub compacted: i64,
    pub api_content: Option<String>,
    pub display_kind: Option<String>,
    pub display_metadata: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewMessage<'a> {
    pub session_id: &'a str,
    pub role: &'a str,
    pub content: Option<&'a str>,
    pub tool_call_id: Option<&'a str>,
    pub tool_calls: Option<&'a str>,
    pub tool_name: Option<&'a str>,
    pub effect_disposition: Option<&'a str>,
    pub timestamp: Option<f64>,
    pub token_count: Option<i64>,
    pub finish_reason: Option<&'a str>,
    pub reasoning: Option<&'a str>,
    pub reasoning_content: Option<&'a str>,
    pub platform_message_id: Option<&'a str>,
    pub api_content: Option<&'a str>,
    pub display_kind: Option<&'a str>,
    pub display_metadata: Option<&'a str>,
}
```

### 7.2 Connection Setup & Durability PRAGMAs

```rust
use rusqlite::{Connection, OpenFlags, Result};
use std::path::Path;

pub fn open_state_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    // Set 1-second busy timeout (application retry logic handles long holds)
    conn.busy_timeout(std::time::Duration::from_millis(1000))?;

    // 1. Attempt WAL mode with fallback check
    let journal_mode: String = conn.query_row("PRAGMA journal_mode=WAL;", [], |r| r.get(0))?;
    let is_wal = journal_mode.to_lowercase() == "wal";

    if is_wal {
        // Truncate WAL back to 64 MiB at checkpoints
        let _: Result<()> = conn.execute_batch("PRAGMA journal_size_limit = 67108864;");

        #[cfg(target_os = "macos")]
        {
            let _: Result<()> = conn.execute_batch(
                "PRAGMA checkpoint_fullfsync = 1;
                 PRAGMA synchronous = FULL;",
            );
        }
    }

    // 2. Performance and safety pragmas
    let _: Result<()> = conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -65536; -- 64MB cache",
    );

    Ok(conn)
}
```

### 7.3 Message Append & Paging Functions

```rust
use rusqlite::{params, Connection, Result, TransactionBehavior};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn append_message(conn: &mut Connection, msg: &NewMessage) -> Result<i64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let ts = msg.timestamp.unwrap_or(now);

    // Use IMMEDIATE transaction behavior to match Python's BEGIN IMMEDIATE
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    tx.execute(
        "INSERT INTO messages (
            session_id, role, content, tool_call_id, tool_calls, tool_name,
            effect_disposition, timestamp, token_count, finish_reason,
            reasoning, reasoning_content, platform_message_id,
            observed, _compressed_summary, active, compacted,
            api_content, display_kind, display_metadata
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            ?7, ?8, ?9, ?10,
            ?11, ?12, ?13,
            0, 0, 1, 0,
            ?14, ?15, ?16
        )",
        params![
            msg.session_id,
            msg.role,
            msg.content,
            msg.tool_call_id,
            msg.tool_calls,
            msg.tool_name,
            msg.effect_disposition,
            ts,
            msg.token_count,
            msg.finish_reason,
            msg.reasoning,
            msg.reasoning_content,
            msg.platform_message_id,
            msg.api_content,
            msg.display_kind,
            msg.display_metadata,
        ],
    )?;

    let msg_id = tx.last_insert_rowid();

    // Bump session message_count and last_activity_at
    let has_tools = msg.tool_calls.is_some() || msg.role == "tool";
    if has_tools {
        tx.execute(
            "UPDATE sessions SET message_count = message_count + 1,
                                 tool_call_count = tool_call_count + 1,
                                 last_activity_at = ?1
             WHERE id = ?2",
            params![ts, msg.session_id],
        )?;
    } else {
        tx.execute(
            "UPDATE sessions SET message_count = message_count + 1,
                                 last_activity_at = ?1
             WHERE id = ?2",
            params![ts, msg.session_id],
        )?;
    }

    tx.commit()?;
    Ok(msg_id)
}

/// Load live session messages (active = 1) strictly ordered by id ASC.
pub fn load_active_messages(
    conn: &Connection,
    session_id: &str,
    after_id: Option<i64>,
    limit: Option<usize>,
) -> Result<Vec<MessageRow>> {
    let mut sql = String::from(
        "SELECT id, session_id, role, content, tool_call_id, tool_calls, tool_name,
                effect_disposition, timestamp, token_count, finish_reason,
                reasoning, reasoning_content, reasoning_details, codex_reasoning_items,
                codex_message_items, platform_message_id, observed, _compressed_summary,
                active, compacted, api_content, display_kind, display_metadata
         FROM messages
         WHERE session_id = ?1 AND active = 1",
    );

    if after_id.is_some() {
        sql.push_str(" AND id > ?2");
    }
    sql.push_str(" ORDER BY id ASC");

    if let Some(lim) = limit {
        sql.push_str(&format!(" LIMIT {lim}"));
    }

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = match after_id {
        Some(aid) => stmt.query(params![session_id, aid])?,
        None => stmt.query(params![session_id])?,
    };

    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push(MessageRow {
            id: row.get(0)?,
            session_id: row.get(1)?,
            role: row.get(2)?,
            content: row.get(3)?,
            tool_call_id: row.get(4)?,
            tool_calls: row.get(5)?,
            tool_name: row.get(6)?,
            effect_disposition: row.get(7)?,
            timestamp: row.get(8)?,
            token_count: row.get(9)?,
            finish_reason: row.get(10)?,
            reasoning: row.get(11)?,
            reasoning_content: row.get(12)?,
            reasoning_details: row.get(13)?,
            codex_reasoning_items: row.get(14)?,
            codex_message_items: row.get(15)?,
            platform_message_id: row.get(16)?,
            observed: row.get(17)?,
            _compressed_summary: row.get(18)?,
            active: row.get(19)?,
            compacted: row.get(20)?,
            api_content: row.get(21)?,
            display_kind: row.get(22)?,
            display_metadata: row.get(23)?,
        });
    }
    Ok(result)
}
```

---

## 8. Summary of Critical Traps & Non-Obvious Invariants

1. **Ordering Invariant (`id ASC`, Never `timestamp`)**:
   [`hermes_state.py:14062-14069`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L14062-L14069). Timestamps regularly jump backwards or duplicate in WSL2, VM suspend/resume, or container environments. Sorting by `timestamp` breaks the adjacency between assistant tool calls and tool responses, causing LLM providers to reject turns with HTTP 400. Always sort by `id ASC`.
2. **`active` Column Nullability Repair**:
   Historical databases created before schema v12 may contain `active IS NULL` on rows. An idempotent `UPDATE messages SET active = 1 WHERE active IS NULL;` on startup prevents historical messages from vanishing from active-filtered queries.
3. **Structured Content Encoding (`\x00json:`)**:
   [`hermes_state.py:12455-12491`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L12455-L12491). Multimodal message content (arrays of text and image payload dicts) is stored with a `\x00json:` prefix. Readers must check for this prefix before treating `content` as plain text.
4. **WAL Journal Sizing (64 MiB Ceiling)**:
   [`hermes_state.py:1220-1256`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L1220-L1256). Omitting `PRAGMA journal_size_limit=67108864;` permanently strands the peak transaction size (e.g. multi-gigabyte vacuum or batch inserts) inside `.state.db-wal` on disk.
5. **macOS `checkpoint_fullfsync` Requirement**:
   [`hermes_state.py:1259-1322`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L1259-L1322). On macOS Darwin, standard `fsync()` does not flush device write caches. To prevent btree corruption across launchd termination, `PRAGMA checkpoint_fullfsync=1;` and `PRAGMA synchronous=FULL;` are mandatory.
6. **Silent Refusal of WAL on macOS NFS/SMB**:
   [`hermes_state.py:1543-1564`](file:///home/eins0fx/development/hermes-agent-port/hermes_state.py#L1543-L1564). SQLite does not raise errors when WAL is refused on Darwin network filesystems; it silently returns `delete`. Never assume `PRAGMA journal_mode=WAL;` worked without inspecting the returned row.
7. **Granular `AFTER UPDATE OF` FTS Triggers**:
   [`hermes_state_common.py:740-745`](file:///home/eins0fx/development/hermes-agent-port/hermes_state_common.py#L740-L745). FTS triggers must specify `AFTER UPDATE OF content, tool_name, tool_calls, role`. A broad `AFTER UPDATE` trigger causes massive write amplification and SQLite write-lock saturation on every background heartbeat or status flag update.
