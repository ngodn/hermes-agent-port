"""Persistent compatibility host for Rust-native extension capabilities.

The Rust gateway owns model turns and prompt persistence. This child keeps
legacy Python plugins and one configured external-memory provider alive for a
single conversation, exposing only line-delimited JSON on its original stdout.
Plugin prints are redirected to stderr so they cannot corrupt the protocol.
"""

from __future__ import annotations

import json
import logging
import os
import sys
from pathlib import Path
from typing import Any, Mapping


_LOG = logging.getLogger(__name__)


def _string_list(value: Any) -> list[str] | None:
    if value is None:
        return None
    if not isinstance(value, list):
        return []
    return [item for item in value if isinstance(item, str)]


def _install_profile_secrets(value: Any) -> None:
    """Replace inherited profile settings with the caller's scoped snapshot."""
    if value is None:
        return
    if not isinstance(value, dict):
        raise ValueError("profile_secrets must be an object")

    # A multiplex gateway's process environment can contain another profile's
    # last-loaded values. Clear every Hermes-managed profile key before imports
    # can observe it, then install only the task-local snapshot sent by Rust.
    from agent.secret_scope import _is_global_env
    from hermes_cli.env_loader import _known_hermes_env_keys

    managed = _known_hermes_env_keys()
    credential_suffixes = (
        "_API_KEY",
        "_TOKEN",
        "_SECRET",
        "_KEY",
        "_PASSWORD",
        "_CREDENTIAL",
        "_CREDENTIALS",
    )
    for name in tuple(os.environ):
        upper = name.upper()
        if not _is_global_env(name) and (
            name in managed or upper.endswith(credential_suffixes)
        ):
            os.environ.pop(name, None)

    for name, secret in value.items():
        if not isinstance(name, str) or not isinstance(secret, str):
            raise ValueError("profile_secrets must map strings to strings")
        if not _is_global_env(name):
            os.environ[name] = secret


class ExtensionHost:
    def __init__(self) -> None:
        self._initialized = False
        self._home_token = None
        self._plugin_manager = None
        self._memory_manager = None
        self._session_info: dict[str, str] = {}
        self._plugin_tool_names: set[str] = set()
        self._memory_tool_names: set[str] = set()
        self._memory_exposed = False
        self._enabled_toolsets: list[str] | None = None
        self._disabled_toolsets: list[str] = []

    def initialize(self, params: Mapping[str, Any]) -> dict[str, Any]:
        if self._initialized:
            raise RuntimeError("extension host is already initialized")
        home = params.get("home")
        if not isinstance(home, str) or not home:
            raise ValueError("initialize requires a nonempty home")

        # The child is conversation-scoped, so an environment binding is safe
        # and gives legacy imports the same profile identity as the Rust owner.
        resolved_home = str(Path(home).resolve())
        os.environ["HERMES_HOME"] = resolved_home
        from hermes_constants import set_hermes_home_override

        self._home_token = set_hermes_home_override(resolved_home)
        profile_secrets = params.get("profile_secrets")
        _install_profile_secrets(profile_secrets)
        from hermes_cli.env_loader import load_hermes_dotenv

        load_hermes_dotenv(
            hermes_home=resolved_home,
            # A Rust task-local scope already contains the profile's hydrated
            # external-source snapshot. Avoid repeating secret-manager I/O in
            # every conversation child.
            load_external_secrets=profile_secrets is None,
        )
        if profile_secrets is not None:
            # A prepared Rust scope is authoritative. Dotenv is still loaded
            # for non-secret child settings, then scoped values win again.
            _install_profile_secrets(profile_secrets)
        # HERMES_HOME is the process identity, not profile-controlled config.
        # Restore it if an operator-edited .env contained a conflicting value.
        os.environ["HERMES_HOME"] = resolved_home
        cwd = params.get("cwd")
        if cwd:
            if not isinstance(cwd, str) or not Path(cwd).is_dir():
                raise ValueError("initialize cwd must be an existing directory")
            os.chdir(cwd)

        from hermes_cli.config import load_config_readonly
        from hermes_cli.plugins import get_plugin_manager
        from tools.registry import registry

        config = load_config_readonly() or {}
        native_tool_names = _string_list(params.get("native_tool_names")) or []
        for name in native_tool_names:
            if not name:
                continue
            registry.register(
                name=name,
                toolset="_rust_native_reserved",
                schema={"name": name, "parameters": {"type": "object"}},
                handler=lambda _args, **_kwargs: None,
            )
        self._plugin_manager = get_plugin_manager()
        self._plugin_manager.discover_and_load()

        self._session_info = {
            key: str(params.get(key) or "")
            for key in (
                "session_id",
                "model",
                "provider",
                "platform",
                "profile_name",
                "cwd",
            )
        }
        memory_config = config.get("memory")
        memory_config = memory_config if isinstance(memory_config, Mapping) else {}
        provider_name = memory_config.get("provider", "")
        provider_name = provider_name.strip() if isinstance(provider_name, str) else ""
        active_provider = ""
        if provider_name:
            from agent.memory_manager import MemoryManager
            from plugins.memory import load_memory_provider

            provider = load_memory_provider(provider_name)
            if provider is not None and provider.is_available():
                self._memory_manager = MemoryManager()
                self._memory_manager.add_provider(provider)
                init_kwargs = {
                    "session_id": self._session_info["session_id"],
                    "platform": self._session_info["platform"] or "cli",
                    "hermes_home": os.environ["HERMES_HOME"],
                    "agent_context": "primary",
                    "agent_identity": self._session_info["profile_name"] or "default",
                    "agent_workspace": "hermes",
                }
                for source, target in (
                    ("session_title", "session_title"),
                    ("user_id", "user_id"),
                    ("user_id_alt", "user_id_alt"),
                    ("user_name", "user_name"),
                    ("chat_id", "chat_id"),
                    ("chat_name", "chat_name"),
                    ("chat_type", "chat_type"),
                    ("thread_id", "thread_id"),
                    ("gateway_session_key", "gateway_session_key"),
                ):
                    value = params.get(source)
                    if value:
                        init_kwargs[target] = str(value)
                self._memory_manager.initialize_all(**init_kwargs)
                active_provider = provider_name
            elif provider is not None:
                try:
                    reason = provider.unavailable_reason()
                except Exception:
                    reason = ""
                _LOG.warning(
                    "Memory provider %r is unavailable%s",
                    provider_name,
                    f": {reason}" if reason else "",
                )
            else:
                _LOG.warning("Memory provider %r could not be loaded", provider_name)

        # Gateway sessions resolve platform_toolsets before constructing the
        # Python agent. Do the same here, after discovery so plugin-provided
        # toolsets participate in the resolver.
        from hermes_cli.tools_config import _get_platform_tools

        enabled = sorted(
            _get_platform_tools(config, self._session_info["platform"] or "cli")
        )
        agent_config = config.get("agent")
        agent_config = agent_config if isinstance(agent_config, Mapping) else {}
        disabled = _string_list(agent_config.get("disabled_toolsets")) or []
        self._enabled_toolsets = enabled
        self._disabled_toolsets = disabled

        # Use the real Python resolver and check_fn path, then retain only tools
        # owned by loaded plugins. Core tools remain native Rust responsibilities.
        from model_tools import get_tool_definitions

        plugin_names = set(self._plugin_manager._plugin_tool_names)
        resolved = get_tool_definitions(
            enabled_toolsets=enabled,
            disabled_toolsets=disabled,
            quiet_mode=True,
            skip_tool_search_assembly=True,
        )
        plugin_tools = [
            item
            for item in resolved
            if isinstance(item, dict)
            and isinstance(item.get("function"), dict)
            and item["function"].get("name") in plugin_names
        ]
        # Keep every registered handler routable. Rust may restore an
        # unavailable tool into a frozen conversation prefix after a transient
        # check_fn flap, matching Python's prefix-restoration behavior.
        self._plugin_tool_names = plugin_names
        from tools.registry import registry

        registered_plugin_tools = []
        for name in sorted(plugin_names):
            entry = registry.get_entry(name, scope=self._plugin_manager.scope_key)
            if entry is None:
                continue
            schema = dict(entry.schema)
            schema["name"] = entry.name
            registered_plugin_tools.append(
                {"type": "function", "function": schema}
            )

        memory_tools: list[dict[str, Any]] = []
        if self._memory_manager is not None:
            from agent.memory_manager import memory_provider_tools_enabled

            self._memory_exposed = memory_provider_tools_enabled(
                enabled,
                disabled,
                memory_tool_present=False,
            )
            if self._memory_exposed:
                for schema in self._memory_manager.get_all_tool_schemas():
                    memory_tools.append({"type": "function", "function": schema})
                self._memory_tool_names = {
                    item["function"]["name"] for item in memory_tools
                }

        self._initialized = True
        return {
            "active_memory_provider": active_provider or None,
            "registered_plugin_tools": registered_plugin_tools,
            "plugin_tools": plugin_tools,
            "memory_tools": memory_tools,
            "memory_exposed": self._memory_exposed,
        }

    def snapshot(self) -> dict[str, Any]:
        if not self._initialized:
            raise RuntimeError("extension host is not initialized")
        sections = self._plugin_manager.render_system_prompt_sections(self._session_info)
        plugin_sections = [
            {"id": section.id, "content": section.content} for section in sections
        ]
        memory_prompt = None
        if self._memory_manager is not None and self._memory_exposed:
            block = self._memory_manager.build_system_prompt()
            if isinstance(block, str) and block.strip():
                memory_prompt = block
        return {
            "plugin_sections": plugin_sections,
            "memory_prompt": memory_prompt,
        }

    def call_tool(self, params: Mapping[str, Any]) -> Any:
        if not self._initialized:
            raise RuntimeError("extension host is not initialized")
        name = params.get("name")
        args = params.get("args")
        if not isinstance(name, str) or not name:
            raise ValueError("call_tool requires a nonempty name")
        if not isinstance(args, dict):
            raise ValueError("call_tool args must be an object")
        if name in self._plugin_tool_names:
            # Drive the real dispatch seam so schema coercion, plugin hooks,
            # middleware, and result normalization match Python agent turns.
            from model_tools import handle_function_call

            return handle_function_call(
                name,
                args,
                session_id=self._session_info["session_id"],
                enabled_tools=sorted(self._plugin_tool_names | self._memory_tool_names),
                enabled_toolsets=self._enabled_toolsets,
                disabled_toolsets=self._disabled_toolsets,
            )
        if name in self._memory_tool_names:
            return self._memory_manager.handle_tool_call(
                name,
                args,
                session_id=self._session_info["session_id"],
            )
        raise ValueError(f"extension tool is not registered: {name}")

    def turn_start(self, params: Mapping[str, Any]) -> dict[str, Any]:
        """Prepare one model-bound user message with recalled memory."""
        if not self._initialized:
            raise RuntimeError("extension host is not initialized")
        manager = self._memory_manager
        if manager is None:
            return {"api_content": None, "recall_indicator": None}

        user_message = params.get("user_message")
        turn_number = params.get("turn_number")
        if not isinstance(turn_number, int) or isinstance(turn_number, bool):
            raise ValueError("turn_start requires an integer turn_number")
        query = user_message if isinstance(user_message, str) else ""

        try:
            manager.on_turn_start(turn_number, query)
        except Exception:
            _LOG.exception("Memory provider turn start failed (non-fatal)")

        recalled = ""
        from agent.memory_provider import is_trivial_prompt

        if not is_trivial_prompt(query):
            try:
                recalled = manager.prefetch_all(
                    query,
                    session_id=self._session_info["session_id"],
                ) or ""
            except Exception:
                _LOG.exception("Memory provider prefetch failed (non-fatal)")

        indicator = None
        if recalled:
            try:
                indicator = manager.describe_recall() or None
            except Exception:
                _LOG.exception("Memory provider recall status failed (non-fatal)")

        from agent.turn_context import compose_user_api_content

        return {
            "api_content": compose_user_api_content(user_message, recalled, ""),
            "recall_indicator": indicator,
        }

    def turn_complete(self, params: Mapping[str, Any]) -> None:
        """Queue a completed turn for external-memory persistence and warming."""
        if not self._initialized:
            raise RuntimeError("extension host is not initialized")
        manager = self._memory_manager
        if manager is None or params.get("interrupted"):
            return
        original = params.get("user_message")
        final_response = params.get("final_response")
        if not (original and final_response):
            return

        from agent.codex_responses_adapter import _summarize_user_message_for_log
        from agent.memory_provider import is_trivial_prompt

        user_text = _summarize_user_message_for_log(original, sep="\n")
        response_text = _summarize_user_message_for_log(final_response, sep="\n")
        if not (user_text and response_text):
            return
        messages = params.get("messages")
        sync_kwargs: dict[str, Any] = {
            "session_id": self._session_info["session_id"],
        }
        if isinstance(messages, list):
            sync_kwargs["messages"] = messages
        manager.sync_all(user_text, response_text, **sync_kwargs)
        if not is_trivial_prompt(user_text):
            manager.queue_prefetch_all(
                user_text,
                session_id=self._session_info["session_id"],
            )

    def session_switch(self, params: Mapping[str, Any]) -> None:
        """Switch the conversation-scoped session identity and notify memory."""
        if not self._initialized:
            raise RuntimeError("extension host is not initialized")

        if not isinstance(params, Mapping):
            raise ValueError("session_switch requires a params mapping")

        if "new_session_id" not in params:
            raise ValueError("session_switch requires a nonempty new_session_id")
        new_session_id = params["new_session_id"]
        if not isinstance(new_session_id, str) or not new_session_id.strip():
            raise ValueError("session_switch requires a nonempty new_session_id")

        if "parent_session_id" not in params:
            raise ValueError("session_switch requires parent_session_id")
        parent_session_id = params["parent_session_id"]
        if not isinstance(parent_session_id, str):
            raise ValueError("session_switch parent_session_id must be a string")

        if "reset" not in params:
            raise ValueError("session_switch reset must be a boolean")
        reset = params["reset"]
        if type(reset) is not bool:
            raise ValueError("session_switch reset must be a boolean")

        if "rewound" not in params:
            raise ValueError("session_switch rewound must be a boolean")
        rewound = params["rewound"]
        if type(rewound) is not bool:
            raise ValueError("session_switch rewound must be a boolean")

        if "reason" not in params:
            raise ValueError("session_switch requires a nonempty reason")
        reason = params["reason"]
        if not isinstance(reason, str) or not reason.strip():
            raise ValueError("session_switch requires a nonempty reason")

        self._session_info["session_id"] = new_session_id

        if self._memory_manager is not None:
            switch_kwargs: dict[str, Any] = {
                "parent_session_id": parent_session_id,
                "reset": reset,
                "reason": reason,
            }
            if rewound:
                switch_kwargs["rewound"] = True
            self._memory_manager.on_session_switch(
                new_session_id,
                **switch_kwargs,
            )
        return None

    def session_end(self, params: Mapping[str, Any]) -> None:
        """Commit end-of-session provider state without unloading the host."""
        if not self._initialized:
            raise RuntimeError("extension host is not initialized")
        messages = params.get("messages")
        if not isinstance(messages, list):
            raise ValueError("session_end requires a messages array")
        if self._memory_manager is not None:
            self._memory_manager.on_session_end(messages)

    def flush_pending(self, params: Mapping[str, Any]) -> bool:
        """Wait behind the memory manager's serialized background queue."""
        if not self._initialized:
            raise RuntimeError("extension host is not initialized")
        if self._memory_manager is None:
            return True
        timeout = params.get("timeout")
        if timeout is not None and not isinstance(timeout, (int, float)):
            raise ValueError("flush_pending timeout must be numeric")
        return self._memory_manager.flush_pending(timeout=timeout)

    def pre_compress(self, params: Mapping[str, Any]) -> dict[str, Any]:
        """Execute pre-compression memory checkpointing and collect context."""
        if not self._initialized:
            raise RuntimeError("extension host is not initialized")

        messages = params.get("messages")
        if not isinstance(messages, list):
            raise ValueError("pre_compress requires a messages list")

        require_checkpoint = params.get("require_checkpoint")
        if not isinstance(require_checkpoint, bool):
            raise ValueError("pre_compress require_checkpoint must be a boolean")

        checkpoint_api_version = params.get("checkpoint_api_version")
        if (
            isinstance(checkpoint_api_version, bool)
            or not isinstance(checkpoint_api_version, int)
            or checkpoint_api_version <= 0
        ):
            raise ValueError(
                "pre_compress checkpoint_api_version must be a positive integer"
            )

        manager = self._memory_manager
        if manager is None:
            if require_checkpoint:
                raise RuntimeError(
                    "pre_compress checkpoint required but no memory manager is active"
                )
            return {
                "checkpoint_supported": False,
                "memory_context": None,
            }

        probe_fn = getattr(manager, "supports_pre_compress_checkpoint", None)
        supported = False
        if callable(probe_fn):
            try:
                supported = bool(probe_fn(checkpoint_api_version))
            except Exception as exc:
                if require_checkpoint:
                    _LOG.warning(
                        "Memory provider checkpoint capability probe failed (%s)",
                        type(exc).__name__,
                    )
                    raise RuntimeError(
                        "pre_compress checkpoint capability probe failed"
                    ) from None
                _LOG.warning(
                    "Memory provider checkpoint capability probe failed (non-fatal, %s)",
                    type(exc).__name__,
                )
                return {
                    "checkpoint_supported": False,
                    "memory_context": None,
                }
        elif require_checkpoint:
            raise RuntimeError(
                "memory manager does not implement supports_pre_compress_checkpoint"
            )

        if require_checkpoint and not supported:
            raise RuntimeError(
                f"pre_compress checkpoint required but active provider does not support API v{checkpoint_api_version}"
            )

        from agent.conversation_compression import (
            _direct_messages_for_pre_compress_memory,
        )

        evidence_messages = _direct_messages_for_pre_compress_memory(messages)

        if require_checkpoint:
            try:
                raw_context = manager.on_pre_compress(
                    messages,
                    evidence_messages=evidence_messages,
                    require_checkpoint=True,
                    checkpoint_api_version=checkpoint_api_version,
                )
            except Exception as exc:
                _LOG.warning("Pre-compress checkpoint failed (%s)", type(exc).__name__)
                raise RuntimeError("pre_compress checkpoint failed") from None
            checkpoint_supported = True
        else:
            try:
                raw_context = manager.on_pre_compress(
                    messages,
                    evidence_messages=evidence_messages,
                    require_checkpoint=False,
                    checkpoint_api_version=checkpoint_api_version,
                )
            except Exception:
                _LOG.exception("Memory provider on_pre_compress failed (non-fatal)")
                return {
                    "checkpoint_supported": False,
                    "memory_context": None,
                }
            checkpoint_supported = supported

        memory_context = None
        if isinstance(raw_context, str) and raw_context.strip():
            from agent.context_engine import sanitize_memory_context

            sanitized = sanitize_memory_context(raw_context)
            if sanitized and sanitized.strip():
                memory_context = sanitized

        return {
            "checkpoint_supported": checkpoint_supported,
            "memory_context": memory_context,
        }

    def shutdown(self) -> None:
        if self._memory_manager is not None:
            self._memory_manager.shutdown_all()
            self._memory_manager = None
        if self._plugin_manager is not None:
            self._plugin_manager.unload()
            self._plugin_manager = None
        if self._home_token is not None:
            from hermes_constants import reset_hermes_home_override

            reset_hermes_home_override(self._home_token)
            self._home_token = None


def _response(request_id: Any, *, result: Any = None, error: str | None = None) -> dict:
    if error is not None:
        return {"id": request_id, "ok": False, "error": error}
    return {"id": request_id, "ok": True, "result": result}


def main() -> int:
    # Keep a private duplicate of the protocol pipe, then redirect both Python
    # and C-level writes on fd 1 to stderr before any plugin import can happen.
    protocol_fd = os.dup(sys.stdout.fileno())
    protocol_stdout = os.fdopen(
        protocol_fd, "w", buffering=1, encoding="utf-8", closefd=True
    )
    os.dup2(sys.stderr.fileno(), sys.stdout.fileno())
    sys.stdout = sys.stderr
    host = ExtensionHost()
    try:
        for raw_line in sys.stdin:
            request_id = None
            stop = False
            try:
                request = json.loads(raw_line)
                if not isinstance(request, dict):
                    raise ValueError("request must be an object")
                request_id = request.get("id")
                method = request.get("method")
                params = request.get("params") or {}
                if not isinstance(params, dict):
                    raise ValueError("params must be an object")
                if method == "initialize":
                    result = host.initialize(params)
                elif method == "snapshot":
                    result = host.snapshot()
                elif method == "call_tool":
                    result = host.call_tool(params)
                elif method == "turn_start":
                    result = host.turn_start(params)
                elif method == "turn_complete":
                    result = host.turn_complete(params)
                elif method == "session_switch":
                    result = host.session_switch(params)
                elif method == "session_end":
                    result = host.session_end(params)
                elif method == "flush_pending":
                    result = host.flush_pending(params)
                elif method == "pre_compress":
                    result = host.pre_compress(params)
                elif method == "shutdown":
                    host.shutdown()
                    result = None
                    stop = True
                else:
                    raise ValueError("unknown extension host method")
                response = _response(request_id, result=result)
            except Exception:
                _LOG.exception("Extension host request failed")
                response = _response(request_id, error="extension host request failed")
            protocol_stdout.write(json.dumps(response, ensure_ascii=False) + "\n")
            protocol_stdout.flush()
            if stop:
                return 0
    finally:
        try:
            host.shutdown()
        except Exception:
            _LOG.exception("Extension host shutdown failed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
