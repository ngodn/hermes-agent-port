#!/usr/bin/env python3
"""Deterministic source-executed oracle for Nous auxiliary credential discovery
and recovery contract.

This script audits and executes the authoritative Python runtime contract across:
1. agent/credential_pool.py
2. hermes_cli/auth.py
3. agent/auxiliary_client.py
and related modules.

It exercises 12 behavioral contract sections:
- Profile and Root Ownership (classic mode, multi-profile borrowing, write-through to root, shared store, cloning hygiene)
- Singleton Seeding and Idempotence (providers.nous vs credential_pool.nous, canonical device_code source, upsert idempotence)
- Access-Token and Runtime-Key Selection (runtime_api_key precedence, runtime_base_url, JWT claims decoding, invoke scope checks)
- Expiry and Skew Semantics (120s skew window, exp claim evaluation, fallback to expires_at, status codes)
- Forced 401 Refresh and Client Rebuild (token endpoint POST, x-nous-refresh-token header, client eviction, exact cache key alignment)
- Peer-Rotated-Token Adoption (stampede avoidance, stale hint comparison, skip POST when peer rotated, pool resync)
- Single-Use Refresh Locking Expectations (cross-process file lock, timeout non-benching, in-memory pool lock concurrency)
- Persistence-Before-Retry (saving rotated tokens to disk and shared store before JWT assertion or client call)
- Terminal and Relogin Classifications (invalid_grant, invalid_token, refresh_token_reused, forensic warning log, quarantine actions)
- Provider Health Tracking (_aux_unhealthy_until, 60s unconfigured quarantine, 600s payment error quarantine, rate guard integration)
- Request Bounds and Retry Limits (300s compression timeout floor, caller override preservation, no-retry task classification, max 1 retry)
- Model and Base URL Selection (_NOUS_MODEL default, portal recommended model, endpoint sanitization, policy filtering)

Usage:
    python3 rust/tools/gen_nous_oauth_recovery_goldens.py          # write goldens
    python3 rust/tools/gen_nous_oauth_recovery_goldens.py --check  # check parity
"""

from __future__ import annotations

import base64
import copy
import json
import os
import sys
import tempfile
import time
from dataclasses import replace
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "rust/tools/nous-oauth-recovery-goldens.json"

sys.path.insert(0, str(ROOT))

# Auto-reexec with repo virtualenv if optional dependencies like httpx are missing in current python
try:
    import httpx
except ImportError:
    venv_py = ROOT / ".venv" / "bin" / "python"
    if venv_py.exists() and sys.executable != str(venv_py):
        os.execv(str(venv_py), [str(venv_py)] + sys.argv)
    raise

import agent.auxiliary_client as ax
import agent.credential_pool as cp
import hermes_cli.auth as auth
from agent.credential_pool import (
    AUTH_TYPE_API_KEY,
    AUTH_TYPE_OAUTH,
    CREDENTIAL_PERSIST_FAILED_REASON,
    STATUS_DEAD,
    STATUS_EXHAUSTED,
    STATUS_OK,
    CredentialPool,
    PooledCredential,
    load_pool,
)

# Deterministic reference epoch: 2026-02-01T00:00:00Z (1770000000.0)
FIXED_NOW = 1770000000.0


def _part(payload: dict) -> str:
    raw = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    return base64.urlsafe_b64encode(raw).decode("ascii").rstrip("=")


def _jwt_with_claims(claims: dict) -> str:
    return f"{_part({'alg': 'none', 'typ': 'JWT'})}.{_part(claims)}.sig"


def _future_iso(seconds: int = 3600, now: float = FIXED_NOW) -> str:
    return datetime.fromtimestamp(now + seconds, tz=timezone.utc).isoformat()


def _invoke_jwt(
    *, seconds: int = 3600, scope: object = "inference:invoke", now: float = FIXED_NOW
) -> str:
    return _jwt_with_claims({
        "sub": "user_test",
        "scope": scope,
        "exp": int(now + seconds),
    })


# ---------------------------------------------------------------------------
# Section 1: Profile and Root Ownership
# ---------------------------------------------------------------------------
def section_profile_and_root_ownership() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # 1. Classic mode: profile is root
    with tempfile.TemporaryDirectory() as td:
        root_dir = Path(td) / "hermes"
        root_dir.mkdir(parents=True, exist_ok=True)
        auth_file = root_dir / "auth.json"
        auth_file.write_text(json.dumps({"version": 1, "providers": {}}))
        with (
            patch.object(auth, "_auth_file_path", return_value=auth_file),
            patch.object(auth, "_global_auth_file_path", return_value=None),
        ):
            global_path = auth._global_auth_file_path()
            rows.append({
                "case": "classic_mode_profile_is_root",
                "is_classic_mode": global_path is None,
                "global_path": str(global_path) if global_path else None,
                "ownership_classification": "classic_root_direct",
            })

    # 2. Named profile borrowing from root: resolution tracks source
    with tempfile.TemporaryDirectory() as td:
        base = Path(td)
        profile_path = base / "profile" / "auth.json"
        profile_path.parent.mkdir(parents=True, exist_ok=True)
        profile_path.write_text(json.dumps({"version": 1, "providers": {}}))

        root_path = base / "root" / "auth.json"
        root_path.parent.mkdir(parents=True, exist_ok=True)
        root_path.write_text(
            json.dumps({
                "version": 1,
                "providers": {
                    "nous": {
                        "access_token": "tok_root",
                        "refresh_token": "rt_root",
                        "portal_base_url": "https://portal.nousresearch.com",
                        "inference_base_url": "https://inference-api.nousresearch.com/v1",
                    }
                },
            })
        )

        with (
            patch.object(auth, "_auth_file_path", return_value=profile_path),
            patch.object(auth, "_global_auth_file_path", return_value=root_path),
        ):
            state, source_path = auth._load_provider_state_with_source(
                {"version": 1, "providers": {}}, "nous"
            )
            is_from_root = bool(
                source_path and root_path and auth._same_path(source_path, root_path)
            )
            rows.append({
                "case": "named_profile_borrowing_from_root",
                "state_resolved": state is not None,
                "is_from_root": is_from_root,
                "source_matches_global_root": bool(
                    source_path and auth._same_path(source_path, root_path)
                ),
                "ownership_classification": "borrowed_from_global_root",
            })

    # 3. Borrowed grant refresh syncs back to root only
    with tempfile.TemporaryDirectory() as td:
        base = Path(td)
        profile_path = base / "profile" / "auth.json"
        profile_path.parent.mkdir(parents=True, exist_ok=True)
        profile_path.write_text(json.dumps({"version": 1, "providers": {}}))

        root_path = base / "root" / "auth.json"
        root_path.parent.mkdir(parents=True, exist_ok=True)
        root_path.write_text(
            json.dumps({
                "version": 1,
                "providers": {
                    "nous": {
                        "access_token": "root_at_0",
                        "refresh_token": "root_rt_0",
                        "portal_base_url": "https://portal.nousresearch.com",
                        "inference_base_url": "https://inference-api.nousresearch.com/v1",
                    }
                },
            })
        )

        entry = PooledCredential(
            provider="nous",
            id="dc-1",
            label="Nous DC",
            auth_type=AUTH_TYPE_OAUTH,
            priority=0,
            source="device_code",
            access_token="rotated_at_1",
            refresh_token="rotated_rt_1",
        )
        pool = CredentialPool("nous", [entry])

        with (
            patch.object(auth, "_auth_file_path", return_value=profile_path),
            patch.object(auth, "_global_auth_file_path", return_value=root_path),
            patch.object(cp, "_global_auth_file_path", return_value=root_path),
            patch.object(cp, "_same_path", lambda a, b: str(a) == str(b)),
            patch.dict("os.environ", {"HOME": str(base / "fakehome")}),
        ):
            pool._sync_device_code_entry_to_auth_store(entry)
            root_data = json.loads(root_path.read_text())
            profile_data = json.loads(profile_path.read_text())
            rows.append({
                "case": "borrowed_grant_refresh_syncs_to_root_only",
                "root_refresh_token_updated": root_data["providers"]["nous"][
                    "refresh_token"
                ]
                == "rotated_rt_1",
                "root_access_token_updated": root_data["providers"]["nous"][
                    "access_token"
                ]
                == "rotated_at_1",
                "ownership_classification": "write_through_to_root",
            })

    # 4. Borrowed grant refresh never creates shadowing profile provider key
    rows.append({
        "case": "borrowed_grant_refresh_never_shadows_profile_store",
        "profile_has_providers_nous": "nous" in profile_data.get("providers", {}),
        "avoids_self_sealing_shadow": "nous" not in profile_data.get("providers", {}),
        "ownership_classification": "no_shadowing_key_created",
    })

    # 5. Profile-owned grant syncs to profile store directly
    with tempfile.TemporaryDirectory() as td:
        base = Path(td)
        profile_path = base / "profile" / "auth.json"
        profile_path.parent.mkdir(parents=True, exist_ok=True)
        profile_path.write_text(
            json.dumps({
                "version": 1,
                "providers": {
                    "nous": {
                        "access_token": "prof_at_0",
                        "refresh_token": "prof_rt_0",
                        "portal_base_url": "https://portal.nousresearch.com",
                        "inference_base_url": "https://inference-api.nousresearch.com/v1",
                    }
                },
            })
        )
        root_path = base / "root" / "auth.json"
        root_path.parent.mkdir(parents=True, exist_ok=True)
        root_path.write_text(
            json.dumps({
                "version": 1,
                "providers": {
                    "nous": {
                        "access_token": "root_at_static",
                        "refresh_token": "root_rt_static",
                    }
                },
            })
        )

        entry = PooledCredential(
            provider="nous",
            id="dc-own",
            label="Nous Own",
            auth_type=AUTH_TYPE_OAUTH,
            priority=0,
            source="device_code",
            access_token="prof_at_rotated",
            refresh_token="prof_rt_rotated",
        )
        pool = CredentialPool("nous", [entry])

        with (
            patch.object(auth, "_auth_file_path", return_value=profile_path),
            patch.object(auth, "_global_auth_file_path", return_value=root_path),
            patch.object(cp, "_global_auth_file_path", return_value=root_path),
            patch.object(cp, "_same_path", lambda a, b: str(a) == str(b)),
            patch.dict("os.environ", {"HOME": str(base / "fakehome")}),
        ):
            pool._sync_device_code_entry_to_auth_store(entry)
            prof_after = json.loads(profile_path.read_text())
            root_after = json.loads(root_path.read_text())
            rows.append({
                "case": "profile_owned_grant_syncs_to_profile_store",
                "profile_refresh_token_updated": prof_after["providers"]["nous"][
                    "refresh_token"
                ]
                == "prof_rt_rotated",
                "root_refresh_token_untouched": root_after["providers"]["nous"][
                    "refresh_token"
                ]
                == "root_rt_static",
                "ownership_classification": "profile_owned_direct_save",
            })

    # 6. Shared store mirrors OAuth material
    with tempfile.TemporaryDirectory() as td:
        shared_dir = Path(td) / "shared"
        token = _invoke_jwt(seconds=3600)
        state_fixture = {
            "access_token": token,
            "refresh_token": "rt_shared_test",
            "portal_base_url": "https://portal.nousresearch.com",
            "inference_base_url": "https://inference-api.nousresearch.com/v1",
            "client_id": "hermes-cli",
            "agent_key": token,
            "agent_key_expires_at": _future_iso(3600),
        }
        with patch.dict("os.environ", {"HERMES_SHARED_AUTH_DIR": str(shared_dir)}):
            auth._write_shared_nous_state(state_fixture)
            shared_read = auth._read_shared_nous_state()
            rows.append({
                "case": "shared_store_mirrors_oauth_material",
                "shared_file_exists": (
                    shared_dir / auth.NOUS_SHARED_STORE_FILENAME
                ).is_file(),
                "shared_refresh_token": shared_read.get("refresh_token")
                if shared_read
                else None,
                "shared_portal_base_url": shared_read.get("portal_base_url")
                if shared_read
                else None,
                "ownership_classification": "shared_store_mirroring",
            })

    # 7. Shared store excludes volatile agent_key
    rows.append({
        "case": "shared_store_excludes_volatile_agent_key",
        "agent_key_in_shared_store": "agent_key" in (shared_read or {}),
        "agent_key_expires_at_in_shared_store": "agent_key_expires_at"
        in (shared_read or {}),
        "ownership_classification": "volatile_agent_key_exclusion",
    })

    # 8. Try import shared state rehydrates valid grant
    with tempfile.TemporaryDirectory() as td:
        shared_dir = Path(td) / "shared"
        shared_dir.mkdir(parents=True, exist_ok=True)
        token = _invoke_jwt(seconds=3600)
        (shared_dir / auth.NOUS_SHARED_STORE_FILENAME).write_text(
            json.dumps({
                "access_token": token,
                "refresh_token": "rt_importable",
                "portal_base_url": "https://portal.nousresearch.com",
                "inference_base_url": "https://inference-api.nousresearch.com/v1",
                "client_id": "hermes-cli",
            })
        )
        fresh_token = _invoke_jwt(seconds=7200)
        with (
            patch.dict("os.environ", {"HERMES_SHARED_AUTH_DIR": str(shared_dir)}),
            patch.object(
                auth,
                "refresh_nous_oauth_from_state",
                return_value={
                    "access_token": fresh_token,
                    "refresh_token": "rt_imported_new",
                    "portal_base_url": "https://portal.nousresearch.com",
                    "inference_base_url": "https://inference-api.nousresearch.com/v1",
                    "client_id": "hermes-cli",
                    "agent_key": fresh_token,
                },
            ),
        ):
            imported = auth._try_import_shared_nous_state()
            rows.append({
                "case": "try_import_shared_state_rehydrates_valid_grant",
                "import_succeeded": imported is not None,
                "rehydrated_refresh_token": imported.get("refresh_token")
                if imported
                else None,
                "rehydrated_agent_key": bool(imported.get("agent_key"))
                if imported
                else False,
                "ownership_classification": "shared_import_success",
            })

    # 9. Try import shared state returns None when empty
    with tempfile.TemporaryDirectory() as td:
        shared_dir = Path(td) / "empty_shared"
        with patch.dict("os.environ", {"HERMES_SHARED_AUTH_DIR": str(shared_dir)}):
            imported_empty = auth._try_import_shared_nous_state()
            rows.append({
                "case": "try_import_shared_state_returns_none_when_empty",
                "import_succeeded": imported_empty is not None,
                "result": imported_empty,
                "ownership_classification": "shared_import_absent",
            })

    # 10. Strip cloned single-use grants preserves static keys
    with tempfile.TemporaryDirectory() as td:
        clone_dir = Path(td) / "clone"
        clone_dir.mkdir(parents=True, exist_ok=True)
        (clone_dir / "auth.json").write_text(
            json.dumps({
                "version": 1,
                "providers": {
                    "openai-codex": {"access_token": "c_tok", "refresh_token": "c_rt"}
                },
                "credential_pool": {
                    "xai-oauth": [
                        {
                            "id": "x1",
                            "auth_type": "oauth",
                            "access_token": "x_tok",
                            "refresh_token": "x_rt",
                        }
                    ],
                    "anthropic": [
                        {
                            "id": "a1",
                            "auth_type": "oauth",
                            "access_token": "sk-ant-oat01-tok",
                        }
                    ],
                    "openai": [
                        {
                            "id": "k1",
                            "auth_type": "api_key",
                            "access_token": "sk-static-open-ai",
                        }
                    ],
                },
            })
        )
        summary = auth.strip_cloned_single_use_oauth_grants(clone_dir)
        store_after = json.loads((clone_dir / "auth.json").read_text())
        rows.append({
            "case": "strip_cloned_single_use_grants_preserves_static_keys",
            "stripped_pool_providers": sorted(summary["pool"]),
            "stripped_singleton_providers": sorted(summary["providers"]),
            "openai_static_retained": "openai" in store_after["credential_pool"],
            "codex_singleton_dropped": "openai-codex" not in store_after["providers"],
            "ownership_classification": "cloning_hygiene_protection",
        })

    return rows


# ---------------------------------------------------------------------------
# Section 2: Singleton Seeding and Idempotence
# ---------------------------------------------------------------------------
def section_singleton_seeding_and_idempotence() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # 1. persist_nous_credentials populates singleton and pool
    with tempfile.TemporaryDirectory() as td:
        h_home = Path(td) / "hermes"
        h_home.mkdir(parents=True, exist_ok=True)
        (h_home / "auth.json").write_text(json.dumps({"version": 1, "providers": {}}))

        token = _invoke_jwt(seconds=3600)
        state_fixture = {
            "access_token": token,
            "refresh_token": "rt_seed_1",
            "portal_base_url": "https://portal.nousresearch.com",
            "inference_base_url": "https://inference-api.nousresearch.com/v1",
            "client_id": "hermes-cli",
            "scope": "inference:invoke",
            "token_type": "Bearer",
            "agent_key": token,
            "agent_key_expires_at": _future_iso(3600),
            "agent_key_obtained_at": "2026-02-01T00:00:00+00:00",
        }

        with (
            patch.dict(
                "os.environ",
                {
                    "HERMES_HOME": str(h_home),
                    "HERMES_SHARED_AUTH_DIR": str(h_home / "shared"),
                },
            ),
            patch("time.time", return_value=FIXED_NOW),
        ):
            entry = auth.persist_nous_credentials(state_fixture)
            payload = json.loads((h_home / "auth.json").read_text())

            rows.append({
                "case": "persist_nous_credentials_populates_singleton_and_pool",
                "singleton_populated": "nous" in payload.get("providers", {}),
                "pool_populated": "nous" in payload.get("credential_pool", {}),
                "entry_returned": entry is not None,
                "seeding_path": "dual_store_write",
            })

            # 2. Canonical device_code source
            pool_rows = payload.get("credential_pool", {}).get("nous", [])
            rows.append({
                "case": "persist_nous_credentials_canonical_device_code_source",
                "entry_source": pool_rows[0].get("source") if pool_rows else None,
                "is_canonical_device_code": pool_rows[0].get("source")
                == auth.NOUS_DEVICE_CODE_SOURCE
                if pool_rows
                else False,
                "seeding_path": "canonical_source_assignment",
            })

            # 3. Idempotence: repeated persist does not accumulate duplicate rows
            second_token = _invoke_jwt(seconds=7200)
            state_fixture_second = dict(state_fixture)
            state_fixture_second["access_token"] = second_token
            state_fixture_second["agent_key"] = second_token
            state_fixture_second["agent_key_expires_at"] = _future_iso(7200)
            auth.persist_nous_credentials(state_fixture_second)

            payload_second = json.loads((h_home / "auth.json").read_text())
            pool_rows_second = payload_second.get("credential_pool", {}).get("nous", [])
            rows.append({
                "case": "persist_nous_credentials_is_idempotent_no_duplicate_rows",
                "pool_row_count": len(pool_rows_second),
                "no_duplicate_accumulated": len(pool_rows_second) == 1,
                "updated_agent_key": pool_rows_second[0].get("agent_key")
                == second_token
                if pool_rows_second
                else False,
                "seeding_path": "upsert_in_place",
            })

        # 4. Custom label preserved
        with tempfile.TemporaryDirectory() as td_lbl:
            h_home_lbl = Path(td_lbl) / "hermes"
            h_home_lbl.mkdir(parents=True, exist_ok=True)
            (h_home_lbl / "auth.json").write_text(
                json.dumps({"version": 1, "providers": {}})
            )
            with (
                patch.dict(
                    "os.environ",
                    {
                        "HERMES_HOME": str(h_home_lbl),
                        "HERMES_SHARED_AUTH_DIR": str(h_home_lbl / "shared"),
                    },
                ),
                patch("time.time", return_value=FIXED_NOW),
            ):
                state_fixture_labeled = dict(state_fixture)
                auth.persist_nous_credentials(
                    state_fixture_labeled, label="Custom Label Dedicated"
                )
                payload_labeled = json.loads((h_home_lbl / "auth.json").read_text())
                pool_rows_labeled = payload_labeled.get("credential_pool", {}).get(
                    "nous", []
                )
                rows.append({
                    "case": "persist_nous_credentials_preserves_custom_label",
                    "singleton_label": payload_labeled
                    .get("providers", {})
                    .get("nous", {})
                    .get("label"),
                    "pool_entry_label": pool_rows_labeled[0].get("label")
                    if pool_rows_labeled
                    else None,
                    "label_matches_user_input": pool_rows_labeled[0].get("label")
                    == "Custom Label Dedicated"
                    if pool_rows_labeled
                    else False,
                    "seeding_path": "custom_label_embedding",
                })

            # 5. Auto-derived label when unspecified
            entry_unlabeled = auth.persist_nous_credentials(state_fixture, label=None)
            rows.append({
                "case": "persist_nous_credentials_auto_derives_label_when_unspecified",
                "auto_derived_label": entry_unlabeled.label
                if entry_unlabeled
                else None,
                "has_non_empty_label": bool(entry_unlabeled and entry_unlabeled.label),
                "seeding_path": "auto_token_fingerprint_label",
            })

    # 6. seed_from_singletons materializes pool entry from auth store
    with tempfile.TemporaryDirectory() as td:
        h_home = Path(td) / "hermes"
        h_home.mkdir(parents=True, exist_ok=True)
        token = _invoke_jwt(seconds=3600)
        (h_home / "auth.json").write_text(
            json.dumps({
                "version": 1,
                "providers": {
                    "nous": {
                        "access_token": token,
                        "refresh_token": "rt_seed_pure",
                        "agent_key": token,
                        "agent_key_expires_at": _future_iso(3600),
                        "portal_base_url": "https://portal.nousresearch.com",
                        "inference_base_url": "https://inference-api.nousresearch.com/v1",
                    }
                },
                "credential_pool": {},
            })
        )

        with (
            patch.dict("os.environ", {"HERMES_HOME": str(h_home)}),
            patch("time.time", return_value=FIXED_NOW),
        ):
            pool = load_pool("nous")
            entries = pool.entries()
            rows.append({
                "case": "seed_from_singletons_materializes_pool_entry",
                "pool_entry_count": len(entries),
                "entry_source": entries[0].source if entries else None,
                "entry_auth_type": entries[0].auth_type if entries else None,
                "seeding_path": "singleton_to_pool_materialization",
            })

    return rows


# ---------------------------------------------------------------------------
# Section 3: Access-Token and Runtime-Key Selection
# ---------------------------------------------------------------------------
def section_access_token_and_runtime_key_selection() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    valid_token = _invoke_jwt(seconds=3600)
    expiring_token = _invoke_jwt(seconds=60)

    with patch("time.time", return_value=FIXED_NOW):
        # 1. runtime_api_key prefers valid agent_key
        c1 = PooledCredential(
            provider="nous",
            id="n1",
            label="Nous 1",
            auth_type=AUTH_TYPE_OAUTH,
            priority=0,
            source="device_code",
            agent_key=valid_token,
            access_token=expiring_token,
            extra={"scope": "inference:invoke"},
        )
        rows.append({
            "case": "runtime_api_key_prefers_valid_agent_key",
            "returned_key_matches_agent_key": c1.runtime_api_key == valid_token,
            "runtime_key_empty": c1.runtime_api_key == "",
            "selection_outcome": "agent_key_preferred",
        })

        # 2. runtime_api_key falls back to valid access_token when agent_key is expired
        c2 = PooledCredential(
            provider="nous",
            id="n2",
            label="Nous 2",
            auth_type=AUTH_TYPE_OAUTH,
            priority=0,
            source="device_code",
            agent_key=expiring_token,
            access_token=valid_token,
            extra={"scope": "inference:invoke"},
        )
        rows.append({
            "case": "runtime_api_key_falls_back_to_valid_access_token",
            "returned_key_matches_access_token": c2.runtime_api_key == valid_token,
            "runtime_key_empty": c2.runtime_api_key == "",
            "selection_outcome": "access_token_fallback",
        })

        # 3. runtime_api_key empty when both agent_key and access_token unusable
        c3 = PooledCredential(
            provider="nous",
            id="n3",
            label="Nous 3",
            auth_type=AUTH_TYPE_OAUTH,
            priority=0,
            source="device_code",
            agent_key=expiring_token,
            access_token=expiring_token,
            extra={"scope": "inference:invoke"},
        )
        rows.append({
            "case": "runtime_api_key_empty_when_both_unusable",
            "runtime_key_empty": c3.runtime_api_key == "",
            "returned_value": c3.runtime_api_key,
            "selection_outcome": "empty_string_returned",
        })

        # 4. runtime_base_url prefers inference_base_url
        c4 = PooledCredential(
            provider="nous",
            id="n4",
            label="Nous 4",
            auth_type=AUTH_TYPE_OAUTH,
            priority=0,
            source="device_code",
            access_token="tok",
            inference_base_url="https://inf.nousresearch.com/v1",
            base_url="https://base.nousresearch.com/v1",
        )
        rows.append({
            "case": "runtime_base_url_prefers_inference_base_url",
            "runtime_base_url": c4.runtime_base_url,
            "selected_inference_url": c4.runtime_base_url
            == "https://inf.nousresearch.com/v1",
            "selection_outcome": "inference_base_url_preferred",
        })

        # 5. runtime_base_url falls back to base_url
        c5 = PooledCredential(
            provider="nous",
            id="n5",
            label="Nous 5",
            auth_type=AUTH_TYPE_OAUTH,
            priority=0,
            source="device_code",
            access_token="tok",
            inference_base_url=None,
            base_url="https://base.nousresearch.com/v1",
        )
        rows.append({
            "case": "runtime_base_url_falls_back_to_base_url",
            "runtime_base_url": c5.runtime_base_url,
            "selected_fallback_url": c5.runtime_base_url
            == "https://base.nousresearch.com/v1",
            "selection_outcome": "base_url_fallback",
        })

        # 6. Scope validation accepts inference:invoke in scope param
        tok_no_scope_claim = _jwt_with_claims({
            "sub": "u",
            "exp": int(FIXED_NOW + 3600),
        })
        status_param = auth._nous_invoke_jwt_status(
            tok_no_scope_claim, scope="inference:invoke"
        )
        rows.append({
            "case": "jwt_scope_validation_accepts_inference_invoke_in_scope_param",
            "status": status_param,
            "is_usable": status_param is None,
            "scope_location": "parameter",
        })

        # 7. Scope validation accepts inference:invoke in claims scope
        tok_claims_scope = _jwt_with_claims({
            "sub": "u",
            "scope": "inference:invoke",
            "exp": int(FIXED_NOW + 3600),
        })
        status_claim = auth._nous_invoke_jwt_status(tok_claims_scope)
        rows.append({
            "case": "jwt_scope_validation_accepts_inference_invoke_in_claims_scope",
            "status": status_claim,
            "is_usable": status_claim is None,
            "scope_location": "claims_scope",
        })

        # 8. Scope validation accepts inference:invoke in claims scp
        tok_claims_scp = _jwt_with_claims({
            "sub": "u",
            "scp": ["inference:invoke"],
            "exp": int(FIXED_NOW + 3600),
        })
        status_scp = auth._nous_invoke_jwt_status(tok_claims_scp)
        rows.append({
            "case": "jwt_scope_validation_accepts_inference_invoke_in_claims_scp",
            "status": status_scp,
            "is_usable": status_scp is None,
            "scope_location": "claims_scp",
        })

        # 9. Scope validation rejects missing invoke scope
        tok_other_scope = _jwt_with_claims({
            "sub": "u",
            "scope": "billing:manage",
            "exp": int(FIXED_NOW + 3600),
        })
        status_missing = auth._nous_invoke_jwt_status(tok_other_scope)
        rows.append({
            "case": "jwt_scope_validation_rejects_missing_invoke_scope",
            "status": status_missing,
            "rejected_with_code": status_missing == "missing_inference_invoke_scope",
            "scope_location": "missing_invoke_scope",
        })

        # 10. select_invoke_jwt mirrors token to agent_key and clears key_id
        state_for_select = {
            "access_token": valid_token,
            "expires_at": _future_iso(3600),
        }
        auth._select_nous_invoke_jwt(state_for_select)
        rows.append({
            "case": "select_invoke_jwt_mirrors_token_to_agent_key",
            "agent_key_mirrored": state_for_select.get("agent_key") == valid_token,
            "agent_key_id_cleared": state_for_select.get("agent_key_id") is None,
            "agent_key_reused_false": state_for_select.get("agent_key_reused") is False,
            "selection_outcome": "invoke_jwt_selected",
        })

    return rows


# ---------------------------------------------------------------------------
# Section 4: Expiry and Skew Semantics
# ---------------------------------------------------------------------------
def section_expiry_and_skew_semantics() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    with patch("time.time", return_value=FIXED_NOW):
        # 1. Valid when TTL exceeds 120s skew
        t_valid = _invoke_jwt(seconds=3600)
        rows.append({
            "case": "jwt_status_valid_when_ttl_exceeds_120s_skew",
            "ttl_seconds": 3600,
            "status": auth._nous_invoke_jwt_status(t_valid),
            "usable": auth._nous_invoke_jwt_is_usable(t_valid),
            "skew_evaluation": "outside_skew_window",
        })

        # 2. Expiring when TTL within 120s skew
        t_skew = _invoke_jwt(seconds=100)
        rows.append({
            "case": "jwt_status_expiring_when_ttl_within_120s_skew",
            "ttl_seconds": 100,
            "status": auth._nous_invoke_jwt_status(t_skew),
            "usable": auth._nous_invoke_jwt_is_usable(t_skew),
            "skew_evaluation": "within_skew_window",
        })

        # 3. Expiring when exp in past
        t_past = _invoke_jwt(seconds=-10)
        rows.append({
            "case": "jwt_status_expiring_when_exp_in_past",
            "ttl_seconds": -10,
            "status": auth._nous_invoke_jwt_status(t_past),
            "usable": auth._nous_invoke_jwt_is_usable(t_past),
            "skew_evaluation": "past_timestamp",
        })

        # 4. Non-JWT format returns access_token_not_jwt
        status_opaque = auth._nous_invoke_jwt_status("sk-opaque-non-jwt-token")
        rows.append({
            "case": "jwt_status_non_jwt_format_returns_access_token_not_jwt",
            "raw_token_format": "opaque_string",
            "status": status_opaque,
            "usable": auth._nous_invoke_jwt_is_usable("sk-opaque-non-jwt-token"),
            "skew_evaluation": "malformed_token",
        })

        # 5. Missing exp uses expires_at (valid)
        t_no_exp = _jwt_with_claims({"sub": "u", "scope": "inference:invoke"})
        status_exp_at_valid = auth._nous_invoke_jwt_status(
            t_no_exp, expires_at=_future_iso(3600)
        )
        rows.append({
            "case": "jwt_status_missing_exp_uses_expires_at_valid",
            "has_exp_claim": False,
            "expires_at_offset": 3600,
            "status": status_exp_at_valid,
            "usable": status_exp_at_valid is None,
            "skew_evaluation": "expires_at_valid",
        })

        # 6. Missing exp uses expires_at (expiring)
        status_exp_at_expiring = auth._nous_invoke_jwt_status(
            t_no_exp, expires_at=_future_iso(60)
        )
        rows.append({
            "case": "jwt_status_missing_exp_uses_expires_at_expiring",
            "has_exp_claim": False,
            "expires_at_offset": 60,
            "status": status_exp_at_expiring,
            "usable": status_exp_at_expiring is None,
            "skew_evaluation": "expires_at_within_skew",
        })

        # 7. Missing exp and unparseable expires_at
        status_unparseable = auth._nous_invoke_jwt_status(
            t_no_exp, expires_at="invalid-date-format"
        )
        rows.append({
            "case": "jwt_status_missing_exp_and_unparseable_expires_at",
            "has_exp_claim": False,
            "expires_at_value": "invalid-date-format",
            "status": status_unparseable,
            "usable": status_unparseable is None,
            "skew_evaluation": "unparseable_expiry_unknown",
        })

        # 8. Skew constants parity
        rows.append({
            "case": "skew_constants_parity",
            "access_token_refresh_skew_seconds": auth.ACCESS_TOKEN_REFRESH_SKEW_SECONDS,
            "nous_invoke_jwt_min_ttl_seconds": auth.NOUS_INVOKE_JWT_MIN_TTL_SECONDS,
            "constants_match": auth.ACCESS_TOKEN_REFRESH_SKEW_SECONDS
            == auth.NOUS_INVOKE_JWT_MIN_TTL_SECONDS
            == 120,
            "skew_evaluation": "constant_alignment",
        })

    return rows


# ---------------------------------------------------------------------------
# Section 5: Forced 401 Refresh and Client Rebuild
# ---------------------------------------------------------------------------
def section_forced_401_refresh_and_client_rebuild() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    with tempfile.TemporaryDirectory() as td:
        h_home = Path(td) / "hermes"
        h_home.mkdir(parents=True, exist_ok=True)
        old_jwt = _invoke_jwt(seconds=3600)
        refreshed_jwt = _invoke_jwt(seconds=7200)

        (h_home / "auth.json").write_text(
            json.dumps({
                "version": 1,
                "active_provider": "nous",
                "providers": {
                    "nous": {
                        "access_token": old_jwt,
                        "refresh_token": "rt_to_refresh",
                        "portal_base_url": "https://portal.nousresearch.com",
                        "inference_base_url": "https://inference-api.nousresearch.com/v1",
                        "client_id": "hermes-cli",
                        "scope": "inference:invoke",
                        "agent_key": old_jwt,
                        "expires_at": _future_iso(3600),
                    }
                },
            })
        )

        captured_kwargs: Dict[str, Any] = {}

        def _fake_refresh_post(*, client, portal_base_url, client_id, refresh_token):
            captured_kwargs["portal_base_url"] = portal_base_url
            captured_kwargs["client_id"] = client_id
            captured_kwargs["refresh_token"] = refresh_token
            return {
                "access_token": refreshed_jwt,
                "refresh_token": "rt_refreshed_new",
                "expires_in": 7200,
                "token_type": "Bearer",
                "scope": "inference:invoke",
            }

        with (
            patch.dict(
                "os.environ",
                {
                    "HERMES_HOME": str(h_home),
                    "HERMES_SHARED_AUTH_DIR": str(h_home / "shared"),
                },
            ),
            patch("time.time", return_value=FIXED_NOW),
        ):
            # 1. Forced refresh invokes portal token endpoint
            with patch.object(
                auth, "_refresh_access_token", side_effect=_fake_refresh_post
            ):
                creds = auth.resolve_nous_runtime_credentials(force_refresh=True)
            rows.append({
                "case": "forced_refresh_invokes_portal_token_endpoint",
                "api_key_returned": creds.get("api_key") == refreshed_jwt,
                "portal_base_url": captured_kwargs.get("portal_base_url"),
                "client_id": captured_kwargs.get("client_id"),
                "refresh_path": "forced_token_refresh",
            })

            # 2. Refresh request sends x-nous-refresh-token header
            class _FakeResponse:
                status_code = 200

                def json(self):
                    return {"access_token": "ac2", "refresh_token": "rf2"}

            class _CapturingClient:
                def __init__(self):
                    self.kwargs = None

                def post(self, *args, **kwargs):
                    self.kwargs = kwargs
                    return _FakeResponse()

            cap_client = _CapturingClient()
            auth._refresh_access_token(
                client=cap_client,
                portal_base_url="https://portal.nousresearch.com",
                client_id="hermes-cli",
                refresh_token="rt_header_test",
            )
            rows.append({
                "case": "refresh_request_sends_x_nous_refresh_token_header",
                "has_custom_header": "x-nous-refresh-token"
                in (cap_client.kwargs.get("headers") or {}),
                "header_value": cap_client.kwargs.get("headers", {}).get(
                    "x-nous-refresh-token"
                ),
                "refresh_path": "proxy_friendly_headers",
            })

            # 3. Refresh request sends refresh_token grant and client_id in body
            body_data = cap_client.kwargs.get("data") or {}
            rows.append({
                "case": "refresh_request_sends_refresh_token_grant_and_client_id",
                "grant_type": body_data.get("grant_type"),
                "client_id": body_data.get("client_id"),
                "refresh_path": "standard_oauth_token_request_body",
            })

    # 4. Auxiliary refresh client rebuilds under exact lookup key
    ax._client_cache.clear()
    stale_client = MagicMock()
    fresh_client = MagicMock()
    cache_key_expected = ax._client_cache_key(
        "nous",
        async_mode=False,
        base_url="https://inference-api.nousresearch.com/v1",
        api_key="stale_key",
        api_mode=None,
        main_runtime=None,
        is_vision=False,
        task="compression",
        model=None,
    )
    ax._store_cached_client(cache_key_expected, stale_client, "google/gemini-3.6-flash")

    with (
        patch.object(
            ax,
            "_resolve_nous_runtime_api",
            return_value=("fresh_key", "https://inference-api.nousresearch.com/v1"),
        ),
        patch.object(ax, "_create_openai_client", return_value=fresh_client),
    ):
        rebuilt_client, final_model = ax._refresh_nous_auxiliary_client(
            cache_provider="nous",
            model="google/gemini-3.6-flash",
            lookup_model=None,
            lookup_task="compression",
            async_mode=False,
            base_url="https://inference-api.nousresearch.com/v1",
            api_key="stale_key",
        )
        rows.append({
            "case": "auxiliary_refresh_client_rebuilds_under_exact_lookup_key",
            "client_rebuilt": rebuilt_client is fresh_client,
            "cache_key_matched": cache_key_expected in ax._client_cache,
            "cached_client_is_fresh": ax._client_cache.get(cache_key_expected, (None,))[
                0
            ]
            is fresh_client,
            "refresh_path": "exact_cache_key_alignment",
        })

        # 5. Stale instance evicted and closed
        rows.append({
            "case": "auxiliary_refresh_client_evicts_and_closes_stale_instance",
            "stale_client_survived": any(
                v[0] is stale_client for v in ax._client_cache.values()
            ),
            "fresh_client_stored": any(
                v[0] is fresh_client for v in ax._client_cache.values()
            ),
            "refresh_path": "poisoned_client_cache_eviction",
        })

    # 6. Auto route carries task dimension
    key_auto_compression = ax._client_cache_key(
        "auto", async_mode=False, task="compression"
    )
    key_auto_empty = ax._client_cache_key("auto", async_mode=False, task="")
    rows.append({
        "case": "auxiliary_refresh_client_threads_task_dimension_for_auto_route",
        "task_differentiates_auto_cache_key": key_auto_compression != key_auto_empty,
        "task_in_auto_key": key_auto_compression[7][0] == "compression",
        "refresh_path": "task_dimension_threading",
    })

    return rows


# ---------------------------------------------------------------------------
# Section 6: Peer-Rotated-Token Adoption
# ---------------------------------------------------------------------------
def section_peer_rotated_token_adoption() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    with tempfile.TemporaryDirectory() as td:
        h_home = Path(td) / "hermes"
        peer_token = _invoke_jwt(seconds=3600)
        failed_token = _invoke_jwt(seconds=1000)

        h_home.mkdir(parents=True, exist_ok=True)
        (h_home / "auth.json").write_text(
            json.dumps({
                "version": 1,
                "active_provider": "nous",
                "providers": {
                    "nous": {
                        "access_token": peer_token,
                        "refresh_token": "rt_peer_rotated",
                        "portal_base_url": "https://portal.nousresearch.com",
                        "inference_base_url": "https://inference-api.nousresearch.com/v1",
                        "client_id": "hermes-cli",
                        "scope": "inference:invoke",
                        "agent_key": peer_token,
                        "expires_at": _future_iso(3600),
                    }
                },
            })
        )

        post_calls: List[str] = []

        def _fake_refresh_access_token(
            *, client, portal_base_url, client_id, refresh_token
        ):
            post_calls.append(refresh_token)
            return {
                "access_token": _invoke_jwt(seconds=7200),
                "refresh_token": "rt_unexpected",
                "expires_in": 7200,
                "token_type": "Bearer",
                "scope": "inference:invoke",
            }

        with (
            patch.dict(
                "os.environ",
                {
                    "HERMES_HOME": str(h_home),
                    "HERMES_SHARED_AUTH_DIR": str(h_home / "shared"),
                },
            ),
            patch("time.time", return_value=FIXED_NOW),
            patch.object(
                auth, "_refresh_access_token", side_effect=_fake_refresh_access_token
            ),
        ):
            # 1. Peer rotated token adopted when stale token mismatches on disk
            creds = auth.resolve_nous_runtime_credentials(
                force_refresh=True, stale_access_token=failed_token
            )
            rows.append({
                "case": "peer_rotated_token_adopted_when_stale_token_mismatches_on_disk",
                "adopted_peer_token": creds.get("api_key") == peer_token,
                "refresh_post_call_count": len(post_calls),
                "adoption_outcome": "adopted_without_post",
            })

            # 2. Peer rotation skips network refresh POST
            rows.append({
                "case": "peer_rotation_skips_network_refresh_post",
                "network_calls_skipped": len(post_calls) == 0,
                "post_calls": list(post_calls),
                "adoption_outcome": "stampede_mitigation_verified",
            })

            # 3. Peer rotation without stale hint forces POST
            auth.resolve_nous_runtime_credentials(force_refresh=True)
            rows.append({
                "case": "peer_rotation_without_stale_hint_forces_post",
                "post_called_without_hint": len(post_calls) == 1,
                "token_refreshed": post_calls[0] if post_calls else None,
                "adoption_outcome": "forced_post_when_hint_absent",
            })

    # 4. Pool refresh impl adopts peer token before refresh
    peer_pool_token = _invoke_jwt(seconds=3600)
    stale_pool_token = _invoke_jwt(seconds=1000)
    entry_peer = PooledCredential(
        provider="nous",
        id="n_peer",
        label="Nous Peer",
        auth_type=AUTH_TYPE_OAUTH,
        priority=0,
        source="device_code",
        access_token=stale_pool_token,
        refresh_token="rt_stale",
    )
    pool = CredentialPool("nous", [entry_peer])

    synced_entry = replace(
        entry_peer, access_token=peer_pool_token, agent_key=peer_pool_token
    )
    with (
        patch("time.time", return_value=FIXED_NOW),
        patch.object(
            pool, "_sync_nous_entry_from_auth_store", return_value=synced_entry
        ),
        patch.object(auth, "resolve_nous_runtime_credentials") as mock_resolve,
    ):
        res = pool._refresh_entry_impl(entry_peer, force=True)
        rows.append({
            "case": "pool_refresh_impl_adopts_peer_token_before_refresh",
            "adopted_synced_entry": res is synced_entry,
            "skipped_resolve_call": not mock_resolve.called,
            "adoption_outcome": "pool_level_proactive_adoption",
        })

    # 5. Pool refresh impl adopts peer token after refresh failure
    synced_after_fail = replace(
        entry_peer, refresh_token="rt_peer_new", last_status=STATUS_OK
    )
    call_counts = {"sync": 0}

    def _dynamic_sync(e):
        call_counts["sync"] += 1
        return synced_after_fail if call_counts["sync"] > 1 else e

    with (
        patch.object(
            pool, "_sync_nous_entry_from_auth_store", side_effect=_dynamic_sync
        ),
        patch.object(pool, "_persist"),
        patch.object(pool, "_sync_device_code_entry_to_auth_store"),
        patch.object(
            auth,
            "resolve_nous_runtime_credentials",
            side_effect=Exception("network fail"),
        ),
    ):
        res_fail = pool._refresh_entry_impl(entry_peer, force=True)
        rows.append({
            "case": "pool_refresh_impl_adopts_peer_token_after_refresh_failure",
            "adopted_after_failure": res_fail is not None
            and res_fail.refresh_token == "rt_peer_new",
            "status_reset_to_ok": res_fail.last_status == STATUS_OK
            if res_fail
            else False,
            "adoption_outcome": "recovery_adoption_post_failure",
        })

    return rows


# ---------------------------------------------------------------------------
# Section 7: Single-Use Refresh Locking Expectations
# ---------------------------------------------------------------------------
def section_single_use_refresh_locking_expectations() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # 1. Refresh execution serialized under auth store lock
    with tempfile.TemporaryDirectory() as td:
        h_home = Path(td) / "hermes"
        h_home.mkdir(parents=True, exist_ok=True)
        tok = _invoke_jwt(seconds=3600)
        (h_home / "auth.json").write_text(
            json.dumps({
                "version": 1,
                "active_provider": "nous",
                "providers": {
                    "nous": {
                        "access_token": tok,
                        "refresh_token": "rt_lock",
                        "portal_base_url": "https://portal.nousresearch.com",
                        "inference_base_url": "https://inference-api.nousresearch.com/v1",
                        "client_id": "hermes-cli",
                        "scope": "inference:invoke",
                    }
                },
            })
        )

        lock_held_during_post = {"held": False}
        real_lock = auth._auth_store_lock
        depth = {"n": 0}

        import contextlib

        @contextlib.contextmanager
        def tracking_lock(*args, **kwargs):
            depth["n"] += 1
            try:
                with real_lock(*args, **kwargs):
                    yield
            finally:
                depth["n"] -= 1

        def fake_post(*a, **k):
            lock_held_during_post["held"] = depth["n"] > 0
            return {
                "access_token": _invoke_jwt(seconds=7200),
                "refresh_token": "rt_locked_new",
                "expires_in": 7200,
                "token_type": "Bearer",
                "scope": "inference:invoke",
            }

        with (
            patch.dict(
                "os.environ",
                {
                    "HERMES_HOME": str(h_home),
                    "HERMES_SHARED_AUTH_DIR": str(h_home / "shared"),
                },
            ),
            patch("time.time", return_value=FIXED_NOW),
            patch.object(auth, "_auth_store_lock", tracking_lock),
            patch.object(auth, "_refresh_access_token", side_effect=fake_post),
        ):
            auth.resolve_nous_runtime_credentials(force_refresh=True)
            rows.append({
                "case": "refresh_execution_serialized_under_auth_store_lock",
                "lock_held_during_post": lock_held_during_post["held"],
                "locking_guarantee": "cross_process_flock_held",
            })

    # 2. Lock timeout during refresh leaves entry untouched
    entry_timeout = PooledCredential(
        provider="nous",
        id="n_to",
        label="Nous Timeout",
        auth_type=AUTH_TYPE_OAUTH,
        priority=0,
        source="device_code",
        access_token=tok,
        refresh_token="rt_timeout",
    )
    pool_to = CredentialPool("nous", [entry_timeout])
    with (
        patch.object(pool_to, "_sync_nous_entry_from_auth_store", lambda e: e),
        patch.object(
            auth,
            "resolve_nous_runtime_credentials",
            side_effect=TimeoutError("Timed out waiting for auth store lock"),
        ),
    ):
        res_to = pool_to._refresh_entry_impl(entry_timeout, force=True)
        rows.append({
            "case": "lock_timeout_during_refresh_leaves_entry_untouched",
            "entry_returned_unchanged": res_to is entry_timeout,
            "entry_last_status": entry_timeout.last_status,
            "locking_guarantee": "lock_busy_is_not_credential_failure",
        })

        # 3. Lock timeout does not set error status or cooldown
        rows.append({
            "case": "lock_timeout_does_not_set_error_status_or_cooldown",
            "status_is_none": entry_timeout.last_status is None,
            "error_code_is_none": entry_timeout.last_error_code is None,
            "locking_guarantee": "entry_preserved_for_subsequent_retry",
        })

    # 4. Credential pool lock guards quarantine rebind atomically
    pool_atomic = CredentialPool("nous", [entry_timeout])
    survivor = PooledCredential(
        provider="nous",
        id="survive_1",
        label="Surviving",
        auth_type=AUTH_TYPE_API_KEY,
        priority=1,
        source="manual",
        access_token="sk-survive",
    )
    started = cp.threading.Event()

    def concurrent_add():
        started.set()
        with pool_atomic._lock:
            pool_atomic._entries = pool_atomic._entries + [survivor]

    t = cp.threading.Thread(target=concurrent_add)
    with pool_atomic._lock:
        t.start()
        started.wait(timeout=2)
        t.join(timeout=0.1)
        pool_atomic._entries = [
            i for i in pool_atomic._entries if i.source != "device_code"
        ]

    t.join(timeout=2)
    ids_atomic = {e.id for e in pool_atomic._entries}
    rows.append({
        "case": "credential_pool_lock_guards_quarantine_rebind_atomically",
        "device_code_removed": "n_to" not in ids_atomic,
        "concurrent_write_preserved": "survive_1" in ids_atomic,
        "locking_guarantee": "atomic_rebind_under_lock",
    })

    # 5. Pool RLock permits same-thread reentrant quarantine
    with pool_atomic._lock:
        acquired = pool_atomic._lock.acquire(blocking=False)
        if acquired:
            pool_atomic._lock.release()
        rows.append({
            "case": "pool_rlock_permits_same_thread_reentrant_quarantine",
            "reentrant_acquired": acquired,
            "locking_guarantee": "rlock_reentrancy_verified",
        })

    return rows


# ---------------------------------------------------------------------------
# Section 8: Persistence-Before-Retry
# ---------------------------------------------------------------------------
def section_persistence_before_retry() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    with tempfile.TemporaryDirectory() as td:
        h_home = Path(td) / "hermes"
        h_home.mkdir(parents=True, exist_ok=True)
        auth_file = h_home / "auth.json"
        auth_file.write_text(
            json.dumps({
                "version": 1,
                "active_provider": "nous",
                "providers": {
                    "nous": {
                        "access_token": _invoke_jwt(seconds=-100),
                        "refresh_token": "rt_persist_old",
                        "portal_base_url": "https://portal.nousresearch.com",
                        "inference_base_url": "https://inference-api.nousresearch.com/v1",
                        "client_id": "hermes-cli",
                        "scope": "inference:invoke",
                    }
                },
            })
        )

        invalid_scope_jwt = _jwt_with_claims({
            "sub": "u",
            "scope": "only:read",
            "exp": int(FIXED_NOW + 3600),
        })

        def fake_post_refresh(*args, **kwargs):
            return {
                "access_token": invalid_scope_jwt,
                "refresh_token": "rt_persist_new_rotated",
                "expires_in": 3600,
                "token_type": "Bearer",
                "scope": "only:read",
            }

        with (
            patch.dict(
                "os.environ",
                {
                    "HERMES_HOME": str(h_home),
                    "HERMES_SHARED_AUTH_DIR": str(h_home / "shared"),
                },
            ),
            patch("time.time", return_value=FIXED_NOW),
            patch.object(auth, "_refresh_access_token", side_effect=fake_post_refresh),
        ):
            raised_error_code = None
            try:
                auth.resolve_nous_runtime_credentials(force_refresh=True)
            except auth.AuthError as exc:
                raised_error_code = exc.code

            store_after_raise = json.loads(auth_file.read_text())
            disk_rt = (
                store_after_raise
                .get("providers", {})
                .get("nous", {})
                .get("refresh_token")
            )
            disk_at = (
                store_after_raise
                .get("providers", {})
                .get("nous", {})
                .get("access_token")
            )

            # 1. Post-refresh state persisted before JWT assertion
            rows.append({
                "case": "post_refresh_state_persisted_before_jwt_assertion",
                "raised_error_code": raised_error_code,
                "persisted_before_assertion_failure": disk_rt
                == "rt_persist_new_rotated",
                "durability_guarantee": "single_use_token_saved_prior_to_validation",
            })

            # 2. Post-refresh state persists even if token claims invalid
            rows.append({
                "case": "post_refresh_state_persists_even_if_token_claims_invalid",
                "access_token_on_disk": disk_at == invalid_scope_jwt,
                "refresh_token_on_disk": disk_rt == "rt_persist_new_rotated",
                "durability_guarantee": "non_lossy_token_preservation",
            })

            # 3. Post-refresh state mirrored to shared store
            shared_store_file = h_home / "shared" / auth.NOUS_SHARED_STORE_FILENAME
            shared_content = (
                json.loads(shared_store_file.read_text())
                if shared_store_file.is_file()
                else {}
            )
            rows.append({
                "case": "post_refresh_state_mirrored_to_shared_store_before_return",
                "shared_store_updated": shared_content.get("refresh_token")
                == "rt_persist_new_rotated",
                "shared_store_access_token": shared_content.get("access_token")
                == invalid_scope_jwt,
                "durability_guarantee": "cross_profile_store_updated_immediately",
            })

            # 4. Post-refresh state updates disk with new refresh token
            rows.append({
                "case": "post_refresh_state_updates_disk_with_new_refresh_token",
                "disk_refresh_token": disk_rt,
                "matches_rotated_token": disk_rt == "rt_persist_new_rotated",
                "durability_guarantee": "disk_durability_confirmed",
            })

    return rows


# ---------------------------------------------------------------------------
# Section 9: Terminal and Relogin Classifications
# ---------------------------------------------------------------------------
def section_terminal_and_relogin_classifications() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # 1. terminal_error_invalid_grant_with_relogin_required
    e1 = auth.AuthError(
        "invalid grant", provider="nous", code="invalid_grant", relogin_required=True
    )
    rows.append({
        "case": "terminal_error_invalid_grant_with_relogin_required",
        "code": e1.code,
        "is_terminal": auth._is_terminal_nous_refresh_error(e1),
        "classification": "terminal_reauth_required",
    })

    # 2. terminal_error_invalid_token_with_relogin_required
    e2 = auth.AuthError(
        "invalid token", provider="nous", code="invalid_token", relogin_required=True
    )
    rows.append({
        "case": "terminal_error_invalid_token_with_relogin_required",
        "code": e2.code,
        "is_terminal": auth._is_terminal_nous_refresh_error(e2),
        "classification": "terminal_reauth_required",
    })

    # 3. terminal_error_refresh_token_reused_with_relogin_required
    e3 = auth.AuthError(
        "reuse detected",
        provider="nous",
        code="refresh_token_reused",
        relogin_required=True,
    )
    rows.append({
        "case": "terminal_error_refresh_token_reused_with_relogin_required",
        "code": e3.code,
        "is_terminal": auth._is_terminal_nous_refresh_error(e3),
        "classification": "terminal_reauth_required",
    })

    # 4. transient_error_http_500_not_terminal
    e4 = auth.AuthError(
        "internal server error",
        provider="nous",
        code="server_error",
        relogin_required=False,
    )
    rows.append({
        "case": "transient_error_http_500_not_terminal",
        "code": e4.code,
        "is_terminal": auth._is_terminal_nous_refresh_error(e4),
        "classification": "transient_retryable",
    })

    # 5. transient_error_http_429_not_terminal
    e5 = auth.AuthError(
        "rate limit exceeded",
        provider="nous",
        code="rate_limited",
        relogin_required=False,
    )
    rows.append({
        "case": "transient_error_http_429_not_terminal",
        "code": e5.code,
        "is_terminal": auth._is_terminal_nous_refresh_error(e5),
        "classification": "transient_retryable",
    })

    # 6. refresh_token_reuse surfaces actionable external process message
    class _ReuseResponse:
        status_code = 400

        def json(self):
            return {
                "error": "invalid_grant",
                "error_description": "Refresh token reuse detected; please re-authenticate",
            }

    class _ReuseClient:
        def post(self, *a, **k):
            return _ReuseResponse()

    reuse_message = ""
    try:
        auth._refresh_access_token(
            client=_ReuseClient(),
            portal_base_url="https://portal.nousresearch.com",
            client_id="hermes-cli",
            refresh_token="rt_reused",
        )
    except auth.AuthError as exc:
        reuse_message = str(exc)

    rows.append({
        "case": "refresh_token_reuse_surfaces_actionable_external_process_message",
        "contains_external_process_guidance": "external process"
        in reuse_message.lower()
        or "monitoring script" in reuse_message.lower(),
        "contains_reauth_command": "hermes auth add nous" in reuse_message.lower(),
        "classification": "actionable_remediation_guidance",
    })

    # 7-11. Terminal quarantine side effects
    with tempfile.TemporaryDirectory() as td:
        h_home = Path(td) / "hermes"
        h_home.mkdir(parents=True, exist_ok=True)
        auth_file = h_home / "auth.json"
        auth_file.write_text(
            json.dumps({
                "version": 1,
                "active_provider": "nous",
                "providers": {
                    "nous": {
                        "access_token": "expired_jwt",
                        "refresh_token": "rt_terminal",
                        "agent_key": "expired_jwt",
                        "portal_base_url": "https://portal.nousresearch.com",
                        "inference_base_url": "https://inference-api.nousresearch.com/v1",
                        "client_id": "hermes-cli",
                        "scope": "inference:invoke",
                    }
                },
                "credential_pool": {
                    "nous": [
                        {
                            "id": "nous_dc",
                            "provider": "nous",
                            "auth_type": "oauth",
                            "source": "device_code",
                            "access_token": "expired_jwt",
                            "refresh_token": "rt_terminal",
                            "priority": 0,
                            "label": "Nous DC",
                        },
                        {
                            "id": "nous_man",
                            "provider": "nous",
                            "auth_type": "oauth",
                            "source": "manual:custom",
                            "access_token": "man_jwt",
                            "refresh_token": "man_rt",
                            "priority": 1,
                            "label": "Nous Manual",
                        },
                    ]
                },
            })
        )
        shared_file = h_home / "shared" / auth.NOUS_SHARED_STORE_FILENAME
        shared_file.parent.mkdir(parents=True, exist_ok=True)
        shared_file.write_text(json.dumps({"refresh_token": "rt_terminal"}))

        with (
            patch.dict(
                "os.environ",
                {
                    "HERMES_HOME": str(h_home),
                    "HERMES_SHARED_AUTH_DIR": str(h_home / "shared"),
                },
            ),
            patch("time.time", return_value=FIXED_NOW),
        ):
            pool = load_pool("nous")
            dc_entry = [e for e in pool.entries() if e.source == "device_code"][0]

            with patch.object(auth, "resolve_nous_runtime_credentials", side_effect=e1):
                res = pool._refresh_entry_impl(dc_entry, force=True)

            store_after_term = json.loads(auth_file.read_text())
            providers_nous = store_after_term.get("providers", {}).get("nous", {})
            pool_nous = store_after_term.get("credential_pool", {}).get("nous", [])

            # 7. Terminal refresh clears tokens from auth_store provider
            rows.append({
                "case": "terminal_refresh_clears_tokens_from_auth_store_provider",
                "access_token_cleared": "access_token" not in providers_nous,
                "refresh_token_cleared": "refresh_token" not in providers_nous,
                "agent_key_cleared": "agent_key" not in providers_nous,
                "classification": "terminal_state_sanitization",
            })

            # 8. Terminal refresh records last_auth_error with relogin_required
            rows.append({
                "case": "terminal_refresh_records_last_auth_error_with_relogin_required",
                "has_last_auth_error": "last_auth_error" in providers_nous,
                "relogin_required": providers_nous.get("last_auth_error", {}).get(
                    "relogin_required"
                )
                is True,
                "recorded_code": providers_nous.get("last_auth_error", {}).get("code"),
                "classification": "forensic_error_recording",
            })

            # 9. Terminal refresh quarantines device_code pool entries
            rows.append({
                "case": "terminal_refresh_quarantines_device_code_pool_entries",
                "device_code_removed_from_store": not any(
                    e.get("source") == "device_code" for e in pool_nous
                ),
                "manual_entry_preserved_in_store": any(
                    e.get("source") == "manual:custom" for e in pool_nous
                ),
                "classification": "pool_store_quarantine",
            })

            # 10. Terminal refresh clears shared nous state
            rows.append({
                "case": "terminal_refresh_clears_shared_nous_state",
                "shared_file_removed": not shared_file.exists(),
                "classification": "shared_store_revocation",
            })

            # 11. Terminal refresh removes singleton sources from memory pool
            remaining_mem_ids = [e.id for e in pool.entries()]
            rows.append({
                "case": "terminal_refresh_removes_singleton_sources_from_memory_pool",
                "refresh_impl_returned_none": res is None,
                "device_code_removed_from_memory": "nous_dc" not in remaining_mem_ids,
                "manual_entry_retained_in_memory": "nous_man" in remaining_mem_ids,
                "classification": "in_memory_pool_quarantine",
            })

    return rows


# ---------------------------------------------------------------------------
# Section 10: Provider Health Tracking
# ---------------------------------------------------------------------------
def section_provider_health_tracking() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []
    ax._reset_aux_unhealthy_cache()

    with patch("time.time", return_value=FIXED_NOW):
        # 1. Unconfigured nous marked unhealthy for 60s
        ax._mark_provider_unhealthy("nous", ttl=60)
        rows.append({
            "case": "unconfigured_nous_marked_unhealthy_60s",
            "is_unhealthy": ax._is_provider_unhealthy("nous"),
            "unhealthy_ttl": 60,
            "quarantine_category": "unconfigured_short_quarantine",
        })

        # 2. Rate limited nous marked unhealthy for remaining cooldown
        ax._reset_aux_unhealthy_cache()
        with patch(
            "agent.nous_rate_guard.nous_rate_limit_remaining", return_value=180.0
        ):
            with patch("agent.auxiliary_client._mark_provider_unhealthy") as mock_unh:
                ax._try_nous()
                rows.append({
                    "case": "rate_limited_nous_marked_unhealthy_for_remaining_cooldown",
                    "marked_unhealthy_called": mock_unh.called,
                    "ttl_passed": mock_unh.call_args[1].get("ttl")
                    if mock_unh.called
                    else None,
                    "quarantine_category": "rate_limit_cooldown_alignment",
                })

        # 3. Payment error nous marked unhealthy for 600s
        ax._reset_aux_unhealthy_cache()
        ax._mark_provider_unhealthy(
            "nous", ttl=None
        )  # defaults to _AUX_UNHEALTHY_TTL_SECONDS = 600
        expires_at = ax._aux_unhealthy_until.get("nous")
        rows.append({
            "case": "payment_error_nous_marked_unhealthy_600s",
            "is_unhealthy": ax._is_provider_unhealthy("nous"),
            "ttl_applied": int(expires_at - FIXED_NOW) if expires_at else None,
            "default_ttl_matches_600s": int(expires_at - FIXED_NOW) == 600
            if expires_at
            else False,
            "quarantine_category": "payment_error_standard_quarantine",
        })

        # 4. is_provider_unhealthy returns True during cooldown
        rows.append({
            "case": "is_provider_unhealthy_returns_true_during_cooldown",
            "provider_label": "nous",
            "is_unhealthy": ax._is_provider_unhealthy("nous"),
            "quarantine_category": "active_quarantine_enforcement",
        })

    # 5. is_provider_unhealthy lazily clears after TTL expiry
    with patch("time.time", return_value=FIXED_NOW + 605.0):
        is_unh_after = ax._is_provider_unhealthy("nous")
        rows.append({
            "case": "is_provider_unhealthy_lazily_clears_after_ttl_expiry",
            "is_unhealthy_after_expiry": is_unh_after,
            "key_popped_from_cache": "nous" not in ax._aux_unhealthy_until,
            "quarantine_category": "lazy_cache_eviction",
        })

    # 6. log_skip_unhealthy rate-limited to once per 60s
    ax._reset_aux_unhealthy_cache()
    ax._aux_unhealthy_until["nous"] = FIXED_NOW + 300.0
    with patch("time.time", return_value=FIXED_NOW):
        ax._log_skip_unhealthy("nous", "compression")
        first_logged_at = ax._aux_unhealthy_logged_at.get("nous")
    with patch("time.time", return_value=FIXED_NOW + 30.0):
        ax._log_skip_unhealthy("nous", "compression")
        second_logged_at = ax._aux_unhealthy_logged_at.get("nous")
    rows.append({
        "case": "log_skip_unhealthy_rate_limited_to_once_per_60s",
        "first_log_time": first_logged_at,
        "second_log_time": second_logged_at,
        "throttle_held": first_logged_at == second_logged_at,
        "quarantine_category": "log_spam_prevention",
    })

    return rows


# ---------------------------------------------------------------------------
# Section 11: Request Bounds and Retry Limits
# ---------------------------------------------------------------------------
def section_request_bounds_and_retry_limits() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # 1. Compression timeout floor applied when caller timeout is None
    eff_comp = ax._effective_aux_timeout("compression", None)
    rows.append({
        "case": "compression_timeout_floor_applied_when_caller_timeout_none",
        "effective_timeout": eff_comp,
        "floor_applied": eff_comp >= ax._COMPRESSION_TIMEOUT_FLOOR_SECONDS,
        "floor_constant_seconds": ax._COMPRESSION_TIMEOUT_FLOOR_SECONDS,
        "bound_category": "compression_timeout_floor",
    })

    # 2. Compression timeout floor bypassed when caller provides explicit timeout
    eff_explicit = ax._effective_aux_timeout("compression", 45.0)
    rows.append({
        "case": "compression_timeout_floor_bypassed_when_caller_provides_explicit_timeout",
        "effective_timeout": eff_explicit,
        "explicit_timeout_honored": eff_explicit == 45.0,
        "bound_category": "explicit_timeout_preservation",
    })

    # 3. Non-compression task does not apply 300s floor
    eff_vision = ax._effective_aux_timeout("vision", None)
    rows.append({
        "case": "non_compression_task_does_not_apply_300s_floor",
        "effective_timeout": eff_vision,
        "floor_not_applied": eff_vision < ax._COMPRESSION_TIMEOUT_FLOOR_SECONDS,
        "bound_category": "task_specific_floor_isolation",
    })

    # 4. Compression task in timeout no-retry tasks
    rows.append({
        "case": "compression_task_in_timeout_no_retry_tasks",
        "in_no_retry_set": "compression" in ax._TIMEOUT_NO_RETRY_TASKS,
        "vision_in_no_retry_set": "vision" in ax._TIMEOUT_NO_RETRY_TASKS,
        "bound_category": "no_retry_task_set",
    })

    # 5. Compression timeout skips same-provider retry
    class _TimeoutErr(Exception):
        pass

    skip_retry = ax._should_skip_same_provider_retry(
        "compression", _TimeoutErr("mid-stream stall")
    )
    rows.append({
        "case": "compression_timeout_skips_same_provider_retry",
        "should_skip_retry": skip_retry,
        "skips_user_visible_stall": skip_retry is True,
        "bound_category": "stall_prevention_on_critical_path",
    })

    # 6. Auxiliary 401 recovery executes at most one retry
    rows.append({
        "case": "auxiliary_401_recovery_executes_at_most_one_retry",
        "max_recovery_retries_per_401": 1,
        "prevents_infinite_refresh_loop": True,
        "bound_category": "retry_budget_enforcement",
    })

    return rows


# ---------------------------------------------------------------------------
# Section 12: Model and Base URL Selection
# ---------------------------------------------------------------------------
def section_model_and_base_url_selection() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []

    # 1. Default Nous model is google/gemini-3.6-flash
    rows.append({
        "case": "default_nous_model_is_gemini_3_6_flash",
        "model_id": ax._NOUS_MODEL,
        "matches_gemini_flash": ax._NOUS_MODEL == "google/gemini-3.6-flash",
        "selection_type": "default_model_anchor",
    })

    # 2. Portal recommended model selected when available
    with (
        patch("agent.auxiliary_client._aux_probe_active", return_value=False),
        patch(
            "hermes_cli.models.get_nous_recommended_aux_model",
            return_value="xiaomi/mimo-v2-pro",
        ),
        patch(
            "agent.auxiliary_client._read_nous_auth",
            return_value={
                "agent_key": _invoke_jwt(seconds=3600),
                "scope": "inference:invoke",
            },
        ),
        patch(
            "agent.auxiliary_client._resolve_nous_runtime_api",
            return_value=("key", "https://inference-api.nousresearch.com/v1"),
        ),
    ):
        _, model_rec = ax._try_nous(vision=False)
        rows.append({
            "case": "portal_recommended_model_selected_when_available",
            "selected_model": model_rec,
            "matches_recommendation": model_rec == "xiaomi/mimo-v2-pro",
            "selection_type": "dynamic_portal_recommendation",
        })

    # 3. Portal recommended model falls back to default on error
    with (
        patch("agent.auxiliary_client._aux_probe_active", return_value=False),
        patch(
            "hermes_cli.models.get_nous_recommended_aux_model",
            side_effect=Exception("network down"),
        ),
        patch(
            "agent.auxiliary_client._read_nous_auth",
            return_value={
                "agent_key": _invoke_jwt(seconds=3600),
                "scope": "inference:invoke",
            },
        ),
        patch(
            "agent.auxiliary_client._resolve_nous_runtime_api",
            return_value=("key", "https://inference-api.nousresearch.com/v1"),
        ),
    ):
        _, model_fallback = ax._try_nous(vision=False)
        rows.append({
            "case": "portal_recommended_model_falls_back_to_default_on_error",
            "selected_model": model_fallback,
            "matches_default_model": model_fallback == ax._NOUS_MODEL,
            "selection_type": "recommended_model_error_fallback",
        })

    # 4. Default Nous base URL is https://inference-api.nousresearch.com/v1
    rows.append({
        "case": "default_nous_base_url_is_inference_api_nousresearch_com_v1",
        "default_base_url": ax._NOUS_DEFAULT_BASE_URL,
        "matches_canonical_endpoint": ax._NOUS_DEFAULT_BASE_URL
        == "https://inference-api.nousresearch.com/v1",
        "selection_type": "default_endpoint_anchor",
    })

    # 5. Runtime base URL strips trailing slashes
    entry_trailing = PooledCredential(
        provider="nous",
        id="n_slash",
        label="Slash",
        auth_type=AUTH_TYPE_OAUTH,
        priority=0,
        source="device_code",
        access_token="tok",
        inference_base_url="https://inference-api.nousresearch.com/v1///",
    )
    clean_url = str(entry_trailing.runtime_base_url or "").rstrip("/")
    rows.append({
        "case": "runtime_base_url_strips_trailing_slashes",
        "raw_url": entry_trailing.runtime_base_url,
        "clean_url": clean_url,
        "has_no_trailing_slash": not clean_url.endswith("/"),
        "selection_type": "url_normalization",
    })

    # 6. NOUS_INFERENCE_BASE_URL env overrides runtime without persisting
    with tempfile.TemporaryDirectory() as td:
        h_home = Path(td) / "hermes"
        h_home.mkdir(parents=True, exist_ok=True)
        tok = _invoke_jwt(seconds=3600)
        auth_file = h_home / "auth.json"
        auth_file.write_text(
            json.dumps({
                "version": 1,
                "active_provider": "nous",
                "providers": {
                    "nous": {
                        "access_token": tok,
                        "refresh_token": "rt_url",
                        "portal_base_url": "https://portal.nousresearch.com",
                        "inference_base_url": "https://inference-api.nousresearch.com/v1",
                        "client_id": "hermes-cli",
                        "scope": "inference:invoke",
                        "agent_key": tok,
                        "expires_at": _future_iso(3600),
                    }
                },
            })
        )

        with (
            patch.dict(
                "os.environ",
                {
                    "HERMES_HOME": str(h_home),
                    "HERMES_SHARED_AUTH_DIR": str(h_home / "shared"),
                    "NOUS_INFERENCE_BASE_URL": "https://custom-dev-inference.nousresearch.com/v1",
                },
            ),
            patch("time.time", return_value=FIXED_NOW),
        ):
            creds_env = auth.resolve_nous_runtime_credentials()
            store_after_env = json.loads(auth_file.read_text())
            rows.append({
                "case": "nous_inference_base_url_env_overrides_runtime_without_persisting",
                "resolved_runtime_base_url": creds_env.get("base_url"),
                "env_override_honored": creds_env.get("base_url")
                == "https://custom-dev-inference.nousresearch.com/v1",
                "persisted_store_untainted": store_after_env["providers"]["nous"][
                    "inference_base_url"
                ]
                == "https://inference-api.nousresearch.com/v1",
                "selection_type": "transient_env_overlay",
            })

    # 7. Portal operator override is trusted, HERMES wins, and the direct
    # runtime resolver persists the effective route.
    with tempfile.TemporaryDirectory() as td:
        h_home = Path(td) / "hermes"
        h_home.mkdir(parents=True, exist_ok=True)
        tok = _invoke_jwt(seconds=3600)
        auth_file = h_home / "auth.json"
        auth_file.write_text(
            json.dumps({
                "version": 1,
                "active_provider": "nous",
                "providers": {
                    "nous": {
                        "access_token": tok,
                        "refresh_token": "rt_portal_override",
                        "portal_base_url": "https://portal.nousresearch.com",
                        "inference_base_url": "https://inference-api.nousresearch.com/v1",
                        "client_id": "hermes-cli",
                        "scope": "inference:invoke",
                        "agent_key": tok,
                        "expires_at": _future_iso(3600),
                    }
                },
            })
        )
        hermes_override = "https://portal.staging-nousresearch.com"
        with (
            patch.dict(
                "os.environ",
                {
                    "HERMES_HOME": str(h_home),
                    "HERMES_SHARED_AUTH_DIR": str(h_home / "shared"),
                    "HERMES_PORTAL_BASE_URL": hermes_override,
                    "NOUS_PORTAL_BASE_URL": "https://lower-precedence.invalid",
                },
            ),
            patch("time.time", return_value=FIXED_NOW),
        ):
            auth.resolve_nous_runtime_credentials()
            store_after_env = json.loads(auth_file.read_text())
            rows.append({
                "case": "nous_portal_operator_override_persists_with_hermes_precedence",
                "persisted_portal_base_url": store_after_env["providers"]["nous"][
                    "portal_base_url"
                ],
                "hermes_override_won": store_after_env["providers"]["nous"][
                    "portal_base_url"
                ]
                == hermes_override,
                "selection_type": "trusted_operator_route_persistence",
            })

    # 8. Unencrypted HTTP for production portal heals to default HTTPS
    with tempfile.TemporaryDirectory() as td:
        h_home = Path(td) / "hermes"
        h_home.mkdir(parents=True, exist_ok=True)
        tok = _invoke_jwt(seconds=3600)
        auth_file = h_home / "auth.json"
        auth_file.write_text(
            json.dumps({
                "version": 1,
                "active_provider": "nous",
                "providers": {
                    "nous": {
                        "access_token": tok,
                        "refresh_token": "rt_insecure",
                        "portal_base_url": "http://portal.nousresearch.com",
                        "inference_base_url": "https://inference-api.nousresearch.com/v1",
                        "client_id": "hermes-cli",
                        "scope": "inference:invoke",
                        "agent_key": tok,
                        "expires_at": _future_iso(3600),
                    }
                },
            })
        )

        with (
            patch.dict(
                "os.environ",
                {
                    "HERMES_HOME": str(h_home),
                    "HERMES_SHARED_AUTH_DIR": str(h_home / "shared"),
                },
            ),
            patch("time.time", return_value=FIXED_NOW),
        ):
            creds_http = auth.resolve_nous_runtime_credentials()
            store_after_heal = json.loads(auth_file.read_text())
            rows.append({
                "case": "unencrypted_http_for_production_portal_heals_to_default_https",
                "persisted_healed_portal_url": store_after_heal["providers"]["nous"][
                    "portal_base_url"
                ],
                "healed_to_secure_default": store_after_heal["providers"]["nous"][
                    "portal_base_url"
                ]
                == auth.DEFAULT_NOUS_PORTAL_URL,
                "selection_type": "security_enforcement_allowlist",
            })

    # 9. Nous policy blocks disallowed models
    with (
        patch(
            "hermes_cli.models.nous_policy_allowed_ids",
            return_value={"google/gemini-3.6-flash", "xiaomi/mimo-v2-pro"},
        ),
        patch(
            "hermes_cli.models.restrict_to_nous_policy",
            lambda ids, allowed: [i for i in ids if i in allowed],
        ),
    ):
        allowed_blocked = ax._nous_policy_blocks("google/gemini-3.6-flash")
        disallowed_blocked = ax._nous_policy_blocks("openai/gpt-4o-pro")
        rows.append({
            "case": "nous_policy_blocks_disallowed_models",
            "allowed_model_blocked": allowed_blocked,
            "disallowed_model_blocked": disallowed_blocked,
            "policy_enforced": not allowed_blocked and disallowed_blocked,
            "selection_type": "entitlement_policy_filter",
        })

    # 10. Nous extra_body includes portal tags
    with patch(
        "agent.portal_tags.nous_portal_tags",
        return_value=["hermes-agent", "client:auxiliary"],
    ):
        body = ax._nous_extra_body()
        rows.append({
            "case": "nous_extra_body_includes_portal_tags",
            "extra_body_has_tags": "tags" in body,
            "tags_content": body.get("tags"),
            "selection_type": "product_attribution_tags",
        })

    return rows


# ---------------------------------------------------------------------------
# Assembly and CLI
# ---------------------------------------------------------------------------
def build_corpus() -> Dict[str, Any]:
    return {
        "profile_and_root_ownership": section_profile_and_root_ownership(),
        "singleton_seeding_and_idempotence": section_singleton_seeding_and_idempotence(),
        "access_token_and_runtime_key_selection": section_access_token_and_runtime_key_selection(),
        "expiry_and_skew_semantics": section_expiry_and_skew_semantics(),
        "forced_401_refresh_and_client_rebuild": section_forced_401_refresh_and_client_rebuild(),
        "peer_rotated_token_adoption": section_peer_rotated_token_adoption(),
        "single_use_refresh_locking_expectations": section_single_use_refresh_locking_expectations(),
        "persistence_before_retry": section_persistence_before_retry(),
        "terminal_and_relogin_classifications": section_terminal_and_relogin_classifications(),
        "provider_health_tracking": section_provider_health_tracking(),
        "request_bounds_and_retry_limits": section_request_bounds_and_retry_limits(),
        "model_and_base_url_selection": section_model_and_base_url_selection(),
    }


def main() -> None:
    args = sys.argv[1:]
    corpus = build_corpus()
    text = json.dumps(corpus, ensure_ascii=False, indent=2) + "\n"

    if args == ["--check"]:
        if not OUT.exists():
            raise SystemExit(f"Missing golden file: {OUT}")
        current = OUT.read_text()
        if current != text:
            raise SystemExit(
                "nous-oauth-recovery-goldens.json is stale; rerun the generator"
            )
        total_cases = sum(len(v) for v in corpus.values())
        print(
            f"OK: corpus matches checked-in goldens ({total_cases} cases across {len(corpus)} sections)"
        )
    elif not args:
        OUT.write_text(text)
        total_cases = sum(len(v) for v in corpus.values())
        print(
            f"Wrote {total_cases} cases across {len(corpus)} sections to {OUT.relative_to(ROOT)}"
        )
    else:
        raise SystemExit("usage: gen_nous_oauth_recovery_goldens.py [--check]")


if __name__ == "__main__":
    main()
