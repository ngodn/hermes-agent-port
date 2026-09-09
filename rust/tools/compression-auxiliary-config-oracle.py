#!/usr/bin/env python3
"""Source-executed oracle for full-compression auxiliary configuration.

This driver executes the REAL Python decision functions that own four
decisions on the full context-compression summary path. It never
reimplements the logic; it patches only the config loader, environment,
model-metadata seams, and pure client-construction helpers needed for
deterministic, offline execution, then records the genuine outputs.

Sections (see the companion report for citations):

  1. task_config_resolution  ``auxiliary_client._resolve_task_provider_model``
     resolves ``auxiliary.compression`` provider/model/base_url/api_key/api_mode
     and their precedence (explicit arg > config > auto).

  2. summary_output_cap
       fast_lane       ``auxiliary_client.resolve_compression_fast_lane``
                       decides whether a configured max-output cap is honored.
       wire_forwarding ``auxiliary_client._build_call_kwargs`` decides whether a
                       cap is forwarded on the wire and under which model-specific
                       parameter name (``max_tokens`` vs ``max_completion_tokens``).

  3. summary_temperature  ``auxiliary_client._build_call_kwargs`` +
     ``_fixed_temperature_for_model`` decide omit / fixed / default temperature.

  4. main_model_fallback  the inline exception classifier and the two one-shot
     main-model fallback predicates inside
     ``ContextCompressor._generate_summary`` (extracted verbatim via AST, executed
     against synthetic exceptions and compressor state).

Usage:
    compression-auxiliary-config-oracle.py            # (re)write the corpus
    compression-auxiliary-config-oracle.py --check    # regenerate and compare
"""
from __future__ import annotations

import ast
import contextlib
import json
import os
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/compression-auxiliary-config-goldens.json"

sys.path.insert(0, str(ROOT))

import agent.auxiliary_client as ax  # noqa: E402
import agent.context_compressor as cc  # noqa: E402
import hermes_cli.config as hcfg  # noqa: E402


# ---------------------------------------------------------------------------
# Patch helpers (filesystem / env / metadata / construction seams only)
# ---------------------------------------------------------------------------
@contextlib.contextmanager
def patched_config(aux_compression: Optional[Dict[str, Any]], env: Optional[Dict[str, str]] = None):
    """Feed a deterministic ``auxiliary.compression`` config and env.

    Patches only ``hermes_cli.config.load_config_readonly`` (the filesystem
    read that ``_get_auxiliary_task_config`` performs) and ``os.environ`` (the
    ``_scoped_key_env`` fallback used off the profile secret scope). No decision
    logic is replaced.
    """
    if aux_compression is None:
        cfg: Dict[str, Any] = {}
    else:
        cfg = {"auxiliary": {"compression": aux_compression}}
    orig_loader = hcfg.load_config_readonly
    hcfg.load_config_readonly = lambda *a, **k: cfg
    saved_env = dict(os.environ)
    try:
        if env:
            os.environ.update(env)
        yield
    finally:
        hcfg.load_config_readonly = orig_loader
        os.environ.clear()
        os.environ.update(saved_env)


@contextlib.contextmanager
def patched_route_seams(custom_base: str = ""):
    """Pin the pure client-construction seams that read process/credential state.

    ``_build_call_kwargs`` and ``auxiliary_max_tokens_param`` consult the active
    custom base URL, the OpenRouter key, and Nous auth to pick the wire param.
    Force them to fixed, offline values so the recorded parameter name reflects
    only the model/provider/base_url inputs under test.
    """
    orig_base = ax._current_custom_base_url
    orig_nous = ax._read_nous_auth
    ax._current_custom_base_url = lambda: custom_base
    ax._read_nous_auth = lambda: None
    saved = os.environ.pop("OPENROUTER_API_KEY", None)
    try:
        yield
    finally:
        ax._current_custom_base_url = orig_base
        ax._read_nous_auth = orig_nous
        if saved is not None:
            os.environ["OPENROUTER_API_KEY"] = saved


# ---------------------------------------------------------------------------
# Section 1: auxiliary.compression provider/model/base_url/api_mode/credentials
# ---------------------------------------------------------------------------
def section_task_config_resolution() -> List[Dict[str, Any]]:
    fields = ("provider", "model", "base_url", "api_key", "api_mode")

    def resolve(config, env=None, args=None):
        args = args or {}
        with patched_config(config, env):
            tup = ax._resolve_task_provider_model(task="compression", **args)
        return dict(zip(fields, tup))

    cases = [
        # name, config, env, explicit args
        ("absent_config_auto", None, None, None),
        ("empty_config_auto", {}, None, None),
        ("provider_and_model", {"provider": "openrouter", "model": "x/y"}, None, None),
        ("first_class_provider_with_base_url",
         {"provider": "anthropic", "model": "claude-x", "base_url": "https://relay/v1"}, None, None),
        ("bare_base_url_no_key_falls_to_auto",
         {"base_url": "https://endpoint/v1"}, None, None),
        ("base_url_and_api_key_custom",
         {"base_url": "https://endpoint/v1", "api_key": "sk-inline"}, None, None),
        ("base_url_with_provider_no_key_keeps_provider",
         {"provider": "openrouter", "base_url": "https://or/v1"}, None, None),
        ("key_env_resolves_from_environment",
         {"provider": "openrouter", "model": "m", "key_env": "AUX_KEY"}, {"AUX_KEY": "sk-env"}, None),
        ("api_key_env_alias_resolves",
         {"provider": "openrouter", "model": "m", "api_key_env": "AUX_KEY2"}, {"AUX_KEY2": "sk-env2"}, None),
        ("key_env_missing_fails_to_none",
         {"provider": "openrouter", "model": "m", "key_env": "AUX_MISSING"}, None, None),
        ("api_key_wins_over_key_env",
         {"provider": "openrouter", "api_key": "sk-direct", "key_env": "AUX_KEY"}, {"AUX_KEY": "sk-env"}, None),
        ("model_auto_normalized_to_none",
         {"provider": "anthropic", "model": "auto"}, None, None),
        ("provider_auto_falls_through",
         {"provider": "auto", "model": "m"}, None, None),
        ("api_mode_passthrough_anthropic",
         {"base_url": "https://relay/v1", "api_key": "k", "api_mode": "anthropic_messages"}, None, None),
        ("api_mode_codex_responses",
         {"base_url": "https://relay/v1", "api_key": "k", "api_mode": "codex_responses"}, None, None),
        ("whitespace_values_treated_absent",
         {"provider": "  ", "model": "  ", "base_url": "  "}, None, None),
        ("direct_api_alias_openai_becomes_custom",
         {"provider": "openai", "model": "gpt-4o"}, None, None),
        ("direct_api_alias_openai_keeps_user_base",
         {"provider": "openai", "model": "gpt-4o", "base_url": "https://proxy/v1"}, None, None),
        ("explicit_provider_arg_overrides_config",
         {"provider": "openrouter", "model": "cfg/model"}, None, {"provider": "anthropic", "model": "arg/model"}),
        ("explicit_provider_arg_adopts_config_base",
         {"provider": "openrouter", "base_url": "https://or/v1", "api_key": "cfgk"}, None, {"provider": "openrouter"}),
    ]

    rows = []
    for name, config, env, args in cases:
        rows.append({
            "case": name,
            "config": config,
            "env": env,
            "explicit_args": args,
            "resolved": resolve(config, env, args),
        })
    return rows


# ---------------------------------------------------------------------------
# Section 2a: summary output-cap certification (fast lane)
# ---------------------------------------------------------------------------
def section_fast_lane() -> List[Dict[str, Any]]:
    def run(config, actual_provider, actual_model, req_provider=None, req_model=None):
        fl = ax.resolve_compression_fast_lane(
            actual_provider,
            actual_model,
            requested_provider=req_provider,
            requested_model=req_model,
            route_config=config,
        )
        return {
            "certified_non_reasoning": fl.certified_non_reasoning,
            "max_tokens": fl.max_tokens,
            "reasoning_config": fl.reasoning_config,
        }

    cases = [
        ("certified_cap_applied",
         {"provider": "openrouter", "model": "m", "reasoning_effort": "none", "max_output_tokens": 2048},
         "openrouter", "m", None, None),
        ("model_drift_uncapped",
         {"provider": "openrouter", "model": "m", "reasoning_effort": "none", "max_output_tokens": 2048},
         "openrouter", "other", None, None),
        ("provider_drift_uncapped",
         {"provider": "openrouter", "model": "m", "reasoning_effort": "none", "max_output_tokens": 2048},
         "anthropic", "m", None, None),
        ("reasoning_on_uncapped",
         {"provider": "openrouter", "model": "m", "max_output_tokens": 2048},
         "openrouter", "m", None, None),
        ("auto_provider_uncapped",
         {"provider": "auto", "model": "m", "reasoning_effort": "none", "max_output_tokens": 2048},
         "openrouter", "m", None, None),
        ("auto_model_uncapped",
         {"provider": "openrouter", "model": "auto", "reasoning_effort": "none", "max_output_tokens": 2048},
         "openrouter", "auto", None, None),
        ("bool_cap_is_config_drift_none",
         {"provider": "openrouter", "model": "m", "reasoning_effort": "none", "max_output_tokens": True},
         "openrouter", "m", None, None),
        ("zero_cap_none",
         {"provider": "openrouter", "model": "m", "reasoning_effort": "none", "max_output_tokens": 0},
         "openrouter", "m", None, None),
        ("negative_cap_none",
         {"provider": "openrouter", "model": "m", "reasoning_effort": "none", "max_output_tokens": -5},
         "openrouter", "m", None, None),
        ("string_cap_parsed",
         {"provider": "openrouter", "model": "m", "reasoning_effort": "none", "max_output_tokens": "4096"},
         "openrouter", "m", None, None),
        ("requested_model_override_matches",
         {"provider": "openrouter", "model": "m", "reasoning_effort": "none", "max_output_tokens": 1024},
         "openrouter", "pinned", "openrouter", "pinned"),
        ("certified_no_cap_field",
         {"provider": "openrouter", "model": "m", "reasoning_effort": "none"},
         "openrouter", "m", None, None),
        ("no_reasoning_effort_uncapped",
         {"provider": "openrouter", "model": "m", "max_output_tokens": 2048},
         "openrouter", "m", None, None),
    ]

    rows = []
    for name, config, ap, am, rp, rm in cases:
        rows.append({
            "case": name,
            "route_config": config,
            "actual_provider": ap,
            "actual_model": am,
            "requested_provider": rp,
            "requested_model": rm,
            "result": run(config, ap, am, rp, rm),
        })
    return rows


# ---------------------------------------------------------------------------
# Section 2b: wire forwarding of the cap + model-specific parameter name
# ---------------------------------------------------------------------------
def section_wire_forwarding() -> List[Dict[str, Any]]:
    def build(provider, model, base_url, max_tokens, custom_base=""):
        with patched_route_seams(custom_base=custom_base):
            kwargs = ax._build_call_kwargs(
                provider=provider,
                model=model,
                messages=[{"role": "user", "content": "x"}],
                max_tokens=max_tokens,
                base_url=base_url,
            )
        # Record only the output-cap-relevant keys.
        cap = {k: kwargs[k] for k in ("max_tokens", "max_completion_tokens") if k in kwargs}
        return cap

    cases = [
        # The compression summary path deliberately sends NO max_tokens; these
        # rows show what the builder does when a cap IS supplied (fast lane) and
        # when it is omitted (the default derived behavior).
        ("no_cap_default_omitted", "custom", "some-model", "https://plain/v1", None, "https://plain/v1"),
        ("plain_custom_cap_dropped", "custom", "some-model", "https://plain/v1", 2048, "https://plain/v1"),
        ("openrouter_cap_forwarded_max_tokens", "openrouter", "meta/llama", "https://openrouter.ai/api/v1", 2048, ""),
        ("openai_host_custom_gpt5_cap_not_forwarded", "custom", "gpt-5", "https://api.openai.com/v1", 2048, "https://api.openai.com/v1"),
        ("openai_host_custom_gpt4o_cap_not_forwarded", "custom", "gpt-4o", "https://api.openai.com/v1", 2048, "https://api.openai.com/v1"),
        ("openrouter_gpt5_by_name_uses_max_completion_tokens", "openrouter", "openai/gpt-5", "https://openrouter.ai/api/v1", 2048, ""),
        ("openrouter_llama_uses_max_tokens", "openrouter", "meta/llama-3", "https://openrouter.ai/api/v1", 2048, ""),
        ("nvidia_nim_cap_forwarded", "nvidia", "minimaxai/minimax-m3", "https://integrate.api.nvidia.com/v1", 2048, ""),
        ("plain_openai_family_name_but_no_forward_provider", "custom", "gpt-5", "https://plain/v1", 2048, "https://plain/v1"),
    ]

    rows = []
    for name, provider, model, base_url, max_tokens, custom_base in cases:
        rows.append({
            "case": name,
            "provider": provider,
            "model": model,
            "base_url": base_url,
            "max_tokens": max_tokens,
            "cap_kwargs": build(provider, model, base_url, max_tokens, custom_base),
        })
    return rows


# ---------------------------------------------------------------------------
# Section 3: summary temperature (omit / fixed / default)
# ---------------------------------------------------------------------------
def section_temperature() -> List[Dict[str, Any]]:
    omit = ax.OMIT_TEMPERATURE

    def fixed(model, base_url):
        r = ax._fixed_temperature_for_model(model, base_url)
        if r is omit:
            return "OMIT"
        return r

    def build_temp(provider, model, base_url, temperature):
        with patched_route_seams(custom_base=base_url or ""):
            kwargs = ax._build_call_kwargs(
                provider=provider,
                model=model,
                messages=[{"role": "user", "content": "x"}],
                temperature=temperature,
                base_url=base_url,
            )
        return kwargs.get("temperature", "OMITTED")

    cases = [
        # name, provider, model, base_url, caller_temperature
        ("kimi_moonshot_omits", "custom", "kimi-k2", "https://api.moonshot.ai/v1", None),
        ("kimi_name_omits", "custom", "moonshot/kimi-k2.5", None, None),
        ("arcee_trinity_thinking_forced_half", "custom", "arcee/trinity-large-thinking", None, None),
        ("generic_model_default_none_omitted", "openrouter", "meta/llama-3", None, None),
        ("generic_model_caller_temperature_kept", "openrouter", "meta/llama-3", None, 0.3),
        ("arcee_overrides_caller_temperature", "custom", "arcee/trinity-large-thinking", None, 0.9),
        ("kimi_overrides_caller_temperature", "custom", "kimi-k2", "https://api.moonshot.ai/v1", 0.7),
    ]

    rows = []
    for name, provider, model, base_url, temperature in cases:
        rows.append({
            "case": name,
            "provider": provider,
            "model": model,
            "base_url": base_url,
            "caller_temperature": temperature,
            "fixed_temperature_for_model": fixed(model, base_url),
            "resolved_temperature": build_temp(provider, model, base_url, temperature),
        })
    return rows


# ---------------------------------------------------------------------------
# Section 4: one-shot main-model fallback predicate (AST-extracted, verbatim)
# ---------------------------------------------------------------------------
class _CompressorStub:
    """Minimal stand-in exposing only the attributes the predicates read."""

    def __init__(self, model: str, summary_model: str, fell_back: bool):
        self.model = model
        self.summary_model = summary_model
        self._summary_model_fallen_back = fell_back


def _extract_fallback_logic():
    """Pull the exception classifier and both fallback predicates verbatim.

    Nothing is retyped: the boolean assignments and the ``if`` test expressions
    are compiled straight from ``ContextCompressor._generate_summary``'s source,
    then executed against the real ``agent.context_compressor`` module globals so
    every helper (``_is_connection_error``, ``_is_summary_access_or_quota_error``,
    ``classify_api_error``, the marker tuples) is the genuine one.
    """
    src = (ROOT / "agent/context_compressor.py").read_text()
    tree = ast.parse(src)
    func = next(
        n for n in ast.walk(tree)
        if isinstance(n, ast.FunctionDef) and n.name == "_generate_summary"
    )
    handler = None
    for n in ast.walk(func):
        if isinstance(n, ast.ExceptHandler):
            names = {
                t.id
                for st in n.body if isinstance(st, ast.Assign)
                for t in st.targets if isinstance(t, ast.Name)
            }
            if "_is_model_not_found" in names:
                handler = n
                break
    assert handler is not None, "classification except-handler not found"

    classify_names = {
        "_status", "_err_str", "_is_model_not_found", "_is_timeout",
        "_is_json_decode", "_is_streaming_closed", "_is_empty_content",
        "_is_truncated_summary", "_is_access_or_quota_error",
    }
    assigns = [
        st for st in handler.body
        if isinstance(st, ast.Assign)
        and isinstance(st.targets[0], ast.Name)
        and st.targets[0].id in classify_names
    ]
    classify_code = compile(
        ast.Module(body=assigns, type_ignores=[]), "cc-classify", "exec"
    )

    # No-provider early-return predicate (long-cooldown / fail-closed branch).
    no_provider_if = next(
        st for st in handler.body
        if isinstance(st, ast.If) and "no llm provider configured" in ast.unparse(st.test)
    )
    no_provider_code = compile(
        ast.Expression(no_provider_if.test), "cc-no-provider", "eval"
    )

    # The two one-shot main-model fallback predicates.
    fallback_ifs = [
        st for st in handler.body
        if isinstance(st, ast.If)
        and "summary_model" in ast.unparse(st.test)
        and ".model" in ast.unparse(st.test)
    ]
    assert len(fallback_ifs) == 2, f"expected 2 fallback ifs, found {len(fallback_ifs)}"
    fast_if = next(i for i in fallback_ifs if "_is_model_not_found" in ast.unparse(i.test))
    generic_if = next(i for i in fallback_ifs if "_is_model_not_found" not in ast.unparse(i.test))
    fast_code = compile(ast.Expression(fast_if.test), "cc-fast-fallback", "eval")
    generic_code = compile(ast.Expression(generic_if.test), "cc-generic-fallback", "eval")

    return {
        "classify_lines": [a.lineno for a in assigns],
        "classify": classify_code,
        "no_provider": no_provider_code,
        "fast": fast_code,
        "generic": generic_code,
    }


_BOOL_KEYS = [
    "_is_model_not_found", "_is_timeout", "_is_json_decode",
    "_is_streaming_closed", "_is_empty_content", "_is_truncated_summary",
    "_is_access_or_quota_error",
]


def _make_exception(spec: Dict[str, Any]) -> Exception:
    kind = spec["type"]
    msg = spec.get("message", "")
    if kind == "runtime":
        exc: Exception = RuntimeError(msg)
    elif kind == "value":
        exc = ValueError(msg)
    elif kind == "connection":
        exc = ConnectionError(msg)
    elif kind == "json_decode":
        exc = json.JSONDecodeError(msg or "Expecting value", "doc", 0)
    else:
        exc = Exception(msg)
    if spec.get("status_code") is not None:
        exc.status_code = spec["status_code"]  # type: ignore[attr-defined]
    return exc


def section_fallback(logic) -> List[Dict[str, Any]]:
    def evaluate(exc_spec, model, summary_model, fell_back):
        exc = _make_exception(exc_spec)
        ns = dict(vars(cc))
        ns["e"] = exc
        ns["self"] = _CompressorStub(model, summary_model, fell_back)
        # exec/eval here run code objects compiled directly from this repo's
        # own agent/context_compressor.py source (the classifier assignments and
        # the fallback predicate expressions), never any external input. That is
        # deliberate: the oracle executes the real decision logic verbatim.
        exec(logic["classify"], ns)
        classification = {k: bool(ns[k]) for k in _BOOL_KEYS}
        return {
            "classification": classification,
            "no_provider_cooldown": bool(eval(logic["no_provider"], ns)),
            "fast_path_fallback": bool(eval(logic["fast"], ns)),
            "generic_fallback": bool(eval(logic["generic"], ns)),
        }

    # Distinct failure specs exercised against several compressor states.
    exc_specs = [
        ("model_not_found_404", {"type": "value", "message": "model_not_found", "status_code": 404}),
        ("service_unavailable_503", {"type": "value", "message": "temporarily down", "status_code": 503}),
        ("does_not_exist_text", {"type": "value", "message": "the model does not exist"}),
        ("no_available_channel", {"type": "value", "message": "no available channel for request"}),
        ("timeout_504", {"type": "value", "message": "upstream gateway", "status_code": 504}),
        ("rate_limited_429", {"type": "value", "message": "slow down", "status_code": 429}),
        ("timeout_text", {"type": "value", "message": "request timed out"}),
        ("json_decode", {"type": "json_decode", "message": "Expecting value"}),
        ("expecting_value_text", {"type": "value", "message": "Expecting value: line 1 column 1"}),
        ("streaming_closed", {"type": "connection", "message": "peer closed connection"}),
        ("empty_content_runtime", {"type": "runtime", "message": "Context compression LLM returned empty content"}),
        ("truncated_summary_runtime", {"type": "runtime", "message": "summary was truncated (finish_reason=length)"}),
        ("auth_401", {"type": "value", "message": "invalid api key", "status_code": 401}),
        ("payment_402", {"type": "value", "message": "payment required", "status_code": 402}),
        ("forbidden_403", {"type": "value", "message": "forbidden", "status_code": 403}),
        ("insufficient_quota", {"type": "value", "message": "insufficient_quota for org"}),
        ("out_of_credits", {"type": "value", "message": "you are out of credits"}),
        ("missing_credential", {"type": "value", "message": "no api key was found"}),
        ("no_provider_configured", {"type": "runtime", "message": "No LLM provider configured for compression"}),
        ("unknown_400", {"type": "value", "message": "bad request", "status_code": 400}),
    ]

    # Compressor states: distinct aux model, same model as main, and no override.
    states = [
        ("distinct_summary_model", "main-model", "aux-model", False),
        ("same_summary_model", "main-model", "main-model", False),
        ("no_summary_override", "main-model", "", False),
        ("distinct_already_fell_back", "main-model", "aux-model", True),
    ]

    rows = []
    for exc_name, spec in exc_specs:
        for state_name, model, summary_model, fell in states:
            rows.append({
                "case": f"{exc_name}::{state_name}",
                "exception": spec,
                "state": {
                    "model": model,
                    "summary_model": summary_model,
                    "already_fell_back": fell,
                },
                "result": evaluate(spec, model, summary_model, fell),
            })
    return rows


# ---------------------------------------------------------------------------
# Assembly
# ---------------------------------------------------------------------------
def build_corpus() -> Dict[str, Any]:
    logic = _extract_fallback_logic()
    return {
        "task_config_resolution": section_task_config_resolution(),
        "summary_output_cap": {
            "fast_lane": section_fast_lane(),
            "wire_forwarding": section_wire_forwarding(),
        },
        "summary_temperature": section_temperature(),
        "main_model_fallback": {
            "classify_source_lines": logic["classify_lines"],
            "cases": section_fallback(logic),
        },
    }


def main() -> None:
    args = sys.argv[1:]
    corpus = build_corpus()
    text = json.dumps(corpus, ensure_ascii=False, indent=2) + "\n"
    if args == ["--check"]:
        current = OUT.read_text()
        if current != text:
            raise SystemExit("compression-auxiliary-config-goldens.json is stale; rerun the generator")
        print("OK: corpus matches checked-in goldens")
    elif not args:
        OUT.write_text(text)
        n = (
            len(corpus["task_config_resolution"])
            + len(corpus["summary_output_cap"]["fast_lane"])
            + len(corpus["summary_output_cap"]["wire_forwarding"])
            + len(corpus["summary_temperature"])
            + len(corpus["main_model_fallback"]["cases"])
        )
        print(f"Wrote {n} cases to {OUT.relative_to(ROOT)}")
    else:
        raise SystemExit("usage: compression-auxiliary-config-oracle.py [--check]")


if __name__ == "__main__":
    main()
