#!/usr/bin/env python3
"""Generate golden test fixtures for environment prompt rendering via Python AST execution."""
import ast
import json
import os
import platform
import sys
from pathlib import Path
from types import ModuleType, SimpleNamespace

root = Path(__file__).resolve().parents[2]
tree = ast.parse((root / "agent/prompt_builder.py").read_text())

needed = [
    "WSL_ENVIRONMENT_HINT",
    "_REMOTE_TERMINAL_BACKENDS",
    "_plugin_backend_is_remote",
    "_plugin_backend_description",
    "_BACKEND_FALLBACK_DESCRIPTIONS",
    "_windows_marketing_version",
    "_WINDOWS_BASH_SHELL_HINT",
    "_tenv_read",
    "_probe_remote_backend",
    "build_environment_hints",
]

nodes = [
    node for node in tree.body
    if (isinstance(node, (ast.Assign, ast.AnnAssign))
        and any(isinstance(t, ast.Name) and t.id in needed
                for t in getattr(node, "targets", [getattr(node, "target", None)])))
    or (isinstance(node, ast.FunctionDef) and node.name in needed)
]

scope = {
    "sys": sys,
    "os": os,
    "platform": platform,
    "logger": SimpleNamespace(debug=lambda *args, **kwargs: None),
}

mod = ast.Module(body=nodes, type_ignores=[])
ast.fix_missing_locations(mod)
exec(compile(mod, "agent/prompt_builder.py", "exec"), scope)

fn = scope["build_environment_hints"]


def run_oracle(
    backend="local",
    is_remote=None,
    probe_output=None,
    fallback_desc=None,
    platform_name="linux",
    windows_build=None,
    macos_ver=None,
    system_name="Linux",
    release_name="6.6.0",
    user_home="/home/user",
    cwd="/home/user/work",
    is_wsl_val=False,
    extra_env=None,
    extra_config=None,
):
    orig_platform_sys = sys.platform
    orig_expanduser = os.path.expanduser
    orig_getenv = os.getenv
    orig_mac_ver = platform.mac_ver
    orig_system = platform.system
    orig_release = platform.release

    scope["is_wsl"] = lambda: is_wsl_val
    if cwd is not None:
        scope["resolve_agent_cwd"] = lambda: cwd
    else:
        def fail_cwd():
            raise OSError("no cwd")
        scope["resolve_agent_cwd"] = fail_cwd

    scope["_tenv_read"] = lambda name, default="": backend if name == "TERMINAL_ENV" else default
    if is_remote is not None:
        scope["_plugin_backend_is_remote"] = lambda b: is_remote
    else:
        scope["_plugin_backend_is_remote"] = lambda b: False
    if fallback_desc is not None:
        scope["_plugin_backend_description"] = lambda b: fallback_desc
    else:
        scope["_plugin_backend_description"] = lambda b: None

    scope["_probe_remote_backend"] = lambda b: probe_output

    config_mod = ModuleType("hermes_cli.config")
    if extra_config is not None:
        config_mod.load_config_readonly = lambda: {"agent": {"environment_hint": extra_config}}
    else:
        config_mod.load_config_readonly = lambda: {}
    sys.modules["hermes_cli.config"] = config_mod

    sys.platform = platform_name
    if windows_build is not None:
        sys.getwindowsversion = lambda: SimpleNamespace(build=windows_build)
    elif hasattr(sys, "getwindowsversion"):
        delattr(sys, "getwindowsversion")

    os.path.expanduser = lambda p: user_home
    os.getenv = lambda k, default="": extra_env if (k == "HERMES_ENVIRONMENT_HINT" and extra_env is not None) else default
    if macos_ver is not None:
        platform.mac_ver = lambda: (macos_ver, ("", "", ""), "")
    platform.system = lambda: system_name
    platform.release = lambda: release_name

    try:
        return fn()
    finally:
        sys.platform = orig_platform_sys
        os.path.expanduser = orig_expanduser
        os.getenv = orig_getenv
        platform.mac_ver = orig_mac_ver
        platform.system = orig_system
        platform.release = orig_release
        if hasattr(sys, "getwindowsversion"):
            delattr(sys, "getwindowsversion")


# Define test specifications
test_specs = [
    {
        "id": "local_linux_standard",
        "oracle_args": {
            "backend": "local",
            "platform_name": "linux",
            "system_name": "Linux",
            "release_name": "6.8.0-generic",
            "user_home": "/home/alice",
            "cwd": "/home/alice/project",
        },
        "inputs": {
            "backend": "local",
            "local_host": {
                "platform": {"type": "other", "system": "Linux", "release": "6.8.0-generic"},
                "user_home": "/home/alice",
                "cwd": "/home/alice/project",
            },
        },
    },
    {
        "id": "local_linux_no_cwd",
        "oracle_args": {
            "backend": "local",
            "platform_name": "linux",
            "system_name": "Linux",
            "release_name": "6.8.0-generic",
            "user_home": "/home/bob",
            "cwd": None,
        },
        "inputs": {
            "backend": "local",
            "local_host": {
                "platform": {"type": "other", "system": "Linux", "release": "6.8.0-generic"},
                "user_home": "/home/bob",
                "cwd": None,
            },
        },
    },
    {
        "id": "local_macos_standard",
        "oracle_args": {
            "backend": "local",
            "platform_name": "darwin",
            "macos_ver": "14.4.1",
            "user_home": "/Users/alice",
            "cwd": "/Users/alice/repo",
        },
        "inputs": {
            "backend": "local",
            "local_host": {
                "platform": {"type": "mac_os", "version": "14.4.1"},
                "user_home": "/Users/alice",
                "cwd": "/Users/alice/repo",
            },
        },
    },
    {
        "id": "local_macos_release_fallback",
        "oracle_args": {
            "backend": "local",
            "platform_name": "darwin",
            "macos_ver": "",
            "release_name": "23.4.0",
            "user_home": "/Users/alice",
            "cwd": "/Users/alice/repo",
        },
        "inputs": {
            "backend": "local",
            "local_host": {
                "platform": {"type": "mac_os", "version": "23.4.0"},
                "user_home": "/Users/alice",
                "cwd": "/Users/alice/repo",
            },
        },
    },
    {
        "id": "local_windows_11",
        "oracle_args": {
            "backend": "local",
            "platform_name": "win32",
            "windows_build": 22631,
            "user_home": r"C:\Users\alice",
            "cwd": r"C:\Users\alice\project",
        },
        "inputs": {
            "backend": "local",
            "local_host": {
                "platform": {"type": "windows", "marketing_version": "11"},
                "user_home": r"C:\Users\alice",
                "cwd": r"C:\Users\alice\project",
            },
        },
    },
    {
        "id": "local_windows_10",
        "oracle_args": {
            "backend": "local",
            "platform_name": "win32",
            "windows_build": 19045,
            "user_home": r"C:\Users\alice",
            "cwd": r"C:\Users\alice\project",
        },
        "inputs": {
            "backend": "local",
            "local_host": {
                "platform": {"type": "windows", "marketing_version": "10"},
                "user_home": r"C:\Users\alice",
                "cwd": r"C:\Users\alice\project",
            },
        },
    },
    {
        "id": "local_windows_no_cwd",
        "oracle_args": {
            "backend": "local",
            "platform_name": "win32",
            "windows_build": 22631,
            "user_home": r"C:\Users\alice",
            "cwd": None,
        },
        "inputs": {
            "backend": "local",
            "local_host": {
                "platform": {"type": "windows", "marketing_version": "11"},
                "user_home": r"C:\Users\alice",
                "cwd": None,
            },
        },
    },
    {
        "id": "local_wsl",
        "oracle_args": {
            "backend": "local",
            "is_wsl_val": True,
            "user_home": "/home/alice",
            "cwd": "/home/alice/project",
        },
        "inputs": {
            "backend": "local",
            "is_wsl": True,
            "local_host": {
                "platform": {"type": "wsl"},
                "user_home": "/home/alice",
                "cwd": "/home/alice/project",
            },
        },
    },
    {
        "id": "local_other_freebsd",
        "oracle_args": {
            "backend": "local",
            "platform_name": "freebsd",
            "system_name": "FreeBSD",
            "release_name": "14.0-RELEASE",
            "user_home": "/usr/home/alice",
            "cwd": "/usr/home/alice/code",
        },
        "inputs": {
            "backend": "local",
            "local_host": {
                "platform": {"type": "other", "system": "FreeBSD", "release": "14.0-RELEASE"},
                "user_home": "/usr/home/alice",
                "cwd": "/usr/home/alice/code",
            },
        },
    },
    {
        "id": "remote_docker_probe_success",
        "oracle_args": {
            "backend": "docker",
            "probe_output": "  OS: Linux 6.6.0\n  User: root\n  Home: /root\n  Working directory: /workspace",
        },
        "inputs": {
            "backend": "docker",
            "remote_probe": {
                "type": "formatted",
                "data": "  OS: Linux 6.6.0\n  User: root\n  Home: /root\n  Working directory: /workspace",
            },
        },
    },
    {
        "id": "remote_docker_probe_fallback",
        "oracle_args": {
            "backend": "docker",
            "probe_output": None,
        },
        "inputs": {
            "backend": "docker",
            "remote_probe": {"type": "failed"},
        },
    },
    {
        "id": "remote_singularity_fallback",
        "oracle_args": {
            "backend": "singularity",
            "probe_output": None,
        },
        "inputs": {
            "backend": "singularity",
            "remote_probe": {"type": "failed"},
        },
    },
    {
        "id": "remote_modal_fallback",
        "oracle_args": {
            "backend": "modal",
            "probe_output": None,
        },
        "inputs": {
            "backend": "modal",
            "remote_probe": {"type": "failed"},
        },
    },
    {
        "id": "remote_managed_modal_fallback",
        "oracle_args": {
            "backend": "managed_modal",
            "probe_output": None,
        },
        "inputs": {
            "backend": "managed_modal",
            "remote_probe": {"type": "failed"},
        },
    },
    {
        "id": "remote_daytona_fallback",
        "oracle_args": {
            "backend": "daytona",
            "probe_output": None,
        },
        "inputs": {
            "backend": "daytona",
            "remote_probe": {"type": "failed"},
        },
    },
    {
        "id": "remote_vercel_sandbox_fallback",
        "oracle_args": {
            "backend": "vercel_sandbox",
            "probe_output": None,
        },
        "inputs": {
            "backend": "vercel_sandbox",
            "remote_probe": {"type": "failed"},
        },
    },
    {
        "id": "remote_ssh_fallback",
        "oracle_args": {
            "backend": "ssh",
            "probe_output": None,
        },
        "inputs": {
            "backend": "ssh",
            "remote_probe": {"type": "failed"},
        },
    },
    {
        "id": "remote_ssh_probe_success",
        "oracle_args": {
            "backend": "ssh",
            "probe_output": "  OS: Ubuntu 24.04\n  User: dev\n  Home: /home/dev\n  Working directory: /srv/app",
        },
        "inputs": {
            "backend": "ssh",
            "remote_probe": {
                "type": "formatted",
                "data": "  OS: Ubuntu 24.04\n  User: dev\n  Home: /home/dev\n  Working directory: /srv/app",
            },
        },
    },
    {
        "id": "remote_custom_plugin_fallback",
        "oracle_args": {
            "backend": "k8s_pod",
            "is_remote": True,
            "fallback_desc": "a Kubernetes pod (Linux)",
            "probe_output": None,
        },
        "inputs": {
            "backend": "k8s_pod",
            "is_remote": True,
            "fallback_description": "a Kubernetes pod (Linux)",
            "remote_probe": {"type": "failed"},
        },
    },
    {
        "id": "remote_unknown_fallback_default",
        "oracle_args": {
            "backend": "custom_box",
            "is_remote": True,
            "probe_output": None,
        },
        "inputs": {
            "backend": "custom_box",
            "is_remote": True,
            "remote_probe": {"type": "failed"},
        },
    },
    {
        "id": "remote_docker_on_wsl_host",
        "oracle_args": {
            "backend": "docker",
            "is_wsl_val": True,
            "probe_output": "  OS: Linux 6.6.0\n  User: root\n  Home: /root\n  Working directory: /workspace",
        },
        "inputs": {
            "backend": "docker",
            "is_wsl": True,
            "remote_probe": {
                "type": "formatted",
                "data": "  OS: Linux 6.6.0\n  User: root\n  Home: /root\n  Working directory: /workspace",
            },
        },
    },
    {
        "id": "remote_docker_fallback_on_wsl_host",
        "oracle_args": {
            "backend": "docker",
            "is_wsl_val": True,
            "probe_output": None,
        },
        "inputs": {
            "backend": "docker",
            "is_wsl": True,
            "remote_probe": {"type": "failed"},
        },
    },
    {
        "id": "local_linux_with_extra_env",
        "oracle_args": {
            "backend": "local",
            "platform_name": "linux",
            "system_name": "Linux",
            "release_name": "6.8.0-generic",
            "user_home": "/home/alice",
            "cwd": "/home/alice/project",
            "extra_env": "Corporate proxy configured at 10.0.0.1:8080",
        },
        "inputs": {
            "backend": "local",
            "local_host": {
                "platform": {"type": "other", "system": "Linux", "release": "6.8.0-generic"},
                "user_home": "/home/alice",
                "cwd": "/home/alice/project",
            },
            "extra_hint": "Corporate proxy configured at 10.0.0.1:8080",
        },
    },
    {
        "id": "local_linux_with_extra_config",
        "oracle_args": {
            "backend": "local",
            "platform_name": "linux",
            "system_name": "Linux",
            "release_name": "6.8.0-generic",
            "user_home": "/home/alice",
            "cwd": "/home/alice/project",
            "extra_config": "Sandbox memory limit 8GB",
        },
        "inputs": {
            "backend": "local",
            "local_host": {
                "platform": {"type": "other", "system": "Linux", "release": "6.8.0-generic"},
                "user_home": "/home/alice",
                "cwd": "/home/alice/project",
            },
            "extra_hint": "Sandbox memory limit 8GB",
        },
    },
    {
        "id": "local_linux_extra_whitespace_trimmed",
        "oracle_args": {
            "backend": "local",
            "platform_name": "linux",
            "system_name": "Linux",
            "release_name": "6.8.0-generic",
            "user_home": "/home/alice",
            "cwd": "/home/alice/project",
            "extra_env": "  Padded extra hint with trailing newline\n",
        },
        "inputs": {
            "backend": "local",
            "local_host": {
                "platform": {"type": "other", "system": "Linux", "release": "6.8.0-generic"},
                "user_home": "/home/alice",
                "cwd": "/home/alice/project",
            },
            "extra_hint": "  Padded extra hint with trailing newline\n",
        },
    },
    {
        "id": "remote_docker_with_extra_hint",
        "oracle_args": {
            "backend": "docker",
            "probe_output": "  OS: Linux 6.6.0\n  User: root\n  Home: /root\n  Working directory: /workspace",
            "extra_env": "Container runs with read-only rootfs.",
        },
        "inputs": {
            "backend": "docker",
            "remote_probe": {
                "type": "formatted",
                "data": "  OS: Linux 6.6.0\n  User: root\n  Home: /root\n  Working directory: /workspace",
            },
            "extra_hint": "Container runs with read-only rootfs.",
        },
    },
    {
        "id": "backend_case_and_whitespace",
        "oracle_args": {
            "backend": "  DOCKER  ",
            "probe_output": "  OS: Linux\n  User: root\n  Home: /root\n  Working directory: /workspace",
        },
        "inputs": {
            "backend": "  DOCKER  ",
            "remote_probe": {
                "type": "formatted",
                "data": "  OS: Linux\n  User: root\n  Home: /root\n  Working directory: /workspace",
            },
        },
    },
    {
        "id": "backend_empty_defaults_to_local",
        "oracle_args": {
            "backend": "",
            "platform_name": "linux",
            "system_name": "Linux",
            "release_name": "6.8.0-generic",
            "user_home": "/home/user",
            "cwd": "/home/user",
        },
        "inputs": {
            "backend": "",
            "local_host": {
                "platform": {"type": "other", "system": "Linux", "release": "6.8.0-generic"},
                "user_home": "/home/user",
                "cwd": "/home/user",
            },
        },
    },
    {
        "id": "backend_whitespace_only_defaults_to_local",
        "oracle_args": {
            "backend": "   ",
            "platform_name": "linux",
            "system_name": "Linux",
            "release_name": "6.8.0-generic",
            "user_home": "/home/user",
            "cwd": "/home/user",
        },
        "inputs": {
            "backend": "   ",
            "local_host": {
                "platform": {"type": "other", "system": "Linux", "release": "6.8.0-generic"},
                "user_home": "/home/user",
                "cwd": "/home/user",
            },
        },
    },
    {
        "id": "probe_partial_user_home_only",
        "oracle_args": {
            "backend": "docker",
            "probe_output": "  User: deploy\n  Home: /home/deploy",
        },
        "inputs": {
            "backend": "docker",
            "remote_probe": {
                "type": "formatted",
                "data": "  User: deploy\n  Home: /home/deploy",
            },
        },
    },
    {
        "id": "probe_partial_os_no_kernel",
        "oracle_args": {
            "backend": "docker",
            "probe_output": "  OS: Linux\n  User: root\n  Home: /root\n  Working directory: /app",
        },
        "inputs": {
            "backend": "docker",
            "remote_probe": {
                "type": "formatted",
                "data": "  OS: Linux\n  User: root\n  Home: /root\n  Working directory: /app",
            },
        },
    },
    {
        "id": "local_windows_11_with_extra_hint",
        "oracle_args": {
            "backend": "local",
            "platform_name": "win32",
            "windows_build": 22631,
            "user_home": r"C:\Users\alice",
            "cwd": r"C:\Users\alice\project",
            "extra_env": "Windows developer mode enabled.",
        },
        "inputs": {
            "backend": "local",
            "local_host": {
                "platform": {"type": "windows", "marketing_version": "11"},
                "user_home": r"C:\Users\alice",
                "cwd": r"C:\Users\alice\project",
            },
            "extra_hint": "Windows developer mode enabled.",
        },
    },
]

# Built-ins retain their remote classification and description even when a
# plugin registry supplies conflicting answers. Preserve unusual whitespace.
for backend, flag, description, probe in [
    ("docker", False, "plugin description", None),
    ("\u001cDOCKER\u001c", False, "plugin description", None),
    ("custom", True, "", None),
    ("ssh", False, None, " \n"),
]:
    test_specs.append({
        "id": f"precedence_{len(test_specs)}",
        "oracle_args": dict(backend=backend, is_remote=flag, fallback_desc=description, probe_output=probe),
        "inputs": dict(backend=backend, is_remote=flag, fallback_description=description,
                       remote_probe={"type": "formatted", "data": probe} if probe is not None else {"type": "failed"}),
    })

cases = []
for spec in test_specs:
    expected = run_oracle(**spec["oracle_args"])
    cases.append({
        "id": spec["id"],
        "inputs": spec["inputs"],
        "expected": expected,
    })

goldens_path = root / "rust/tools/environment-prompt-goldens.json"
text = json.dumps(cases, indent=2) + "\n"

if sys.argv[1:] == ["--check"]:
    assert goldens_path.read_text() == text, "Goldens mismatch under --check"
    print(f"Verified {len(cases)} environment prompt cases matches {goldens_path}")
elif not sys.argv[1:]:
    goldens_path.write_text(text)
    print(f"Generated {len(cases)} environment prompt cases to {goldens_path}")
else:
    raise SystemExit("usage: gen_environment_prompt_goldens.py [--check]")
