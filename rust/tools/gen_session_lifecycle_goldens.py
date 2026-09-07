#!/usr/bin/env python3
"""Generate session lifecycle test goldens using AST-extracted state methods."""
import ast
import json
from pathlib import Path
import sqlite3
import sys
import types
from typing import Any, Dict, List, Optional

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/session-lifecycle-goldens.json"

# 1. Parse hermes_state_common.py
common_tree = ast.parse((ROOT / "hermes_state_common.py").read_text(encoding="utf-8"))
needed_assigns = {
    "_RESET_END_REASONS",
    "_RESET_END_REASONS_SQL",
    "_RECOVERABLE_END_REASONS",
    "_RECOVERABLE_END_REASONS_SQL",
    "SCHEMA_SQL",
}
common_nodes = [
    n
    for n in common_tree.body
    if (isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id in needed_assigns for t in n.targets))
    or (isinstance(n, ast.FunctionDef) and n.name == "_legacy_reset_child_sql")
]
common_module = ast.Module(body=common_nodes, type_ignores=[])
namespace: Dict[str, Any] = {
    "time": types.SimpleNamespace(time=lambda: 100.0),
    "Optional": None,
}
exec(compile(common_module, "hermes_state_common.py", "exec"), namespace)

# 2. Parse hermes_state.py
state_tree = ast.parse((ROOT / "hermes_state.py").read_text(encoding="utf-8"))
target_methods = {
    "end_session",
    "reopen_session",
    "promote_to_session_reset",
    "_bump_conversation_generation",
}
method_nodes = [
    n
    for n in ast.walk(state_tree)
    if isinstance(n, ast.FunctionDef) and n.name in target_methods
]
for fn_node in method_nodes:
    mod = ast.Module(body=[fn_node], type_ignores=[])
    exec(compile(mod, f"hermes_state.py:{fn_node.name}", "exec"), namespace)


class LifecycleHarness:
    """Harness that provides transactional _execute_write and AST methods."""

    def __init__(self, conn: sqlite3.Connection):
        self.conn = conn

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

    end_session = namespace["end_session"]
    reopen_session = namespace["reopen_session"]
    promote_to_session_reset = namespace["promote_to_session_reset"]
    _bump_conversation_generation = namespace["_bump_conversation_generation"]


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


def make_session(
    id: str,
    source: str = "cli",
    session_key: Optional[str] = None,
    parent_session_id: Optional[str] = None,
    started_at: float = 10.0,
    ended_at: Optional[float] = None,
    end_reason: Optional[str] = None,
    model_config: Optional[Any] = None,
) -> Dict[str, Any]:
    return {
        "id": id,
        "source": source,
        "session_key": session_key,
        "parent_session_id": parent_session_id,
        "started_at": started_at,
        "ended_at": ended_at,
        "end_reason": end_reason,
        "model_config": normalize_model_config(model_config),
    }


def make_generation(
    source: str,
    session_key: str,
    generation: int,
) -> Dict[str, Any]:
    return {
        "source": source,
        "session_key": session_key,
        "generation": generation,
    }


def make_op(op: str, **kwargs: Any) -> Dict[str, Any]:
    return {"op": op, "args": kwargs}


def run_case(case_def: Dict[str, Any]) -> List[Dict[str, Any]]:
    conn = sqlite3.connect(":memory:", isolation_level=None)
    conn.row_factory = sqlite3.Row
    conn.executescript(namespace["SCHEMA_SQL"])

    cols = [
        "id",
        "source",
        "session_key",
        "parent_session_id",
        "started_at",
        "ended_at",
        "end_reason",
        "model_config",
    ]
    placeholders = ", ".join("?" for _ in cols)
    for s in case_def.get("initial_sessions", []):
        cfg = s.get("model_config")
        cfg_str = json.dumps(cfg) if isinstance(cfg, (dict, list)) else cfg
        conn.execute(
            f"INSERT INTO sessions ({', '.join(cols)}) VALUES ({placeholders})",
            [
                s["id"],
                s["source"],
                s.get("session_key"),
                s.get("parent_session_id"),
                s["started_at"],
                s.get("ended_at"),
                s.get("end_reason"),
                cfg_str,
            ],
        )

    for g in case_def.get("initial_generations", []):
        conn.execute(
            "INSERT INTO conversation_generations (source, session_key, generation) VALUES (?, ?, ?)",
            (g["source"], g["session_key"], g["generation"]),
        )

    harness = LifecycleHarness(conn)
    steps: List[Dict[str, Any]] = []

    for op_info in case_def.get("operations", []):
        op_name = op_info["op"]
        op_args = op_info.get("args")
        if op_args is None:
            op_args = {k: v for k, v in op_info.items() if k != "op"}

        result = None
        error = None
        try:
            if op_name == "end_session":
                result = harness.end_session(**op_args)
            elif op_name == "promote_to_session_reset":
                result = harness.promote_to_session_reset(**op_args)
            elif op_name == "reopen_session":
                result = harness.reopen_session(**op_args)
            else:
                raise ValueError(f"Unknown operation: {op_name}")
        except Exception as exc:
            error = type(exc).__name__

        session_rows = []
        for r in conn.execute(
            "SELECT id, source, session_key, parent_session_id, started_at, ended_at, end_reason, model_config "
            "FROM sessions ORDER BY id ASC"
        ).fetchall():
            session_rows.append(
                {
                    "id": r["id"],
                    "source": r["source"],
                    "session_key": r["session_key"],
                    "parent_session_id": r["parent_session_id"],
                    "started_at": r["started_at"],
                    "ended_at": r["ended_at"],
                    "end_reason": r["end_reason"],
                    "model_config": normalize_model_config(r["model_config"]),
                }
            )

        generation_rows = []
        for r in conn.execute(
            "SELECT source, session_key, generation FROM conversation_generations "
            "ORDER BY source ASC, session_key ASC"
        ).fetchall():
            generation_rows.append(
                {
                    "source": r["source"],
                    "session_key": r["session_key"],
                    "generation": r["generation"],
                }
            )

        steps.append(
            {
                "op": op_name,
                "args": op_args,
                "result": result,
                "error": error,
                "sessions": session_rows,
                "conversation_generations": generation_rows,
            }
        )

    conn.close()
    return steps


raw_cases: List[Dict[str, Any]] = []


def add_case(
    name: str,
    description: str,
    initial_sessions: List[Dict[str, Any]],
    operations: List[Dict[str, Any]],
    initial_generations: Optional[List[Dict[str, Any]]] = None,
) -> None:
    assert "\u2014" not in description and "\u2013" not in description, (
        f"Prohibited dash in case description: {name}"
    )
    assert "\u2014" not in name and "\u2013" not in name, (
        f"Prohibited dash in case name: {name}"
    )
    normalized_ops = []
    for op in operations:
        op_name = op["op"]
        op_args = op.get("args")
        if op_args is None:
            op_args = {k: v for k, v in op.items() if k != "op"}
        normalized_ops.append({"op": op_name, "args": op_args})

    case_dict = {
        "name": name,
        "description": description,
        "initial_sessions": initial_sessions,
        "initial_generations": initial_generations or [],
        "operations": normalized_ops,
    }
    case_dict["steps"] = run_case(case_dict)
    raw_cases.append(case_dict)


# ==============================================================================
# Group 1: First-End-Wins (5 cases)
# ==============================================================================

add_case(
    name="first_end_wins_compression_then_reset",
    description="Compression ended session ignores subsequent reset attempt.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="compression"),
        make_op("end_session", session_id="s1", end_reason="session_reset"),
    ],
)

add_case(
    name="first_end_wins_reset_then_compression",
    description="Reset ended session ignores subsequent compression end attempt.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="session_reset"),
        make_op("end_session", session_id="s1", end_reason="compression"),
    ],
)

add_case(
    name="first_end_wins_reset_then_reset",
    description="Second reset call no-ops and does not increment generation again.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="session_reset"),
        make_op("end_session", session_id="s1", end_reason="session_reset"),
    ],
)

add_case(
    name="first_end_wins_accidental_then_plain_end",
    description="Plain end_session cannot overwrite an accidental end reason.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="agent_close"),
        make_op("end_session", session_id="s1", end_reason="session_reset"),
    ],
)

add_case(
    name="first_end_wins_user_exit_then_reset",
    description="User exit end reason is preserved against subsequent reset calls.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="user_exit"),
        make_op("end_session", session_id="s1", end_reason="session_reset"),
    ],
)


# ==============================================================================
# Group 2: Accidental-End Promotion vs Explicit-Boundary Preservation (19 cases)
# ==============================================================================

add_case(
    name="promote_live_session_default_reason",
    description="Promoting live session sets session_reset and increments generation.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1"),
    ],
)

add_case(
    name="promote_live_session_reason_idle",
    description="Promoting live session with idle reason sets idle and increments generation.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1", reason="idle"),
    ],
)

add_case(
    name="promote_live_session_reason_daily",
    description="Promoting live session with daily reason sets daily and increments generation.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1", reason="daily"),
    ],
)

add_case(
    name="promote_live_session_reason_suspended",
    description="Promoting live session with suspended reason sets suspended and increments generation.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1", reason="suspended"),
    ],
)

add_case(
    name="promote_live_session_reason_resume_pending_expired",
    description="Promoting live session with resume_pending_expired sets reason and increments generation.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1", reason="resume_pending_expired"),
    ],
)

add_case(
    name="promote_accidental_agent_close",
    description="Promote overwrites accidental agent_close end reason with session_reset.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="agent_close")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1"),
    ],
)

add_case(
    name="promote_accidental_ws_orphan_reap",
    description="Promote overwrites accidental ws_orphan_reap with session_reset.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="ws_orphan_reap")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1"),
    ],
)

add_case(
    name="promote_accidental_superseded_by_resume",
    description="Promote overwrites accidental superseded_by_resume with session_reset.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="superseded_by_resume")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1"),
    ],
)

add_case(
    name="promote_accidental_startup_orphan_reap",
    description="Promote overwrites accidental startup_orphan_reap with daily reset reason.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="startup_orphan_reap")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1", reason="daily"),
    ],
)

add_case(
    name="preserve_explicit_compression",
    description="Explicit compression boundary is preserved against promote.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="compression")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1"),
    ],
)

add_case(
    name="preserve_explicit_session_reset",
    description="Explicit session_reset boundary is preserved against promote with different reason.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="session_reset")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1", reason="idle"),
    ],
)

add_case(
    name="preserve_explicit_session_switch",
    description="Explicit session_switch boundary is preserved against promote.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="session_switch")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1"),
    ],
)

add_case(
    name="preserve_explicit_idle",
    description="Explicit idle boundary is preserved against promote.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="idle")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1"),
    ],
)

add_case(
    name="preserve_explicit_daily",
    description="Explicit daily boundary is preserved against promote.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="daily")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1"),
    ],
)

add_case(
    name="preserve_explicit_suspended",
    description="Explicit suspended boundary is preserved against promote.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="suspended")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1"),
    ],
)

add_case(
    name="preserve_explicit_resume_pending_expired",
    description="Explicit resume_pending_expired boundary is preserved against promote.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="resume_pending_expired")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1"),
    ],
)

add_case(
    name="preserve_other_user_exit",
    description="Non-recoverable user_exit reason is preserved against promote.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="user_exit")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1"),
    ],
)

add_case(
    name="preserve_other_archived",
    description="Non-recoverable archived reason is preserved against promote.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="archived")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1"),
    ],
)

add_case(
    name="preserve_other_branched",
    description="Non-recoverable branched reason is preserved against promote.",
    initial_sessions=[make_session("s1", session_key="k1", ended_at=50.0, end_reason="branched")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1"),
    ],
)


# ==============================================================================
# Group 3: No-Op / Missing Sessions (7 cases)
# ==============================================================================

add_case(
    name="missing_session_end",
    description="Ending nonexistent session is a silent no-op.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="missing_session", end_reason="session_reset"),
    ],
)

add_case(
    name="missing_session_promote",
    description="Promoting nonexistent session returns False without modifications.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("promote_to_session_reset", session_id="missing_session"),
    ],
)

add_case(
    name="missing_session_reopen",
    description="Reopening nonexistent session is a silent no-op.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("reopen_session", session_id="missing_session"),
    ],
)

add_case(
    name="empty_session_id_promote",
    description="Promoting empty session id returns False early.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("promote_to_session_reset", session_id=""),
    ],
)

add_case(
    name="empty_session_id_end",
    description="Ending empty session id affects no rows.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="", end_reason="session_reset"),
    ],
)

add_case(
    name="empty_session_id_reopen",
    description="Reopening empty session id affects no rows.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("reopen_session", session_id=""),
    ],
)

add_case(
    name="reopen_live_session_noop",
    description="Reopening an already live session is safe and leaves it live.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("reopen_session", session_id="s1"),
    ],
)


# ==============================================================================
# Group 4: Generation Increments Only for Real Keyed Reset Boundaries (17 cases)
# ==============================================================================

add_case(
    name="generation_null_session_key",
    description="Unkeyed session reset does not increment conversation generation.",
    initial_sessions=[make_session("s1", session_key=None)],
    operations=[
        make_op("end_session", session_id="s1", end_reason="session_reset"),
    ],
)

add_case(
    name="generation_empty_session_key",
    description="Empty session key string does not increment conversation generation.",
    initial_sessions=[make_session("s1", session_key="")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="session_reset"),
    ],
)

add_case(
    name="generation_whitespace_session_key",
    description="Whitespace-only session key does not increment conversation generation.",
    initial_sessions=[make_session("s1", session_key="   ")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="session_reset"),
    ],
)

add_case(
    name="generation_empty_source",
    description="Empty source string does not increment conversation generation.",
    initial_sessions=[make_session("s1", source="", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="session_reset"),
    ],
)

add_case(
    name="generation_whitespace_source",
    description="Whitespace-only source string does not increment conversation generation.",
    initial_sessions=[make_session("s1", source="  \t  ", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="session_reset"),
    ],
)

add_case(
    name="generation_non_reset_reason_compression",
    description="Compression end reason does not increment conversation generation.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="compression"),
    ],
)

add_case(
    name="generation_non_reset_reason_agent_close",
    description="Accidental agent_close reason does not increment conversation generation.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="agent_close"),
    ],
)

add_case(
    name="generation_non_reset_reason_user_exit",
    description="User exit reason does not increment conversation generation.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="user_exit"),
    ],
)

add_case(
    name="generation_increments_reason_session_reset",
    description="Keyed session_reset reason increments conversation generation to 1.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="session_reset"),
    ],
)

add_case(
    name="generation_increments_reason_session_switch",
    description="Keyed session_switch reason increments conversation generation to 1.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="session_switch"),
    ],
)

add_case(
    name="generation_increments_reason_idle",
    description="Keyed idle reset reason increments conversation generation to 1.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="idle"),
    ],
)

add_case(
    name="generation_increments_reason_daily",
    description="Keyed daily reset reason increments conversation generation to 1.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="daily"),
    ],
)

add_case(
    name="generation_increments_reason_suspended",
    description="Keyed suspended reset reason increments conversation generation to 1.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="suspended"),
    ],
)

add_case(
    name="generation_increments_reason_resume_pending_expired",
    description="Keyed resume_pending_expired increments conversation generation to 1.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="resume_pending_expired"),
    ],
)

add_case(
    name="generation_increments_existing_counter",
    description="Reset increment updates existing conversation generation count from 4 to 5.",
    initial_sessions=[make_session("s1", source="cli", session_key="k1")],
    initial_generations=[make_generation("cli", "k1", 4)],
    operations=[
        make_op("end_session", session_id="s1", end_reason="session_reset"),
    ],
)

add_case(
    name="generation_independent_by_source_and_key",
    description="Generations are tracked independently per source and session_key pair.",
    initial_sessions=[
        make_session("s1", source="cli", session_key="k1"),
        make_session("s2", source="slack", session_key="k1"),
        make_session("s3", source="cli", session_key="k2"),
    ],
    operations=[
        make_op("end_session", session_id="s1", end_reason="session_reset"),
        make_op("end_session", session_id="s2", end_reason="session_reset"),
        make_op("end_session", session_id="s3", end_reason="session_reset"),
    ],
)

add_case(
    name="generation_promote_unregistered_reason_no_increment",
    description="Promote with unregistered reason succeeds on live row but does not bump generation.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1", reason="unregistered_custom"),
    ],
)


# ==============================================================================
# Group 5: Repeated Resets After Reopen (3 cases)
# ==============================================================================

add_case(
    name="repeated_end_reopen_cycle",
    description="Session ended, reopened, and re-ended increments generation on each reset.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="session_reset"),
        make_op("reopen_session", session_id="s1"),
        make_op("end_session", session_id="s1", end_reason="session_reset"),
        make_op("reopen_session", session_id="s1"),
        make_op("end_session", session_id="s1", end_reason="session_reset"),
    ],
)

add_case(
    name="repeated_promote_reopen_cycle",
    description="Repeated promote and reopen cycles advance generation sequentially.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("promote_to_session_reset", session_id="s1", reason="daily"),
        make_op("reopen_session", session_id="s1"),
        make_op("promote_to_session_reset", session_id="s1", reason="idle"),
        make_op("reopen_session", session_id="s1"),
        make_op("promote_to_session_reset", session_id="s1", reason="session_reset"),
    ],
)

add_case(
    name="repeated_mixed_end_promote_cycle",
    description="Mixed cycle with accidental close does not bump until promoted.",
    initial_sessions=[make_session("s1", session_key="k1")],
    operations=[
        make_op("end_session", session_id="s1", end_reason="session_reset"),
        make_op("reopen_session", session_id="s1"),
        make_op("end_session", session_id="s1", end_reason="agent_close"),
        make_op("promote_to_session_reset", session_id="s1", reason="suspended"),
        make_op("reopen_session", session_id="s1"),
        make_op("end_session", session_id="s1", end_reason="session_switch"),
    ],
)


# ==============================================================================
# Group 6: Legacy Child Marker Stamping (16 cases)
# ==============================================================================

add_case(
    name="legacy_child_stamped_same_key_null_config",
    description="Legacy child with same key and null config receives _reset_from marker on parent reopen.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="session_reset"),
        make_session("c1", session_key="k1", parent_session_id="p1", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="legacy_child_stamped_same_key_existing_config",
    description="Legacy child preserves existing JSON config keys when _reset_from marker is added.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="session_reset"),
        make_session(
            "c1",
            session_key="k1",
            parent_session_id="p1",
            model_config={"temperature": 0.5, "tags": ["quick"]},
        ),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="legacy_child_not_stamped_different_key",
    description="Child with different session_key is not stamped on parent reopen.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="session_reset"),
        make_session("c1", session_key="k2", parent_session_id="p1", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="legacy_child_not_stamped_child_key_empty",
    description="Child with empty session_key is not stamped on parent reopen.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="session_reset"),
        make_session("c1", session_key="", parent_session_id="p1", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="legacy_child_not_stamped_child_key_null",
    description="Child with null session_key is not stamped on parent reopen.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="session_reset"),
        make_session("c1", session_key=None, parent_session_id="p1", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="legacy_child_not_stamped_parent_key_empty",
    description="Empty session_key on parent and child does not satisfy child.session_key != empty.",
    initial_sessions=[
        make_session("p1", session_key="", ended_at=50.0, end_reason="session_reset"),
        make_session("c1", session_key="", parent_session_id="p1", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="legacy_child_not_stamped_parent_key_null",
    description="Null session_key on parent does not match child key.",
    initial_sessions=[
        make_session("p1", session_key=None, ended_at=50.0, end_reason="session_reset"),
        make_session("c1", session_key=None, parent_session_id="p1", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="existing_marker_preserved",
    description="Existing _reset_from marker in child model_config is not overwritten.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="session_reset"),
        make_session(
            "c1",
            session_key="k1",
            parent_session_id="p1",
            model_config={"_reset_from": "prior_parent", "extra": 42},
        ),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="legacy_child_not_stamped_parent_compression",
    description="Child is not stamped when parent ended with compression instead of reset.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="compression"),
        make_session("c1", session_key="k1", parent_session_id="p1", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="legacy_child_not_stamped_parent_agent_close",
    description="Child is not stamped when parent ended with accidental agent_close.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="agent_close"),
        make_session("c1", session_key="k1", parent_session_id="p1", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="legacy_child_stamped_parent_reason_session_switch",
    description="Child is stamped when parent ended with session_switch.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="session_switch"),
        make_session("c1", session_key="k1", parent_session_id="p1", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="legacy_child_stamped_parent_reason_idle",
    description="Child is stamped when parent ended with idle.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="idle"),
        make_session("c1", session_key="k1", parent_session_id="p1", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="legacy_child_stamped_parent_reason_daily",
    description="Child is stamped when parent ended with daily.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="daily"),
        make_session("c1", session_key="k1", parent_session_id="p1", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="legacy_child_stamped_parent_reason_suspended",
    description="Child is stamped when parent ended with suspended.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="suspended"),
        make_session("c1", session_key="k1", parent_session_id="p1", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="legacy_child_stamped_parent_reason_resume_pending_expired",
    description="Child is stamped when parent ended with resume_pending_expired.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="resume_pending_expired"),
        make_session("c1", session_key="k1", parent_session_id="p1", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="multiple_children_mixed_filtering",
    description="Only matching unmarked child with identical key is stamped among mixed children.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="session_reset"),
        make_session("c1", session_key="k1", parent_session_id="p1", model_config=None),
        make_session("c2", session_key="k2", parent_session_id="p1", model_config=None),
        make_session(
            "c3",
            session_key="k1",
            parent_session_id="p1",
            model_config={"_reset_from": "old_p"},
        ),
        make_session("c4", session_key="", parent_session_id="p1", model_config=None),
        make_session("c5", session_key=None, parent_session_id="p1", model_config=None),
        make_session("c6", session_key="k1", parent_session_id="p2", model_config=None),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)


# ==============================================================================
# Group 7: Malformed JSON Rollback (3 cases)
# ==============================================================================

add_case(
    name="malformed_json_rollback_single_child",
    description="Malformed JSON in matching child model_config rolls back parent reopen.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="session_reset"),
        make_session(
            "c1",
            session_key="k1",
            parent_session_id="p1",
            model_config="{not valid json",
        ),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="malformed_json_rollback_atomicity_multiple_children",
    description="Transaction rollback ensures valid sibling child is not partially stamped.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="session_reset"),
        make_session("c1", session_key="k1", parent_session_id="p1", model_config=None),
        make_session(
            "c2",
            session_key="k1",
            parent_session_id="p1",
            model_config="[unterminated array",
        ),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
    ],
)

add_case(
    name="malformed_json_unrelated_session_not_blocking",
    description="Malformed JSON in an unrelated session does not block reopening parent.",
    initial_sessions=[
        make_session("p1", session_key="k1", ended_at=50.0, end_reason="session_reset"),
        make_session("c1", session_key="k1", parent_session_id="p1", model_config=None),
        make_session(
            "c_other",
            session_key="k_other",
            parent_session_id="p_other",
            model_config="{broken json",
        ),
    ],
    operations=[
        make_op("reopen_session", session_id="p1"),
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
            raise SystemExit("Session lifecycle fixtures differ from Python generated fixtures")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_session_lifecycle_goldens.py [--check]")
    else:
        OUT.write_text(content, encoding="utf-8")
    print(f"Verified {len(raw_cases)} session lifecycle cases")
