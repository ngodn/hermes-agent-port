"""Python compatibility runner for Rust-launched gateway hooks.

Loads a hook handler.py module, calls handle(event_type, context),
and emits the compact JSON result to stdout.
"""

from __future__ import annotations

import asyncio
import importlib.util
import inspect
import json
from pathlib import Path
import sys
from typing import Any, Sequence, TextIO
import uuid


def main(
    argv: Sequence[str] | None = None,
    stdin: TextIO | None = None,
    stdout: TextIO | None = None,
    stderr: TextIO | None = None,
) -> int:
    """Run a gateway hook handler and emit its JSON output."""
    if argv is None:
        argv = sys.argv[1:]
    if stdin is None:
        stdin = sys.stdin
    if stdout is None:
        stdout = sys.stdout
    if stderr is None:
        stderr = sys.stderr

    if len(argv) != 2:
        stderr.write(
            "Usage: python -m hermes_cli.rust_hook_runner <handler-path> <event-type>\n"
        )
        return 2

    handler_path_str, event_type = argv[0], argv[1]

    try:
        raw_input = stdin.read()
    except Exception as exc:
        stderr.write(f"Failed to read stdin: {exc}\n")
        return 1

    try:
        context = json.loads(raw_input)
    except Exception as exc:
        stderr.write(f"Malformed JSON context: {exc}\n")
        return 1

    if not isinstance(context, dict):
        stderr.write("Context must be a JSON object\n")
        return 1

    handler_path = Path(handler_path_str).resolve()
    if not handler_path.is_file():
        stderr.write(f"Handler file not found: {handler_path_str}\n")
        return 1

    module_name = f"_hermes_rust_hook_{uuid.uuid4().hex}"
    try:
        try:
            spec = importlib.util.spec_from_file_location(
                module_name, str(handler_path)
            )
        except Exception as exc:
            stderr.write(f"Could not load module spec for {handler_path_str}: {exc}\n")
            return 1

        if spec is None or spec.loader is None:
            stderr.write(f"Could not load module spec for: {handler_path_str}\n")
            return 1

        module = importlib.util.module_from_spec(spec)
        sys.modules[module_name] = module

        try:
            spec.loader.exec_module(module)
        except Exception as exc:
            stderr.write(f"Failed to import handler module: {exc}\n")
            return 1

        handle_fn = getattr(module, "handle", None)
        if not callable(handle_fn):
            stderr.write(
                f"Handler module at {handler_path_str} has no callable 'handle'\n"
            )
            return 1

        try:
            result = handle_fn(event_type, context)
            if inspect.isawaitable(result):
                if not asyncio.iscoroutine(result):

                    async def _wrap(target: Any) -> Any:
                        return await target

                    result = _wrap(result)
                if hasattr(asyncio, "Runner"):
                    with asyncio.Runner() as runner:
                        result = runner.run(result)
                else:
                    result = asyncio.run(result)
        except Exception as exc:
            stderr.write(f"Handler execution failed: {exc}\n")
            return 1

        if result is not None:
            try:
                compact_json = json.dumps(result, separators=(",", ":"))
            except Exception as exc:
                stderr.write(f"Failed to serialize handler result to JSON: {exc}\n")
                return 1
            stdout.write(compact_json)

        return 0
    finally:
        sys.modules.pop(module_name, None)


if __name__ == "__main__":
    sys.exit(main())
