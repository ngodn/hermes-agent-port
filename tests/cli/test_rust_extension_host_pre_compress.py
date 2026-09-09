"""Focused unit and protocol tests for ExtensionHost.pre_compress."""

from __future__ import annotations

import json
import subprocess
import sys
from typing import Any
from unittest.mock import MagicMock

import pytest

from agent.context_compressor import COMPRESSED_SUMMARY_METADATA_KEY
from agent.memory_manager import MemoryManager
from agent.memory_provider import (
    PRE_COMPRESS_CHECKPOINT_API_VERSION,
    MemoryProvider,
)
from hermes_cli.rust_extension_host import ExtensionHost


class _StubV1Provider(MemoryProvider):
    pre_compress_checkpoint_api_version = 1

    def __init__(
        self, name: str = "legacy_v1", return_context: str = "legacy context"
    ) -> None:
        self._name = name
        self.return_context = return_context
        self.calls: list[Any] = []

    @property
    def name(self) -> str:
        return self._name

    def is_available(self) -> bool:
        return True

    def initialize(self, session_id: str, **kwargs: Any) -> None:
        pass

    def get_tool_schemas(self) -> list[dict[str, Any]]:
        return []

    def on_pre_compress(self, messages: Any) -> str:
        self.calls.append(messages)
        return self.return_context


class _StubV2Provider(MemoryProvider):
    pre_compress_checkpoint_api_version = PRE_COMPRESS_CHECKPOINT_API_VERSION

    def __init__(
        self, name: str = "checkpoint_v2", return_context: str = "v2 context"
    ) -> None:
        self._name = name
        self.return_context = return_context
        self.calls: list[Any] = []
        self.require_checkpoint_calls: list[bool] = []

    @property
    def name(self) -> str:
        return self._name

    def is_available(self) -> bool:
        return True

    def initialize(self, session_id: str, **kwargs: Any) -> None:
        pass

    def get_tool_schemas(self) -> list[dict[str, Any]]:
        return []

    def on_pre_compress(
        self, messages: Any, *, require_checkpoint: bool = False
    ) -> str:
        self.calls.append(messages)
        self.require_checkpoint_calls.append(require_checkpoint)
        return self.return_context


class _FailingV2Provider(_StubV2Provider):
    def __init__(
        self, name: str = "failing_v2", fail_on_required_only: bool = True
    ) -> None:
        super().__init__(name=name)
        self.fail_on_required_only = fail_on_required_only

    def on_pre_compress(
        self, messages: Any, *, require_checkpoint: bool = False
    ) -> str:
        self.require_checkpoint_calls.append(require_checkpoint)
        if require_checkpoint or not self.fail_on_required_only:
            raise RuntimeError(f"provider {self._name} persistence failed")
        return ""


def _make_host(manager: MemoryManager | None = None) -> ExtensionHost:
    host = ExtensionHost()
    host._initialized = True
    host._memory_manager = manager
    return host


# ---------------------------------------------------------------------------
# 1. Validation and malformed params
# ---------------------------------------------------------------------------


def test_pre_compress_requires_initialized_host() -> None:
    host = ExtensionHost()
    with pytest.raises(RuntimeError, match="extension host is not initialized"):
        host.pre_compress({
            "messages": [],
            "require_checkpoint": False,
            "checkpoint_api_version": 2,
        })


@pytest.mark.parametrize("bad_messages", [None, "invalid", {"role": "user"}, 123, 4.5])
def test_pre_compress_validates_messages_is_list(bad_messages: Any) -> None:
    host = _make_host()
    params: dict[str, Any] = {
        "require_checkpoint": False,
        "checkpoint_api_version": 2,
    }
    if bad_messages is not None:
        params["messages"] = bad_messages

    with pytest.raises(ValueError, match="pre_compress requires a messages list"):
        host.pre_compress(params)


@pytest.mark.parametrize(
    "bad_require", [None, 1, 0, "true", "false", [True], {"a": True}]
)
def test_pre_compress_validates_require_checkpoint_is_strict_bool(
    bad_require: Any,
) -> None:
    host = _make_host()
    params: dict[str, Any] = {
        "messages": [],
        "checkpoint_api_version": 2,
    }
    if bad_require is not None:
        params["require_checkpoint"] = bad_require

    with pytest.raises(
        ValueError, match="pre_compress require_checkpoint must be a boolean"
    ):
        host.pre_compress(params)


@pytest.mark.parametrize("bad_version", [None, True, False, 0, -1, -5, "2", 2.0, [2]])
def test_pre_compress_validates_checkpoint_api_version_is_positive_int(
    bad_version: Any,
) -> None:
    host = _make_host()
    params: dict[str, Any] = {
        "messages": [],
        "require_checkpoint": False,
    }
    if bad_version is not None:
        params["checkpoint_api_version"] = bad_version

    with pytest.raises(
        ValueError,
        match="pre_compress checkpoint_api_version must be a positive integer",
    ):
        host.pre_compress(params)


# ---------------------------------------------------------------------------
# 2. Exact raw-vs-evidence routing
# ---------------------------------------------------------------------------


def test_pre_compress_routes_direct_evidence_to_v2_checkpoint_provider() -> None:
    provider = _StubV2Provider()
    mgr = MemoryManager()
    mgr.add_provider(provider)
    host = _make_host(mgr)

    raw_messages = [
        {"role": "system", "content": "system prompt"},
        {"role": "user", "content": "user request"},
        {
            "role": "assistant",
            "content": "assistant explanation",
            "tool_calls": [{"id": "call_1", "function": {"name": "terminal"}}],
        },
        {
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "call_2", "function": {"name": "terminal"}}],
        },
        {
            "role": "assistant",
            "content": "   \n\t  ",
            "tool_calls": [{"id": "call_3", "function": {"name": "terminal"}}],
        },
        {"role": "tool", "content": "tool result", "tool_call_id": "call_1"},
        {
            "role": "assistant",
            "content": "prior compaction summary",
            COMPRESSED_SUMMARY_METADATA_KEY: True,
        },
        "malformed_non_dict_message",
    ]

    result = host.pre_compress({
        "messages": raw_messages,
        "require_checkpoint": True,
        "checkpoint_api_version": 2,
    })

    assert result["checkpoint_supported"] is True
    assert result["memory_context"] == "v2 context"

    # Provider should have received only direct user/assistant evidence:
    # 1. user request
    # 2. assistant explanation (with tool_calls stripped)
    assert len(provider.calls) == 1
    evidence = provider.calls[0]
    assert evidence == [
        {"role": "user", "content": "user request"},
        {"role": "assistant", "content": "assistant explanation"},
    ]

    # Verify original raw messages were not mutated
    assert raw_messages[2]["tool_calls"] == [
        {"id": "call_1", "function": {"name": "terminal"}}
    ]


def test_pre_compress_routes_raw_messages_to_legacy_v1_provider() -> None:
    provider = _StubV1Provider()
    mgr = MemoryManager()
    mgr.add_provider(provider)
    host = _make_host(mgr)

    raw_messages = [
        {"role": "system", "content": "system prompt"},
        {"role": "user", "content": "user request"},
        {"role": "tool", "content": "tool result", "tool_call_id": "call_1"},
    ]

    result = host.pre_compress({
        "messages": raw_messages,
        "require_checkpoint": False,
        "checkpoint_api_version": 2,
    })

    assert result["checkpoint_supported"] is False
    assert result["memory_context"] == "legacy context"

    assert len(provider.calls) == 1
    # Legacy provider must receive the raw unnormalized messages
    assert provider.calls[0] == raw_messages


# ---------------------------------------------------------------------------
# 3. Required supported success
# ---------------------------------------------------------------------------


def test_pre_compress_required_supported_success() -> None:
    provider = _StubV2Provider(return_context="  key facts saved  ")
    mgr = MemoryManager()
    mgr.add_provider(provider)
    host = _make_host(mgr)

    messages = [{"role": "user", "content": "persist this"}]
    result = host.pre_compress({
        "messages": messages,
        "require_checkpoint": True,
        "checkpoint_api_version": 2,
    })

    assert result == {
        "checkpoint_supported": True,
        "memory_context": "key facts saved",
    }
    assert provider.require_checkpoint_calls == [True]


def test_pre_compress_required_supported_success_empty_context() -> None:
    provider = _StubV2Provider(return_context="")
    mgr = MemoryManager()
    mgr.add_provider(provider)
    host = _make_host(mgr)

    result = host.pre_compress({
        "messages": [{"role": "user", "content": "persist this"}],
        "require_checkpoint": True,
        "checkpoint_api_version": 2,
    })

    assert result == {
        "checkpoint_supported": True,
        "memory_context": None,
    }
    assert provider.require_checkpoint_calls == [True]


# ---------------------------------------------------------------------------
# 4. Required unsupported failure (fail closed)
# ---------------------------------------------------------------------------


def test_pre_compress_required_fails_when_no_memory_manager() -> None:
    host = _make_host(manager=None)
    with pytest.raises(RuntimeError, match="no memory manager is active"):
        host.pre_compress({
            "messages": [],
            "require_checkpoint": True,
            "checkpoint_api_version": 2,
        })


def test_pre_compress_required_fails_when_active_provider_is_legacy() -> None:
    mgr = MemoryManager()
    mgr.add_provider(_StubV1Provider())
    host = _make_host(mgr)

    with pytest.raises(RuntimeError, match="does not support API v2"):
        host.pre_compress({
            "messages": [],
            "require_checkpoint": True,
            "checkpoint_api_version": 2,
        })


def test_pre_compress_required_fails_when_probe_raises() -> None:
    mgr = MemoryManager()
    mgr.supports_pre_compress_checkpoint = MagicMock(
        side_effect=RuntimeError("probe exploded")
    )
    host = _make_host(mgr)

    with pytest.raises(RuntimeError, match="checkpoint capability probe failed"):
        host.pre_compress({
            "messages": [],
            "require_checkpoint": True,
            "checkpoint_api_version": 2,
        })


def test_pre_compress_required_fails_when_provider_callback_raises() -> None:
    mgr = MemoryManager()
    mgr.add_provider(_FailingV2Provider(fail_on_required_only=True))
    host = _make_host(mgr)

    with pytest.raises(RuntimeError, match="pre_compress checkpoint failed"):
        host.pre_compress({
            "messages": [{"role": "user", "content": "save"}],
            "require_checkpoint": True,
            "checkpoint_api_version": 2,
        })


# ---------------------------------------------------------------------------
# 5. Optional legacy behavior and best-effort failures
# ---------------------------------------------------------------------------


def test_pre_compress_optional_legacy_provider_success() -> None:
    mgr = MemoryManager()
    mgr.add_provider(_StubV1Provider(return_context="remembered facts"))
    host = _make_host(mgr)

    result = host.pre_compress({
        "messages": [{"role": "user", "content": "hello"}],
        "require_checkpoint": False,
        "checkpoint_api_version": 2,
    })

    assert result == {
        "checkpoint_supported": False,
        "memory_context": "remembered facts",
    }


def test_pre_compress_optional_no_memory_manager() -> None:
    host = _make_host(manager=None)
    result = host.pre_compress({
        "messages": [],
        "require_checkpoint": False,
        "checkpoint_api_version": 2,
    })

    assert result == {
        "checkpoint_supported": False,
        "memory_context": None,
    }


def test_pre_compress_optional_probe_failure_is_best_effort() -> None:
    mgr = MemoryManager()
    mgr.supports_pre_compress_checkpoint = MagicMock(
        side_effect=RuntimeError("probe failure")
    )
    host = _make_host(mgr)

    result = host.pre_compress({
        "messages": [],
        "require_checkpoint": False,
        "checkpoint_api_version": 2,
    })

    assert result == {
        "checkpoint_supported": False,
        "memory_context": None,
    }


def test_pre_compress_optional_provider_failure_is_best_effort() -> None:
    # When a provider throws inside MemoryManager.on_pre_compress during optional mode,
    # MemoryManager catches it and returns empty string.
    mgr = MemoryManager()
    mgr.add_provider(_FailingV2Provider(fail_on_required_only=False))
    host = _make_host(mgr)

    result = host.pre_compress({
        "messages": [{"role": "user", "content": "save"}],
        "require_checkpoint": False,
        "checkpoint_api_version": 2,
    })

    assert result == {
        "checkpoint_supported": True,
        "memory_context": None,
    }


def test_pre_compress_optional_manager_callback_raises_is_best_effort() -> None:
    # If the manager call itself raises an exception, best-effort catches it and returns no context.
    mgr = MemoryManager()
    mgr.on_pre_compress = MagicMock(side_effect=RuntimeError("manager failure"))
    host = _make_host(mgr)

    result = host.pre_compress({
        "messages": [{"role": "user", "content": "save"}],
        "require_checkpoint": False,
        "checkpoint_api_version": 2,
    })

    assert result == {
        "checkpoint_supported": False,
        "memory_context": None,
    }


# ---------------------------------------------------------------------------
# 6. Context sanitization
# ---------------------------------------------------------------------------


def test_pre_compress_sanitizes_credentials() -> None:
    mgr = MemoryManager()
    raw_context = "API credentials: https://admin:supersecret@db.internal:5432/main"
    mgr.add_provider(_StubV2Provider(return_context=raw_context))
    host = _make_host(mgr)

    result = host.pre_compress({
        "messages": [],
        "require_checkpoint": True,
        "checkpoint_api_version": 2,
    })

    assert result["checkpoint_supported"] is True
    assert "supersecret" not in result["memory_context"]
    assert "https://admin:***@db.internal:5432/main" in result["memory_context"]


@pytest.mark.parametrize("blank_context", ["", "   ", "\n\t  \r\n"])
def test_pre_compress_sanitizes_blank_string_to_null(blank_context: str) -> None:
    mgr = MemoryManager()
    mgr.add_provider(_StubV2Provider(return_context=blank_context))
    host = _make_host(mgr)

    result = host.pre_compress({
        "messages": [],
        "require_checkpoint": True,
        "checkpoint_api_version": 2,
    })

    assert result == {
        "checkpoint_supported": True,
        "memory_context": None,
    }


def test_pre_compress_sanitizes_non_string_to_null() -> None:
    mgr = MemoryManager()
    mgr.supports_pre_compress_checkpoint = MagicMock(return_value=True)
    mgr.on_pre_compress = MagicMock(return_value=12345)
    host = _make_host(mgr)

    result = host.pre_compress({
        "messages": [],
        "require_checkpoint": True,
        "checkpoint_api_version": 2,
    })

    assert result == {
        "checkpoint_supported": True,
        "memory_context": None,
    }


def test_pre_compress_sanitizes_oversized_context() -> None:
    mgr = MemoryManager()
    long_text = "important memory context " * 1000
    mgr.add_provider(_StubV2Provider(return_context=long_text))
    host = _make_host(mgr)

    result = host.pre_compress({
        "messages": [],
        "require_checkpoint": True,
        "checkpoint_api_version": 2,
    })

    assert result["checkpoint_supported"] is True
    assert result["memory_context"] is not None
    assert "...[memory provider context truncated]..." in result["memory_context"]
    assert len(result["memory_context"]) <= 8000


# ---------------------------------------------------------------------------
# 7. Subprocess JSONL dispatcher branch
# ---------------------------------------------------------------------------


def test_pre_compress_dispatcher_protocol() -> None:
    proc = subprocess.Popen(
        [sys.executable, "-m", "hermes_cli.rust_extension_host"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )

    def request(req_id: int, method: str, params: dict[str, Any]) -> dict[str, Any]:
        assert proc.stdin is not None
        assert proc.stdout is not None
        proc.stdin.write(
            json.dumps({"id": req_id, "method": method, "params": params}) + "\n"
        )
        proc.stdin.flush()
        line = proc.stdout.readline()
        return json.loads(line)

    try:
        # Uninitialized call fails with protocol error (ok=False)
        resp1 = request(
            1,
            "pre_compress",
            {
                "messages": [],
                "require_checkpoint": False,
                "checkpoint_api_version": 2,
            },
        )
        assert resp1 == {"id": 1, "ok": False, "error": "extension host request failed"}

        # Initialize with no configured memory provider
        init_resp = request(
            2,
            "initialize",
            {
                "home": "/tmp",
                "session_id": "test_pre_compress_session",
                "model": "test-model",
                "provider": "test-provider",
                "platform": "cli",
                "profile_name": "default",
                "cwd": "/tmp",
            },
        )
        assert init_resp["id"] == 2
        assert init_resp["ok"] is True

        # Optional pre_compress with no memory provider succeeds with supported=False, context=null
        resp2 = request(
            3,
            "pre_compress",
            {
                "messages": [],
                "require_checkpoint": False,
                "checkpoint_api_version": 2,
            },
        )
        assert resp2 == {
            "id": 3,
            "ok": True,
            "result": {
                "checkpoint_supported": False,
                "memory_context": None,
            },
        }

        # Required pre_compress with no memory provider fails closed
        resp3 = request(
            4,
            "pre_compress",
            {
                "messages": [],
                "require_checkpoint": True,
                "checkpoint_api_version": 2,
            },
        )
        assert resp3 == {"id": 4, "ok": False, "error": "extension host request failed"}

        # Malformed params (e.g. messages not a list) fails closed
        resp4 = request(
            5,
            "pre_compress",
            {
                "messages": "invalid",
                "require_checkpoint": False,
                "checkpoint_api_version": 2,
            },
        )
        assert resp4 == {"id": 5, "ok": False, "error": "extension host request failed"}

        # Clean shutdown
        shutdown_resp = request(6, "shutdown", {})
        assert shutdown_resp == {"id": 6, "ok": True, "result": None}
        proc.wait(timeout=5)
    finally:
        if proc.poll() is None:
            proc.kill()
