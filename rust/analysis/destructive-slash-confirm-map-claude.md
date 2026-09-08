# Destructive slash-confirmation primitive: audit and Rust design

Independent audit of Python's destructive gateway slash-confirm primitive and a
narrow Rust design for a complete text fallback now, with a clean seam for
adapter buttons later. Every Python contract below was traced in the working
tree (`tools/slash_confirm.py`, `gateway/run.py`, `cli.py`,
`gateway/platforms/*`, `hermes_cli/config.py`, and the six test files) and
independently corroborated by three source-grounded agents. Every Rust fact was
read from `rust/crates/hermes-gateway/src`. No impl files were touched, no Cargo,
no git.

## Headline

Rust already executes `/new` and its alias `/reset` natively on both ingress
paths through `session_commands::reset_session`, which serializes rotation with
route and transcript leases internally. What is missing is the confirmation UI
the rotation resolution doc explicitly deferred (see
`explicit-session-rotation-resolution.md` lines 32-35). This checkpoint is that
UI, text fallback only. It introduces four things that do not exist in the Rust
port yet: a pending-confirm store, a config gate read, a config writer for the
"Always Approve" opt-out, and a confirm-reply intercept in ingress. Buttons stay
deferred because there is no outbound button or inbound callback surface in the
Rust adapter trait at all.

Two things force real design decisions rather than a mechanical port:

- **R1 (config snapshot).** Python re-reads `config.yaml` live on every gate
  check, so a persisted "Always Approve" takes effect with no restart. Rust
  holds `user_config: Arc<serde_json::Value>` snapshotted at construction
  (`dispatch.rs:41`), never re-read, and has no config writer at all. A naive
  port silently makes "Always Approve" a no-op until restart.
- **R2 (comment-preserving write).** Python persists via
  `atomic_roundtrip_yaml_update` (ruamel), preserving comments, order, and
  quotes, then `chmod 0600`. Rust parses YAML into a JSON `Value` and has no
  round-trip writer, so a straight port would rewrite the user's `config.yaml`
  and drop their comments.

Recommendation: land the store, gate, text fallback, and `/new` wrapping now.
Handle "Always Approve" with an in-process override plus a best-effort durable
write, and until a comment-safe writer exists, fall through to Python's already
specified "could not save that preference" branch rather than clobbering
`config.yaml`. Details in section 8.

---

## 1. The Python primitive: `tools/slash_confirm.py`

Module-global state, deliberately not hung off the runner so platform callbacks
can resolve without a backreference:

- `_pending: Dict[session_key -> {confirm_id, command, handler, created_at}]`,
  guarded by a `threading.RLock`.
- `DEFAULT_TIMEOUT_SECONDS = 300`.

Functions and their exact contracts:

- `register(session_key, confirm_id, command, handler)` overwrites any prior
  pending entry for the same `session_key`. This is the supersession rule: a new
  confirmable command retires the stale one.
- `get_pending(session_key)` returns a **copy** of the dict or `None`. It never
  leaks the live entry.
- `clear(session_key)` drops without running.
- `clear_if_stale(session_key, timeout=300)` drops if
  `now - created_at > timeout`, returns `True` if it dropped one.
- `resolve(session_key, confirm_id, choice, timeout=300)`:
  1. `None` if no entry.
  2. `None` if `entry.confirm_id != confirm_id` (stale / superseded prompt).
  3. **Pops the entry before running the handler** to make double-resolve
     (button double-tap, duplicate webhook) a no-op. This is the exactly-once
     guarantee (`slash_confirm.py:120-124`).
  4. Re-checks the timeout after popping; `None` if expired.
  5. Runs `await handler(choice)`; on exception logs and returns
     `f"❌ Error handling confirmation: {exc}"`; otherwise returns the handler's
     string (or `None` if it did not return a string).
- `resolve_sync_compat(loop, ...)` schedules `resolve` onto a loop from another
  thread and blocks up to 30s. For platform callbacks that run off the event
  loop.

`choice` is always one of `"once"`, `"always"`, `"cancel"`.

## 2. The gateway wrapper: `_maybe_confirm_destructive_slash`

`gateway/run.py:26585`. Signature
`(event, command, title, detail, execute)`, where `execute` is an async
callable performing the destructive action.

- **Gate.** Reads `_read_user_config()` (which calls
  `hermes_cli.config.load_config()` fresh, so on-disk changes apply with no
  restart) and takes `approvals.destructive_slash_confirm`, **default `True`**.
  If `False`, `execute()` runs immediately and returns.
- **Session key.** `session_key = self._session_key_for_source(event.source)`,
  the same derivation the rest of the gateway uses (`run.py:8596`), namespaced
  by profile and DM/group/thread isolation via `build_session_key`.
- **`_on_confirm(choice)`** (the handler registered into the primitive):
  - `"cancel"` returns `f"🟡 /{command} cancelled. Conversation unchanged."`,
    execute never runs.
  - `"always"` calls `cli.save_config_value("approvals.destructive_slash_confirm", False)`,
    logs success or a not-writable warning, then runs `execute()`.
  - `"once"` and `"always"` both run `execute()`.
  - On `"always"`, appends a note to the string result. Persisted branch:
    `"\n\nℹ️ Future /clear, /new, /reset, and /undo will run without confirmation. Re-enable via \`approvals.destructive_slash_confirm: true\` in config.yaml."`
    Not-persisted branch:
    `"\n\n⚠️ Could not save that preference (config.yaml is not writable), so /clear, /new, /reset, and /undo will ask again next time. To silence it permanently, set \`approvals.destructive_slash_confirm: false\` in config.yaml."`
    A non-string result (EphemeralReply) is passed through untouched so the note
    does not mangle a structured reply.
- **Prompt text.** Built with the platform's typed prefix
  `_p = _typed_command_prefix_for(platform)` (`"/"` default, `"!"` on
  Slack/Matrix). The message is a `⚠️ Confirm /{command}` header, `detail`,
  three bullet options (Approve Once / Always Approve / Cancel), and a text
  fallback line: `_Text fallback: reply \`{_p}approve\`, \`{_p}always\`, or \`{_p}cancel\`._`.
- Hands off to `_request_slash_confirm`.

`_request_slash_confirm` (`run.py:26697`):
- `confirm_id` comes from a per-runner monotonic `itertools.count(1)`
  (`_slash_confirm_counter`).
- **Registers the pending confirm first**, then calls
  `adapter.send_slash_confirm(...)`. Registering before sending closes the race
  where a fast button click arrives before the send returns.
- If the adapter returns a `SendResult` with `.success` truthy, buttons
  rendered, so the ack is `None` (no redundant text). Otherwise the prompt
  `message` itself is returned as the direct text reply. That is the text
  fallback.

Call sites: `/new` at `run.py:19761` and `/undo` at `run.py:19953`; `/reset` is
an alias of `/new` (`hermes_cli/commands.py:152`). `/clear` is CLI-only. So the
gateway destructive set is `/new`, `/reset` (alias), `/undo`.

## 3. Text-fallback intercept (the path this checkpoint ports)

`gateway/run.py:19243-19291`, early in `_handle_message`:

- Runs only when `event.allow_gateway_control` is set, a pending confirm exists
  for `_quick_key = _session_key_for_source(source)`, and **no blocking tool
  approval is live** for that key (`tools.approval.has_blocking_approval`).
- **Tool-approval precedence.** If a tool approval is waiting inside
  `tools/approval.py`, `/approve` there unblocks the tool thread; slash-confirm
  only catches `/approve` when no tool approval is live (`run.py:19249-19252`).
- Recognized replies (leading `!`/`/` stripped, lowercased):
  - once: `approve`, `yes`, `ok`, `confirm`, `once`, `approve once`
  - always: `always`, `remember`, `always approve`
  - cancel: `cancel`, `no`, `deny`, `nevermind`
  - On a match it calls `resolve(_quick_key, pending.confirm_id, choice)` and
    returns `resolved or ""`.
- If a confirm is pending but the reply is unrecognized, it calls
  `clear_if_stale(_quick_key)` so a stale confirm never blocks normal use, and
  falls through to normal dispatch.

Related precedence: a pending slash-confirm also lets a message past the global
emergency-stop pause (`run.py:19061-19064`).

## 4. Adapters and the button surface (Python)

- Base hook `gateway/platforms/base.py:4568` `send_slash_confirm(...) -> SendResult`.
  Default returns `SendResult(success=False, error="Not supported")`, the "no
  buttons, use text fallback" sentinel. `SendResult` is a dataclass whose only
  asserted field here is `success`.
- Despite the docstring naming Telegram/Discord/Slack/Matrix/Feishu, the only
  `gateway/platforms/*.py` overrides are **WhatsApp Cloud** and the **relay
  adapter**. Telegram's button render lives in its plugin adapter (its test,
  `test_telegram_slash_confirm.py`, exercises MarkdownV2 escaping of the preview).
  The others fall through to text.
- **WhatsApp Cloud** (`whatsapp_cloud.py:905`) encodes three reply-button ids of
  exact form `sc:<choice>:<confirm_id>` with `choice` in
  `{once, always, cancel}`, labels `✅ Approve Once` / `🔒 Always` / `❌ Cancel`.
  Inbound: split on `:`, `session_key = _slash_confirm_state.pop(confirm_id)`
  (**pop before resolve**, so a duplicate webhook finds nothing and cannot fire
  twice), validate the choice, then `slash_confirm.resolve(session_key, confirm_id, choice)`.
  `_slash_confirm_state` is a FIFO-bounded `OrderedDict` capped at 1000.
- **Relay adapter** (`gateway/relay/adapter.py:2965`) renders options with ids
  `once` / `always` / `cancel` and resolves through the same primitive.
- Canonical choice strings passed to `resolve` across every path: `"once"`,
  `"always"`, `"cancel"`.

## 5. CLI variant (out of scope for the gateway checkpoint)

`cli.py` uses a separate synchronous confirmer `_confirm_destructive_slash`
(`cli.py:14621`): a prompt-toolkit modal over a `queue.Queue`, numeric mapping
`1/2/3 -> once/always/cancel`, inline skip tokens `now`, `--yes`, `-y`,
`always` persisting `approvals.destructive_slash_confirm: false`, plus Windows
thread-safety guarantees (degrade to a clean cancel on scheduling failure, never
a blocking raw `input()`, snapshot capture/restore around the modal). It has no
async `_pending` registry. The Rust gateway is not the interactive CLI, so this
variant is noted for parity but not ported here.

## 6. Config write and gate-read semantics (Python)

- `cli.save_config_value(key_path, value) -> bool` (`cli.py:5042`): resolves
  `get_hermes_home()/config.yaml` live (not import-time), `mkdir -p` the parent,
  `atomic_roundtrip_yaml_update` (comment/order/quote preserving), `chmod 0600`
  (the file holds API keys), returns `True`/`False`. It deliberately never
  writes the repo's `cli-config.yaml` template.
- The gate is read through `hermes_cli.config.load_config()`, which deep-merges
  `DEFAULT_CONFIG` so a legacy config missing the key still resolves
  `destructive_slash_confirm = True`. `DEFAULT_CONFIG["approvals"]["destructive_slash_confirm"] is True`.

## 7. Rust today (read from the working tree)

- `/new` (+ `/reset` alias) execute natively on both ingress paths:
  `dispatch.rs:286-317` (push, replies via `self.deliver(&msg, String)`) and
  `message.rs:129-158` (HTTP, replies via `MessageResponse { reply: String }`).
  Both call `session_commands::reset_session`, which acquires route and
  transcript leases and does a compare-and-swap store publish internally
  (`session_commands.rs:13-89`). It **already serializes with in-flight turns**,
  so the prior R1 pre-lease race from the rotation audit does not reappear: the
  confirm layer wraps `reset_session`, it does not replace the lease.
- `/compact` and `/sessions` return `Unavailable` replies (`slash.rs:78-83`).
  `/undo` and `/clear` are **not native in Rust yet**, so the Rust destructive
  set for this checkpoint is `/new` and `/reset` only.
- No confirmation, approval, or pending-map scaffolding exists. `pending_messages.rs`
  and `pending_stt.rs` are unrelated. `MessageEvent.prompt_response` and
  `allow_gateway_control` (`platform_base_types.rs:159-190`) are receive-only
  plumbing with no renderer behind them.
- Config is read-only: `load_config`/`load_gateway_config` parse into a JSON
  `Value`; there is no `save_config_value`, no atomic YAML write, no live
  re-read, and `approvals` is never referenced. `user_config` is a frozen
  `Arc<Value>` on the dispatcher (R1).
- Session key: `store.session_key_for_source(source)` (`session_store.rs:45`)
  wrapping `build_session_key` (`session.rs:341`), the same key `reset_session`
  keys its route lease on.
- Adapter trait `PlatformAdapter` (`platform.rs:24-34`) has only
  `name`/`run`/`send(&Message)`. No inline-keyboard, reply-markup, callback, or
  answer-callback surface exists anywhere. Buttons are genuinely greenfield.
- Insertion point: between the `NativeSlashCommand::Reset` classification and
  the two `reset_session` call sites, keyed by `session_key_for_source`.

---

## 8. Rust design (this checkpoint)

### 8.1 The pending-confirm store `slash_confirm.rs`

A direct analog of the Python module, process-local so a future adapter callback
can resolve without a runner backreference.

```rust
pub enum Choice { Once, Always, Cancel }

// FnOnce so taking it out of the map is the exactly-once fence.
type ConfirmHandler =
    Box<dyn FnOnce(Choice) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + Send>> + Send>;

struct Pending {
    confirm_id: u64,
    command: String,
    created_at: Instant,
    handler: ConfirmHandler,
}

pub struct SlashConfirmStore {
    inner: Mutex<HashMap<String, Pending>>, // std Mutex, never held across .await
    counter: AtomicU64,
}
```

- `register(key, command, handler) -> u64`: mint `confirm_id` from `counter`,
  insert (overwrite = supersession), return the id.
- `pending_meta(key) -> Option<PendingMeta>`: copy of `{confirm_id, command,
  created_at}` only, never the handler (mirrors `get_pending` returning a copy).
- `clear(key)`, `clear_if_stale(key, timeout) -> bool`.
- `resolve(key, confirm_id, choice, timeout) -> Option<String>`:
  1. Lock, look up. `None` if absent.
  2. `None` if stored `confirm_id != confirm_id` (supersession).
  3. **Remove the entry (take the handler) before unlocking.** This is the
     exactly-once fence.
  4. Re-check `now - created_at > timeout` after removal; `None` if expired.
  5. Unlock, then `handler(choice).await`. `Ok(Some(s)) -> Some(s)`,
     `Ok(None) -> None`, `Err(e) -> Some(format!("❌ Error handling confirmation: {e}"))`.

The handler returns `Result` rather than relying on panic capture, because the
handler runs off-lock and Rust does not offer a clean async catch. A panic
inside the handler cannot poison the Mutex since the lock is already released;
still, prefer `parking_lot::Mutex` (no poison) or explicit poison handling to be
safe. `DEFAULT_TIMEOUT_SECONDS = 300`.

Bounding: keyed one-entry-per-session with overwrite, so growth is only across
distinct sessions. Add a lazy stale sweep on `register` (or a small cap like
WhatsApp's 1000) so an abandoned confirm on a session that never speaks again is
eventually reclaimed. Python leans on `clear_if_stale` firing on the next
message; a cap is the defensive equivalent for sessions that go silent.

### 8.2 The gate + prompt layer (gateway-side)

Mirror `_maybe_confirm_destructive_slash` as a helper on the dispatcher and the
HTTP handler (or a free function taking the pieces both hold):

```rust
enum ConfirmOutcome { RunNow, Prompt(String) } // Prompt text is the text-fallback ack
```

- Read the gate: `approvals.destructive_slash_confirm`, default `true`. See R1
  below for how to read it live rather than from the frozen `Arc`.
- If off: `RunNow` (call `reset_session` unchanged).
- If on: build the prompt string with the platform's typed prefix (port
  `_typed_command_prefix_for`; `"/"` default, `"!"` for Slack/Matrix), register
  a handler that captures the reset inputs (`source`, `owner_key`, `title`, the
  admission deps) and:
  - `Once` / `Always`: run `reset_session(...)`, return its reply.
  - `Always`: persist the opt-out (R1/R2), append the persisted or
    not-persisted note.
  - `Cancel`: return `🟡 /{command} cancelled. Conversation unchanged.`,
    reset never runs.
  Return `Prompt(message)` and deliver it.

Reuse the existing reset reply text (`✨ Session reset! Starting fresh.` /
`✨ New session started!`); the "always" note appends to it.

### 8.3 Ingress ordering

Both `dispatch.rs` and `message.rs` need two edits, in this order:

1. **Confirm-reply intercept, before `slash::evaluate`.** At the top of turn
   handling, compute `key = store.session_key_for_source(source)`, and if the
   confirm store has a pending entry for `key`, test the inbound text against the
   recognized reply keywords (section 3). On a match, `resolve` and reply,
   return. On a pending-but-unrecognized reply, `clear_if_stale(key)` and fall
   through. Guard this on the same control-legitimacy condition Python uses
   (`allow_gateway_control`); the dispatcher's `Message` lacks that flag today,
   so gate it on the same predicate that already lets built-ins run and exclude
   programmatic senders (API/webhook). **Flag:** wiring the equivalent of
   `allow_gateway_control` into the `Message` dispatch path is a small
   prerequisite; do not let a programmatic caller resolve another session's
   confirm.
2. **Wrap the reset execution.** Replace the direct `reset_session` call under
   `NativeSlashCommand::Reset` with the gate layer: `RunNow` calls
   `reset_session` exactly as today; `Prompt(text)` registers and delivers the
   prompt.

Because `reset_session` takes its own leases, the confirm layer holds no lease
and touches no store state. Serialization against in-flight turns is unchanged.

### 8.4 R1 / R2 resolution (the "Always Approve" persistence)

- **Gate read (R1).** Do not read the frozen `Arc`. Add a small live read of
  `approvals.destructive_slash_confirm` from `config.yaml` at gate time (cheap,
  mirrors Python's `load_config()` per check), or maintain a process-local
  override cell (`AtomicBool` / `ArcSwap`) updated whenever "Always Approve"
  persists. Recommend the live read for exact Python parity (no-restart), with
  the override cell as the in-process fast path. Absent key defaults to `true`
  (deep-merge default), so also confirm the Rust default loader supplies that
  default, matching `test_existing_user_config_without_key_gets_default`.
- **Durable write (R2).** Two honest options:
  - **Interim (recommended for this checkpoint):** update the in-process
    override so "Always Approve" holds for the process, and skip the durable
    write until a comment-preserving writer exists. Emit Python's already
    specified not-persisted note so the user is told the preference will be asked
    again after restart. This never clobbers their `config.yaml` comments and
    ships the full three-way UX now.
  - **Full:** add an atomic YAML writer (temp file, rename, `chmod 0600`,
    parent `mkdir -p`, target live `HERMES_HOME/config.yaml`) and emit the
    persisted note. `serde_yaml` round-trips lose comments, so a comment-safe
    write needs a round-trip crate or a targeted line edit. That is the real
    cost and the reason to consider deferring it.

Do not ship a writer that drops user comments silently. Either preserve them or
use the not-persisted branch.

---

## 9. Security and concurrency review

- **Exactly-once.** Remove-before-run under the Mutex makes a double reply,
  button double-tap, or duplicate webhook a no-op. The `confirm_id` check
  rejects a resolve against a superseded prompt. This matches the Python pop and
  the WhatsApp adapter pop; the Rust store is the single fence, so an adapter
  callback added later inherits it for free.
- **Supersession.** `register` overwrites, so a newer destructive command
  invalidates the old `confirm_id`; a late reply to the old prompt resolves to
  `None`. Benign here because every destructive command shares one gate key.
- **Timeout.** 300s; cleared on the next unrelated message and re-checked inside
  `resolve`. Bound or lazily sweep the map so silent sessions do not pin entries.
- **Session-key scoping / IDOR.** Keyed by `session_key_for_source`, the same
  profile and DM/group/thread namespace sessions use. A confirm in one chat
  cannot be resolved from another. Unlike `/resume`, the destructive action
  targets the caller's own session, so there is no cross-owner authority gate to
  add. The one real exposure is a programmatic sender resolving a confirm: keep
  the intercept behind the control-legitimacy guard (section 8.3, flag 1).
- **Config write.** If the full writer lands, preserve `0600` (the file holds
  API keys) and write atomically (temp + rename) so a crash mid-write cannot
  truncate `config.yaml`. The interim path writes nothing, so it carries no such
  risk.
- **Concurrency.** The std Mutex is held only around map operations and never
  across an `.await`. The handler runs off-lock; `reset_session` leases
  internally, so two concurrent resolves cannot double-rotate (one wins the
  remove, the other sees an empty slot and returns `None`). Prefer a non-poison
  Mutex.

---

## 10. Tests

Port the Python contracts, not the Python mechanics. HTTP tests drive
`message.rs`; push tests drive `dispatch.rs` via a capturing `deliver`.

**Unit, `slash_confirm.rs`** (mirrors `tests/tools/test_slash_confirm.py`):

- register stores; `pending_meta` returns a copy of metadata only, never the
  handler.
- resolve runs the handler once and removes the entry; a second resolve returns
  `None`.
- two concurrent resolves (tokio `join`) on one `(key, confirm_id)`: exactly one
  runs the handler, exactly one gets the result, the other gets `None`.
- resolve with no pending returns `None`.
- resolve with a mismatched `confirm_id` returns `None` (supersession).
- resolve after the timeout returns `None` (backdate `created_at`).
- `clear` removes; `clear_if_stale` drops a backdated entry and returns `true`,
  returns `false` for a missing key.
- a handler returning `Err` yields `❌ Error handling confirmation: ...`.

**Gate / wrapper unit** (mirrors `tests/gateway/test_destructive_slash_confirm.py`
and `tests/hermes_cli/test_destructive_slash_confirm_gate.py`):

- gate on: a `/new` registers a pending entry with `command == "new"` and does
  not rotate yet.
- resolve `always`: rotation runs, the opt-out is applied (override set, and
  durable write attempted), the reply contains the reset text and the config.yaml
  note.
- resolve `once`: rotation runs, no opt-out persisted.
- resolve `cancel`: `🟡 /new cancelled. Conversation unchanged.`, no rotation.
- gate off: rotation runs immediately, no pending entry.
- default gate is `true` when `approvals.destructive_slash_confirm` is absent.

**Text recognition unit:** each literal (`approve`, `yes`, `ok`, `confirm`,
`once`, `approve once`; `always`, `remember`, `always approve`; `cancel`, `no`,
`deny`, `nevermind`), with and without a leading `/` or `!`, maps to the right
choice.

**HTTP integration, `message.rs`:**

- gate on, POST `/new`: response is the prompt text, session id unchanged; then
  POST `/approve`: session id rotates and the reset reply comes back.
- gate on, POST `/new` then `/cancel`: no rotation, cancelled message.
- gate on, POST `/new` then `/always`: rotation plus the note (persisted or
  not-persisted per section 8.4).
- gate off, POST `/new`: rotates immediately.
- POST `/reset` behaves as `/new` (alias).

**Push integration, `dispatch.rs`** (capture `deliver`):

- `/new` delivers the prompt; `/approve` delivers the reset confirmation and
  rotates; an unrelated message while a confirm is pending clears the stale entry
  and dispatches normally.
- two concurrent `/approve` deliveries for one pending confirm rotate exactly
  once.
- placeholder assertion documenting tool-approval precedence: once a Rust tool
  approval store exists, a live tool approval must suppress the confirm
  intercept. No tool approval exists yet, so this is a documented seam, not a
  live test.

---

## 11. This checkpoint vs later

**In this checkpoint (text fallback):**

- `slash_confirm.rs` store: register / pending_meta / clear / clear_if_stale /
  resolve, with the remove-before-run fence, timeout, and supersession.
- Live-ish gate read of `approvals.destructive_slash_confirm` (default true).
- Wrap `/new` and `/reset` behind the gate on both ingress paths.
- Text-fallback prompt plus the confirm-reply intercept (once/always/cancel
  keyword recognition), behind a control-legitimacy guard.
- once -> reset; cancel -> cancelled message; always -> opt-out (in-process
  override now, durable write per 8.4) + reset + note.
- The unit, HTTP, and push tests above.

**Deferred:**

- Adapter buttons: a `send_slash_confirm(...) -> SendResult` trait method
  (default `success=false` -> text fallback) and inbound callback parsing with
  ids `sc:<choice>:<confirm_id>`, resolving through the same store. No outbound
  button or inbound callback surface exists in the Rust adapter trait yet, so
  this is a larger, separate piece. The store's `resolve` is already the shared
  primitive both the text and button paths will call, so the button work is
  purely adapter plumbing.
- `/undo` and `/clear`: not native in Rust yet; their confirm wrapping arrives
  with the commands themselves.
- The comment-preserving atomic config writer, if the interim in-process
  override ships first.
- The CLI modal variant (belongs to the interactive CLI, not the gateway).

---

One process note: I added a one-line entry to `INDEX.md` for this report,
matching how `explicit-session-rotation-map-claude.md` and the other analysis
artifacts stay discoverable. A couple of earlier sessions deliberately left
`INDEX.md` untouched; say the word and I will drop the line.
