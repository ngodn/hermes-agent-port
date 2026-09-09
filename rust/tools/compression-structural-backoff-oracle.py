#!/usr/bin/env python3
"""Golden oracle for the full-compression structural no-op backoff (#93022).

A full-compression attempt that finds nothing eligible inside the protection
window (too few messages, no compressible window, empty post-handoff residue)
or a fired compaction that returns the transcript unchanged is "nothing to
compress right now", NOT an ineffective attempt. It arms a transient in-memory
backoff instead of striking the permanent anti-thrash breaker, so a short
session can still auto-compact once it grows real compressible material or the
backoff lapses. This oracle drives the *real* current Python implementation so
the Rust port can be reviewed against CPython behaviour rather than a paraphrase.

Nothing here reimplements a backoff decision. Every recorded value is the state
or return of an authoritative local method executed under a monkeypatched
monotonic clock (and, where a branch needs it, patched-out summary I/O). The
one place the caller lives in a multi-thousand-line generator
(``agent/conversation_compression.py``'s no-progress commit path) is covered by
executing the exact recorder call that path makes with its real reason string;
the surrounding commit/rotate plumbing is documented as deferred to Rust's live
integration test.

Authoritative sources executed (current local source):

  * ``ContextCompressor.__init__`` / ``on_session_reset`` /
    ``on_session_end`` / ``bind_session_state``
    (agent/context_compressor.py) - the field lifecycle: init to 0.0 and every
    reset that re-zeroes ``_structural_no_op_backoff_until``.
  * ``ContextCompressor._record_structural_no_op``
    (agent/context_compressor.py) - arming the backoff and its duration
    (``_STRUCTURAL_NO_OP_BACKOFF_SECONDS``), and its non-interaction with the
    ineffective / fallback counters.
  * ``ContextCompressor._compression_block_reason`` /
    ``_automatic_compression_blocked_locally`` / ``_automatic_compression_blocked``
    (agent/context_compressor.py) - the eligibility gate while the backoff is
    active, its transient expiry, guard precedence, and the overflow
    ``ignore_cooldown`` bypass interaction.
  * ``ContextCompressor.should_compress_info`` (agent/context_compressor.py) -
    the caller-facing (should, reason) tuple over threshold.
  * ``ContextCompressor.compress`` (agent/context_compressor.py) - the three
    in-method caller reasons that arm the backoff (insufficient_messages,
    no_compressible_window, empty_post_handoff_window) driven end-to-end, and
    the ``force=True`` override that clears it.
  * ``ContextCompressor.record_completed_compaction``
    (agent/context_compressor.py) - a completed boundary lifting the backoff.
  * the no-progress recorder call in
    ``agent/conversation_compression.py`` (the commit-layer dead-loop breaker,
    around line 4467) - executed as its real recorder invocation with the
    caller's reason string.

Only the monotonic clock is patched for every case (a fixed base so the
recorded ``backoff_until`` and remaining-seconds are stable). ``time.time`` is
left real; no wall-clock value enters a golden. For the ``compress`` caller
cases the model-window probe is pinned and the summary I/O is either
short-circuited before the LLM (the structural branches return early) or, for
the forced-success case, replaced by a scripted ``call_llm`` stand-in.
Persistence is deliberately left unbound: an unbound ``_session_db`` makes the
durable-guard refresh no-op, which is the exact shape the in-memory backoff must
survive. No network, timestamps, randomness, absolute paths, or credentials
enter a golden.

Run from the repository root with the project virtualenv:

    .venv/bin/python rust/tools/compression-structural-backoff-oracle.py

Writes ``rust/tools/compression-structural-backoff-goldens.json`` next to this
script. Pass ``--check`` to regenerate in memory and compare against the
checked-in fixture.
"""
from __future__ import annotations

import json
import logging
import sys
from pathlib import Path
from typing import Any, Dict, List
from unittest.mock import MagicMock, patch

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT = Path(__file__).resolve().parent / "compression-structural-backoff-goldens.json"

sys.path.insert(0, str(REPO_ROOT))

# The compressor logs warnings on every no-op branch; none of them are part of
# a golden and they only clutter generator output.
logging.getLogger("agent.context_compressor").setLevel(logging.CRITICAL)

import agent.context_compressor as cc  # noqa: E402
from agent.context_compressor import ContextCompressor, SUMMARY_PREFIX  # noqa: E402

FIXED_WINDOW = 100_000
# Fixed monotonic base so every armed ``_structural_no_op_backoff_until`` and
# every remaining-seconds read is deterministic. Chosen large enough that
# subtracting the 300s backoff never goes negative unexpectedly.
MONO_BASE = 10_000.0


# ---------------------------------------------------------------------------
# controllable monotonic clock
# ---------------------------------------------------------------------------
class Clock:
    """A settable stand-in for ``time.monotonic``.

    ``context_compressor`` does ``import time`` and calls ``time.monotonic()``,
    so patching ``time.monotonic`` with this callable drives every backoff read
    (arming, remaining seconds, expiry) from one place. ``time.time`` is left
    real because no wall-clock value is recorded.
    """

    def __init__(self, value: float = MONO_BASE) -> None:
        self.value = value

    def __call__(self) -> float:
        return self.value


CLOCK = Clock()


# ---------------------------------------------------------------------------
# compressor construction (no network)
# ---------------------------------------------------------------------------
def make_compressor(
    *,
    protect_first_n: int = 1,
    protect_last_n: int = 1,
    threshold_percent: float = 0.85,
    model: str = "test/model",
) -> ContextCompressor:
    """Build a compressor with the window probe pinned so no /models call fires.

    Mirrors tests/agent/test_context_compressor_structural_backoff.py so the
    oracle exercises exactly the construction the regression suite pins.
    """
    with patch.object(cc, "get_model_context_length", return_value=FIXED_WINDOW):
        c = ContextCompressor(
            model=model,
            threshold_percent=threshold_percent,
            protect_first_n=protect_first_n,
            protect_last_n=protect_last_n,
            quiet_mode=True,
        )
        # Resolve derived budgets while the window patch is live.
        _ = c.threshold_tokens
        _ = c.tail_token_budget
    return c


def _response(content: str) -> Any:
    resp = MagicMock()
    resp.choices = [MagicMock()]
    resp.choices[0].message.content = content
    return resp


# ---------------------------------------------------------------------------
# state projection
# ---------------------------------------------------------------------------
def snapshot(c: ContextCompressor) -> Dict[str, Any]:
    """Project only the backoff-relevant in-memory state into a golden.

    Every field is read straight off the real compressor. ``backoff_active`` and
    ``backoff_remaining`` are derived with the same ``time.monotonic`` the gate
    uses, so they read identically to what the source sees at CLOCK.value.
    """
    remaining = c._structural_no_op_backoff_until - CLOCK()
    return {
        "structural_no_op_backoff_until": c._structural_no_op_backoff_until,
        "backoff_active": remaining > 0,
        "backoff_remaining_seconds": round(remaining, 6),
        "ineffective_compression_count": c._ineffective_compression_count,
        "fallback_compression_streak": c._fallback_compression_streak,
        "anti_thrash_recovery_deadline": c._anti_thrash_recovery_deadline,
        "summary_failure_cooldown_until": c._summary_failure_cooldown_until,
        "block_reason": c._compression_block_reason(),
        "blocked_locally": c._automatic_compression_blocked_locally(),
        "blocked_locally_ignore_cooldown": c._automatic_compression_blocked_locally(
            ignore_cooldown=True
        ),
    }


def case(name: str, note: str, c: ContextCompressor, **extra: Any) -> Dict[str, Any]:
    entry: Dict[str, Any] = {
        "name": name,
        "note": note,
        "mono_now": CLOCK(),
        "backoff_seconds_const": ContextCompressor._STRUCTURAL_NO_OP_BACKOFF_SECONDS,
        "state": snapshot(c),
    }
    entry.update(extra)
    return entry


# ---------------------------------------------------------------------------
# message builders
# ---------------------------------------------------------------------------
def _msg(role: str, content: str) -> Dict[str, Any]:
    return {"role": role, "content": content}


def _plain_transcript() -> List[Dict[str, Any]]:
    """A transcript that fits inside the tail budget once the cut is forced."""
    return [
        _msg("system", "system prompt"),
        _msg("user", "turn one"),
        _msg("assistant", "answer one"),
        _msg("user", "turn two"),
        _msg("assistant", "answer two"),
        _msg("user", "turn three"),
        _msg("assistant", "answer three"),
        _msg("user", "latest request in protected tail"),
    ]


def _handoff_only_window() -> List[Dict[str, Any]]:
    """A standalone handoff summary alone in the compressible window."""
    old_summary = "WINDOW-END-SUMMARY durable facts already captured"
    return [
        _msg("system", "system prompt"),
        _msg("user", f"{SUMMARY_PREFIX}\n{old_summary}"),
        _msg("assistant", "recent tail response"),
        _msg("user", "tail request"),
        _msg("assistant", "tail answer"),
        _msg("user", "latest tail request"),
        _msg("assistant", "latest tail answer"),
    ]


def _compressible_with_handoff() -> List[Dict[str, Any]]:
    """A transcript with a resumable old handoff plus fresh compressible turns."""
    return [
        _msg("system", "system prompt"),
        _msg("user", "CONTEXT SUMMARY (from previous session):\nold summary body"),
        _msg("assistant", "handoff acknowledged after resume"),
        _msg("user", "new user turn after resume"),
        _msg("assistant", "new assistant work after resume"),
        _msg("user", "more new work after resume"),
        _msg("assistant", "latest tail response"),
        _msg("user", "final active request stays in protected tail"),
    ]


# ---------------------------------------------------------------------------
# case groups
# ---------------------------------------------------------------------------
def initialization_cases() -> List[Dict[str, Any]]:
    """Init to 0.0 and every reset that re-zeroes the backoff field."""
    cases: List[Dict[str, Any]] = []

    c = make_compressor()
    cases.append(case(
        "init_fresh_zero",
        "A freshly constructed compressor has the backoff cleared (0.0); the "
        "gate is not blocked by it.",
        c,
    ))

    # on_session_reset re-zeroes an armed backoff.
    c = make_compressor()
    c._record_structural_no_op("armed before reset")
    armed_until = c._structural_no_op_backoff_until
    c.on_session_reset()
    cases.append(case(
        "on_session_reset_clears",
        "on_session_reset() (/new, /reset) re-zeroes an armed backoff.",
        c,
        armed_until_before_reset=armed_until,
    ))

    # on_session_end re-zeroes it at a real session boundary.
    c = make_compressor()
    c._record_structural_no_op("armed before end")
    c.on_session_end("s-end", [])
    cases.append(case(
        "on_session_end_clears",
        "on_session_end() re-zeroes an armed backoff when the owning session "
        "ends.",
        c,
    ))

    # bind_session_state re-zeroes it (unbound session -> durable loads no-op).
    c = make_compressor()
    c._record_structural_no_op("armed before bind")
    c.bind_session_state(session_db=None, session_id="")
    cases.append(case(
        "bind_session_state_clears",
        "bind_session_state() re-zeroes the in-memory backoff; the durable "
        "loads are best-effort and no-op when the session is unbound.",
        c,
    ))

    return cases


def record_duration_cases() -> List[Dict[str, Any]]:
    """_record_structural_no_op arms until = now + 300 and leaves counters alone."""
    cases: List[Dict[str, Any]] = []

    c = make_compressor()
    before_ineffective = c._ineffective_compression_count
    before_fallback = c._fallback_compression_streak
    c._record_structural_no_op("insufficient window")
    cases.append(case(
        "record_arms_backoff",
        "_record_structural_no_op sets _structural_no_op_backoff_until to "
        "monotonic() + _STRUCTURAL_NO_OP_BACKOFF_SECONDS (300s).",
        c,
        expected_until=CLOCK() + ContextCompressor._STRUCTURAL_NO_OP_BACKOFF_SECONDS,
        ineffective_before=before_ineffective,
        fallback_before=before_fallback,
    ))

    # Re-arming from a later clock slides the deadline forward (not additive).
    c = make_compressor()
    c._record_structural_no_op("first")
    first_until = c._structural_no_op_backoff_until
    CLOCK.value = MONO_BASE + 50.0
    c._record_structural_no_op("second, 50s later")
    CLOCK.value = MONO_BASE  # restore base for the snapshot's remaining calc
    cases.append(case(
        "record_rearm_slides_deadline",
        "A second arming from a later monotonic reading replaces the deadline "
        "with now+300 (absolute, not additive).",
        c,
        first_until=first_until,
        second_armed_at_mono=MONO_BASE + 50.0,
        second_until=MONO_BASE + 50.0 + ContextCompressor._STRUCTURAL_NO_OP_BACKOFF_SECONDS,
    ))

    return cases


def status_while_active_cases() -> List[Dict[str, Any]]:
    """Eligibility gate + block reason while the backoff is live."""
    cases: List[Dict[str, Any]] = []

    c = make_compressor()
    c._record_structural_no_op("window empty")
    reason = c._compression_block_reason()
    blocked = c._automatic_compression_blocked_locally()
    # Over threshold: the caller-facing tuple must defer with the same reason.
    should, info_reason = c.should_compress_info(prompt_tokens=300_000)
    cases.append(case(
        "active_blocks_and_defers",
        "While the backoff is live: _compression_block_reason() is "
        "structural_backoff:<remaining>, the local gate is blocked, and "
        "should_compress_info over threshold returns (False, structural_backoff:...).",
        c,
        block_reason=reason,
        blocked_locally=blocked,
        over_threshold_should=should,
        over_threshold_reason=info_reason,
    ))

    # Halfway through the window the remaining seconds shrink, still blocked.
    c = make_compressor()
    c._record_structural_no_op("window empty")
    CLOCK.value = MONO_BASE + 120.0
    reason_mid = c._compression_block_reason()
    should_mid, info_mid = c.should_compress_info(prompt_tokens=300_000)
    cases.append(case(
        "active_midwindow_remaining_shrinks",
        "120s into the window the reason reports the shrinking remainder "
        "(180s) and the gate is still blocked.",
        c,
        block_reason=reason_mid,
        over_threshold_should=should_mid,
        over_threshold_reason=info_mid,
    ))
    CLOCK.value = MONO_BASE

    # Below threshold, an active backoff does not force a reason: should is
    # False with reason None because should_compress_info short-circuits on
    # tokens < threshold_tokens before consulting the block.
    c = make_compressor()
    c._record_structural_no_op("window empty")
    should_low, info_low = c.should_compress_info(prompt_tokens=10)
    cases.append(case(
        "active_below_threshold_no_reason",
        "Under the token threshold should_compress_info returns (False, None) "
        "regardless of the backoff: the threshold check short-circuits first.",
        c,
        below_threshold_should=should_low,
        below_threshold_reason=info_low,
        threshold_tokens=c.threshold_tokens,
    ))

    return cases


def expiry_cases() -> List[Dict[str, Any]]:
    """The backoff is transient: once monotonic passes the deadline it lapses."""
    cases: List[Dict[str, Any]] = []

    c = make_compressor()
    c._record_structural_no_op("window empty")
    # Advance past the 300s deadline. The field is unchanged; only the clock
    # moved, proving the block is transient, not a latched breaker.
    CLOCK.value = MONO_BASE + 300.0 + 1.0
    reason = c._compression_block_reason()
    blocked = c._automatic_compression_blocked_locally()
    should, info = c.should_compress_info(prompt_tokens=300_000)
    cases.append(case(
        "expired_lapses_and_resumes",
        "One second past the deadline the same over-threshold transcript "
        "compresses again: block reason None, gate unblocked, should True. "
        "_structural_no_op_backoff_until is still the old deadline; only the "
        "clock advanced.",
        c,
        block_reason=reason,
        blocked_locally=blocked,
        over_threshold_should=should,
        over_threshold_reason=info,
    ))
    CLOCK.value = MONO_BASE

    # Exactly at the deadline: remaining == 0 is NOT > 0, so it has lapsed.
    c = make_compressor()
    c._record_structural_no_op("window empty")
    CLOCK.value = MONO_BASE + 300.0
    cases.append(case(
        "expired_exact_boundary_lapsed",
        "At exactly the deadline remaining is 0.0, which is not > 0, so the "
        "backoff has lapsed (strict-greater boundary).",
        c,
        block_reason=c._compression_block_reason(),
    ))
    CLOCK.value = MONO_BASE

    return cases


def clearing_cases() -> List[Dict[str, Any]]:
    """Real progress clears the backoff: completed boundary and forced retry."""
    cases: List[Dict[str, Any]] = []

    # record_completed_compaction lifts a pending backoff.
    c = make_compressor()
    c._record_structural_no_op("armed")
    armed = c._structural_no_op_backoff_until
    c.record_completed_compaction()
    cases.append(case(
        "completed_boundary_lifts",
        "record_completed_compaction() proves the transcript was compressible "
        "and zeroes _structural_no_op_backoff_until.",
        c,
        armed_until=armed,
    ))

    # feasibility_skip completion also lifts it (the clear runs before the
    # early return for the skip branch).
    c = make_compressor()
    c._record_structural_no_op("armed")
    c.record_completed_compaction(feasibility_skip=True)
    cases.append(case(
        "completed_feasibility_skip_lifts",
        "A pre-LLM feasibility-skip completion still lifts the backoff: the "
        "clear precedes the streak-neutral early return.",
        c,
    ))

    # Manual /compress (force=True) clears the backoff then runs a real pass.
    c = make_compressor()
    c._record_structural_no_op("armed")
    CLOCK.value = MONO_BASE
    with patch("agent.context_compressor.call_llm", return_value=_response("fresh replacement summary body")):
        compressed = c.compress(_compressible_with_handoff(), force=True)
    cases.append(case(
        "forced_compress_overrides",
        "compress(force=True) (manual /compress) zeroes the backoff before "
        "running, so an explicit request always gets a real try; the forced "
        "pass committed a boundary here.",
        c,
        committed_boundary=len(compressed) < len(_compressible_with_handoff()),
        original_len=len(_compressible_with_handoff()),
        result_len=len(compressed),
    ))

    return cases


def caller_reason_cases() -> List[Dict[str, Any]]:
    """Every caller reason that arms the backoff, driven for real."""
    cases: List[Dict[str, Any]] = []

    # 1. insufficient_messages (compress, in-method).
    c = make_compressor()
    result = c.compress([_msg("system", "system prompt"), _msg("user", "hello")], current_tokens=90_000)
    telemetry = c._last_compression_telemetry or {}
    cases.append(case(
        "caller_insufficient_messages",
        "compress() with too few messages arms the backoff via "
        "_record_structural_no_op without striking the breaker "
        "(failure_class=insufficient_messages).",
        c,
        result_unchanged=(result == [_msg("system", "system prompt"), _msg("user", "hello")]),
        failure_class=telemetry.get("failure_class"),
        last_savings_pct=c._last_compression_savings_pct,
    ))

    # 2. no_compressible_window (compress, forced tail cut swallows everything).
    c = make_compressor()
    msgs = _plain_transcript()
    with patch.object(c, "_find_tail_cut_by_tokens", return_value=2):
        result = c.compress(msgs, current_tokens=90_000)
    telemetry = c._last_compression_telemetry or {}
    cases.append(case(
        "caller_no_compressible_window",
        "compress() where compress_start >= compress_end (transcript fits the "
        "tail budget) arms the backoff (failure_class=no_compressible_window).",
        c,
        result_unchanged=(result == msgs),
        failure_class=telemetry.get("failure_class"),
        last_savings_pct=c._last_compression_savings_pct,
    ))

    # 3. empty_post_handoff_window (compress, standalone handoff fills window).
    c = make_compressor()
    msgs = _handoff_only_window()
    with (
        patch.object(c, "_find_tail_cut_by_tokens", return_value=2),
        patch.object(c, "_generate_summary") as gen,
    ):
        result = c.compress(msgs, current_tokens=90_000)
    telemetry = c._last_compression_telemetry or {}
    cases.append(case(
        "caller_empty_post_handoff_window",
        "compress() where the window holds only already-summarized handoffs "
        "arms the backoff and skips the summary LLM entirely "
        "(failure_class=empty_post_handoff_window).",
        c,
        result_unchanged=(result == msgs),
        summary_llm_called=gen.called,
        failure_class=telemetry.get("failure_class"),
        last_savings_pct=c._last_compression_savings_pct,
    ))

    # 4. no_progress (conversation_compression commit-layer dead-loop breaker).
    # That path invokes exactly this recorder with this reason string; the
    # surrounding commit/rotate plumbing is deferred to Rust integration.
    c = make_compressor()
    before_ineffective = c._ineffective_compression_count
    c._record_structural_no_op("compaction returned the transcript unchanged (no_progress)")
    cases.append(case(
        "caller_no_progress_commit_layer",
        "The conversation_compression no-progress commit path arms the backoff "
        "via _record_structural_no_op('...no_progress') without a strike; the "
        "commit/rotate plumbing around it is deferred to Rust integration.",
        c,
        ineffective_before=before_ineffective,
    ))

    return cases


def overflow_bypass_cases() -> List[Dict[str, Any]]:
    """Overflow recovery ignores the cooldown but NOT the structural backoff."""
    cases: List[Dict[str, Any]] = []

    # Structural backoff active, cooldown clear: ignore_cooldown does not help.
    c = make_compressor()
    c._record_structural_no_op("window empty")
    cases.append(case(
        "overflow_bypass_still_blocked_by_structural",
        "Provider-proven overflow recovery calls the gate with "
        "ignore_cooldown=True; the structural backoff is evaluated regardless, "
        "so an armed backoff still blocks the overflow attempt.",
        c,
        blocked=c._automatic_compression_blocked_locally(ignore_cooldown=True),
        blocked_via_public=c._automatic_compression_blocked(ignore_cooldown=True),
    ))

    # Only cooldown active: ignore_cooldown unblocks, proving the structural
    # gate is a separate, non-bypassable leg.
    c = make_compressor()
    c._summary_failure_cooldown_until = CLOCK() + 200.0
    cases.append(case(
        "overflow_bypass_clears_cooldown_only",
        "With only the summary cooldown active, ignore_cooldown=True unblocks "
        "(the structural leg is untouched and inactive here) - contrast with "
        "the structural-active case, which stays blocked.",
        c,
        blocked_default=c._automatic_compression_blocked_locally(),
        blocked_ignore_cooldown=c._automatic_compression_blocked_locally(ignore_cooldown=True),
    ))
    c._summary_failure_cooldown_until = 0.0

    return cases


def non_interaction_cases() -> List[Dict[str, Any]]:
    """The backoff does not touch, and is ordered around, the breaker."""
    cases: List[Dict[str, Any]] = []

    # Arming a backoff leaves an existing ineffective strike untouched.
    c = make_compressor()
    c._record_ineffective_compression_verdict(1)
    strike_before = c._ineffective_compression_count
    c._record_structural_no_op("window empty")
    cases.append(case(
        "structural_does_not_strike_breaker",
        "_record_structural_no_op arms the backoff but never increments the "
        "ineffective counter or the fallback streak; the two mechanisms are "
        "independent (that separation IS #93022's fix).",
        c,
        ineffective_before=strike_before,
        ineffective_after=c._ineffective_compression_count,
    ))

    # Precedence: cooldown > structural > ineffective in _compression_block_reason.
    c = make_compressor()
    c._record_structural_no_op("window empty")
    c._ineffective_compression_count = 2  # latched breaker also true
    reason_structural_wins = c._compression_block_reason()
    cases.append(case(
        "structural_precedes_ineffective_in_reason",
        "When both the structural backoff and the latched ineffective breaker "
        "are active, _compression_block_reason reports structural_backoff "
        "(structural is checked before ineffective).",
        c,
        block_reason=reason_structural_wins,
    ))

    # Cooldown precedes structural.
    c = make_compressor()
    c._summary_failure_cooldown_until = CLOCK() + 100.0
    c._record_structural_no_op("window empty")
    reason_cooldown_wins = c._compression_block_reason()
    cases.append(case(
        "cooldown_precedes_structural_in_reason",
        "When both the summary cooldown and the structural backoff are active, "
        "_compression_block_reason reports cooldown (cooldown is checked "
        "before structural).",
        c,
        block_reason=reason_cooldown_wins,
    ))
    c._summary_failure_cooldown_until = 0.0

    # After the structural backoff lapses, a latched breaker still blocks:
    # the two are independent, the ineffective one is not transient.
    c = make_compressor()
    c._record_structural_no_op("window empty")
    c._ineffective_compression_count = 2
    CLOCK.value = MONO_BASE + 300.0 + 1.0
    reason_after = c._compression_block_reason()
    cases.append(case(
        "ineffective_survives_structural_expiry",
        "Once the structural backoff lapses, a latched ineffective breaker "
        "still reports 'ineffective': the structural timer never cleared or "
        "armed the breaker.",
        c,
        block_reason=reason_after,
    ))
    CLOCK.value = MONO_BASE

    return cases


# ---------------------------------------------------------------------------
# corpus assembly
# ---------------------------------------------------------------------------
def build() -> Dict[str, Any]:
    corpus: Dict[str, Any] = {
        "_meta": {
            "description": (
                "Golden oracle for the full-compression structural no-op "
                "backoff (#93022). Every value is the state or return of a "
                "real ContextCompressor method executed under a patched "
                "monotonic clock; no backoff decision is reimplemented."
            ),
            "fixed_window_tokens": FIXED_WINDOW,
            "mono_base": MONO_BASE,
            "backoff_seconds_const": ContextCompressor._STRUCTURAL_NO_OP_BACKOFF_SECONDS,
            "threshold_percent": 0.85,
            "protect_first_n": 1,
            "protect_last_n": 1,
            "authoritative_sources": [
                "agent/context_compressor.py: ContextCompressor.__init__"
                " / on_session_reset / on_session_end / bind_session_state",
                "agent/context_compressor.py: _record_structural_no_op"
                " / _STRUCTURAL_NO_OP_BACKOFF_SECONDS",
                "agent/context_compressor.py: _compression_block_reason"
                " / _automatic_compression_blocked_locally"
                " / _automatic_compression_blocked",
                "agent/context_compressor.py: should_compress_info / compress"
                " / record_completed_compaction",
                "agent/conversation_compression.py: no-progress commit-layer"
                " recorder call (dead-loop breaker, ~line 4467)",
            ],
            "deferred_to_rust_integration": [
                "the conversation_compression commit/rotate plumbing that wraps"
                " the no-progress recorder call (session split, telemetry emit)",
                "the overflow-recovery loop in conversation_loop that consumes"
                " the ignore_cooldown gate result",
                "durable-guard DB round-trips (backoff is in-memory only; no"
                " persistence leg exists to cover)",
            ],
        },
        "initialization": initialization_cases(),
        "record_duration": record_duration_cases(),
        "status_while_active": status_while_active_cases(),
        "expiry": expiry_cases(),
        "clearing": clearing_cases(),
        "caller_reasons": caller_reason_cases(),
        "overflow_bypass": overflow_bypass_cases(),
        "non_interaction": non_interaction_cases(),
    }
    return corpus


def main() -> int:
    with patch("time.monotonic", CLOCK):
        corpus = build()
    text = json.dumps(corpus, indent=2, ensure_ascii=False, sort_keys=True) + "\n"
    if "--check" in sys.argv[1:]:
        if not OUT.exists():
            print(f"FAIL: {OUT} missing; run the generator first", file=sys.stderr)
            return 1
        existing = OUT.read_text(encoding="utf-8")
        if existing != text:
            print("FAIL: goldens differ from freshly generated corpus", file=sys.stderr)
            return 1
        print("OK: goldens match freshly generated corpus")
        return 0
    OUT.write_text(text, encoding="utf-8")
    total = sum(len(v) for k, v in corpus.items() if not k.startswith("_"))
    print(f"wrote {OUT} ({total} cases)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
