//! Port of load_gateway_config and _validate_gateway_config from gateway/config.py.
//!
// Public API is ahead of its callers while the gateway config pipeline is ported.
#![allow(dead_code)]
//!
//! This is the impure top of the gateway config pipeline: it reads the two
//! on-disk sources, flattens them into a single `gw_data` map, and hands that to
//! [`GatewayConfig::from_dict`]. Everything it merges is documented below.
//!
//! Sources, lowest priority first:
//!   1. `<hermes_home>/gateway.json` - the legacy base layer, whole file.
//!   2. `<hermes_home>/config.yaml` - wins over gateway.json key by key.
//!   3. Environment variables - applied AFTER `from_dict`, by
//!      `crate::config_env_overrides::apply_env_overrides` (ported separately;
//!      see the marked call site in [`load_gateway_config_from`]).
//!
//! Precedence contract for config.yaml, reproduced exactly: for most settings a
//! key present at the TOP LEVEL wins, and the nested `gateway.<key>` form is
//! consulted only when the top-level key is ABSENT (not merely falsy or
//! mistyped). Python spells that as a mix of `in` tests and `is None` tests and
//! the two are NOT interchangeable, so each key below is ported with the exact
//! test the Python uses. The ones that differ from the plain `in` majority:
//!   - `quick_commands` and `profile_routes` use `is None`, so an explicit YAML
//!     null at the top level falls through to the nested form.
//!   - `streaming` uses `not isinstance(..., dict)`, so a mistyped top-level
//!     value also falls through to the nested form.
//!   - `session_reset` reads the top-level first and only re-reads the nested
//!     form when the top-level key is absent, then requires a non-empty dict.
//!   - `multiplex_profiles` has NO `elif`; the nested form is applied later and
//!     only when the key is still missing from `gw_data`, which means a value
//!     already supplied by gateway.json beats `gateway.multiplex_profiles`.
//!   - `max_concurrent_sessions` is written from the nested block first and then
//!     unconditionally overwritten by a present top-level key.
//!
//! Two Python dependencies are NOT ported; both are handled the way Python's
//! own fail-open path handles them:
//!   - `hermes_cli.managed_scope.apply_managed_overlay(yaml_cfg)`: administrator
//!     -pinned overlay values. Not ported, so the overlay is skipped entirely
//!     and treated as the identity function. A managed `session_reset` /
//!     `quick_commands` / `stt` / `model` therefore does not reach the gateway
//!     from this loader yet.
//!   - `hermes_cli.plugins.discover_plugins()` + `gateway.platform_registry`:
//!     Python wraps both in try/except and sets `_pr = None` on failure. That is
//!     the branch ported here. Consequences: the shared-key bridging loop
//!     iterates the built-in `Platform` members only (plugin platforms are not
//!     discovered), and the `apply_yaml_config_fn` dispatch that follows it is
//!     skipped wholesale.
//!
//! Smaller divergences, all noted at their site: `has_usable_secret` is inlined
//! (hermes_cli.auth is not ported), YAML mappings with non-string keys cannot be
//! represented in `serde_json::Value` so `str(k)` normalization is a no-op here,
//! and a few Python `TypeError` paths that the outer `except Exception` would
//! swallow are reproduced as an `Err` that triggers the same warning.

use std::path::Path;

use serde_json::{json, Map, Value};
use tracing::{debug, error, info, warn};

use crate::config_file::hermes_home;
use crate::config_gateway::GatewayConfig;
use crate::config_schema::{
    ensure_platform_extra_dict, normalize_notice_delivery, normalize_unauthorized_dm_behavior,
    Platform,
};

// --- PLATFORM_TOKEN_ENV_NAMES -----------------------------------------------

/// Canonical map of platforms whose primary credential is `PlatformConfig.token`
/// and the env var it loads from (`PLATFORM_TOKEN_ENV_NAMES`). Platforms absent
/// from this map authenticate some other way and must never be flagged for a
/// missing token. Lives here because `gateway/config.py` defines it beside the
/// validator and no already-ported module owns it.
pub const PLATFORM_TOKEN_ENV_NAMES: &[(Platform, &str)] = &[
    (Platform::Telegram, "TELEGRAM_BOT_TOKEN"),
    (Platform::Discord, "DISCORD_BOT_TOKEN"),
    (Platform::Slack, "SLACK_BOT_TOKEN"),
    (Platform::Mattermost, "MATTERMOST_TOKEN"),
    (Platform::Matrix, "MATRIX_ACCESS_TOKEN"),
    (Platform::Weixin, "WEIXIN_TOKEN"),
];

fn token_env_name(platform: Platform) -> Option<&'static str> {
    PLATFORM_TOKEN_ENV_NAMES
        .iter()
        .find(|(p, _)| *p == platform)
        .map(|(_, name)| *name)
}

// --- Python value helpers ---------------------------------------------------

/// Python truthiness for the value shapes a config can hold.
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `str(value)` for the shapes we bridge into env vars.
fn py_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            // Python renders an int bare and a float with a decimal point;
            // serde_json keeps the two apart the same way.
            if n.is_f64() {
                let f = n.as_f64().unwrap_or_default();
                if f.fract() == 0.0 && f.is_finite() {
                    format!("{f:.1}")
                } else {
                    format!("{f}")
                }
            } else {
                n.to_string()
            }
        }
        other => other.to_string(),
    }
}

/// Python's `key in container`. A mapping tests keys, a string tests substrings,
/// a sequence tests membership; anything else raises TypeError in Python, which
/// the caller turns into the same "failed to process config.yaml" warning.
fn py_contains(container: &Value, key: &str) -> Result<bool, String> {
    match container {
        Value::Object(m) => Ok(m.contains_key(key)),
        Value::String(s) => Ok(s.contains(key)),
        Value::Array(a) => Ok(a.iter().any(|v| v.as_str() == Some(key))),
        other => Err(format!(
            "argument of type '{}' is not iterable",
            py_type_name(other)
        )),
    }
}

fn py_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) => {
            if n.is_f64() {
                "float"
            } else {
                "int"
            }
        }
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `value` when it is a mapping, else None (Python's `isinstance(x, dict)` guard).
fn as_obj(value: Option<&Value>) -> Option<&Map<String, Value>> {
    match value {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// `dict(**value)` for the extra-dict merges: a mapping spreads, anything else
/// raises TypeError in Python. `None`/absent is handled by the caller's default.
fn spread(value: Option<&Value>) -> Result<Map<String, Value>, String> {
    match value {
        None => Ok(Map::new()),
        Some(Value::Object(m)) => Ok(m.clone()),
        Some(other) => Err(format!(
            "argument of type '{}' is not a mapping",
            py_type_name(other)
        )),
    }
}

// --- load_gateway_config ----------------------------------------------------

/// Port of `load_gateway_config()`: the full pipeline (file layers, then the
/// environment overrides, then validation) under the real Hermes home.
pub fn load_gateway_config() -> GatewayConfig {
    load_gateway_config_from(&hermes_home())
}

/// The full `load_gateway_config()` pipeline with the home directory injected.
///
/// Mirrors Python's ordering exactly: build the config from the file layers,
/// apply the environment overrides, then validate/sanitize.
pub fn load_gateway_config_from(home: &Path) -> GatewayConfig {
    let mut config = load_config_layers_from(home);
    crate::config_env_overrides::apply_env_overrides(&mut config);
    validate_gateway_config(&mut config);
    config
}

/// The FILE layers only (gateway.json then config.yaml), without the env
/// overrides or validation.
///
/// This is the seam the loader's own golden tests target: their expected values
/// were captured from Python with `_apply_env_overrides` stubbed to a no-op, so
/// they describe the file-merge contract alone. The full pipeline is
/// [`load_gateway_config_from`].
pub fn load_config_layers_from(home: &Path) -> GatewayConfig {
    let mut gw_data: Map<String, Value> = Map::new();

    // Legacy fallback: gateway.json provides the base layer.
    // config.yaml keys always win when both specify the same setting.
    let gateway_json_path = home.join("gateway.json");
    if gateway_json_path.exists() {
        let loaded = std::fs::read_to_string(&gateway_json_path)
            .map_err(|e| e.to_string())
            .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|e| e.to_string()));
        match loaded {
            Ok(value) => {
                // Python: `json.load(f) or {}`. A falsy document becomes {}. A
                // truthy non-mapping would be assigned and then blow up on the
                // first dict operation inside the same try block, so collapsing
                // it to {} here reaches the same end state without the detour.
                gw_data = match value {
                    Value::Object(m) => m,
                    _ => Map::new(),
                };
                info!(
                    path = %gateway_json_path.display(),
                    "Loaded legacy gateway.json; consider moving settings to config.yaml"
                );
            }
            Err(e) => {
                warn!(path = %gateway_json_path.display(), error = %e, "Failed to load gateway.json");
            }
        }
    }

    // Primary source: config.yaml. Everything from here to the end of
    // `apply_yaml_config` sits inside one `try/except Exception` in Python, so a
    // failure anywhere warns and keeps whatever was merged so far.
    let config_yaml_path = home.join("config.yaml");
    if config_yaml_path.exists() {
        let outcome = std::fs::read_to_string(&config_yaml_path)
            .map_err(|e| e.to_string())
            .and_then(|text| parse_yaml_mapping(&text))
            .and_then(|yaml_cfg| apply_yaml_config(&mut gw_data, &yaml_cfg));
        if let Err(e) = outcome {
            warn!(
                path = %config_yaml_path.display(),
                error = %e,
                "Failed to process config.yaml; falling back to .env / gateway.json values. \
                 Check the file for syntax errors."
            );
        }
    }

    let config = GatewayConfig::from_dict(&Value::Object(gw_data));

    // The environment overrides and validation that Python applies next are
    // driven by [`load_gateway_config_from`], which composes this function with
    // `config_env_overrides::apply_env_overrides` and `validate_gateway_config`
    // in Python's order.
    config
}

/// `yaml.safe_load(f) or {}`, restricted to a top-level mapping. A falsy
/// document (null, empty string, `[]`, ...) becomes `{}`; a truthy non-mapping
/// is what makes Python's first `.get()` raise AttributeError, which the caller
/// reports as a config.yaml processing failure.
fn parse_yaml_mapping(text: &str) -> Result<Map<String, Value>, String> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let parsed: Value = serde_yaml_ng::from_str(text).map_err(|e| e.to_string())?;
    match parsed {
        Value::Object(m) => Ok(m),
        other if !py_truthy(&other) => Ok(Map::new()),
        other => Err(format!(
            "'{}' object has no attribute 'get'",
            py_type_name(&other)
        )),
    }
}

/// The config.yaml half of `load_gateway_config`. Mutations to `gw_data` are
/// kept on the error path, matching Python where the dict is mutated in place
/// inside the `try` block.
fn apply_yaml_config(
    gw_data: &mut Map<String, Value>,
    yaml_cfg: &Map<String, Value>,
) -> Result<(), String> {
    // Managed scope overlay: `yaml_cfg = managed_scope.apply_managed_overlay(yaml_cfg)`
    // in Python. `hermes_cli.managed_scope` is not ported, so the overlay is the
    // identity here. See the module doc.

    // Shared nested-fallback source: settings meant to be top-level keys are
    // also accepted when a user nests them under `gateway:` (which is what
    // `hermes config set gateway.<key> ...` naturally produces).
    let gateway_section = yaml_cfg.get("gateway");
    let gsec = as_obj(gateway_section);

    // session_reset -> default_reset_policy. `in` test, then truthy-and-dict.
    let mut sr: Option<&Value> = yaml_cfg.get("session_reset");
    if !yaml_cfg.contains_key("session_reset") {
        if let Some(g) = gsec {
            sr = g.get("session_reset");
        }
    }
    if let Some(v) = sr {
        if py_truthy(v) && v.is_object() {
            gw_data.insert("default_reset_policy".to_string(), v.clone());
        }
    }

    // quick_commands. NOTE: `is None`, not `in`. An explicit top-level null
    // falls through to the nested form.
    let mut qc: Option<&Value> = yaml_cfg.get("quick_commands").filter(|v| !v.is_null());
    if qc.is_none() {
        if let Some(g) = gsec {
            qc = g.get("quick_commands").filter(|v| !v.is_null());
        }
    }
    if let Some(v) = qc {
        if v.is_object() {
            gw_data.insert("quick_commands".to_string(), v.clone());
        } else {
            warn!(
                got = py_type_name(v),
                "Ignoring invalid quick_commands in config.yaml (expected mapping)"
            );
        }
    }

    // stt block. `in` test on the top level, then dict-only.
    let mut stt_cfg: Option<&Value> = yaml_cfg.get("stt");
    if !yaml_cfg.contains_key("stt") {
        if let Some(g) = gsec {
            stt_cfg = g.get("stt");
        }
    }
    if let Some(v) = stt_cfg {
        if v.is_object() {
            gw_data.insert("stt".to_string(), v.clone());
        }
    }
    bridge_key(gw_data, yaml_cfg, gsec, "stt_echo_transcripts");

    // `gateway_cfg` is a second read of the same key; kept as its own binding to
    // mirror Python, where it is what the platform merges below consult.
    let gateway_cfg = yaml_cfg.get("gateway");

    bridge_key(gw_data, yaml_cfg, gsec, "group_sessions_per_user");
    bridge_key(gw_data, yaml_cfg, gsec, "thread_sessions_per_user");

    // multiplex_profiles: top-level ONLY here. There is deliberately no `elif`;
    // the nested form is applied further down and only if the key is still
    // absent from gw_data, so a gateway.json value beats gateway.multiplex_profiles.
    if let Some(v) = yaml_cfg.get("multiplex_profiles") {
        gw_data.insert("multiplex_profiles".to_string(), v.clone());
    }

    bridge_key(gw_data, yaml_cfg, gsec, "multiplex_profile_allowlist");
    bridge_key(gw_data, yaml_cfg, gsec, "room_link_url");

    // profile_routes. NOTE: `is None`, not `in`. List-only.
    let mut pr_routes: Option<&Value> = yaml_cfg.get("profile_routes").filter(|v| !v.is_null());
    if pr_routes.is_none() {
        if let Some(g) = gsec {
            pr_routes = g.get("profile_routes").filter(|v| !v.is_null());
        }
    }
    if let Some(v) = pr_routes {
        if v.is_array() {
            gw_data.insert("profile_routes".to_string(), v.clone());
        }
    }

    if let Some(g) = gsec {
        if g.contains_key("multiplex_profiles") && !gw_data.contains_key("multiplex_profiles") {
            // gateway.multiplex_profiles, written by
            // `hermes config set gateway.multiplex_profiles true`.
            gw_data.insert(
                "multiplex_profiles".to_string(),
                g["multiplex_profiles"].clone(),
            );
        }
        if g.contains_key("max_concurrent_sessions") {
            gw_data.insert(
                "max_concurrent_sessions".to_string(),
                g["max_concurrent_sessions"].clone(),
            );
        }
        if g.contains_key("systemd_watchdog_seconds") {
            gw_data.insert(
                "systemd_watchdog_seconds".to_string(),
                g["systemd_watchdog_seconds"].clone(),
            );
        }
    }

    // Top-level max_concurrent_sessions overwrites whatever the nested block set.
    if let Some(v) = yaml_cfg.get("max_concurrent_sessions") {
        gw_data.insert("max_concurrent_sessions".to_string(), v.clone());
    }

    // streaming. NOTE: the guard is `not isinstance(dict)`, not a presence test,
    // so a mistyped top-level `streaming:` also falls back to gateway.streaming.
    let mut streaming_cfg: Option<&Value> = yaml_cfg.get("streaming");
    if !matches!(streaming_cfg, Some(Value::Object(_))) {
        if let Some(g) = gsec {
            streaming_cfg = g.get("streaming");
        }
    }
    if let Some(Value::Object(_)) = streaming_cfg {
        gw_data.insert("streaming".to_string(), streaming_cfg.unwrap().clone());
    }

    bridge_key(gw_data, yaml_cfg, gsec, "reset_triggers");
    bridge_key(gw_data, yaml_cfg, gsec, "always_log_local");
    // write_sessions_json: top-level wins, nested gateway.* fallback (matches
    // the gateway.streaming precedence pattern).
    bridge_key(gw_data, yaml_cfg, gsec, "write_sessions_json");

    // Loop-liveness watchdog toggle plus tuning knobs. GatewayConfig::from_dict
    // has its own nested fallback, but this loader builds gw_data FLAT and never
    // forwards the yaml `gateway:` section, so without this bridge the
    // documented keys are silently ignored on the real startup path.
    for wd_key in [
        "loop_watchdog",
        "loop_watchdog_probe_interval_s",
        "loop_watchdog_probe_timeout_s",
        "loop_watchdog_max_strikes",
    ] {
        bridge_key(gw_data, yaml_cfg, gsec, wd_key);
    }

    bridge_key(gw_data, yaml_cfg, gsec, "filter_silence_narration");

    if yaml_cfg.contains_key("unauthorized_dm_behavior") {
        let normalized =
            normalize_unauthorized_dm_behavior(&yaml_cfg["unauthorized_dm_behavior"], "pair");
        gw_data.insert("unauthorized_dm_behavior".to_string(), json!(normalized));
    } else if let Some(g) = gsec {
        if g.contains_key("unauthorized_dm_behavior") {
            let normalized =
                normalize_unauthorized_dm_behavior(&g["unauthorized_dm_behavior"], "pair");
            gw_data.insert("unauthorized_dm_behavior".to_string(), json!(normalized));
        }
    }

    // ---- platforms ----------------------------------------------------------
    //
    // Python does `platforms_data = gw_data.setdefault("platforms", {})` and
    // mutates that dict in place for the rest of the function. Here it is lifted
    // into a local and written back unconditionally (including on the error
    // path) so the in-place aliasing is preserved.
    let mut platforms_data: Map<String, Value> = match gw_data.get("platforms") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let result = apply_yaml_platforms(gw_data, &mut platforms_data, yaml_cfg, gateway_cfg);
    gw_data.insert("platforms".to_string(), Value::Object(platforms_data));
    result
}

/// `if key in yaml_cfg: gw_data[key] = yaml_cfg[key]` /
/// `elif key in gateway_section: gw_data[key] = gateway_section[key]`. The plain
/// presence-based bridge that most keys use.
fn bridge_key(
    gw_data: &mut Map<String, Value>,
    yaml_cfg: &Map<String, Value>,
    gsec: Option<&Map<String, Value>>,
    key: &str,
) {
    if let Some(v) = yaml_cfg.get(key) {
        gw_data.insert(key.to_string(), v.clone());
    } else if let Some(g) = gsec {
        if let Some(v) = g.get(key) {
            gw_data.insert(key.to_string(), v.clone());
        }
    }
}

/// Port of `_merge_platform_map`. Deep-merges one `{platform: block}` map into
/// `platforms_data`: the block wins key by key, `extra` dicts are deep-merged so
/// gateway.json defaults survive, and a block that carries `enabled` at all
/// leaves the `_enabled_explicit` marker behind in `extra`.
fn merge_platform_map(
    platforms_data: &mut Map<String, Value>,
    source_platforms: Option<&Value>,
) -> Result<(), String> {
    let source = match as_obj(source_platforms) {
        Some(m) => m,
        None => return Ok(()),
    };
    for (plat_name, plat_block) in source {
        let block = match plat_block.as_object() {
            Some(b) => b,
            None => continue,
        };
        let existing: Map<String, Value> = match platforms_data.get(plat_name) {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        // Deep-merge extra dicts so gateway.json defaults survive.
        let mut merged_extra = spread(existing.get("extra"))?;
        for (k, v) in spread(block.get("extra"))? {
            merged_extra.insert(k, v);
        }
        if block.contains_key("enabled") {
            merged_extra.insert("_enabled_explicit".to_string(), json!(true));
        }
        let mut merged = existing;
        for (k, v) in block {
            merged.insert(k.clone(), v.clone());
        }
        if !merged_extra.is_empty() {
            merged.insert("extra".to_string(), Value::Object(merged_extra));
        }
        platforms_data.insert(plat_name.clone(), Value::Object(merged));
    }
    Ok(())
}

fn apply_yaml_platforms(
    gw_data: &Map<String, Value>,
    platforms_data: &mut Map<String, Value>,
    yaml_cfg: &Map<String, Value>,
    gateway_cfg: Option<&Value>,
) -> Result<(), String> {
    // Runtime-only settings under `gateway.platforms` load the same way as
    // top-level `platforms`. Merge nested FIRST so top-level keeps precedence,
    // matching the gateway.streaming fallback.
    let gateway_platforms: Option<&Value> = as_obj(gateway_cfg).and_then(|g| g.get("platforms"));
    merge_platform_map(platforms_data, gateway_platforms)?;
    merge_platform_map(platforms_data, yaml_cfg.get("platforms"))?;

    // Also merge platform configs placed directly under `gateway.*` (e.g.
    // `gateway.api_server`) so subsections are discovered the same way
    // `gateway.streaming` is. Iterate all `gateway:*` keys and merge only those
    // that name a known platform, skipping the reserved `platforms` key.
    if let Some(g) = as_obj(gateway_cfg) {
        let mut nested_platforms: Map<String, Value> = Map::new();
        for (k, v) in g {
            if k == "platforms" {
                continue;
            }
            // Python: `Platform(_k)` inside try/except ValueError|AttributeError.
            // Plugin pseudo-members are not ported (see config_schema), so only
            // built-in platform values match here.
            if Platform::from_value(k).is_none() {
                continue;
            }
            if v.is_object() {
                nested_platforms.insert(k.clone(), v.clone());
            }
        }
        if !nested_platforms.is_empty() {
            merge_platform_map(platforms_data, Some(&Value::Object(nested_platforms)))?;
        }
    }

    // Bridge api_server-specific keys into `extra` so PlatformConfig::from_dict
    // preserves them. Users writing `gateway.api_server.port: 8642` expect these
    // to land in the platform's extra dict. Note this is a POP: the key is moved
    // out of the platform block, not copied.
    if matches!(platforms_data.get("api_server"), Some(Value::Object(_))) {
        let api_plat = platforms_data
            .get_mut("api_server")
            .and_then(Value::as_object_mut)
            .expect("checked above");
        if !matches!(api_plat.get("extra"), Some(Value::Object(_))) {
            api_plat.insert("extra".to_string(), Value::Object(Map::new()));
        }
        let existing_extra: Map<String, Value> = match api_plat.get("extra") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        let mut moved: Vec<(String, Value)> = Vec::new();
        for bridge_key in ["port", "key", "host", "cors_origins", "model_name"] {
            if api_plat.contains_key(bridge_key) && !existing_extra.contains_key(bridge_key) {
                if let Some(v) = api_plat.remove(bridge_key) {
                    moved.push((bridge_key.to_string(), v));
                }
            }
        }
        if !moved.is_empty() {
            let extra = api_plat
                .get_mut("extra")
                .and_then(Value::as_object_mut)
                .expect("ensured above");
            for (k, v) in moved {
                extra.insert(k, v);
            }
        }
    }

    // Python: `if platforms_data: gw_data["platforms"] = platforms_data`. A
    // no-op here because the caller writes the map back unconditionally, and
    // Python's setdefault already put the same object there.

    // Plugin discovery: Python imports `hermes_cli.plugins.discover_plugins` and
    // `gateway.platform_registry` in a try/except that sets `_pr = None` on
    // failure. Neither is ported, so this is permanently the `_pr = None`
    // branch: the shared-key loop below covers the BUILT-IN platforms only, and
    // the `apply_yaml_config_fn` dispatch that follows it never runs.
    debug!("plugin discovery skipped: hermes_cli.plugins is not ported");
    let shared_loop_targets: &[Platform] = Platform::ALL;

    // gw_data's unauthorized_dm_behavior is the per-platform default. It is only
    // ever written as a normalized string above, so reading it as one is exact.
    let gw_unauthorized_default: String = match gw_data.get("unauthorized_dm_behavior") {
        Some(Value::String(s)) => s.clone(),
        _ => "pair".to_string(),
    };

    for &plat in shared_loop_targets {
        if plat == Platform::Local {
            continue;
        }
        let mut platform_cfg: Option<&Value> = yaml_cfg.get(plat.value());
        let cfg_toplevel = matches!(platform_cfg, Some(Value::Object(_)));

        // Fall back to the platform's block under `platforms` / `gateway.platforms`
        // so shared-key bridging still runs when the user configured the platform
        // only under those nested paths and not via a top-level block.
        //
        // Note: `enabled` is only written from a top-level block (`cfg_toplevel`);
        // for nested-only configs `merge_platform_map` already merged it with the
        // correct precedence, so re-applying it here would overwrite that.
        if !cfg_toplevel {
            for src in [gateway_platforms, yaml_cfg.get("platforms")] {
                if let Some(m) = as_obj(src) {
                    if let Some(candidate) = m.get(plat.value()) {
                        if candidate.is_object() {
                            platform_cfg = Some(candidate);
                            break;
                        }
                    }
                }
            }
        }
        let platform_cfg = match platform_cfg {
            Some(Value::Object(m)) => m,
            _ => continue,
        };

        // Collect bridgeable keys from this platform section.
        let mut bridged: Map<String, Value> = Map::new();

        if platform_cfg.contains_key("unauthorized_dm_behavior") {
            bridged.insert(
                "unauthorized_dm_behavior".to_string(),
                json!(normalize_unauthorized_dm_behavior(
                    &platform_cfg["unauthorized_dm_behavior"],
                    &gw_unauthorized_default,
                )),
            );
        }
        if platform_cfg.contains_key("notice_delivery") {
            bridged.insert(
                "notice_delivery".to_string(),
                json!(normalize_notice_delivery(
                    &platform_cfg["notice_delivery"],
                    "public",
                )),
            );
        }
        for key in [
            "reply_prefix",
            "reply_in_thread",
            "cron_continuable_surface",
            "require_mention",
            "send_read_receipts",
        ] {
            if let Some(v) = platform_cfg.get(key) {
                bridged.insert(key.to_string(), v.clone());
            }
        }
        // Telegram-only keys.
        if plat == Platform::Telegram {
            for key in ["allowed_chats", "group_allowed_chats", "allowed_topics"] {
                if let Some(v) = platform_cfg.get(key) {
                    bridged.insert(key.to_string(), v.clone());
                }
            }
        }
        for key in [
            "free_response_channels",
            "mention_patterns",
            "exclusive_bot_mentions",
        ] {
            if let Some(v) = platform_cfg.get(key) {
                bridged.insert(key.to_string(), v.clone());
            }
        }
        if plat == Platform::Telegram {
            if let Some(v) = platform_cfg.get("observe_unmentioned_group_messages") {
                bridged.insert("observe_unmentioned_group_messages".to_string(), v.clone());
            }
        }
        for key in [
            "dm_policy",
            "allow_from",
            "allow_admin_from",
            "user_allowed_commands",
            "group_policy",
            "group_allow_from",
            "group_allow_admin_from",
            "group_user_allowed_commands",
        ] {
            if let Some(v) = platform_cfg.get(key) {
                bridged.insert(key.to_string(), v.clone());
            }
        }
        // Discord / Slack only.
        if matches!(plat, Platform::Discord | Platform::Slack) {
            if let Some(v) = platform_cfg.get("channel_skill_bindings") {
                bridged.insert("channel_skill_bindings".to_string(), v.clone());
            }
        }
        if let Some(channel_prompts) = platform_cfg.get("channel_prompts") {
            // Python rebuilds this as `{str(k): v}` so non-string YAML keys
            // (e.g. numeric channel ids) become strings. serde_json mappings can
            // only ever hold string keys, so the rebuild is already done for us
            // by the YAML parse and this is a plain copy.
            bridged.insert("channel_prompts".to_string(), channel_prompts.clone());
        }
        for key in [
            "gateway_restart_notification",
            "typing_indicator",
            "typing_status_text",
        ] {
            if let Some(v) = platform_cfg.get(key) {
                bridged.insert(key.to_string(), v.clone());
            }
        }

        // Bridge top-level port/host/secret into extra for platforms whose
        // adapters read these from config.extra. Without this, YAML like
        //   platforms:
        //     webhook:
        //       enabled: true
        //       port: 8649
        // silently falls back to the hardcoded DEFAULT_PORT, because
        // PlatformConfig::from_dict only reads `extra` from the `extra:` sub-key.
        let extra_of_cfg = platform_cfg.get("extra").cloned().unwrap_or(json!({}));
        if matches!(plat, Platform::Webhook | Platform::MsgraphWebhook) {
            for bridge in ["port", "host", "secret"] {
                if platform_cfg.contains_key(bridge) && !py_contains(&extra_of_cfg, bridge)? {
                    bridged.insert(bridge.to_string(), platform_cfg[bridge].clone());
                }
            }
        }
        if plat == Platform::ApiServer {
            for bridge in ["port", "host"] {
                if platform_cfg.contains_key(bridge) && !py_contains(&extra_of_cfg, bridge)? {
                    bridged.insert(bridge.to_string(), platform_cfg[bridge].clone());
                }
            }
        }

        let has_channel_overrides = platform_cfg.contains_key("channel_overrides");
        if has_channel_overrides {
            if let Some(Value::Object(raw_overrides)) = platform_cfg.get("channel_overrides") {
                let mut filtered: Map<String, Value> = Map::new();
                for (cid, ov_data) in raw_overrides {
                    if ov_data.is_object() {
                        filtered.insert(cid.clone(), ov_data.clone());
                    }
                }
                // `_ensure_platform_extra_dict` returns (plat_data, extra); the
                // Rust helper hands back `extra`, so the outer entry is re-read
                // afterwards to set channel_overrides on plat_data.
                let _ = ensure_platform_extra_dict(platforms_data, plat.value());
                let plat_data = platforms_data
                    .get_mut(plat.value())
                    .and_then(Value::as_object_mut)
                    .expect("ensure_platform_extra_dict created an object");
                plat_data.insert("channel_overrides".to_string(), Value::Object(filtered));
            }
        }

        let enabled_was_explicit = cfg_toplevel && platform_cfg.contains_key("enabled");
        if bridged.is_empty() && !enabled_was_explicit && !has_channel_overrides {
            continue;
        }

        let _ = ensure_platform_extra_dict(platforms_data, plat.value());
        if enabled_was_explicit {
            let plat_data = platforms_data
                .get_mut(plat.value())
                .and_then(Value::as_object_mut)
                .expect("ensure_platform_extra_dict created an object");
            plat_data.insert("enabled".to_string(), platform_cfg["enabled"].clone());
        }
        let extra = ensure_platform_extra_dict(platforms_data, plat.value());
        if enabled_was_explicit {
            // Mark the explicit enable/disable so the registry-driven
            // plugin-enable pass in apply_env_overrides honors an explicit
            // `enabled: false` for migrated plugin platforms instead of
            // re-enabling them on token/SDK presence.
            extra.insert("_enabled_explicit".to_string(), json!(true));
        }
        for (k, v) in bridged {
            extra.insert(k, v);
        }
    }

    // Plugin-owned YAML->env config bridges (`PlatformEntry.apply_yaml_config_fn`)
    // run here in Python, guarded by `if _pr is not None`. `_pr` is always None
    // in this port (see above), so the whole dispatch is skipped. The Slack,
    // Telegram, WhatsApp, DingTalk, Mattermost, Matrix and Feishu env bridges all
    // live behind that hook and are therefore not applied yet.

    // Bridge top-level require_mention to Telegram when the `telegram:` section
    // does not already provide one. Users often write "require_mention: true" at
    // the top level alongside group_sessions_per_user and expect it to work the
    // same way.
    if let Some(tl_require_mention) = yaml_cfg.get("require_mention").filter(|v| !v.is_null()) {
        // Python: `yaml_cfg.get("telegram") or {}`, so a falsy value becomes {}.
        let tg_section: Value = match yaml_cfg.get("telegram") {
            Some(v) if py_truthy(v) => v.clone(),
            _ => json!({}),
        };
        if !py_contains(&tg_section, "require_mention")? {
            let tg_plat = platforms_data
                .entry(Platform::Telegram.value().to_string())
                .or_insert_with(|| json!({}));
            let tg_plat = tg_plat.as_object_mut().ok_or_else(|| {
                format!(
                    "'{}' object has no attribute 'setdefault'",
                    py_type_name(&json!(null))
                )
            })?;
            let tg_extra = tg_plat
                .entry("extra".to_string())
                .or_insert_with(|| json!({}));
            let tg_extra = tg_extra
                .as_object_mut()
                .ok_or_else(|| "'extra' is not a mapping".to_string())?;
            tg_extra
                .entry("require_mention".to_string())
                .or_insert_with(|| tl_require_mention.clone());

            // Also bridge to the TELEGRAM_REQUIRE_MENTION env var the adapter
            // reads at runtime. This stays in core because it keys off the
            // TOP-LEVEL require_mention, so the telegram plugin's
            // apply_yaml_config_fn hook (which only runs when a telegram config
            // block exists) cannot cover the no-telegram-block case.
            if std::env::var("TELEGRAM_REQUIRE_MENTION")
                .ok()
                .filter(|s| !s.is_empty())
                .is_none()
            {
                std::env::set_var(
                    "TELEGRAM_REQUIRE_MENTION",
                    py_str(tl_require_mention).to_lowercase(),
                );
            }
        }
    }

    // Signal settings -> env vars (env vars take precedence).
    if let Some(signal_cfg) = as_obj(yaml_cfg.get("signal")) {
        if signal_cfg.contains_key("require_mention")
            && std::env::var("SIGNAL_REQUIRE_MENTION")
                .ok()
                .filter(|s| !s.is_empty())
                .is_none()
        {
            std::env::set_var(
                "SIGNAL_REQUIRE_MENTION",
                py_str(&signal_cfg["require_mention"]).to_lowercase(),
            );
        }
    }

    Ok(())
}

// --- _validate_gateway_config -----------------------------------------------

/// Placeholder secret values from `hermes_cli.auth._PLACEHOLDER_SECRET_VALUES`.
/// Inlined because `hermes_cli.auth` is not ported; Python has an ImportError
/// fallback that skips the whole check, but the import succeeds in practice, so
/// the active branch is what is reproduced here.
const PLACEHOLDER_SECRET_VALUES: &[&str] = &[
    "*",
    "**",
    "***",
    "changeme",
    "your_api_key",
    "your_api_key_here",
    "your-api-key",
    "placeholder",
    "example",
    "dummy",
    "null",
    "none",
];

/// Port of `hermes_cli.auth.has_usable_secret(value, min_length=4)`: True when a
/// configured secret looks usable, not empty and not a known placeholder.
pub fn has_usable_secret(value: &Value, min_length: usize) -> bool {
    let s = match value.as_str() {
        Some(s) => s,
        None => return false,
    };
    let cleaned = s.trim();
    if cleaned.chars().count() < min_length {
        return false;
    }
    !PLACEHOLDER_SECRET_VALUES.contains(&cleaned.to_lowercase().as_str())
}

/// Port of `_validate_gateway_config`. Validates and sanitizes a loaded
/// GatewayConfig IN PLACE (hence `&mut`, where Python relies on object identity):
///   - `default_reset_policy.at_hour` outside 0..=23 is reset to 4 with a warning.
///   - `default_reset_policy.idle_minutes` that is null or <= 0 is reset to 1440
///     with a warning.
///   - an enabled platform whose credential env var is known and whose token is
///     present but blank gets a warning (the adapter will fail to connect).
///   - an enabled platform whose token is a known placeholder gets an ERROR and
///     is force-disabled, so a copied .env.example fails loudly at startup
///     rather than with an opaque auth error later.
pub fn validate_gateway_config(config: &mut GatewayConfig) {
    let policy = &mut config.default_reset_policy;

    // Python compares `0 <= policy.at_hour <= 23` directly; a non-numeric value
    // would raise TypeError out of load_gateway_config. Here a non-numeric value
    // is treated as invalid and reset, which is the fail-safe reading of the
    // same intent.
    let at_hour_ok = match &policy.at_hour {
        Value::Number(n) => n
            .as_f64()
            .map(|f| (0.0..=23.0).contains(&f))
            .unwrap_or(false),
        Value::Bool(b) => (0.0..=23.0).contains(&(if *b { 1.0 } else { 0.0 })),
        _ => false,
    };
    if !at_hour_ok {
        warn!(at_hour = %policy.at_hour, "Invalid at_hour (must be 0-23). Using default 4.");
        policy.at_hour = json!(4);
    }

    let idle_ok = match &policy.idle_minutes {
        Value::Null => false,
        Value::Number(n) => n.as_f64().map(|f| f > 0.0).unwrap_or(false),
        Value::Bool(b) => *b,
        _ => false,
    };
    if !idle_ok {
        warn!(
            idle_minutes = %policy.idle_minutes,
            "Invalid idle_minutes (must be positive). Using default 1440."
        );
        policy.idle_minutes = json!(1440);
    }

    // Warn about empty bot tokens: platforms that loaded an empty string will not
    // connect and the cause is confusing without a log line.
    for (platform, pconfig) in config.platforms.iter() {
        if !pconfig.enabled {
            continue;
        }
        let env_name = match token_env_name(*platform) {
            Some(n) => n,
            None => continue,
        };
        if let Some(token) = &pconfig.token {
            // Python calls `.strip()`, so a non-string token raises AttributeError
            // there; a non-string is simply not an empty token here.
            if let Some(s) = token.as_str() {
                if s.trim().is_empty() {
                    warn!(
                        platform = platform.value(),
                        env = env_name,
                        "Platform is enabled but its token env var is empty. \
                         The adapter will likely fail to connect."
                    );
                }
            }
        }
    }

    // Reject known-weak placeholder tokens: users who copy .env.example without
    // changing placeholder values get a clear startup error instead of a
    // confusing "auth failed" from the platform API.
    let mut to_disable: Vec<Platform> = Vec::new();
    for (platform, pconfig) in config.platforms.iter() {
        if !pconfig.enabled {
            continue;
        }
        let env_name = match token_env_name(*platform) {
            Some(n) => n,
            None => continue,
        };
        let token = match &pconfig.token {
            Some(t) => t,
            None => continue,
        };
        let token_str = match token.as_str() {
            Some(s) => s,
            None => continue,
        };
        if !token_str.trim().is_empty() && !has_usable_secret(token, 4) {
            let trimmed = token_str.trim();
            let preview: String = trimmed.chars().take(6).collect();
            error!(
                platform = platform.value(),
                env = env_name,
                value = %format!("{preview}..."),
                "Platform is enabled but its token env var is set to a placeholder value. \
                 Set a real bot token before starting the gateway. The adapter will NOT be started."
            );
            to_disable.push(*platform);
        }
    }
    for platform in to_disable {
        if let Some(pconfig) = config.platforms.get_mut(&platform) {
            pconfig.enabled = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_types::SessionResetPolicy;
    use std::path::PathBuf;

    // Every test that touches HERMES_HOME or the process environment takes
    // `crate::secret_scope::GLOBAL_TEST_LOCK` exactly once, at the top, and never
    // nests it. `GatewayConfig::from_dict` derives sessions_dir from HERMES_HOME,
    // so even the "pure" loader tests are env-sensitive.
    //
    // Golden expectations below were produced by running the real Python:
    //
    //   env -i PATH=/usr/bin:/bin HERMES_HOME=<fixture> python3 -c "
    //     import sys, json
    //     sys.path.insert(0, '.')
    //     import gateway.config as gc
    //     gc._apply_env_overrides = lambda cfg: None
    //     print(json.dumps(gc.load_gateway_config().to_dict(), sort_keys=True, default=str))"
    //
    // `env -i` keeps the environment clean so nothing leaks in, and
    // `_apply_env_overrides` is stubbed to a no-op because Python runs the env
    // layer inside load_gateway_config while this port stops before it (see the
    // marked call site). Without the stub the env pass strips `_enabled_explicit`
    // and the goldens would not describe the loader alone.

    fn temp_home(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_cfgload_test_{}_{}_{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).expect("create temp home");
        p
    }

    /// Set HERMES_HOME (GatewayConfig::from_dict derives sessions_dir from it),
    /// write the fixture files, load, and hand back the to_dict() Value.
    fn load_fixture(
        name: &str,
        yaml: Option<&str>,
        gateway_json: Option<&str>,
    ) -> (Value, PathBuf) {
        let home = temp_home(name);
        if let Some(y) = yaml {
            std::fs::write(home.join("config.yaml"), y).expect("write config.yaml");
        }
        if let Some(j) = gateway_json {
            std::fs::write(home.join("gateway.json"), j).expect("write gateway.json");
        }
        std::env::set_var("HERMES_HOME", &home);
        std::env::remove_var("TELEGRAM_REQUIRE_MENTION");
        std::env::remove_var("SIGNAL_REQUIRE_MENTION");
        let cfg = load_config_layers_from(&home);
        (cfg.to_dict(), home)
    }

    fn cleanup(home: &PathBuf) {
        let _ = std::fs::remove_dir_all(home);
        std::env::remove_var("HERMES_HOME");
    }

    /// The default to_dict() shape for an empty home, with sessions_dir filled
    /// in for the given home. Every golden below is this map plus overrides.
    fn expected_defaults(home: &Path) -> Map<String, Value> {
        let sessions = home.join("sessions");
        let v = json!({
            "always_log_local": true,
            "default_reset_policy": {
                "at_hour": 4,
                "bg_process_max_age_hours": 24,
                "idle_minutes": 1440,
                "mode": "none",
                "notify": true,
                "notify_exclude_platforms": ["api_server", "webhook"]
            },
            "filter_silence_narration": true,
            "group_sessions_per_user": true,
            "loop_watchdog": true,
            "loop_watchdog_max_strikes": 3,
            "loop_watchdog_probe_interval_s": 30.0,
            "loop_watchdog_probe_timeout_s": 10.0,
            "max_concurrent_sessions": null,
            "multiplex_profile_allowlist": null,
            "multiplex_profiles": false,
            "platforms": {},
            "profile_routes": [],
            "quick_commands": {},
            "reset_by_platform": {},
            "reset_by_type": {},
            "reset_triggers": ["/new", "/reset"],
            "room_link_url": null,
            "session_store_max_age_days": 90,
            "sessions_dir": sessions.to_string_lossy(),
            "streaming": {
                "buffer_threshold": 24,
                "cursor": " \u{2589}",
                "edit_interval": 0.8,
                "enabled": false,
                "fresh_final_after_seconds": 0.0,
                "transport": "auto"
            },
            "stt_echo_transcripts": true,
            "stt_enabled": true,
            "systemd_watchdog_seconds": 0,
            "thread_sessions_per_user": false,
            "unauthorized_dm_behavior": "pair",
            "write_sessions_json": true
        });
        v.as_object().cloned().expect("object")
    }

    fn with(base: Map<String, Value>, overrides: Value) -> Value {
        let mut out = base;
        for (k, v) in overrides.as_object().expect("object") {
            out.insert(k.clone(), v.clone());
        }
        Value::Object(out)
    }

    #[test]
    fn empty_home_yields_defaults() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (got, home) = load_fixture("empty", None, None);
        assert_eq!(got, Value::Object(expected_defaults(&home)));
        cleanup(&home);
    }

    #[test]
    fn gateway_json_only_is_the_base_layer() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let gj = r#"{"group_sessions_per_user": true, "max_concurrent_sessions": 7,
                     "always_log_local": true,
                     "platforms": {"telegram": {"enabled": true, "extra": {"a": 1}}}}"#;
        let (got, home) = load_fixture("gwjson", None, Some(gj));
        let want = with(
            expected_defaults(&home),
            json!({
                "max_concurrent_sessions": 7,
                "platforms": {
                    "telegram": {
                        "enabled": true,
                        "extra": {"a": 1},
                        "gateway_restart_notification": true,
                        "reply_to_mode": "first",
                        "typing_indicator": true
                    }
                }
            }),
        );
        assert_eq!(got, want);
        cleanup(&home);
    }

    #[test]
    fn config_yaml_only_maps_top_level_keys() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let yaml = "\
session_reset:
  mode: daily
  at_hour: 9
quick_commands:
  hi: echo hi
stt:
  enabled: false
group_sessions_per_user: true
multiplex_profiles: true
room_link_url: https://example.test/room
reset_triggers: [\"reset\"]
write_sessions_json: true
loop_watchdog: false
loop_watchdog_max_strikes: 5
filter_silence_narration: true
unauthorized_dm_behavior: ignore
";
        let (got, home) = load_fixture("yamlonly", Some(yaml), None);
        let want = with(
            expected_defaults(&home),
            json!({
                "default_reset_policy": {
                    "at_hour": 9,
                    "bg_process_max_age_hours": 24,
                    "idle_minutes": 1440,
                    "mode": "daily",
                    "notify": true,
                    "notify_exclude_platforms": ["api_server", "webhook"]
                },
                "loop_watchdog": false,
                "loop_watchdog_max_strikes": 5,
                "multiplex_profiles": true,
                "quick_commands": {"hi": "echo hi"},
                "reset_triggers": ["reset"],
                "room_link_url": "https://example.test/room",
                "stt_enabled": false,
                "unauthorized_dm_behavior": "ignore"
            }),
        );
        assert_eq!(got, want);
        cleanup(&home);
    }

    #[test]
    fn config_yaml_wins_over_gateway_json_and_extras_deep_merge() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let gj = r#"{"group_sessions_per_user": true, "max_concurrent_sessions": 7,
                     "always_log_local": true,
                     "platforms": {"telegram": {"enabled": true, "extra": {"a": 1}}}}"#;
        let yaml = "\
max_concurrent_sessions: 3
always_log_local: false
platforms:
  telegram:
    enabled: false
    extra:
      b: 2
";
        let (got, home) = load_fixture("both", Some(yaml), Some(gj));
        let want = with(
            expected_defaults(&home),
            json!({
                "always_log_local": false,
                "max_concurrent_sessions": 3,
                "platforms": {
                    "telegram": {
                        // gateway.json's extra.a survives the deep merge, and the
                        // yaml block carrying `enabled` sets _enabled_explicit.
                        "enabled": false,
                        "extra": {"_enabled_explicit": true, "a": 1, "b": 2},
                        "gateway_restart_notification": true,
                        "reply_to_mode": "first",
                        "typing_indicator": true
                    }
                }
            }),
        );
        assert_eq!(got, want);
        cleanup(&home);
    }

    #[test]
    fn top_level_beats_nested_gateway_section() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // room_link_url and write_sessions_json exist at BOTH levels: top wins.
        // Everything else exists only under `gateway:` and is picked up there.
        let yaml = "\
room_link_url: https://top.test/
write_sessions_json: false
gateway:
  room_link_url: https://nested.test/
  write_sessions_json: true
  max_concurrent_sessions: 11
  systemd_watchdog_seconds: 45
  streaming:
    enabled: true
  reset_triggers: [\"nested-reset\"]
  quick_commands:
    n: nested
  loop_watchdog_probe_interval_s: 55
";
        let (got, home) = load_fixture("prec", Some(yaml), None);
        let want = with(
            expected_defaults(&home),
            json!({
                "loop_watchdog_probe_interval_s": 55.0,
                "max_concurrent_sessions": 11,
                "quick_commands": {"n": "nested"},
                "reset_triggers": ["nested-reset"],
                "room_link_url": "https://top.test/",
                "streaming": {
                    "buffer_threshold": 24,
                    "cursor": " \u{2589}",
                    "edit_interval": 0.8,
                    "enabled": true,
                    "fresh_final_after_seconds": 0.0,
                    "transport": "auto"
                },
                "systemd_watchdog_seconds": 45,
                "write_sessions_json": false
            }),
        );
        assert_eq!(got, want);
        cleanup(&home);
    }

    #[test]
    fn platform_blocks_bridge_shared_keys_and_mark_enabled_explicit() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let yaml = "\
platforms:
  discord:
    enabled: true
telegram:
  enabled: false
  require_mention: true
  allowed_chats: [1, 2]
  channel_prompts:
    \"123\": \"be brief\"
  channel_overrides:
    \"456\":
      model: sonnet
  unauthorized_dm_behavior: ignore
  notice_delivery: dm
";
        let (got, home) = load_fixture("plat", Some(yaml), None);
        let want = with(
            expected_defaults(&home),
            json!({
                "platforms": {
                    // enabled came from the nested platforms block, so
                    // _merge_platform_map is what set _enabled_explicit here.
                    "discord": {
                        "enabled": true,
                        "extra": {"_enabled_explicit": true},
                        "gateway_restart_notification": true,
                        "reply_to_mode": "first",
                        "typing_indicator": true
                    },
                    // telegram came from a TOP-LEVEL block, so the shared-key
                    // loop set enabled + _enabled_explicit and bridged the rest.
                    // notice_delivery "dm" is not a valid value and normalizes
                    // back to the "public" default.
                    "telegram": {
                        "channel_overrides": {"456": {"model": "sonnet"}},
                        "enabled": false,
                        "extra": {
                            "_enabled_explicit": true,
                            "allowed_chats": [1, 2],
                            "channel_prompts": {"123": "be brief"},
                            "notice_delivery": "public",
                            "require_mention": true,
                            "unauthorized_dm_behavior": "ignore"
                        },
                        "gateway_restart_notification": true,
                        "reply_to_mode": "first",
                        "typing_indicator": true
                    }
                }
            }),
        );
        assert_eq!(got, want);
        cleanup(&home);
    }

    #[test]
    fn nested_platforms_merge_before_top_level_platforms() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let yaml = "\
gateway:
  platforms:
    slack:
      enabled: true
      reply_to_mode: first
      extra:
        n1: nested
        shared: from-nested
platforms:
  slack:
    reply_to_mode: all
    extra:
      t1: top
      shared: from-top
";
        let (got, home) = load_fixture("mergeorder", Some(yaml), None);
        let want = with(
            expected_defaults(&home),
            json!({
                "platforms": {
                    "slack": {
                        // enabled survives from the nested block (top has none),
                        // reply_to_mode and the shared extra key come from the
                        // top-level block because it merges second.
                        "enabled": true,
                        "extra": {
                            "_enabled_explicit": true,
                            "n1": "nested",
                            "shared": "from-top",
                            "t1": "top"
                        },
                        "gateway_restart_notification": true,
                        "reply_to_mode": "all",
                        "typing_indicator": true
                    }
                }
            }),
        );
        assert_eq!(got, want);
        cleanup(&home);
    }

    #[test]
    fn api_server_keys_are_popped_into_extra() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Both blocks are `gateway.<platform>` subsections. api_server gets its
        // port/key/host/model_name popped into extra by the dedicated bridge.
        // webhook does NOT: the shared-key loop only looks at a top-level
        // `webhook:` block or `platforms.webhook`, never at `gateway.webhook`,
        // so its port/secret stay top-level keys and PlatformConfig drops them.
        let yaml = "\
gateway:
  api_server:
    enabled: true
    port: 8642
    key: secret-key-value
    host: 0.0.0.0
    model_name: hermes
  webhook:
    enabled: true
    port: 8649
    secret: whsec
";
        let (got, home) = load_fixture("api", Some(yaml), None);
        let want = with(
            expected_defaults(&home),
            json!({
                "platforms": {
                    "api_server": {
                        "enabled": true,
                        "extra": {
                            "_enabled_explicit": true,
                            "host": "0.0.0.0",
                            "key": "secret-key-value",
                            "model_name": "hermes",
                            "port": 8642
                        },
                        "gateway_restart_notification": true,
                        "reply_to_mode": "first",
                        "typing_indicator": true
                    },
                    "webhook": {
                        "enabled": true,
                        "extra": {"_enabled_explicit": true},
                        "gateway_restart_notification": true,
                        "reply_to_mode": "first",
                        "typing_indicator": true
                    }
                }
            }),
        );
        assert_eq!(got, want);
        cleanup(&home);
    }

    #[test]
    fn validate_resets_out_of_range_reset_policy() {
        let mut cfg = GatewayConfig {
            default_reset_policy: SessionResetPolicy {
                at_hour: json!(99),
                idle_minutes: json!(0),
                ..SessionResetPolicy::default()
            },
            ..GatewayConfig::default()
        };
        validate_gateway_config(&mut cfg);
        assert_eq!(cfg.default_reset_policy.at_hour, json!(4));
        assert_eq!(cfg.default_reset_policy.idle_minutes, json!(1440));
    }

    #[test]
    fn validate_keeps_valid_reset_policy() {
        let mut cfg = GatewayConfig {
            default_reset_policy: SessionResetPolicy {
                at_hour: json!(0),
                idle_minutes: json!(5),
                ..SessionResetPolicy::default()
            },
            ..GatewayConfig::default()
        };
        validate_gateway_config(&mut cfg);
        assert_eq!(cfg.default_reset_policy.at_hour, json!(0));
        assert_eq!(cfg.default_reset_policy.idle_minutes, json!(5));
    }

    #[test]
    fn validate_disables_platform_with_placeholder_token() {
        let mut cfg = GatewayConfig::default();
        let mut pc = crate::config_types::PlatformConfig {
            enabled: true,
            token: Some(json!("changeme")),
            ..Default::default()
        };
        cfg.platforms.insert(Platform::Telegram, pc.clone());
        // An empty token warns but is left enabled.
        pc.token = Some(json!("   "));
        cfg.platforms.insert(Platform::Discord, pc);
        validate_gateway_config(&mut cfg);
        assert!(!cfg.platforms[&Platform::Telegram].enabled);
        assert!(cfg.platforms[&Platform::Discord].enabled);
    }

    #[test]
    fn has_usable_secret_matches_python() {
        assert!(!has_usable_secret(&json!(""), 4));
        assert!(!has_usable_secret(&json!("abc"), 4));
        assert!(!has_usable_secret(&json!("  ChangeMe  "), 4));
        assert!(!has_usable_secret(&json!("None"), 4));
        assert!(!has_usable_secret(&json!(1234), 4));
        assert!(has_usable_secret(&json!("real-bot-token"), 4));
    }
}

// ---------------------------------------------------------------------------
// Differential corpus: the FULL pipeline vs real Python
// ---------------------------------------------------------------------------
//
// rust/tools/config-goldens/<name>/ holds a fixture HERMES_HOME plus the
// expected `load_gateway_config().to_dict()` captured from real Python by
// rust/tools/gen_config_goldens.py. Unlike this module's other tests, these
// exercise the WHOLE pipeline (file layers + env overrides + validation), so
// they cover config_loader, config_env_overrides, config_gateway, config_types
// and config_schema together.
//
// The goldens were generated under `env -i` with only HERMES_HOME/PATH/HOME
// (plus each fixture's declared env). This test reproduces that by clearing the
// entire process environment for the duration of each fixture and restoring it
// afterwards, so a stray TELEGRAM_BOT_TOKEN in the developer's shell cannot
// make the comparison pass or fail spuriously.
#[cfg(test)]
mod golden_corpus {
    use super::*;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn goldens_root() -> PathBuf {
        // <crate>/rust/crates/hermes-gateway -> rust/tools/config-goldens.
        // Canonicalize: the goldens record Python's resolved sessions_dir, so a
        // path carrying `..` segments would differ as a pure formatting artifact.
        let raw = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tools")
            .join("config-goldens");
        std::fs::canonicalize(&raw).unwrap_or(raw)
    }

    /// Run `f` with the process environment replaced by exactly `vars`,
    /// restoring the original environment afterwards.
    fn with_clean_env<T>(vars: &HashMap<String, String>, f: impl FnOnce() -> T) -> T {
        let saved: Vec<(String, String)> = std::env::vars().collect();
        for (k, _) in &saved {
            std::env::remove_var(k);
        }
        for (k, v) in vars {
            std::env::set_var(k, v);
        }
        let out = f();
        for (k, _) in std::env::vars().collect::<Vec<_>>() {
            std::env::remove_var(&k);
        }
        for (k, v) in saved {
            std::env::set_var(k, v);
        }
        out
    }

    #[test]
    fn full_pipeline_matches_python_goldens() {
        // The env is process-global; hold the one crate-wide test lock.
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();

        let root = goldens_root();
        assert!(
            root.is_dir(),
            "golden corpus missing at {} (run rust/tools/gen_config_goldens.py)",
            root.display()
        );

        let mut checked = 0usize;
        let mut failures: Vec<String> = Vec::new();

        let mut entries: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let dir = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let expected_path = dir.join("expected.json");
            let home = dir.join("home");
            if !expected_path.is_file() || !home.is_dir() {
                continue;
            }

            let expected: Value =
                serde_json::from_str(&std::fs::read_to_string(&expected_path).unwrap())
                    .unwrap_or_else(|e| panic!("bad expected.json for {name}: {e}"));

            let fixture_env: HashMap<String, String> =
                match std::fs::read_to_string(dir.join("env.json")) {
                    Ok(t) => serde_json::from_str(&t).unwrap_or_default(),
                    Err(_) => HashMap::new(),
                };

            let mut env = HashMap::new();
            env.insert(
                "HERMES_HOME".to_string(),
                home.to_string_lossy().to_string(),
            );
            env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
            env.insert("HOME".to_string(), "/tmp".to_string());
            for (k, v) in &fixture_env {
                env.insert(k.clone(), v.clone());
            }

            let actual = with_clean_env(&env, || load_gateway_config_from(&home).to_dict());

            if actual != expected {
                // Report only the differing keys so the failure is readable.
                let mut diffs = Vec::new();
                if let (Value::Object(a), Value::Object(e)) = (&actual, &expected) {
                    let mut keys: Vec<&String> = a.keys().chain(e.keys()).collect();
                    keys.sort();
                    keys.dedup();
                    for k in keys {
                        let av = a.get(k);
                        let ev = e.get(k);
                        if av != ev {
                            diffs.push(format!("  {k}:\n    rust   = {av:?}\n    python = {ev:?}"));
                        }
                    }
                } else {
                    diffs.push(format!("  shape mismatch: {actual:?} vs {expected:?}"));
                }
                failures.push(format!("fixture `{name}` differs:\n{}", diffs.join("\n")));
            }
            checked += 1;
        }

        assert!(
            checked > 0,
            "no golden fixtures found under {}",
            root.display()
        );
        assert!(
            failures.is_empty(),
            "{} of {checked} golden fixtures differ from real Python:\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }
}
