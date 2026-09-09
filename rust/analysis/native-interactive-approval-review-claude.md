# Native interactive terminal-approval checkpoint review

## Primary disposition after review

The primary lane accepted and fixed findings 1 through 3 before publication.
Approval routing now uses the current admitted durable route instead of the
physical-session fallback or a frozen client field. An RAII guard unlinks a
request whenever its waiter is dropped, and session grants live in the
route-scoped broker so client
eviction does not revoke them. Sender identity is now supplied by the current
tool turn, which also fixes a shared-group risk in the review snapshot where a
frozen client retained its first sender. Finding 4 remains a documented
cross-process configuration-write limitation shared with the Python path; the
native in-process merge is serialized and tested.

Scope: the uncommitted approval-broker checkpoint. Files read: `tool_approval.rs`,
`dangerous_command.rs`, `native_terminal.rs`, `dispatch.rs`, `message.rs`, `main.rs`,
`native_tools.rs`, both goldens, plus the Python oracle sources
(`tools/approval.py`) for the ported functions. Read-only; no production code touched.

This is not a re-run of AGY's contract-oracle work. The pattern corpus, prompt-byte
goldens, and the `_has_allowlist_shell_operator` port were checked against the real
Python (`tools/approval.py:3074-3171`) and are faithful; findings below are the
things the corpus does not cover: wiring, lifecycle, and route identity.

## Verified-correct (the load-bearing requirements)

These held up under inspection, so I am stating them plainly rather than hedging.

- **Typed approval control bypasses the held transcript lease and never enters
  history.** In both ingress paths the broker resolve runs *before* session
  admission and the transcript lease (`dispatch.rs:295-321`, `message.rs:112-135`).
  A `Resolved` or `Unauthorized` reply is delivered out-of-band and `return`s early,
  so it is never written to the transcript. The dispatch test
  (`dispatch.rs:145-308`) proves the point end to end: history ends with exactly
  `["user: please deploy", "assistant: approved answer"]` and contains no
  `approve session`. Prompt-cache prefix stability follows from this: the only
  history mutation from an approval is the deterministic tool-result note
  `"Command required approval (<desc>) and was approved by the user."`
  (`native_terminal.rs:1072`), which is stable given the finding.

- **Exactly-once, FIFO, timeout, cancellation.** The remove-then-send fence
  (`tool_approval.rs:432-448`), the `claim_timeout` reclaim in the `select!` loser
  branch (`tool_approval.rs:388-399, 549-568`), and the balanced `total`/`id_route`
  bookkeeping are correct. The `resolve_all` pop loop cannot panic on its
  `expect("head observed under the same lock")`: `take` is captured from
  `queue.len()` under the same lock that the loop holds, so it never over-pops
  (`tool_approval.rs:431-442`). No std mutex is held across an await.

- **Authorization is defense-in-depth and cannot be spoofed cross-user.** The
  resolve predicate is `principal == sender_id && can_run_command(cfg, msg,
  "approve")` (`dispatch.rs:300-303`, `message.rs:118-121`). `principal` is the
  current turn's sender, so a different group member cannot approve another
  user's command even on a shared route. `Malformed`
  and `Unauthorized` are non-consuming (`tool_approval.rs:80-93, 422-429`), matching
  the Python `sender_unauthorized_drop` / plaintext contract.

- **Native eligibility boundaries are correctly fenced.** `manual` is admitted only
  for `telegram|discord|slack` with `security.tirith_enabled == Some(false)` and a
  valid deny list on unix-local (`main.rs:337-364`); `smart` and Tirith-on both fall
  through to ineligible, and the tool re-checks `Smart` at runtime as
  defense-in-depth (`native_terminal.rs:964-969`). The synchronous HTTP `/message`
  endpoint hardcodes `platform: Platform::Cli` (`message.rs:94`, never overwritten),
  so manual mode is unreachable there; the `ApprovalRequest` warn arm in
  `message.rs:470-472` is defensive dead code, not a live hang path. Good.

## Findings (severity-ranked)

### 1. FIXED: Request-route vs reply-route derivation was asymmetric

- Request time: `route_key = gateway_session_key`, which is the stored
  `session_key` **or `session_id` when that column is empty**
  (`main.rs:833-838`, passed as `session_identity` at `main.rs:860` →
  `native_terminal.rs:178`).
- Reply time: `route_key = store.session_key_for_source(&source)`, which rebuilds
  the key from the inbound source and **never falls back to `session_id`**
  (`dispatch.rs:297`, `message.rs:113`, `session_store.rs:51-65`).

For a normal push session these coincide, because the session was created with
`session_key = session_key_for_source(source)`. They diverge when the stored
`session_key` is empty (fallback path) or when multiplex-profile / grouping
resolution differs between session creation and the reply. On divergence the reply
lands on a route with no pending entry → `NoPending` → the waiter blocks to the full
approval timeout (default 300s) and then denies. It is fail-closed (no wrong-session
resolution, since a mismatched key finds no queue and principal-match still gates),
but a legitimate approval is silently lost and the transcript lease is pinned for the
timeout. The dispatch test masks this by constructing the agent's request route from
`session_key_for_source` itself (`dispatch.rs:1371-1373`), so it never exercises the
`session_id` fallback that production uses.

Fix: derive the request route from the same function as the reply (persist and reuse
the exact `session_key_for_source` value, or drop the `session_id` fallback for the
approval route), so both sides are guaranteed byte-identical.

Disposition: fixed by carrying the current admitted gateway route through the
turn-local tool context. A missing route now fails closed immediately instead
of registering an unresolvable request, and a resumed cached client cannot
retain its former route.

### 2. FIXED: A superseded/aborted turn leaked its pending approval until timeout

`request_with_notify` awaits the oneshot inside the tool call inside `run_turn`
(`native_terminal.rs:1006-1024`). If that future is dropped without a session
rotation, only `rx` drops; the `Pending` entry stays in the broker map. Rotation
does clean up (`cancel_route` at `dispatch.rs:591`, `message.rs:384`) and run
shutdown drains everything (`main.rs:1435`), but a turn that is superseded or aborted
*without* rotating the route leaves the entry live until its timeout. Consequences:
`has_pending()` stays true (it feeds the reset-confirmation gate at
`dispatch.rs:334` / `message.rs:147`), and the entry keeps consuming a per-route
(16) and global (1024) cap slot. No stuck waiter, just leaked state.

Fix: give `request_with_notify` an RAII guard whose `Drop` calls `claim_timeout(&id)`
so a dropped waiter always unlinks its own entry.

Disposition: fixed with that guard and a dropped-waiter regression test.

### 3. FIXED: Session-scope approvals were bound to the cached tool instance

`session_approvals` is a `HashSet` on the `TerminalTool` (`native_terminal.rs:806,
1032-1043`). The tool is built per session and reused via `ConversationAgent`'s cache
(`main.rs:1227-1252`), so an `AllowSession` grant survives across turns *only while
the client stays cached*. Under `agent_cache_pressure` eviction the client is rebuilt
and the set resets, so the user is re-prompted for a pattern they already
session-approved. It is fail-safe (re-prompt, never a silent allow), but it diverges
from Python, where session approval lives in module state
(`approve_session` / `_session_approved` in `tools/approval.py`) independent of any
tool object. Worth documenting as a compat gap; fix only if session persistence
across cache eviction is a requirement.

Disposition: fixed. Session grants are broker-owned by stable route, survive
client eviction, and clear on reset, resume, freshness rotation, or shutdown.

### 4. LOW: Permanent-allowlist persistence is single-process and reorders entries

`persist_permanent_allowlist_sync` serializes writers with
`config_file::config_write_lock()` (`native_terminal.rs:1248`), which is an
in-process guard only. Two gateway processes writing `config.yaml` concurrently can
still clobber each other's allowlist addition (last writer wins). The in-process
concurrent test (`native_terminal.rs:1536-1559`) does not cover the cross-process
case. Secondary: the writer `sort()`s and `dedup()`s the whole `command_allowlist`
(`native_terminal.rs:1281-1282`), reordering a user's hand-ordered list; top-level
comments are preserved by `yaml_edit` (verified by the test) but item order is not.
Both are acceptable under a single-gateway assumption; note them as limitations.

## Residual limitations (not defects)

- The broad plaintext accept set (`yes`, `ok`, `okay`, `confirm`, `y`, `👍`, `no`,
  `cancel`, `👎`, …) at `tool_approval.rs:601-649` means a casual one-word message
  auto-resolves a pending approval. This is **contract-faithful**. The Python
  goldens `sender_authorized_plaintext_{yes,confirm,cancel,…}` accept exactly these,
  so it is intended, not a Rust regression. It only fires when the sender is the
  authorized principal and something is already pending.
- `bash -c '<dangerous-but-metachar-free>'` (e.g. `rm -rf /` inside single quotes)
  remains eligible for a `command_allowlist` glob, because `has_reinterpretable`
  stays false with no shell metachar in the payload. This is an exact port of
  Python's `_has_allowlist_shell_operator` (`tools/approval.py:3080-3142`) and only
  matters if the operator explicitly allowlisted a glob matching it, so it is the
  documented shared contract, not a porting defect.
- `simple_echo_substitution_variants` (`dangerous_command.rs:81-103`) only
  deobfuscates a single-arg `echo` inside `$(...)`/backticks; anything richer relies
  on the regex corpus. In-scope for the contract oracle, out of scope here.

## Bottom line

The broker itself is solid: exactly-once, FIFO, fail-closed, no lock-across-await,
and the transcript-lease bypass with no history mutation is proven. The real risks
are at the wiring seams, not in the primitive. Finding 1 (route-derivation
asymmetry) is the one worth fixing before this lands, because it fails silently and
the current test hides it; findings 2–4 are hardening.
