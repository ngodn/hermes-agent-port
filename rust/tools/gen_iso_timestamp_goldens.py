#!/usr/bin/env python3
"""Compare the shared ISO parser with CPython 3.12 datetime.fromisoformat."""
import ast
import itertools
import json
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/iso-timestamp-goldens.json"
tree = ast.parse((ROOT / "agent/credential_pool.py").read_text())
node = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == "_parse_absolute_timestamp")
ns = dict(Any=Any, Optional=Optional, datetime=datetime)
exec(compile(ast.Module(body=[node], type_ignores=[]), "deadline", "exec"), ns)
dates = ["2026-09-06", "20260906", "2026-W36-7", "2026W367", "2026-W36", "2026W36"]
times = ["12", "1234", "12:34", "123456", "12:34:56", "12.5", "12:34,5", "123456.123456789"]
offsets = ["", "Z", "+08", "-0530", "+08:00", "+01:02:03.4", "-00:00:00.5", "+00:90"]
texts = list(dates)
texts += [date + sep + clock + offset for date, sep, clock, offset in itertools.product(dates, ["T", " ", "🐍", "0"], times, offsets)]
texts += ["", "2026", "2026-09", "2026-249", "2026-02-29", "0000-01-01", "10000-01-01", "2026-W00-1", "2026-W54-1", "2026W360", "9999-W52-7"]
texts += ["2026-09-06" + suffix for suffix in ["T", "Z", "TT12", "T1", "T12:", "T12:3", "T1234:56", "T12:3456", "T24:00", "T12:60", "T12:34:60", "T12.", "T12:34z", "T12:34+24:00", "T12:34+00:00:00.5", "T12:34Zgarbage", "T١٢:٣٤", "T12:34+01:99:99", "\x0012:34Z"]]
rows = []
for text in dict.fromkeys(texts):
    try:
        parsed = datetime.fromisoformat(text)
        aware = parsed.tzinfo is not None
        result = parsed.replace(tzinfo=timezone.utc).timestamp() if not aware else parsed.timestamp()
    except (ValueError, OverflowError):
        result, aware = None, False
    row = dict(text=text, result=result)
    # Naive deadlines use the machine timezone, so keep their fixtures portable
    # by testing the shared parser with an explicit UTC interpretation instead.
    if aware or result is None:
        row["cooldown"] = ns["_parse_absolute_timestamp"](text)
    rows.append(row)
encoded = json.dumps(rows, indent=2) + "\n"
if sys.argv[1:] == ["--check"]:
    assert OUT.read_text() == encoded
elif not sys.argv[1:]:
    OUT.write_text(encoded)
else:
    raise SystemExit("usage: gen_iso_timestamp_goldens.py [--check]")
print(f"Verified {len(rows)} ISO timestamp cases")
