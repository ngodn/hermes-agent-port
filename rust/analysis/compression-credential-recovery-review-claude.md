# Review: native auxiliary compression API-key pool recovery

Post-implementation correctness/security review of the uncommitted diff across
`auth_store.rs`, `credential_pool.rs`, `main.rs`, `native_agent.rs`. Oracle:
`agent/auxiliary_client.py`, `agent/credential_pool.py`, `hermes_cli/auth.py`.
No production code was edited.

Scope note: line numbers below are from the current working tree, which is a
touch newer than the `git diff` snapshot (a parallel lane refined
`request_client`/`rotate_after_failure` after the status snapshot). I reviewed
the tree as it stands.

## Verdict: SHIP

The state machine, persistence merge, byte stability, and the 401/402/429
recovery ladder are faithful ports and are golden-tested. The primary lane
resolved M1 before the review completed, then moved the blocking store work
identified in L1 onto Tokio's blocking pool. The remaining notes are low
severity, deliberate checkpoint limits, or coverage follow-ups.

### Primary dispositions

- **M1 resolved:** pool-backed 401 handling now calls `mark_unhealthy` only
  when the binding has no successor. Non-pool candidates retain the original
  quarantine behavior.
- **L1 resolved:** pool reload, flock acquisition, mutation, and persistence
  now run inside `spawn_blocking`; no async worker performs the 15-second lock
  wait.
- **L2 retained:** quarantining duplicate rows for the actual second failed key
  prevents the same bad secret from cycling under another ID.
- **L3 retained:** the provider set remains conversation-frozen, while failure
  recovery re-reads the durable pool. This preserves prompt-cache stability.
- **L4 retained:** the shared atomic writer intentionally preserves user-owned
  symlinks. File mode is forced to `0600`; normal Hermes home creation owns
  parent-directory hardening.
- **L5 resolved:** a focused HTTP integration now proves that 402 rotates
  immediately while ordinary 429 retries the failed key once before rotation.

---

## M1 (Medium): 401 on a pool provider quarantines the whole provider for 600s even when the pool still has usable keys

`native_agent.rs:1867-1875` (the auth arm) and `1877-1881` (the payment arm)
call `discovery.mark_unhealthy(client.provider_name())` unconditionally once
`recover_summary` (`native_agent.rs:597`) gives up. `recover_summary` only ever
tries two keys per attempt: the key that already failed in the main loop, then
one rotation (`rotate_after_failure`, `native_agent.rs:578`). For a pool with 3+
independently-valid keys the recovery ladder cannot drain the pool, so after
key1 (401) → rotate → key2 (401) it returns `Err`, and the auth arm then marks
the provider unhealthy via `compression_discovery::Health` for the 600s TTL.
Every later discovery route for that provider is then skipped by the
`is_unhealthy` gates at `native_agent.rs:1683` and `:1703`, so key3+ (still
`available` in the pool, cooldown never written for them) are stranded until the
Health TTL expires.

This diverges from Python. `_mark_provider_unhealthy` (`auxiliary_client.py:4562`,
600s TTL) is called ONLY on a confirmed payment error
(`auxiliary_client.py:11081-11089`); the auth branch sets `reason = "auth error"`
(`:11079`) and never quarantines. Python re-runs `load_pool` fresh on the next
aux call and selects key3. The Rust auth-arm quarantine predates this diff (it
was correct when a discovery candidate held a single frozen env key), but wiring
a rotatable pool behind that same candidate is exactly what makes it wrong now.

Runtime consequence: openrouter (or any api-key discovery provider) with 3+
pooled keys where the first two are expired/revoked (401) gets blacklisted for
10 minutes, compression falls back to a worse provider or fails, and the healthy
keys go unused. The 402 arm matches Python (account-level credit exhaustion, all
keys share the account), so leave it.

Smallest fix: for pool-backed candidates, gate the auth-arm `mark_unhealthy` on
the pool being actually drained. After `recover_summary` runs, the binding's
`current()` is `None` exactly when the pool has no next key (`rotate_after_failure`
sets `*self.current = next` and `next` is `None` only on exhaustion,
`native_agent.rs:518-522`); when `current()` is still `Some`, keys remain, so
skip the quarantine and let per-key cooldowns do their job. Do not touch the
non-pool (`binding == None`) path; there Health is the only backstop and the
current behavior is correct.

Original verdict: CONFIRMED. Reachable whenever a discovery pool holds ≥3 keys and the
first two return 401; two-key and single-key pools correctly end with
`current() == None` and quarantine as before.

Disposition: RESOLVED before commit.

---

## L1 (Low): blocking file I/O and a 15s flock spin run on the async worker without spawn_blocking

`recover_summary` (`native_agent.rs:597`, `async`) calls the synchronous
`rotate_after_failure` → `PoolLocator::mark_exhausted_and_rotate`
(`credential_pool.rs:985`) → `auth_store::write_pool` (`auth_store.rs:265`).
`write_pool` takes a process-global `std::sync::Mutex` (`WRITE_LOCK`,
`auth_store.rs:271`) held across the whole read-modify-write, then an advisory
`flock` that busy-waits with `std::thread::sleep(50ms)` up to a 15s deadline
(`auth_store.rs:142-173`). None of this is wrapped in `tokio::task::spawn_blocking`,
so under cross-process contention on `auth.json` a tokio worker thread is
blocked (not just the task). On the uncontended local-file common case this is
sub-millisecond, which is why it is Low, but the worst case stalls unrelated
work on that worker for up to 15s. Python does equivalent locking but from
sync/thread context, so it has no event-loop to starve.

Smallest fix: run the load/mutate/persist inside `spawn_blocking`, or drop the
flock deadline to a second or two (the process `WRITE_LOCK` already serializes
in-process writers).

Original verdict: PLAUSIBLE (depends on real multi-process contention).

Disposition: RESOLVED before commit with `spawn_blocking`.

---

## L2 (Low): second-failure recovery marks siblings that Python leaves untouched

In `recover_summary`'s post-rotation failure path
(`native_agent.rs:642-645`) the code re-enters `rotate_after_failure(index, &error)`,
which passes the rotated key's `id` + `api_key` as identity
(`native_agent.rs:511-516`). In `mark_exhausted_and_rotate` that makes
`identity_supplied == true`, so the sibling-marking loop
(`credential_pool.rs`, the `identity_supplied && !failed_key.is_empty()` block)
marks every entry sharing the rotated key. Python's equivalent second recovery
(`auxiliary_client.py:11017`, `_recover_provider_pool(pool_provider, retry2_err)`)
passes NO `failed_api_key`, so `identity_supplied` is `False` and siblings are
not marked. The divergence only matters when two pool rows share one runtime key
(rare for discovery pools), and Rust's behavior is arguably the safer one
(marking a shared-key sibling exhausted is correct). No action needed; noted for
completeness.

Verdict: CONFIRMED but benign.

---

## L3 (Low): the selected pool key is frozen at conversation-build time, not re-read per compression call

`build_native_compression_discovery` (`main.rs:522` region) calls
`locator.select_runtime()` once at startup and caches the resulting
`RuntimeCredential` in `CompressionPoolCredential.current`
(`Arc<Mutex<Option<RuntimeCredential>>>`, `native_agent.rs`). Python's
`_select_pool_entry` (`auxiliary_client.py:1582`) re-loads and re-selects on
every aux call. Consequence: if another process exhausts or rotates the selected
key between build and the first compression, Rust spends one doomed request on
the stale key before `recover_summary` rotates (self-healing); and a
better/newly-added key is not adopted mid-conversation absent a failure. This is
the documented "frozen candidate SET vs recoverable KEY" checkpoint from the
seam analysis, so it is expected, not a defect.

Verdict: CONFIRMED, by design.

---

## L4 (Low): symlink write-through diverges from Python's `_save_auth_store`

`write_pool` persists via `atomic_file::write_private_preserving_symlink`
(`auth_store.rs:236` / `atomic_file.rs:17`), which resolves the symlink and
writes THROUGH to the canonical target, preserving the link
(test `pool_write_is_private_and_preserves_auth_symlink`, `auth_store.rs`).
Python's `_save_auth_store` (`hermes_cli/auth.py:1471`) does `atomic_replace`
over the path name, which DETACHES a symlink (replaces it with a regular file).
For the normal profile model auth.json is a real file (Python's #100339
write-through machinery exists precisely because profile and root are separate
files), so this only matters if a user hand-symlinks auth.json. Rust's behavior
is the safer one for a legit shared-credential symlink; the theoretical security
angle (following an attacker-planted symlink to write 0o600 creds + `fchown`)
requires the attacker to already own `~/.hermes`. This is a pre-existing
convention reused by the diff, not introduced here. Both writers force file
mode 0o600. Python also attempts to harden the parent to 0o700; this Rust writer
relies on the already-created Hermes home and does not repeat that chmod.

Verdict: CONFIRMED divergence, negligible impact.

---

## L5 (Low): no end-to-end test for the 429 cheap-retry and 402 no-retry branches

The one integration test, `compression_pool_persists_failure_before_fresh_client_retry`
(`native_agent.rs`), exercises only the 401 path (key-one returns 401). The
distinct behaviors in `recover_summary`, the single cheap retry on the same key
for an ordinary 429 (`native_agent.rs:610-625`) and the no-cheap-retry-for-402
path, are covered only indirectly by pool-level goldens, not through
`summarize_history`. Add a 429 case (assert exactly two requests on the failing
key before rotation) and a 402 case (assert no cheap retry) at the integration
level.

Verdict: CONFIRMED coverage gap.

Disposition: RESOLVED before commit by
`compression_pool_429_retries_failed_key_but_402_rotates_immediately`.

---

## Items checked and found correct (not reproducible as defects)

1. **Profile/root isolation, no cross-conversation/profile key leak.** The
   `PoolLocator` sink always writes to `self.profile` (the profile auth.json,
   `main.rs` build block); `root_auth` is load-only fallback. `RuntimeCredential`
   omits `Debug` (`credential_pool.rs`), error contexts are run through
   `compression_redact::redact` before storage/logging
   (`native_agent.rs:508`), and the raw key is never persisted as an
   `api_key_hint` (it is only used to match an entry). Single-use-refresh and
   `custom:` providers are bailed by `load_pool_from_store`
   (`credential_pool.rs:828-830`), so the #100339 root write-through fork cannot
   be triggered from this path.

2. **No lock held across await.** `CompressionPoolCredential.current` is a
   `std::sync::Mutex` locked only to clone/swap (`native_agent.rs:492-498`,
   `518-522`); the `.await` points in `recover_summary` hold no guard. The
   `WRITE_LOCK`/`flock` live entirely inside the synchronous `write_pool`. (The
   fact that this synchronous section runs on the async worker is L1, a
   throughput concern, not a correctness/deadlock one.)

3. **Auth-store read/modify/write race and cooldown merge.** `write_pool`
   re-loads under `WRITE_LOCK` + `flock`, re-merges disk rows by id, and applies
   `merge_newer_disk_status` (`auth_store.rs:199`), which is a faithful port of
   `_merge_disk_cooldown_state` (`hermes_cli/auth.py:2295`): adopt disk status
   only when disk is dead/exhausted, the access_token matches (or one side is
   empty), disk `last_status_at` is strictly newer, and (for exhausted) the
   cooldown is still binding. `_POOL_STATUS_FIELDS` (`auth.py:2285`) matches the
   Rust `STATUS_FIELDS` list exactly. Concurrent-add and rotate+remove are
   covered by `pool_writes_merge_concurrent_rows_and_newer_cooldowns`.
   `AUTH_STORE_VERSION == 1` (`auth.py:109`) matches the hardcoded `version` in
   `write_pool`.

4. **Exact 401/402/429/duplicate-key/stale-id/bounded-retry behavior.**
   `mark_exhausted_and_rotate` (`credential_pool.rs:1374`) mirrors
   `credential_pool.py:2637`: stale-id-vs-key disagreement trusts the key
   (#79156), unmatched-identity streak is capped at one lap of available entries
   (#70401, single-entry pools escape), duplicate runtime keys are marked as
   siblings, and DEAD vs EXHAUSTED follows `_is_terminal_auth_failure`
   (`credential_pool.py:1090`). The recovery ladder in `recover_summary` mirrors
   `auxiliary_client.py:10968-11020`: one cheap retry for ordinary 429 only,
   immediate rotation for 401/402, and no third rotated request. Cooldown TTLs
   (401→300s, 402/billing→3600s, 429→60s) and the terminal-DEAD case are
   asserted by `failure_status_and_cooldown_are_recorded_before_rotation`; the
   disagreement/sibling/streak cases by
   `request_failure_rotation_matches_compression_oracle` against
   `recovery_401_refresh_and_rotation` goldens.

5. **Poisoned reqwest client replacement.** `with_runtime_credential`
   (`native_agent.rs:827`) rebuilds `self.client` from a fresh builder, dropping
   the old (possibly half-open) connection pool. No configuration is lost: the
   base client is also built bare (`native_agent.rs:779`) and the per-request
   timeout is applied at send time from `self.summary_timeout`
   (`native_agent.rs:1260`), while `provider_headers`, model, and caps are left
   intact. `request_client` (`native_agent.rs:558`) now short-circuits the
   rebuild when the key and base_url are unchanged, so a client is only rebuilt
   on an actual rotation.

6. **Prompt/message byte stability and tool-free compression.**
   `with_runtime_credential` changes only key/base_url/client and resets
   `compression_routes` to default (prevents recursive discovery), leaving the
   assembled prompt untouched. `compression_pool_persists_failure_before_fresh_client_retry`
   asserts `bodies[0] == bodies[1]` across the failing and rotated requests and
   that `body.get("tools").is_none()`.

7. **Production startup selects pool before env.** In
   `build_native_compression_discovery` the openrouter and per-profile branches
   resolve `pool_runtime(...).api_key()` first and only `.or_else(secret(...))`
   to the env var (`main.rs:577-640` region), and the wiring goes through
   `CompressionDiscovery::new_with_pool_credentials` on the real
   `build_agent_client_for_home_with_discovery` path (`main.rs:1223` region), not
   a test-only path. `compression_routes_preserve_explicit_and_auto_fallback_order`
   (`main.rs:2169`) writes a `gmi` pool key and asserts the discovery request
   carries `Bearer gmi-pool-key`, not the env `gmi-key`.

8. **Frozen discovery ordering and two-candidate budget preserved.** Entry order
   (openrouter → requested-main → `ordered_native_profiles`) is unchanged; the
   `unzip` in `new_with_pool_credentials` (`native_agent.rs:549`) keeps client
   and binding index-aligned. The `discovery_attempts >= 2` gate
   (`native_agent.rs:1696`) still bounds distinct providers to two, and recovery
   retries happen inside a single candidate's failure handling without
   incrementing `discovery_attempts`, so the budget is intact. A pool-drained
   candidate is skipped via `request_client` returning `Ok(None)`
   (`native_agent.rs:1677`) before the budget is spent, which is the correct
   no-request-made semantics.

---

## Tests inspected

- `credential_pool.rs`: `request_failure_rotation_matches_compression_oracle`,
  `failure_status_and_cooldown_are_recorded_before_rotation`.
- `auth_store.rs`: `pool_writes_merge_concurrent_rows_and_newer_cooldowns`,
  `pool_write_is_private_and_preserves_auth_symlink`.
- `native_agent.rs`: `compression_pool_persists_failure_before_fresh_client_retry`,
  `exhausted_compression_pool_drops_the_frozen_failed_client`, and
  `compression_pool_429_retries_failed_key_but_402_rotates_immediately`.
- `main.rs`: `compression_routes_preserve_explicit_and_auto_fallback_order`
  (startup, gmi-pool-key assertion).
- Golden corpus: `rust/tools/compression-credential-recovery-goldens.json`,
  `recovery_401_refresh_and_rotation` cases.
