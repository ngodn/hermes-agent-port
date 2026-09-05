#!/usr/bin/env python3
"""Execute reference cooldown/deadline/error-normalization policies."""
import ast
import itertools
import json
import re
import sys
import types
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, Optional

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/credential-cooldown-goldens.json"
tree = ast.parse((ROOT / "agent/credential_pool.py").read_text())
names = {"_exhausted_ttl", "_parse_absolute_timestamp", "_extract_retry_delay_seconds", "_normalize_error_context", "_exhausted_until"}
nodes = [node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name in names]
ns = dict(Any=Any, Dict=Dict, Optional=Optional, datetime=datetime, re=re,
          time=types.SimpleNamespace(time=lambda:1_700_000_000.0),
          PooledCredential=object, STATUS_EXHAUSTED="exhausted")
for node in tree.body:
    if isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and (t.id.startswith("EXHAUSTED_TTL_") or t.id.startswith("FAILURE_REASON_BILLING")) for t in node.targets):
        exec(compile(ast.Module(body=[node], type_ignores=[]), "constants", "exec"), ns)
exec(compile(ast.Module(body=nodes, type_ignores=[]), "cooldowns", "exec"), ns)
rows = []
for code, sole, reason in itertools.product([None, 400, 401, 402, 403, 429, 500, 503], [False, True], [None, "billing", "billing_unverified", "rate_limit"]):
    rows.append(dict(kind="ttl", code=code, sole=sole, reason=reason,
                     result=float(ns["_exhausted_ttl"](code, sole_credential=sole, failure_reason=reason))))
timestamps = [None, "", 0, -1, 1, True, False, 1_000_000_000_000, 1_000_000_000_001,
              "0", "-1", " 1_700_000_000_123 ", "١٧٠٠٠٠٠٠٠٠", "invalid", [], {},
              "2026-09-06T12:34:56Z", "2026-09-06T12:34:56.123456+08:00"]
for value in timestamps:
    rows.append(dict(kind="timestamp", value=value, result=ns["_parse_absolute_timestamp"](value)))
for status, reset, at, sole in itertools.product([None, "ok", "dead", "exhausted"], timestamps, [None, 0.0, 1_700_000_000.0], [False, True]):
    entry = dict(last_status=status, last_error_reset_at=reset, last_status_at=at,
                 last_error_code=429, failure_reason=None)
    result = ns["_exhausted_until"](types.SimpleNamespace(**entry), sole_credential=sole)
    rows.append(dict(kind="until", entry=entry, sole=sole, result=result))
messages = ["", "no reset", "quotaResetDelay: 1500ms", 'quotaResetDelay"2.5s',
            "retry after 3 seconds", "retry 2.5secs", "RETRY\x1cAFTER\x1f٤ sec",
            "Resets in 4hr 5min", "reset in 2 hr", "resets in 12 min", "retry 2 s foo",
            "retry -2 seconds", "retry 3 seconds; quotaResetDelay: 200ms",
            "resets in 2hr 3min; retry 9 seconds", "reset in 2 hours"]
for message in messages:
    result = ns["_extract_retry_delay_seconds"](message)
    rows.append(dict(kind="delay", message=message, result=None if result is None else float(result)))
for message, reset in itertools.product(messages, [None, 0, "0", "invalid", 1_700_000_010]):
    context = dict(reason=" rate_limit ", message=message, reset_at=reset,
                   resets_at="2026-09-06T12:34:56Z", retry_until=42, ignored=True)
    rows.append(dict(kind="context", context=context, result=ns["_normalize_error_context"](context)))
for context in [None, [], {}, {"reason":7,"message":False}, {"reset_at":"invalid","message":"retry after 5 s"}]:
    rows.append(dict(kind="context", context=context, result=ns["_normalize_error_context"](context)))
text = json.dumps(rows, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert OUT.read_text() == text
elif not sys.argv[1:]:
    OUT.write_text(text)
else:
    raise SystemExit("usage: gen_credential_cooldown_goldens.py [--check]")
print(f"Verified {len(rows)} credential cooldown cases")
