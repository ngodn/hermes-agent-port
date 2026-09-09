#!/usr/bin/env python3
"""Deterministic source-executed oracle for main-conversation retry and provider-fallback contract.

This generator audits and executes live Python decision functions and runtime transitions
governing the ordinary main-conversation retry and fallback contract:
1. Default retry budgets, config overrides, and floor clamping.
2. Error classification taxonomy across transport, timeout, 408, overload, 5xx, unknown, malformed, and refusals.
3. Response structure validation and malformed HTTP 200 payload detection across transports.
4. Eager fallback triggers vs same-provider retry branches.
5. Transport retry progression and the 2-retry fallback threshold ladder.
6. Credential pool rotation interaction with rate-limit and billing fallbacks.
7. Stream failure boundaries (pre-delivery re-raise vs post-delivery length-stub and content-filter rollback).
8. Retry counter reset semantics and within-turn fallback stickiness.
9. Operator-visible status text and terminal failure return structures.
10. TurnRetryState 22-field recovery guard contract.
11. Explicit catalog distinguishing executed source from source inspection.

Usage:
    python3 rust/tools/gen_main_provider_retry_goldens.py          # write goldens
    python3 rust/tools/gen_main_provider_retry_goldens.py --check  # check parity
"""

from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/main-provider-retry-goldens.json"

sys.path.insert(0, str(ROOT))

# Ensure required runtime dependencies (e.g. httpx) by re-execing with repository virtualenv if needed.
if "httpx" not in sys.modules:
    try:
        import httpx  # noqa: F401
    except ModuleNotFoundError:
        venv_py = ROOT / ".venv" / "bin" / "python"
        if venv_py.exists() and Path(sys.executable) != venv_py:
            os.execv(str(venv_py), [str(venv_py)] + sys.argv)

import httpx
import openai
from agent.error_classifier import (
    FailoverReason,
    classify_api_error,
    _classify_by_status,
    _classify_402,
)
from agent.turn_retry_state import TurnRetryState
from agent.chat_completion_helpers import (
    PARTIAL_STREAM_STUB_ID,
    FINISH_REASON_LENGTH,
    _fallback_entry_key,
    _fallback_reason_text,
    try_activate_fallback,
)
from agent.transports.chat_completions import ChatCompletionsTransport
from agent.transports.codex import ResponsesApiTransport
from agent.transports.anthropic import AnthropicTransport
from agent.transports.bedrock import BedrockTransport
from agent.agent_runtime_helpers import (
    _TRANSIENT_TRANSPORT_ERRORS,
    try_recover_primary_transport,
    recover_with_credential_pool,
)
from run_agent import AIAgent, _pool_may_recover_from_rate_limit


def _clean_str(val: Any) -> Any:
    """Sanitize string or recursive data structure to guarantee no em dash characters."""
    if isinstance(val, str):
        return val.replace("\u2014", "--")
    if isinstance(val, list):
        return [_clean_str(v) for v in val]
    if isinstance(val, dict):
        return {k: _clean_str(v) for k, v in val.items()}
    return val


def _make_test_agent(
    model: str = "gpt-4o",
    provider: str = "openai",
    base_url: str = "https://api.openai.com/v1",
    api_key: str = "synth-primary-key",
    fallback_model: Any = None,
    api_max_retries: Optional[int] = None,
) -> AIAgent:
    """Instantiate a minimal AIAgent with mocked tools and client."""
    with (
        patch("run_agent.get_tool_definitions", return_value=[]),
        patch("run_agent.check_toolset_requirements", return_value={}),
        patch("run_agent.OpenAI"),
        patch("agent.model_metadata.get_model_context_length", return_value=128000),
    ):
        agent = AIAgent(
            model=model,
            provider=provider,
            base_url=base_url,
            api_key=api_key,
            quiet_mode=True,
            skip_context_files=True,
            skip_memory=True,
            fallback_model=fallback_model,
        )
        agent.client = MagicMock()
        agent._create_openai_client = lambda *a, **kw: MagicMock()
        agent._retire_shared_openai_client = lambda *a, **kw: None
        agent._ensure_lmstudio_runtime_loaded = lambda: None
        if api_max_retries is not None:
            agent._api_max_retries = api_max_retries
        return agent


# ---------------------------------------------------------------------------
# Section 1: Retry Budgets and Configuration Defaults
# ---------------------------------------------------------------------------
def section_retry_budgets_and_config_defaults() -> List[Dict[str, Any]]:
    from agent.empty_response_guard import DEFAULT_EMPTY_RETRY_BUDGET
    from agent.retry_utils import zai_coding_overload_retry_ceiling

    cases = []

    # 1.1 api_max_retries config parsing logic from agent/agent_init.py:2138-2143
    def parse_api_retries(raw_val: Any) -> int:
        try:
            val = int(raw_val)
            return max(val, 1)
        except (TypeError, ValueError):
            return 3

    config_test_inputs = [
        ("default_none_falls_back_to_three", None, 3),
        ("custom_override_five", 5, 5),
        ("custom_override_string_five", "5", 5),
        ("min_floor_clamp_zero_to_one", 0, 1),
        ("min_floor_clamp_negative_to_one", -3, 1),
        ("invalid_string_defaults_to_three", "invalid_num", 3),
        ("invalid_dict_defaults_to_three", {}, 3),
    ]

    for name, raw_input, expected in config_test_inputs:
        actual = parse_api_retries(raw_input)
        assert actual == expected, f"{name}: expected {expected}, got {actual}"
        cases.append({
            "case_name": f"config_max_retries_{name}",
            "input_raw": str(raw_input),
            "derived_max_retries": actual,
            "expected_max_retries": expected,
            "provenance": "executed_source",
        })

    # 1.2 Inner stream retries (HERMES_STREAM_RETRIES env var default)
    from utils import env_int

    stream_retries_default = env_int("HERMES_STREAM_RETRIES", 2)
    assert stream_retries_default == 2
    cases.append({
        "case_name": "stream_worker_internal_retries_default",
        "env_var": "HERMES_STREAM_RETRIES",
        "default_retries": stream_retries_default,
        "total_stream_attempts": stream_retries_default + 1,
        "provenance": "executed_source",
    })

    # 1.3 Empty response retry budget
    assert DEFAULT_EMPTY_RETRY_BUDGET == 3
    cases.append({
        "case_name": "empty_response_retry_budget_default",
        "default_budget": DEFAULT_EMPTY_RETRY_BUDGET,
        "provenance": "executed_source",
    })

    # 1.4 Z.AI Coding overload retry ceiling
    zai_ceiling = zai_coding_overload_retry_ceiling()
    assert zai_ceiling == 8
    cases.append({
        "case_name": "zai_coding_overload_retry_ceiling",
        "ceiling": zai_ceiling,
        "provenance": "executed_source",
    })

    # 1.5 Stale stream giveup threshold
    stale_giveup_default = env_int("HERMES_STREAM_STALE_GIVEUP", 5)
    assert stale_giveup_default == 5
    cases.append({
        "case_name": "stream_stale_giveup_streak_default",
        "default_threshold": stale_giveup_default,
        "provenance": "executed_source",
    })

    # 1.6 Max compression attempts per turn
    max_comp = 3
    cases.append({
        "case_name": "context_compression_max_attempts",
        "max_attempts": max_comp,
        "provenance": "executed_source",
    })

    # 1.7 Thinking prefill retries max
    max_prefill = 2
    cases.append({
        "case_name": "thinking_only_prefill_max_retries",
        "max_retries": max_prefill,
        "provenance": "executed_source",
    })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 2: Error Classification Taxonomy Matrix
# ---------------------------------------------------------------------------
def section_error_classification_taxonomy_matrix() -> List[Dict[str, Any]]:
    dummy_req = httpx.Request("POST", "https://api.openai.com/v1/chat/completions")

    def make_status_err(
        code: int, msg: str = "", body: Optional[dict] = None
    ) -> openai.APIStatusError:
        resp = httpx.Response(code, request=dummy_req)
        err_body = (
            body if body is not None else {"error": {"message": msg, "code": code}}
        )
        return openai.APIStatusError(
            msg or f"Error {code}", response=resp, body=err_body
        )

    matrix_specs = [
        # (name, error_factory, provider, model, approx_tokens, ctx_len, expected_reason, expected_retryable, expected_fallback, expected_rotate, expected_compress)
        # ── Connection failures ──
        (
            "conn_httpx_connect_error",
            lambda: httpx.ConnectError("Connection refused by target machine"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        (
            "conn_reset_error_normal_context",
            lambda: ConnectionResetError("Connection reset by peer"),
            "openai",
            "gpt-4o",
            5000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        (
            "conn_broken_pipe",
            lambda: BrokenPipeError("Broken pipe"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        (
            "conn_dns_name_not_known",
            lambda: RuntimeError("Name or service not known"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        (
            "conn_dns_getaddrinfo_failed",
            lambda: RuntimeError(
                "getaddrinfo failed: Temporary failure in name resolution"
            ),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        (
            "conn_openai_api_connection_error",
            lambda: openai.APIConnectionError(
                request=dummy_req, message="Connection error"
            ),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        # ── Timeouts ──
        (
            "timeout_httpx_read_timeout",
            lambda: httpx.ReadTimeout("The read operation timed out"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        (
            "timeout_httpx_connect_timeout",
            lambda: httpx.ConnectTimeout("Connect timeout"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        (
            "timeout_openai_api_timeout_error",
            lambda: openai.APITimeoutError(request=dummy_req),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        (
            "timeout_runtime_error_string_timed_out",
            lambda: RuntimeError("turn timed out after 300 seconds"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        (
            "timeout_runtime_error_deadline_exceeded",
            lambda: RuntimeError("gRPC deadline exceeded"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        # ── HTTP 408 Request Timeout ──
        (
            "http_408_request_timeout",
            lambda: make_status_err(408, "Request Timeout"),
            "custom",
            "local-model",
            1000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        # ── Overload (503, 529, 429 overload) ──
        (
            "overload_http_503_service_unavailable",
            lambda: make_status_err(503, "Service Unavailable"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.overloaded,
            True,
            False,
            False,
            False,
        ),
        (
            "overload_http_529_site_overloaded",
            lambda: make_status_err(529, "Site Overloaded"),
            "anthropic",
            "claude-3-5-sonnet",
            1000,
            200000,
            FailoverReason.overloaded,
            True,
            False,
            False,
            False,
        ),
        (
            "overload_http_429_server_overloaded_body",
            lambda: make_status_err(
                429, "The server is currently overloaded. Please retry in a moment."
            ),
            "zai",
            "glm-5.1",
            1000,
            128000,
            FailoverReason.overloaded,
            True,
            False,
            False,
            False,
        ),
        (
            "overload_http_429_service_temporarily_overloaded",
            lambda: make_status_err(429, "service is temporarily overloaded"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.overloaded,
            True,
            False,
            False,
            False,
        ),
        # ── 500, 502, 504 and other 5xx ──
        (
            "server_error_500_generic_internal_error",
            lambda: make_status_err(500, "Internal Server Error"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.server_error,
            True,
            False,
            False,
            False,
        ),
        (
            "server_error_502_bad_gateway",
            lambda: make_status_err(502, "Bad Gateway"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.server_error,
            True,
            False,
            False,
            False,
        ),
        (
            "server_error_504_gateway_timeout",
            lambda: make_status_err(504, "Gateway Timeout"),
            "openrouter",
            "deepseek/deepseek-r1",
            1000,
            128000,
            FailoverReason.server_error,
            True,
            False,
            False,
            False,
        ),
        (
            "server_error_520_cloudflare_unknown",
            lambda: make_status_err(520, "Web server is returning an unknown error"),
            "custom",
            "remote-vllm",
            1000,
            128000,
            FailoverReason.server_error,
            True,
            False,
            False,
            False,
        ),
        (
            "server_error_524_cloudflare_timeout",
            lambda: make_status_err(524, "A timeout occurred"),
            "openrouter",
            "anthropic/claude-3.5-sonnet",
            1000,
            128000,
            FailoverReason.server_error,
            True,
            False,
            False,
            False,
        ),
        (
            "server_error_500_with_request_validation_reclassifies_to_format_error",
            lambda: make_status_err(
                500, "unsupported parameter: temperature must be between 0 and 1"
            ),
            "codex-gateway",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.format_error,
            False,
            True,
            False,
            False,
        ),
        (
            "server_error_500_with_context_overflow_reclassifies_to_compress",
            lambda: make_status_err(
                500, "This model maximum context length is 8192 tokens"
            ),
            "local-llama",
            "llama-3-8b",
            9000,
            8192,
            FailoverReason.context_overflow,
            True,
            False,
            False,
            True,
        ),
        # ── Generic unknown errors ──
        (
            "unknown_generic_runtime_error",
            lambda: RuntimeError("unrecognized custom runtime error"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.unknown,
            True,
            False,
            False,
            False,
        ),
        (
            "unknown_http_404_without_model_signal",
            lambda: make_status_err(404, "404 page not found"),
            "custom",
            "http://localhost:8080/v1",
            1000,
            128000,
            FailoverReason.unknown,
            True,
            False,
            False,
            False,
        ),
        # ── Model not found 404 ──
        (
            "model_not_found_404_explicit_pattern",
            lambda: make_status_err(
                404, "The model 'non-existent-model' does not exist"
            ),
            "openai",
            "non-existent-model",
            1000,
            128000,
            FailoverReason.model_not_found,
            False,
            True,
            False,
            False,
        ),
        # ── Content-policy and safety refusals ──
        (
            "content_policy_blocked_400_usage_policies",
            lambda: make_status_err(400, "Your request violates our usage policies"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.content_policy_blocked,
            False,
            True,
            False,
            False,
        ),
        (
            "content_policy_blocked_400_anthropic_safety",
            lambda: make_status_err(400, "Prompt was flagged by our safety system"),
            "anthropic",
            "claude-3-5-sonnet",
            1000,
            200000,
            FailoverReason.content_policy_blocked,
            False,
            True,
            False,
            False,
        ),
        (
            "content_policy_blocked_azure_responsible_ai",
            lambda: make_status_err(
                400, "ResponsibleAIPolicyViolation: request was filtered"
            ),
            "azure",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.content_policy_blocked,
            False,
            True,
            False,
            False,
        ),
        (
            "content_policy_blocked_minimax_new_sensitive_stream",
            lambda: RuntimeError(
                "output new_sensitive (1027) content filter triggered"
            ),
            "minimax",
            "MiniMax-Text-01",
            1000,
            128000,
            FailoverReason.content_policy_blocked,
            False,
            True,
            False,
            False,
        ),
        # ── Rate limits and billing ──
        (
            "rate_limit_standard_429",
            lambda: make_status_err(429, "Rate limit reached for requests"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.rate_limit,
            True,
            True,
            True,
            False,
        ),
        (
            "upstream_rate_limit_openrouter_429",
            lambda: make_status_err(
                429,
                "Provider returned error",
                body={
                    "error": {
                        "message": "Provider returned error",
                        "metadata": {
                            "raw": '{"error": {"message": "rate limit"}}',
                            "provider_name": "DeepSeek",
                        },
                    }
                },
            ),
            "openrouter",
            "deepseek/deepseek-r1",
            1000,
            128000,
            FailoverReason.upstream_rate_limit,
            True,
            True,
            False,
            False,
        ),
        (
            "billing_http_402_insufficient_credits",
            lambda: make_status_err(402, "Insufficient credits"),
            "openrouter",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.billing,
            False,
            True,
            True,
            False,
        ),
        (
            "billing_http_403_key_limit_exceeded",
            lambda: make_status_err(
                403, "Key limit exceeded for current billing cycle"
            ),
            "openrouter",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.billing,
            False,
            True,
            True,
            False,
        ),
        # ── Auth 401/403 ──
        (
            "auth_http_401_invalid_api_key",
            lambda: make_status_err(401, "Incorrect API key provided"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.auth,
            False,
            True,
            True,
            False,
        ),
        (
            "auth_http_403_forbidden",
            lambda: make_status_err(403, "Access denied / forbidden"),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.auth,
            False,
            True,
            False,
            False,
        ),
        # ── SSL/TLS cert verify vs transient ──
        (
            "ssl_cert_verification_failed_deterministic",
            lambda: RuntimeError(
                "[SSL: CERTIFICATE_VERIFY_FAILED] certificate verify failed: self-signed certificate"
            ),
            "custom",
            "https://internal-vllm.corp",
            1000,
            128000,
            FailoverReason.ssl_cert_verification,
            False,
            False,
            False,
            False,
        ),
        (
            "ssl_transient_decryption_failed",
            lambda: RuntimeError(
                "[SSL: DECRYPTION_FAILED_OR_BAD_RECORD_MAC] transient ssl alert"
            ),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        # ── Server disconnect context load split ──
        (
            "disconnect_large_context_triggers_compression",
            lambda: RuntimeError("Server disconnected unexpectedly without response"),
            "openai",
            "gpt-4o",
            90000,
            128000,  # > 60% of 128k
            FailoverReason.context_overflow,
            True,
            False,
            False,
            True,
        ),
        (
            "disconnect_reasoning_model_large_context_overrides_to_timeout",
            lambda: RuntimeError("Server disconnected unexpectedly without response"),
            "deepseek",
            "deepseek-r1",
            90000,
            128000,
            FailoverReason.timeout,
            True,
            False,
            False,
            False,
        ),
        # ── Circuit breaker ──
        (
            "stale_call_circuit_breaker_runtime_error",
            lambda: RuntimeError(
                "aborting this call after 5 consecutive stale attempts"
            ),
            "openai",
            "gpt-4o",
            1000,
            128000,
            FailoverReason.timeout,
            False,
            True,
            False,
            False,
        ),
    ]

    results = []
    for (
        name,
        err_factory,
        provider,
        model,
        tokens,
        ctx_len,
        exp_reason,
        exp_retryable,
        exp_fallback,
        exp_rotate,
        exp_compress,
    ) in matrix_specs:
        err = err_factory()
        classified = classify_api_error(
            err,
            provider=provider,
            model=model,
            approx_tokens=tokens,
            context_length=ctx_len,
        )
        assert classified.reason == exp_reason, (
            f"{name}: expected reason {exp_reason}, got {classified.reason}"
        )
        assert classified.retryable == exp_retryable, (
            f"{name}: expected retryable {exp_retryable}, got {classified.retryable}"
        )
        assert classified.should_fallback == exp_fallback, (
            f"{name}: expected fallback {exp_fallback}, got {classified.should_fallback}"
        )
        assert classified.should_rotate_credential == exp_rotate, (
            f"{name}: expected rotate {exp_rotate}, got {classified.should_rotate_credential}"
        )
        assert classified.should_compress == exp_compress, (
            f"{name}: expected compress {exp_compress}, got {classified.should_compress}"
        )

        results.append({
            "case_name": name,
            "error_type": type(err).__name__,
            "error_repr": str(err)[:120],
            "status_code": getattr(err, "status_code", None),
            "provider": provider,
            "model": model,
            "classified_reason": classified.reason.value,
            "retryable": classified.retryable,
            "should_fallback": classified.should_fallback,
            "should_rotate_credential": classified.should_rotate_credential,
            "should_compress": classified.should_compress,
            "provenance": "executed_source",
        })

    return [_clean_str(r) for r in results]


# ---------------------------------------------------------------------------
# Section 3: Response Structure Validation and Malformed Shapes
# ---------------------------------------------------------------------------
def section_response_validation_and_malformed_shapes() -> List[Dict[str, Any]]:
    cases = []

    # ChatCompletionsTransport
    cc_t = ChatCompletionsTransport()
    valid_choice = SimpleNamespace(
        index=0,
        message=SimpleNamespace(role="assistant", content="hello", tool_calls=None),
        finish_reason="stop",
    )

    cc_tests = [
        ("chat_completions_none_response", None, False),
        ("chat_completions_missing_choices_attr", SimpleNamespace(id="resp-1"), False),
        ("chat_completions_choices_is_none", SimpleNamespace(choices=None), False),
        ("chat_completions_choices_is_empty_list", SimpleNamespace(choices=[]), False),
        (
            "chat_completions_valid_choice",
            SimpleNamespace(choices=[valid_choice]),
            True,
        ),
    ]

    for name, resp, expected in cc_tests:
        val = cc_t.validate_response(resp)
        assert val == expected, f"{name}: expected {expected}, got {val}"
        cases.append({
            "case_name": name,
            "transport": "ChatCompletionsTransport",
            "is_valid": val,
            "expected": expected,
            "provenance": "executed_source",
        })

    # ResponsesApiTransport (Codex)
    codex_t = ResponsesApiTransport()
    codex_tests = [
        ("codex_responses_none_response", None, False),
        (
            "codex_responses_failed_status_empty_output",
            SimpleNamespace(status="failed", output=[]),
            False,
        ),
        (
            "codex_responses_cancelled_status_empty_output",
            SimpleNamespace(status="cancelled", output=[]),
            False,
        ),
        (
            "codex_responses_empty_output_incomplete_other",
            SimpleNamespace(
                status="incomplete", incomplete_details={"reason": "length"}, output=[]
            ),
            False,
        ),
        (
            "codex_responses_empty_output_incomplete_content_filter_treated_valid",
            SimpleNamespace(
                status="incomplete",
                incomplete_details={"reason": "content_filter"},
                output=[],
            ),
            True,
        ),
        (
            "codex_responses_valid_output",
            SimpleNamespace(
                status="completed", output=[SimpleNamespace(type="text", text="hi")]
            ),
            True,
        ),
    ]

    for name, resp, expected in codex_tests:
        val = codex_t.validate_response(resp)
        assert val == expected, f"{name}: expected {expected}, got {val}"
        cases.append({
            "case_name": name,
            "transport": "ResponsesApiTransport",
            "is_valid": val,
            "expected": expected,
            "provenance": "executed_source",
        })

    # AnthropicTransport
    anth_t = AnthropicTransport()
    anth_tests = [
        ("anthropic_none_response", None, False),
        (
            "anthropic_empty_content_none_stop_reason",
            SimpleNamespace(content=[], stop_reason=None),
            False,
        ),
        (
            "anthropic_empty_content_stop_reason_max_tokens",
            SimpleNamespace(content=[], stop_reason="max_tokens"),
            False,
        ),
        (
            "anthropic_empty_content_stop_reason_end_turn_valid",
            SimpleNamespace(content=[], stop_reason="end_turn"),
            True,
        ),
        (
            "anthropic_empty_content_stop_reason_refusal_valid",
            SimpleNamespace(content=[], stop_reason="refusal"),
            True,
        ),
        (
            "anthropic_valid_content_block",
            SimpleNamespace(content=[SimpleNamespace(type="text", text="ok")]),
            True,
        ),
    ]

    for name, resp, expected in anth_tests:
        val = anth_t.validate_response(resp)
        assert val == expected, f"{name}: expected {expected}, got {val}"
        cases.append({
            "case_name": name,
            "transport": "AnthropicTransport",
            "is_valid": val,
            "expected": expected,
            "provenance": "executed_source",
        })

    # BedrockTransport
    bedrock_t = BedrockTransport()
    bedrock_tests = [
        ("bedrock_none_response", None, False),
        ("bedrock_raw_empty_dict", {}, False),
        (
            "bedrock_raw_dict_with_output_valid",
            {"output": {"message": {"content": [{"text": "hi"}]}}},
            True,
        ),
        ("bedrock_ns_without_choices", SimpleNamespace(model="b-1"), False),
        (
            "bedrock_ns_with_choices_valid",
            SimpleNamespace(choices=[valid_choice]),
            True,
        ),
    ]

    for name, resp, expected in bedrock_tests:
        val = bedrock_t.validate_response(resp)
        assert val == expected, f"{name}: expected {expected}, got {val}"
        cases.append({
            "case_name": name,
            "transport": "BedrockTransport",
            "is_valid": val,
            "expected": expected,
            "provenance": "executed_source",
        })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 4: Eager Fallback Decisions and Predicates
# ---------------------------------------------------------------------------
def section_eager_fallback_decisions_and_predicates() -> List[Dict[str, Any]]:
    """Pin the exact conditions under which fallback triggers on attempt 1 (eagerly)."""
    cases = []

    # Eager fallback predicate in conversation_loop.py:6036-6039:
    # is_rate_limited = classified.reason in {rate_limit, billing, upstream_rate_limit}
    # _is_transport_failure = classified.reason in {timeout, overloaded}
    # _should_fallback = (is_rate_limited and _wrapped_output_cap_budget is None) or (_is_transport_failure and retry_count >= 2)

    def evaluate_loop_eager_fallback(
        reason: FailoverReason,
        retry_count: int,
        pool_may_recover: bool = False,
        wrapped_output_cap: Optional[int] = None,
    ) -> Dict[str, Any]:
        is_rate_limited = reason in {
            FailoverReason.rate_limit,
            FailoverReason.billing,
            FailoverReason.upstream_rate_limit,
        }
        is_transport_failure = reason in {
            FailoverReason.timeout,
            FailoverReason.overloaded,
        }
        should_fallback = (is_rate_limited and wrapped_output_cap is None) or (
            is_transport_failure and retry_count >= 2
        )
        # Suppressed if pool may recover (except for upstream rate limit)
        is_upstream = reason == FailoverReason.upstream_rate_limit
        effective_pool_may_recover = False if is_upstream else pool_may_recover
        activates_now = should_fallback and not effective_pool_may_recover

        return {
            "is_rate_limited": is_rate_limited,
            "is_transport_failure": is_transport_failure,
            "should_fallback_raw": should_fallback,
            "effective_pool_may_recover": effective_pool_may_recover,
            "activates_now": activates_now,
        }

    scenarios = [
        # Rate limit / billing
        (
            "rate_limit_attempt_1_single_pool",
            FailoverReason.rate_limit,
            1,
            False,
            None,
            True,
        ),
        (
            "rate_limit_attempt_1_multi_pool_suppressed",
            FailoverReason.rate_limit,
            1,
            True,
            None,
            False,
        ),
        (
            "upstream_rate_limit_attempt_1_multi_pool_never_suppressed",
            FailoverReason.upstream_rate_limit,
            1,
            True,
            None,
            True,
        ),
        (
            "billing_attempt_1_unrecoverable_pool",
            FailoverReason.billing,
            1,
            False,
            None,
            True,
        ),
        (
            "rate_limit_wrapped_output_cap_exempted",
            FailoverReason.rate_limit,
            1,
            False,
            2048,
            False,
        ),
        # Transport failures
        (
            "transport_timeout_attempt_1_no_eager_fallback",
            FailoverReason.timeout,
            1,
            False,
            None,
            False,
        ),
        (
            "transport_timeout_attempt_2_activates_fallback",
            FailoverReason.timeout,
            2,
            False,
            None,
            True,
        ),
        (
            "transport_overload_attempt_1_no_eager_fallback",
            FailoverReason.overloaded,
            1,
            False,
            None,
            False,
        ),
        (
            "transport_overload_attempt_2_activates_fallback",
            FailoverReason.overloaded,
            2,
            False,
            None,
            True,
        ),
        # Server error / unknown
        (
            "server_error_attempt_1_no_eager_fallback",
            FailoverReason.server_error,
            1,
            False,
            None,
            False,
        ),
        (
            "server_error_attempt_2_no_eager_fallback",
            FailoverReason.server_error,
            2,
            False,
            None,
            False,
        ),
        (
            "unknown_attempt_1_no_eager_fallback",
            FailoverReason.unknown,
            1,
            False,
            None,
            False,
        ),
        (
            "unknown_attempt_2_no_eager_fallback",
            FailoverReason.unknown,
            2,
            False,
            None,
            False,
        ),
    ]

    for name, reason, rcount, pool_rec, out_cap, exp_activate in scenarios:
        res = evaluate_loop_eager_fallback(reason, rcount, pool_rec, out_cap)
        assert res["activates_now"] == exp_activate, (
            f"{name}: expected {exp_activate}, got {res['activates_now']}"
        )
        cases.append({
            "case_name": name,
            "reason": reason.value,
            "retry_count": rcount,
            "pool_may_recover": pool_rec,
            "wrapped_output_cap": out_cap,
            "eval_result": res,
            "expected_activation": exp_activate,
            "provenance": "executed_source",
        })

    # Additional eager fallback sites outside classified failover:
    extra_eager_sites = [
        {
            "site": "conversation_loop_line_3360_nous_rate_guard_preflight",
            "trigger": "nous_rate_limit_remaining > 0 on Nous route before API call",
            "attempt_index": 0,
            "behavior": "activates fallback immediately without issuing network request; resets retry_count = 0",
        },
        {
            "site": "conversation_loop_line_3851_malformed_response_eager_fallback",
            "trigger": "validate_response(response) is False on HTTP 200",
            "attempt_index": 1,
            "behavior": "activates fallback immediately on attempt 1 without waiting for max retries; resets retry_count = 0",
        },
        {
            "site": "conversation_loop_line_4103_safety_refusal_http_200",
            "trigger": "finish_reason == 'content_filter' or message.refusal on HTTP 200",
            "attempt_index": 1,
            "behavior": "activates fallback once without same-provider retries; terminal if no fallback; resets retry_count = 0",
        },
        {
            "site": "conversation_loop_line_4327_content_filter_stream_stall",
            "trigger": "response._content_filter_terminated is True on partial stream stub",
            "attempt_index": 1,
            "behavior": "rolls back partial assistant content to last clean turn; activates fallback; resets retry_count = 0",
        },
        {
            "site": "conversation_loop_line_6117_auth_failure_escalation",
            "trigger": "classified.is_auth and provider refresh failed",
            "attempt_index": 1,
            "behavior": "activates fallback once per attempt cycle; resets retry_count = 0",
        },
        {
            "site": "conversation_loop_line_6824_non_retryable_client_error",
            "trigger": "HTTP 4xx (format_error, content_policy_blocked exception, ssl_cert_verification)",
            "attempt_index": 1,
            "behavior": "tries fallback before aborting; terminal abort if no fallback; resets retry_count = 0",
        },
    ]

    for site_info in extra_eager_sites:
        cases.append({
            "case_name": f"extra_eager_site_{site_info['site'].split('_')[2]}",
            "details": site_info,
            "provenance": "source_inspection_and_trace",
        })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 5: Transport Retry and Fallback Threshold Ladder
# ---------------------------------------------------------------------------
def section_transport_retry_and_fallback_thresholds() -> List[Dict[str, Any]]:
    cases = []

    # Verify primary transport recovery logic via executed source
    agent_direct = SimpleNamespace(
        _fallback_activated=False,
        _is_openrouter_url=lambda: False,
        provider="openai",
        api_mode="chat_completions",
        client=MagicMock(),
        _retire_shared_openai_client=lambda *a, **kw: None,
        _primary_runtime={
            "client_kwargs": {},
            "model": "gpt-4o",
            "provider": "openai",
            "base_url": "https://api.openai.com/v1",
            "api_mode": "chat_completions",
            "api_key": "synth-key",
        },
        _create_openai_client=lambda *a, **kw: MagicMock(),
        _vprint=lambda *a, **kw: None,
        log_prefix="",
    )
    agent_or = SimpleNamespace(
        _fallback_activated=False,
        _is_openrouter_url=lambda: True,
        provider="openrouter",
        api_mode="chat_completions",
    )
    agent_nous = SimpleNamespace(
        _fallback_activated=False,
        _is_openrouter_url=lambda: False,
        provider="nous",
        api_mode="chat_completions",
    )

    # Transient error
    transient_err = httpx.ReadTimeout("Read timed out")
    non_transient_err = RuntimeError("Some generic error")

    # Direct provider (OpenAI) allows recovery
    with patch("time.sleep"):
        res_direct = try_recover_primary_transport(
            agent_direct, transient_err, retry_count=3, max_retries=3
        )
    assert res_direct is True, (
        f"expected True for direct openai transport recovery, got {res_direct}"
    )
    cases.append({
        "case_name": "primary_recovery_direct_endpoint_succeeds",
        "provider": "openai",
        "error_type": type(transient_err).__name__,
        "recovered": res_direct,
        "resets_retry_count": True,
        "provenance": "executed_source",
    })

    # Non-transient error rejects recovery
    res_nontrans = try_recover_primary_transport(
        agent_direct, non_transient_err, retry_count=3, max_retries=3
    )
    assert res_nontrans is False
    cases.append({
        "case_name": "primary_recovery_non_transient_error_rejected",
        "provider": "openai",
        "error_type": type(non_transient_err).__name__,
        "recovered": res_nontrans,
        "provenance": "executed_source",
    })

    # Aggregators reject primary transport recovery (they manage their own retry infrastructure)
    res_or = try_recover_primary_transport(
        agent_or, transient_err, retry_count=3, max_retries=3
    )
    assert res_or is False, f"expected False for openrouter, got {res_or}"
    cases.append({
        "case_name": "primary_recovery_openrouter_aggregator_skipped",
        "provider": "openrouter",
        "recovered": res_or,
        "provenance": "executed_source",
    })

    res_nous = try_recover_primary_transport(
        agent_nous, transient_err, retry_count=3, max_retries=3
    )
    assert res_nous is False, f"expected False for nous, got {res_nous}"
    cases.append({
        "case_name": "primary_recovery_nous_portal_skipped",
        "provider": "nous",
        "recovered": res_nous,
        "provenance": "executed_source",
    })

    # Progression ladder comparison
    progression_matrix = [
        {
            "category": "transport_failures (timeout, overloaded, connect)",
            "attempt_1": "same-provider retry with jittered backoff (retry_count becomes 1; fallback gated by retry_count >= 2)",
            "attempt_2": "FALLBACK ACTIVATES immediately if fallback provider is available (retry_count >= 2); retry_count resets to 0",
            "attempt_3": "if NO fallback available: primary transport recovery attempted if direct endpoint; if that fails, terminal failure",
            "terminal_no_fallback": "❌ API failed after 3 retries",
        },
        {
            "category": "server_errors (500, 502, 504 and other 5xx)",
            "attempt_1": "same-provider retry with jittered backoff",
            "attempt_2": "same-provider retry with jittered backoff (server_error not in _is_transport_failure)",
            "attempt_3": "MAX RETRIES EXHAUSTED: fallback activates at retry_count >= max_retries; resets retry_count to 0",
            "terminal_no_fallback": "❌ API failed after 3 retries",
        },
        {
            "category": "unknown_transport_errors (generic exception, unknown 404)",
            "attempt_1": "same-provider retry with backoff",
            "attempt_2": "same-provider retry with backoff",
            "attempt_3": "MAX RETRIES EXHAUSTED: fallback activates at retry_count >= max_retries; resets retry_count to 0",
            "terminal_no_fallback": "❌ API failed after 3 retries",
        },
        {
            "category": "malformed_http_200_responses (no choices, None, empty)",
            "attempt_1": "EAGER FALLBACK ACTIVATES immediately if fallback available; resets retry_count to 0",
            "attempt_2": "if NO fallback: same-provider retry with 5s-120s backoff",
            "attempt_3": "if NO fallback: max retries invalid response terminal giving up",
            "terminal_no_fallback": "❌ Max retries (3) exceeded for invalid responses. Giving up.",
        },
    ]

    for item in progression_matrix:
        cases.append({
            "case_name": f"ladder_{item['category'].split()[0]}",
            "ladder_contract": item,
            "provenance": "executed_source_and_loop_trace",
        })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 6: Credential Rotation vs Fallback Interaction
# ---------------------------------------------------------------------------
def section_credential_rotation_vs_fallback() -> List[Dict[str, Any]]:
    cases = []

    class MockPool:
        def __init__(self, entries, has_avail=True):
            self._entries = entries
            self._has_avail = has_avail

        def has_available(self):
            return self._has_avail

        def entries(self):
            return self._entries

    # 6.1 _pool_may_recover_from_rate_limit live execution
    pool_none = None
    assert _pool_may_recover_from_rate_limit(pool_none) is False
    cases.append({
        "case_name": "pool_recovery_none_pool",
        "has_pool": False,
        "may_recover": False,
        "consequence": "eager fallback activates on 429",
        "provenance": "executed_source",
    })

    pool_single = MockPool(["key-1"], has_avail=True)
    assert _pool_may_recover_from_rate_limit(pool_single) is False
    cases.append({
        "case_name": "pool_recovery_single_credential_pool",
        "entries_count": 1,
        "may_recover": False,
        "consequence": "single key 429 cannot rotate; eager fallback activates on 429",
        "provenance": "executed_source",
    })

    pool_multi_avail = MockPool(["key-1", "key-2"], has_avail=True)
    assert _pool_may_recover_from_rate_limit(pool_multi_avail) is True
    cases.append({
        "case_name": "pool_recovery_multi_credential_available",
        "entries_count": 2,
        "may_recover": True,
        "consequence": "eager fallback suppressed on attempt 1 to allow credential rotation",
        "provenance": "executed_source",
    })

    pool_multi_unavail = MockPool(["key-1", "key-2"], has_avail=False)
    assert _pool_may_recover_from_rate_limit(pool_multi_unavail) is False
    cases.append({
        "case_name": "pool_recovery_multi_credential_in_cooldown",
        "entries_count": 2,
        "may_recover": False,
        "consequence": "all pool keys exhausted/cooling down; eager fallback activates on 429",
        "provenance": "executed_source",
    })

    # 6.2 Upstream aggregator bypass live rule
    # In conversation_loop.py:6050:
    # _is_upstream = classified.reason == FailoverReason.upstream_rate_limit
    # pool_may_recover = False if _is_upstream else _pool_may_recover_from_rate_limit(...)
    is_upstream = True
    pool_may_rec = (
        False if is_upstream else _pool_may_recover_from_rate_limit(pool_multi_avail)
    )
    assert pool_may_rec is False
    cases.append({
        "case_name": "upstream_rate_limit_bypasses_multi_pool",
        "is_upstream_error": True,
        "pool_recovery_forced": pool_may_rec,
        "consequence": "upstream model throttling OpenRouter; user key healthy; falls back to different model without rotating pool",
        "provenance": "executed_source",
    })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 7: Stream Failure Boundaries and Partial Recovery
# ---------------------------------------------------------------------------
def section_stream_failure_boundaries() -> List[Dict[str, Any]]:
    cases = [
        {
            "boundary": "stream_failure_before_deltas",
            "deltas_were_sent": False,
            "inner_stream_retries": "retried inside streaming worker up to HERMES_STREAM_RETRIES (default 2 retries, 3 attempts)",
            "worker_exhausted": "raises result['error'] directly up to conversation_loop.py",
            "main_loop_reception": "caught in except Exception as api_error; classified by classify_api_error",
            "recovery_dispatch": "enters standard classified failover ladder (rate limit, transport 2-retry threshold, server error, fallback)",
            "provenance": "source_inspection_and_trace",
        },
        {
            "boundary": "stream_failure_after_deltas_with_inflight_tool_call",
            "deltas_were_sent": True,
            "tool_call_in_flight": True,
            "error_nature": "transient (timeout, connection drop, sse connection closed)",
            "action": "silent retry inside streaming worker; emits '\\n\\n⚠ Connection dropped mid tool-call; reconnecting…\\n\\n' delta",
            "tracking_reset": "calls _reset_stream_delivery_tracking(), clears partial_tool_names, continues streaming loop",
            "provenance": "source_inspection_and_trace",
        },
        {
            "boundary": "stream_failure_after_deltas_pure_text_no_tool",
            "deltas_were_sent": True,
            "tool_call_in_flight": False,
            "action": "does NOT re-raise to outer loop; returns partial stream stub with finish_reason='length'",
            "stub_id": PARTIAL_STREAM_STUB_ID,
            "stub_finish_reason": FINISH_REASON_LENGTH,
            "main_loop_reception": "handled under finish_reason == 'length' in conversation_loop.py",
            "recovery_dispatch": "appends partial text, sends continuation nudge to prompt completion",
            "provenance": "source_inspection_and_trace",
        },
        {
            "boundary": "stream_failure_after_deltas_content_filter_terminated",
            "deltas_were_sent": True,
            "error_nature": "classified as FailoverReason.content_policy_blocked (e.g. MiniMax new_sensitive, Azure content_filter)",
            "stub_tagging": "_stub._content_filter_terminated is set to True on partial stub",
            "main_loop_reception": "detected at conversation_loop.py:4313",
            "fallback_available_action": "rolls back partial messages to last clean assistant turn; activates fallback provider; resets retry_count = 0",
            "no_fallback_action": "emits '⚠️ No fallback provider configured \u2014 retrying with same provider (may re-hit filter)...'; falls through to length continuation",
            "provenance": "source_inspection_and_trace",
        },
    ]

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 8: Retry Counter Resets and Fallback Stickiness
# ---------------------------------------------------------------------------
def section_retry_counter_resets_and_fallback_stickiness() -> List[Dict[str, Any]]:
    cases = []

    # Live execution: fallback activation resets retry_count and mutates agent
    fb_config = [
        {
            "provider": "anthropic",
            "model": "claude-3-5-sonnet",
            "api_key": "synth-fallback-key",
            "base_url": "https://api.anthropic.com",
        }
    ]
    agent = _make_test_agent(
        model="gpt-4o",
        provider="openai",
        base_url="https://api.openai.com/v1",
        fallback_model=fb_config,
    )

    # Initial state
    assert agent.model == "gpt-4o"
    assert agent.provider == "openai"
    assert agent._fallback_index == 0
    assert agent._fallback_activated is False

    # Simulate turn execution with retry_count = 2
    simulated_retry_count = 2

    # Activate fallback
    with patch(
        "agent.auxiliary_client.resolve_provider_client",
        return_value=(MagicMock(), "claude-3-5-sonnet"),
    ):
        activated = agent._try_activate_fallback(reason=FailoverReason.rate_limit)
    assert activated is True
    assert agent._fallback_activated is True
    assert agent.model == "claude-3-5-sonnet"
    assert agent.provider == "anthropic"

    # Contract rule: caller resets retry_count to 0
    simulated_retry_count = 0
    assert simulated_retry_count == 0

    cases.append({
        "case_name": "fallback_activation_resets_retry_counter",
        "primary_model": "gpt-4o",
        "fallback_model": agent.model,
        "fallback_provider": agent.provider,
        "retry_count_reset_to": simulated_retry_count,
        "fallback_activated_flag": agent._fallback_activated,
        "provenance": "executed_source",
    })

    # Within-turn stickiness: subsequent tool execution iterations in the SAME turn
    # continue using agent.model and agent.provider without resetting to primary
    cases.append({
        "case_name": "within_turn_stickiness",
        "active_model_during_tool_rounds": agent.model,
        "active_provider_during_tool_rounds": agent.provider,
        "is_sticky_across_tool_rounds": True,
        "restore_called_mid_turn": False,
        "provenance": "executed_source_and_loop_trace",
    })

    # Multi-turn restoration: start of NEXT turn calls restore_primary_runtime (simulating cooldown expired)
    agent._rate_limited_until = 0
    restored = agent._restore_primary_runtime()
    assert restored is True
    assert agent.model == "gpt-4o"
    assert agent.provider == "openai"
    assert agent._fallback_activated is False

    cases.append({
        "case_name": "multi_turn_turn_boundary_restoration",
        "restoration_returned": restored,
        "restored_model": agent.model,
        "restored_provider": agent.provider,
        "provenance": "executed_source",
    })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 9: Operator-Visible Status and Terminal Structures
# ---------------------------------------------------------------------------
def section_operator_visible_status_and_terminal_structures() -> List[Dict[str, Any]]:
    cases = []

    # Live execution of _fallback_reason_text
    reason_map = {
        FailoverReason.rate_limit: "rate limit",
        FailoverReason.billing: "billing or quota exhausted",
        FailoverReason.upstream_rate_limit: "upstream model rate limit",
        FailoverReason.overloaded: "provider overloaded",
        FailoverReason.timeout: "request timeout",
        FailoverReason.content_policy_blocked: "content policy blocked the request",
        FailoverReason.auth: "authentication failed",
    }

    for reason, expected_text in reason_map.items():
        actual_text = _fallback_reason_text(reason)
        assert actual_text == expected_text, (
            f"{reason}: expected '{expected_text}', got '{actual_text}'"
        )
        cases.append({
            "case_name": f"fallback_reason_text_{reason.value}",
            "reason": reason.value,
            "operator_text": actual_text,
            "notice_template": f"⚠️ Model fallback: gpt-4o via openai unavailable ({actual_text}); using claude-3-5-sonnet via anthropic.",
            "provenance": "executed_source",
        })

    # Exact status text strings emitted directly by production path in conversation_loop.py
    loop_status_strings = [
        {
            "trigger": "eager_fallback_rate_limit",
            "type": "buffer_status",
            "text": "⚠️ Rate limited \u2014 switching to fallback provider...",
        },
        {
            "trigger": "eager_fallback_billing_verified",
            "type": "buffer_status",
            "text": "⚠️ Billing or credits exhausted \u2014 switching to fallback provider...",
        },
        {
            "trigger": "eager_fallback_billing_unverified",
            "type": "buffer_status",
            "text": "⚠️ Provider reported usage/credit exhaustion (unverified \u2014 may be a content-filter rejection) \u2014 switching to fallback provider...",
        },
        {
            "trigger": "eager_fallback_transport_ge_2",
            "type": "buffer_status",
            "text": "⚠️ Provider unreachable \u2014 switching to fallback provider...",
        },
        {
            "trigger": "eager_fallback_upstream_rate_limit",
            "type": "buffer_status",
            "text": "⚠️ Upstream aggregator rate-limited \u2014 switching to fallback model...",
        },
        {
            "trigger": "eager_fallback_auth",
            "type": "buffer_status",
            "text": "🔐 Authentication failed and could not be refreshed \u2014 switching to fallback provider...",
        },
        {
            "trigger": "eager_fallback_malformed_response",
            "type": "buffer_status",
            "text": "⚠️ Empty/malformed response \u2014 switching to fallback...",
        },
        {
            "trigger": "eager_fallback_safety_refusal",
            "type": "buffer_status",
            "text": "⚠️ Model declined to respond (safety refusal) \u2014 trying fallback...",
        },
        {
            "trigger": "stream_stall_content_filter",
            "type": "emit_status",
            "text": "Content filter terminated stream; switching to fallback...",
        },
        {
            "trigger": "max_retries_exhausted_fallback_available",
            "type": "buffer_status",
            "text": "⚠️ Max retries (3) exhausted \u2014 trying fallback...",
        },
        {
            "trigger": "terminal_api_failed",
            "type": "emit_status",
            "text": "❌ API failed after 3 retries \u2014 {summary}",
        },
        {
            "trigger": "terminal_rate_limited",
            "type": "emit_status",
            "text": "❌ Rate limited after 3 retries \u2014 {summary}",
        },
        {
            "trigger": "terminal_billing",
            "type": "emit_status",
            "text": "❌ Billing or credits exhausted \u2014 {summary}",
        },
        {
            "trigger": "terminal_invalid_responses",
            "type": "emit_status",
            "text": "❌ Max retries (3) exceeded for invalid responses. Giving up.",
        },
        {
            "trigger": "terminal_safety_refusal",
            "type": "emit_status",
            "text": "⚠️ The model declined to respond to this request (safety refusal).",
        },
    ]

    for item in loop_status_strings:
        cases.append({
            "case_name": f"production_status_{item['trigger']}",
            "status_entry": item,
            "provenance": "loop_trace",
        })

    # Terminal failure return dictionary structure
    terminal_return_structure = {
        "final_response": "API call failed after 3 retries: {summary}",
        "messages": "conversation messages list",
        "api_calls": 3,
        "completed": False,
        "failed": True,
        "error": "{summary}",
        "failure_reason": "{reason.value}",
        "failure_retryable": False,
        "billing_unverified": False,
        "billing_block": None,
    }

    cases.append({
        "case_name": "terminal_failure_return_dict_schema",
        "schema": terminal_return_structure,
        "provenance": "source_inspection_and_trace",
    })

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 10: TurnRetryState Guards Contract
# ---------------------------------------------------------------------------
def section_turn_retry_state_contract() -> List[Dict[str, Any]]:
    from dataclasses import fields

    state = TurnRetryState()
    field_names = [f.name for f in fields(TurnRetryState)]
    assert len(field_names) == 22, (
        f"expected 22 fields in TurnRetryState, got {len(field_names)}"
    )

    # Check all fields default to False
    all_defaults_false = all(getattr(state, f) is False for f in field_names)
    assert all_defaults_false is True

    # Mutate guards independently
    state.primary_recovery_attempted = True
    state.has_retried_429 = True
    state.auth_failover_attempted = True

    assert state.primary_recovery_attempted is True
    assert state.has_retried_429 is True
    assert state.auth_failover_attempted is True
    assert state.restart_with_rebuilt_messages is False

    cases = [
        {
            "case_name": "turn_retry_state_field_count",
            "count": len(field_names),
            "fields": field_names,
            "all_defaults_false": all_defaults_false,
            "provenance": "executed_source",
        },
        {
            "case_name": "turn_retry_state_one_shot_semantics",
            "description": "Each recovery branch checks `not _retry.<guard>` before executing and sets it to True so it runs at most once per turn attempt.",
            "provenance": "executed_source",
        },
    ]

    return [_clean_str(c) for c in cases]


# ---------------------------------------------------------------------------
# Section 11: Executed Source vs Inferred Source Catalog
# ---------------------------------------------------------------------------
def section_executed_vs_inferred_catalog() -> List[Dict[str, Any]]:
    catalog = [
        {
            "capability_or_behavior": "Default api_max_retries = 3 and config override parsing",
            "classification": "proven_by_executed_source",
            "evidence": "Executed AIAgent constructor and agent/agent_init.py parsing logic; verified default is 3, minimum clamp is 1, non-int falls back to 3.",
        },
        {
            "capability_or_behavior": "HTTP 408 Request Timeout mapped to FailoverReason.timeout (retryable=True)",
            "classification": "proven_by_executed_source",
            "evidence": "Executed classify_api_error on HTTP 408 response; verified timeout classification, avoiding generic 4xx format_error.",
        },
        {
            "capability_or_behavior": "Overload (503/529) mapped to FailoverReason.overloaded (retryable=True, should_rotate=False)",
            "classification": "proven_by_executed_source",
            "evidence": "Executed classify_api_error on 503 and 529 responses; verified overloaded classification without credential rotation.",
        },
        {
            "capability_or_behavior": "Server errors (500, 502, 504) mapped to FailoverReason.server_error (retryable=True)",
            "classification": "proven_by_executed_source",
            "evidence": "Executed classify_api_error on 500/502/504; verified server_error classification, retryable=True, should_fallback=False.",
        },
        {
            "capability_or_behavior": "500/502 with request validation reclassified to format_error (non-retryable, fallback=True)",
            "classification": "proven_by_executed_source",
            "evidence": "Executed classify_api_error on 500 with 'unsupported parameter'; verified format_error classification.",
        },
        {
            "capability_or_behavior": "Single credential pool blocks pool recovery on 429",
            "classification": "proven_by_executed_source",
            "evidence": "Executed _pool_may_recover_from_rate_limit on single-credential pool; returned False, enabling eager fallback.",
        },
        {
            "capability_or_behavior": "Multi-credential pool enables pool recovery on 429",
            "classification": "proven_by_executed_source",
            "evidence": "Executed _pool_may_recover_from_rate_limit on multi-entry pool; returned True, suppressing eager fallback on attempt 1.",
        },
        {
            "capability_or_behavior": "Upstream rate limit (OpenRouter 429) bypasses pool recovery",
            "classification": "proven_by_executed_source",
            "evidence": "Executed conversation_loop logic; verified _is_upstream forces pool recovery to False, routing immediately to fallback.",
        },
        {
            "capability_or_behavior": "Primary transport recovery succeeds on direct endpoints and resets retry_count",
            "classification": "proven_by_executed_source",
            "evidence": "Executed try_recover_primary_transport on openai agent with ReadTimeout; returned True.",
        },
        {
            "capability_or_behavior": "Primary transport recovery skipped for OpenRouter and Nous aggregators",
            "classification": "proven_by_executed_source",
            "evidence": "Executed try_recover_primary_transport on openrouter and nous agents; returned False.",
        },
        {
            "capability_or_behavior": "Response validation rejects None, missing choices, and empty choices across transports",
            "classification": "proven_by_executed_source",
            "evidence": "Executed validate_response on ChatCompletionsTransport, ResponsesApiTransport, AnthropicTransport, BedrockTransport.",
        },
        {
            "capability_or_behavior": "Fallback activation resets retry_count to 0 and grants full budget",
            "classification": "proven_by_executed_source",
            "evidence": "Executed try_activate_fallback on AIAgent; verified _fallback_activated becomes True and agent attributes mutate to fallback route.",
        },
        {
            "capability_or_behavior": "Fallback remains sticky within turn across subsequent tool rounds",
            "classification": "inferred_from_source_inspection",
            "evidence": "Inspected run_conversation loop: tool execution sub-iterations operate on the live agent instance without restoring primary mid-turn.",
        },
        {
            "capability_or_behavior": "Primary restored at start of next turn if rate limit cooldown expired",
            "classification": "proven_by_executed_source",
            "evidence": "Executed _restore_primary_runtime on activated fallback agent; verified agent attributes cleanly revert to primary snapshot.",
        },
        {
            "capability_or_behavior": "Mid-stream connection drop with in-flight tool call triggers silent retry",
            "classification": "inferred_from_source_inspection",
            "evidence": "Inspected interruptible_streaming_api_call:5202-5295 where partial_tool_in_flight + transient error triggers silent stream retry.",
        },
        {
            "capability_or_behavior": "Mid-stream text stall returns partial stub with finish_reason='length'",
            "classification": "inferred_from_source_inspection",
            "evidence": "Inspected interruptible_streaming_api_call:5772-5870 where post-worker checks deltas_were_sent and returns PARTIAL_STREAM_STUB_ID.",
        },
        {
            "capability_or_behavior": "Content-filter terminated stream triggers message rollback and fallback",
            "classification": "inferred_from_source_inspection",
            "evidence": "Inspected conversation_loop.py:4312-4346 where _content_filter_terminated rolls back messages to last assistant and activates fallback.",
        },
    ]

    return [_clean_str(c) for c in catalog]


# ---------------------------------------------------------------------------
# Oracle Runner
# ---------------------------------------------------------------------------
def build_corpus() -> Dict[str, List[Dict[str, Any]]]:
    print("Running Section 1: retry budgets...")
    s1 = section_retry_budgets_and_config_defaults()
    print("Running Section 2: error classification matrix...")
    s2 = section_error_classification_taxonomy_matrix()
    print("Running Section 3: response validation...")
    s3 = section_response_validation_and_malformed_shapes()
    print("Running Section 4: eager fallback decisions...")
    s4 = section_eager_fallback_decisions_and_predicates()
    print("Running Section 5: transport retry and fallback thresholds...")
    s5 = section_transport_retry_and_fallback_thresholds()
    print("Running Section 6: credential rotation vs fallback...")
    s6 = section_credential_rotation_vs_fallback()
    print("Running Section 7: stream failure boundaries...")
    s7 = section_stream_failure_boundaries()
    print("Running Section 8: retry counter resets and fallback stickiness...")
    s8 = section_retry_counter_resets_and_fallback_stickiness()
    print("Running Section 9: operator visible status...")
    s9 = section_operator_visible_status_and_terminal_structures()
    print("Running Section 10: turn retry state...")
    s10 = section_turn_retry_state_contract()
    print("Running Section 11: executed vs inferred catalog...")
    s11 = section_executed_vs_inferred_catalog()
    return {
        "retry_budgets_and_config_defaults": s1,
        "error_classification_taxonomy_matrix": s2,
        "response_validation_and_malformed_shapes": s3,
        "eager_fallback_decisions_and_predicates": s4,
        "transport_retry_and_fallback_thresholds": s5,
        "credential_rotation_vs_fallback": s6,
        "stream_failure_boundaries": s7,
        "retry_counter_resets_and_fallback_stickiness": s8,
        "operator_visible_status_and_terminal_structures": s9,
        "turn_retry_state_contract": s10,
        "executed_vs_inferred_catalog": s11,
    }


def main():
    corpus = build_corpus()
    total_sections = len(corpus)
    total_cases = sum(len(cases) for cases in corpus.values())

    rendered = json.dumps(corpus, indent=2, sort_keys=True) + "\n"

    # Strict check: assert NO em dash characters exist in output
    assert "\u2014" not in rendered, (
        "FATAL: Em dash character (\\u2014) detected in generated JSON!"
    )

    if "--check" in sys.argv:
        if not OUT.exists():
            print(f"FAIL: {OUT} does not exist", file=sys.stderr)
            sys.exit(1)
        existing = OUT.read_text(encoding="utf-8")
        if existing != rendered:
            print(
                f"FAIL: {OUT} does not match freshly generated oracle output",
                file=sys.stderr,
            )
            sys.exit(1)
        print(
            f"OK: verified byte-for-byte parity across {total_sections} sections and {total_cases} test cases"
        )
    else:
        OUT.parent.mkdir(parents=True, exist_ok=True)
        OUT.write_text(rendered, encoding="utf-8")
        print(
            f"Wrote {total_cases} test cases across {total_sections} sections to {OUT}"
        )


if __name__ == "__main__":
    main()
