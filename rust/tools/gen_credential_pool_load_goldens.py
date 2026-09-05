#!/usr/bin/env python3
"""Differential oracle for load_pool's store-backed assembly (non-OAuth path).

Executes the REAL load_pool control flow (AST-extracted from
agent/credential_pool.py) with the real _prune_stale_seeded_entries,
_normalize_pool_priorities, _normalize_pool_auth_type, PooledCredential and
is_borrowed_credential_source. Only the boundaries the Rust store-backed slice
DEFERS or performs as I/O are stubbed, identically on both sides:

  read_credential_pool -> returns the fixture rows
  persist_pool_entries -> captured (count + sorted removed_ids)
  _seed_from_singletons / _seed_from_env / _seed_custom_pool -> (False, set())
  _load_auth_store -> {},  _profile_owns_pool_provider -> True
  heal_forked_single_use_oauth_grants -> no-op
  sanitize_borrowed_credential_payload -> identity
  SINGLE_USE_REFRESH_POOL_PROVIDERS -> empty

So this proves the ASSEMBLY glue (disk_ids, the changed flag, persist timing,
removed_ids, priority sort) matches Python, given the already-verified prune /
normalize helpers. Env/singleton/custom SEEDING is out of scope for this slice
and is stubbed off on both sides.

Scope: only non-anthropic, non-custom, non-single-use providers, where no
seeder would re-add an entry, so the store-backed load equals full Python for
the persisted set. Anthropic/custom/single-use pools need the deferred seeding
and are intentionally excluded (the Rust load_pool guards them).

Usage: python rust/tools/gen_credential_pool_load_goldens.py [--check]
"""
import ast
import json
import sys
import types
from dataclasses import dataclass, field, fields, replace
from pathlib import Path
from typing import Any, Dict, List, Optional, Set, Tuple

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/credential-pool-load-goldens.json"
POOL_SRC = ast.parse((ROOT / "agent/credential_pool.py").read_text())
PERS_SRC = ast.parse((ROOT / "agent/credential_persistence.py").read_text())

captured: List[dict] = []


def _persist(provider, entries, removed_ids=None):
    captured.append({"provider": provider, "count": len(entries),
                     "removed_ids": sorted(removed_ids or [])})


def _read_credential_pool(provider):
    return list(FIXTURE_ROWS)


ns: Dict[str, Any] = dict(
    Any=Any, Dict=Dict, List=List, Optional=Optional, Set=Set, Tuple=Tuple,
    dataclass=dataclass, field=field, fields=fields, replace=replace,
    uuid=__import__("uuid"), types=types,
    logger=types.SimpleNamespace(info=lambda *a, **k: None, warning=lambda *a, **k: None,
                                 debug=lambda *a, **k: None, error=lambda *a, **k: None),
    read_credential_pool=_read_credential_pool,
    persist_pool_entries=_persist,
    sanitize_borrowed_credential_payload=lambda payload, provider: payload,
    _load_auth_store=lambda: {},
    _profile_owns_pool_provider=lambda provider: True,
    _seed_from_singletons=lambda provider, entries: (False, set()),
    _seed_from_env=lambda provider, entries: (False, set()),
    _seed_custom_pool=lambda pool_key, entries: (False, set()),
    get_pool_strategy=lambda provider: "fill_first",
    SINGLE_USE_REFRESH_POOL_PROVIDERS=set(),
    auth_mod=types.SimpleNamespace(heal_forked_single_use_oauth_grants=lambda p: None),
    threading=__import__("threading"),
    time=types.SimpleNamespace(time=lambda: 1_700_000_000.0, monotonic=lambda: 0.0),
    random=__import__("random"),
    re=__import__("re"),
    os=__import__("os"),
)

WANT_CONST_PREFIXES = ("STATUS_", "AUTH_TYPE_", "STRATEGY_", "SUPPORTED_POOL_",
                       "DEAD_MANUAL_PRUNE_TTL", "NO_AVAILABLE_ENTRIES_LOG",
                       "DEFAULT_MAX_CONCURRENT", "EXHAUSTED_TTL_", "SOURCE_MANUAL",
                       "_EXTRA_KEYS", "FAILURE_REASON_", "CUSTOM_POOL_PREFIX",
                       "BORROWED_", "SINGLETON_")
for node in POOL_SRC.body:
    if isinstance(node, ast.Assign):
        names = [t.id for t in node.targets if isinstance(t, ast.Name)]
        if any(n.startswith(WANT_CONST_PREFIXES) for n in names):
            try:
                exec(compile(ast.Module(body=[node], type_ignores=[]), "c", "exec"), ns)
            except Exception:
                pass

FUNCS = {"_normalize_pool_auth_type", "_parse_absolute_timestamp", "_is_manual_source",
         "_normalize_pool_priorities", "_prune_stale_seeded_entries", "load_pool",
         "CredentialPool"}
future = ast.ImportFrom(module="__future__", names=[ast.alias(name="annotations", asname=None)], level=0)
ast.fix_missing_locations(future)

# is_borrowed_credential_source from credential_persistence.py (+ its constants).
pers_nodes = [n for n in PERS_SRC.body
              if (isinstance(n, ast.FunctionDef) and n.name == "is_borrowed_credential_source")
              or (isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id.isupper() for t in n.targets))]
exec(compile(ast.Module(body=[future] + pers_nodes, type_ignores=[]), "pers", "exec"), ns)

pool_defs = [n for n in POOL_SRC.body
             if (isinstance(n, ast.FunctionDef) and n.name in FUNCS)
             or (isinstance(n, ast.ClassDef) and n.name in ("PooledCredential", "CredentialPool"))]
exec(compile(ast.Module(body=[future] + pool_defs, type_ignores=[]), "pool", "exec"), ns)

load_pool = ns["load_pool"]


def _peek_key(pool):
    entry = pool.peek()
    if entry is None:
        return None
    key = str(getattr(entry, "runtime_api_key", "") or getattr(entry, "access_token", "") or "").strip()
    return key or None

FIXTURES = {
    # Clean manual pool: nothing prunes/normalizes -> no persist. Priority order.
    "clean_manual": ("openai-api", [
        {"id": "aaaaaa", "auth_type": "api_key", "source": "manual", "access_token": "sk-A", "priority": 0, "last_status": "ok"},
        {"id": "bbbbbb", "auth_type": "api_key", "source": "manual", "access_token": "sk-B", "priority": 1, "last_status": "ok"},
    ]),
    # A borrowed (hermes_pkce) source with no active backing -> pruned + persisted with removed_ids.
    "prune_stale_borrowed": ("openai-api", [
        {"id": "aaaaaa", "auth_type": "api_key", "source": "manual", "access_token": "sk-A", "priority": 0, "last_status": "ok"},
        {"id": "pkce00", "auth_type": "oauth", "source": "hermes_pkce", "access_token": "tok", "priority": 1, "last_status": "ok"},
    ]),
    # env: source is NOT pruned on a plain load (prune_env_sources=False).
    "env_source_kept": ("openrouter", [
        {"id": "env000", "auth_type": "api_key", "source": "env:OPENROUTER_API_KEY", "access_token": "sk-E", "priority": 0, "last_status": "ok"},
    ]),
    # Entry without an id in the store: minted id, not counted in disk_ids.
    "no_id_row": ("openai-api", [
        {"auth_type": "api_key", "source": "manual", "access_token": "sk-noid", "priority": 0, "last_status": "ok"},
    ]),
}

rows = []
for name, (provider, fixture) in FIXTURES.items():
    global FIXTURE_ROWS
    FIXTURE_ROWS = fixture
    captured.clear()
    pool = load_pool(provider)
    # Rows without an id in the fixture get a random minted id (uuid4); it is
    # not part of the contract and would make this golden non-deterministic, so
    # emit id=null for them. The Rust test skips id assertions for id-less rows.
    fixture_ids = {r.get("id") for r in fixture if r.get("id")}
    entries_out = [
        {"id": (e.id if e.id in fixture_ids else None), "priority": e.priority,
         "source": e.source, "last_status": e.last_status, "access_token": e.access_token}
        for e in pool.entries()
    ]
    rows.append({
        "name": name, "provider": provider, "fixture": fixture,
        "entries_out": entries_out,
        "has_credentials": pool.has_credentials(),
        "peek_key": _peek_key(pool),
        "persisted": list(captured),
    })

data = {"rows": rows}
text = json.dumps(data, indent=2, sort_keys=True, default=str) + "\n"
if "--check" in sys.argv:
    if (OUT.read_text() if OUT.exists() else "") != text:
        print("MISMATCH: regenerate credential-pool-load-goldens.json", file=sys.stderr)
        sys.exit(1)
    print(f"Verified {len(rows)} load_pool assembly cases")
else:
    OUT.write_text(text)
    print(f"Wrote {len(rows)} cases to {OUT}")
