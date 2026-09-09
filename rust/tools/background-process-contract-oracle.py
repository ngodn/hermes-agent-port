#!/usr/bin/env python3
"""Golden contract oracle for native local non-PTY background process runtime.

This script source-executes the REAL Python implementations under:
  - tools/process_registry.py (ProcessRegistry, ProcessSession, spawn_local,
    _reader_loop, poll, read_log, wait, kill_process, write_stdin, submit_stdin,
    close_stdin, list_sessions, _resolve_prefix, _handle_process)
  - tools/terminal_tool.py (_handle_terminal, terminal_tool background dispatch,
    _validate_workdir)
  - agent/redact.py (redact_terminal_output, redact_sensitive_text)

It generates or verifies `rust/tools/background-process-contract-goldens.json` to
define the exact behavioral and structural contract that the native Rust port
(such as `rust/crates/hermes-gateway/src/background_process.rs`) must replicate.

Key properties:
  - Source execution: real Python methods and subprocesses are executed.
  - Offline & credential-free: no external services or network calls.
  - Child cleanup: all spawned child processes are killed and reaped in finally blocks.
  - Deterministic: temporary directories, OS PIDs, timestamps, and UUIDs are normalized.
  - Verification: supports `--check` / `--verify` to ensure checked-in goldens match byte-for-byte.

Explicit Exclusions (documented in `_meta` and companion analysis):
  - PTY: pseudo-terminal allocation and interactive TUI escape sequences excluded.
  - Remote backends: Docker, Singularity, Modal, Daytona, and SSH backends excluded; local only.
  - Notifications & watchers: notify=True queue injection, watch_patterns regex triggers, and background watcher tasks excluded.
  - Systemd cgroup isolation: systemd-run --user --scope wrapping excluded.
  - Delegation attribution: subagent delegation tracking excluded.
  - Restart adoption: recovery from processes.json after gateway restart excluded.
  - Platform-specific Windows behavior: Windows ConPTY CRLF cooked input, taskkill /T /F, and ASLR crash recovery excluded.

Usage:
  .venv/bin/python rust/tools/background-process-contract-oracle.py            # regenerate goldens
  .venv/bin/python rust/tools/background-process-contract-oracle.py --check    # verify goldens
"""

from __future__ import annotations

import argparse
import atexit
import contextlib
import json
import logging
import os
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path
from typing import Any, Dict, List, Optional

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT = Path(__file__).resolve().parent / "background-process-contract-goldens.json"

sys.path.insert(0, str(REPO_ROOT))

# Silence loggers during execution so stdout/stderr stays clean
logging.getLogger("tools.process_registry").setLevel(logging.CRITICAL)
logging.getLogger("tools.terminal_tool").setLevel(logging.CRITICAL)
logging.getLogger("tools.approval").setLevel(logging.CRITICAL)
logging.getLogger("agent.redact").setLevel(logging.CRITICAL)

import agent.redact as redact_mod  # noqa: E402
import tools.process_registry as pr  # noqa: E402
import tools.terminal_tool as tt  # noqa: E402


# ---------------------------------------------------------------------------
# Child Process & Environment Tracker
# ---------------------------------------------------------------------------
class ProcessTracker:
    """Tracks spawned processes and ensures guaranteed cleanup in finally blocks."""

    def __init__(self, temp_dir: str):
        self.temp_dir = temp_dir
        self.spawned_pids: List[int] = []
        self.spawned_sessions: List[pr.ProcessSession] = []
        self.pid_map: Dict[int, int] = {}
        self._next_virtual_pid = 9001
        self._lock = threading.Lock()

    def track_session(self, session: Optional[pr.ProcessSession]) -> None:
        if session is None:
            return
        with self._lock:
            self.spawned_sessions.append(session)
            if session.pid is not None:
                self.spawned_pids.append(session.pid)
                if session.pid not in self.pid_map:
                    self.pid_map[session.pid] = self._next_virtual_pid
                    self._next_virtual_pid += 1

    def track_popen(self, proc: subprocess.Popen) -> None:
        if proc is None or proc.pid is None:
            return
        with self._lock:
            self.spawned_pids.append(proc.pid)
            if proc.pid not in self.pid_map:
                self.pid_map[proc.pid] = self._next_virtual_pid
                self._next_virtual_pid += 1

    def virtual_pid(self, pid: Optional[int]) -> Optional[int]:
        if pid is None:
            return None
        with self._lock:
            if pid not in self.pid_map:
                self.pid_map[pid] = self._next_virtual_pid
                self._next_virtual_pid += 1
            return self.pid_map[pid]

    def cleanup(self) -> None:
        """Terminate and reap all tracked processes."""
        with self._lock:
            for session in list(self.spawned_sessions):
                proc = getattr(session, "process", None)
                if proc is not None:
                    try:
                        if proc.poll() is None:
                            proc.terminate()
                            try:
                                proc.wait(timeout=0.5)
                            except subprocess.TimeoutExpired:
                                proc.kill()
                                proc.wait(timeout=0.5)
                    except Exception:
                        pass

            for pid in list(self.spawned_pids):
                try:
                    os.kill(pid, signal.SIGKILL)
                except OSError:
                    pass


# ---------------------------------------------------------------------------
# Normalization Helper for Deterministic Goldens
# ---------------------------------------------------------------------------
def normalize_value(val: Any, tracker: ProcessTracker, temp_dir: str) -> Any:
    """Recursively normalize dynamic values (PIDs, timestamps, temp paths)."""
    if isinstance(val, dict):
        normalized = {}
        for k, v in val.items():
            if k == "pid":
                normalized[k] = tracker.virtual_pid(v) if isinstance(v, int) else v
            elif k == "uptime_seconds":
                normalized[k] = 0
            elif k == "started_at" and isinstance(v, str):
                normalized[k] = "2026-09-09T12:00:00"
            elif k == "cwd" and isinstance(v, str) and temp_dir in v:
                normalized[k] = "[TEMPDIR]"
            elif k == "timeout_note" and isinstance(v, str):
                s = v.replace(temp_dir, "[TEMPDIR]").replace("\N{EM DASH}", "-")
                s = re.sub(r" Uptime: \d+s\.", " Uptime: 0s.", s)
                normalized[k] = s
            else:
                normalized[k] = normalize_value(v, tracker, temp_dir)
        return normalized
    elif isinstance(val, list):
        return [normalize_value(item, tracker, temp_dir) for item in val]
    elif isinstance(val, str):
        s = val.replace(temp_dir, "[TEMPDIR]").replace("\N{EM DASH}", "-")
        s = re.sub(r" Uptime: \d+s\.", " Uptime: 0s.", s)
        if re.match(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}$", s):
            return "2026-09-09T12:00:00"
        return s
    return val


def wait_for_status(
    registry: pr.ProcessRegistry,
    session_id: str,
    target_status: str,
    timeout: float = 5.0,
    interval: float = 0.02,
) -> bool:
    """Poll process status until target status is reached or timeout elapses."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        res = registry.poll(session_id)
        if res.get("status") == target_status:
            return True
        time.sleep(interval)
    return False


def wait_for_output_substring(
    registry: pr.ProcessRegistry,
    session_id: str,
    substring: str,
    timeout: float = 5.0,
    interval: float = 0.02,
) -> bool:
    """Poll until output buffer contains substring or timeout elapses."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        sess = registry.get(session_id)
        if sess is not None:
            with sess._lock:
                if substring in sess.output_buffer:
                    return True
        time.sleep(interval)
    return False


# ---------------------------------------------------------------------------
# Isolated Test Context
# ---------------------------------------------------------------------------
@contextlib.contextmanager
def isolated_contract_context():
    """Hermetic context manager isolating HERMES_HOME, config, env, and UUID generation."""
    temp_dir = tempfile.mkdtemp(prefix="hermes_bg_contract_")
    tracker = ProcessTracker(temp_dir)
    atexit.register(tracker.cleanup)

    # Deterministic sequential UUID generator
    uuid_counter = 0

    def deterministic_uuid4() -> uuid.UUID:
        nonlocal uuid_counter
        uuid_counter += 1
        hex_str = f"{uuid_counter:012x}" + "0" * 20
        return uuid.UUID(hex=hex_str)

    # Isolate environment variables
    saved_env = dict(os.environ)
    clean_env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith("HERMES_")
        and k not in ("SUDO_PASSWORD", "TERMINAL_TIMEOUT")
    }
    clean_env["HERMES_HOME"] = temp_dir
    clean_env["TERMINAL_TIMEOUT"] = "180"
    clean_env["TERMINAL_ENV"] = "local"
    os.environ.clear()
    os.environ.update(clean_env)

    # Point checkpoint path to temp directory
    orig_checkpoint_path = pr.CHECKPOINT_PATH
    pr.CHECKPOINT_PATH = Path(temp_dir) / "processes.json"

    # Reset systemd scope cache to ensure exclusion of systemd scopes
    orig_scope_avail = pr._SYSTEMD_SCOPE_AVAILABLE
    pr._SYSTEMD_SCOPE_AVAILABLE = False

    # Force redact enabled for deterministic secret testing
    orig_redact_enabled = redact_mod._REDACT_ENABLED
    redact_mod._REDACT_ENABLED = True

    # Patch uuid.uuid4
    orig_uuid4 = uuid.uuid4
    uuid.uuid4 = deterministic_uuid4

    try:
        yield tracker, temp_dir
    finally:
        tracker.cleanup()
        uuid.uuid4 = orig_uuid4
        pr.CHECKPOINT_PATH = orig_checkpoint_path
        pr._SYSTEMD_SCOPE_AVAILABLE = orig_scope_avail
        redact_mod._REDACT_ENABLED = orig_redact_enabled
        os.environ.clear()
        os.environ.update(saved_env)
        try:
            import shutil

            shutil.rmtree(temp_dir, ignore_errors=True)
        except Exception:
            pass


# ---------------------------------------------------------------------------
# Section 1: Meta
# ---------------------------------------------------------------------------
def build_meta() -> Dict[str, Any]:
    return {
        "authoritative_sources": [
            "tools/process_registry.py: ProcessRegistry, ProcessSession, spawn_local, _reader_loop, poll, read_log, wait, kill_process, write_stdin, submit_stdin, close_stdin, list_sessions, _resolve_prefix, _handle_process",
            "tools/terminal_tool.py: terminal_tool (background=true path), _handle_terminal, _validate_workdir",
            "agent/redact.py: redact_terminal_output, redact_sensitive_text",
        ],
        "description": (
            "Golden reference contract for bounded local non-PTY background process execution, "
            "process registry lifecycle, prefix resolution, ownership filtering, stdin handling, "
            "and secret redaction. Every value is source-executed from real Python repository code."
        ),
        "exclusions": {
            "delegation_attribution": (
                "Async subagent delegation tracking and durable task attribution are excluded from this slice."
            ),
            "notifications_watchers": (
                "notify=true completion queue delivery, watch_patterns regex matches, and gateway watcher intervals are excluded."
            ),
            "platform_windows": (
                "Windows-specific pywinpty, ConPTY CRLF cooked input, taskkill /T /F tree kill, and ASLR crash recovery are excluded."
            ),
            "pty": (
                "Pseudo-terminal allocation (ptyprocess) and interactive terminal TUI escape handling are excluded; local pipe mode is authoritative."
            ),
            "remote_backends": (
                "Docker, Singularity, Modal, Daytona, and SSH remote sandboxes are excluded; local host execution is authoritative."
            ),
            "restart_adoption": (
                "Crash recovery and orphan adoption from processes.json across gateway restarts are excluded."
            ),
            "systemd_cgroup_isolation": (
                "systemd-run --user --scope transient cgroup wrapping and systemd-oomd protection are excluded."
            ),
        },
        "schema_version": "1.0.0",
    }


# ---------------------------------------------------------------------------
# Section 2: Spawn and Status
# ---------------------------------------------------------------------------
def build_spawn_and_status_cases(
    tracker: ProcessTracker, temp_dir: str
) -> List[Dict[str, Any]]:
    cases = []
    reg = pr.ProcessRegistry()

    # Case 1: spawn_local basic command
    sess = reg.spawn_local(
        command="echo 'hello background'",
        cwd=temp_dir,
        task_id="task_alpha",
        session_key="gw_sess_alpha",
    )
    tracker.track_session(sess)
    wait_for_status(reg, sess.id, "exited", timeout=5.0)

    sess_snapshot = {
        "command": sess.command,
        "cwd": "[TEMPDIR]",
        "exited": sess.exited,
        "exit_code": sess.exit_code,
        "id": sess.id,
        "owner_task_id": sess.owner_task_id,
        "pid": tracker.virtual_pid(sess.pid),
        "session_key": sess.session_key,
        "task_id": sess.task_id,
    }
    cases.append({
        "id": "spawn_local_basic_session",
        "input": {
            "command": "echo 'hello background'",
            "cwd": "[TEMPDIR]",
            "session_key": "gw_sess_alpha",
            "task_id": "task_alpha",
            "use_pty": False,
        },
        "observed": sess_snapshot,
    })

    # Case 2: terminal_tool background spawn success envelope
    orig_reg = pr.process_registry
    try:
        pr.process_registry = reg
        term_res_raw = tt.terminal_tool(
            command="echo 'spawned via terminal'",
            background=True,
            workdir=temp_dir,
        )
        term_res = json.loads(term_res_raw)
        if term_res.get("session_id"):
            created_sess = reg.get(term_res["session_id"])
            tracker.track_session(created_sess)
            wait_for_status(reg, term_res["session_id"], "exited", timeout=5.0)
        cases.append({
            "id": "terminal_tool_background_success",
            "input": {
                "background": True,
                "command": "echo 'spawned via terminal'",
                "workdir": "[TEMPDIR]",
            },
            "observed": normalize_value(term_res, tracker, temp_dir),
        })
    finally:
        pr.process_registry = orig_reg

    # Case 3: terminal_tool invalid command type rejection
    bad_cmd_res = json.loads(
        tt.terminal_tool(command=12345, background=True, workdir=temp_dir)
    )
    cases.append({
        "id": "terminal_tool_invalid_command_type",
        "input": {"background": True, "command": 12345},
        "observed": bad_cmd_res,
    })

    # Case 4: terminal_tool forbidden workdir metacharacter rejection
    forbidden_workdir_res = json.loads(
        tt.terminal_tool(
            command="echo safe",
            background=True,
            workdir="/tmp/test;rm -rf",
        )
    )
    cases.append({
        "id": "terminal_tool_forbidden_workdir_metachar",
        "input": {
            "background": True,
            "command": "echo safe",
            "workdir": "/tmp/test;rm -rf",
        },
        "observed": forbidden_workdir_res,
    })

    # Case 5: _handle_terminal misplaced code parameter recovery
    code_recovery_res = json.loads(
        tt._handle_terminal({"code": "print('hello')", "background": True})
    )
    cases.append({
        "id": "handle_terminal_code_misplaced_parameter",
        "input": {"background": True, "code": "print('hello')"},
        "observed": code_recovery_res,
    })

    # Case 6: _handle_terminal notify on foreground rejection
    notify_fg_res = json.loads(
        tt._handle_terminal({"command": "ls", "notify": True, "background": False})
    )
    cases.append({
        "id": "handle_terminal_notify_on_foreground_rejected",
        "input": {"background": False, "command": "ls", "notify": True},
        "observed": notify_fg_res,
    })

    # Case 7: _handle_terminal pty on foreground rejection
    pty_fg_res = json.loads(
        tt._handle_terminal({"command": "ls", "pty": True, "background": False})
    )
    cases.append({
        "id": "handle_terminal_pty_on_foreground_rejected",
        "input": {"background": False, "command": "ls", "pty": True},
        "observed": pty_fg_res,
    })

    return cases


# ---------------------------------------------------------------------------
# Section 3: Poll and Log Output
# ---------------------------------------------------------------------------
def build_poll_and_log_cases(
    tracker: ProcessTracker, temp_dir: str
) -> List[Dict[str, Any]]:
    cases = []
    reg = pr.ProcessRegistry()

    # Case 1: Poll running process (synchronized via output)
    sleep_sess = reg.spawn_local(
        "python3 -u -c 'import time; print(\"RUNNING\", flush=True); time.sleep(10)'",
        cwd=temp_dir,
    )
    tracker.track_session(sleep_sess)
    wait_for_output_substring(reg, sleep_sess.id, "RUNNING", timeout=5.0)
    poll_running = reg.poll(sleep_sess.id)
    cases.append({
        "id": "poll_running_process",
        "input": {"action": "poll", "session_id": sleep_sess.id},
        "observed": normalize_value(poll_running, tracker, temp_dir),
    })
    reg.kill_process(sleep_sess.id)

    # Case 2: Poll exited process with multiline output
    out_sess = reg.spawn_local(
        'python3 -c \'print("line 1"); print("line 2")\'', cwd=temp_dir
    )
    tracker.track_session(out_sess)
    wait_for_status(reg, out_sess.id, "exited", timeout=5.0)
    poll_exited = reg.poll(out_sess.id)
    cases.append({
        "id": "poll_exited_process_with_output",
        "input": {"action": "poll", "session_id": out_sess.id},
        "observed": normalize_value(poll_exited, tracker, temp_dir),
    })

    # Case 3: Poll ANSI color code stripping
    ansi_sess = reg.spawn_local(
        "printf '\\033[31mRed\\033[0m \\033[32mGreen\\033[0m\\n'", cwd=temp_dir
    )
    tracker.track_session(ansi_sess)
    wait_for_status(reg, ansi_sess.id, "exited", timeout=5.0)
    poll_ansi = reg.poll(ansi_sess.id)
    cases.append({
        "id": "poll_strips_ansi_color_codes",
        "input": {"action": "poll", "session_id": ansi_sess.id},
        "observed": normalize_value(poll_ansi, tracker, temp_dir),
    })

    # Multiline session for read_log tests (5 lines)
    log_sess = reg.spawn_local(
        "printf 'alpha\\nbeta\\ngamma\\ndelta\\nepsilon\\n'", cwd=temp_dir
    )
    tracker.track_session(log_sess)
    wait_for_status(reg, log_sess.id, "exited", timeout=5.0)

    # Case 4: Full log read
    log_full = reg.read_log(log_sess.id)
    cases.append({
        "id": "read_log_full_default",
        "input": {
            "action": "log",
            "limit": 200,
            "offset": None,
            "session_id": log_sess.id,
        },
        "observed": normalize_value(log_full, tracker, temp_dir),
    })

    # Case 5: Paged log read from start
    log_start = reg.read_log(log_sess.id, offset=0, limit=2)
    cases.append({
        "id": "read_log_paged_from_start",
        "input": {"action": "log", "limit": 2, "offset": 0, "session_id": log_sess.id},
        "observed": normalize_value(log_start, tracker, temp_dir),
    })

    # Case 6: Paged log read middle window
    log_mid = reg.read_log(log_sess.id, offset=2, limit=2)
    cases.append({
        "id": "read_log_paged_middle",
        "input": {"action": "log", "limit": 2, "offset": 2, "session_id": log_sess.id},
        "observed": normalize_value(log_mid, tracker, temp_dir),
    })

    # Case 7: Paged log read tail default (offset=None, limit=2)
    log_tail = reg.read_log(log_sess.id, offset=None, limit=2)
    cases.append({
        "id": "read_log_tail_default",
        "input": {
            "action": "log",
            "limit": 2,
            "offset": None,
            "session_id": log_sess.id,
        },
        "observed": normalize_value(log_tail, tracker, temp_dir),
    })

    # Case 8: Paged log read out of bounds offset
    log_oob = reg.read_log(log_sess.id, offset=10, limit=5)
    cases.append({
        "id": "read_log_offset_past_end",
        "input": {"action": "log", "limit": 5, "offset": 10, "session_id": log_sess.id},
        "observed": normalize_value(log_oob, tracker, temp_dir),
    })

    return cases


# ---------------------------------------------------------------------------
# Section 4: Bounded Wait
# ---------------------------------------------------------------------------
def build_bounded_wait_cases(
    tracker: ProcessTracker, temp_dir: str
) -> List[Dict[str, Any]]:
    cases = []
    reg = pr.ProcessRegistry()

    # Case 1: Fast process exits cleanly before timeout
    fast_sess = reg.spawn_local("echo 'job done'", cwd=temp_dir)
    tracker.track_session(fast_sess)
    wait_fast = reg.wait(fast_sess.id, timeout=5)
    cases.append({
        "id": "wait_fast_process_exits_cleanly",
        "input": {"session_id": fast_sess.id, "timeout": 5},
        "observed": normalize_value(wait_fast, tracker, temp_dir),
    })

    # Case 2: Slow process wait window expires
    slow_sess = reg.spawn_local(
        "python3 -u -c 'import time; print(\"WAITING\", flush=True); time.sleep(10)'",
        cwd=temp_dir,
    )
    tracker.track_session(slow_sess)
    wait_for_output_substring(reg, slow_sess.id, "WAITING", timeout=5.0)
    wait_timeout = reg.wait(slow_sess.id, timeout=1)
    cases.append({
        "id": "wait_timeout_expires_on_slow_process",
        "input": {"session_id": slow_sess.id, "timeout": 1},
        "observed": normalize_value(wait_timeout, tracker, temp_dir),
    })
    reg.kill_process(slow_sess.id)

    # Case 3: Zero timeout rejected
    wait_zero = reg.wait(fast_sess.id, timeout=0)
    cases.append({
        "id": "wait_rejects_zero_timeout",
        "input": {"session_id": fast_sess.id, "timeout": 0},
        "observed": wait_zero,
    })

    # Case 4: Negative timeout rejected
    wait_neg = reg.wait(fast_sess.id, timeout=-5)
    cases.append({
        "id": "wait_rejects_negative_timeout",
        "input": {"session_id": fast_sess.id, "timeout": -5},
        "observed": wait_neg,
    })

    # Case 5: Timeout clamped to configured TERMINAL_TIMEOUT
    saved_timeout = os.environ.get("TERMINAL_TIMEOUT")
    try:
        os.environ["TERMINAL_TIMEOUT"] = "5"
        clamped_sess = reg.spawn_local("echo 'clamped'", cwd=temp_dir)
        tracker.track_session(clamped_sess)
        wait_clamped = reg.wait(clamped_sess.id, timeout=10)
        cases.append({
            "id": "wait_timeout_clamped_to_configured_max",
            "input": {
                "configured_terminal_timeout": 5,
                "session_id": clamped_sess.id,
                "timeout": 10,
            },
            "observed": normalize_value(wait_clamped, tracker, temp_dir),
        })
    finally:
        if saved_timeout is not None:
            os.environ["TERMINAL_TIMEOUT"] = saved_timeout

    return cases


# ---------------------------------------------------------------------------
# Section 5: Exit Semantics
# ---------------------------------------------------------------------------
def build_exit_semantics_cases(
    tracker: ProcessTracker, temp_dir: str
) -> List[Dict[str, Any]]:
    cases = []
    reg = pr.ProcessRegistry()

    # Case 1: Natural zero exit
    zero_sess = reg.spawn_local("echo 'natural exit 0'", cwd=temp_dir)
    tracker.track_session(zero_sess)
    wait_for_status(reg, zero_sess.id, "exited", timeout=5.0)
    cases.append({
        "id": "natural_exit_zero",
        "input": {"command": "echo 'natural exit 0'"},
        "observed": normalize_value(reg.poll(zero_sess.id), tracker, temp_dir),
    })

    # Case 2: Natural non-zero exit (42)
    nonzero_sess = reg.spawn_local("sh -c 'exit 42'", cwd=temp_dir)
    tracker.track_session(nonzero_sess)
    wait_for_status(reg, nonzero_sess.id, "exited", timeout=5.0)
    cases.append({
        "id": "natural_exit_non_zero_42",
        "input": {"command": "sh -c 'exit 42'"},
        "observed": normalize_value(reg.poll(nonzero_sess.id), tracker, temp_dir),
    })

    # Case 3: Explicit kill of running process via kill_process
    kill_sess = reg.spawn_local(
        "python3 -u -c 'import time; print(\"READY\", flush=True); time.sleep(10)'",
        cwd=temp_dir,
    )
    tracker.track_session(kill_sess)
    wait_for_output_substring(reg, kill_sess.id, "READY", timeout=5.0)
    kill_result = reg.kill_process(kill_sess.id)
    cases.append({
        "id": "explicit_kill_via_kill_process",
        "input": {"action": "kill", "session_id": kill_sess.id},
        "observed": normalize_value(kill_result, tracker, temp_dir),
    })

    # Case 4: Redundant kill on already exited process
    redundant_kill = reg.kill_process(kill_sess.id)
    cases.append({
        "id": "redundant_kill_already_exited",
        "input": {"action": "kill", "session_id": kill_sess.id},
        "observed": normalize_value(redundant_kill, tracker, temp_dir),
    })

    # Case 5: External signal SIGKILL termination
    sig_sess = reg.spawn_local(
        "python3 -u -c 'import time; print(\"READY\", flush=True); time.sleep(10)'",
        cwd=temp_dir,
    )
    tracker.track_session(sig_sess)
    wait_for_output_substring(reg, sig_sess.id, "READY", timeout=5.0)
    if sig_sess.pid is not None:
        try:
            os.kill(sig_sess.pid, signal.SIGKILL)
        except OSError:
            pass
    wait_for_status(reg, sig_sess.id, "exited", timeout=5.0)
    sig_poll = reg.poll(sig_sess.id)
    cases.append({
        "id": "external_signal_sigkill",
        "input": {"signal": "SIGKILL", "target_pid": tracker.virtual_pid(sig_sess.pid)},
        "observed": normalize_value(sig_poll, tracker, temp_dir),
    })

    return cases


# ---------------------------------------------------------------------------
# Section 6: Prefix Lookup
# ---------------------------------------------------------------------------
def build_prefix_lookup_cases(
    tracker: ProcessTracker, temp_dir: str
) -> List[Dict[str, Any]]:
    cases = []
    reg = pr.ProcessRegistry()

    # Pre-populate registry with distinct session IDs
    s_a1 = pr.ProcessSession(id="proc_aaaa11112222", command="cmd a1")
    s_a2 = pr.ProcessSession(id="proc_aaaa33334444", command="cmd a2")
    s_b = pr.ProcessSession(id="proc_bbbb55556666", command="cmd b")
    reg._running[s_a1.id] = s_a1
    reg._running[s_a2.id] = s_a2
    reg._running[s_b.id] = s_b

    queries = [
        ("exact_full_id", "proc_bbbb55556666", "proc_bbbb55556666"),
        ("unique_4char_prefix_with_proc", "proc_bbbb", "proc_bbbb55556666"),
        ("unique_4char_bare_suffix", "bbbb", "proc_bbbb55556666"),
        ("too_short_prefix_3chars", "proc_bbb", None),
        ("too_short_bare_suffix_3chars", "bbb", None),
        ("ambiguous_prefix_with_proc", "proc_aaaa", None),
        ("ambiguous_bare_suffix", "aaaa", None),
        ("disambiguated_longer_prefix_a1", "aaaa1", "proc_aaaa11112222"),
        ("disambiguated_longer_prefix_a2", "proc_aaaa3", "proc_aaaa33334444"),
        ("nonexistent_id", "cccc", None),
        ("empty_string", "", None),
        ("whitespace_only", "   ", None),
    ]

    for case_id, query, expected_id in queries:
        resolved = reg.get(query)
        cases.append({
            "id": case_id,
            "input": {"query": query},
            "observed": {
                "matched": resolved is not None,
                "resolved_session_id": resolved.id if resolved else None,
            },
        })

    return cases


# ---------------------------------------------------------------------------
# Section 7: Ownership Filtering
# ---------------------------------------------------------------------------
def build_ownership_filtering_cases(
    tracker: ProcessTracker, temp_dir: str
) -> List[Dict[str, Any]]:
    cases = []
    reg = pr.ProcessRegistry()

    # Pre-populate registry sessions with different tasks and session keys
    s1 = pr.ProcessSession(
        id="proc_own_0001",
        command="cmd 1",
        cwd=temp_dir,
        task_id="task_parent",
        session_key="gw_sess_1",
        started_at=time.time(),
    )
    s2 = pr.ProcessSession(
        id="proc_own_0002",
        command="cmd 2",
        cwd=temp_dir,
        task_id="task_parent",
        session_key="gw_sess_1",
        started_at=time.time(),
        exited=True,
        exit_code=0,
    )
    s3 = pr.ProcessSession(
        id="proc_own_0003",
        command="cmd 3",
        cwd=temp_dir,
        task_id="task_child",
        session_key="gw_sess_1",
        started_at=time.time(),
    )
    s4 = pr.ProcessSession(
        id="proc_own_0004",
        command="cmd 4",
        cwd=temp_dir,
        task_id="task_foreign",
        session_key="gw_sess_2",
        started_at=time.time(),
    )
    reg._running[s1.id] = s1
    reg._finished[s2.id] = s2
    reg._running[s3.id] = s3
    reg._running[s4.id] = s4

    # Case 1: Unfiltered listing
    list_all = reg.list_sessions()
    cases.append({
        "id": "list_all_unfiltered",
        "input": {"session_key": None, "task_id": None},
        "observed": normalize_value(list_all, tracker, temp_dir),
    })

    # Case 2: Filter by task_id only
    list_task = reg.list_sessions(task_id="task_parent")
    cases.append({
        "id": "list_filter_by_task_id",
        "input": {"session_key": None, "task_id": "task_parent"},
        "observed": normalize_value(list_task, tracker, temp_dir),
    })

    # Case 3: Filter by task_id and session_key (surfaces session_scoped cross-task process)
    list_cross = reg.list_sessions(task_id="task_parent", session_key="gw_sess_1")
    cases.append({
        "id": "list_filter_by_task_and_session_key_cross_task",
        "input": {"session_key": "gw_sess_1", "task_id": "task_parent"},
        "observed": normalize_value(list_cross, tracker, temp_dir),
    })

    # Case 4: Filter by nonexistent task
    list_empty = reg.list_sessions(task_id="task_nonexistent")
    cases.append({
        "id": "list_filter_by_nonexistent_task",
        "input": {"session_key": None, "task_id": "task_nonexistent"},
        "observed": list_empty,
    })

    # Active checks
    cases.append({
        "id": "has_active_processes_checks",
        "observed": {
            "task_foreign": reg.has_active_processes("task_foreign"),
            "task_parent": reg.has_active_processes("task_parent"),
            "task_unknown": reg.has_active_processes("task_unknown"),
        },
    })
    cases.append({
        "id": "has_active_for_session_checks",
        "observed": {
            "gw_sess_1": reg.has_active_for_session("gw_sess_1"),
            "gw_sess_2": reg.has_active_for_session("gw_sess_2"),
            "gw_sess_unknown": reg.has_active_for_session("gw_sess_unknown"),
        },
    })

    # Bulk kill for task
    killed_count = reg.kill_all("task_parent")
    cases.append({
        "id": "kill_all_by_task_id",
        "input": {"task_id": "task_parent"},
        "observed": {
            "has_active_after_kill": reg.has_active_processes("task_parent"),
            "killed_count": killed_count,
        },
    })

    return cases


# ---------------------------------------------------------------------------
# Section 8: Stdin Lifecycle
# ---------------------------------------------------------------------------
def build_stdin_lifecycle_cases(
    tracker: ProcessTracker, temp_dir: str
) -> List[Dict[str, Any]]:
    cases = []
    reg = pr.ProcessRegistry()

    # Part A: Non-PTY default spawn (spawn_local attaches stdin=subprocess.DEVNULL)
    devnull_sess = reg.spawn_local(
        "python3 -u -c 'import time; print(\"BG\", flush=True); time.sleep(10)'",
        cwd=temp_dir,
    )
    tracker.track_session(devnull_sess)
    wait_for_output_substring(reg, devnull_sess.id, "BG", timeout=5.0)

    w_err = reg.write_stdin(devnull_sess.id, "data")
    s_err = reg.submit_stdin(devnull_sess.id, "data")
    c_err = reg.close_stdin(devnull_sess.id)

    cases.append({
        "id": "non_pty_default_write_stdin_error",
        "input": {"action": "write", "data": "data", "session_id": devnull_sess.id},
        "observed": w_err,
    })
    cases.append({
        "id": "non_pty_default_submit_stdin_error",
        "input": {"action": "submit", "data": "data", "session_id": devnull_sess.id},
        "observed": s_err,
    })
    cases.append({
        "id": "non_pty_default_close_stdin_error",
        "input": {"action": "close", "session_id": devnull_sess.id},
        "observed": c_err,
    })
    reg.kill_process(devnull_sess.id)

    # Part B: Pipe-backed process stdin lifecycle
    pipe_proc = subprocess.Popen(
        [
            sys.executable,
            "-u",
            "-c",
            "import sys; l = sys.stdin.readline(); print('GOT:' + l.strip(), flush=True); sys.stdin.read(); print('EOF_DONE', flush=True)",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        cwd=temp_dir,
    )
    tracker.track_popen(pipe_proc)

    pipe_sess = pr.ProcessSession(
        id="proc_pipetest01",
        command="python stdin pipe test",
        pid=pipe_proc.pid,
        process=pipe_proc,
        cwd=temp_dir,
        started_at=time.time(),
    )
    reg._running[pipe_sess.id] = pipe_sess
    tracker.track_session(pipe_sess)

    reader = threading.Thread(target=reg._reader_loop, args=(pipe_sess,), daemon=True)
    pipe_sess._reader_thread = reader
    reader.start()

    # 1. write_stdin (raw bytes, no newline)
    write_res = reg.write_stdin(pipe_sess.id, "part1 ")
    cases.append({
        "id": "pipe_stdin_write_raw_bytes",
        "input": {"action": "write", "data": "part1 ", "session_id": pipe_sess.id},
        "observed": write_res,
    })

    # 2. submit_stdin (appends \n)
    submit_res = reg.submit_stdin(pipe_sess.id, "line1")
    cases.append({
        "id": "pipe_stdin_submit_appends_newline",
        "input": {"action": "submit", "data": "line1", "session_id": pipe_sess.id},
        "observed": submit_res,
    })

    wait_for_output_substring(reg, pipe_sess.id, "GOT:part1 line1", timeout=5.0)

    # 3. close_stdin (sends EOF)
    close_res = reg.close_stdin(pipe_sess.id)
    cases.append({
        "id": "pipe_stdin_close_sends_eof",
        "input": {"action": "close", "session_id": pipe_sess.id},
        "observed": close_res,
    })

    wait_for_status(reg, pipe_sess.id, "exited", timeout=5.0)
    poll_done = reg.poll(pipe_sess.id)
    cases.append({
        "id": "pipe_stdin_poll_after_eof_exit",
        "input": {"action": "poll", "session_id": pipe_sess.id},
        "observed": normalize_value(poll_done, tracker, temp_dir),
    })

    # 4. write_stdin after exit
    write_post_exit = reg.write_stdin(pipe_sess.id, "after exit")
    cases.append({
        "id": "pipe_stdin_write_after_exit",
        "input": {"action": "write", "data": "after exit", "session_id": pipe_sess.id},
        "observed": write_post_exit,
    })

    # 5. Stdin operation on missing session
    missing_stdin = reg.write_stdin("proc_nonexistent", "data")
    cases.append({
        "id": "stdin_missing_session_id",
        "input": {"action": "write", "session_id": "proc_nonexistent"},
        "observed": missing_stdin,
    })

    return cases


# ---------------------------------------------------------------------------
# Section 9: Handle Process Envelopes
# ---------------------------------------------------------------------------
def build_handle_process_envelope_cases(
    tracker: ProcessTracker, temp_dir: str
) -> List[Dict[str, Any]]:
    cases = []
    reg = pr.ProcessRegistry()

    # Pre-populate sessions for dispatch with unique ID prefixes
    s1 = pr.ProcessSession(
        id="proc_disp11112222",
        command="echo dispatch_test",
        output_buffer="dispatch output line\n",
        exited=True,
        exit_code=0,
        task_id="task_disp",
    )
    s2 = pr.ProcessSession(
        id="proc_disp22223333",
        command="echo dispatch_test2",
        task_id="task_disp",
    )
    reg._finished[s1.id] = s1
    reg._running[s2.id] = s2

    orig_reg = pr.process_registry
    try:
        pr.process_registry = reg

        # Unknown action
        unknown_act = json.loads(pr._handle_process({"action": "bad_action"}))
        cases.append({
            "id": "handle_process_unknown_action",
            "input": {"action": "bad_action"},
            "observed": unknown_act,
        })

        # Empty action
        empty_act = json.loads(pr._handle_process({}))
        cases.append({
            "id": "handle_process_empty_action",
            "input": {},
            "observed": empty_act,
        })

        # Missing session_id for all required actions
        for act in ["poll", "log", "wait", "kill", "write", "submit", "close"]:
            missing_sid = json.loads(pr._handle_process({"action": act}))
            cases.append({
                "id": f"handle_process_missing_session_id_{act}",
                "input": {"action": act},
                "observed": missing_sid,
            })

        # Integer session_id coercion
        int_sid = json.loads(pr._handle_process({"action": "poll", "session_id": 9999}))
        cases.append({
            "id": "handle_process_integer_session_id_coercion",
            "input": {"action": "poll", "session_id": 9999},
            "observed": int_sid,
        })

        # Ambiguous session_id resolution (query "disp" matches both disp1111 and disp2222)
        ambig_poll = json.loads(
            pr._handle_process({"action": "poll", "session_id": "disp"})
        )
        cases.append({
            "id": "handle_process_ambiguous_prefix_resolution",
            "input": {"action": "poll", "session_id": "disp"},
            "observed": ambig_poll,
        })

        # Successful unique prefix dispatch for poll, log, list (query "disp1" matches s1)
        succ_poll = json.loads(
            pr._handle_process({"action": "poll", "session_id": "disp1"})
        )
        cases.append({
            "id": "handle_process_successful_poll_dispatch",
            "input": {"action": "poll", "session_id": "disp1"},
            "observed": normalize_value(succ_poll, tracker, temp_dir),
        })

        succ_log = json.loads(
            pr._handle_process({
                "action": "log",
                "session_id": "disp1",
                "offset": 0,
                "limit": 10,
            })
        )
        cases.append({
            "id": "handle_process_successful_log_dispatch",
            "input": {"action": "log", "limit": 10, "offset": 0, "session_id": "disp1"},
            "observed": normalize_value(succ_log, tracker, temp_dir),
        })

        succ_list = json.loads(
            pr._handle_process({"action": "list"}, task_id="task_disp")
        )
        cases.append({
            "id": "handle_process_successful_list_dispatch",
            "input": {"action": "list", "task_id": "task_disp"},
            "observed": normalize_value(succ_list, tracker, temp_dir),
        })
    finally:
        pr.process_registry = orig_reg

    return cases


# ---------------------------------------------------------------------------
# Section 10: Secret Redaction
# ---------------------------------------------------------------------------
def build_secret_redaction_cases(
    tracker: ProcessTracker, temp_dir: str
) -> List[Dict[str, Any]]:
    cases = []
    reg = pr.ProcessRegistry()

    # Session 1: Env dump command (printenv) with opaque secret token
    s_env = pr.ProcessSession(
        id="proc_sec_env01",
        command="printenv",
        output_buffer="SERVICE_TOKEN=abc123randomopaquetokenvalue999\nHOME=/home/user\n",
        exited=True,
        exit_code=0,
    )
    # Session 2: Command containing inline Bearer token
    s_cmd = pr.ProcessSession(
        id="proc_sec_cmd02",
        command="curl -H 'Authorization: Bearer sk-proj-1234567890abcdef1234567890abcdef'",
        output_buffer="connected\n",
        exited=True,
        exit_code=0,
    )
    # Session 3: Output containing well-known provider key format (Anthropic API key)
    s_out = pr.ProcessSession(
        id="proc_sec_out03",
        command="python test.py",
        output_buffer="Leaked key: sk-ant-api03-abcdef1234567890abcdef1234567890abcdef-ZZZZZZ inside output\n",
        exited=True,
        exit_code=0,
    )

    reg._finished[s_env.id] = s_env
    reg._finished[s_cmd.id] = s_cmd
    reg._finished[s_out.id] = s_out

    orig_reg = pr.process_registry
    try:
        pr.process_registry = reg

        # 1. Env dump log redaction
        env_log = json.loads(
            pr._handle_process({"action": "log", "session_id": s_env.id})
        )
        cases.append({
            "id": "secret_redaction_env_dump_opaque_token",
            "input": {"action": "log", "session_id": s_env.id},
            "observed": normalize_value(env_log, tracker, temp_dir),
        })

        # 2. Command inline Bearer token redaction
        cmd_poll = json.loads(
            pr._handle_process({"action": "poll", "session_id": s_cmd.id})
        )
        cases.append({
            "id": "secret_redaction_command_inline_bearer_token",
            "input": {"action": "poll", "session_id": s_cmd.id},
            "observed": normalize_value(cmd_poll, tracker, temp_dir),
        })

        # 3. Provider secret key in output preview redaction
        out_poll = json.loads(
            pr._handle_process({"action": "poll", "session_id": s_out.id})
        )
        cases.append({
            "id": "secret_redaction_output_provider_key",
            "input": {"action": "poll", "session_id": s_out.id},
            "observed": normalize_value(out_poll, tracker, temp_dir),
        })

        # 4. List action redacts both command and output_preview across sessions
        list_redacted = json.loads(pr._handle_process({"action": "list"}))
        cases.append({
            "id": "secret_redaction_list_command_and_output",
            "input": {"action": "list"},
            "observed": normalize_value(list_redacted, tracker, temp_dir),
        })
    finally:
        pr.process_registry = orig_reg

    return cases


# ---------------------------------------------------------------------------
# Corpus Builder
# ---------------------------------------------------------------------------
def build_corpus() -> Dict[str, Any]:
    with isolated_contract_context() as (tracker, temp_dir):
        return {
            "_meta": build_meta(),
            "bounded_wait": build_bounded_wait_cases(tracker, temp_dir),
            "exit_semantics": build_exit_semantics_cases(tracker, temp_dir),
            "handle_process_envelopes": build_handle_process_envelope_cases(
                tracker, temp_dir
            ),
            "ownership_filtering": build_ownership_filtering_cases(tracker, temp_dir),
            "poll_and_log": build_poll_and_log_cases(tracker, temp_dir),
            "prefix_lookup": build_prefix_lookup_cases(tracker, temp_dir),
            "secret_redaction": build_secret_redaction_cases(tracker, temp_dir),
            "spawn_and_status": build_spawn_and_status_cases(tracker, temp_dir),
            "stdin_lifecycle": build_stdin_lifecycle_cases(tracker, temp_dir),
        }


# ---------------------------------------------------------------------------
# CLI Entrypoint
# ---------------------------------------------------------------------------
def main() -> int:
    parser = argparse.ArgumentParser(
        description="Golden oracle for native local non-PTY background process contract."
    )
    parser.add_argument(
        "--check",
        "--verify",
        dest="check_only",
        action="store_true",
        help="Verify checked-in goldens match fresh generator output byte-for-byte.",
    )
    parser.add_argument(
        "--generate",
        "--write",
        dest="generate_only",
        action="store_true",
        help="Regenerate and write goldens file.",
    )
    args = parser.parse_args()

    corpus = build_corpus()
    formatted_json = (
        json.dumps(corpus, indent=2, ensure_ascii=True, sort_keys=True) + "\n"
    )

    if args.check_only:
        if not OUT.exists():
            print(f"FAIL: {OUT} does not exist; run generator first.", file=sys.stderr)
            return 1
        existing_content = OUT.read_text(encoding="utf-8")
        if existing_content != formatted_json:
            print(
                f"FAIL: {OUT} differs from freshly generated oracle corpus.",
                file=sys.stderr,
            )
            # Show diff snippet for fast diagnosis
            import difflib

            diff = difflib.unified_diff(
                existing_content.splitlines()[:50],
                formatted_json.splitlines()[:50],
                fromfile="existing",
                tofile="generated",
            )
            print("\n".join(diff), file=sys.stderr)
            return 1
        print("OK: goldens match freshly generated corpus")
        return 0

    OUT.write_text(formatted_json, encoding="utf-8")
    total_cases = sum(
        len(v)
        for k, v in corpus.items()
        if not k.startswith("_") and isinstance(v, list)
    )
    print(f"wrote {OUT} ({total_cases} cases across {len(corpus) - 1} sections)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
