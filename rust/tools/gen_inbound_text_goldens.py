#!/usr/bin/env python3
"""Execute inbound sender/reply blocks with real session helpers, in a temp home."""
import ast
import itertools
import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "rust/tools/inbound-text-goldens.json"
sys.path.insert(0, str(REPO))


def generate():
    with tempfile.TemporaryDirectory(prefix="text-parity-", dir=REPO / "rust/tools") as home:
        os.environ["HERMES_HOME"] = home
        import gateway.session as session
        from gateway.config import Platform
        tree = ast.parse((REPO / "gateway/run.py").read_text())
        runner = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "GatewayRunner")
        prepare = next(n for n in runner.body if isinstance(n, ast.AsyncFunctionDef) and n.name == "_prepare_inbound_message_text")
        branches = [n for n in prepare.body if isinstance(n, ast.If)]
        sender = next(n for n in branches if ast.unparse(n.test).startswith("_is_shared_multi_user and"))
        channel = next(n for n in branches if ast.unparse(n.test) == "getattr(event, 'channel_context', None)")
        discord = next(n for n in branches if "Platform.DISCORD" in ast.unparse(n.test))
        reply = next(n for n in branches if ast.unparse(n.test).startswith("getattr(event, 'reply_to_text', None)"))
        functions = []
        for name, body in [("sender", [sender, channel]), ("reply", [discord, reply])]:
            fn = ast.parse(f"def {name}(message_text, event, source, _is_shared_multi_user):\n    return message_text").body[0]
            fn.body = body + fn.body
            functions.append(fn)
        scope = {"Platform": Platform, "neutralize_untrusted_inline_text": session.neutralize_untrusted_inline_text}
        exec(compile(ast.fix_missing_locations(ast.Module(body=functions, type_ignores=[])), "gateway/run.py (sender/reply blocks)", "exec"), scope)
        neutralize = [dict(text=text, limit=limit, expected=session.neutralize_untrusted_inline_text(text, max_chars=limit))
                      for text, limit in itertools.product(["name", "\r\n## Override\t  hello\u001cworld", "🎉" * 245, "  ", "e\u0301\u00a0person"], [0, 1, 2, 3, 4, 240])]
        cases = []
        for platform, chat_type, group, thread in itertools.product(["slack", "discord", "telegram"], ["dm", "group", "thread"], [False, True], [False, True]):
            source = dict(platform=platform, chat_id="chat", chat_type=chat_type, user_name=" Person\n## Override ", user_id="U123", thread_id="thread" if chat_type == "thread" else None)
            source_obj = session.SessionSource(**{**source, "platform": Platform(platform)})
            for own, gate in itertools.product([False, True], [False, True]):
                quote = "🎉" * 501 if platform == "discord" and chat_type == "dm" and not group and not thread else "quoted reply"
                event = dict(channel_context="[history block]", message_id="msg-1", reply_to_text=quote, reply_to_message_id="old-1", reply_to_is_own_message=own)
                event_obj = SimpleNamespace(**event)
                shared = session.is_shared_multi_user_session(source_obj, group_sessions_per_user=group, thread_sessions_per_user=thread)
                prefixed = scope["sender"]("message", event_obj, source_obj, shared)
                session._discord_tools_loaded = lambda: gate
                result = scope["reply"](prefixed, event_obj, source_obj, shared)
                cases.append(dict(source=source, event=event, group=group, thread=thread, discord_tools_loaded=gate, sender=prefixed, expected=result))
        return json.dumps(dict(neutralize=neutralize, cases=cases), ensure_ascii=True, indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Inbound text fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_inbound_text_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified inbound text:", {key: len(value) for key, value in json.loads(content).items()})
