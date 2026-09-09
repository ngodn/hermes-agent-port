"""Session-switch tests for the native Rust extension host."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock

import pytest

from agent.memory_manager import MemoryManager
from agent.memory_provider import MemoryProvider
from hermes_cli.rust_extension_host import ExtensionHost


class _RecordingProvider(MemoryProvider):
    def __init__(self, name: str = "rec", should_raise: bool = False) -> None:
        self._name = name
        self.should_raise = should_raise
        self.switch_calls: list[dict[str, Any]] = []
        self.sync_calls: list[dict[str, Any]] = []
        self.prefetch_calls: list[dict[str, Any]] = []
        self.queue_calls: list[dict[str, Any]] = []

    @property
    def name(self) -> str:
        return self._name

    def is_available(self) -> bool:
        return True

    def initialize(self, session_id: str, **kwargs: Any) -> None:
        pass

    def get_tool_schemas(self) -> list[dict[str, Any]]:
        return []

    def on_session_switch(
        self,
        new_session_id: str,
        *,
        parent_session_id: str = "",
        reset: bool = False,
        **kwargs: Any,
    ) -> None:
        if self.should_raise:
            raise RuntimeError(f"provider {self._name} switch failed")
        self.switch_calls.append({
            "new": new_session_id,
            "parent": parent_session_id,
            "reset": reset,
            "extra": dict(kwargs),
        })

    def prefetch(self, query: str, *, session_id: str = "") -> str:
        self.prefetch_calls.append({"query": query, "session_id": session_id})
        return "recalled_data"

    def sync_turn(
        self,
        user_content: str,
        assistant_content: str,
        *,
        session_id: str = "",
    ) -> None:
        self.sync_calls.append({
            "user": user_content,
            "assistant": assistant_content,
            "session_id": session_id,
        })

    def queue_prefetch(self, query: str, *, session_id: str = "") -> None:
        self.queue_calls.append({"query": query, "session_id": session_id})


def _make_host(
    manager: MemoryManager | None = None,
    session_id: str = "init-session-id",
) -> ExtensionHost:
    host = ExtensionHost()
    host._initialized = True
    host._memory_manager = manager
    host._session_info = {
        "session_id": session_id,
        "model": "test-model",
        "provider": "test-provider",
        "platform": "cli",
        "profile_name": "default",
        "cwd": "/tmp",
    }
    return host


def _params(**overrides: Any) -> dict[str, Any]:
    params = {
        "new_session_id": "new-sid",
        "parent_session_id": "old-sid",
        "reset": False,
        "rewound": False,
        "reason": "compression",
    }
    params.update(overrides)
    return params


def test_session_switch_requires_initialized_host() -> None:
    with pytest.raises(RuntimeError, match="extension host is not initialized"):
        ExtensionHost().session_switch(_params())


def test_session_switch_validates_params_mapping() -> None:
    with pytest.raises(ValueError, match="requires a params mapping"):
        _make_host().session_switch("not-a-mapping")  # type: ignore[arg-type]


@pytest.mark.parametrize(
    "missing_key",
    ["new_session_id", "parent_session_id", "reset", "rewound", "reason"],
)
def test_session_switch_rejects_missing_fields(missing_key: str) -> None:
    params = _params()
    del params[missing_key]
    with pytest.raises(ValueError, match=f"session_switch.*{missing_key}"):
        _make_host().session_switch(params)


@pytest.mark.parametrize(
    "bad_value",
    [None, "", "   ", 123, 0, True, False, ["sid"], {"sid": "val"}],
)
def test_session_switch_rejects_invalid_new_session_id(bad_value: Any) -> None:
    with pytest.raises(ValueError, match="requires a nonempty new_session_id"):
        _make_host().session_switch(_params(new_session_id=bad_value))


@pytest.mark.parametrize(
    "bad_value",
    [None, 123, 0, True, False, ["old-sid"], {"parent": "val"}],
)
def test_session_switch_rejects_mistyped_parent_session_id(bad_value: Any) -> None:
    with pytest.raises(ValueError, match="parent_session_id must be a string"):
        _make_host().session_switch(_params(parent_session_id=bad_value))


def test_session_switch_accepts_empty_parent_session_id() -> None:
    host = _make_host()
    assert host.session_switch(_params(parent_session_id="")) is None
    assert host._session_info["session_id"] == "new-sid"


@pytest.mark.parametrize(
    "bad_value",
    [None, 1, 0, 2, -1, "true", "false", [True], {"reset": True}],
)
def test_session_switch_rejects_non_bool_reset(bad_value: Any) -> None:
    with pytest.raises(ValueError, match="reset must be a boolean"):
        _make_host().session_switch(_params(reset=bad_value))


@pytest.mark.parametrize(
    "bad_value",
    [None, 1, 0, 2, -1, "true", "false", [False], {"rewound": False}],
)
def test_session_switch_rejects_non_bool_rewound(bad_value: Any) -> None:
    with pytest.raises(ValueError, match="rewound must be a boolean"):
        _make_host().session_switch(_params(rewound=bad_value))


@pytest.mark.parametrize(
    "bad_value",
    [None, "", "   ", 123, 0, True, False, ["compression"], {"reason": "val"}],
)
def test_session_switch_rejects_invalid_reason(bad_value: Any) -> None:
    with pytest.raises(ValueError, match="requires a nonempty reason"):
        _make_host().session_switch(_params(reason=bad_value))


def test_session_switch_succeeds_without_memory_manager() -> None:
    host = _make_host(session_id="before-switch")
    assert host.session_switch(_params(new_session_id="after-switch")) is None
    assert host._session_info["session_id"] == "after-switch"


@pytest.mark.parametrize(
    ("old_id", "new_id"),
    [("session-parent", "session-child"), ("session-inplace", "session-inplace")],
)
def test_compression_switch_forwards_exact_provider_args(
    old_id: str,
    new_id: str,
) -> None:
    manager = MemoryManager()
    provider = _RecordingProvider()
    manager.add_provider(provider)
    host = _make_host(manager, old_id)

    assert (
        host.session_switch(_params(new_session_id=new_id, parent_session_id=old_id))
        is None
    )
    assert host._session_info["session_id"] == new_id
    assert provider.switch_calls == [
        {
            "new": new_id,
            "parent": old_id,
            "reset": False,
            "extra": {"reason": "compression"},
        }
    ]


def test_session_switch_forwards_true_rewound_only() -> None:
    manager = MemoryManager()
    provider = _RecordingProvider()
    manager.add_provider(provider)

    _make_host(manager).session_switch(
        _params(parent_session_id="", rewound=True, reason="undo")
    )
    assert provider.switch_calls == [
        {
            "new": "new-sid",
            "parent": "",
            "reset": False,
            "extra": {"reason": "undo", "rewound": True},
        }
    ]


def test_session_switch_forwards_reset_true() -> None:
    manager = MemoryManager()
    provider = _RecordingProvider()
    manager.add_provider(provider)

    _make_host(manager).session_switch(_params(reset=True, reason="reset"))
    assert provider.switch_calls == [
        {
            "new": "new-sid",
            "parent": "old-sid",
            "reset": True,
            "extra": {"reason": "reset"},
        }
    ]


def test_session_switch_isolates_individual_provider_failures() -> None:
    manager = MemoryManager()
    manager.add_provider(_RecordingProvider("builtin", should_raise=True))
    good = _RecordingProvider("hindsight")
    manager.add_provider(good)

    host = _make_host(manager, "sid-start")
    assert host.session_switch(_params(new_session_id="sid-next")) is None
    assert host._session_info["session_id"] == "sid-next"
    assert good.switch_calls[0]["new"] == "sid-next"


def test_switched_identity_reaches_subsequent_operations() -> None:
    manager = MemoryManager()
    provider = _RecordingProvider()
    manager.add_provider(provider)
    host = _make_host(manager, "sid-v1")
    host.session_switch(_params(new_session_id="sid-v2", parent_session_id="sid-v1"))

    host.turn_complete({
        "user_message": "can you write a complex script to analyze server logs?",
        "final_response": "response here",
    })
    manager.flush_pending()
    host.turn_start({
        "turn_number": 1,
        "user_message": "tell me a story about testing",
    })
    assert provider.sync_calls[0]["session_id"] == "sid-v2"
    assert provider.queue_calls[0]["session_id"] == "sid-v2"
    assert provider.prefetch_calls[0]["session_id"] == "sid-v2"

    plugin_manager = MagicMock()
    plugin_manager.render_system_prompt_sections.return_value = []
    host._plugin_manager = plugin_manager
    host.snapshot()
    session_info = plugin_manager.render_system_prompt_sections.call_args[0][0]
    assert session_info["session_id"] == "sid-v2"

    host._memory_tool_names = {"save_memory"}
    manager.handle_tool_call = MagicMock(return_value={"saved": True})  # type: ignore[assignment]
    assert host.call_tool({
        "name": "save_memory",
        "args": {"content": "remember this"},
    }) == {"saved": True}
    manager.handle_tool_call.assert_called_once_with(  # type: ignore[attr-defined]
        "save_memory",
        {"content": "remember this"},
        session_id="sid-v2",
    )


def test_subprocess_session_switch_jsonl_dispatch(tmp_path: Path) -> None:
    proc = subprocess.Popen(
        [sys.executable, "-m", "hermes_cli.rust_extension_host"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    assert proc.stdin is not None
    assert proc.stdout is not None

    def request(request_id: int, method: str, params: dict[str, Any]) -> dict[str, Any]:
        proc.stdin.write(
            json.dumps({
                "id": request_id,
                "method": method,
                "params": params,
            })
            + "\n"
        )
        proc.stdin.flush()
        return json.loads(proc.stdout.readline())

    try:
        assert request(1, "session_switch", _params()) == {
            "id": 1,
            "ok": False,
            "error": "extension host request failed",
        }
        initialized = request(
            2,
            "initialize",
            {
                "home": str(tmp_path),
                "session_id": "initial-sid",
                "model": "test-model",
                "provider": "test-provider",
                "platform": "cli",
                "profile_name": "default",
                "cwd": str(tmp_path),
            },
        )
        assert initialized["id"] == 2
        assert initialized["ok"] is True
        assert request(3, "session_switch", _params()) == {
            "id": 3,
            "ok": True,
            "result": None,
        }
        assert request(4, "session_switch", _params(reset=1)) == {
            "id": 4,
            "ok": False,
            "error": "extension host request failed",
        }
        missing_id = _params()
        del missing_id["new_session_id"]
        assert request(5, "session_switch", missing_id) == {
            "id": 5,
            "ok": False,
            "error": "extension host request failed",
        }
        assert request(6, "session_switch", _params(rewound=True, reason="undo")) == {
            "id": 6,
            "ok": True,
            "result": None,
        }
        assert request(7, "shutdown", {}) == {"id": 7, "ok": True, "result": None}
        proc.wait(timeout=5)
    finally:
        if proc.poll() is None:
            proc.kill()
