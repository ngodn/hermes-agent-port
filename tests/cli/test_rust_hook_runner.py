"""Tests for hermes_cli.rust_hook_runner."""

from __future__ import annotations

import io
import json
from pathlib import Path
import subprocess
import sys

import pytest

from hermes_cli.rust_hook_runner import main


def _run_seam(
    argv: list[str],
    stdin_data: str,
) -> tuple[int, str, str]:
    stdin = io.StringIO(stdin_data)
    stdout = io.StringIO()
    stderr = io.StringIO()
    code = main(argv, stdin=stdin, stdout=stdout, stderr=stderr)
    return code, stdout.getvalue(), stderr.getvalue()


def test_sync_handler_returns_structured_json(tmp_path: Path) -> None:
    handler = tmp_path / "handler.py"
    handler.write_text(
        "def handle(event_type, context):\n"
        "    return {'handled': True, 'event': event_type, 'user': context.get('user')}\n",
        encoding="utf-8",
    )
    code, stdout, stderr = _run_seam(
        [str(handler), "session:start"],
        '{"user": "alice"}',
    )
    assert code == 0
    assert stdout == '{"handled":true,"event":"session:start","user":"alice"}'
    assert stderr == ""


def test_sync_handler_returns_none(tmp_path: Path) -> None:
    handler = tmp_path / "handler.py"
    handler.write_text(
        "def handle(event_type, context):\n    return None\n",
        encoding="utf-8",
    )
    code, stdout, stderr = _run_seam(
        [str(handler), "session:start"],
        '{"key": "val"}',
    )
    assert code == 0
    assert stdout == ""
    assert stderr == ""


def test_async_handler_returns_structured_json(tmp_path: Path) -> None:
    handler = tmp_path / "handler.py"
    handler.write_text(
        "import asyncio\n"
        "async def handle(event_type, context):\n"
        "    await asyncio.sleep(0.001)\n"
        "    return {'async': True, 'num': context.get('count', 0) + 1}\n",
        encoding="utf-8",
    )
    code, stdout, stderr = _run_seam(
        [str(handler), "agent:start"],
        '{"count": 41}',
    )
    assert code == 0
    assert stdout == '{"async":true,"num":42}'
    assert stderr == ""


def test_async_handler_returns_none(tmp_path: Path) -> None:
    handler = tmp_path / "handler.py"
    handler.write_text(
        "async def handle(event_type, context):\n    return None\n",
        encoding="utf-8",
    )
    code, stdout, stderr = _run_seam(
        [str(handler), "agent:end"],
        "{}",
    )
    assert code == 0
    assert stdout == ""
    assert stderr == ""


def test_exact_arguments_delivered_to_handle(tmp_path: Path) -> None:
    handler = tmp_path / "handler.py"
    handler.write_text(
        "def handle(event_type, context):\n"
        "    assert isinstance(event_type, str)\n"
        "    assert isinstance(context, dict)\n"
        "    return {'event': event_type, 'ctx': context}\n",
        encoding="utf-8",
    )
    payload = {"nested": {"items": [1, 2, 3]}}
    code, stdout, stderr = _run_seam(
        [str(handler), "custom:test_event"],
        json.dumps(payload),
    )
    assert code == 0
    assert json.loads(stdout) == {"event": "custom:test_event", "ctx": payload}
    assert stderr == ""


@pytest.mark.parametrize("bad_json", ["{bad json", "", "{}{}"])
def test_malformed_json_input(tmp_path: Path, bad_json: str) -> None:
    handler = tmp_path / "handler.py"
    handler.write_text(
        "def handle(event_type, context):\n    return {'ok': True}\n",
        encoding="utf-8",
    )
    code, stdout, stderr = _run_seam(
        [str(handler), "event:test"],
        bad_json,
    )
    assert code != 0
    assert stdout == ""
    assert "Malformed JSON context" in stderr or "Expecting value" in stderr


@pytest.mark.parametrize(
    "non_object_json",
    ["[1, 2, 3]", '"string"', "42", "true", "null"],
)
def test_non_object_json_input(tmp_path: Path, non_object_json: str) -> None:
    handler = tmp_path / "handler.py"
    handler.write_text(
        "def handle(event_type, context):\n    return {'ok': True}\n",
        encoding="utf-8",
    )
    code, stdout, stderr = _run_seam(
        [str(handler), "event:test"],
        non_object_json,
    )
    assert code != 0
    assert stdout == ""
    assert "Context must be a JSON object" in stderr


def test_missing_handle_symbol(tmp_path: Path) -> None:
    handler = tmp_path / "handler.py"
    handler.write_text("FOO = 1\n", encoding="utf-8")
    code, stdout, stderr = _run_seam([str(handler), "event:test"], "{}")
    assert code != 0
    assert stdout == ""
    assert "has no callable 'handle'" in stderr


def test_non_callable_handle(tmp_path: Path) -> None:
    handler = tmp_path / "handler.py"
    handler.write_text("handle = 'not_callable'\n", encoding="utf-8")
    code, stdout, stderr = _run_seam([str(handler), "event:test"], "{}")
    assert code != 0
    assert stdout == ""
    assert "has no callable 'handle'" in stderr


def test_import_error_syntax_error(tmp_path: Path) -> None:
    handler = tmp_path / "handler.py"
    handler.write_text("def handle(: syntax error\n", encoding="utf-8")
    code, stdout, stderr = _run_seam([str(handler), "event:test"], "{}")
    assert code != 0
    assert stdout == ""
    assert "Failed to import handler module" in stderr


def test_import_error_missing_module(tmp_path: Path) -> None:
    handler = tmp_path / "handler.py"
    handler.write_text("import nonexistent_module_xyz_123\n", encoding="utf-8")
    code, stdout, stderr = _run_seam([str(handler), "event:test"], "{}")
    assert code != 0
    assert stdout == ""
    assert "Failed to import handler module" in stderr


def test_import_error_nonexistent_file(tmp_path: Path) -> None:
    missing = tmp_path / "does_not_exist.py"
    code, stdout, stderr = _run_seam([str(missing), "event:test"], "{}")
    assert code != 0
    assert stdout == ""
    assert "Handler file not found" in stderr


def test_raised_handler_sync(tmp_path: Path) -> None:
    handler = tmp_path / "handler.py"
    handler.write_text(
        "def handle(event_type, context):\n"
        "    raise ValueError('secret_error_code_42')\n",
        encoding="utf-8",
    )
    code, stdout, stderr = _run_seam([str(handler), "event:test"], "{}")
    assert code != 0
    assert stdout == ""
    assert "secret_error_code_42" in stderr
    assert "Handler execution failed" in stderr


def test_raised_handler_async(tmp_path: Path) -> None:
    handler = tmp_path / "handler.py"
    handler.write_text(
        "async def handle(event_type, context):\n"
        "    raise RuntimeError('async_secret_failure')\n",
        encoding="utf-8",
    )
    code, stdout, stderr = _run_seam([str(handler), "event:test"], "{}")
    assert code != 0
    assert stdout == ""
    assert "async_secret_failure" in stderr
    assert "Handler execution failed" in stderr


def test_paths_containing_spaces(tmp_path: Path) -> None:
    spaced_dir = tmp_path / "folder with spaces in name"
    spaced_dir.mkdir()
    handler = spaced_dir / "my hook handler.py"
    handler.write_text(
        "def handle(event_type, context):\n    return {'spaced_path': True}\n",
        encoding="utf-8",
    )
    code, stdout, stderr = _run_seam([str(handler), "event:test"], "{}")
    assert code == 0
    assert stdout == '{"spaced_path":true}'
    assert stderr == ""


@pytest.mark.parametrize(
    "bad_argv",
    [
        [],
        ["only_one_arg"],
        ["one", "two", "three"],
    ],
)
def test_malformed_argv(bad_argv: list[str]) -> None:
    code, stdout, stderr = _run_seam(bad_argv, "{}")
    assert code == 2
    assert stdout == ""
    assert "Usage: python -m hermes_cli.rust_hook_runner" in stderr


def test_imported_module_cleanup(tmp_path: Path) -> None:
    handler = tmp_path / "cleanup_handler.py"
    handler.write_text(
        "def handle(event_type, context):\n    return {'cleaned': True}\n",
        encoding="utf-8",
    )
    code, stdout, stderr = _run_seam([str(handler), "event:test"], "{}")
    assert code == 0
    assert not any(k.startswith("_hermes_rust_hook_") for k in sys.modules)

    # Test that cleanup happens even on execution error
    bad_handler = tmp_path / "bad_cleanup_handler.py"
    bad_handler.write_text(
        "def handle(event_type, context):\n    raise RuntimeError('fail')\n",
        encoding="utf-8",
    )
    code, stdout, stderr = _run_seam([str(bad_handler), "event:test"], "{}")
    assert code != 0
    assert not any(k.startswith("_hermes_rust_hook_") for k in sys.modules)


def test_subprocess_python_m_proof(tmp_path: Path) -> None:
    handler = tmp_path / "subproc_handler.py"
    handler.write_text(
        "def handle(event_type, context):\n"
        "    return {'ok': True, 'event': event_type, 'payload': context}\n",
        encoding="utf-8",
    )
    proc = subprocess.run(
        [
            sys.executable,
            "-m",
            "hermes_cli.rust_hook_runner",
            str(handler),
            "gateway:startup",
        ],
        input=b'{"gateway_id":"test-1"}',
        capture_output=True,
        check=False,
    )
    assert proc.returncode == 0
    assert proc.stdout.decode("utf-8") == (
        '{"ok":true,"event":"gateway:startup","payload":{"gateway_id":"test-1"}}'
    )
    assert proc.stderr.decode("utf-8") == ""
