"""Single-turn JSONL streaming entrypoint for subprocess callers.

This is the strangler-migration bridge for the Rust rewrite (see
``rust/PORT.md``). The Rust gateway spawns this module as a child process,
feeds it one user prompt, and reads newline-delimited JSON events from stdout,
mapping them onto ``hermes_core::stream::StreamEvent``.

It exists because there is no headless *streaming* single-turn CLI today:
``hermes -z`` blocks and mutes streaming, and ``hermes chat -q`` streams ANSI
to a Rich TTY. This module hooks ``AIAgent``'s streaming callbacks and emits
machine-readable JSON Lines instead, one event per line:

    {"event": "text_delta",     "data": {"text": "..."}}
    {"event": "thinking_delta", "data": {"text": "..."}}
    {"event": "tool_start",     "data": {"tool": "...", "args": {...}}}
    {"event": "tool_complete",  "data": {"tool": "...", "ok": true, "duration": 0.0}}
    {"event": "done",           "data": {"completed": true, "final_response": "..."}}
    {"event": "error",          "data": {"message": "..."}}

Usage:
    python -m hermes_cli.stream_turn --prompt "hello"      # prompt on argv
    echo "hello" | python -m hermes_cli.stream_turn -p -   # prompt on stdin

Headless execution guards (yolo, accept-hooks, stateless delegation) are set
the same way ``hermes_cli/oneshot.py`` sets them, so approvals never block.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import threading


# stdout is written from the agent's worker thread(s) via callbacks AND from
# the main thread for the terminal events; serialize so lines never interleave.
_STDOUT_LOCK = threading.Lock()


def _emit(event: str, data: dict) -> None:
    line = json.dumps({"event": event, "data": data}, ensure_ascii=False)
    with _STDOUT_LOCK:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


def _resolve_model(args, cfg) -> str | None:
    """Model precedence: explicit flag, then config ``model.default`` / ``model.model``."""
    if args.model:
        return args.model
    model_cfg = cfg.get("model") if isinstance(cfg, dict) else None
    if isinstance(model_cfg, dict):
        return model_cfg.get("default") or model_cfg.get("model")
    return None


def main() -> None:
    parser = argparse.ArgumentParser(prog="hermes_cli.stream_turn")
    parser.add_argument(
        "-p",
        "--prompt",
        default="-",
        help="Prompt text, or '-' to read the whole prompt from stdin.",
    )
    parser.add_argument("-m", "--model", default=None)
    parser.add_argument("--provider", default=None)
    parser.add_argument("-t", "--toolsets", default=None)
    args = parser.parse_args()

    prompt = sys.stdin.read() if args.prompt == "-" else args.prompt
    if not prompt.strip():
        # Nothing to do; emit a terminal event so the caller isn't left waiting.
        _emit("done", {"completed": True, "final_response": ""})
        sys.exit(0)

    # Headless execution guards, mirroring hermes_cli/oneshot.py.
    os.environ["HERMES_YOLO_MODE"] = "1"
    os.environ["HERMES_ACCEPT_HOOKS"] = "1"

    # Heavy imports happen after arg parsing so --help stays fast.
    from gateway.session_context import declare_stateless_channel
    from hermes_cli.config import load_config
    from hermes_cli.runtime_provider import resolve_runtime_provider
    from run_agent import AIAgent

    declare_stateless_channel()

    cfg = load_config()
    runtime = resolve_runtime_provider(requested=args.provider, target_model=args.model)
    model = _resolve_model(args, cfg)

    def _on_tool_complete(tool_name, *rest, **kw):
        # AIAgent's tool_complete_callback signature has drifted over time;
        # accept anything and report just what we can name reliably.
        _emit(
            "tool_complete",
            {
                "tool": tool_name,
                "ok": bool(kw.get("ok", True)),
                "duration": float(kw.get("duration", 0.0) or 0.0),
            },
        )

    agent = AIAgent(
        api_key=runtime.get("api_key"),
        base_url=runtime.get("base_url"),
        provider=runtime.get("provider"),
        model=model,
        quiet_mode=True,
        stream_delta_callback=lambda text: _emit("text_delta", {"text": text}),
        thinking_callback=lambda text: _emit("thinking_delta", {"text": text}),
        tool_start_callback=lambda name, tool_args=None: _emit(
            "tool_start", {"tool": name, "args": tool_args or {}}
        ),
        tool_complete_callback=_on_tool_complete,
    )

    try:
        result = agent.run_conversation(prompt) or {}
        _emit(
            "done",
            {
                "completed": bool(result.get("completed", True)),
                "final_response": result.get("final_response", "") or "",
            },
        )
    except Exception as exc:  # noqa: BLE001 - report any failure to the caller
        _emit("error", {"message": f"{type(exc).__name__}: {exc}"})
        sys.exit(1)
    finally:
        try:
            agent.close()
        except Exception:
            pass


if __name__ == "__main__":
    main()
