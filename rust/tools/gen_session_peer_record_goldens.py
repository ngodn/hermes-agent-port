#!/usr/bin/env python3
"""Generate session peer record test goldens using AST-extracted state methods."""
import ast
import json
from pathlib import Path
import sqlite3
import sys
import types
from typing import Any, Dict, List, Optional

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/session-peer-record-goldens.json"

# 1. Parse hermes_state_common.py for SCHEMA_SQL
common_tree = ast.parse((ROOT / "hermes_state_common.py").read_text(encoding="utf-8"))
needed_assigns = {"SCHEMA_SQL"}
common_nodes = [
    n
    for n in common_tree.body
    if isinstance(n, ast.Assign)
    and any(isinstance(t, ast.Name) and t.id in needed_assigns for t in n.targets)
]
common_module = ast.Module(body=common_nodes, type_ignores=[])
namespace: Dict[str, Any] = {
    "time": types.SimpleNamespace(time=lambda: 100.0),
    "Optional": None,
}
exec(compile(common_module, "hermes_state_common.py", "exec"), namespace)

# 2. Parse hermes_state.py for target methods
state_tree = ast.parse((ROOT / "hermes_state.py").read_text(encoding="utf-8"))
target_methods = {
    "record_gateway_session_peer",
    "set_expiry_finalized",
}
method_nodes = [
    n
    for n in ast.walk(state_tree)
    if isinstance(n, ast.FunctionDef) and n.name in target_methods
]
for fn_node in method_nodes:
    mod = ast.Module(body=[fn_node], type_ignores=[])
    exec(compile(mod, f"hermes_state.py:{fn_node.name}", "exec"), namespace)


class PeerRecordHarness:
    """Harness that provides transactional _execute_write and AST methods."""

    def __init__(self, conn: sqlite3.Connection, owner_profile: Optional[str] = None):
        self.conn = conn
        self.owner_profile = owner_profile

    def _own_profile_name(self) -> Optional[str]:
        return self.owner_profile

    def _execute_write(self, fn, *args, **kwargs):
        self.conn.execute("BEGIN IMMEDIATE")
        try:
            res = fn(self.conn)
            self.conn.execute("COMMIT")
            return res
        except BaseException:
            try:
                self.conn.execute("ROLLBACK")
            except Exception:
                pass
            raise

    record_gateway_session_peer = namespace["record_gateway_session_peer"]
    set_expiry_finalized = namespace["set_expiry_finalized"]


def normalize_model_config(raw: Any) -> Any:
    """Normalize model_config by parsing valid JSON so formatting is non-semantic."""
    if raw is None:
        return None
    if isinstance(raw, (dict, list)):
        return raw
    if isinstance(raw, str):
        try:
            return json.loads(raw)
        except Exception:
            return raw
    return raw


SESSION_COLS = [
    "id",
    "source",
    "user_id",
    "session_key",
    "chat_id",
    "chat_type",
    "thread_id",
    "display_name",
    "origin_json",
    "expiry_finalized",
    "parent_session_id",
    "started_at",
    "ended_at",
    "end_reason",
    "model_config",
    "profile_name",
]


def make_session(
    id: str,
    source: str = "telegram",
    user_id: Optional[str] = None,
    session_key: Optional[str] = None,
    chat_id: Optional[str] = None,
    chat_type: Optional[str] = None,
    thread_id: Optional[str] = None,
    display_name: Optional[str] = None,
    origin_json: Optional[str] = None,
    expiry_finalized: int = 0,
    parent_session_id: Optional[str] = None,
    started_at: float = 10.0,
    ended_at: Optional[float] = None,
    end_reason: Optional[str] = None,
    model_config: Optional[Any] = None,
    profile_name: Optional[str] = None,
) -> Dict[str, Any]:
    return {
        "id": id,
        "source": source,
        "user_id": user_id,
        "session_key": session_key,
        "chat_id": chat_id,
        "chat_type": chat_type,
        "thread_id": thread_id,
        "display_name": display_name,
        "origin_json": origin_json,
        "expiry_finalized": expiry_finalized,
        "parent_session_id": parent_session_id,
        "started_at": started_at,
        "ended_at": ended_at,
        "end_reason": end_reason,
        "model_config": normalize_model_config(model_config),
        "profile_name": profile_name,
    }


def make_op(op: str, **kwargs: Any) -> Dict[str, Any]:
    return {"op": op, "args": kwargs}


def run_case(case_def: Dict[str, Any]) -> List[Dict[str, Any]]:
    conn = sqlite3.connect(":memory:", isolation_level=None)
    conn.row_factory = sqlite3.Row
    conn.executescript(namespace["SCHEMA_SQL"])

    placeholders = ", ".join("?" for _ in SESSION_COLS)
    for s in case_def.get("initial_sessions", []):
        cfg = s.get("model_config")
        cfg_str = json.dumps(cfg) if isinstance(cfg, (dict, list)) else cfg
        conn.execute(
            f"INSERT INTO sessions ({', '.join(SESSION_COLS)}) VALUES ({placeholders})",
            [
                s["id"],
                s.get("source", "telegram"),
                s.get("user_id"),
                s.get("session_key"),
                s.get("chat_id"),
                s.get("chat_type"),
                s.get("thread_id"),
                s.get("display_name"),
                s.get("origin_json"),
                s.get("expiry_finalized", 0),
                s.get("parent_session_id"),
                s.get("started_at", 10.0),
                s.get("ended_at"),
                s.get("end_reason"),
                cfg_str,
                s.get("profile_name"),
            ],
        )

    harness = PeerRecordHarness(conn, owner_profile=case_def.get("owner_profile"))
    steps: List[Dict[str, Any]] = []

    for op_info in case_def.get("operations", []):
        op_name = op_info["op"]
        op_args = op_info.get("args")
        if op_args is None:
            op_args = {k: v for k, v in op_info.items() if k != "op"}

        result = None
        error = None
        try:
            if op_name == "record_gateway_session_peer":
                result = harness.record_gateway_session_peer(**op_args)
            elif op_name == "set_expiry_finalized":
                result = harness.set_expiry_finalized(**op_args)
            else:
                raise ValueError(f"Unknown operation: {op_name}")
        except Exception as exc:
            error = type(exc).__name__

        session_rows = []
        for r in conn.execute(
            f"SELECT {', '.join(SESSION_COLS)} FROM sessions ORDER BY id ASC"
        ).fetchall():
            session_rows.append(
                {
                    "id": r["id"],
                    "source": r["source"],
                    "user_id": r["user_id"],
                    "session_key": r["session_key"],
                    "chat_id": r["chat_id"],
                    "chat_type": r["chat_type"],
                    "thread_id": r["thread_id"],
                    "display_name": r["display_name"],
                    "origin_json": r["origin_json"],
                    "expiry_finalized": r["expiry_finalized"],
                    "parent_session_id": r["parent_session_id"],
                    "started_at": r["started_at"],
                    "ended_at": r["ended_at"],
                    "end_reason": r["end_reason"],
                    "model_config": normalize_model_config(r["model_config"]),
                    "profile_name": r["profile_name"],
                }
            )

        steps.append(
            {
                "op": op_name,
                "args": op_args,
                "result": result,
                "error": error,
                "sessions": session_rows,
            }
        )

    conn.close()
    return steps


raw_cases: List[Dict[str, Any]] = []


def add_case(
    name: str,
    description: str,
    operations: List[Dict[str, Any]],
    initial_sessions: Optional[List[Dict[str, Any]]] = None,
    owner_profile: Optional[str] = None,
) -> None:
    assert "\u2014" not in description and "\u2013" not in description, (
        f"Prohibited dash in case description: {name}"
    )
    assert "\u2014" not in name and "\u2013" not in name, (
        f"Prohibited dash in case name: {name}"
    )
    case_def = {
        "name": name,
        "description": description,
        "owner_profile": owner_profile,
        "initial_sessions": initial_sessions or [],
        "operations": operations,
    }
    steps = run_case(case_def)
    case_def["steps"] = steps
    raw_cases.append(case_def)


# ==============================================================================
# Group 1: No-op Empty IDs and Keys (9 cases)
# ==============================================================================

add_case(
    name="noop_empty_session_id_on_existing_session",
    description="Empty session_id early returns without modifying existing session.",
    initial_sessions=[
        make_session("s1", session_key="k1", source="telegram"),
    ],
    operations=[
        make_op("record_gateway_session_peer", session_id="", session_key="k2", source="slack"),
    ],
)

add_case(
    name="noop_none_session_id_on_existing_session",
    description="None session_id early returns without modifying existing session.",
    initial_sessions=[
        make_session("s1", session_key="k1", source="telegram"),
    ],
    operations=[
        make_op("record_gateway_session_peer", session_id=None, session_key="k2", source="slack"),
    ],
)

add_case(
    name="noop_empty_session_key_on_existing_session",
    description="Empty session_key early returns without modifying existing session.",
    initial_sessions=[
        make_session("s1", session_key="k1", source="telegram"),
    ],
    operations=[
        make_op("record_gateway_session_peer", session_id="s1", session_key="", source="slack"),
    ],
)

add_case(
    name="noop_none_session_key_on_existing_session",
    description="None session_key early returns without modifying existing session.",
    initial_sessions=[
        make_session("s1", session_key="k1", source="telegram"),
    ],
    operations=[
        make_op("record_gateway_session_peer", session_id="s1", session_key=None, source="slack"),
    ],
)

add_case(
    name="noop_both_empty_on_existing_session",
    description="Empty session_id and empty session_key early returns without modifications.",
    initial_sessions=[
        make_session("s1", session_key="k1", source="telegram"),
    ],
    operations=[
        make_op("record_gateway_session_peer", session_id="", session_key="", source="slack"),
    ],
)

add_case(
    name="noop_empty_session_id_missing_row",
    description="Empty session_id does not trigger self-healing insert on missing row.",
    initial_sessions=[],
    owner_profile="default",
    operations=[
        make_op("record_gateway_session_peer", session_id="", session_key="k1", source="telegram"),
    ],
)

add_case(
    name="noop_empty_session_key_missing_row",
    description="Empty session_key does not trigger self-healing insert on missing row.",
    initial_sessions=[],
    owner_profile="default",
    operations=[
        make_op("record_gateway_session_peer", session_id="s_missing", session_key="", source="telegram"),
    ],
)

add_case(
    name="noop_empty_session_id_set_expiry_finalized",
    description="Empty session_id in set_expiry_finalized early returns.",
    initial_sessions=[
        make_session("s1", expiry_finalized=0),
    ],
    operations=[
        make_op("set_expiry_finalized", session_id="", finalized=True),
    ],
)

add_case(
    name="noop_none_session_id_set_expiry_finalized",
    description="None session_id in set_expiry_finalized early returns.",
    initial_sessions=[
        make_session("s1", expiry_finalized=0),
    ],
    operations=[
        make_op("set_expiry_finalized", session_id=None, finalized=True),
    ],
)


# ==============================================================================
# Group 2: Ordinary Identity Refresh (6 cases)
# ==============================================================================

add_case(
    name="refresh_full_identity_single_row",
    description="Refresh updates all routing identity fields on existing session row.",
    initial_sessions=[
        make_session(
            "s1",
            source="telegram",
            user_id="u0",
            session_key="k0",
            chat_id="c0",
            chat_type="dm",
            thread_id=None,
            display_name="Old Name",
            origin_json='{"team": "old"}',
        ),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="slack",
            user_id="u1",
            session_key="k1",
            chat_id="c1",
            chat_type="channel",
            thread_id="t1",
            display_name="New Name",
            origin_json='{"team": "new"}',
        ),
    ],
)

add_case(
    name="refresh_clear_optional_fields_to_none",
    description="Explicit None overwrites nullable routing fields to null.",
    initial_sessions=[
        make_session(
            "s1",
            source="telegram",
            user_id="u1",
            session_key="k1",
            chat_id="c1",
            chat_type="group",
            thread_id="t1",
        ),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="telegram",
            session_key="k1",
            user_id=None,
            chat_id=None,
            chat_type=None,
            thread_id=None,
        ),
    ],
)

add_case(
    name="refresh_leaves_unrelated_session_intact",
    description="Peer update on target session does not alter other sessions.",
    initial_sessions=[
        make_session("s1", session_key="k1", user_id="u1"),
        make_session("s2", session_key="k2", user_id="u2"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="telegram",
            session_key="k1_updated",
            user_id="u1_updated",
        ),
    ],
)

add_case(
    name="refresh_preserves_non_routing_columns",
    description="Peer update preserves model, model_config, timestamps, and ownership.",
    initial_sessions=[
        make_session(
            "s1",
            source="telegram",
            session_key="k1",
            parent_session_id="p0",
            started_at=42.0,
            ended_at=55.0,
            end_reason="compression",
            model_config={"temp": 0.5, "model": "gemini"},
            profile_name="prof_a",
        ),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="discord",
            session_key="k1_new",
            user_id="u_new",
        ),
    ],
)

add_case(
    name="refresh_sequential_two_updates",
    description="Sequential peer updates record incremental routing evolution.",
    initial_sessions=[
        make_session("s1", source="telegram", session_key="k1", chat_id="c0"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="telegram",
            session_key="k1",
            chat_id="c1",
            display_name="Step 1",
        ),
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="slack",
            session_key="k2",
            chat_id="c2",
            thread_id="t2",
            display_name="Step 2",
        ),
    ],
)

add_case(
    name="refresh_empty_source_string_persisted",
    description="Empty string source is accepted and stored by SQLite update.",
    initial_sessions=[
        make_session("s1", source="telegram", session_key="k1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="",
            session_key="k1",
        ),
    ],
)


# ==============================================================================
# Group 3: Null-vs-Empty Display and Origin Updates (9 cases)
# ==============================================================================

add_case(
    name="display_null_leaves_existing_display_untouched",
    description="Passing display_name=None preserves existing display_name via COALESCE.",
    initial_sessions=[
        make_session("s1", session_key="k1", display_name="Existing Display"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="telegram",
            session_key="k1",
            display_name=None,
        ),
    ],
)

add_case(
    name="display_empty_overwrites_existing_display",
    description="Passing empty display_name overwrites existing string with empty string.",
    initial_sessions=[
        make_session("s1", session_key="k1", display_name="Existing Display"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="telegram",
            session_key="k1",
            display_name="",
        ),
    ],
)

add_case(
    name="display_value_overwrites_existing_display",
    description="Passing new display_name string replaces existing display_name.",
    initial_sessions=[
        make_session("s1", session_key="k1", display_name="Existing Display"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="telegram",
            session_key="k1",
            display_name="Updated Display",
        ),
    ],
)

add_case(
    name="origin_null_leaves_existing_origin_untouched",
    description="Passing origin_json=None preserves existing origin_json via COALESCE.",
    initial_sessions=[
        make_session("s1", session_key="k1", origin_json='{"team": "T1"}'),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="telegram",
            session_key="k1",
            origin_json=None,
        ),
    ],
)

add_case(
    name="origin_empty_overwrites_existing_origin",
    description="Passing empty origin_json overwrites existing JSON with empty string.",
    initial_sessions=[
        make_session("s1", session_key="k1", origin_json='{"team": "T1"}'),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="telegram",
            session_key="k1",
            origin_json="",
        ),
    ],
)

add_case(
    name="origin_value_overwrites_existing_origin",
    description="Passing new origin_json string replaces existing origin_json.",
    initial_sessions=[
        make_session("s1", session_key="k1", origin_json='{"team": "T1"}'),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="telegram",
            session_key="k1",
            origin_json='{"team": "T2"}',
        ),
    ],
)

add_case(
    name="display_and_origin_null_on_initial_null",
    description="Passing None for null display and origin leaves both as null.",
    initial_sessions=[
        make_session("s1", session_key="k1", display_name=None, origin_json=None),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="telegram",
            session_key="k1",
            display_name=None,
            origin_json=None,
        ),
    ],
)

add_case(
    name="display_and_origin_empty_from_initial_null",
    description="Passing empty strings for null display and origin updates both to empty.",
    initial_sessions=[
        make_session("s1", session_key="k1", display_name=None, origin_json=None),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="telegram",
            session_key="k1",
            display_name="",
            origin_json="",
        ),
    ],
)

add_case(
    name="display_and_origin_mixed_null_and_empty",
    description="None display preserves existing while empty origin overwrites existing.",
    initial_sessions=[
        make_session("s1", session_key="k1", display_name="Preserved", origin_json='{"k": 1}'),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="telegram",
            session_key="k1",
            display_name=None,
            origin_json="",
        ),
    ],
)


# ==============================================================================
# Group 4: Missing-Row Self-Heal and Profile Stamping (7 cases)
# ==============================================================================

add_case(
    name="self_heal_stamps_owner_profile_and_started_at",
    description="Self-healing insert populates profile_name from harness and time=100.",
    initial_sessions=[],
    owner_profile="prod_profile",
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s_heal",
            source="telegram",
            session_key="k1",
            user_id="u1",
        ),
    ],
)

add_case(
    name="self_heal_stamps_none_owner_profile",
    description="Self-healing insert leaves profile_name null when owner_profile is None.",
    initial_sessions=[],
    owner_profile=None,
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s_heal",
            source="telegram",
            session_key="k1",
        ),
    ],
)

add_case(
    name="self_heal_stamps_empty_owner_profile",
    description="Self-healing insert stores empty string profile_name when owner is empty.",
    initial_sessions=[],
    owner_profile="",
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s_heal",
            source="telegram",
            session_key="k1",
        ),
    ],
)

add_case(
    name="self_heal_populates_all_identity_fields",
    description="Self-healing insert populates all supplied routing identity columns.",
    initial_sessions=[],
    owner_profile="dev_profile",
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s_heal",
            source="discord",
            user_id="u1",
            session_key="k1",
            chat_id="c1",
            chat_type="guild",
            thread_id="t1",
            display_name="Guild Channel",
            origin_json='{"guild": "123"}',
        ),
    ],
)

add_case(
    name="self_heal_with_none_display_and_origin",
    description="Self-healing insert handles None display_name and origin_json.",
    initial_sessions=[],
    owner_profile="dev_profile",
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s_heal",
            source="telegram",
            session_key="k1",
            display_name=None,
            origin_json=None,
        ),
    ],
)

add_case(
    name="self_heal_skipped_when_compression_ancestors_true",
    description="Self-healing insert does not run when include_compression_ancestors is True.",
    initial_sessions=[],
    owner_profile="dev_profile",
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s_heal",
            source="telegram",
            session_key="k1",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="self_heal_does_not_overwrite_existing_row_profile",
    description="Existing row update does not overwrite original profile_name or started_at.",
    initial_sessions=[
        make_session(
            "s1",
            source="telegram",
            session_key="k1",
            started_at=10.0,
            profile_name="original_profile",
        ),
    ],
    owner_profile="new_profile",
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="telegram",
            session_key="k_updated",
        ),
    ],
)


# ==============================================================================
# Group 5: Optional Compression Ancestors (8 cases)
# ==============================================================================

add_case(
    name="compression_ancestor_false_updates_only_target",
    description="include_compression_ancestors=False updates child but leaves compression parent.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason="compression"),
        make_session("c1", session_key="k_old", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=False,
        ),
    ],
)

add_case(
    name="compression_ancestor_true_updates_parent",
    description="include_compression_ancestors=True updates both child and compression parent.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason="compression"),
        make_session("c1", session_key="k_old", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="compression_ancestor_two_level_chain",
    description="Lineage traversal updates two consecutive compression ancestors.",
    initial_sessions=[
        make_session("p0", session_key="k_old", end_reason="compression"),
        make_session("p1", session_key="k_old", parent_session_id="p0", end_reason="compression"),
        make_session("c1", session_key="k_old", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="compression_ancestor_three_level_chain",
    description="Lineage traversal updates three consecutive compression ancestors.",
    initial_sessions=[
        make_session("p0", session_key="k_old", end_reason="compression"),
        make_session("p1", session_key="k_old", parent_session_id="p0", end_reason="compression"),
        make_session("p2", session_key="k_old", parent_session_id="p1", end_reason="compression"),
        make_session("c1", session_key="k_old", parent_session_id="p2"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="compression_ancestor_preserves_display_coalesce_per_row",
    description="Lineage update with None display preserves distinct display names across rows.",
    initial_sessions=[
        make_session("p1", display_name="Parent Display", end_reason="compression"),
        make_session("c1", display_name="Child Display", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            display_name=None,
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="compression_ancestor_empty_display_overwrites_all_in_lineage",
    description="Lineage update with empty display overwrites display across child and parent.",
    initial_sessions=[
        make_session("p1", display_name="Parent Display", end_reason="compression"),
        make_session("c1", display_name="Child Display", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            display_name="",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="compression_ancestor_new_display_overwrites_all_in_lineage",
    description="Lineage update with string display unifies display name across lineage.",
    initial_sessions=[
        make_session("p1", display_name="Parent Display", end_reason="compression"),
        make_session("c1", display_name="Child Display", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            display_name="Unified Display",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="compression_ancestor_leaves_sibling_session_intact",
    description="Lineage traversal does not affect sibling sessions of target child.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason="compression"),
        make_session("c1", session_key="k_old", parent_session_id="p1"),
        make_session("c2", session_key="k_sibling", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)


# ==============================================================================
# Group 6: Stops at Boundaries (12 cases)
# ==============================================================================

add_case(
    name="boundary_stop_parent_end_reason_ended",
    description="Lineage traversal stops when parent end_reason is ended.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason="ended"),
        make_session("c1", session_key="k_old", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="boundary_stop_parent_end_reason_session_reset",
    description="Lineage traversal stops when parent end_reason is session_reset.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason="session_reset"),
        make_session("c1", session_key="k_old", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="boundary_stop_parent_end_reason_none",
    description="Lineage traversal stops when parent end_reason is null.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason=None),
        make_session("c1", session_key="k_old", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="boundary_stop_parent_end_reason_timeout",
    description="Lineage traversal stops when parent end_reason is timeout.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason="timeout"),
        make_session("c1", session_key="k_old", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="boundary_stop_intermediate_non_compression",
    description="Non-compression intermediate parent blocks traversal to grandparent.",
    initial_sessions=[
        make_session("p0", session_key="k_old", end_reason="compression"),
        make_session("p1", session_key="k_old", parent_session_id="p0", end_reason="ended"),
        make_session("c1", session_key="k_old", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="boundary_stop_child_branched_from",
    description="Child with _branched_from marker stops traversal to compression parent.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason="compression"),
        make_session(
            "c1",
            session_key="k_old",
            parent_session_id="p1",
            model_config={"_branched_from": "s_other"},
        ),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="boundary_stop_child_delegate_from",
    description="Child with _delegate_from marker stops traversal to compression parent.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason="compression"),
        make_session(
            "c1",
            session_key="k_old",
            parent_session_id="p1",
            model_config={"_delegate_from": "s_agent"},
        ),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="boundary_stop_child_source_tool",
    description="Child with source=tool stops traversal to compression parent.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason="compression"),
        make_session("c1", source="tool", session_key="k_old", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="boundary_stop_parent_branched_from_stops_grandparent",
    description="Parent with _branched_from stops traversal before reaching grandparent.",
    initial_sessions=[
        make_session("p0", session_key="k_old", end_reason="compression"),
        make_session(
            "p1",
            session_key="k_old",
            parent_session_id="p0",
            end_reason="compression",
            model_config={"_branched_from": "fork_session"},
        ),
        make_session("c1", session_key="k_old", parent_session_id="p1", model_config={}),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="boundary_stop_parent_delegate_from_stops_grandparent",
    description="Parent with _delegate_from stops traversal before reaching grandparent.",
    initial_sessions=[
        make_session("p0", session_key="k_old", end_reason="compression"),
        make_session(
            "p1",
            session_key="k_old",
            parent_session_id="p0",
            end_reason="compression",
            model_config={"_delegate_from": "delegate_agent"},
        ),
        make_session("c1", session_key="k_old", parent_session_id="p1", model_config={}),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="boundary_stop_parent_tool_source_stops_grandparent",
    description="Parent with source=tool stops traversal before reaching grandparent.",
    initial_sessions=[
        make_session("p0", source="slack", session_key="k_old", end_reason="compression"),
        make_session(
            "p1",
            source="tool",
            session_key="k_old",
            parent_session_id="p0",
            end_reason="compression",
        ),
        make_session("c1", source="slack", session_key="k_old", parent_session_id="p1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="telegram",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="boundary_null_branch_and_delegate_values_do_not_stop",
    description="Explicit null values for branch and delegate do not stop lineage traversal.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason="compression"),
        make_session(
            "c1",
            session_key="k_old",
            parent_session_id="p1",
            model_config={"_branched_from": None, "_delegate_from": None},
        ),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)


# ==============================================================================
# Group 7: Cyclic Lineage Deduplication (4 cases)
# ==============================================================================

add_case(
    name="cyclic_lineage_two_nodes",
    description="Mutual cycle between two compression nodes deduplicates cleanly.",
    initial_sessions=[
        make_session("s1", session_key="k_old", parent_session_id="s2", end_reason="compression"),
        make_session("s2", session_key="k_old", parent_session_id="s1", end_reason="compression"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="cyclic_lineage_self_reference",
    description="Self-referencing parent pointer deduplicates cleanly without loop.",
    initial_sessions=[
        make_session("s1", session_key="k_old", parent_session_id="s1", end_reason="compression"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="cyclic_lineage_three_node_cycle",
    description="Three-node circular compression cycle deduplicates cleanly.",
    initial_sessions=[
        make_session("s1", session_key="k_old", parent_session_id="s2", end_reason="compression"),
        make_session("s2", session_key="k_old", parent_session_id="s3", end_reason="compression"),
        make_session("s3", session_key="k_old", parent_session_id="s1", end_reason="compression"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="cyclic_lineage_cycle_with_tail_child",
    description="Child pointing into cyclic ancestor loop updates child and loop nodes.",
    initial_sessions=[
        make_session("r1", session_key="k_old", parent_session_id="r2", end_reason="compression"),
        make_session("r2", session_key="k_old", parent_session_id="r1", end_reason="compression"),
        make_session("c1", session_key="k_old", parent_session_id="r1"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)


# ==============================================================================
# Group 8: Malformed Child JSON Rollback (4 cases)
# ==============================================================================

add_case(
    name="malformed_child_json_rolls_back_ancestor_update",
    description="Malformed child model_config rolls back entire lineage update transaction.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason="compression"),
        make_session("c1", session_key="k_old", parent_session_id="p1", model_config="{bad_json"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="malformed_child_json_succeeds_when_ancestors_false",
    description="Non-lineage update succeeds on session with malformed model_config.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason="compression"),
        make_session("c1", session_key="k_old", parent_session_id="p1", model_config="{bad_json"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=False,
        ),
    ],
)

add_case(
    name="malformed_intermediate_json_rolls_back",
    description="Malformed JSON in intermediate ancestor rolls back entire transaction.",
    initial_sessions=[
        make_session("p0", session_key="k_old", end_reason="compression"),
        make_session(
            "p1",
            session_key="k_old",
            parent_session_id="p0",
            end_reason="compression",
            model_config="[unterminated array",
        ),
        make_session("c1", session_key="k_old", parent_session_id="p1", model_config={}),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)

add_case(
    name="malformed_unrelated_session_json_ignored",
    description="Malformed JSON in unrelated session does not block valid lineage update.",
    initial_sessions=[
        make_session("p1", session_key="k_old", end_reason="compression"),
        make_session("c1", session_key="k_old", parent_session_id="p1", model_config={}),
        make_session("s_other", session_key="k_other", model_config="{invalid_json"),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="c1",
            source="slack",
            session_key="k_new",
            include_compression_ancestors=True,
        ),
    ],
)


# ==============================================================================
# Group 9: Expiry Flag Writes (6 cases)
# ==============================================================================

add_case(
    name="expiry_set_finalized_true_default",
    description="Default finalized argument sets expiry_finalized flag to 1.",
    initial_sessions=[
        make_session("s1", expiry_finalized=0),
    ],
    operations=[
        make_op("set_expiry_finalized", session_id="s1"),
    ],
)

add_case(
    name="expiry_set_finalized_true_explicit",
    description="Explicit finalized=True sets expiry_finalized flag to 1.",
    initial_sessions=[
        make_session("s1", expiry_finalized=0),
    ],
    operations=[
        make_op("set_expiry_finalized", session_id="s1", finalized=True),
    ],
)

add_case(
    name="expiry_set_finalized_false",
    description="finalized=False clears expiry_finalized flag to 0.",
    initial_sessions=[
        make_session("s1", expiry_finalized=1),
    ],
    operations=[
        make_op("set_expiry_finalized", session_id="s1", finalized=False),
    ],
)

add_case(
    name="expiry_toggle_sequence",
    description="Sequential expiry calls toggle expiry_finalized between 1 and 0.",
    initial_sessions=[
        make_session("s1", expiry_finalized=0),
    ],
    operations=[
        make_op("set_expiry_finalized", session_id="s1", finalized=True),
        make_op("set_expiry_finalized", session_id="s1", finalized=False),
        make_op("set_expiry_finalized", session_id="s1", finalized=True),
    ],
)

add_case(
    name="expiry_missing_session_noop",
    description="Calling set_expiry_finalized on missing session is a safe no-op.",
    initial_sessions=[],
    operations=[
        make_op("set_expiry_finalized", session_id="s_missing", finalized=True),
    ],
)

add_case(
    name="expiry_combined_with_peer_record",
    description="Sequential peer record and expiry finalization update row coherently.",
    initial_sessions=[
        make_session("s1", source="telegram", session_key="k1", expiry_finalized=0),
    ],
    operations=[
        make_op(
            "record_gateway_session_peer",
            session_id="s1",
            source="slack",
            session_key="k2",
            chat_id="c2",
        ),
        make_op("set_expiry_finalized", session_id="s1", finalized=True),
    ],
)


def generate() -> str:
    serialized = json.dumps(raw_cases, indent=2) + "\n"
    assert "\u2014" not in serialized and "\u2013" not in serialized, (
        "Generated golden JSON contains prohibited em or en dash"
    )
    return serialized


if __name__ == "__main__":
    script_source = Path(__file__).read_text(encoding="utf-8")
    assert "\u2014" not in script_source and "\u2013" not in script_source, (
        "Script source contains prohibited em or en dash"
    )
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if not OUT.exists():
            raise SystemExit(f"Golden file {OUT} does not exist")
        if OUT.read_text(encoding="utf-8") != content:
            raise SystemExit("Session peer record fixtures differ from Python generated fixtures")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_session_peer_record_goldens.py [--check]")
    else:
        OUT.write_text(content, encoding="utf-8")
    print(f"Verified {len(raw_cases)} session peer record cases")
