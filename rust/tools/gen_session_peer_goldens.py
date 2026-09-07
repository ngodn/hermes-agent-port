#!/usr/bin/env python3
"""Execute source peer recovery queries against SQLite; --check verifies fixtures."""
import ast
import contextlib
import json
import sqlite3
import sys
import typing
from pathlib import Path
from typing import Any, Dict, List, Optional

root = Path(__file__).resolve().parents[2]

# 1. Parse hermes_state_common.py
common_tree = ast.parse((root / "hermes_state_common.py").read_text())
needed = {
    "_RESET_END_REASONS",
    "_RESET_END_REASONS_SQL",
    "_RECOVERABLE_END_REASONS",
    "_RECOVERABLE_END_REASONS_SQL",
    "SCHEMA_SQL",
}
common_nodes = [
    n
    for n in common_tree.body
    if isinstance(n, ast.Assign)
    and any(isinstance(t, ast.Name) and t.id in needed for t in n.targets)
]
namespace = dict(vars(typing))
exec(compile(ast.Module(body=common_nodes, type_ignores=[]), "common-source", "exec"), namespace)

# 2. Parse hermes_state.py
state_tree = ast.parse((root / "hermes_state.py").read_text())
method_node = next(
    n
    for n in ast.walk(state_tree)
    if isinstance(n, ast.FunctionDef) and n.name == "find_latest_gateway_session_for_peer"
)
exec(compile(ast.Module(body=[method_node], type_ignores=[]), "state-method-source", "exec"), namespace)

class PeerHarness:
    def __init__(self, conn: sqlite3.Connection, owner_profile: Optional[str] = None):
        self.conn = conn
        self.owner_profile = owner_profile

    @contextlib.contextmanager
    def _read_ctx(self):
        yield self.conn

    def _own_profile_name(self) -> Optional[str]:
        return self.owner_profile

    @staticmethod
    def _session_row_dict(row: sqlite3.Row) -> Dict[str, Any]:
        data = dict(row)
        if "_system_prompt_resolved" in data:
            resolved = data.pop("_system_prompt_resolved")
            if "system_prompt" in data:
                data["system_prompt"] = resolved
        return data

    find_latest_gateway_session_for_peer = namespace["find_latest_gateway_session_for_peer"]

def make_session(
    id: str,
    source: str = "telegram",
    session_key: Optional[str] = None,
    user_id: Optional[str] = None,
    chat_id: Optional[str] = None,
    chat_type: Optional[str] = None,
    thread_id: Optional[str] = None,
    profile_name: Optional[str] = None,
    started_at: float = 1000.0,
    ended_at: Optional[float] = None,
    end_reason: Optional[str] = None,
    last_activity_at: Optional[float] = None,
    message_count: Optional[int] = 0,
    system_prompt: Optional[str] = None,
    system_prompt_hash: Optional[str] = None,
) -> Dict[str, Any]:
    return {
        "id": id,
        "source": source,
        "session_key": session_key,
        "user_id": user_id,
        "chat_id": chat_id,
        "chat_type": chat_type,
        "thread_id": thread_id,
        "profile_name": profile_name,
        "started_at": started_at,
        "ended_at": ended_at,
        "end_reason": end_reason,
        "last_activity_at": last_activity_at,
        "message_count": message_count,
        "system_prompt": system_prompt,
        "system_prompt_hash": system_prompt_hash,
    }

def make_message(
    session_id: str,
    role: str = "user",
    content: Optional[str] = "test message",
    timestamp: float = 1000.0,
) -> Dict[str, Any]:
    return {
        "session_id": session_id,
        "role": role,
        "content": content,
        "timestamp": timestamp,
    }

def make_system_prompt(hash: str, prompt: str) -> Dict[str, Any]:
    return {
        "hash": hash,
        "prompt": prompt,
    }

def make_lookup(
    source: str = "telegram",
    user_id: Optional[str] = None,
    session_key: Optional[str] = None,
    chat_id: Optional[str] = None,
    chat_type: Optional[str] = None,
    thread_id: Optional[str] = None,
) -> Dict[str, Any]:
    return {
        "source": source,
        "user_id": user_id,
        "session_key": session_key,
        "chat_id": chat_id,
        "chat_type": chat_type,
        "thread_id": thread_id,
    }

def run_case(case: Dict[str, Any]) -> Optional[str]:
    conn = sqlite3.connect(":memory:")
    conn.row_factory = sqlite3.Row
    conn.executescript(namespace["SCHEMA_SQL"])

    for sp in case.get("system_prompts", []):
        cols = list(sp.keys())
        conn.execute(
            f"INSERT INTO system_prompts ({', '.join(cols)}) VALUES ({', '.join('?' for _ in cols)})",
            list(sp.values()),
        )

    for s in case.get("sessions", []):
        cols = list(s.keys())
        conn.execute(
            f"INSERT INTO sessions ({', '.join(cols)}) VALUES ({', '.join('?' for _ in cols)})",
            list(s.values()),
        )

    for m in case.get("messages", []):
        cols = list(m.keys())
        conn.execute(
            f"INSERT INTO messages ({', '.join(cols)}) VALUES ({', '.join('?' for _ in cols)})",
            list(m.values()),
        )

    harness = PeerHarness(conn, owner_profile=case.get("owner_profile"))
    row = harness.find_latest_gateway_session_for_peer(**case["lookup"])
    conn.close()
    return row["id"] if row else None

cases: List[Dict[str, Any]] = []

def add_case(
    name: str,
    description: str,
    owner_profile: Optional[str],
    lookup: Dict[str, Any],
    expected_session_id: Optional[str],
    sessions: List[Dict[str, Any]],
    messages: Optional[List[Dict[str, Any]]] = None,
    system_prompts: Optional[List[Dict[str, Any]]] = None,
):
    assert "\u2014" not in description and "\u2013" not in description, (
        f"Description in {name} contains prohibited dash"
    )
    case = {
        "name": name,
        "description": description,
        "owner_profile": owner_profile,
        "lookup": lookup,
        "system_prompts": system_prompts or [],
        "sessions": sessions,
        "messages": messages or [],
        "expected_session_id": expected_session_id,
    }
    actual = run_case(case)
    assert actual == expected_session_id, (
        f"Case '{name}' mismatch: expected {expected_session_id}, got {actual}"
    )
    cases.append(case)

# ==============================================================================
# Group 1: Exact-Key Precedence (12 cases)
# ==============================================================================

# 1. Basic exact key match for single active session.
add_case(
    name="exact_key_single_active",
    description="Exact session_key matches a single active session with messages.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1", user_id="user1", chat_id="chat1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="agent:main:telegram:dm:user1", user_id="user1", chat_id="chat1", chat_type="dm", message_count=1, last_activity_at=1050.0),
    ],
)

# 2. Exact key candidate wins over newer fallback candidate.
add_case(
    name="exact_key_beats_newer_fallback_candidate",
    description="Exact session_key candidate wins over newer fallback candidate with higher message count.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1", user_id="user1", chat_id="chat1", chat_type="dm"),
    expected_session_id="s_exact",
    sessions=[
        make_session("s_exact", session_key="agent:main:telegram:dm:user1", user_id="user1", chat_id="chat1", chat_type="dm", message_count=1, last_activity_at=1000.0),
        make_session("s_fallback", session_key="other_key", user_id="user1", chat_id="chat1", chat_type="dm", message_count=10, last_activity_at=2000.0),
    ],
)

# 3. Exact key match takes precedence even if exact row is empty.
add_case(
    name="exact_key_beats_newer_fallback_even_if_exact_is_empty",
    description="Exact key candidate is selected even if empty, preventing fallback from running.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1", user_id="user1", chat_id="chat1", chat_type="dm"),
    expected_session_id="s_exact_empty",
    sessions=[
        make_session("s_exact_empty", session_key="agent:main:telegram:dm:user1", user_id="user1", chat_id="chat1", chat_type="dm", message_count=0, last_activity_at=1000.0),
        make_session("s_fallback_active", session_key="other_key", user_id="user1", chat_id="chat1", chat_type="dm", message_count=5, last_activity_at=2000.0),
    ],
)

# 4. Exact key query matches on session_key and source alone without checking peer tuple.
add_case(
    name="exact_key_ignores_peer_tuple_mismatches_on_row",
    description="Exact key query succeeds when session_key matches even if row peer tuple differs from lookup.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1", user_id="user1", chat_id="chat1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="agent:main:telegram:dm:user1", user_id="different_user", chat_id="different_chat", chat_type="group", message_count=1, last_activity_at=1050.0),
    ],
)

# 5. Exact key query filters on both session_key and source.
add_case(
    name="exact_key_source_must_match",
    description="Exact key query filters on both session_key and source matching lookup arguments.",
    owner_profile="default",
    lookup=make_lookup(source="telegram", session_key="shared_key", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s_telegram",
    sessions=[
        make_session("s_slack", source="slack", session_key="shared_key", message_count=1, last_activity_at=2000.0),
        make_session("s_telegram", source="telegram", session_key="shared_key", message_count=1, last_activity_at=1000.0),
    ],
)

# 6. Profile owner fencing only applies to fallback query, not exact key query.
add_case(
    name="exact_key_ignores_owner_profile_fencing",
    description="Exact key query matches row regardless of profile_name because profile fencing is fallback only.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:bot2:telegram:dm:user1", user_id="user1", chat_id="chat1", chat_type="dm"),
    expected_session_id="s_bot2",
    sessions=[
        make_session("s_bot2", session_key="agent:bot2:telegram:dm:user1", profile_name="bot2", message_count=1, last_activity_at=1050.0),
    ],
)

# 7. Multiple exact key candidates ordered by recency.
add_case(
    name="exact_key_two_active_candidates_recency_order",
    description="When multiple exact key candidates exist with messages, the newest activity wins.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1"),
    expected_session_id="s_new",
    sessions=[
        make_session("s_old", session_key="agent:main:telegram:dm:user1", message_count=1, last_activity_at=1000.0),
        make_session("s_new", session_key="agent:main:telegram:dm:user1", message_count=1, last_activity_at=2000.0),
    ],
)

# 8. COALESCE prefers actual last_activity_at over started_at.
add_case(
    name="exact_key_prefers_last_activity_over_started_at",
    description="Ranking prefers recent last_activity_at over started_at in exact key query.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1"),
    expected_session_id="s_early_start_late_activity",
    sessions=[
        make_session("s_early_start_late_activity", session_key="agent:main:telegram:dm:user1", started_at=500.0, last_activity_at=2500.0, message_count=1),
        make_session("s_late_start_early_activity", session_key="agent:main:telegram:dm:user1", started_at=1500.0, last_activity_at=1800.0, message_count=1),
    ],
)

# 9. When last_activity_at is null, started_at is used for recency ordering.
add_case(
    name="exact_key_falls_back_to_started_at_when_activity_null",
    description="When last_activity_at is null, started_at is used for recency ordering in exact key query.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1"),
    expected_session_id="s2",
    sessions=[
        make_session("s1", session_key="agent:main:telegram:dm:user1", started_at=1000.0, last_activity_at=None, message_count=1),
        make_session("s2", session_key="agent:main:telegram:dm:user1", started_at=1200.0, last_activity_at=None, message_count=1),
    ],
)

# 10. Direct system_prompt column is preserved when system_prompt_hash is null.
add_case(
    name="exact_key_system_prompt_direct",
    description="Direct system_prompt column is retained when system_prompt_hash is null.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="agent:main:telegram:dm:user1", system_prompt="direct prompt text", system_prompt_hash=None, message_count=1),
    ],
)

# 11. system_prompts table lookup resolves prompt by system_prompt_hash.
add_case(
    name="exact_key_system_prompt_resolved_from_hash",
    description="system_prompts table join resolves prompt from system_prompt_hash.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1"),
    expected_session_id="s1",
    system_prompts=[
        make_system_prompt("hash_abc", "resolved prompt from table"),
    ],
    sessions=[
        make_session("s1", session_key="agent:main:telegram:dm:user1", system_prompt=None, system_prompt_hash="hash_abc", message_count=1),
    ],
)

# 12. Joined hash prompt overrides direct system_prompt via COALESCE.
add_case(
    name="exact_key_system_prompt_hash_overrides_direct",
    description="COALESCE resolves hash prompt over inline direct prompt when both exist.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1"),
    expected_session_id="s1",
    system_prompts=[
        make_system_prompt("hash_abc", "hash prompt wins"),
    ],
    sessions=[
        make_session("s1", session_key="agent:main:telegram:dm:user1", system_prompt="inline prompt loses", system_prompt_hash="hash_abc", message_count=1),
    ],
)

# ==============================================================================
# Group 2: Message-Bearing versus Empty Rows (14 cases)
# ==============================================================================

# 13. Exact key order puts _has_messages DESC before activity recency.
add_case(
    name="exact_key_has_messages_beats_newer_empty",
    description="Exact key candidate with messages beats newer empty exact key candidate.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1"),
    expected_session_id="s_has_msgs",
    sessions=[
        make_session("s_has_msgs", session_key="agent:main:telegram:dm:user1", last_activity_at=1000.0, message_count=2),
        make_session("s_empty", session_key="agent:main:telegram:dm:user1", last_activity_at=2000.0, message_count=0),
    ],
)

# 14. An empty exact key row is returned rather than null to avoid duplicate minting.
add_case(
    name="exact_key_empty_row_returned_if_only_candidate",
    description="An empty exact key row is returned rather than null when no other candidate exists.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1"),
    expected_session_id="s_empty",
    sessions=[
        make_session("s_empty", session_key="agent:main:telegram:dm:user1", last_activity_at=1000.0, message_count=0),
    ],
)

# 15. Multiple empty rows for exact key are ranked by recency.
add_case(
    name="exact_key_multiple_empty_rows_ordered_by_recency",
    description="When all exact key candidates are empty, the newest activity wins.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1"),
    expected_session_id="s_empty_new",
    sessions=[
        make_session("s_empty_old", session_key="agent:main:telegram:dm:user1", last_activity_at=1000.0, message_count=0),
        make_session("s_empty_new", session_key="agent:main:telegram:dm:user1", last_activity_at=2000.0, message_count=0),
    ],
)

# 16. Positive message_count marks row as message-bearing without message table rows.
add_case(
    name="exact_key_message_count_positive_without_message_rows",
    description="message_count positive counts as message-bearing without rows in messages table.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="agent:main:telegram:dm:user1", last_activity_at=1000.0, message_count=5),
        make_session("s_empty", session_key="agent:main:telegram:dm:user1", last_activity_at=2000.0, message_count=0),
    ],
)

# 17. Row in messages table marks row as message-bearing even with message_count 0.
add_case(
    name="exact_key_messages_table_row_without_message_count",
    description="EXISTS in messages table marks row as message-bearing even with message_count 0.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="agent:main:telegram:dm:user1", last_activity_at=1000.0, message_count=0),
        make_session("s_empty", session_key="agent:main:telegram:dm:user1", last_activity_at=2000.0, message_count=0),
    ],
    messages=[
        make_message("s1", role="user", content="hello"),
    ],
)

# 18. NULL message_count with messages table row treats session as message-bearing.
add_case(
    name="exact_key_message_count_null_with_messages_row",
    description="NULL message_count with existing message row treats session as message-bearing.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="agent:main:telegram:dm:user1", last_activity_at=1000.0, message_count=None),
        make_session("s_empty", session_key="agent:main:telegram:dm:user1", last_activity_at=2000.0, message_count=0),
    ],
    messages=[
        make_message("s1", role="user", content="hello"),
    ],
)

# 19. NULL message_count without messages rows is treated as empty.
add_case(
    name="exact_key_message_count_null_without_messages_row_is_empty",
    description="NULL message_count without message rows is treated as empty but returned for exact key.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="agent:main:telegram:dm:user1", last_activity_at=1000.0, message_count=None),
    ],
)

# 20. Fallback query accepts candidate with positive message_count.
add_case(
    name="fallback_requires_messages_message_count_positive",
    description="Fallback candidate with positive message_count satisfies message requirement.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", message_count=1),
    ],
)

# 21. Fallback query accepts candidate with message in messages table.
add_case(
    name="fallback_requires_messages_from_messages_table",
    description="Fallback candidate with row in messages table satisfies message requirement.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", message_count=0),
    ],
    messages=[
        make_message("s1", role="user", content="hello"),
    ],
)

# 22. Fallback query strictly rejects empty candidate with message_count 0.
add_case(
    name="fallback_rejects_empty_row_message_count_zero",
    description="Fallback query strictly rejects empty candidate row with message_count 0.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", message_count=0),
    ],
)

# 23. Fallback query strictly rejects empty candidate with NULL message_count.
add_case(
    name="fallback_rejects_empty_row_message_count_null",
    description="Fallback query strictly rejects candidate with NULL message_count and no messages.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", message_count=None),
    ],
)

# 24. Fallback ignores newer empty row and selects older message-bearing row.
add_case(
    name="fallback_empty_row_newer_message_bearing_older",
    description="Fallback query filters out newer empty row and returns older message-bearing row.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s_has_msg",
    sessions=[
        make_session("s_has_msg", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=1000.0, message_count=1),
        make_session("s_empty", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=2000.0, message_count=0),
    ],
)

# 25. Fallback returns null when all matching peer candidates are empty.
add_case(
    name="fallback_all_candidates_empty_returns_null",
    description="When all fallback candidates have message_count 0, fallback returns null.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=1000.0, message_count=0),
        make_session("s2", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=2000.0, message_count=0),
    ],
)

# 26. Messages for other session do not satisfy EXISTS clause.
add_case(
    name="fallback_messages_for_other_session_do_not_count",
    description="Messages associated with a different session do not satisfy message check for target.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", message_count=0),
        make_session("s_other", session_key=None, user_id="u2", chat_id="c2", chat_type="dm", message_count=0),
    ],
    messages=[
        make_message("s_other", role="user", content="message for other session"),
    ],
)

# ==============================================================================
# Group 3: Recoverable End Reasons (16 cases)
# ==============================================================================

# 27. Active session with ended_at null is eligible for recovery.
add_case(
    name="recoverable_active_session_ended_at_null",
    description="Active session with ended_at null is eligible for recovery.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", ended_at=None, end_reason=None, message_count=1),
    ],
)

# 28. Accidental closure from agent_close is recoverable.
add_case(
    name="recoverable_reason_agent_close",
    description="Accidental closure from agent_close is recoverable.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", ended_at=1200.0, end_reason="agent_close", message_count=1),
    ],
)

# 29. Accidental closure from ws_orphan_reap is recoverable.
add_case(
    name="recoverable_reason_ws_orphan_reap",
    description="Accidental closure from ws_orphan_reap is recoverable.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", ended_at=1200.0, end_reason="ws_orphan_reap", message_count=1),
    ],
)

# 30. Accidental closure from superseded_by_resume is recoverable.
add_case(
    name="recoverable_reason_superseded_by_resume",
    description="Accidental closure from superseded_by_resume is recoverable.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", ended_at=1200.0, end_reason="superseded_by_resume", message_count=1),
    ],
)

# 31. Accidental closure from startup_orphan_reap is recoverable.
add_case(
    name="recoverable_reason_startup_orphan_reap",
    description="Accidental closure from startup_orphan_reap is recoverable.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", ended_at=1200.0, end_reason="startup_orphan_reap", message_count=1),
    ],
)

# 32. Intentional session_reset closure cannot be recovered.
add_case(
    name="non_recoverable_reason_session_reset",
    description="Intentional session_reset closure cannot be recovered.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", ended_at=1200.0, end_reason="session_reset", message_count=1),
    ],
)

# 33. Intentional session_switch closure cannot be recovered.
add_case(
    name="non_recoverable_reason_session_switch",
    description="Intentional session_switch closure cannot be recovered.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", ended_at=1200.0, end_reason="session_switch", message_count=1),
    ],
)

# 34. Intentional idle timeout closure cannot be recovered.
add_case(
    name="non_recoverable_reason_idle",
    description="Intentional idle timeout closure cannot be recovered.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", ended_at=1200.0, end_reason="idle", message_count=1),
    ],
)

# 35. Intentional daily boundary closure cannot be recovered.
add_case(
    name="non_recoverable_reason_daily",
    description="Intentional daily boundary closure cannot be recovered.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", ended_at=1200.0, end_reason="daily", message_count=1),
    ],
)

# 36. Suspended session closure cannot be recovered.
add_case(
    name="non_recoverable_reason_suspended",
    description="Suspended session closure cannot be recovered.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", ended_at=1200.0, end_reason="suspended", message_count=1),
    ],
)

# 37. Expired resume pending closure cannot be recovered.
add_case(
    name="non_recoverable_reason_resume_pending_expired",
    description="Expired resume pending closure cannot be recovered.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", ended_at=1200.0, end_reason="resume_pending_expired", message_count=1),
    ],
)

# 38. Compressed parent session is non-recoverable.
add_case(
    name="non_recoverable_reason_compression",
    description="Compressed parent session is non-recoverable.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", ended_at=1200.0, end_reason="compression", message_count=1),
    ],
)

# 39. Branched parent session is non-recoverable.
add_case(
    name="non_recoverable_reason_branched",
    description="Branched parent session is non-recoverable.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", ended_at=1200.0, end_reason="branched", message_count=1),
    ],
)

# 40. Arbitrary unknown end reason is non-recoverable.
add_case(
    name="non_recoverable_reason_unknown",
    description="Arbitrary unknown end reason is non-recoverable.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", ended_at=1200.0, end_reason="unexpected_crash", message_count=1),
    ],
)

# 41. Recoverable older session beats non-recoverable newer session in exact key query.
add_case(
    name="recoverable_older_beats_non_recoverable_newer_in_exact_key",
    description="Older recoverable session is selected when newer session has non-recoverable end reason.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s_rec",
    sessions=[
        make_session("s_rec", session_key="k1", last_activity_at=1000.0, ended_at=1100.0, end_reason="agent_close", message_count=1),
        make_session("s_non_rec", session_key="k1", last_activity_at=1500.0, ended_at=1600.0, end_reason="compression", message_count=1),
    ],
)

# 42. Fallback candidate with recoverable end_reason is eligible.
add_case(
    name="recoverable_in_peer_fallback",
    description="Fallback candidate ended with ws_orphan_reap is eligible and recovered.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=1000.0, ended_at=1100.0, end_reason="ws_orphan_reap", message_count=1),
    ],
)

# ==============================================================================
# Group 4: Strict Reset Boundary Timestamps and Fencing (22 cases)
# ==============================================================================

# 43. Newer session_reset row fences recovery of older candidate for the same key.
add_case(
    name="reset_boundary_exact_newer_fences_older_candidate",
    description="Newer session_reset row fences recovery of older candidate for the same key.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b_reset", session_key="k1", started_at=1100.0, ended_at=1200.0, end_reason="session_reset"),
    ],
)

# 44. Older session_reset row does not fence candidate whose activity is newer than reset.
add_case(
    name="reset_boundary_exact_older_does_not_fence_newer_candidate",
    description="Older session_reset row does not fence candidate whose activity is newer than reset ended_at.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b_reset", session_key="k1", started_at=800.0, ended_at=900.0, end_reason="session_reset"),
    ],
)

# 45. Equal timestamp ended_at == activity timestamp does not fence the candidate.
add_case(
    name="reset_boundary_exact_equal_timestamp_does_not_fence",
    description="Strict inequality in SQL means ended_at equal to activity timestamp does not fence candidate.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", started_at=500.0, last_activity_at=1000.0, message_count=1),
        make_session("b_reset", session_key="k1", started_at=500.0, ended_at=1000.0, end_reason="session_reset"),
    ],
)

# 46. ended_at strictly greater than candidate activity timestamp fences the candidate.
add_case(
    name="reset_boundary_exact_strictly_greater_timestamp_fences",
    description="ended_at strictly greater than candidate activity timestamp fences the candidate.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", started_at=500.0, last_activity_at=1000.0, message_count=1),
        make_session("b_reset", session_key="k1", started_at=500.0, ended_at=1000.0001, end_reason="session_reset"),
    ],
)

# 47. When last_activity_at is null, started_at is used for reset fence comparison.
add_case(
    name="reset_boundary_uses_started_at_when_activity_null_fenced",
    description="When last_activity_at is null, COALESCE falls back to started_at for reset fence comparison.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", started_at=1000.0, last_activity_at=None, message_count=1),
        make_session("b_reset", session_key="k1", started_at=500.0, ended_at=1001.0, end_reason="session_reset"),
    ],
)

# 48. When candidate started_at is newer than reset ended_at, candidate is not fenced.
add_case(
    name="reset_boundary_uses_started_at_when_activity_null_not_fenced",
    description="When candidate started_at is newer than reset ended_at, candidate is not fenced.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", started_at=1000.0, last_activity_at=None, message_count=1),
        make_session("b_reset", session_key="k1", started_at=500.0, ended_at=999.0, end_reason="session_reset"),
    ],
)

# 49. session_switch end_reason fences older candidates.
add_case(
    name="reset_boundary_reason_session_switch_fences",
    description="session_switch end_reason fences older candidates.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key="k1", ended_at=1100.0, end_reason="session_switch"),
    ],
)

# 50. idle end_reason fences older candidates.
add_case(
    name="reset_boundary_reason_idle_fences",
    description="idle end_reason fences older candidates.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key="k1", ended_at=1100.0, end_reason="idle"),
    ],
)

# 51. daily end_reason fences older candidates.
add_case(
    name="reset_boundary_reason_daily_fences",
    description="daily end_reason fences older candidates.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key="k1", ended_at=1100.0, end_reason="daily"),
    ],
)

# 52. suspended end_reason fences older candidates.
add_case(
    name="reset_boundary_reason_suspended_fences",
    description="suspended end_reason fences older candidates.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key="k1", ended_at=1100.0, end_reason="suspended"),
    ],
)

# 53. resume_pending_expired end_reason fences older candidates.
add_case(
    name="reset_boundary_reason_resume_pending_expired_fences",
    description="resume_pending_expired end_reason fences older candidates.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key="k1", ended_at=1100.0, end_reason="resume_pending_expired"),
    ],
)

# 54. agent_close is not in reset reasons and does not fence older candidates.
add_case(
    name="reset_boundary_reason_agent_close_does_not_fence",
    description="agent_close is not in reset reasons and does not fence older candidates.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key="k1", ended_at=1100.0, end_reason="agent_close"),
    ],
)

# 55. ws_orphan_reap is not in reset reasons and does not fence older candidates.
add_case(
    name="reset_boundary_reason_ws_orphan_reap_does_not_fence",
    description="ws_orphan_reap is not in reset reasons and does not fence older candidates.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key="k1", ended_at=1100.0, end_reason="ws_orphan_reap"),
    ],
)

# 56. compression is not in reset reasons and does not fence older candidates.
add_case(
    name="reset_boundary_reason_compression_does_not_fence",
    description="compression is not in reset reasons and does not fence older candidates.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key="k1", ended_at=1100.0, end_reason="compression"),
    ],
)

# 57. Unknown end reason does not fence older candidates.
add_case(
    name="reset_boundary_reason_unknown_does_not_fence",
    description="Unknown end reason does not fence older candidates.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key="k1", ended_at=1100.0, end_reason="crash"),
    ],
)

# 58. Row with ended_at NULL cannot act as reset boundary even if end_reason is set.
add_case(
    name="reset_boundary_open_session_does_not_fence",
    description="Row with ended_at NULL cannot act as reset boundary even if end_reason is set.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key="k1", started_at=1100.0, ended_at=None, end_reason="session_reset"),
    ],
)

# 59. In exact key query, reset row with different key does not fence candidate.
add_case(
    name="reset_boundary_different_key_does_not_fence_exact_query",
    description="In exact key query, reset row with different key does not fence candidate.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key="k2", ended_at=1100.0, end_reason="session_reset"),
    ],
)

# 60. Reset boundary on different source does not fence exact key candidate.
add_case(
    name="reset_boundary_different_source_does_not_fence_exact_query",
    description="Reset boundary on different source does not fence exact key candidate.",
    owner_profile="default",
    lookup=make_lookup(source="telegram", session_key="k1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", source="telegram", session_key="k1", last_activity_at=1000.0, message_count=1),
        make_session("b", source="slack", session_key="k1", ended_at=1100.0, end_reason="session_reset"),
    ],
)

# 61. In fallback query, reset boundary matching the peer tuple fences older candidates.
add_case(
    name="reset_boundary_fallback_fences_by_peer_tuple",
    description="In fallback query, reset boundary matching the peer tuple fences older candidates.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", ended_at=1100.0, end_reason="session_reset"),
    ],
)

# 62. Reset boundary for different chat does not fence fallback candidate.
add_case(
    name="reset_boundary_fallback_different_chat_does_not_fence",
    description="Reset boundary for different chat does not fence fallback candidate.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key=None, user_id="u1", chat_id="c2", chat_type="dm", ended_at=1100.0, end_reason="session_reset"),
    ],
)

# 63. Reset boundary for different user in group chat does not fence fallback candidate.
add_case(
    name="reset_boundary_fallback_different_user_does_not_fence",
    description="Reset boundary for different user in group chat does not fence fallback candidate.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="group"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="group", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key=None, user_id="u2", chat_id="c1", chat_type="group", ended_at=1100.0, end_reason="session_reset"),
    ],
)

# 64. Reset boundary on different thread does not fence fallback candidate.
add_case(
    name="reset_boundary_fallback_different_thread_does_not_fence",
    description="Reset boundary on different thread does not fence fallback candidate.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="group", thread_id="t1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="group", thread_id="t1", last_activity_at=1000.0, message_count=1),
        make_session("b", session_key=None, user_id="u1", chat_id="c1", chat_type="group", thread_id="t2", ended_at=1100.0, end_reason="session_reset"),
    ],
)

# ==============================================================================
# Group 5: Fallback Availability and Disabled Fallback (10 cases)
# ==============================================================================

# 65. When exact key misses and chat_id is None, fallback is disabled and returns null.
add_case(
    name="fallback_disabled_when_chat_id_is_none",
    description="When exact key misses and chat_id is None, fallback is disabled and returns null.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id=None, chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id=None, chat_type="dm", message_count=1),
    ],
)

# 66. When exact key misses and chat_type is None, fallback is disabled and returns null.
add_case(
    name="fallback_disabled_when_chat_type_is_none",
    description="When exact key misses and chat_type is None, fallback is disabled and returns null.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type=None),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type=None, message_count=1),
    ],
)

# 67. When exact key misses and both chat_id and chat_type are None, returns null.
add_case(
    name="fallback_disabled_when_both_chat_id_and_chat_type_none",
    description="When exact key misses and both chat_id and chat_type are None, returns null.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id=None, chat_type=None),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id=None, chat_type=None, message_count=1),
    ],
)

# 68. Lookup with session_key None returns null immediately before executing any SQL.
add_case(
    name="lookup_session_key_none_returns_none_immediately",
    description="Lookup with session_key None returns null immediately before executing any SQL.",
    owner_profile="default",
    lookup=make_lookup(session_key=None, user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", user_id="u1", chat_id="c1", chat_type="dm", message_count=1),
    ],
)

# 69. Lookup with empty string session_key returns null immediately before executing any SQL.
add_case(
    name="lookup_session_key_empty_string_returns_none_immediately",
    description="Lookup with empty string session_key returns null immediately before executing any SQL.",
    owner_profile="default",
    lookup=make_lookup(session_key="", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key="k1", user_id="u1", chat_id="c1", chat_type="dm", message_count=1),
    ],
)

# 70. Fallback runs and succeeds when both chat_id and chat_type are non-None.
add_case(
    name="fallback_enabled_when_chat_id_and_chat_type_present",
    description="Fallback runs and succeeds when both chat_id and chat_type are non-None.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", message_count=1),
    ],
)

# 71. Fallback recovers a session row that never received a session_key.
add_case(
    name="fallback_recovers_candidate_with_null_session_key",
    description="Fallback recovers a session row that never received a session_key.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", message_count=1),
    ],
)

# 72. Fallback recovers a session row whose persisted session_key does not match lookup key.
add_case(
    name="fallback_recovers_candidate_with_different_session_key",
    description="Fallback recovers a session row whose persisted session_key does not match lookup key.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_new", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k_old", user_id="u1", chat_id="c1", chat_type="dm", message_count=1),
    ],
)

# 73. Fallback query matches thread_id when present.
add_case(
    name="fallback_matches_with_thread_id_present",
    description="Fallback query matches thread_id when present on both row and lookup.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="group", thread_id="t1"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="group", thread_id="t1", message_count=1),
    ],
)

# 74. Fallback query matches when thread_id is null on both row and lookup.
add_case(
    name="fallback_matches_with_thread_id_null",
    description="Fallback query matches when thread_id is null on both row and lookup.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm", thread_id=None),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", thread_id=None, message_count=1),
    ],
)

# ==============================================================================
# Group 6: Peer Tuple Mismatches (14 cases)
# ==============================================================================

# 75. Fallback fails when source does not match.
add_case(
    name="peer_mismatch_source",
    description="Fallback fails when source does not match.",
    owner_profile="default",
    lookup=make_lookup(source="telegram", session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", source="slack", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", message_count=1),
    ],
)

# 76. Fallback fails when user_id does not match.
add_case(
    name="peer_mismatch_user_id",
    description="Fallback fails when user_id does not match.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u2", chat_id="c1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", message_count=1),
    ],
)

# 77. Fallback fails when chat_id does not match.
add_case(
    name="peer_mismatch_chat_id",
    description="Fallback fails when chat_id does not match.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c2", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", message_count=1),
    ],
)

# 78. Fallback fails when chat_type does not match.
add_case(
    name="peer_mismatch_chat_type",
    description="Fallback fails when chat_type does not match.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="group"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", message_count=1),
    ],
)

# 79. Fallback fails when thread_id values differ.
add_case(
    name="peer_mismatch_thread_id_values",
    description="Fallback fails when thread_id values differ.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="group", thread_id="t2"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="group", thread_id="t1", message_count=1),
    ],
)

# 80. Fallback fails when lookup specifies thread_id but row has null thread_id.
add_case(
    name="peer_mismatch_thread_id_lookup_present_row_null",
    description="Fallback fails when lookup specifies thread_id but row has null thread_id.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="group", thread_id="t1"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="group", thread_id=None, message_count=1),
    ],
)

# 81. Fallback fails when row specifies thread_id but lookup has null thread_id.
add_case(
    name="peer_mismatch_thread_id_lookup_null_row_present",
    description="Fallback fails when row specifies thread_id but lookup has null thread_id.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="group", thread_id=None),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="group", thread_id="t1", message_count=1),
    ],
)

# 82. COALESCE treats NULL and empty string as equivalent for user_id.
add_case(
    name="peer_match_user_id_null_and_empty_string",
    description="COALESCE treats NULL and empty string as equivalent for user_id.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="", chat_id="c1", chat_type="group"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id=None, chat_id="c1", chat_type="group", message_count=1),
    ],
)

# 83. Fallback matches when user_id is null on both row and lookup.
add_case(
    name="peer_match_user_id_both_null",
    description="Fallback matches when user_id is null on both row and lookup.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id=None, chat_id="c1", chat_type="group"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id=None, chat_id="c1", chat_type="group", message_count=1),
    ],
)

# 84. COALESCE treats NULL and empty string as equivalent for thread_id.
add_case(
    name="peer_match_thread_id_null_and_empty_string",
    description="COALESCE treats NULL and empty string as equivalent for thread_id.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="group", thread_id=""),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="group", thread_id=None, message_count=1),
    ],
)

# 85. When multiple fallback candidates match, newest activity wins.
add_case(
    name="peer_fallback_selects_newest_activity_among_matching",
    description="When multiple fallback candidates match, newest activity wins.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s_new",
    sessions=[
        make_session("s_old", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=1000.0, message_count=1),
        make_session("s_new", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=2000.0, message_count=1),
    ],
)

# 86. Fallback ranking prefers recent activity over started_at.
add_case(
    name="peer_fallback_activity_beats_started_at",
    description="Fallback ranking prefers recent activity over started_at.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s_early_start",
    sessions=[
        make_session("s_early_start", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", started_at=100.0, last_activity_at=2500.0, message_count=1),
        make_session("s_late_start", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", started_at=1500.0, last_activity_at=1800.0, message_count=1),
    ],
)

# 87. Fallback ranking uses started_at when last_activity_at is null.
add_case(
    name="peer_fallback_falls_back_to_started_at_when_activity_null",
    description="Fallback ranking uses started_at when last_activity_at is null.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s2",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", started_at=1000.0, last_activity_at=None, message_count=1),
        make_session("s2", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", started_at=1200.0, last_activity_at=None, message_count=1),
    ],
)

# 88. started_at of 1100 beats last_activity_at of 900 in COALESCE ranking.
add_case(
    name="peer_fallback_mixed_activity_and_started_at",
    description="started_at of 1100 beats last_activity_at of 900 in COALESCE ranking.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s_started",
    sessions=[
        make_session("s_activity", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", started_at=100.0, last_activity_at=900.0, message_count=1),
        make_session("s_started", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", started_at=1100.0, last_activity_at=None, message_count=1),
    ],
)

# ==============================================================================
# Group 7: Profile-Owner Fencing in Fallback (12 cases)
# ==============================================================================

# 89. Store outside profile tree (owner None) adopts row with null profile.
add_case(
    name="profile_fence_owner_none_adopts_null_profile",
    description="Store outside profile tree (owner None) adopts row with null profile.",
    owner_profile=None,
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name=None, message_count=1),
    ],
)

# 90. Store outside profile tree (owner None) adopts row with named profile.
add_case(
    name="profile_fence_owner_none_adopts_named_profile",
    description="Store outside profile tree (owner None) adopts row with named profile.",
    owner_profile=None,
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name="bot2", message_count=1),
    ],
)

# 91. Store owned by default adopts row with profile_name default.
add_case(
    name="profile_fence_owner_default_adopts_matching_default",
    description="Store owned by default adopts row with profile_name default.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name="default", message_count=1),
    ],
)

# 92. Store owned by default adopts legacy row with null profile_name.
add_case(
    name="profile_fence_owner_default_adopts_null_profile",
    description="Store owned by default adopts legacy row with null profile_name.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name=None, message_count=1),
    ],
)

# 93. Store owned by default rejects sibling profile row.
add_case(
    name="profile_fence_owner_default_rejects_sibling_profile",
    description="Store owned by default rejects sibling profile row.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name="bot2", message_count=1),
    ],
)

# 94. Store owned by bot2 adopts row with profile_name bot2.
add_case(
    name="profile_fence_owner_bot2_adopts_matching_bot2",
    description="Store owned by bot2 adopts row with profile_name bot2.",
    owner_profile="bot2",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name="bot2", message_count=1),
    ],
)

# 95. Store owned by bot2 adopts legacy row with null profile_name.
add_case(
    name="profile_fence_owner_bot2_adopts_null_profile",
    description="Store owned by bot2 adopts legacy row with null profile_name.",
    owner_profile="bot2",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name=None, message_count=1),
    ],
)

# 96. Store owned by bot2 rejects default profile row.
add_case(
    name="profile_fence_owner_bot2_rejects_default_profile",
    description="Store owned by bot2 rejects default profile row.",
    owner_profile="bot2",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name="default", message_count=1),
    ],
)

# 97. Store owned by bot2 rejects third-party sibling profile row.
add_case(
    name="profile_fence_owner_bot2_rejects_bot3_profile",
    description="Store owned by bot2 rejects third-party sibling profile row.",
    owner_profile="bot2",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name="bot3", message_count=1),
    ],
)

# 98. Sibling profile row is newer but fenced out; older own row wins.
add_case(
    name="profile_fence_sibling_newer_own_older_own_wins",
    description="Sibling profile row is newer but fenced out; older own row wins.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s_own",
    sessions=[
        make_session("s_own", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name="default", last_activity_at=1000.0, message_count=1),
        make_session("s_sibling", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name="bot2", last_activity_at=2000.0, message_count=1),
    ],
)

# 99. Sibling profile row is newer but fenced out; older legacy null profile row wins.
add_case(
    name="profile_fence_sibling_newer_legacy_null_older_legacy_wins",
    description="Sibling profile row is newer but fenced out; older legacy null profile row wins.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s_legacy",
    sessions=[
        make_session("s_legacy", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name=None, last_activity_at=1000.0, message_count=1),
        make_session("s_sibling", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name="bot2", last_activity_at=2000.0, message_count=1),
    ],
)

# 100. When only a sibling row exists, fallback fails closed and returns null.
add_case(
    name="profile_fence_only_sibling_exists_fails_closed",
    description="When only a sibling row exists, fallback fails closed and returns null.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s_sibling", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", profile_name="bot2", last_activity_at=2000.0, message_count=1),
    ],
)

# ==============================================================================
# Group 8: Realistic Incident Combinations and Scenarios (12 cases)
# ==============================================================================

# 101. Before peer heal stamps session_key, exact query resolves to zombie predecessor.
add_case(
    name="incident_82616_zombie_beats_unkeyed_before_heal",
    description="Before peer heal stamps session_key, exact query resolves to zombie predecessor.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1", user_id="user1", chat_id="user1", chat_type="dm"),
    expected_session_id="s_zombie",
    sessions=[
        make_session("s_zombie", session_key="agent:main:telegram:dm:user1", started_at=1000.0, last_activity_at=2000.0, message_count=4),
        make_session("s_real", session_key=None, user_id="user1", chat_id="user1", chat_type="dm", started_at=3000.0, last_activity_at=5000.0, message_count=8),
    ],
)

# 102. After peer heal stamps session_key, newer activity on real session beats zombie row.
add_case(
    name="incident_82616_healed_real_session_beats_zombie",
    description="After peer heal stamps session_key, newer activity on real session beats zombie row.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1", user_id="user1", chat_id="user1", chat_type="dm"),
    expected_session_id="s_real",
    sessions=[
        make_session("s_zombie", session_key="agent:main:telegram:dm:user1", started_at=1000.0, last_activity_at=2000.0, message_count=4),
        make_session("s_real", session_key="agent:main:telegram:dm:user1", user_id="user1", chat_id="user1", chat_type="dm", started_at=3000.0, last_activity_at=5000.0, message_count=8),
    ],
)

# 103. session_reset row prevents recovery from resurrecting pre-reset conversation context.
add_case(
    name="incident_68539_reset_row_blocks_cross_conversation_leak",
    description="session_reset row prevents recovery from resurrecting pre-reset conversation context.",
    owner_profile="default",
    lookup=make_lookup(session_key="agent:main:telegram:dm:user1", user_id="user1", chat_id="user1", chat_type="dm"),
    expected_session_id=None,
    sessions=[
        make_session("s_old", session_key="agent:main:telegram:dm:user1", user_id="user1", chat_id="user1", chat_type="dm", last_activity_at=1000.0, message_count=5),
        make_session("s_reset", session_key="agent:main:telegram:dm:user1", user_id="user1", chat_id="user1", chat_type="dm", started_at=1500.0, ended_at=1600.0, end_reason="session_reset", message_count=1),
    ],
)

# 104. Telegram DM with identical user and chat ID isolates sibling bot in fallback recovery.
add_case(
    name="incident_74285_telegram_dm_sibling_bot_isolation",
    description="Telegram DM with identical user and chat ID isolates sibling bot in fallback recovery.",
    owner_profile="bot1",
    lookup=make_lookup(session_key="agent:bot1:telegram:dm:42", user_id="42", chat_id="42", chat_type="dm"),
    expected_session_id="s_bot1",
    sessions=[
        make_session("s_bot2", session_key="agent:bot2:telegram:dm:42", user_id="42", chat_id="42", chat_type="dm", profile_name="bot2", last_activity_at=2000.0, message_count=1),
        make_session("s_bot1", session_key="agent:bot1:telegram:dm:42:old", user_id="42", chat_id="42", chat_type="dm", profile_name="bot1", last_activity_at=1000.0, message_count=1),
    ],
)

# 105. Reconnect after ws_orphan_reap successfully recovers the active conversation.
add_case(
    name="incident_ws_orphan_reap_clean_recovery",
    description="Reconnect after ws_orphan_reap successfully recovers the active conversation.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=1000.0, ended_at=1010.0, end_reason="ws_orphan_reap", message_count=3),
    ],
)

# 106. Boot-time orphan sweep row is recovered cleanly on next user turn.
add_case(
    name="incident_startup_orphan_reap_clean_recovery",
    description="Boot-time orphan sweep row is recovered cleanly on next user turn.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=1000.0, ended_at=1010.0, end_reason="startup_orphan_reap", message_count=3),
    ],
)

# 107. Session closed with superseded_by_resume is recoverable.
add_case(
    name="incident_superseded_by_resume_recovery",
    description="Session closed with superseded_by_resume is recoverable.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=1000.0, ended_at=1010.0, end_reason="superseded_by_resume", message_count=2),
    ],
)

# 108. Older gateway cleanup agent_close bug does not prevent resuming conversation.
add_case(
    name="incident_agent_close_bug_recovery",
    description="Older gateway cleanup agent_close bug does not prevent resuming conversation.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    sessions=[
        make_session("s1", session_key="k1", user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=1000.0, ended_at=1010.0, end_reason="agent_close", message_count=4),
    ],
)

# 109. Reset boundary for chat2 does not fence fallback candidate for chat1.
add_case(
    name="fallback_reset_boundary_preserves_unrelated_chat",
    description="Reset boundary for chat2 does not fence fallback candidate for chat1.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s_chat1",
    sessions=[
        make_session("s_chat1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", last_activity_at=1000.0, message_count=2),
        make_session("b_chat2", session_key=None, user_id="u1", chat_id="c2", chat_type="dm", ended_at=2000.0, end_reason="session_reset", message_count=1),
    ],
)

# 110. Reset boundary on thread2 does not fence fallback candidate on thread1.
add_case(
    name="fallback_reset_boundary_preserves_unrelated_thread",
    description="Reset boundary on thread2 does not fence fallback candidate on thread1.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="group", thread_id="t1"),
    expected_session_id="s_thread1",
    sessions=[
        make_session("s_thread1", session_key=None, user_id="u1", chat_id="c1", chat_type="group", thread_id="t1", last_activity_at=1000.0, message_count=2),
        make_session("b_thread2", session_key=None, user_id="u1", chat_id="c1", chat_type="group", thread_id="t2", ended_at=2000.0, end_reason="session_reset", message_count=1),
    ],
)

# 111. If system_prompt_hash misses in system_prompts table, COALESCE falls back to s.system_prompt.
add_case(
    name="system_prompt_direct_unaffected_when_system_prompts_empty",
    description="If system_prompt_hash misses in system_prompts table, COALESCE falls back to s.system_prompt.",
    owner_profile="default",
    lookup=make_lookup(session_key="k1"),
    expected_session_id="s1",
    system_prompts=[],
    sessions=[
        make_session("s1", session_key="k1", system_prompt="standalone prompt", system_prompt_hash="nonexistent_hash", message_count=1),
    ],
)

# 112. Peer fallback query also joins system_prompts table and resolves system_prompt.
add_case(
    name="fallback_system_prompt_resolved_from_hash",
    description="Peer fallback query also joins system_prompts table and resolves system_prompt.",
    owner_profile="default",
    lookup=make_lookup(session_key="k_miss", user_id="u1", chat_id="c1", chat_type="dm"),
    expected_session_id="s1",
    system_prompts=[
        make_system_prompt("h_fallback", "fallback prompt text"),
    ],
    sessions=[
        make_session("s1", session_key=None, user_id="u1", chat_id="c1", chat_type="dm", system_prompt_hash="h_fallback", message_count=1),
    ],
)

golden_path = root / "rust/tools/session-peer-goldens.json"
text = json.dumps(cases, indent=2) + "\n"

if sys.argv[1:] == ["--check"]:
    assert golden_path.exists(), f"Golden file missing: {golden_path}"
    assert golden_path.read_text() == text, f"Golden file {golden_path} does not match generated output"
elif not sys.argv[1:]:
    golden_path.write_text(text)
else:
    raise SystemExit("usage: gen_session_peer_goldens.py [--check]")

print(f"Verified {len(cases)} session peer recovery decisions")
