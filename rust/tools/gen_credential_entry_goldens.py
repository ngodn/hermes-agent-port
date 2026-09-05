#!/usr/bin/env python3
"""Execute the real pool dataclass and borrowed-credential disk sanitizer."""
import ast
import dataclasses
import importlib.util
import itertools
import json
import sys
import types
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, Optional, List, Set, Tuple

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("credential_disk_oracle", ROOT / "agent/credential_persistence.py")
disk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(disk)
tree = ast.parse((ROOT / "agent/credential_pool.py").read_text())
module = types.ModuleType("credential_entry_oracle")
sys.modules[module.__name__] = module
ns = module.__dict__
ns.update(Any=Any, Dict=Dict, Optional=Optional, List=List, Set=Set, Tuple=Tuple, datetime=datetime,
          is_borrowed_credential_source=disk.is_borrowed_credential_source,
          dataclass=dataclasses.dataclass, fields=dataclasses.fields, replace=dataclasses.replace,
          fingerprint_secret_value=disk.fingerprint_secret_value,
          uuid=types.SimpleNamespace(uuid4=lambda: types.SimpleNamespace(hex="a1b2c3d4")),
          sanitize_borrowed_credential_payload=disk.sanitize_borrowed_credential_payload,
          auth_mod=types.SimpleNamespace(_nous_invoke_jwt_is_usable=lambda token, **kw: token.strip().startswith("valid")))
nodes = []
for node in tree.body:
    if isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id in {"_EXTRA_KEYS", "AUTH_TYPE_API_KEY", "AUTH_TYPE_OAUTH", "SOURCE_MANUAL", "CUSTOM_POOL_PREFIX"} for t in node.targets):
        nodes.append(node)
    elif isinstance(node, (ast.FunctionDef, ast.ClassDef)) and node.name in {"_normalize_pool_auth_type", "_parse_absolute_timestamp", "PooledCredential", "_upsert_entry", "_next_priority", "_is_manual_source", "_prune_stale_seeded_entries", "_normalize_pool_priorities", "_seed_from_env", "get_env_prefer_dotenv", "_normalize_custom_pool_name", "_custom_entry_name_aliases", "_requested_custom_name_aliases", "_pool_keys_for_custom_entry", "custom_provider_pool_key_candidates", "_get_custom_provider_config", "_seed_custom_pool"}:
        nodes.append(node)
exec(compile(ast.Module(body=nodes, type_ignores=[]), "credential-entry", "exec"), ns)
providers = ["openai-api", "custom:openai-api", "anthropic", "nous", "minimax-oauth", "openai-codex", "xai-oauth"]
sources = [None, "", "manual", " Manual:secondary ", "env:OPENAI_API_KEY", "device_code", "hermes_pkce", "oauth", "future-secret-manager"]
variants = [{}, {"access_token":" access-fixture "}, {"access_token":"sk-ant-oat-fixture", "auth_type":"api_key"},
            {"access_token":False,"auth_type":False,"label":None,"priority":None},
            {"access_token":123,"auth_type":7},
            {"access_token":"valid-access", "agent_key":" invalid-agent ", "scope":"invoke", "expires_at":"2030-01-01T00:00:00Z"},
            {"access_token":"valid-access", "agent_key":" valid-agent ", "scope":"invoke"},
            {"last_status":"exhausted", "last_status_at":"2026-09-06T12:34:56Z", "last_error_code":403, "failure_reason":"billing"},
            {"last_status_at":"invalid", "extra":{"ignored":"field"}, "unknown":"ignored", "tls":False, "request_count":3},
            {"base_url":"https://base.example/v1","inference_base_url":"https://inference.example/v1"},
            {"base_url":"https://base.example/v1","inference_base_url":""}]
rows = []
for provider, source, variant in itertools.product(providers, sources, variants):
    payload = dict(variant, source=source)
    entry = ns["PooledCredential"].from_dict(provider, payload)
    rows.append(dict(provider=provider, payload=payload, result=entry.to_dict(), runtime_key=entry.runtime_api_key, runtime_base_url=entry.runtime_base_url))
for payload in [{}, {"id":None}, {"source":None,"id":"saved"}, {"last_status_at":0}, {"last_status_at":"0"}]:
    entry = ns["PooledCredential"].from_dict("openai-api", payload)
    rows.append(dict(provider="openai-api", payload=payload, result=entry.to_dict(), runtime_key=entry.runtime_api_key, runtime_base_url=entry.runtime_base_url))

disk_rows = []
secret_keys = ["accessToken", "refresh-token", "API.Key", "other_api_key", "credential", "credentials", "Authorization", "sessionToken", "password", "tokens", "secret_fingerprint", "token_type", "agent_key_id", "scope", "client_id", "last_error_message"]
for provider, source in itertools.product(providers, sources):
    payload = dict(source=source, label="fixture-label", access_token="fixture-access", refresh_token="fixture-refresh", agent_key="fixture-agent", last_status="exhausted", request_count=3)
    disk_rows.append(dict(provider=provider, payload=payload, result=disk.sanitize_borrowed_credential_payload(payload, provider)))
for key, value in itertools.product(secret_keys, [None, "", 0, False, " fixture-secret ", {"nested":"fixture"}]):
    payload = dict(source="external", secret_fingerprint="sha256:existing")
    payload[key] = value
    disk_rows.append(dict(provider="openai-api", payload=payload, result=disk.sanitize_borrowed_credential_payload(payload, "openai-api")))
upserts = []
for source, status, stored, incoming, duplicate in itertools.product(
    ["manual", "env:OPENAI_API_KEY", "claude_code"], [None,"ok","exhausted","dead"],
    ["same-key", "", "other-key"], [None,"same-key","new-key"], [False,True],
):
    payloads = [dict(id="kept", source=source, label="kept-label", priority=3, access_token=stored,
                     secret_fingerprint=disk.fingerprint_secret_value("same-key"),
                     last_status=status, last_status_at=1000.0, last_error_code=429,
                     last_error_reason="quota", last_error_message="limited", last_error_reset_at=2000.0)]
    if duplicate:
        payloads.append(dict(id="duplicate", source=source, access_token="duplicate-key"))
    payloads.append(dict(id="unrelated",source="manual:other",access_token="other-key",priority=9))
    payload = dict(source=source, access_token=incoming, label="new-label", id="ignored-id",priority=0)
    entries = [ns["PooledCredential"].from_dict("openai-api", p) for p in payloads]
    changed = ns["_upsert_entry"](entries, "openai-api", source, payload)
    upserts.append(dict(entries=payloads, provider="openai-api", source=source, payload=payload,
                        changed=changed, result=[e.to_dict() for e in entries], runtime=[e.runtime_api_key for e in entries]))
for incoming in [dict(id="new-id",source="new-source",access_token="new-key"),
                 dict(source="manual",extra={"nested":"kept","scope":"old"}),
                 dict(source="manual",extra={"nested":"discarded"},scope="new"),
                 dict(source="manual",access_token="sk-ant-oat-new",auth_type="api_key")]:
    payloads = [dict(id="kept",source="manual",access_token="old-key",priority=2)]
    entries = [ns["PooledCredential"].from_dict("anthropic", p) for p in payloads]
    payload = dict(incoming)
    original_payload = dict(payload)
    changed = ns["_upsert_entry"](entries,"anthropic",payload["source"],payload)
    upserts.append(dict(entries=payloads,provider="anthropic",source=payload["source"],payload=original_payload, updated_payload=payload,
                        changed=changed,result=[e.to_dict() for e in entries],runtime=[e.runtime_api_key for e in entries]))

maintenance = []
for provider, source, active, prune_env in itertools.product(providers, [s for s in sources if s is not None] + ["claude_code","env:ANTHROPIC_TOKEN","ENV:KEY"], [False,True], [False,True]):
    payloads = [dict(id="manual",source="manual:keep",access_token="manual-key"),
                dict(id="target",source=source,access_token="target-key",last_status="dead"),
                dict(id="active",source="external:active",access_token="active-key")]
    active_sources = ["external:active"] + ([source] if active else [])
    entries = [ns["PooledCredential"].from_dict(provider, p) for p in payloads]
    changed = ns["_prune_stale_seeded_entries"](entries,set(active_sources),prune_env_sources=prune_env)
    maintenance.append(dict(kind="prune",provider=provider,entries=payloads,active=active_sources,prune_env=prune_env,changed=changed,result=[e.to_dict() for e in entries]))
priority_sources = ["env:ANTHROPIC_API_KEY", "manual:second", "claude_code", "hermes_pkce", "env:CLAUDE_CODE_OAUTH_TOKEN", "manual", "env:ANTHROPIC_TOKEN", "unknown", "unknown"]
for provider, rotation, reverse, duplicate in itertools.product(["anthropic","openai-api"], range(len(priority_sources)), [False,True], [False,True]):
    order = priority_sources[rotation:] + priority_sources[:rotation]
    if reverse:
        order = list(reversed(order))
    payloads = [dict(id="duplicate" if duplicate and i in (0,3) else str(i),source=source,label=str(9-i),priority=(i*3)%5,access_token="fixture") for i,source in enumerate(order)]
    entries = [ns["PooledCredential"].from_dict(provider, p) for p in payloads]
    changed = ns["_normalize_pool_priorities"](provider,entries)
    maintenance.append(dict(kind="priority",provider=provider,entries=payloads,changed=changed,result=[e.to_dict() for e in entries]))

prefer = ns["get_env_prefer_dotenv"]
auth_stub = types.ModuleType("hermes_cli.auth")
env_stub = types.ModuleType("hermes_cli.env_loader")
sys.modules[auth_stub.__name__] = auth_stub
sys.modules[env_stub.__name__] = env_stub
seed_rows = []
for provider, auth_type, have_keys, suppressed, override in itertools.product(
    ["openai-api", "anthropic", "openrouter", "copilot", "kimi-coding", "zai", "missing"],
    ["api_key", "oauth"], [False,True], [False,True], ["", "https://override.example/v1///"],
):
    config = dict(auth_type=auth_type,base_url="https://default.example/v1",base_url_env="BASE_URL",key_vars=["FIRST_KEY","SECOND_KEY"])
    ns["PROVIDER_REGISTRY"] = {} if provider == "missing" else {provider:types.SimpleNamespace(auth_type=auth_type,inference_base_url=config["base_url"],base_url_env_var="BASE_URL",api_key_env_vars=config["key_vars"])}
    ns["OPENROUTER_BASE_URL"] = "https://openrouter.ai/api/v1"
    calls = []
    def get(key):
        calls.append(["get",key])
        return override if key == "BASE_URL" else "fixture-key" if have_keys else ""
    def is_suppressed(provider, source):
        calls.append(["suppressed",provider,source]); return suppressed
    def provenance(key):
        calls.append(["provenance",key]); return " fixture-source "
    def url(key, default, override_url):
        calls.append(["url",provider,key,default,override_url]); return override_url or "https://resolved.example/v1"
    auth_stub.is_source_suppressed = is_suppressed
    env_stub.get_secret_source = provenance
    ns.update(get_env_prefer_dotenv=get,_resolve_kimi_base_url=url,_resolve_zai_base_url=url,_warn_env_ingestion_once=lambda *args:None)
    entries = []
    changed, active = ns["_seed_from_env"](provider,entries)
    serialized = []
    for entry in entries:
        item = entry.to_dict(); item.pop("id",None); serialized.append(item)
    seed_rows.append(dict(provider=provider,config=None if provider=="missing" else config,have_keys=have_keys,suppressed=suppressed,override=override,changed=changed,active=sorted(active),result=serialized,calls=calls))

helper_rows=[]
for file_value, scoped_value in itertools.product([""," file-key ","op://Vault/key"," OP://Vault/key ","\x1c"],[""," scope-key ","\x1c"]):
    ns["load_env"]=lambda:{"KEY":file_value}
    ns["_get_secret"]=lambda *args:scoped_value
    helper_rows.append(dict(kind="prefer",file=file_value,scoped=scoped_value,result=prefer("KEY")))
auth_tree=ast.parse((ROOT/"hermes_cli/auth.py").read_text())
suppression=next(node for node in auth_tree.body if isinstance(node,ast.FunctionDef) and node.name=="is_source_suppressed")
suppression_ns={}
exec(compile(ast.Module(body=[suppression],type_ignores=[]),"suppression","exec"),suppression_ns)
for marker, source in itertools.product([None,[],{},False,"env:KEY",["env:KEY"],{"env:KEY":False},["other",7]],["env:KEY","KEY","missing",""]):
    store={"suppressed_sources":{"openai-api":marker}}
    suppression_ns["_load_auth_store"]=lambda:store
    helper_rows.append(dict(kind="suppression",store=store,source=source,result=suppression_ns["is_source_suppressed"]("openai-api",source)))

custom_rows=[]
custom=[dict(name="One Test",provider_key="one-slug",base_url="https://shared.example/v1/",api_key=" first-key "),dict(name="Two Test",provider_key="two-slug",base_url="https://shared.example/v1",api_key="second-key")]
ns["_iter_custom_providers"]=lambda:iter((ns["_normalize_custom_pool_name"](entry["name"]),entry) for entry in custom)
for base,name in itertools.product(["","https://shared.example/v1///","https://other.example/v1"],[None,"one-slug","Two Test","custom:two-test"," CUSTOM:ONE-TEST ","missing"]):
    custom_rows.append(dict(kind="candidates",custom=custom,base=base,name=name,result=ns["custom_provider_pool_key_candidates"](base,name)))
for pool, provider, key, suppressed in itertools.product(["custom:one-test","one-slug","custom:two-test","two-slug","custom:missing"],["custom","openai"],["model-key","",None],[False,True]):
    config={"model":{"provider":provider,"base_url":" https://shared.example/v1/// ","api_key":key,"api":"fallback-key"}}
    ns["_load_config_safe"]=lambda:config
    calls=[]
    def suppress(pool,source):
        calls.append([pool,source]); return suppressed
    auth_stub.is_source_suppressed=suppress
    entries=[]
    changed,active=ns["_seed_custom_pool"](pool,entries)
    result=[]
    for entry in entries:
        item=entry.to_dict(); item.pop("id",None); result.append(item)
    custom_rows.append(dict(kind="seed",custom=custom,pool=pool,config=config,suppressed=suppressed,changed=changed,active=sorted(active),calls=calls,result=result))

for name, output in [("credential-entry-goldens.json", rows), ("credential-persistence-goldens.json", disk_rows), ("credential-upsert-goldens.json", upserts), ("credential-maintenance-goldens.json", maintenance), ("credential-env-seed-goldens.json", seed_rows), ("credential-source-helper-goldens.json",helper_rows), ("credential-custom-seed-goldens.json",custom_rows)]:
    path = ROOT / "rust/tools" / name
    text = json.dumps(output, indent=2, sort_keys=True) + "\n"
    if sys.argv[1:] == ["--check"]:
        assert path.read_text() == text
    elif not sys.argv[1:]:
        path.write_text(text)
    else:
        raise SystemExit("usage: gen_credential_entry_goldens.py [--check]")
    print(f"Verified {len(output)} cases in {name}")
