#!/usr/bin/env python3
"""Generate session creation test goldens using AST-extracted state methods."""
import ast
import hashlib
import json
from pathlib import Path
import sqlite3
import sys
import types
import typing
from typing import Any, Dict, List, Optional

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/session-create-goldens.json"

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
namespace: Dict[str, Any] = dict(
    vars(typing),
    time=types.SimpleNamespace(time=lambda: 100.0),
    hashlib=hashlib,
    json=json,
)
exec(compile(common_module, "hermes_state_common.py", "exec"), namespace)

# 2. Parse hermes_state.py for SQL constants, prompt helpers, and session create methods
state_tree = ast.parse((ROOT / "hermes_state.py").read_text(encoding="utf-8"))
assign_nodes = [
    n
    for n in state_tree.body
    if isinstance(n, ast.Assign)
    and any(isinstance(t, ast.Name) and t.id == "_SAME_KEY_NAMESPACE_SQL" for t in n.targets)
]
exec(compile(assign_nodes and ast.Module(body=assign_nodes, type_ignores=[]) or ast.Module(body=[], type_ignores=[]), "hermes_state.py:assigns", "exec"), namespace)

target_methods = {
    "_system_prompt_hash",
    "_store_system_prompt",
    "_delete_unreferenced_system_prompts",
    "_insert_session_row",
    "create_session",
}
method_nodes = [
    n
    for n in ast.walk(state_tree)
    if isinstance(n, ast.FunctionDef) and n.name in target_methods
]
for fn_node in method_nodes:
    mod = ast.Module(body=[fn_node], type_ignores=[])
    exec(compile(mod, f"hermes_state.py:{fn_node.name}", "exec"), namespace)


class CreateHarness:
    """Harness that provides transactional _execute_write and AST methods."""

    def __init__(self, conn: sqlite3.Connection, owner_profile: Optional[str] = None):
        self.conn = conn
        self.owner_profile = owner_profile
        self._TRANSCRIPT_WRITE_PATIENCE_S = 30.0

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

    _store_system_prompt = namespace["_store_system_prompt"]
    _delete_unreferenced_system_prompts = namespace["_delete_unreferenced_system_prompts"]
    _insert_session_row = namespace["_insert_session_row"]
    create_session = namespace["create_session"]


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
    "model",
    "model_config",
    "system_prompt",
    "system_prompt_hash",
    "parent_session_id",
    "started_at",
    "ended_at",
    "end_reason",
    "cwd",
    "git_branch",
    "git_repo_root",
    "profile_name",
    "expiry_finalized",
]


def make_session(
    id: str,
    source: str = "cli",
    user_id: Optional[str] = None,
    session_key: Optional[str] = None,
    chat_id: Optional[str] = None,
    chat_type: Optional[str] = None,
    thread_id: Optional[str] = None,
    display_name: Optional[str] = None,
    origin_json: Optional[str] = None,
    model: Optional[str] = None,
    model_config: Optional[Any] = None,
    system_prompt: Optional[str] = None,
    system_prompt_hash: Optional[str] = None,
    parent_session_id: Optional[str] = None,
    started_at: float = 10.0,
    ended_at: Optional[float] = None,
    end_reason: Optional[str] = None,
    cwd: Optional[str] = None,
    git_branch: Optional[str] = None,
    git_repo_root: Optional[str] = None,
    profile_name: Optional[str] = None,
    expiry_finalized: int = 0,
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
        "model": model,
        "model_config": normalize_model_config(model_config),
        "system_prompt": system_prompt,
        "system_prompt_hash": system_prompt_hash,
        "parent_session_id": parent_session_id,
        "started_at": started_at,
        "ended_at": ended_at,
        "end_reason": end_reason,
        "cwd": cwd,
        "git_branch": git_branch,
        "git_repo_root": git_repo_root,
        "profile_name": profile_name,
        "expiry_finalized": expiry_finalized,
    }


def make_prompt(hash: str, prompt: str) -> Dict[str, str]:
    return {"hash": hash, "prompt": prompt}


def make_op(op: str = "create_session", **kwargs: Any) -> Dict[str, Any]:
    return {"op": op, "args": kwargs}


def run_case(case_def: Dict[str, Any]) -> List[Dict[str, Any]]:
    conn = sqlite3.connect(":memory:", isolation_level=None)
    conn.row_factory = sqlite3.Row
    conn.executescript(namespace["SCHEMA_SQL"])

    for p in case_def.get("initial_prompts", []):
        conn.execute(
            "INSERT INTO system_prompts (hash, prompt) VALUES (?, ?)",
            (p["hash"], p["prompt"]),
        )

    cols = SESSION_COLS
    placeholders = ", ".join("?" for _ in cols)
    for s in case_def.get("initial_sessions", []):
        cfg = s.get("model_config")
        cfg_str = json.dumps(cfg) if isinstance(cfg, (dict, list)) else cfg
        conn.execute(
            f"INSERT INTO sessions ({', '.join(cols)}) VALUES ({placeholders})",
            [
                s["id"],
                s.get("source", "cli"),
                s.get("user_id"),
                s.get("session_key"),
                s.get("chat_id"),
                s.get("chat_type"),
                s.get("thread_id"),
                s.get("display_name"),
                s.get("origin_json"),
                s.get("model"),
                cfg_str,
                s.get("system_prompt"),
                s.get("system_prompt_hash"),
                s.get("parent_session_id"),
                s.get("started_at", 10.0),
                s.get("ended_at"),
                s.get("end_reason"),
                s.get("cwd"),
                s.get("git_branch"),
                s.get("git_repo_root"),
                s.get("profile_name"),
                s.get("expiry_finalized", 0),
            ],
        )

    harness = CreateHarness(conn, owner_profile=case_def.get("owner_profile"))
    steps: List[Dict[str, Any]] = []

    for op_info in case_def.get("operations", []):
        op_name = op_info["op"]
        op_args = op_info.get("args")
        if op_args is None:
            op_args = {k: v for k, v in op_info.items() if k != "op"}

        result = None
        error = None
        try:
            if op_name == "create_session":
                result = harness.create_session(**op_args)
            elif op_name == "_insert_session_row":
                result = harness._insert_session_row(**op_args)
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
                    "model": r["model"],
                    "model_config": normalize_model_config(r["model_config"]),
                    "system_prompt": r["system_prompt"],
                    "system_prompt_hash": r["system_prompt_hash"],
                    "parent_session_id": r["parent_session_id"],
                    "started_at": r["started_at"],
                    "ended_at": r["ended_at"],
                    "end_reason": r["end_reason"],
                    "cwd": r["cwd"],
                    "git_branch": r["git_branch"],
                    "git_repo_root": r["git_repo_root"],
                    "profile_name": r["profile_name"],
                    "expiry_finalized": r["expiry_finalized"],
                }
            )

        prompt_rows = []
        for r in conn.execute(
            "SELECT hash, prompt FROM system_prompts ORDER BY hash ASC"
        ).fetchall():
            prompt_rows.append(
                {
                    "hash": r["hash"],
                    "prompt": r["prompt"],
                }
            )

        steps.append(
            {
                "op": op_name,
                "args": op_args,
                "result": result,
                "error": error,
                "sessions": session_rows,
                "system_prompts": prompt_rows,
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
    initial_prompts: Optional[List[Dict[str, Any]]] = None,
    owner_profile: Optional[str] = None,
) -> None:
    case = {
        "name": name,
        "description": description,
        "owner_profile": owner_profile,
        "initial_sessions": initial_sessions or [],
        "initial_prompts": initial_prompts or [],
        "operations": operations,
    }
    case["steps"] = run_case(case)
    raw_cases.append(case)


# =============================================================================
# Category 1: Basic creation and explicit empty/null values
# =============================================================================

add_case(
    name="create_bare_session",
    description="Create a bare session with minimal arguments; verify clock100 timestamp and null fields.",
    operations=[
        make_op("create_session", session_id="s_bare", source="cli"),
    ],
)

add_case(
    name="insert_session_row_direct",
    description="Call _insert_session_row directly; verify it returns None while persisting the row.",
    operations=[
        make_op("_insert_session_row", session_id="s_row", source="cli", model="claude-3"),
    ],
)

add_case(
    name="create_session_with_full_metadata",
    description="Create session with all explicit metadata columns populated.",
    operations=[
        make_op(
            "create_session",
            session_id="s_full",
            source="slack",
            user_id="u1",
            session_key="agent:main:slack:dm:C1",
            chat_id="C1",
            chat_type="dm",
            thread_id="T1",
            display_name="Main Channel",
            origin_json='{"team": "T1"}',
            model="gpt-4o",
            cwd="/home/user/proj",
            git_repo_root="/home/user/proj",
            profile_name="main",
        ),
    ],
)

add_case(
    name="create_session_explicit_empty_strings",
    description="Explicit empty strings for cwd, display_name, origin_json, model are preserved as non-null empty strings.",
    operations=[
        make_op(
            "create_session",
            session_id="s_empty_str",
            source="slack",
            cwd="",
            display_name="",
            origin_json="",
            model="",
        ),
    ],
)

add_case(
    name="create_session_whitespace_profile_name_falls_back_to_owner",
    description="Whitespace profile name falls back to injected store owner.",
    owner_profile="work",
    operations=[
        make_op(
            "create_session",
            session_id="s_ws_profile",
            source="cli",
            profile_name="   ",
        ),
    ],
)

add_case(
    name="create_session_empty_profile_name_no_owner",
    description="Empty profile name with no store owner sets profile_name to None.",
    owner_profile=None,
    operations=[
        make_op(
            "create_session",
            session_id="s_no_owner",
            source="cli",
            profile_name="",
        ),
    ],
)

add_case(
    name="create_session_explicit_empty_dict_model_config",
    description="Explicit empty dict for model_config evaluates falsy and stores None.",
    operations=[
        make_op(
            "create_session",
            session_id="s_empty_cfg",
            source="cli",
            model_config={},
        ),
    ],
)

add_case(
    name="create_session_explicit_empty_system_prompt",
    description="Explicit empty system prompt hashes empty string and stores in system_prompts table.",
    operations=[
        make_op(
            "create_session",
            session_id="s_empty_prompt",
            source="cli",
            system_prompt="",
        ),
    ],
)


# =============================================================================
# Category 2: Enrichment of existing sessions
# =============================================================================

add_case(
    name="enrich_bare_session_with_model_and_config",
    description="Enrich bare session with model and model_config on conflict.",
    operations=[
        make_op("create_session", session_id="s_enrich", source="gateway", user_id="u_gateway"),
        make_op(
            "create_session",
            session_id="s_enrich",
            source="agent",
            model="claude-3-5",
            model_config={"temperature": 0.5},
        ),
    ],
)

add_case(
    name="enrich_preserves_existing_model_on_conflict",
    description="Enrichment preserves existing model on conflict.",
    operations=[
        make_op("create_session", session_id="s_model", source="cli", model="existing_model"),
        make_op("create_session", session_id="s_model", source="cli", model="new_model"),
    ],
)

add_case(
    name="enrich_does_not_overwrite_source_or_user_id",
    description="Enrichment preserves immutable source, user_id, and started_at.",
    initial_sessions=[
        make_session("s_immut", source="web", user_id="u_orig", started_at=25.0),
    ],
    operations=[
        make_op("create_session", session_id="s_immut", source="cli", user_id="u_new"),
    ],
)

add_case(
    name="enrich_fills_null_peer_and_origin_fields",
    description="Enrichment fills null peer and origin fields on existing row.",
    operations=[
        make_op("create_session", session_id="s_peer", source="slack"),
        make_op(
            "create_session",
            session_id="s_peer",
            source="slack",
            chat_id="C99",
            chat_type="channel",
            thread_id="T88",
            display_name="General",
            origin_json='{"team":"T99"}',
        ),
    ],
)

add_case(
    name="enrich_preserves_existing_peer_and_origin_fields",
    description="Enrichment preserves existing peer and origin fields on existing row.",
    operations=[
        make_op(
            "create_session",
            session_id="s_peer_exist",
            source="slack",
            chat_id="C1",
            chat_type="dm",
            thread_id="T1",
            display_name="D1",
            origin_json='{"team":"T1"}',
        ),
        make_op(
            "create_session",
            session_id="s_peer_exist",
            source="slack",
            chat_id="C2",
            chat_type="group",
            thread_id="T2",
            display_name="D2",
            origin_json='{"team":"T2"}',
        ),
    ],
)

add_case(
    name="enrich_fills_null_cwd_and_git_repo_root",
    description="Enrichment fills null cwd and git_repo_root on existing row.",
    operations=[
        make_op("create_session", session_id="s_cwd", source="cli"),
        make_op(
            "create_session",
            session_id="s_cwd",
            source="cli",
            cwd="/workspace/repo",
            git_repo_root="/workspace/repo",
        ),
    ],
)

add_case(
    name="enrich_preserves_existing_cwd_and_git_repo_root",
    description="Enrichment preserves existing cwd and git_repo_root.",
    operations=[
        make_op(
            "create_session",
            session_id="s_cwd_exist",
            source="cli",
            cwd="/orig/path",
            git_repo_root="/orig/repo",
        ),
        make_op(
            "create_session",
            session_id="s_cwd_exist",
            source="cli",
            cwd="/new/path",
            git_repo_root="/new/repo",
        ),
    ],
)

add_case(
    name="enrich_multistep_progressive",
    description="Three sequential calls progressively enrich bare session to fully populated row.",
    operations=[
        make_op("create_session", session_id="s_multi", source="gateway"),
        make_op("create_session", session_id="s_multi", source="agent", model="model_1"),
        make_op(
            "create_session",
            session_id="s_multi",
            source="agent",
            model_config={"temp": 0.2},
            system_prompt="instructions",
        ),
    ],
)


# =============================================================================
# Category 3: Reset-only config merging and preservation
# =============================================================================

add_case(
    name="reset_only_config_merged_with_real_config",
    description="Reset-only config with _reset_from marker merges with incoming real model config.",
    initial_sessions=[
        make_session("s_reset", model_config={"_reset_from": "parent_123"}),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="s_reset",
            source="cli",
            model_config={"temperature": 0.7, "max_tokens": 500},
        ),
    ],
)

add_case(
    name="reset_only_config_with_extra_keys_not_merged",
    description="Config with _reset_from and additional keys is not treated as reset-only stub.",
    initial_sessions=[
        make_session("s_reset_extra", model_config={"_reset_from": "p", "temperature": 0.1}),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="s_reset_extra",
            source="cli",
            model_config={"temperature": 0.9, "max_tokens": 100},
        ),
    ],
)

add_case(
    name="reset_only_config_second_call_has_no_config",
    description="Reset stub remains unchanged when second call passes no model config.",
    initial_sessions=[
        make_session("s_reset_none", model_config={"_reset_from": "p"}),
    ],
    operations=[
        make_op("create_session", session_id="s_reset_none", source="cli", model="new_model"),
    ],
)

add_case(
    name="reset_only_config_overwrites_reset_from_in_excluded",
    description="Reset stub overwrites _reset_from in incoming excluded config.",
    initial_sessions=[
        make_session("s_reset_override", model_config={"_reset_from": "orig_parent"}),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="s_reset_override",
            source="cli",
            model_config={"_reset_from": "incoming_parent", "temperature": 0.5},
        ),
    ],
)

add_case(
    name="reset_only_config_nested_object_enrichment",
    description="Reset stub merges with complex nested object in excluded config.",
    initial_sessions=[
        make_session("s_reset_nested", model_config={"_reset_from": "p"}),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="s_reset_nested",
            source="cli",
            model_config={"provider_options": {"stream": True}},
        ),
    ],
)

add_case(
    name="reset_only_config_multiple_enrichments_idempotent",
    description="Multiple enrichments on reset stub: first merges, second keeps merged config.",
    initial_sessions=[
        make_session("s_reset_idem", model_config={"_reset_from": "p"}),
    ],
    operations=[
        make_op("create_session", session_id="s_reset_idem", source="cli", model_config={"temp": 0.5}),
        make_op("create_session", session_id="s_reset_idem", source="cli", model_config={"temp": 0.9}),
    ],
)

add_case(
    name="reset_only_config_on_null_initial_config",
    description="Incoming config with _reset_from on null initial config is inserted directly.",
    initial_sessions=[
        make_session("s_reset_null", model_config=None),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="s_reset_null",
            source="cli",
            model_config={"_reset_from": "p"},
        ),
    ],
)


# =============================================================================
# Category 4: System prompt caching, GC, and transaction rollback
# =============================================================================

add_case(
    name="prompt_stored_and_deduplicated",
    description="Sessions with identical system prompt share deduplicated row in system_prompts.",
    operations=[
        make_op("create_session", session_id="s_p1", source="cli", system_prompt="shared prompt"),
        make_op("create_session", session_id="s_p2", source="cli", system_prompt="shared prompt"),
    ],
)

add_case(
    name="prompt_gc_unreferenced_prompt_deleted",
    description="Creation with system prompt cleans up unreferenced orphan prompts.",
    initial_prompts=[
        make_prompt("orphan_hash", "orphan prompt"),
    ],
    operations=[
        make_op("create_session", session_id="s_gc", source="cli", system_prompt="active prompt"),
    ],
)

add_case(
    name="prompt_gc_not_triggered_when_system_prompt_none",
    description="Creation without system prompt does not trigger unreferenced prompt cleanup.",
    initial_prompts=[
        make_prompt("orphan_hash", "orphan prompt"),
    ],
    operations=[
        make_op("create_session", session_id="s_nogc", source="cli", system_prompt=None),
    ],
)

add_case(
    name="prompt_enrichment_fills_null_prompt",
    description="Enriching null prompt on existing session stores hash and prompt.",
    operations=[
        make_op("create_session", session_id="s_enrich_p", source="cli", system_prompt=None),
        make_op("create_session", session_id="s_enrich_p", source="cli", system_prompt="enriched prompt"),
    ],
)

add_case(
    name="prompt_enrichment_does_not_overwrite_existing_prompt",
    description="Enrichment does not overwrite existing prompt; second prompt is GC-ed.",
    operations=[
        make_op("create_session", session_id="s_keep_p", source="cli", system_prompt="prompt 1"),
        make_op("create_session", session_id="s_keep_p", source="cli", system_prompt="prompt 2"),
    ],
)

add_case(
    name="prompt_legacy_text_cleared_when_hash_enriched",
    description="Enriching prompt on legacy session with system_prompt text clears text and sets hash.",
    initial_sessions=[
        make_session("s_legacy_p", system_prompt="old legacy text", system_prompt_hash=None),
    ],
    operations=[
        make_op("create_session", session_id="s_legacy_p", source="cli", system_prompt="new prompt"),
    ],
)

add_case(
    name="prompt_rollback_on_malformed_model_config",
    description="Malformed JSON in session model_config causes transaction rollback; orphan prompt is not stored.",
    initial_sessions=[
        make_session("s_broken", model_config="broken_json_string"),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="s_broken",
            source="cli",
            system_prompt="rollback_prompt",
            model_config={"temp": 0.5},
        ),
    ],
)

add_case(
    name="prompt_rollback_preserves_initial_prompts",
    description="Transaction rollback preserves pre-existing valid system prompts.",
    initial_sessions=[
        make_session("s_broken2", model_config="invalid"),
    ],
    initial_prompts=[
        make_prompt("good_hash", "valid prompt"),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="s_broken2",
            source="cli",
            system_prompt="bad_prompt",
            model_config={"temp": 0.5},
        ),
    ],
)


# =============================================================================
# Category 5: Context inheritance from parent (CWD, git_repo_root, git_branch, profile)
# =============================================================================

add_case(
    name="parent_context_inheritance_cwd_repo_branch",
    description="Child session inherits cwd, git_repo_root, git_branch, and profile_name from parent.",
    initial_sessions=[
        make_session(
            "p_ctx",
            cwd="/repo/dir",
            git_repo_root="/repo",
            git_branch="feature-1",
            profile_name="team",
        ),
    ],
    operations=[
        make_op("create_session", session_id="c_ctx", source="cli", parent_session_id="p_ctx"),
    ],
)

add_case(
    name="parent_context_does_not_overwrite_child_explicit_values",
    description="Child session retains its explicit cwd and git_repo_root over parent values.",
    initial_sessions=[
        make_session("p_ctx2", cwd="/parent/dir", git_repo_root="/parent/repo"),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="c_ctx2",
            source="cli",
            parent_session_id="p_ctx2",
            cwd="/child/dir",
            git_repo_root="/child/repo",
        ),
    ],
)

add_case(
    name="parent_context_child_explicit_empty_string_not_overwritten",
    description="Child session with explicit empty string for cwd does not inherit parent cwd.",
    initial_sessions=[
        make_session("p_ctx3", cwd="/parent/dir"),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="c_ctx3",
            source="cli",
            parent_session_id="p_ctx3",
            cwd="",
        ),
    ],
)

add_case(
    name="parent_context_missing_parent",
    description="Child session with nonexistent parent session id creates safely without error.",
    operations=[
        make_op("create_session", session_id="c_missing_p", source="cli", parent_session_id="nonexistent_parent"),
    ],
)

add_case(
    name="parent_context_partial_parent_nulls",
    description="Child inherits only non-null parent context fields when some parent fields are null.",
    initial_sessions=[
        make_session("p_partial", cwd="/parent/dir", git_repo_root=None, git_branch=None),
    ],
    operations=[
        make_op("create_session", session_id="c_partial", source="cli", parent_session_id="p_partial"),
    ],
)

add_case(
    name="parent_context_linear_chain_inheritance",
    description="Context inheritance propagates through linear parent-child-grandchild chain.",
    operations=[
        make_op("create_session", session_id="s_root", source="cli", cwd="/root", profile_name="prof"),
        make_op("create_session", session_id="s_child", source="cli", parent_session_id="s_root"),
        make_op("create_session", session_id="s_grandchild", source="cli", parent_session_id="s_child"),
    ],
)

add_case(
    name="parent_context_parent_has_different_source",
    description="Child inherits parent context across different source types.",
    initial_sessions=[
        make_session("p_slack", source="slack", cwd="/slack/repo", profile_name="prof1"),
    ],
    operations=[
        make_op("create_session", session_id="c_subagent", source="subagent", parent_session_id="p_slack"),
    ],
)


# =============================================================================
# Category 6: Profile inheritance across namespaces
# =============================================================================

add_case(
    name="profile_inheritance_same_namespace",
    description="Child inherits profile when session key namespaces match.",
    initial_sessions=[
        make_session("p_ns1", session_key="agent:work:slack:dm:C1", profile_name="work"),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="c_ns1",
            source="slack",
            session_key="agent:work:telegram:group:G1",
            parent_session_id="p_ns1",
        ),
    ],
)

add_case(
    name="profile_inheritance_different_namespace_fenced",
    description="Profile inheritance is fenced when session key namespaces differ.",
    initial_sessions=[
        make_session("p_ns2", session_key="agent:work:slack:dm:C1", profile_name="work"),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="c_ns2",
            source="slack",
            session_key="agent:personal:slack:dm:C1",
            parent_session_id="p_ns2",
        ),
    ],
)

add_case(
    name="profile_inheritance_parent_keyless",
    description="Child inherits profile when parent is keyless.",
    initial_sessions=[
        make_session("p_nokey", session_key=None, profile_name="work"),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="c_from_nokey",
            source="slack",
            session_key="agent:work:slack:dm:C1",
            parent_session_id="p_nokey",
        ),
    ],
)

add_case(
    name="profile_inheritance_child_keyless",
    description="Child inherits profile when child is keyless.",
    initial_sessions=[
        make_session("p_haskey", session_key="agent:work:slack:dm:C1", profile_name="work"),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="c_nokey",
            source="subagent",
            session_key=None,
            parent_session_id="p_haskey",
        ),
    ],
)

add_case(
    name="profile_inheritance_both_keyless",
    description="Child inherits profile when both parent and child are keyless.",
    initial_sessions=[
        make_session("p_both_nokey", session_key=None, profile_name="work"),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="c_both_nokey",
            source="cli",
            session_key=None,
            parent_session_id="p_both_nokey",
        ),
    ],
)

add_case(
    name="profile_inheritance_child_has_explicit_profile",
    description="Child with explicit profile_name does not inherit parent profile.",
    initial_sessions=[
        make_session("p_prof", session_key="agent:work:slack:dm:C1", profile_name="work"),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="c_prof",
            source="slack",
            session_key="agent:work:slack:dm:C2",
            parent_session_id="p_prof",
            profile_name="override",
        ),
    ],
)

add_case(
    name="profile_inheritance_with_injected_store_owner",
    description="Injected store owner populates child profile; coalesce preserves store owner.",
    owner_profile="default",
    initial_sessions=[
        make_session("p_store", session_key="agent:work:slack:dm:C1", profile_name="work"),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="c_store",
            source="slack",
            session_key="agent:work:slack:dm:C2",
            parent_session_id="p_store",
        ),
    ],
)


# =============================================================================
# Category 7: Compression vs Live-Parent routing inheritance
# =============================================================================

add_case(
    name="live_parent_delegate_does_not_inherit_routing",
    description="Live parent delegate does not inherit routing fields.",
    initial_sessions=[
        make_session(
            "p_live",
            source="slack",
            session_key="agent:work:slack:dm:C",
            user_id="U1",
            chat_id="C1",
            chat_type="dm",
            thread_id="T1",
            display_name="Live Chat",
            origin_json='{"team":"T1"}',
            cwd="/work",
            profile_name="work",
            ended_at=None,
            end_reason=None,
        ),
    ],
    operations=[
        make_op("create_session", session_id="c_live_delegate", source="subagent", parent_session_id="p_live"),
    ],
)

add_case(
    name="compression_fork_inherits_full_routing",
    description="Compression fork inherits all routing and origin fields from compressed parent.",
    initial_sessions=[
        make_session(
            "p_comp",
            source="slack",
            session_key="agent:work:slack:dm:C",
            user_id="U1",
            chat_id="C1",
            chat_type="dm",
            thread_id="T1",
            display_name="Comp Chat",
            origin_json='{"team":"T1"}',
            cwd="/work",
            profile_name="work",
            ended_at=50.0,
            end_reason="compression",
        ),
    ],
    operations=[
        make_op("create_session", session_id="c_comp_fork", source="slack", parent_session_id="p_comp"),
    ],
)

add_case(
    name="compression_fork_child_explicit_routing_not_overwritten",
    description="Compression fork preserves child explicit routing fields over parent.",
    initial_sessions=[
        make_session(
            "p_comp2",
            session_key="p_key",
            user_id="p_user",
            chat_id="p_chat",
            ended_at=50.0,
            end_reason="compression",
        ),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="c_comp2",
            source="slack",
            parent_session_id="p_comp2",
            session_key="c_key",
            user_id="c_user",
        ),
    ],
)

add_case(
    name="other_ended_parent_does_not_inherit_routing",
    description="Parent ended with user_exit does not pass routing fields to child.",
    initial_sessions=[
        make_session("p_exit", session_key="p_key", user_id="p_user", ended_at=50.0, end_reason="user_exit"),
    ],
    operations=[
        make_op("create_session", session_id="c_from_exit", source="slack", parent_session_id="p_exit"),
    ],
)

add_case(
    name="reset_ended_parent_does_not_inherit_routing",
    description="Parent ended with reset does not pass routing fields to child.",
    initial_sessions=[
        make_session("p_reset_end", session_key="p_key", user_id="p_user", ended_at=50.0, end_reason="reset"),
    ],
    operations=[
        make_op("create_session", session_id="c_from_reset", source="slack", parent_session_id="p_reset_end"),
    ],
)

add_case(
    name="compression_fork_with_partial_parent_routing",
    description="Compression fork with partial nulls on parent routing fields.",
    initial_sessions=[
        make_session("p_comp_part", user_id="U1", session_key="K1", chat_id=None, thread_id=None, ended_at=50.0, end_reason="compression"),
    ],
    operations=[
        make_op("create_session", session_id="c_comp_part", source="slack", parent_session_id="p_comp_part"),
    ],
)

add_case(
    name="compression_fork_explicit_empty_string_not_overwritten",
    description="Compression fork does not overwrite child explicit empty strings for origin fields.",
    initial_sessions=[
        make_session(
            "p_comp_empty",
            display_name="Parent Display",
            origin_json='{"p": 1}',
            ended_at=50.0,
            end_reason="compression",
        ),
    ],
    operations=[
        make_op(
            "create_session",
            session_id="c_comp_empty",
            source="slack",
            parent_session_id="p_comp_empty",
            display_name="",
            origin_json="",
        ),
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
            raise SystemExit("Session create fixtures differ from Python generated fixtures")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_session_create_goldens.py [--check]")
    else:
        OUT.write_text(content, encoding="utf-8")
    print(f"Verified {len(raw_cases)} session create cases")
