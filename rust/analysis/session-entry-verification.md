# Session entry codec checkpoint, 2026-09-06

`session_entry.rs` ports the persisted routing entry record from
`gateway/session.py`. Required IDs receive the existing traversal checks.
Created/updated timestamps retain naive versus offset-aware form, microseconds,
and fractional-second offsets. Invalid optional resume timestamps become null;
an absent or invalid active-turn token also clears its timestamp. Legacy
`memory_flushed` supplies `expiry_finalized` only when the new field is absent.
Counters and flags preserve explicit JSON values instead of silently coercing
historical records. Model overrides are sanitized on read and again on write.

`gen_session_entry_goldens.py` executes the actual Python dataclasses, guards,
sanitizer and built-in platform enum through AST extraction. Plugin discovery is
disabled in this fixture environment. The 124 cases cover required fields,
timestamp formats and errors, path guards, built-in platform normalization,
metadata dictionary construction, reset/resume values, origin routing and model
overrides. A separate inline test mutates the override after loading and verifies
that serialization still strips credentials. The origin fixture confirms that
the internal relay trust flag is not restored or written.

The comparisons found a pre-existing shared sanitizer mismatch: JSON `false`
was serialized as the string `false`, while Python produces `False`. Non-string
values now use the existing Python representation helper. The timestamp parser
was split into a component parser and its original epoch-conversion wrapper;
its existing tests still pass.

Validation: 1,294 workspace tests passed, two ignored. Clippy with warnings
denied, formatting, fixture regeneration for all four resumed-work generators,
and whitespace checks passed. Logs: `/tmp/hermes-entry-tests.log`,
`/tmp/hermes-entry-workspace.log`, `/tmp/hermes-entry-clippy.log`.

This is a codec checkpoint, not a working SessionStore. No routing-index file
is read or written yet. Plugin platform registration remains unported. IDs are
required to be strings in Rust, whereas Python can retain some malformed scalar
IDs. Origin fields reuse the existing typed SessionSource codec, so arbitrary
non-string origin metadata is not proven equivalent. Metadata with non-string
dictionary keys also retains Python-versus-JSON key identity limitations.
Next: implement routing-index loading/persistence and entry lifecycle, then
connect reset decisions, live process probes and authorization to inbound
session selection and Slack wake handling.

## Routing database operations

SessionDb now implements the four routing methods from hermes_state.py:
load, single-entry upsert, whole-scope replacement and selected-key deletion.
The table matches the current Python composite primary key `(scope, session_key)`.
Entries remain raw strings at this layer, matching Python; validation belongs
to the routing store. Empty keys/values are skipped on writes. Replacing with
an empty map clears only the selected scope.

Replacement deletes and inserts within one SQLite transaction. An actual
SQLite trigger rejects the second insertion in a test; reopening the database
shows that the original rows survive and the first insertion was rolled back.
Another test verifies upserts and scope isolation across reopen. Ten transitions
execute Python's actual routing methods against its extracted schema and compare
all scopes after each Rust operation. Tests remain inline in session_db.rs.

Validation: 1,297 workspace tests passed, two ignored; Clippy with warnings denied,
formatting, entry/routing fixture regeneration and whitespace checks passed.
Logs: `/tmp/hermes-routing-db-workspace.log`, `/tmp/hermes-routing-db-clippy.log`.
No durability setting was weakened. Crash or power-loss testing was not run.
The store's generation ordering, fallback recovery, legacy mirror and old-schema
healing are still pending. Database atomicity alone does not prevent a caller
from publishing an obsolete snapshot; the source store's revision guard must
be ported before integrating concurrent writers.

## Index loading and recovered database reconciliation

RoutingIndex owns entry loading and the fallback baseline. SQLite wins over
legacy JSON for valid matching keys; malformed entries are skipped individually.
Legacy metadata sentinels are ignored. An unsuccessful database load retains a
canonical baseline and later loads retry database reconciliation. Unchanged
fallback entries yield to durable rows; local edits, deletions and newly created
keys survive. Database-only keys are restored. Successful reconciliation clears
the baseline and runs only once.

Four inline tests include 25 comparisons with Python's actual recovery method,
real SQLite and legacy-file precedence, outage edits/deletions/creations, and a
dropped routing table repaired after two failed loads. The repaired database is
read through the original connection. Changes made to the legacy file after
initial loading are not re-imported. Scope paths are canonicalized after directory
creation. This layer requires serialized access by its owning session store.

Validation: 1,301 workspace tests passed, two ignored; Clippy with warnings denied,
formatting, entry/routing/recovery fixture regeneration and whitespace checks
passed. Logs: `/tmp/hermes-routing-recovery-workspace.log` and
`/tmp/hermes-routing-recovery-clippy.log`.

Pending: revision-ordered persistence, stale-route pruning, reset lifecycle and
inbound integration. Malformed scalar/type parity has the entry-codec limits
already documented above. The initial JSON-equality limitation is corrected
in the recovery comparison follow-up below.

## Revision-ordered persistence

RoutingIndex allocates one monotonically increasing revision sequence for full
snapshots and metadata updates. Immutable snapshots can be persisted after
releasing the owning store lock. RoutingWriter serializes durable writes with a
separate mutex. Full snapshots below the persisted generation are skipped;
newer fast records are folded into delayed snapshots. Entry writes below a full
or same-key fast revision are skipped. Successful full writes retire older fast
records. Failed primary and fallback writes leave the persisted revision intact.

Full saves write the legacy mirror if configured or when the primary fails.
Successful primary writes remain successful if only the mirror fails. The mirror
uses the existing fsync-and-rename helper and the source README sentinel. Fast
upserts leave the legacy mirror alone. Failed upserts request a full-save
fallback; candidate metadata remains the candidate in that fallback, allowing
the owner to persist it before changing the live entry.

Four inline tests cover delayed full/fast interleavings, structural deletion,
candidate fallback, filesystem failure and temporary-file cleanup. Another 64
cases execute Python's actual full writer, varying generations, fast records,
database failures, mirror failures and mirror enablement. Rust checks the same
outcomes against real SQLite rejection triggers and filesystem rename failures.
These are controlled interleavings, not a multi-process stress test. The writer
mutex and revision sequence are shared per RoutingIndex, as in the source store.

Validation: 1,305 workspace tests passed, two ignored; Clippy with warnings denied,
formatting, fixture regeneration and whitespace checks passed. Logs:
`/tmp/hermes-routing-writer-workspace.log`, `/tmp/hermes-routing-writer-clippy.log`.
Pending lifecycle work remains unchanged: stale-route pruning, reset transitions,
legacy schema healing and inbound session selection. No live gateway routing has
been switched to this store yet.

## Legacy routing table healing

SessionDb::open now reconciles the routing table before exposing the connection.
An immediate SQLite transaction covers schema inspection, an absent scope column,
table rename/rebuild, data copy and removal of the old table. Existing composite
primary keys are left intact. Rebuilt rows use the source COALESCE(scope, '') and
ascending timestamp copy, so the newest duplicate mapping survives. This is the
routing-table part of schema reconciliation, not the complete state migration.

Three inline tests cover missing or nullable scope, reopening, duplicate rows,
cross-profile keys and preservation of updated_at. A failed NOT NULL copy verifies
that the old table, its rows and its original columns all survive rollback. Three
fixtures also run the actual Python _heal_gateway_routing_pk method and compare
the resulting SQL rows. Tests use temporary databases; no user database was
opened or modified during validation.

Validation: 1,308 workspace tests passed, two ignored; Clippy with warnings denied,
formatting, fixture regeneration and whitespace checks passed. Logs:
`/tmp/hermes-routing-migration-workspace.log` and
`/tmp/hermes-routing-migration-clippy.log`.

Stale-route pruning requires additional per-profile session recovery queries and
ended-session fields. Those paths, full state schema reconciliation and inbound
lifecycle integration remain pending.

## Recovery comparison follow-up

Twelve additional source cases reproduced an equality mismatch in outage
reconciliation: Python considers bool/int/float equivalents unchanged, including
inside nested metadata containers. Rust JSON equality incorrectly kept those
fallback entries instead of accepting the durable route. The shared python_equal
helper now compares numeric types by value and containers recursively. It keeps
integer operands exact, so numbers above 2^53 and at the u64 boundary do not
collapse through an f64 conversion and erase actual edits.

All 37 Python recovery comparisons pass, including the previously failing
case. The full workspace still passes 1,308 tests with two ignored. Clippy with
warnings denied, formatting, fixture regeneration and whitespace checks pass.
Logs: `/tmp/hermes-routing-equality-workspace.log` and
`/tmp/hermes-routing-equality-clippy.log`.

## Gemini audit follow-up

Gemini completed `session-routing-review.md`. Two findings reproduced on real
SQLite: a cold candidate save returned without loading its route, and an upsert
failure after fallback startup caused full replacement to delete database-only
routes that had not been reconciled. The latter regression uses a BEFORE UPDATE
trigger that rejects the upsert while permitting the fallback DELETE+INSERT.
Both tests failed before the fix. save_entry now enforces ensure_loaded before
entry capture, including recovery reconciliation, matching the loading boundary
provided by Python's metadata callers.

The audit's third finding, a second generation allocation on ordinary upsert
fallback, is not a defect: Python _save_entry calls _save_entries in that branch,
which captures a new snapshot and revision. That behavior remains unchanged.
Missing-scope migration already has a real SQLite regression even though its
standalone PK-healing oracle begins after Python's column-reconciliation step.
Additional field permutations are coverage suggestions, not verified bugs.

Validation: 1,310 workspace tests passed, two ignored; Clippy with warnings denied,
formatting, fixtures and whitespace checks pass. Logs:
`/tmp/hermes-routing-audit-workspace.log`, `/tmp/hermes-routing-audit-clippy.log`.
Gemini's next task maps the queries, schema and profile/workspace fences required
for integrating stale-route recovery. No full gateway parity is claimed.

## Recovery isolation guards

Profile recovery accepts exact-key and unnamespaced legacy rows. Otherwise it
compares the recovered namespace with the active profile in single-profile mode,
or the requested namespace in multiplexed mode. Empty/main namespaces map to
default. Scoped Slack non-DM recovery requires a parseable origin dictionary
whose scope matches the source. Missing scope falls back to guild_id; explicit
null scope does not. Unscoped, DM and non-Slack sources bypass this workspace gate.

128 profile and 198 workspace cases execute the actual Python guards. Tests
remain inline in session_routing.rs. The guards are ready for the recovery query
path but are not yet used by inbound session selection. Gemini's completed
session-recovery-map.md identifies remaining SQL/schema work; its display-name
note needs care because the source entry constructor uses source.chat_name.
Pruning must also resolve the database per routing key, rather than using one
ambient database for every profile.

Validation: 1,311 workspace tests passed, two ignored. Clippy with warnings denied,
formatting, fixture regeneration and whitespace checks pass. Logs:
`/tmp/hermes-recovery-isolation-workspace.log`,
`/tmp/hermes-recovery-isolation-clippy.log`.

## Lifecycle schema and durable row reads

SessionDb reconciles missing user/profile/origin/display/parent identifiers,
system prompt fields, end timestamps/reasons and expiry_finalized in one immediate
transaction. It creates the source-compatible system_prompts table and preserves
existing columns. This is the recovery subset, not complete state schema parity.

get_session uses Python's left join and COALESCE projection, removing the
internal resolved-prompt alias from its result. Missing hashes use the inline
prompt; a present empty deduplicated prompt remains empty. Existing Rust token
writes are synchronous, so there is no token-delta queue to flush here. The row
reader preserves JSON-compatible SQLite scalar values and wider table columns;
malformed blob/nonfinite values return errors instead of lossy conversion.

Two inline tests use real database reopen to verify old session data, extra
columns and message history survive migration, and check lifecycle metadata,
missing rows and all three prompt-resolution cases. Validation: 1,313 workspace
tests passed, two ignored; Clippy with warnings denied, formatting and whitespace
checks pass. Logs: `/tmp/hermes-recovery-schema-workspace.log` and
`/tmp/hermes-recovery-schema-clippy.log`.

Gemini's peer-query oracle is still running. Candidate selection, reset boundaries,
profile-owned database resolution and lifecycle writes remain next.

## Durable peer selection

SessionDb::find_latest_gateway_session_for_peer now executes the source's exact
key and peer fallback queries. Both reject candidates behind newer intentional
reset boundaries. Exact lookup ranks message-bearing sessions first, then durable
activity, accepting an empty keyed row when no better row exists. Fallback needs
chat ID/type, requires messages, matches the whole peer tuple and fences by the
database owner's profile. Exact lookup deliberately precedes that fallback fence;
the session-store profile/workspace guards still apply before adoption.

Ownership comes from the canonical database path within the configured root,
including named profiles, rather than active runtime profile state. Stores
outside that tree retain the source's unfenced legacy behavior. Reads return
resolved system prompts through the same row projection as get_session.

Gemini generated gen_session_peer_goldens.py and 112 fixtures using the actual
Python method, source reason constants and SQLite schema. Codex reviewed the
harness, removed its machine-specific root path and added an inline Rust consumer
that seeds real databases under temporary profile directories. All selected IDs
match. Two additional regressions cover strict reset timestamp equality and
exact-versus-fallback scope behavior. Fixture prompt scenarios assert selection;
the prior lifecycle-read test separately verifies prompt projection itself.

Validation: 1,316 workspace tests passed, two ignored; Clippy with warnings denied,
formatting, peer fixture regeneration and whitespace checks pass. Logs:
`/tmp/hermes-peer-workspace.log`, `/tmp/hermes-peer-clippy.log`.
No incoming gateway traffic uses recovery yet. Remaining work includes lifecycle
writes and their conversation-generation invariant, recovered-entry construction,
per-key database resolution, pruning and runtime session selection.

## Lifecycle writes and conversation generations

SessionDb now provides first-end-wins closure, accidental-closure reset promotion
and reopen. Reset boundaries increment the conversation_generations table only
when the session update changed a row, in the same transaction. Generation rows
are not cascaded with history. Source/key values use Python whitespace trimming;
blank peers and non-reset reasons do not advance the counter. Promotion retains
Python's false-on-write-failure result.

Reopen stamps markerless same-key children with _reset_from before clearing the
parent's reset reason. Both updates share one transaction, so malformed child
JSON cannot leave a reopened parent with lost lineage. The schema adds
model_config and the source-compatible generation table. Closure reads its clock
inside the write critical section, matching source callback timing; promotion
captures time before writing, as Python does.

Two real SQLite regressions cover first closure, accidental promotion, preserved
explicit boundaries, child versus branch lineage, repeated generation increments,
history deletion and rollback on generation-trigger/JSON errors. Validation:
1,318 workspace tests passed, two ignored; Clippy with warnings denied, formatting
and whitespace checks pass. Logs: `/tmp/hermes-lifecycle-workspace.log` and
`/tmp/hermes-lifecycle-clippy.log`.

Gemini's lifecycle fixture task is still running. Broad source comparisons,
recovered-entry construction and per-key stale-route integration remain pending.

## Recovered-entry construction

SessionEntry::from_recovered_row reconstructs routing state from durable start
and last-activity timestamps, preserving naive local time and microseconds.
Invalid start values become epoch-local timestamps; invalid or absent activity
falls back to creation time. Explicit _has_messages overrides inferred activity;
otherwise message count or a non-null activity value establishes prior activity.
Origin/display/platform/chat type come from the supplied source. No current-time
argument can accidentally refresh an old recovered conversation.

216 cases execute Python's actual constructor in controlled UTC and UTC+8 local
zones, including negative/fractional epochs, numeric strings, invalid values and
activity flags. Rust uses fixed offsets in tests and the actual local zone in
production. The constructor retains existing strict ID validation and JSON value
limits. Extreme platform time_t overflow behavior and DST transitions are not
proven by these cases; broader persisted-entry codec limits still apply.

Validation: 1,319 workspace tests passed, two ignored; Clippy with warnings denied,
formatting, entry fixture regeneration and whitespace checks pass. Logs:
`/tmp/hermes-recovered-entry-workspace.log`, `/tmp/hermes-recovered-entry-clippy.log`.
Gemini also completed 70 lifecycle fixtures; their --check regeneration passes.
Rust lifecycle comparisons and per-key stale-route orchestration remain pending.

## Lifecycle oracle consumption

Reviewed Gemini's generator: it extracts actual source lifecycle methods, reset
reason constants and schema, then executes writes inside SQLite transactions at
a fixed clock. The Rust inline consumer now reproduces the same seeds and checks
operation results/errors plus all selected session fields and generation rows
after each operation. Valid model_config JSON is compared structurally, preserving
malformed strings for rollback cases. All 70 cases and 90 operations match.

Validation: 1,320 workspace tests passed, two ignored; Clippy with warnings denied,
formatting, lifecycle fixture regeneration and whitespace checks pass. Logs:
`/tmp/hermes-lifecycle-oracle-workspace.log`,
`/tmp/hermes-lifecycle-oracle-clippy.log`.

Next prerequisite: record_gateway_session_peer must preserve ordinary metadata,
self-heal missing rows with profile ownership, and optionally update compression
ancestors without crossing branch/delegate/tool boundaries. Gemini is generating
that source oracle while the Rust implementation proceeds next. Stale-route and
incoming gateway integration remain incomplete.

## Peer recording and missing-row repair

SessionDb::record_gateway_session_peer now records full gateway identity in one
transaction. Missing ordinary targets are inserted with their database owner's
profile and current start time. Existing targets preserve display_name/origin_json
when those arguments are absent, but accept explicit empty strings. Identity
fields follow the source's replacement semantics. Optional ancestor updates use
the actual recursive compression-lineage SQL, including branch/delegate/tool
boundaries and UNION cycle deduplication. Ancestor mode does not insert a missing
target. set_expiry_finalized persists the durable finalization flag.

Two real SQLite tests cover missing-row repair, profile stamps, metadata updates,
reopen persistence, compression branch boundaries, missing ancestor targets and
malformed-JSON rollback. Validation: 1,322 workspace tests passed, two ignored;
Clippy with warnings denied, formatting, peer-selection fixture regeneration and
whitespace checks pass. Logs: `/tmp/hermes-peer-record-workspace.log` and
`/tmp/hermes-peer-record-clippy.log`.

Gemini is still generating the broader recorder oracle, so delegate/tool/cycle
comparisons remain unverified by those fixtures. Recovery orchestration and
incoming gateway integration are the next integration work, not completed here.


## Recovery orchestration and peer-recorder oracle

The 65 Gemini-generated recorder cases regenerate from the actual Python methods
and schema. Rust now compares each operation's return/error and selected durable
session fields, including valid JSON normalization. Nullable no-op IDs map to the
Rust empty ID. The synthetic owner resolver is injected identically on both sides;
real path ownership remains covered separately. All cases pass, including
compression cycles, branch/delegate/tool boundaries and malformed-JSON rollback.

RoutingIndex::recover_from_db follows the synchronous Python recovery path:
resolve each queried key's database, reserve scoped Slack legacy keys before
fallback, reject mismatched scopes/profiles, reconstruct timestamps, check policy,
promote overdue sessions or reopen valid history, then persist migrated identity.
Lookup errors propagate to allow pruning to retain uncertain routes. Reopen and
peer-record failures remain best-effort, as in Python. This does not implement the
separate query-only path used outside the owning store lock or inbound wiring.

Two real SQLite tests verify migration, once-only legacy claims, persisted reopen,
active-process protection, idle promotion with generation increment, and lookup
errors versus an absent database. Workspace: 1,325 passed, two ignored. Clippy
all-targets with warnings denied passes. Logs are
`/tmp/hermes-recovery-orchestration-workspace.log` and
`/tmp/hermes-recovery-orchestration-clippy.log`.

Gemini's read-only orchestration review is running through agy.sh. Next consume
that report and implement per-key stale-route pruning and query-only recovery.


## Stale-route scan

RoutingIndex::prune_stale now implements the Python scan decisions with an owning
store callback for policy-aware recovery. Per-key DB resolution avoids consulting
the root store about a profile-owned route. Missing/live rows remain unchanged;
recovery errors retain the route; same-ID recovery preserves the complete original
entry; new-ID recovery repoints it. Deletions are applied only after a complete
scan. A get_session failure leaves earlier repoints in memory, abandons queued
deletions and returns false, matching Python's no-save early return.

Two real SQLite tests verify state preservation, child repointing, profile
isolation, recovery failures, missing rows, and a schema failure after a prior
repoint. The callback is not yet wired to startup or the inbound session store.
Validation: 1,327 workspace tests passed, two ignored. Logs:
`/tmp/hermes-stale-prune-workspace.log` and `/tmp/hermes-stale-prune-clippy.log`.

Gemini's review completed. The report maps the separate query-only live recovery
path and single-flight lock phases. Its suggestion to directly reuse that query
from synchronous recovery needs adjustment: legacy peer-record side effects occur
at different points relative to reset evaluation. Follow-up notes preserve this
constraint in the report. Query-only recovery and the owning store remain next.


## Query-only recovery

Candidate selection is shared between recover_from_db and query_recoverable.
Synchronous recovery still propagates lookup errors, evaluates reset policy and
only records migration after reopening. Query-only recovery suppresses each DB
lookup error separately, returns expired predecessors without lifecycle writes,
and records a migrated peer before returning. It does not publish to the index.

Two real SQLite tests verify the different expired-predecessor outcomes and
persisted migration/reset ordering, plus a broken primary database with a working
legacy database. A strict recovery error does not consume the legacy claim; the
following query-only call successfully recovers it. Best-effort migration failure
on the broken primary store does not discard the predecessor.

Workspace: 1,329 passed, two ignored. Logs:
`/tmp/hermes-query-recovery-workspace.log` and
`/tmp/hermes-query-recovery-clippy.log`. Live I/O lock discipline is still pending:
legacy claims currently require mutable index access and must become independently
shared by the owning session coordinator. Gemini is mapping existing per-key
profile resolution helpers before that integration.


## Profile database resolution

SessionDatabases in session_db_recovery.rs separates routing_home from per-key
session ownership. Default/legacy keys use an explicit caller-provided ambient
home; named namespaces resolve under the root profiles directory when multiplexing
is enabled. Successful home lookups are cached using the original namespace,
while misses and tombstones remain uncached. Profile normalization uses the
existing helper, matching profile_exists/get_profile_dir rather than introducing
new validation at this internal resolution layer.

The existing RecoverableHandleCache handles SQLite opening and retry. Two real
SQLite tests verify fixed routing ownership, distinct profile data, normalized
aliases, multiplex-off behavior, late enrollment, tombstones, an actual failed
SQLite open and recovery after advancing the injected retry clock.
Workspace: 1,331 passed, two ignored. Logs:
`/tmp/hermes-profile-databases-workspace.log` and
`/tmp/hermes-profile-databases-clippy.log`.

The resolver cache is per instance. Python additionally uses a process-wide shared
SessionDB registry, which still needs implementing and adoption by callers. Live
startup/inbound wiring, pinned test overrides and independently shared recovery
claims remain pending. Gemini's read-only profile-resolution mapping is running.


## Shared database generations

SessionDb::open_shared uses a process-wide map of per-path opening slots. Schema
work holds only the path's mutex. Weak registry references plus caller Arc owners
model release without allowing one caller to close another's handle. Unix
metadata identity changes retire the weak generation before opening a replacement;
a failed open leaves no borrowable old handle. Existing holders remain alive.
Path resolution follows existing symlink prefixes even before final creation.
Gateway startup and SessionDatabases use this API; explicit one-shot/test opens
remain independent.

Two real SQLite tests verify eight concurrent acquisitions, lexical and symlink
aliases, final-owner drop, malformed replacement retries and retirement of the old
generation. Replacement tests use DELETE journal mode to isolate registry behavior;
they do not prove WAL restore safety or write-time file guards. Full Python
write-time replacement guards, explicit shutdown close_all and non-Unix identity
checks are still pending. Registry path slots currently remain after owners drop.

Workspace: 1,333 passed, two ignored. Logs: `/tmp/hermes-shared-db-workspace.log`
and `/tmp/hermes-shared-db-clippy.log`. Gemini's profile DB map completed; it
confirms the remaining live AppState/SessionDatabases integration and policy scope
propagation work. The source tree changed during its inspection, so recheck line
references before using its proposed implementation outline.


## Recovery lock separation

Moved candidate lookup, query-only recovery, synchronous recovery and peer
migration into SessionRecovery. RoutingIndex holds its Arc and keeps delegating
entry points for startup callers. Live callers can clone the recovery Arc and
release the map lock before I/O. Legacy claims use a separate mutex; reservation
is assigned to a boolean before entering the DB resolver, so the guard cannot
live across that call.

A channel-coordinated regression blocks the first workspace's legacy lookup,
observes both locks available, and proves a second workspace skips the reserved
legacy key. No sleeps or network calls are used. Workspace: 1,334 passed, two
ignored. Logs: `/tmp/hermes-recovery-lock-workspace.log` and
`/tmp/hermes-recovery-lock-clippy.log`.

Live dispatcher/history IDs and owning store initialization remain unchanged.
Gemini is mapping the exact integration call sites before replacing those IDs.


## Durable identity propagation

Added serde-skipped Message.resolved_session_id for assignment by the owning
session coordinator. The existing shared message_session_id helper prefers a
nonempty resolved ID; temporary platform-derived IDs remain fallback. This carries
the same ID through dispatcher leases, history begin/end, native prompt-cache
scope and delivery ledger. Reply messages preserve the internal ID. Adapters and
external JSON cannot choose it.

A real SQLite test verifies one transcript across changes to channel/workspace/
thread metadata, ignored untrusted ID input, omitted output serialization and
bridge-managed-history bypass. Workspace: 1,335 passed, two ignored. Logs:
`/tmp/hermes-resolved-id-workspace.log` and `/tmp/hermes-resolved-id-clippy.log`.
No live coordinator assigns the field yet. Creation/resume must be wired before
the dispatcher acquires its lease or loads history; per-profile DB selection must
follow that same resolved session. Gemini's integration mapping is still running.


## Full session creation metadata

SessionCreate supplies gateway and agent creation metadata. create_session uses
Python _insert_session_row's INSERT/enrichment, parent context and compression
inheritance SQL. System prompts use source-compatible SHA-256 and are inserted,
referenced and garbage-collected in the same transaction. Missing profile stamps
use the existing database owner resolver. Metadata migration now adds model, cwd,
git_repo_root and git_branch. Python's longer transcript-write retry patience is
still unported; this method currently propagates rusqlite failures.

Two real SQLite tests verify reset-stub enrichment, retained source/key values,
resolved prompts, rollback of prompt insertion on malformed existing model JSON,
compression-only routing inheritance and profile namespace boundaries. Workspace:
1,337 passed, two ignored. Logs: `/tmp/hermes-session-create-workspace.log` and
`/tmp/hermes-session-create-clippy.log`. Broader Python creation oracles remain
needed. Coordinator publication and live dispatch are next.


## Candidate construction and ordinary publication

SessionEntry::new_candidate matches the gateway timestamp plus eight random hex
characters, reusing the existing kernel-random identity helper. Created/updated
timestamps, origin, display/chat fields and predecessor/reset context are supplied
before publication. Ordinary publication uses the map entry API while the caller
holds its lock, returns the published entry and identifies whether this candidate
won. Occupied slots retain the previously published entry without overwriting it.

An eight-thread barrier test constructs candidates outside the map lock and proves
one publication winner, a single returned ID and preserved lineage/default fields.
Workspace: 1,338 passed, two ignored. Logs:
`/tmp/hermes-session-candidate-workspace.log` and
`/tmp/hermes-session-candidate-clippy.log`. Force-new observed-entry replacement,
coordinator DB creation/persistence and live assignment remain pending. Gemini is
generating broader actual Python creation fixtures in the background.


## Force-new observed-entry publication

SessionEntry has a private Arc identity token retained by snapshots and excluded
from its explicit serialization. Deserialization and new construction allocate a
new token. publish_forced_candidate therefore matches Python's `current is
force_new_observed_entry` even when another worker restores identical row data.
Metadata changes retain identity. Ordinary publication delegates with no observed
entry and still cannot overwrite an occupied slot.

The regression covers winning replacement, a second stale candidate, equal-data
reload rejection, metadata mutation and publication after the slot becomes empty.
Workspace: 1,339 passed, two ignored. Logs:
`/tmp/hermes-force-publication-workspace.log` and
`/tmp/hermes-force-publication-clippy.log`. Existing-route checks and coordinator
wiring remain pending. Gemini's creation fixture generation remains running.


## Compression continuation and guarded healing

SessionDb::get_compression_chain follows Python's get_compression_chain SQL:
compression-ended parents only; branch/delegate markers and tool children excluded;
compression continuations rank before live then closed siblings. Recency uses the
maximum of heartbeat and message timestamps, falling back to start time. Start
and ID break ties. Each hop has its own read scope; cycles and empty children stop
the walk, and 100 hops bound it. get_compression_tip delegates to the chain.

SessionEntry::heal_compression_tip applies the result only while the entry still
points to the queried original ID. It preserves counters, metadata and the private
observation token. Two real SQLite tests cover ranking, invalid JSON, missing/empty
roots, long chains and cycles; an entry test verifies stale-result rejection and
state preservation. Workspace: 1,342 passed, two ignored. Logs:
`/tmp/hermes-compression-walk-workspace.log` and
`/tmp/hermes-compression-walk-clippy.log`. Coordinator invocation remains pending.


## Existing-route suspension and freshness checks

existing_reset_reason follows get_or_create_session Phase 1b: suspension wins,
then ordinary reset policy, then resume-pending freshness if policy did not reset
and mode is not none. The timestamp is last_resume_marked_at when present,
otherwise updated_at. Expiry is strictly greater than the positive configured
window. Active-process protection belongs to ordinary policy only; the independent
freshness gate still applies, exactly as Python does. Aware/naive timestamp
comparisons fail rather than silently dropping offsets.

The regression exercises mode-none preservation, exact expiry, reference timestamp
precedence, absent resume timestamps, ordinary-policy precedence, suspension,
active-process behavior, nonpositive/NaN/infinite windows and mixed timestamp
errors. Workspace: 1,343 passed, two ignored. Logs:
`/tmp/hermes-existing-reset-workspace.log` and
`/tmp/hermes-existing-reset-clippy.log`.

Gemini completed 52 creation fixtures; regeneration passes under Python 3.12.13.
They have not yet been compared against Rust. Review/consume them next, then
continue coordinator integration. Live dispatch still does not call this helper.


## Creation oracle comparison

All 52 actual Python creation cases now execute against Rust SQLite. The test
seeds initial sessions and prompts, uses the same injected owner and clock100,
and compares success/error, selected session rows (normalizing model_config JSON)
and the full prompt table after every operation. Rust's unit write return is
adapted to Python's public ID/internal-None return convention in the harness.
Ownership derivation still has separate real-path tests. Creation code now accepts
lazy owner and clock dependencies internally; the public method keeps production
owner resolution and current time.

Regeneration with Python 3.12.13 passes. Workspace: 1,344 passed, two ignored.
Logs: `/tmp/hermes-create-oracle-workspace.log` and
`/tmp/hermes-create-oracle-clippy.log`. Next implement per-key shared coordinator
flights: overlapping force-new calls share one transition result, rather than
merely executing several transitions serially. Live dispatch remains unwired.


## Shared session transition flights

SessionFlights provides per-key owner election with a short registry lock.
Owners perform work outside the registry; waiters use a per-flight condition
variable and receive cloned entry snapshots retaining identity, or the same Arc
error. Completion signals waiters and removes the registry entry. An unfinished
owner's Drop publishes an abandonment error, preventing stranded waiters after
unwinding/cancellation. Different keys proceed independently.

Two concurrency tests verify result identity, error sharing, independent-key
progress, cleanup, retry and abandoned-owner wakeup. Workspace: 1,346 passed,
two ignored. Logs: `/tmp/hermes-session-flights-workspace.log` and
`/tmp/hermes-session-flights-clippy.log`.

The owning coordinator is still pending. It must perform waiter activity touching,
resolve per-key Arc DB handles, and run blocking waits/I/O away from async executor
threads. Entry snapshots share identity but not mutable fields; the coordinator
must explicitly synchronize updates through the index. Gemini is reviewing these
integration constraints against the Python source.


## Shared profile handles through recovery and pruning

Recovery and pruning resolver callbacks now accept any Deref<Target=SessionDb>
handle, including borrowed test handles and production Arc handles. Each lookup
can call SessionDatabases::for_key directly instead of maintaining an externally
borrowed handle map. Recovery side effects use the same per-key resolver.

The combined real SQLite regression creates a compression lineage under a named
profile and a conflicting reset-ended parent in root. Pruning with the real handle
resolver recovers the child, then saves the route only in the fixed root routing
store. The profile receives no routing-index rows; root receives no child session.
Workspace: 1,347 passed, two ignored. Logs:
`/tmp/hermes-profile-recovery-workspace.log` and
`/tmp/hermes-profile-recovery-clippy.log`. Owning coordinator startup and live
dispatch remain pending; Gemini's coordination review is running.


## Owning store startup

Added session_store.rs with inline tests. SessionStore owns configuration, the
routing map mutex, shared recovery and per-key flights, and SessionDatabases.
Construction loads routing rows and prunes against each key's owning DB before
publishing the store. Recovery selects the platform/type reset policy and a safe
process-probe result; structural pruning changes are persisted to the fixed store.

The real SQLite restart test reopens an accidental closure without losing queued
metadata and keeps an unprovisioned profile route without creating its directory
or consulting ambient history. Workspace: 1,348 passed, two ignored. Logs:
`/tmp/hermes-store-startup-workspace.log` and
`/tmp/hermes-store-startup-clippy.log`. Main does not instantiate this store yet;
live transitions and waiter activity updates are next.

Gemini's coordination review completed. Its suggestions to remove flights before
signaling and split force_new flights conflict with the Python reference, so they
were rejected. The live I/O lock and detached-snapshot concerns remain integration
constraints, recorded at the top of the review.


## Unlocked coordinator metadata persistence

SessionStore::update_session touches the published entry only when requested,
applies non-null token counts, and captures a coherent peer snapshot plus routing
revision under its index lock. Fast writes and peer refresh then run unlocked;
full-save fallback also captures before writing. RoutingIndex exposes initialized
snapshot/entry capture helpers with no I/O. Coordinator reconciliation reads rows
outside the lock and merges against current state, preserving outage edits.

The real SQLite test verifies a preserved internal clock, advanced activity clock,
null token no-op, exact durable routing data, identity repair for a missing session
row and missing routing-key no-op. Workspace: 1,349 passed, two ignored. Logs:
`/tmp/hermes-store-update-workspace.log` and `/tmp/hermes-store-update-clippy.log`.
Live get-or-create and waiter invocation remain pending; this establishes their
metadata update path without holding the index mutex across persistence.


## Composed session transitions

SessionStore::get_or_create_session elects an owner or joins an existing flight.
Waiters honor touch_activity and retrieve current fields only for the same entry
identity. Owners migrate eligible legacy Slack memory routes, inspect compression
and durable staleness outside the index lock, apply guarded healing/reset decisions,
recover predecessors or publish candidates, then persist routing and perform
predecessor promotion/creation. Newly created rows and migrated peers are refreshed
using captured identity. Failed creation remains logged and repairable as Python
specifies. Reopen/final writes reacquire the per-key handle rather than reusing an
earlier unavailable lookup.

Three real SQLite tests exercise the public coordinator through creation/reuse,
internal clock preservation, accidental-close resume, compression tip healing,
restart continuity, force-new, idle reset with durable parent/_reset_from linkage,
and one-time in-memory Slack DM migration with metadata retained. Workspace:
1,352 passed, two ignored. Logs: `/tmp/hermes-store-transition-workspace.log` and
`/tmp/hermes-store-transition-clippy.log`.

Gemini is reviewing the composed transition against the source. Live gateway
startup/dispatch wiring, profile config propagation and process/freshness runtime
inputs remain pending. These tests validate the store API, not a platform-to-agent
end-to-end turn.

## Optional dispatcher integration, 2026-09-07

Dispatcher::with_session_store resolves session identity and the owning history
database before acquiring the turn lease. Store operations use spawn_blocking;
turn completion updates activity and peer metadata. Agents that manage their own
history retain their existing path. An inline test runs two turns with a store
restart between them and verifies the same durable ID and four history messages.
Production startup and HTTP ingress remain unwired; migration of older Rust
history IDs and profile/process inputs remain pending.

Workspace validation: 1,353 passed (one core, 1,352 gateway), two ignored. Clippy
passes with warnings denied. Logs: `/tmp/hermes-dispatch-store-workspace.log` and
`/tmp/hermes-dispatch-store-clippy.log`. Gemini's transition review completed;
its proposed findings still need source verification.

## Activity changes during unlocked probes

The transition review exposed a reproducible snapshot parity gap: a process
probe can overlap a completing turn's activity update. Python's `_should_reset`
reads the shared entry after that probe, while Rust previously read its earlier
clone. The new inline test touches activity through the real store API during
the probe and checks that the original session stays open. It failed before the
fix. Rust now refreshes policy fields only when the entry instance still matches
the observed one, retaining the old snapshot if another entry replaced it.

An AST execution of Python's actual `_should_reset` confirmed the same scenario
returns no reset. Workspace tests: 1,354 passed, two ignored. Logs:
`/tmp/hermes-probe-activity-red.log`, `/tmp/hermes-probe-activity-workspace.log`.

## HTTP ingress and shared leases

AppState carries the optional coordinator plus one shared lease registry and
generation counter. Production push dispatchers now receive those handles.
HTTP resolves durable identity in spawn_blocking, acquires its lease before
loading history, awaits the model task before flushing, and updates session
activity before releasing ownership. Without an installed coordinator the
existing history identity remains in use; main still needs store initialization.

Two inline tests cover durable HTTP image/tool-round history after routing
reload and blocking a model call while another ingress holds the same lease.
Workspace: 1,356 passed, two ignored. Logs:
`/tmp/hermes-http-routing-workspace.log`, `/tmp/hermes-http-routing-clippy.log`.

## Startup input resolution

Profile inference now derives identity from the resolved home and root, reusing
the existing non-strict path resolver. Tests cover unprovisioned names, syntax
without provisioning reserved-name restrictions, nested paths, terminal newline
regex behavior, symlink ancestry and loops. Freshness resolution preserves the
startup bridge's config precedence, malformed-value default, and Python numeric
text behavior without modifying process env. Actual Python methods were executed
through AST extraction against the key cases.

Workspace: 1,359 passed, two ignored. Logs:
`/tmp/hermes-startup-inputs-workspace.log`, `/tmp/hermes-startup-inputs-clippy.log`.

## Legacy transcript ownership claim

Added one guarded SQL update for legacy Rust rows lacking routing ownership.
It retains the original ID, messages and timestamps while checking source/chat,
thread/type, closure eligibility and absent destination lineage. It does not
perform network I/O or read-then-write ownership checks. Two inline tests cover
preservation and rejection cases, plus exactly one successful claim across two
independent SQLite connections racing on the same row.

Workspace: 1,361 passed, two ignored. Logs:
`/tmp/hermes-legacy-claim-workspace.log`, `/tmp/hermes-legacy-claim-clippy.log`.
Coordinator invocation and normal policy recovery after the claim remain next.

## Live startup and legacy recovery

The flight owner now consumes an optional ingress-supplied legacy ID/source,
claims only after normal recovery misses, and retries the ordinary finder before
reset/reopen/publication. New inline tests cover preserved history on restart,
idle closure with the old ID as predecessor, force-new bypass, and old HTTP
history reaching the actual native model request. Unsafe existing IDs fail with
an explicit migration error before mutation; they still require ID migration.

Main now initializes the coordinator for gateway-owned history with the full
config loader and profile/freshness resolvers. A compiled-binary smoke test used
a temporary Hermes home and fixture CLI backend: two gateway lifetimes retained
the same fresh-session ID/history, and a legacy row inserted between starts was
adopted and supplied to the backend on an HTTP turn.

Workspace: 1,363 passed, two ignored; Clippy clean. Logs:
`/tmp/hermes-legacy-adoption-workspace.log`,
`/tmp/hermes-legacy-adoption-clippy.log`, `/tmp/hermes-legacy-startup-smoke.log`.

## Ingress cancellation ownership

HTTP and push now spawn a complete admitted turn owner, including the lease,
history read/write, agent completion and delivery. Cancelling the outer waiter
does not release ownership while the agent is still running. The HTTP regression
failed before the change. Both inline regressions hold an agent after its final
stream event, abort the ingress waiter, assert a competing lease times out, then
allow completion and verify history before a successful subsequent acquisition.
The push test additionally verifies delivery. Shutdown drain and explicit agent
interruption are not covered by this contract.

Workspace: 1,365 passed, two ignored. Logs:
`/tmp/hermes-http-cancel-red.log`, `/tmp/hermes-turn-owner-workspace.log`,
`/tmp/hermes-turn-owner-clippy.log`.
