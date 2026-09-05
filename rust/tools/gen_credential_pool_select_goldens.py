#!/usr/bin/env python3
"""Differential oracle for the CredentialPool API-key selection core.

Executes the REAL PooledCredential and CredentialPool classes from
agent/credential_pool.py (AST-extracted, with only I/O boundaries stubbed:
time, persist, strategy, logger) against a matrix of API-key pool states, and
records the observable selection behavior. The Rust port in credential_pool.rs
must reproduce these exactly.

Only the API-key path is exercised (provider "openai-api"): the OAuth
sync/refresh branches in _available_entries are guarded by provider ==
anthropic/nous/openai-codex/xai-oauth and never run here, matching the Rust
slice that defers them.

Usage: python rust/tools/gen_credential_pool_select_goldens.py [--check]
"""
import ast
import json
import sys
import types
import threading
from dataclasses import dataclass, field, fields, replace
from pathlib import Path
from typing import Any, Dict, List, Optional, Set, Tuple

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/credential-pool-select-goldens.json"
SRC = (ROOT / "agent/credential_pool.py").read_text()
tree = ast.parse(SRC)

NOW = 1_700_000_000.0

# Names to lift straight from the source module.
FUNCS = {
    "_normalize_pool_auth_type", "_parse_absolute_timestamp", "_exhausted_ttl",
    "_exhausted_until", "_is_manual_source",
}
CLASSES = {"PooledCredential", "CredentialPool"}

persisted: List[dict] = []


def _persist_pool_entries(provider, entries, removed_ids=None):
    persisted.append({"provider": provider, "count": len(entries),
                      "removed_ids": list(removed_ids or [])})


ns: Dict[str, Any] = dict(
    Any=Any, Dict=Dict, List=List, Optional=Optional, Set=Set, Tuple=Tuple,
    dataclass=dataclass, field=field, fields=fields, replace=replace,
    threading=threading, types=types,
    time=types.SimpleNamespace(time=lambda: NOW, monotonic=lambda: 0.0),
    uuid=__import__("uuid"),
    logger=types.SimpleNamespace(
        info=lambda *a, **k: None, warning=lambda *a, **k: None,
        debug=lambda *a, **k: None, error=lambda *a, **k: None),
    persist_pool_entries=_persist_pool_entries,
    # to_dict() calls this (defined in credential_persistence.py). We assert
    # persist call count and removed_ids, not payload bytes, so identity is fine.
    sanitize_borrowed_credential_payload=lambda result, provider: result,
    random=__import__("random"),
)

# Constants: lift every simple module-level assignment we might touch.
WANT_CONST_PREFIXES = ("STATUS_", "AUTH_TYPE_", "STRATEGY_", "SUPPORTED_POOL_",
                       "DEAD_MANUAL_PRUNE_TTL", "NO_AVAILABLE_ENTRIES_LOG",
                       "DEFAULT_MAX_CONCURRENT", "EXHAUSTED_TTL_", "SOURCE_MANUAL",
                       "_EXTRA_KEYS", "FAILURE_REASON_")
for node in tree.body:
    if isinstance(node, ast.Assign):
        targets = [t.id for t in node.targets if isinstance(t, ast.Name)]
        if any(t.startswith(WANT_CONST_PREFIXES) or t == t for t in targets) and \
           any(t.startswith(WANT_CONST_PREFIXES) for t in targets):
            exec(compile(ast.Module(body=[node], type_ignores=[]), "const", "exec"), ns)

# get_pool_strategy is stubbed via a controllable map so we do not read config.
_strategy_map: Dict[str, str] = {}
ns["get_pool_strategy"] = lambda provider: _strategy_map.get(provider, ns["STRATEGY_FILL_FIRST"])

fn_nodes = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in FUNCS]
cls_nodes = [n for n in tree.body if isinstance(n, ast.ClassDef) and n.name in CLASSES]
# The source uses `from __future__ import annotations`, so annotations are never
# evaluated at def time. Reproduce that in the extracted modules so forward
# references like `entry: PooledCredential` do not raise NameError.
future = ast.ImportFrom(module="__future__", names=[ast.alias(name="annotations", asname=None)], level=0)
ast.fix_missing_locations(future)
exec(compile(ast.Module(body=[future] + fn_nodes, type_ignores=[]), "funcs", "exec"), ns)
exec(compile(ast.Module(body=[future] + cls_nodes, type_ignores=[]), "classes", "exec"), ns)

PooledCredential = ns["PooledCredential"]
CredentialPool = ns["CredentialPool"]


def make_pool(provider: str, entry_dicts: List[dict], strategy: str) -> Any:
    _strategy_map[provider] = strategy
    entries = [PooledCredential.from_dict(provider, d) for d in entry_dicts]
    return CredentialPool(provider, entries)


def entry_state(pool) -> List[dict]:
    out = []
    for e in pool.entries():
        out.append(dict(id=e.id, priority=e.priority, last_status=e.last_status,
                        request_count=e.request_count,
                        access_token=e.access_token))
    return out


def observe(name: str, provider: str, entry_dicts: List[dict], strategy: str) -> dict:
    persisted.clear()
    pool = make_pool(provider, entry_dicts, strategy)
    row: Dict[str, Any] = dict(name=name, provider=provider, strategy=strategy,
                               entries_in=entry_dicts)
    row["has_credentials"] = pool.has_credentials()
    row["has_available"] = pool.has_available()
    row["next_available_at"] = pool.next_available_at()
    row["peek"] = getattr(pool.peek(), "id", None)
    sel1 = pool.select()
    row["select_1"] = getattr(sel1, "id", None)
    row["current_after_1"] = getattr(pool.current(), "id", None)
    sel2 = pool.select()
    row["select_2"] = getattr(sel2, "id", None)
    row["entries_after"] = entry_state(pool)
    row["persisted"] = list(persisted)
    # entry_id_for_api_key against the first entry's runtime key and a miss.
    first_key = entry_dicts[0].get("access_token") if entry_dicts else None
    row["id_for_first_key"] = pool.entry_id_for_api_key(first_key) if first_key else None
    row["id_for_unknown_key"] = pool.entry_id_for_api_key("no-such-key")
    return row


P = "openai-api"


def ok(id, key="k-" + "1", prio=0, **kw):
    return dict(id=id, auth_type="api_key", source="manual", access_token=key,
                priority=prio, last_status="ok", **kw)


rows = []
# 1. empty pool
rows.append(observe("empty", P, [], "fill_first"))
# 2. single ok key
rows.append(observe("single_ok", P, [ok("aaaaaa", "key-A")], "fill_first"))
# 3. priority ordering: lower priority wins
rows.append(observe("priority_order", P,
    [ok("bbbbbb", "key-B", prio=5), ok("aaaaaa", "key-A", prio=1)], "fill_first"))
# 4. unhydrated api key (empty token) is skipped
rows.append(observe("empty_token_skipped", P,
    [dict(id="empty0", auth_type="api_key", source="manual", access_token="", priority=0, last_status="ok"),
     ok("aaaaaa", "key-A", prio=1)], "fill_first"))
# 5. exhausted with future reset -> unavailable (multiple entries so not sole)
rows.append(observe("exhausted_future", P,
    [dict(id="exha00", auth_type="api_key", source="manual", access_token="key-X", priority=0,
          last_status="exhausted", last_error_code=429, last_error_reset_at=NOW + 3600,
          last_status_at=NOW - 10),
     ok("aaaaaa", "key-A", prio=1)], "fill_first"))
# 6. exhausted with elapsed reset -> cleared to ok on select (clear_expired)
rows.append(observe("exhausted_elapsed_clears", P,
    [dict(id="exhb00", auth_type="api_key", source="manual", access_token="key-X", priority=0,
          last_status="exhausted", last_error_code=429, last_error_reset_at=NOW - 5,
          last_status_at=NOW - 4000)], "fill_first"))
# 7. sole exhausted credential uses the 60s sole cooldown (still future here)
rows.append(observe("sole_exhausted_recent", P,
    [dict(id="sole00", auth_type="api_key", source="manual", access_token="key-X", priority=0,
          last_status="exhausted", last_error_code=429, last_error_reset_at=None,
          last_status_at=NOW - 10)], "fill_first"))
# 8. dead manual entry, recently dead -> stays (audit), excluded from available
rows.append(observe("dead_recent_kept", P,
    [dict(id="dead00", auth_type="api_key", source="manual", access_token="key-D", priority=0,
          last_status="dead", last_status_at=NOW - 60),
     ok("aaaaaa", "key-A", prio=1)], "fill_first"))
# 9. dead manual entry, aged past TTL -> pruned + persisted
rows.append(observe("dead_aged_pruned", P,
    [dict(id="dead01", auth_type="api_key", source="manual", access_token="key-D", priority=0,
          last_status="dead", last_status_at=NOW - (25 * 3600)),
     ok("aaaaaa", "key-A", prio=1)], "fill_first"))
# 10. least_used strategy increments request_count and picks the least used
rows.append(observe("least_used", P,
    [ok("aaaaaa", "key-A", prio=0, request_count=5), ok("bbbbbb", "key-B", prio=1, request_count=2)],
    "least_used"))
# 11. round_robin rotates priorities and persists
rows.append(observe("round_robin", P,
    [ok("aaaaaa", "key-A", prio=0), ok("bbbbbb", "key-B", prio=1), ok("cccccc", "key-C", prio=2)],
    "round_robin"))
# 12. all exhausted future -> none available, next_available_at is the min reset
rows.append(observe("all_exhausted", P,
    [dict(id="e1aaaa", auth_type="api_key", source="manual", access_token="k1", priority=0,
          last_status="exhausted", last_error_code=429, last_error_reset_at=NOW + 1800, last_status_at=NOW-10),
     dict(id="e2aaaa", auth_type="api_key", source="manual", access_token="k2", priority=1,
          last_status="exhausted", last_error_code=429, last_error_reset_at=NOW + 900, last_status_at=NOW-10)],
    "fill_first"))

data = {"now": NOW, "rows": rows}
text = json.dumps(data, indent=2, sort_keys=True, default=str) + "\n"
if "--check" in sys.argv:
    existing = OUT.read_text() if OUT.exists() else ""
    if existing != text:
        print("MISMATCH: regenerate credential-pool-select-goldens.json", file=sys.stderr)
        sys.exit(1)
    print(f"Verified {len(rows)} credential-pool selection cases")
else:
    OUT.write_text(text)
    print(f"Wrote {len(rows)} cases to {OUT}")
