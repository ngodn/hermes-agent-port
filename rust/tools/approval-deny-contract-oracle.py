#!/usr/bin/env python3
"""Golden contract oracle for approvals.deny matching, mode: off, and terminal execution.

This script source-executes the REAL Python implementations under:
  - tools/approval.py (_match_user_deny_rule, _user_deny_block_result,
    _command_detection_variants, check_all_command_guards,
    detect_hardline_command, _check_sudo_stdin_guard, _get_approval_config)
  - tools/terminal_tool.py (terminal_tool, _check_all_guards)
  - hermes_cli/config.py (load_config_readonly, load_config)

It generates or verifies `rust/tools/approval-deny-contract-goldens.json` to define
the exact behavioral and structural contract that the native Rust port must
replicate for the native terminal deny-rule security slice.

Key properties:
  - Zero candidate shell commands are executed: execution seams are safely mocked.
  - Offline & credential-free: no external services, aux LLM calls, or user credentials.
  - Deterministic: environment variables, clock, and user config are hermetically isolated.
  - Verification: supports `--check` / `--verify` to ensure checked-in goldens match.

Usage:
  .venv/bin/python rust/tools/approval-deny-contract-oracle.py            # regenerate goldens
  .venv/bin/python rust/tools/approval-deny-contract-oracle.py --check    # verify goldens
"""

from __future__ import annotations

import argparse
import contextlib
import copy
import json
import logging
import os
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

REPO_ROOT = Path(__file__).resolve().parents[2]
GOLDEN_PATH = Path(__file__).resolve().parent / "approval-deny-contract-goldens.json"

sys.path.insert(0, str(REPO_ROOT))

# Silence loggers during execution so output remains clean
logging.getLogger("tools.terminal_tool").setLevel(logging.CRITICAL)
logging.getLogger("tools.approval").setLevel(logging.CRITICAL)
logging.getLogger("agent.redact").setLevel(logging.CRITICAL)
logging.getLogger("hermes_cli.config").setLevel(logging.CRITICAL)

import tools.approval as ta  # noqa: E402
import tools.terminal_tool as tt  # noqa: E402


# ---------------------------------------------------------------------------
# Hermetic Evaluation Context
# ---------------------------------------------------------------------------
@contextlib.contextmanager
def isolated_deny_context(
    *,
    deny_rules: Optional[Any] = None,
    approval_mode: str = "off",
    yolo_mode: bool = False,
    is_interactive_cli: bool = False,
    is_gateway: bool = False,
    is_single_query: bool = False,
    allowlist: Optional[List[str]] = None,
    sudo_password: Optional[str] = None,
):
    """Provide an isolated evaluation context for approval deny testing."""
    config_dict: Dict[str, Any] = {
        "mode": approval_mode,
        "timeout": 300,
        "cron_mode": "deny",
        "single_query_mode": "deny",
        "unattended_mode": "deny",
    }
    if deny_rules is not None:
        config_dict["deny"] = deny_rules

    scrubbed_env = {
        k: v
        for k, v in os.environ.items()
        if k
        not in (
            "SUDO_PASSWORD",
            "HERMES_YOLO_MODE",
            "HERMES_INTERACTIVE",
            "HERMES_GATEWAY_SESSION",
            "HERMES_EXEC_ASK",
            "HERMES_SINGLE_QUERY",
        )
    }
    if sudo_password is not None:
        scrubbed_env["SUDO_PASSWORD"] = sudo_password
    if yolo_mode:
        scrubbed_env["HERMES_YOLO_MODE"] = "1"
    if is_single_query:
        scrubbed_env["HERMES_SINGLE_QUERY"] = "1"

    mock_env = MagicMock()
    mock_env.execute.return_value = {
        "output": "[MOCK_EXECUTION_SUCCESS]",
        "returncode": 0,
        "cwd": "/workspace",
        "cwd_observed": True,
    }

    token_interactive = ta.set_hermes_interactive_context(is_interactive_cli)

    with ta._lock:
        ta._pending.clear()
        ta._session_approved.clear()
        ta._session_yolo.clear()

    allowlist_patterns = list(allowlist or [])

    with (
        patch.dict(os.environ, scrubbed_env, clear=True),
        patch("tools.approval._get_approval_config", return_value=config_dict),
        patch("tools.approval._get_approval_mode", return_value=approval_mode),
        patch("tools.approval.is_current_session_yolo_enabled", return_value=yolo_mode),
        patch(
            "tools.approval._is_single_query_approval_context",
            return_value=is_single_query,
        ),
        patch("tools.approval._is_gateway_approval_context", return_value=is_gateway),
        patch("tools.approval._is_interactive_cli", return_value=is_interactive_cli),
        patch(
            "tools.approval._command_matches_permanent_allowlist",
            side_effect=lambda c: any(p in c for p in allowlist_patterns),
        ),
        patch.object(ta, "_YOLO_MODE_FROZEN", yolo_mode),
        patch("tools.terminal_tool._create_environment", return_value=mock_env),
    ):
        try:
            yield
        finally:
            ta.reset_hermes_interactive_context(token_interactive)
            with ta._lock:
                ta._pending.clear()
                ta._session_approved.clear()
                ta._session_yolo.clear()


# ---------------------------------------------------------------------------
# Corpus Definitions
# ---------------------------------------------------------------------------


def run_rule_parsing_cases() -> List[Dict[str, Any]]:
    """Test parsing, filtering, and ordering of approvals.deny rules."""
    test_specs = [
        {
            "id": "parsing_empty_list",
            "deny_config": [],
            "command": "git push --force",
            "expected_matched": None,
        },
        {
            "id": "parsing_none_value",
            "deny_config": None,
            "command": "git push --force",
            "expected_matched": None,
        },
        {
            "id": "parsing_empty_string_entry",
            "deny_config": [""],
            "command": "git push --force",
            "expected_matched": None,
        },
        {
            "id": "parsing_whitespace_only_entry",
            "deny_config": ["   ", "\t\n"],
            "command": "git push --force",
            "expected_matched": None,
        },
        {
            "id": "parsing_whitespace_stripped_entry",
            "deny_config": ["  git push*  "],
            "command": "git push --force origin main",
            "expected_matched": "git push*",
        },
        {
            "id": "parsing_mixed_non_strings_filtered",
            "deny_config": [123, None, True, {"bad": "type"}, "rm *"],
            "command": "rm -rf /tmp/foo",
            "expected_matched": "rm *",
        },
        {
            "id": "parsing_first_rule_wins_order",
            "deny_config": ["git push origin *", "git push*", "*git*"],
            "command": "git push origin main",
            "expected_matched": "git push origin *",
        },
        {
            "id": "parsing_preserves_original_pattern_case_in_return",
            "deny_config": ["Git Push *"],
            "command": "git push --force",
            "expected_matched": "Git Push *",
        },
    ]

    results = []
    for spec in test_specs:
        with isolated_deny_context(deny_rules=spec["deny_config"]):
            matched = ta._match_user_deny_rule(spec["command"])
            results.append({
                "id": spec["id"],
                "deny_config": spec["deny_config"],
                "command": spec["command"],
                "matched_pattern": matched,
            })
    return results


def run_normalization_and_variants_cases() -> List[Dict[str, Any]]:
    """Test interaction of _command_detection_variants with _match_user_deny_rule."""
    specs = [
        {
            "id": "norm_case_insensitivity_upper_command",
            "deny_config": ["git push*"],
            "command": "GIT PUSH --FORCE ORIGIN MAIN",
            "expected_match": "git push*",
        },
        {
            "id": "norm_case_insensitivity_mixed_rule_and_command",
            "deny_config": ["gIt PuSh*"],
            "command": "GiT pUsH --force",
            "expected_match": "gIt PuSh*",
        },
        {
            "id": "norm_empty_double_quotes_stripped",
            "deny_config": ["git push*"],
            "command": 'git pu""sh --force origin main',
            "expected_match": "git push*",
        },
        {
            "id": "norm_empty_single_quotes_stripped",
            "deny_config": ["rm *"],
            "command": "r''m -rf /tmp/scratch",
            "expected_match": "rm *",
        },
        {
            "id": "norm_backslash_escape_stripped",
            "deny_config": ["git push*"],
            "command": r"git p\ush --force origin main",
            "expected_match": "git push*",
        },
        {
            "id": "norm_word_deobfuscation_rm",
            "deny_config": ["rm *"],
            "command": r"r\m -rf /tmp/scratch",
            "expected_match": "rm *",
        },
        {
            "id": "norm_line_continuation_collapsed",
            "deny_config": ["git push*"],
            "command": "git push \\\n --force origin main",
            "expected_match": "git push*",
        },
        {
            "id": "norm_ifs_variable_expanded",
            "deny_config": ["rm *"],
            "command": "rm${IFS}-rf${IFS}/tmp/scratch",
            "expected_match": "rm *",
        },
        {
            "id": "norm_windows_path_flattened",
            "deny_config": ["del c:/users/*"],
            "command": r"del C:\Users\alice\.ssh\id_rsa",
            "expected_match": "del c:/users/*",
        },
        {
            "id": "norm_bash_c_payload_extracted",
            "deny_config": ["git push*"],
            "command": 'bash -c "git push origin main"',
            "expected_match": "git push*",
        },
        {
            "id": "norm_sh_lc_payload_extracted",
            "deny_config": ["rm *"],
            "command": 'sh -lc "rm -rf /tmp/foo"',
            "expected_match": "rm *",
        },
        {
            "id": "norm_nested_bash_in_bash",
            "deny_config": ["git push*"],
            "command": 'bash -c "bash -c \\"git push origin main\\""',
            "expected_match": None,
        },
        {
            "id": "norm_grep_quoted_pattern_masked",
            "deny_config": ["git push*"],
            "command": 'grep -E "git push origin" /var/log/syslog',
            "expected_match": None,
        },
    ]

    results = []
    for s in specs:
        with isolated_deny_context(deny_rules=s["deny_config"]):
            matched = ta._match_user_deny_rule(s["command"])
            variants = list(ta._command_detection_variants(s["command"]))
            results.append({
                "id": s["id"],
                "command": s["command"],
                "deny_config": s["deny_config"],
                "matched_pattern": matched,
                "variants_evaluated": variants,
            })
    return results


def run_fnmatch_glob_semantics_cases() -> List[Dict[str, Any]]:
    """Test fnmatch wildcard capabilities and full-string anchoring."""
    specs = [
        {
            "id": "glob_star_prefix",
            "rule": "*push",
            "command": "git push",
            "matches": True,
        },
        {
            "id": "glob_star_prefix_fails_without_suffix_star",
            "rule": "*push",
            "command": "git push origin main",
            "matches": False,
        },
        {
            "id": "glob_star_suffix",
            "rule": "git push*",
            "command": "git push origin main",
            "matches": True,
        },
        {
            "id": "glob_star_infix",
            "rule": "git*main",
            "command": "git push origin main",
            "matches": True,
        },
        {
            "id": "glob_star_matches_slashes_and_flags",
            "rule": "rm -rf *",
            "command": "rm -rf /etc/hosts",
            "matches": True,
        },
        {
            "id": "glob_question_mark_single_char",
            "rule": "rm -? foo",
            "command": "rm -r foo",
            "matches": True,
        },
        {
            "id": "glob_question_mark_does_not_match_two_chars",
            "rule": "rm -? foo",
            "command": "rm -rf foo",
            "matches": False,
        },
        {
            "id": "glob_bracket_character_class",
            "rule": "rm -[rf]* foo",
            "command": "rm -r foo",
            "matches": True,
        },
        {
            "id": "glob_bracket_negation_class_reject",
            "rule": "rm -[!f]* foo",
            "command": "rm -f foo",
            "matches": False,
        },
        {
            "id": "glob_bracket_negation_class_accept",
            "rule": "rm -[!f]* foo",
            "command": "rm -r foo",
            "matches": True,
        },
        {
            "id": "glob_exact_match_without_wildcard_rejects_arguments",
            "rule": "git push",
            "command": "git push origin main",
            "matches": False,
        },
        {
            "id": "glob_exact_match_without_wildcard_accepts_exact",
            "rule": "git push",
            "command": "git push",
            "matches": True,
        },
    ]

    results = []
    for s in specs:
        with isolated_deny_context(deny_rules=[s["rule"]]):
            matched = ta._match_user_deny_rule(s["command"])
            results.append({
                "id": s["id"],
                "rule": s["rule"],
                "command": s["command"],
                "matched": matched is not None,
                "matched_pattern": matched,
            })
    return results


def run_command_coverage_and_boundary_cases() -> List[Dict[str, Any]]:
    """Test compound commands, pipelines, subshells, and wrapper commands.

    This captures the verified boundary where command-anchored rules (e.g. 'rm *')
    fail to match commands preceded by compounds, pipes, subshells, or wrappers,
    while wildcard-wrapped rules (e.g. '*rm *') succeed.
    """
    commands_to_test = [
        ("bare_command", "git push --force"),
        ("compound_and", "echo 'starting' && git push --force"),
        ("compound_semicolon", "echo 'starting' ; git push --force"),
        ("compound_or", "false || git push --force"),
        ("pipeline", "cat changes.patch | git push --force"),
        ("subshell_parens", "(git push --force)"),
        ("brace_group", "{ git push --force; }"),
        ("env_prefix", "GIT_TRACE=1 git push --force"),
        ("sudo_wrapper", "sudo git push --force"),
        ("sudo_with_options", "sudo -u deployer git push --force"),
    ]

    results = []
    for label, cmd in commands_to_test:
        # Evaluate with leading-anchored rule: "git push*"
        with isolated_deny_context(deny_rules=["git push*"]):
            lead_match = ta._match_user_deny_rule(cmd)

        # Evaluate with unanchored wildcard rule: "*git push*"
        with isolated_deny_context(deny_rules=["*git push*"]):
            wide_match = ta._match_user_deny_rule(cmd)

        variants = list(ta._command_detection_variants(cmd))

        results.append({
            "label": label,
            "command": cmd,
            "leading_anchored_match": lead_match,
            "wildcard_wrapped_match": wide_match,
            "detection_variants": variants,
        })
    return results


def run_precedence_and_bypass_cases() -> List[Dict[str, Any]]:
    """Test precedence across container skip, hardline, sudo-stdin, deny, yolo, mode:off, allowlist."""
    specs = [
        {
            "id": "container_isolated_docker_skips_deny_rule",
            "command": "git push --force origin main",
            "env_type": "docker",
            "has_host_access": False,
            "deny_rules": ["git push*"],
            "mode": "manual",
            "yolo": False,
            "allowlist": [],
            "expected_verdict": "approved",
            "expected_user_deny": False,
        },
        {
            "id": "container_docker_with_host_access_enforces_deny_rule",
            "command": "git push --force origin main",
            "env_type": "docker",
            "has_host_access": True,
            "deny_rules": ["git push*"],
            "mode": "manual",
            "yolo": False,
            "allowlist": [],
            "expected_verdict": "blocked",
            "expected_user_deny": True,
        },
        {
            "id": "hardline_beats_deny_rule",
            "command": "rm -rf /",
            "env_type": "local",
            "has_host_access": False,
            "deny_rules": ["*"],
            "mode": "off",
            "yolo": True,
            "allowlist": ["rm -rf /"],
            "expected_verdict": "hardline_blocked",
            "expected_user_deny": False,
        },
        {
            "id": "sudo_stdin_guessing_beats_deny_rule",
            "command": "echo secret | sudo -S ls",
            "env_type": "local",
            "has_host_access": False,
            "deny_rules": ["*"],
            "mode": "off",
            "yolo": True,
            "allowlist": [],
            "expected_verdict": "sudo_stdin_blocked",
            "expected_user_deny": False,
        },
        {
            "id": "deny_beats_yolo_mode",
            "command": "git push --force origin main",
            "env_type": "local",
            "has_host_access": False,
            "deny_rules": ["git push*"],
            "mode": "manual",
            "yolo": True,
            "allowlist": [],
            "expected_verdict": "blocked",
            "expected_user_deny": True,
        },
        {
            "id": "deny_beats_mode_off",
            "command": "git push --force origin main",
            "env_type": "local",
            "has_host_access": False,
            "deny_rules": ["git push*"],
            "mode": "off",
            "yolo": False,
            "allowlist": [],
            "expected_verdict": "blocked",
            "expected_user_deny": True,
        },
        {
            "id": "deny_beats_permanent_allowlist",
            "command": "git push --force origin main",
            "env_type": "local",
            "has_host_access": False,
            "deny_rules": ["git push*"],
            "mode": "manual",
            "yolo": False,
            "allowlist": ["git push --force origin main"],
            "expected_verdict": "blocked",
            "expected_user_deny": True,
        },
        {
            "id": "mode_off_allows_non_denied_dangerous_command",
            "command": "chmod -R 777 /workspace",
            "env_type": "local",
            "has_host_access": False,
            "deny_rules": ["git push*"],
            "mode": "off",
            "yolo": False,
            "allowlist": [],
            "expected_verdict": "approved",
            "expected_user_deny": False,
        },
        {
            "id": "mode_off_allows_safe_command",
            "command": "ls -la /workspace",
            "env_type": "local",
            "has_host_access": False,
            "deny_rules": ["git push*"],
            "mode": "off",
            "yolo": False,
            "allowlist": [],
            "expected_verdict": "approved",
            "expected_user_deny": False,
        },
    ]

    results = []
    for s in specs:
        with isolated_deny_context(
            deny_rules=s["deny_rules"],
            approval_mode=s["mode"],
            yolo_mode=s["yolo"],
            allowlist=s["allowlist"],
        ):
            guard_res = ta.check_all_command_guards(
                s["command"],
                s["env_type"],
                has_host_access=s["has_host_access"],
            )

            # Classify outcome
            if guard_res.get("approved"):
                verdict = "approved"
            elif guard_res.get("hardline"):
                verdict = "hardline_blocked"
            elif (
                "sudo" in guard_res.get("message", "").lower()
                and "pipe passwords" in guard_res.get("message", "").lower()
            ):
                verdict = "sudo_stdin_blocked"
            elif guard_res.get("user_deny"):
                verdict = "blocked"
            else:
                verdict = "other_blocked"

            results.append({
                "id": s["id"],
                "command": s["command"],
                "env_type": s["env_type"],
                "has_host_access": s["has_host_access"],
                "deny_rules": s["deny_rules"],
                "mode": s["mode"],
                "yolo": s["yolo"],
                "allowlist": s["allowlist"],
                "guard_result": guard_res,
                "classified_verdict": verdict,
            })
    return results


def run_terminal_tool_envelope_cases() -> List[Dict[str, Any]]:
    """Test the exact serialized JSON returned by terminal_tool for deny, hardline, and execution."""
    specs = [
        {
            "id": "envelope_user_deny_block",
            "command": "git push --force origin main",
            "deny_rules": ["git push*"],
            "mode": "off",
            "expected_status": "blocked",
            "expected_exit_code": -1,
        },
        {
            "id": "envelope_hardline_block_over_deny",
            "command": "rm -rf /",
            "deny_rules": ["*"],
            "mode": "off",
            "expected_status": "blocked",
            "expected_exit_code": -1,
        },
        {
            "id": "envelope_sudo_stdin_block_over_deny",
            "command": "echo pass | sudo -S whoami",
            "deny_rules": ["*"],
            "mode": "off",
            "expected_status": "blocked",
            "expected_exit_code": -1,
        },
        {
            "id": "envelope_mode_off_allowed_exec",
            "command": "echo 'safe command'",
            "deny_rules": ["git push*"],
            "mode": "off",
            "expected_status": None,  # omitted on success
            "expected_exit_code": 0,
        },
    ]

    results = []
    for s in specs:
        with isolated_deny_context(
            deny_rules=s["deny_rules"],
            approval_mode=s["mode"],
        ):
            raw_json = tt.terminal_tool(s["command"])
            envelope = json.loads(raw_json)
            results.append({
                "id": s["id"],
                "command": s["command"],
                "deny_rules": s["deny_rules"],
                "mode": s["mode"],
                "raw_envelope": envelope,
            })
    return results


def run_config_reload_and_lkg_cases(tmp_dir: Path) -> List[Dict[str, Any]]:
    """Test mtime invalidation and last-known-good retention on corrupt config.yaml."""
    import time
    from hermes_cli import config as cfg_mod

    cfg_mod._CONFIG_PARSE_WARNED.clear()
    home_dir = tmp_dir / "hermes_test_home"
    home_dir.mkdir(parents=True, exist_ok=True)
    cfg_file = home_dir / "config.yaml"

    results = []

    with patch.dict(os.environ, {"HERMES_HOME": str(home_dir)}):
        # 1. Initial valid configuration with deny rule
        cfg_file.write_text("approvals:\n  mode: 'off'\n  deny:\n    - 'git push*'\n")
        conf1 = cfg_mod.load_config_readonly()
        deny1 = conf1.get("approvals", {}).get("deny")
        results.append({
            "stage": "initial_valid",
            "deny_rules": deny1,
            "mode": conf1.get("approvals", {}).get("mode"),
        })

        # 2. Update config file with additional deny rule (mtime changed)
        time.sleep(0.05)
        cfg_file.write_text(
            "approvals:\n  mode: 'off'\n  deny:\n    - 'git push*'\n    - 'rm *'\n"
        )
        conf2 = cfg_mod.load_config_readonly()
        deny2 = conf2.get("approvals", {}).get("deny")
        results.append({
            "stage": "updated_mtime",
            "deny_rules": deny2,
            "mode": conf2.get("approvals", {}).get("mode"),
        })

        # 3. Corrupt config file with invalid YAML
        time.sleep(0.05)
        cfg_file.write_text("approvals:\n  mode: [unclosed\n  ::bad YAML {{{\n")
        conf3 = cfg_mod.load_config_readonly()
        deny3 = conf3.get("approvals", {}).get("deny")
        results.append({
            "stage": "corrupt_yaml_lkg_retained",
            "deny_rules": deny3,
            "mode": conf3.get("approvals", {}).get("mode"),
            "matches_previous": deny3 == deny2,
        })

    return results


# ---------------------------------------------------------------------------
# Main Orchestrator
# ---------------------------------------------------------------------------


def generate_golden_data() -> Dict[str, Any]:
    """Execute all reference test suites against the real Python code."""
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        tmp_path = Path(td)
        parsing = run_rule_parsing_cases()
        norm = run_normalization_and_variants_cases()
        globs = run_fnmatch_glob_semantics_cases()
        coverage = run_command_coverage_and_boundary_cases()
        precedence = run_precedence_and_bypass_cases()
        envelopes = run_terminal_tool_envelope_cases()
        reload_lkg = run_config_reload_and_lkg_cases(tmp_path)

    metadata = {
        "description": "Deterministic goldens for approvals.deny matching, mode: off, and terminal tool integration",
        "authority_sources": [
            "tools/approval.py: _match_user_deny_rule",
            "tools/approval.py: _user_deny_block_result",
            "tools/approval.py: _command_detection_variants",
            "tools/approval.py: check_all_command_guards",
            "tools/terminal_tool.py: terminal_tool pre-execution gate",
            "hermes_cli/config.py: load_config_readonly and LKG fallback",
        ],
        "test_counts": {
            "rule_parsing": len(parsing),
            "normalization_and_variants": len(norm),
            "fnmatch_glob_semantics": len(globs),
            "command_coverage_and_boundaries": len(coverage),
            "precedence_and_bypass": len(precedence),
            "terminal_tool_envelopes": len(envelopes),
            "config_reload_and_lkg": len(reload_lkg),
            "total_cases": (
                len(parsing)
                + len(norm)
                + len(globs)
                + len(coverage)
                + len(precedence)
                + len(envelopes)
                + len(reload_lkg)
            ),
        },
    }

    return normalize_repository_text({
        "metadata": metadata,
        "rule_parsing": parsing,
        "normalization_and_variants": norm,
        "fnmatch_glob_semantics": globs,
        "command_coverage_and_boundaries": coverage,
        "precedence_and_bypass": precedence,
        "terminal_tool_envelopes": envelopes,
        "config_reload_and_lkg": reload_lkg,
    })


def normalize_repository_text(value: Any) -> Any:
    """Keep captured Python messages while honoring the repository text style."""
    if isinstance(value, str):
        return value.replace("\N{EM DASH}", "-")
    if isinstance(value, list):
        return [normalize_repository_text(item) for item in value]
    if isinstance(value, dict):
        return {key: normalize_repository_text(item) for key, item in value.items()}
    return value


def main():
    parser = argparse.ArgumentParser(
        description="Oracle for approvals.deny Python contract and goldens"
    )
    parser.add_argument(
        "--check",
        "--verify",
        action="store_true",
        help="Check checked-in goldens against newly executed results without overwriting",
    )
    args = parser.parse_args()

    fresh_data = generate_golden_data()

    if args.check:
        if not GOLDEN_PATH.exists():
            print(f"ERROR: Golden file does not exist: {GOLDEN_PATH}", file=sys.stderr)
            sys.exit(1)
        with open(GOLDEN_PATH, "r", encoding="utf-8") as f:
            existing_data = json.load(f)

        fresh_json = json.dumps(
            fresh_data, indent=2, sort_keys=True, ensure_ascii=False
        )
        existing_json = json.dumps(
            existing_data, indent=2, sort_keys=True, ensure_ascii=False
        )

        if fresh_json != existing_json:
            print("ERROR: Golden data does not match fresh execution!", file=sys.stderr)
            sys.exit(1)
        print(
            f"SUCCESS: Golden data verified ({fresh_data['metadata']['test_counts']['total_cases']} cases match)."
        )
    else:
        with open(GOLDEN_PATH, "w", encoding="utf-8") as f:
            json.dump(fresh_data, f, indent=2, sort_keys=True, ensure_ascii=False)
            f.write("\n")
        print(
            f"Wrote {fresh_data['metadata']['test_counts']['total_cases']} golden cases to {GOLDEN_PATH}"
        )


if __name__ == "__main__":
    main()
