#!/usr/bin/env python3
"""Generate source-executed run-budget timeout compatibility fixtures."""

from __future__ import annotations

import argparse
import json
import math
import os
import sys
from pathlib import Path
from typing import Any
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/main-provider-run-budget-goldens.json"
FROZEN_NOW = 1_000_000.0

sys.path.insert(0, str(ROOT))

from agent.agent_init import _normalize_run_budget_seconds  # noqa: E402
from agent.chat_completion_helpers import _derive_stream_stale_timeout  # noqa: E402
from run_agent import AIAgent  # noqa: E402


def _serialized(value: Any) -> Any:
    if isinstance(value, float):
        if math.isnan(value):
            return "nan"
        if math.isinf(value):
            return "inf" if value > 0 else "-inf"
    return value


def _agent(case: dict[str, Any]) -> AIAgent:
    """Build only the fields used by the live timeout methods."""
    agent = object.__new__(AIAgent)
    agent.provider = case.get("provider", "openai")
    agent.model = case.get("model", "gpt-4o")
    agent.base_url = case.get("base_url", "https://api.example.test/v1")
    agent._base_url = agent.base_url
    agent.run_budget_seconds = case.get("run_budget_seconds")
    elapsed = case.get("elapsed_seconds")
    agent._run_budget_started_at = (
        None if elapsed is None else FROZEN_NOW - float(elapsed)
    )
    return agent


def _payload(kind: str) -> dict[str, Any]:
    if kind == "small":
        return {"messages": [{"role": "user", "content": "hi"}]}
    if kind == "medium":
        return {"input": "m" * 240_004}
    if kind == "large":
        return {"input": "L" * 440_004}
    raise ValueError(f"unknown payload kind: {kind}")


def _config(case: dict[str, Any]) -> dict[str, Any]:
    explicit = case.get("explicit")
    if explicit == "model":
        return {
            "providers": {
                case["provider"]: {
                    "models": {
                        case["model"]: {
                            "stale_timeout_seconds": case["explicit_seconds"]
                        }
                    }
                }
            }
        }
    if explicit == "provider":
        return {
            "providers": {
                case["provider"]: {"stale_timeout_seconds": case["explicit_seconds"]}
            }
        }
    return {}


def normalization_cases() -> list[dict[str, Any]]:
    fixtures = [
        ("null", None, None),
        ("false", False, None),
        ("true", True, None),
        ("zero", 0, None),
        ("negative", -5, None),
        ("bad_string", "abc", None),
        ("nan_string", "nan", None),
        ("nan_float", float("nan"), None),
        ("empty_list", [], None),
        ("object", {"seconds": 900}, None),
        ("integer", 900, 900.0),
        ("numeric_string", "850", 850.0),
        ("fraction", 0.5, 0.5),
        ("positive_infinity", float("inf"), float("inf")),
        ("infinity_string", "inf", float("inf")),
    ]
    cases = []
    for name, raw, expected in fixtures:
        actual = _normalize_run_budget_seconds(raw)
        if isinstance(expected, float) and math.isinf(expected):
            assert actual is not None and math.isinf(actual) and actual > 0
        else:
            assert actual == expected, (name, actual, expected)
        cases.append({
            "case_name": name,
            "raw_input": _serialized(raw),
            "raw_type": type(raw).__name__,
            "expected_seconds": _serialized(expected),
        })
    return cases


def buffered_cases() -> list[dict[str, Any]]:
    cases = [
        {
            "case_name": "no_budget_keeps_default",
            "expected_timeout": 90.0,
        },
        {
            "case_name": "budget_without_clock_is_inert",
            "model": "deepseek/deepseek-r1",
            "provider": "deepseek",
            "run_budget_seconds": 120.0,
            "expected_timeout": 600.0,
        },
        {
            "case_name": "fresh_reasoning_floor_uses_half_budget",
            "model": "deepseek/deepseek-r1",
            "provider": "deepseek",
            "run_budget_seconds": 900.0,
            "elapsed_seconds": 0.0,
            "expected_timeout": 450.0,
        },
        {
            "case_name": "elapsed_reasoning_floor_uses_remaining_budget",
            "model": "deepseek/deepseek-r1",
            "provider": "deepseek",
            "run_budget_seconds": 900.0,
            "elapsed_seconds": 100.0,
            "expected_timeout": 400.0,
        },
        {
            "case_name": "minimum_cap_floor",
            "model": "deepseek/deepseek-r1",
            "provider": "deepseek",
            "run_budget_seconds": 900.0,
            "elapsed_seconds": 800.0,
            "expected_timeout": 60.0,
        },
        {
            "case_name": "negative_remaining_uses_minimum_floor",
            "model": "deepseek/deepseek-r1",
            "provider": "deepseek",
            "run_budget_seconds": 120.0,
            "elapsed_seconds": 200.0,
            "expected_timeout": 60.0,
        },
        {
            "case_name": "cap_never_raises_default",
            "run_budget_seconds": 1_000.0,
            "elapsed_seconds": 0.0,
            "expected_timeout": 90.0,
        },
        {
            "case_name": "medium_context_is_capped_after_scaling",
            "payload_kind": "medium",
            "run_budget_seconds": 200.0,
            "elapsed_seconds": 0.0,
            "expected_timeout": 100.0,
        },
        {
            "case_name": "large_context_is_capped_after_scaling",
            "payload_kind": "large",
            "run_budget_seconds": 200.0,
            "elapsed_seconds": 0.0,
            "expected_timeout": 100.0,
        },
        {
            "case_name": "model_explicit_timeout_is_spared",
            "model": "deepseek/deepseek-r1",
            "provider": "deepseek",
            "explicit": "model",
            "explicit_seconds": 600.0,
            "run_budget_seconds": 120.0,
            "elapsed_seconds": 200.0,
            "expected_timeout": 600.0,
        },
        {
            "case_name": "provider_explicit_timeout_is_spared",
            "model": "deepseek/deepseek-r1",
            "provider": "deepseek",
            "explicit": "provider",
            "explicit_seconds": 700.0,
            "run_budget_seconds": 120.0,
            "elapsed_seconds": 200.0,
            "expected_timeout": 700.0,
        },
        {
            "case_name": "environment_explicit_timeout_is_spared",
            "model": "deepseek/deepseek-r1",
            "provider": "deepseek",
            "explicit": "environment",
            "explicit_seconds": 1_200.0,
            "run_budget_seconds": 120.0,
            "elapsed_seconds": 200.0,
            "expected_timeout": 1_200.0,
        },
        {
            "case_name": "local_implicit_default_remains_unbounded",
            "base_url": "http://localhost:11434",
            "run_budget_seconds": 120.0,
            "elapsed_seconds": 200.0,
            "expected_timeout": "inf",
        },
        {
            "case_name": "local_reasoning_floor_remains_finite_and_capped",
            "model": "deepseek/deepseek-r1",
            "provider": "ollama",
            "base_url": "http://localhost:11434",
            "run_budget_seconds": 120.0,
            "elapsed_seconds": 200.0,
            "expected_timeout": 60.0,
        },
    ]

    rendered = []
    for case in cases:
        agent = _agent(case)
        env = {}
        if case.get("explicit") == "environment":
            env["HERMES_API_CALL_STALE_TIMEOUT"] = str(case["explicit_seconds"])
        with (
            patch("run_agent.time.time", return_value=FROZEN_NOW),
            patch("hermes_cli.config.load_config_readonly", return_value=_config(case)),
            patch.dict(os.environ, env, clear=True),
        ):
            actual = agent._compute_non_stream_stale_timeout(
                _payload(case.get("payload_kind", "small"))
            )
        expected = case["expected_timeout"]
        if expected == "inf":
            assert math.isinf(actual), (case["case_name"], actual)
        else:
            assert actual == expected, (case["case_name"], actual, expected)
        rendered.append({**case, "actual_timeout": _serialized(actual)})
    return rendered


def streaming_cases() -> list[dict[str, Any]]:
    cases = [
        {
            "case_name": "plain_small_stream",
            "model": "gpt-4o",
            "payload_kind": "small",
            "expected_timeout": 180.0,
        },
        {
            "case_name": "reasoning_small_stream",
            "model": "deepseek/deepseek-r1",
            "provider": "deepseek",
            "payload_kind": "small",
            "expected_timeout": 600.0,
        },
        {
            "case_name": "plain_large_stream",
            "model": "gpt-4o",
            "payload_kind": "large",
            "expected_timeout": 300.0,
        },
    ]
    rendered = []
    for case in cases:
        payload = {**_payload(case["payload_kind"]), "model": case["model"]}
        without_budget = _agent(case)
        with_budget = _agent({
            **case,
            "run_budget_seconds": 120.0,
            "elapsed_seconds": 200.0,
        })
        with (
            patch("hermes_cli.config.load_config_readonly", return_value={}),
            patch.dict(os.environ, {}, clear=True),
        ):
            ordinary = _derive_stream_stale_timeout(without_budget, payload)
            budgeted = _derive_stream_stale_timeout(with_budget, payload)
        expected = case["expected_timeout"]
        assert ordinary == expected, (case["case_name"], ordinary, expected)
        assert budgeted == ordinary, (case["case_name"], budgeted, ordinary)
        rendered.append({
            **case,
            "without_budget": ordinary,
            "with_expired_budget": budgeted,
        })
    return rendered


def corpus() -> dict[str, Any]:
    result = {
        "normalization": normalization_cases(),
        "buffered_stale": buffered_cases(),
        "streaming_unchanged": streaming_cases(),
    }
    result["metadata"] = {
        "case_count": sum(len(value) for value in result.values()),
        "provenance": "executed_live_python_source",
    }
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    rendered = json.dumps(corpus(), indent=2, sort_keys=True) + "\n"
    assert "\u2014" not in rendered
    if args.check:
        if not OUT.exists() or OUT.read_text(encoding="utf-8") != rendered:
            print(f"out of date: {OUT}", file=sys.stderr)
            return 1
        print(f"verified {OUT.relative_to(ROOT)}")
        return 0
    OUT.write_text(rendered, encoding="utf-8")
    print(f"wrote {OUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
