//! Port of _apply_env_overrides from gateway/config.py.
//!
// Public API is ahead of its callers while the gateway config pipeline is ported.
#![allow(dead_code)]
//!
//! This is the env-override tier of `gateway/config.py`: the big per-platform
//! table that layers environment variables onto an already-loaded
//! `GatewayConfig`, plus the one-time `_warn_explicit_disable_beats_env` notice
//! it emits.
//!
//! Reused wholesale:
//! - `crate::config_schema`: the `Platform` enum and the env readers
//!   (`getenv_str` / `getenv_int`, which already route through the active
//!   profile secret scope exactly like Python's `_getenv`).
//! - `crate::config_types`: `PlatformConfig`, `HomeChannel`.
//! - `crate::config_gateway`: `GatewayConfig`, `has_usable_api_server_key`.
//!
//! Order is load-bearing: later blocks can overwrite earlier ones, so the block
//! sequence below matches the Python function line for line.
//!
//! Deliberate faithfulness notes (Python quirks preserved, not fixed):
//! - `getenv` inside `_apply_env_overrides` is `_getenv_str`, whose default is
//!   `""`, so it NEVER returns None. The `BLUEBUBBLES_REQUIRE_MENTION` block's
//!   `if ... is not None:` guard is therefore always true, and
//!   `extra["require_mention"]` is written on every BlueBubbles env setup even
//!   when the var is unset (it lands as `false`). Ported as-is.
//! - `_warn_explicit_disable_beats_env` reads `os.environ` directly, NOT the
//!   profile scope, when listing which credential vars are present. Ported
//!   as-is (`std::env::var`).
//!
//! Not ported: the registry-driven plugin-enable pass that sits between the
//! session-settings block and the relay block. It imports `hermes_cli.plugins`
//! and `gateway.platform_registry` and calls Python callables
//! (`env_enablement_fn` / `is_connected` / `check_fn`) off discovered plugin
//! entries. There is no plugin registry in the Rust port yet, and the whole
//! Python block is wrapped in `try/except Exception -> logger.debug`, so the
//! faithful stand-in is a no-op at that position. It references no env vars, so
//! it does not affect the env-var surface.

use std::collections::HashSet;
use std::sync::Mutex;

use serde_json::{json, Map, Value};

use crate::config_gateway::{has_usable_api_server_key, GatewayConfig};
use crate::config_schema::{getenv_int, getenv_str, Platform};
use crate::config_types::{HomeChannel, PlatformConfig};

// --- Small local helpers -----------------------------------------------------
//
// config_schema keeps its truthiness / stringify helpers private, so the pieces
// this module needs are reproduced here.

/// `utils.TRUTHY_STRINGS` = `frozenset({"1", "true", "yes", "on"})`.
const TRUTHY_STRINGS: &[&str] = &["1", "true", "yes", "on"];

/// `is_truthy_value(s)` for the string case, which is the only case reached
/// here: strip, lowercase, membership in TRUTHY_STRINGS.
fn is_truthy_str(s: &str) -> bool {
    let lowered = s.trim().to_lowercase();
    TRUTHY_STRINGS.contains(&lowered.as_str())
}

/// Python truthiness `bool(value)` for a JSON value.
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

/// `str(value)` for the shapes an `extra` slot actually holds.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

/// `getenv(name)` -- `_getenv_str` with its `""` default.
fn ge(name: &str) -> String {
    getenv_str(name, "")
}

/// `getenv(name, default)`.
fn ged(name: &str, default: &str) -> String {
    getenv_str(name, default)
}

/// `value or None`: an empty string becomes None, matching Python's
/// `getenv(...) or None` for the thread_id arguments.
fn or_none(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// `[x.strip() for x in raw.split(",") if x.strip()]`.
fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// `int(raw)` with `except ValueError: pass`. Python's `int()` tolerates
/// surrounding whitespace and a leading sign, which `str::parse::<i64>` also
/// does. (Python additionally allows `_` digit separators; that is not
/// reproduced, and no realistic port value hits it.)
fn py_int(raw: &str) -> Option<i64> {
    raw.trim().parse::<i64>().ok()
}

/// Look up `extra["_enabled_explicit"]` truthily without removing it.
fn read_enabled_explicit(pc: &PlatformConfig) -> bool {
    pc.extra
        .get("_enabled_explicit")
        .map(py_truthy)
        .unwrap_or(false)
}

/// `extra.pop("_enabled_explicit", False)` -- consume the marker and return its
/// truthiness.
fn pop_enabled_explicit(pc: &mut PlatformConfig) -> bool {
    pc.extra
        .remove("_enabled_explicit")
        .map(|v| py_truthy(&v))
        .unwrap_or(false)
}

/// Insert the platform with `PlatformConfig()` (enabled=False) if absent, then
/// hand back a mutable borrow. Mirrors the
/// `if X not in config.platforms: config.platforms[X] = PlatformConfig()`
/// prologue used across the blocks.
fn ensure(config: &mut GatewayConfig, platform: Platform) -> &mut PlatformConfig {
    config.platforms.entry(platform).or_default()
}

/// `config.platforms[X]` for a platform known to be present.
fn at(config: &mut GatewayConfig, platform: Platform) -> &mut PlatformConfig {
    config
        .platforms
        .get_mut(&platform)
        .expect("platform inserted by the enclosing block")
}

/// `X in config.platforms`.
fn has(config: &GatewayConfig, platform: Platform) -> bool {
    config.platforms.contains_key(&platform)
}

/// Build a `HomeChannel` the way every block here does.
fn home_channel(
    platform: Platform,
    chat_id: String,
    name: String,
    thread_id: Option<String>,
) -> HomeChannel {
    HomeChannel {
        platform,
        chat_id,
        name,
        thread_id,
        user_id: None,
        scope_id: None,
    }
}

// --- _warn_explicit_disable_beats_env ---------------------------------------

/// Port of `_EXPLICIT_DISABLE_WARNED`: platforms already warned about in this
/// process. The gateway reloads config on every turn, so the notice is one-time
/// per platform per process.
static EXPLICIT_DISABLE_WARNED: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);

/// Port of `_ENV_ENABLE_CREDENTIALS`: env var(s) whose presence drives each
/// platform's env-enable branch, used only by the warning below. Kept private
/// here because the Python dict lives beside the same function.
fn env_enable_credentials(platform: Platform) -> &'static [&'static str] {
    match platform {
        Platform::Telegram => &["TELEGRAM_BOT_TOKEN"],
        Platform::Discord => &["DISCORD_BOT_TOKEN"],
        Platform::Slack => &["SLACK_BOT_TOKEN"],
        Platform::WhatsappCloud => &[
            "WHATSAPP_CLOUD_PHONE_NUMBER_ID",
            "WHATSAPP_CLOUD_ACCESS_TOKEN",
        ],
        Platform::Signal => &["SIGNAL_HTTP_URL"],
        Platform::Mattermost => &["MATTERMOST_TOKEN"],
        Platform::Matrix => &["MATRIX_ACCESS_TOKEN", "MATRIX_PASSWORD"],
        Platform::Homeassistant => &["HASS_TOKEN"],
        Platform::Email => &[
            "EMAIL_ADDRESS",
            "EMAIL_PASSWORD",
            "EMAIL_IMAP_HOST",
            "EMAIL_SMTP_HOST",
        ],
        Platform::Sms => &["TWILIO_ACCOUNT_SID"],
        Platform::Dingtalk => &["DINGTALK_CLIENT_ID", "DINGTALK_CLIENT_SECRET"],
        Platform::Feishu => &["FEISHU_APP_ID", "FEISHU_APP_SECRET"],
        Platform::Wecom => &["WECOM_BOT_ID", "WECOM_SECRET"],
        Platform::WecomCallback => &["WECOM_CALLBACK_CORP_ID", "WECOM_CALLBACK_CORP_SECRET"],
        Platform::Weixin => &["WEIXIN_TOKEN", "WEIXIN_ACCOUNT_ID"],
        Platform::Bluebubbles => &["BLUEBUBBLES_SERVER_URL", "BLUEBUBBLES_PASSWORD"],
        Platform::Qqbot => &["QQ_APP_ID", "QQ_CLIENT_SECRET"],
        Platform::Yuanbao => &["YUANBAO_APP_ID", "YUANBAO_APP_SECRET"],
        Platform::Relay => &["GATEWAY_RELAY_URL"],
        _ => &[],
    }
}

/// Port of `_warn_explicit_disable_beats_env`.
///
/// One-time WARNING that `platforms.<x>.enabled: false` wins over env creds.
/// Names the platform, the config key that is winning, and the env var(s) that
/// used to override it.
pub fn warn_explicit_disable_beats_env(platform: Platform) {
    {
        let mut guard = EXPLICIT_DISABLE_WARNED.lock().unwrap();
        let seen = guard.get_or_insert_with(HashSet::new);
        if !seen.insert(platform.value()) {
            return;
        }
    }
    let names = env_enable_credentials(platform);
    // NOTE: Python reads os.environ here, not the profile scope. Preserved.
    let present: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| {
            std::env::var(n)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
        })
        .collect();
    let joined = if present.is_empty() {
        names.join(", ")
    } else {
        present.join(", ")
    };
    let creds = if joined.is_empty() {
        "its credentials".to_string()
    } else {
        joined
    };
    tracing::warn!(
        "Platform '{}' is explicitly disabled by platforms.{}.enabled: false in \
config.yaml, so the credentials found in the environment ({}) will NOT \
start its adapter. Environment credentials no longer override an \
explicit disable. Remove the key or set platforms.{}.enabled: true to \
turn it back on.",
        platform.value(),
        platform.value(),
        creds,
        platform.value(),
    );
}

/// Test hook: forget which platforms have already been warned about, so a test
/// can exercise the warning path deterministically.
#[cfg(test)]
fn reset_explicit_disable_warned() {
    *EXPLICIT_DISABLE_WARNED.lock().unwrap() = None;
}

// --- _enable_from_env --------------------------------------------------------

/// Port of the nested `_enable_from_env(platform)` helper.
///
/// Reads (does NOT pop) the `_enabled_explicit` marker: the registry-driven
/// plugin-enable pass later in `apply_env_overrides` also needs it to avoid
/// re-enabling a platform the user explicitly disabled. The flag is cleared
/// once for all platforms in the final cleanup at the end of the function.
fn enable_from_env(config: &mut GatewayConfig, platform: Platform) -> &mut PlatformConfig {
    if let std::collections::hash_map::Entry::Vacant(e) = config.platforms.entry(platform) {
        e.insert(PlatformConfig {
            enabled: true,
            ..Default::default()
        });
        return at(config, platform);
    }

    let platform_config = at(config, platform);
    let enabled_was_explicit = read_enabled_explicit(platform_config);
    if !platform_config.enabled {
        if enabled_was_explicit {
            // Credentials are present (that is why we are here) but the user
            // said no in config.yaml. Say so once (#48820).
            warn_explicit_disable_beats_env(platform);
        } else {
            platform_config.enabled = true;
        }
    }
    at(config, platform)
}

// --- _apply_env_overrides ----------------------------------------------------

/// Port of `_apply_env_overrides`. Applies environment variable overrides to
/// `config`, mutating it in place exactly like the Python original.
pub fn apply_env_overrides(config: &mut GatewayConfig) {
    // Telegram
    let telegram_token = ge("TELEGRAM_BOT_TOKEN");
    if !telegram_token.is_empty() {
        let telegram_config = enable_from_env(config, Platform::Telegram);
        telegram_config.token = Some(json!(telegram_token));
    }

    // Reply threading mode for Telegram (off/first/all)
    let telegram_reply_mode = ge("TELEGRAM_REPLY_TO_MODE").to_lowercase();
    if matches!(telegram_reply_mode.as_str(), "off" | "first" | "all") {
        ensure(config, Platform::Telegram).reply_to_mode = json!(telegram_reply_mode);
    }

    let telegram_fallback_ips = ge("TELEGRAM_FALLBACK_IPS");
    if !telegram_fallback_ips.is_empty() {
        let cfg = ensure(config, Platform::Telegram);
        cfg.extra.insert(
            "fallback_ips".to_string(),
            json!(split_csv(&telegram_fallback_ips)),
        );
    }

    let telegram_home = ge("TELEGRAM_HOME_CHANNEL");
    if !telegram_home.is_empty() && has(config, Platform::Telegram) {
        let name = ged("TELEGRAM_HOME_CHANNEL_NAME", "Home");
        let thread = or_none(ge("TELEGRAM_HOME_CHANNEL_THREAD_ID"));
        at(config, Platform::Telegram).home_channel = Some(home_channel(
            Platform::Telegram,
            telegram_home,
            name,
            thread,
        ));
    }

    // Discord
    let discord_token = ge("DISCORD_BOT_TOKEN");
    if !discord_token.is_empty() {
        let discord_config = enable_from_env(config, Platform::Discord);
        discord_config.token = Some(json!(discord_token));
    }

    let discord_home = ge("DISCORD_HOME_CHANNEL");
    if !discord_home.is_empty() && has(config, Platform::Discord) {
        let name = ged("DISCORD_HOME_CHANNEL_NAME", "Home");
        let thread = or_none(ge("DISCORD_HOME_CHANNEL_THREAD_ID"));
        at(config, Platform::Discord).home_channel =
            Some(home_channel(Platform::Discord, discord_home, name, thread));
    }

    // Reply threading mode for Discord (off/first/all)
    let discord_reply_mode = ge("DISCORD_REPLY_TO_MODE").to_lowercase();
    if matches!(discord_reply_mode.as_str(), "off" | "first" | "all") {
        ensure(config, Platform::Discord).reply_to_mode = json!(discord_reply_mode);
    }

    // WhatsApp (typically uses different auth mechanism)
    let whatsapp_enabled = is_truthy_str(&ge("WHATSAPP_ENABLED"));
    let whatsapp_disabled_explicitly = matches!(
        ge("WHATSAPP_ENABLED").to_lowercase().as_str(),
        "false" | "0" | "no"
    );
    if has(config, Platform::Whatsapp) {
        // YAML config exists, so respect an explicit disable.
        let wa_cfg = at(config, Platform::Whatsapp);
        if whatsapp_disabled_explicitly {
            wa_cfg.enabled = false;
        } else if whatsapp_enabled {
            wa_cfg.enabled = true;
        }
        // else: keep whatever the YAML set
    } else if whatsapp_enabled {
        config.platforms.insert(
            Platform::Whatsapp,
            PlatformConfig {
                enabled: true,
                ..Default::default()
            },
        );
    }
    let whatsapp_home = ge("WHATSAPP_HOME_CHANNEL");
    if !whatsapp_home.is_empty() && has(config, Platform::Whatsapp) {
        let name = ged("WHATSAPP_HOME_CHANNEL_NAME", "Home");
        let thread = or_none(ge("WHATSAPP_HOME_CHANNEL_THREAD_ID"));
        at(config, Platform::Whatsapp).home_channel = Some(home_channel(
            Platform::Whatsapp,
            whatsapp_home,
            name,
            thread,
        ));
    }

    // WhatsApp Cloud API (official Business Platform via Meta).
    // Distinct from the Baileys bridge: pure HTTP graph.facebook.com calls
    // outbound, public webhook inbound. Both adapters can run in parallel
    // against different phone numbers.
    let whatsapp_cloud_phone_id = ge("WHATSAPP_CLOUD_PHONE_NUMBER_ID");
    let whatsapp_cloud_token = ge("WHATSAPP_CLOUD_ACCESS_TOKEN");
    if !whatsapp_cloud_phone_id.is_empty() && !whatsapp_cloud_token.is_empty() {
        // Honors an explicit `platforms.whatsapp_cloud.enabled: false` (#48820).
        enable_from_env(config, Platform::WhatsappCloud);
        {
            let extra = &mut at(config, Platform::WhatsappCloud).extra;
            extra.insert(
                "phone_number_id".to_string(),
                json!(whatsapp_cloud_phone_id),
            );
            extra.insert("access_token".to_string(), json!(whatsapp_cloud_token));
        }
        // Optional: app_id / app_secret (signature verification)
        let wa_cloud_app_id = ge("WHATSAPP_CLOUD_APP_ID");
        if !wa_cloud_app_id.is_empty() {
            at(config, Platform::WhatsappCloud)
                .extra
                .insert("app_id".to_string(), json!(wa_cloud_app_id));
        }
        let wa_cloud_app_secret = ge("WHATSAPP_CLOUD_APP_SECRET");
        if !wa_cloud_app_secret.is_empty() {
            at(config, Platform::WhatsappCloud)
                .extra
                .insert("app_secret".to_string(), json!(wa_cloud_app_secret));
        }
        // Optional: WABA id (analytics, future use)
        let wa_cloud_waba_id = ge("WHATSAPP_CLOUD_WABA_ID");
        if !wa_cloud_waba_id.is_empty() {
            at(config, Platform::WhatsappCloud)
                .extra
                .insert("waba_id".to_string(), json!(wa_cloud_waba_id));
        }
        // Webhook verify token, the Meta hub.verify_token shared secret
        let wa_cloud_verify_token = ge("WHATSAPP_CLOUD_VERIFY_TOKEN");
        if !wa_cloud_verify_token.is_empty() {
            at(config, Platform::WhatsappCloud)
                .extra
                .insert("verify_token".to_string(), json!(wa_cloud_verify_token));
        }
        // Webhook server bind config (defaults baked into the adapter)
        let wa_cloud_host = ge("WHATSAPP_CLOUD_WEBHOOK_HOST");
        if !wa_cloud_host.is_empty() {
            at(config, Platform::WhatsappCloud)
                .extra
                .insert("webhook_host".to_string(), json!(wa_cloud_host));
        }
        let wa_cloud_port = ge("WHATSAPP_CLOUD_WEBHOOK_PORT");
        if !wa_cloud_port.is_empty() {
            if let Some(port) = py_int(&wa_cloud_port) {
                at(config, Platform::WhatsappCloud)
                    .extra
                    .insert("webhook_port".to_string(), json!(port));
            }
        }
        let wa_cloud_path = ge("WHATSAPP_CLOUD_WEBHOOK_PATH");
        if !wa_cloud_path.is_empty() {
            at(config, Platform::WhatsappCloud)
                .extra
                .insert("webhook_path".to_string(), json!(wa_cloud_path));
        }
        // Graph API version override (rarely needed)
        let wa_cloud_api_version = ge("WHATSAPP_CLOUD_API_VERSION");
        if !wa_cloud_api_version.is_empty() {
            at(config, Platform::WhatsappCloud)
                .extra
                .insert("api_version".to_string(), json!(wa_cloud_api_version));
        }
    }
    let whatsapp_cloud_home = ge("WHATSAPP_CLOUD_HOME_CHANNEL");
    if !whatsapp_cloud_home.is_empty() && has(config, Platform::WhatsappCloud) {
        let name = ged("WHATSAPP_CLOUD_HOME_CHANNEL_NAME", "Home");
        let thread = or_none(ge("WHATSAPP_CLOUD_HOME_CHANNEL_THREAD_ID"));
        at(config, Platform::WhatsappCloud).home_channel = Some(home_channel(
            Platform::WhatsappCloud,
            whatsapp_cloud_home,
            name,
            thread,
        ));
    }

    // Slack
    let slack_token = ge("SLACK_BOT_TOKEN");
    if !slack_token.is_empty() {
        if !has(config, Platform::Slack) {
            // No yaml config for Slack, so this is an env-only setup: enable it.
            config.platforms.insert(
                Platform::Slack,
                PlatformConfig {
                    enabled: true,
                    ..Default::default()
                },
            );
        } else {
            let slack_config = at(config, Platform::Slack);
            // Read (don't pop) the explicit-enable marker: the registry-driven
            // plugin-enable pass below also needs it to avoid re-enabling a
            // platform the user explicitly disabled (Slack is now a plugin
            // entry, #41112). The flag is cleared once for all platforms in the
            // final cleanup at the end of apply_env_overrides.
            let enabled_was_explicit = read_enabled_explicit(slack_config);
            if !slack_config.enabled && !enabled_was_explicit {
                // Top-level Slack settings such as channel prompts should not
                // turn an env-token setup into a disabled platform. Only an
                // explicit slack.enabled/platforms.slack.enabled false should.
                slack_config.enabled = true;
            } else if !slack_config.enabled {
                warn_explicit_disable_beats_env(Platform::Slack);
            }
        }
        // If yaml config exists, respect its enabled flag (don't override an
        // explicit enabled: false). The token is still stored so skills that
        // send Slack messages can use it without activating the gateway adapter.
        at(config, Platform::Slack).token = Some(json!(slack_token));
    }
    let slack_home = ge("SLACK_HOME_CHANNEL");
    if !slack_home.is_empty() {
        let name = ged("SLACK_HOME_CHANNEL_NAME", "");
        let thread = or_none(ge("SLACK_HOME_CHANNEL_THREAD_ID"));
        // setdefault(Platform.SLACK, PlatformConfig(enabled=False))
        let slack_config = config.platforms.entry(Platform::Slack).or_default();
        let existing_home = slack_config.home_channel.clone();
        let same_home = existing_home
            .as_ref()
            .map(|h| h.chat_id == slack_home)
            .unwrap_or(false);
        let (user_id, scope_id) = match (&existing_home, same_home) {
            (Some(h), true) => (h.user_id.clone(), h.scope_id.clone()),
            _ => (None, None),
        };
        slack_config.home_channel = Some(HomeChannel {
            platform: Platform::Slack,
            chat_id: slack_home,
            name,
            thread_id: thread,
            user_id,
            scope_id,
        });
    }

    // Signal
    let signal_url = ge("SIGNAL_HTTP_URL");
    let signal_account = ge("SIGNAL_ACCOUNT");
    if !signal_url.is_empty() && !signal_account.is_empty() {
        let ignore_stories = is_truthy_str(&ged("SIGNAL_IGNORE_STORIES", "true"));
        let signal_config = enable_from_env(config, Platform::Signal);
        signal_config
            .extra
            .insert("http_url".to_string(), json!(signal_url));
        signal_config
            .extra
            .insert("account".to_string(), json!(signal_account));
        signal_config
            .extra
            .insert("ignore_stories".to_string(), json!(ignore_stories));
    }
    let signal_home = ge("SIGNAL_HOME_CHANNEL");
    if !signal_home.is_empty() && has(config, Platform::Signal) {
        let name = ged("SIGNAL_HOME_CHANNEL_NAME", "Home");
        let thread = or_none(ge("SIGNAL_HOME_CHANNEL_THREAD_ID"));
        at(config, Platform::Signal).home_channel =
            Some(home_channel(Platform::Signal, signal_home, name, thread));
    }

    // Mattermost
    let mattermost_token = ge("MATTERMOST_TOKEN");
    if !mattermost_token.is_empty() {
        let mattermost_url = ge("MATTERMOST_URL");
        if mattermost_url.is_empty() {
            tracing::warn!("MATTERMOST_TOKEN set but MATTERMOST_URL is missing");
        }
        let mattermost_config = enable_from_env(config, Platform::Mattermost);
        mattermost_config.token = Some(json!(mattermost_token));
        mattermost_config
            .extra
            .insert("url".to_string(), json!(mattermost_url));
    }
    let mattermost_home = ge("MATTERMOST_HOME_CHANNEL");
    if !mattermost_home.is_empty() && has(config, Platform::Mattermost) {
        let name = ged("MATTERMOST_HOME_CHANNEL_NAME", "Home");
        let thread = or_none(ge("MATTERMOST_HOME_CHANNEL_THREAD_ID"));
        at(config, Platform::Mattermost).home_channel = Some(home_channel(
            Platform::Mattermost,
            mattermost_home,
            name,
            thread,
        ));
    }

    // Matrix
    let matrix_token = ge("MATRIX_ACCESS_TOKEN");
    let matrix_homeserver = ge("MATRIX_HOMESERVER");
    if !matrix_token.is_empty() || !ge("MATRIX_PASSWORD").is_empty() {
        if matrix_homeserver.is_empty() {
            tracing::warn!(
                "MATRIX_ACCESS_TOKEN/MATRIX_PASSWORD set but MATRIX_HOMESERVER is missing"
            );
        }
        let matrix_user = ge("MATRIX_USER_ID");
        let matrix_password = ge("MATRIX_PASSWORD");
        let matrix_e2ee_mode = ge("MATRIX_E2EE_MODE").trim().to_lowercase();
        let matrix_e2ee = matches!(
            matrix_e2ee_mode.as_str(),
            "required" | "require" | "optional" | "prefer" | "preferred"
        ) || is_truthy_str(&ge("MATRIX_ENCRYPTION"));
        let matrix_device_id = ge("MATRIX_DEVICE_ID");

        let matrix_config = enable_from_env(config, Platform::Matrix);
        if !matrix_token.is_empty() {
            matrix_config.token = Some(json!(matrix_token));
        }
        matrix_config
            .extra
            .insert("homeserver".to_string(), json!(matrix_homeserver));
        if !matrix_user.is_empty() {
            matrix_config
                .extra
                .insert("user_id".to_string(), json!(matrix_user));
        }
        if !matrix_password.is_empty() {
            matrix_config
                .extra
                .insert("password".to_string(), json!(matrix_password));
        }
        matrix_config
            .extra
            .insert("encryption".to_string(), json!(matrix_e2ee));
        if !matrix_e2ee_mode.is_empty() {
            matrix_config
                .extra
                .insert("e2ee_mode".to_string(), json!(matrix_e2ee_mode));
        }
        if !matrix_device_id.is_empty() {
            matrix_config
                .extra
                .insert("device_id".to_string(), json!(matrix_device_id));
        }
    }
    let matrix_home = ge("MATRIX_HOME_ROOM");
    if !matrix_home.is_empty() && has(config, Platform::Matrix) {
        let name = ged("MATRIX_HOME_ROOM_NAME", "Home");
        let thread = or_none(ge("MATRIX_HOME_ROOM_THREAD_ID"));
        at(config, Platform::Matrix).home_channel =
            Some(home_channel(Platform::Matrix, matrix_home, name, thread));
    }

    // Home Assistant
    let hass_token = ge("HASS_TOKEN");
    if !hass_token.is_empty() {
        // Honors an explicit `platforms.homeassistant.enabled: false` (#48820).
        enable_from_env(config, Platform::Homeassistant);
        at(config, Platform::Homeassistant).token = Some(json!(hass_token));
        let hass_url = ge("HASS_URL");
        if !hass_url.is_empty() {
            at(config, Platform::Homeassistant)
                .extra
                .insert("url".to_string(), json!(hass_url));
        }
    }

    // Email
    let email_addr = ge("EMAIL_ADDRESS");
    let email_pwd = ge("EMAIL_PASSWORD");
    let email_imap = ge("EMAIL_IMAP_HOST");
    let email_smtp = ge("EMAIL_SMTP_HOST");
    if !email_addr.is_empty()
        && !email_pwd.is_empty()
        && !email_imap.is_empty()
        && !email_smtp.is_empty()
    {
        // Honors an explicit `platforms.email.enabled: false` (#48820).
        enable_from_env(config, Platform::Email);
        let extra = &mut at(config, Platform::Email).extra;
        extra.insert("address".to_string(), json!(email_addr));
        extra.insert("imap_host".to_string(), json!(email_imap));
        extra.insert("smtp_host".to_string(), json!(email_smtp));
    }
    let email_home = ge("EMAIL_HOME_ADDRESS");
    if !email_home.is_empty() && has(config, Platform::Email) {
        let name = ged("EMAIL_HOME_ADDRESS_NAME", "Home");
        let thread = or_none(ge("EMAIL_HOME_ADDRESS_THREAD_ID"));
        at(config, Platform::Email).home_channel =
            Some(home_channel(Platform::Email, email_home, name, thread));
    }

    // SMS (Twilio)
    let twilio_sid = ge("TWILIO_ACCOUNT_SID");
    if !twilio_sid.is_empty() {
        // Honors an explicit `platforms.sms.enabled: false` (#48820).
        enable_from_env(config, Platform::Sms);
        let auth_token = ge("TWILIO_AUTH_TOKEN");
        at(config, Platform::Sms).api_key = Some(json!(auth_token));
    }
    let sms_home = ge("SMS_HOME_CHANNEL");
    if !sms_home.is_empty() && has(config, Platform::Sms) {
        let name = ged("SMS_HOME_CHANNEL_NAME", "Home");
        let thread = or_none(ge("SMS_HOME_CHANNEL_THREAD_ID"));
        at(config, Platform::Sms).home_channel =
            Some(home_channel(Platform::Sms, sms_home, name, thread));
    }

    // API Server
    let api_server_key = ge("API_SERVER_KEY");
    let api_server_cors_origins = ge("API_SERVER_CORS_ORIGINS");
    let api_server_port = ge("API_SERVER_PORT");
    let api_server_host = ge("API_SERVER_HOST");
    // Require a usable key: API_SERVER_ENABLED alone would load an
    // unauthenticated platform whose adapter refuses to start at connect()
    // anyway (startup guard in gateway/platforms/api_server.py), leaving the
    // reconnect watcher spinning and logging errors forever. Same strength bar
    // as the startup guard (has_usable_secret, min_length=16).
    if has_usable_api_server_key(&json!(api_server_key)) {
        ensure(config, Platform::ApiServer);
        // Respect an explicit `enabled: false` in config.yaml (flagged by
        // `_enabled_explicit`). In multiplex mode a secondary profile's
        // config.yaml pins `platforms.api_server.enabled: false` so it shares
        // the default profile's listener instead of binding its own port. That
        // profile still inherits the process-level env (including
        // API_SERVER_KEY); without this guard the env-var presence would
        // force-enable the listener and trip the MultiplexConfigError check.
        // Pop (don't read) the marker: the api_server branch is terminal (no
        // later registry pass re-enables it), so this both consumes the flag and
        // avoids reading it twice, matching the pop convention used elsewhere.
        let api_server_explicit = pop_enabled_explicit(at(config, Platform::ApiServer));
        if !api_server_explicit || at(config, Platform::ApiServer).enabled {
            at(config, Platform::ApiServer).enabled = true;
        }
        if !api_server_key.is_empty() {
            at(config, Platform::ApiServer)
                .extra
                .insert("key".to_string(), json!(api_server_key));
        }
        if !api_server_cors_origins.is_empty() {
            let origins = split_csv(&api_server_cors_origins);
            if !origins.is_empty() {
                at(config, Platform::ApiServer)
                    .extra
                    .insert("cors_origins".to_string(), json!(origins));
            }
        }
        if !api_server_port.is_empty() {
            if let Some(port) = py_int(&api_server_port) {
                at(config, Platform::ApiServer)
                    .extra
                    .insert("port".to_string(), json!(port));
            }
        }
        if !api_server_host.is_empty() {
            at(config, Platform::ApiServer)
                .extra
                .insert("host".to_string(), json!(api_server_host));
        }
        let api_server_model_name = ge("API_SERVER_MODEL_NAME");
        if !api_server_model_name.is_empty() {
            at(config, Platform::ApiServer)
                .extra
                .insert("model_name".to_string(), json!(api_server_model_name));
        }
    }

    // Webhook platform
    let webhook_enabled = is_truthy_str(&ge("WEBHOOK_ENABLED"));
    let webhook_port = ge("WEBHOOK_PORT");
    let webhook_secret = ge("WEBHOOK_SECRET");
    if webhook_enabled {
        ensure(config, Platform::Webhook);
        // Honor an explicit `enabled: false` in config.yaml (flagged by
        // `_enabled_explicit`). Same multiplex reasoning as the api_server
        // branch above. Pop (don't read) the marker: the webhook branch is
        // terminal.
        let webhook_explicit = pop_enabled_explicit(at(config, Platform::Webhook));
        if !webhook_explicit || at(config, Platform::Webhook).enabled {
            at(config, Platform::Webhook).enabled = true;
        }
        if !webhook_port.is_empty() {
            if let Some(port) = py_int(&webhook_port) {
                at(config, Platform::Webhook)
                    .extra
                    .insert("port".to_string(), json!(port));
            }
        }
        if !webhook_secret.is_empty() {
            at(config, Platform::Webhook)
                .extra
                .insert("secret".to_string(), json!(webhook_secret));
        }
    }

    // Microsoft Graph webhook platform
    let msgraph_webhook_enabled = is_truthy_str(&ge("MSGRAPH_WEBHOOK_ENABLED"));
    let msgraph_webhook_port = ge("MSGRAPH_WEBHOOK_PORT");
    let msgraph_webhook_client_state = ge("MSGRAPH_WEBHOOK_CLIENT_STATE");
    let msgraph_webhook_resources = ge("MSGRAPH_WEBHOOK_ACCEPTED_RESOURCES");
    let msgraph_webhook_allowed_cidrs = ge("MSGRAPH_WEBHOOK_ALLOWED_SOURCE_CIDRS");
    if msgraph_webhook_enabled
        || has(config, Platform::MsgraphWebhook)
        || !msgraph_webhook_port.is_empty()
        || !msgraph_webhook_client_state.is_empty()
        || !msgraph_webhook_resources.is_empty()
        || !msgraph_webhook_allowed_cidrs.is_empty()
    {
        ensure(config, Platform::MsgraphWebhook);
        if msgraph_webhook_enabled {
            // Same explicit-disable guard as the webhook branch above (#85637).
            // READ (don't pop) the marker here: the relay-exclusive pass below
            // still consults it, and the end-of-function scrub removes it for
            // every platform.
            let msgraph_cfg = at(config, Platform::MsgraphWebhook);
            if !read_enabled_explicit(msgraph_cfg) || msgraph_cfg.enabled {
                msgraph_cfg.enabled = true;
            }
        }
        if !msgraph_webhook_port.is_empty() {
            if let Some(port) = py_int(&msgraph_webhook_port) {
                at(config, Platform::MsgraphWebhook)
                    .extra
                    .insert("port".to_string(), json!(port));
            }
        }
        if !msgraph_webhook_client_state.is_empty() {
            at(config, Platform::MsgraphWebhook).extra.insert(
                "client_state".to_string(),
                json!(msgraph_webhook_client_state),
            );
        }
        if !msgraph_webhook_resources.is_empty() {
            let resources = split_csv(&msgraph_webhook_resources);
            if !resources.is_empty() {
                at(config, Platform::MsgraphWebhook)
                    .extra
                    .insert("accepted_resources".to_string(), json!(resources));
            }
        }
        if !msgraph_webhook_allowed_cidrs.is_empty() {
            let cidrs = split_csv(&msgraph_webhook_allowed_cidrs);
            if !cidrs.is_empty() {
                at(config, Platform::MsgraphWebhook)
                    .extra
                    .insert("allowed_source_cidrs".to_string(), json!(cidrs));
            }
        }
    }

    // DingTalk
    let dingtalk_client_id = ge("DINGTALK_CLIENT_ID");
    let dingtalk_client_secret = ge("DINGTALK_CLIENT_SECRET");
    if !dingtalk_client_id.is_empty() && !dingtalk_client_secret.is_empty() {
        // Honors an explicit `platforms.dingtalk.enabled: false` (#48820).
        enable_from_env(config, Platform::Dingtalk);
        {
            let extra = &mut at(config, Platform::Dingtalk).extra;
            extra.insert("client_id".to_string(), json!(dingtalk_client_id));
            extra.insert("client_secret".to_string(), json!(dingtalk_client_secret));
        }
        let dingtalk_home = ge("DINGTALK_HOME_CHANNEL");
        if !dingtalk_home.is_empty() {
            let name = ged("DINGTALK_HOME_CHANNEL_NAME", "Home");
            let thread = or_none(ge("DINGTALK_HOME_CHANNEL_THREAD_ID"));
            at(config, Platform::Dingtalk).home_channel = Some(home_channel(
                Platform::Dingtalk,
                dingtalk_home,
                name,
                thread,
            ));
        }
    }

    // Feishu / Lark
    let feishu_app_id = ge("FEISHU_APP_ID");
    let feishu_app_secret = ge("FEISHU_APP_SECRET");
    if !feishu_app_id.is_empty() && !feishu_app_secret.is_empty() {
        // Honors an explicit `platforms.feishu.enabled: false` (#48820).
        enable_from_env(config, Platform::Feishu);
        let domain = ged("FEISHU_DOMAIN", "feishu");
        let connection_mode = ged("FEISHU_CONNECTION_MODE", "websocket");
        {
            let extra = &mut at(config, Platform::Feishu).extra;
            extra.insert("app_id".to_string(), json!(feishu_app_id));
            extra.insert("app_secret".to_string(), json!(feishu_app_secret));
            extra.insert("domain".to_string(), json!(domain));
            extra.insert("connection_mode".to_string(), json!(connection_mode));
        }
        let feishu_encrypt_key = ge("FEISHU_ENCRYPT_KEY");
        if !feishu_encrypt_key.is_empty() {
            at(config, Platform::Feishu)
                .extra
                .insert("encrypt_key".to_string(), json!(feishu_encrypt_key));
        }
        let feishu_verification_token = ge("FEISHU_VERIFICATION_TOKEN");
        if !feishu_verification_token.is_empty() {
            at(config, Platform::Feishu).extra.insert(
                "verification_token".to_string(),
                json!(feishu_verification_token),
            );
        }
        let feishu_home = ge("FEISHU_HOME_CHANNEL");
        if !feishu_home.is_empty() {
            let name = ged("FEISHU_HOME_CHANNEL_NAME", "Home");
            let thread = or_none(ge("FEISHU_HOME_CHANNEL_THREAD_ID"));
            at(config, Platform::Feishu).home_channel =
                Some(home_channel(Platform::Feishu, feishu_home, name, thread));
        }
    }

    // WeCom (Enterprise WeChat)
    let wecom_bot_id = ge("WECOM_BOT_ID");
    let wecom_secret = ge("WECOM_SECRET");
    if !wecom_bot_id.is_empty() && !wecom_secret.is_empty() {
        // Honors an explicit `platforms.wecom.enabled: false` (#48820).
        enable_from_env(config, Platform::Wecom);
        {
            let extra = &mut at(config, Platform::Wecom).extra;
            extra.insert("bot_id".to_string(), json!(wecom_bot_id));
            extra.insert("secret".to_string(), json!(wecom_secret));
        }
        let wecom_ws_url = ge("WECOM_WEBSOCKET_URL");
        if !wecom_ws_url.is_empty() {
            at(config, Platform::Wecom)
                .extra
                .insert("websocket_url".to_string(), json!(wecom_ws_url));
        }
        let wecom_home = ge("WECOM_HOME_CHANNEL");
        if !wecom_home.is_empty() {
            let name = ged("WECOM_HOME_CHANNEL_NAME", "Home");
            let thread = or_none(ge("WECOM_HOME_CHANNEL_THREAD_ID"));
            at(config, Platform::Wecom).home_channel =
                Some(home_channel(Platform::Wecom, wecom_home, name, thread));
        }
    }

    // WeCom callback mode (self-built apps)
    let wecom_callback_corp_id = ge("WECOM_CALLBACK_CORP_ID");
    let wecom_callback_corp_secret = ge("WECOM_CALLBACK_CORP_SECRET");
    if !wecom_callback_corp_id.is_empty() && !wecom_callback_corp_secret.is_empty() {
        // Honors an explicit `platforms.wecom_callback.enabled: false` (#48820).
        let agent_id = ge("WECOM_CALLBACK_AGENT_ID");
        let token = ge("WECOM_CALLBACK_TOKEN");
        let aes_key = ge("WECOM_CALLBACK_ENCODING_AES_KEY");
        // No default here: an unset WECOM_CALLBACK_HOST leaves extra.host falsy
        // so the adapter's dual-stack DEFAULT_HOST=None applies (binds
        // IPv4 + IPv6; "0.0.0.0" was IPv4-only, NS-603).
        let host = ge("WECOM_CALLBACK_HOST");
        let port = getenv_int("WECOM_CALLBACK_PORT", 8645);

        enable_from_env(config, Platform::WecomCallback);
        let extra = &mut at(config, Platform::WecomCallback).extra;
        extra.insert("corp_id".to_string(), json!(wecom_callback_corp_id));
        extra.insert("corp_secret".to_string(), json!(wecom_callback_corp_secret));
        extra.insert("agent_id".to_string(), json!(agent_id));
        extra.insert("token".to_string(), json!(token));
        extra.insert("encoding_aes_key".to_string(), json!(aes_key));
        extra.insert("host".to_string(), json!(host));
        extra.insert("port".to_string(), json!(port));
    }

    // Weixin (personal WeChat via iLink Bot API)
    let weixin_token = ge("WEIXIN_TOKEN");
    let weixin_account_id = ge("WEIXIN_ACCOUNT_ID");
    if !weixin_token.is_empty() || !weixin_account_id.is_empty() {
        // Honors an explicit `platforms.weixin.enabled: false` (#48820).
        enable_from_env(config, Platform::Weixin);
        if !weixin_token.is_empty() {
            at(config, Platform::Weixin).token = Some(json!(weixin_token));
        }
        let weixin_base_url = ge("WEIXIN_BASE_URL").trim().to_string();
        let weixin_cdn_base_url = ge("WEIXIN_CDN_BASE_URL").trim().to_string();
        let weixin_dm_policy = ge("WEIXIN_DM_POLICY").trim().to_lowercase();
        let weixin_group_policy = ge("WEIXIN_GROUP_POLICY").trim().to_lowercase();
        let weixin_allowed_users = ge("WEIXIN_ALLOWED_USERS").trim().to_string();
        let weixin_group_allowed_users = ge("WEIXIN_GROUP_ALLOWED_USERS").trim().to_string();
        let weixin_split_multiline = ge("WEIXIN_SPLIT_MULTILINE_MESSAGES").trim().to_string();
        {
            let extra = &mut at(config, Platform::Weixin).extra;
            if !weixin_account_id.is_empty() {
                extra.insert("account_id".to_string(), json!(weixin_account_id));
            }
            if !weixin_base_url.is_empty() {
                extra.insert(
                    "base_url".to_string(),
                    json!(weixin_base_url.trim_end_matches('/')),
                );
            }
            if !weixin_cdn_base_url.is_empty() {
                extra.insert(
                    "cdn_base_url".to_string(),
                    json!(weixin_cdn_base_url.trim_end_matches('/')),
                );
            }
            if !weixin_dm_policy.is_empty() {
                extra.insert("dm_policy".to_string(), json!(weixin_dm_policy));
            }
            if !weixin_group_policy.is_empty() {
                extra.insert("group_policy".to_string(), json!(weixin_group_policy));
            }
            if !weixin_allowed_users.is_empty() {
                extra.insert("allow_from".to_string(), json!(weixin_allowed_users));
            }
            if !weixin_group_allowed_users.is_empty() {
                extra.insert(
                    "group_allow_from".to_string(),
                    json!(weixin_group_allowed_users),
                );
            }
            if !weixin_split_multiline.is_empty() {
                extra.insert(
                    "split_multiline_messages".to_string(),
                    json!(weixin_split_multiline),
                );
            }
        }
        let weixin_home = ge("WEIXIN_HOME_CHANNEL").trim().to_string();
        if !weixin_home.is_empty() {
            let name = ged("WEIXIN_HOME_CHANNEL_NAME", "Home");
            let thread = or_none(ge("WEIXIN_HOME_CHANNEL_THREAD_ID"));
            at(config, Platform::Weixin).home_channel =
                Some(home_channel(Platform::Weixin, weixin_home, name, thread));
        }
    }

    // BlueBubbles (iMessage)
    let bluebubbles_server_url = ge("BLUEBUBBLES_SERVER_URL");
    let bluebubbles_password = ge("BLUEBUBBLES_PASSWORD");
    if !bluebubbles_server_url.is_empty() && !bluebubbles_password.is_empty() {
        // Honors an explicit `platforms.bluebubbles.enabled: false` (#48820).
        let webhook_host = ged("BLUEBUBBLES_WEBHOOK_HOST", "127.0.0.1");
        let webhook_port = getenv_int("BLUEBUBBLES_WEBHOOK_PORT", 8645);
        let webhook_path = ged("BLUEBUBBLES_WEBHOOK_PATH", "/bluebubbles-webhook");
        let send_read_receipts = is_truthy_str(&ged("BLUEBUBBLES_SEND_READ_RECEIPTS", "true"));

        enable_from_env(config, Platform::Bluebubbles);
        {
            let extra = &mut at(config, Platform::Bluebubbles).extra;
            extra.insert(
                "server_url".to_string(),
                json!(bluebubbles_server_url.trim_end_matches('/')),
            );
            extra.insert("password".to_string(), json!(bluebubbles_password));
            extra.insert("webhook_host".to_string(), json!(webhook_host));
            extra.insert("webhook_port".to_string(), json!(webhook_port));
            extra.insert("webhook_path".to_string(), json!(webhook_path));
            extra.insert("send_read_receipts".to_string(), json!(send_read_receipts));
        }
        // Python guards this with `is not None`, but `getenv` here is
        // `_getenv_str` (default ""), so the guard is ALWAYS true and
        // require_mention is written even when the var is unset. Preserved.
        let bluebubbles_require_mention = ge("BLUEBUBBLES_REQUIRE_MENTION");
        let require_mention = matches!(
            bluebubbles_require_mention.to_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        );
        at(config, Platform::Bluebubbles)
            .extra
            .insert("require_mention".to_string(), json!(require_mention));
        let bluebubbles_mention_patterns = ge("BLUEBUBBLES_MENTION_PATTERNS");
        if !bluebubbles_mention_patterns.is_empty() {
            let parsed_patterns: Value =
                match serde_json::from_str::<Value>(&bluebubbles_mention_patterns) {
                    Ok(v) => v,
                    Err(_) => {
                        let replaced = bluebubbles_mention_patterns.replace('\n', ",");
                        json!(split_csv(&replaced))
                    }
                };
            at(config, Platform::Bluebubbles)
                .extra
                .insert("mention_patterns".to_string(), parsed_patterns);
        }
    }
    let bluebubbles_home = ge("BLUEBUBBLES_HOME_CHANNEL");
    if !bluebubbles_home.is_empty() && has(config, Platform::Bluebubbles) {
        let name = ged("BLUEBUBBLES_HOME_CHANNEL_NAME", "Home");
        let thread = or_none(ge("BLUEBUBBLES_HOME_CHANNEL_THREAD_ID"));
        at(config, Platform::Bluebubbles).home_channel = Some(home_channel(
            Platform::Bluebubbles,
            bluebubbles_home,
            name,
            thread,
        ));
    }

    // QQ (Official Bot API v2)
    let qq_app_id = ge("QQ_APP_ID");
    let qq_client_secret = ge("QQ_CLIENT_SECRET");
    if !qq_app_id.is_empty() || !qq_client_secret.is_empty() {
        // Honors an explicit `platforms.qqbot.enabled: false` (#48820).
        enable_from_env(config, Platform::Qqbot);
        let qq_allowed_users = ge("QQ_ALLOWED_USERS").trim().to_string();
        let qq_group_allowed = ge("QQ_GROUP_ALLOWED_USERS").trim().to_string();
        {
            let extra = &mut at(config, Platform::Qqbot).extra;
            if !qq_app_id.is_empty() {
                extra.insert("app_id".to_string(), json!(qq_app_id));
            }
            if !qq_client_secret.is_empty() {
                extra.insert("client_secret".to_string(), json!(qq_client_secret));
            }
            if !qq_allowed_users.is_empty() {
                extra.insert("allow_from".to_string(), json!(qq_allowed_users));
            }
            if !qq_group_allowed.is_empty() {
                extra.insert("group_allow_from".to_string(), json!(qq_group_allowed));
            }
        }
        let mut qq_home = ge("QQBOT_HOME_CHANNEL").trim().to_string();
        let mut qq_home_name_env = "QQBOT_HOME_CHANNEL_NAME";
        if qq_home.is_empty() {
            // Back-compat: accept the pre-rename name and log a one-time warning.
            let legacy_home = ge("QQ_HOME_CHANNEL").trim().to_string();
            if !legacy_home.is_empty() {
                qq_home = legacy_home;
                qq_home_name_env = "QQ_HOME_CHANNEL_NAME";
                tracing::warn!(
                    "QQ_HOME_CHANNEL is deprecated; rename to QQBOT_HOME_CHANNEL \
in your .env for consistency with the platform key."
                );
            }
        }
        if !qq_home.is_empty() {
            let preferred = ge("QQBOT_HOME_CHANNEL_NAME");
            let name = if preferred.is_empty() {
                ged(qq_home_name_env, "Home")
            } else {
                preferred
            };
            let thread = or_none(ge("QQBOT_HOME_CHANNEL_THREAD_ID"))
                .or_else(|| or_none(ge("QQ_HOME_CHANNEL_THREAD_ID")));
            at(config, Platform::Qqbot).home_channel =
                Some(home_channel(Platform::Qqbot, qq_home, name, thread));
        }
    }

    // Yuanbao, YUANBAO_APP_ID preferred
    let yuanbao_app_id = {
        let primary = ge("YUANBAO_APP_ID");
        if primary.is_empty() {
            ge("YUANBAO_APP_KEY")
        } else {
            primary
        }
    };
    let yuanbao_app_secret = ge("YUANBAO_APP_SECRET");
    if !yuanbao_app_id.is_empty() && !yuanbao_app_secret.is_empty() {
        // Honors an explicit `platforms.yuanbao.enabled: false` (#48820).
        enable_from_env(config, Platform::Yuanbao);
        let yuanbao_bot_id = ge("YUANBAO_BOT_ID");
        let yuanbao_ws_url = ge("YUANBAO_WS_URL");
        let yuanbao_api_domain = ge("YUANBAO_API_DOMAIN");
        let yuanbao_route_env = ge("YUANBAO_ROUTE_ENV");
        {
            let extra = &mut at(config, Platform::Yuanbao).extra;
            extra.insert("app_id".to_string(), json!(yuanbao_app_id));
            extra.insert("app_secret".to_string(), json!(yuanbao_app_secret));
            if !yuanbao_bot_id.is_empty() {
                extra.insert("bot_id".to_string(), json!(yuanbao_bot_id));
            }
            if !yuanbao_ws_url.is_empty() {
                extra.insert("ws_url".to_string(), json!(yuanbao_ws_url));
            }
            if !yuanbao_api_domain.is_empty() {
                extra.insert("api_domain".to_string(), json!(yuanbao_api_domain));
            }
            if !yuanbao_route_env.is_empty() {
                extra.insert("route_env".to_string(), json!(yuanbao_route_env));
            }
        }
        let yuanbao_home = ge("YUANBAO_HOME_CHANNEL");
        if !yuanbao_home.is_empty() {
            let name = ged("YUANBAO_HOME_CHANNEL_NAME", "Home");
            let thread = or_none(ge("YUANBAO_HOME_CHANNEL_THREAD_ID"));
            at(config, Platform::Yuanbao).home_channel =
                Some(home_channel(Platform::Yuanbao, yuanbao_home, name, thread));
        }
        let yuanbao_dm_policy = ge("YUANBAO_DM_POLICY");
        let yuanbao_dm_allow_from = ge("YUANBAO_DM_ALLOW_FROM");
        let yuanbao_group_policy = ge("YUANBAO_GROUP_POLICY");
        let yuanbao_group_allow_from = ge("YUANBAO_GROUP_ALLOW_FROM");
        let extra = &mut at(config, Platform::Yuanbao).extra;
        if !yuanbao_dm_policy.is_empty() {
            extra.insert(
                "dm_policy".to_string(),
                json!(yuanbao_dm_policy.trim().to_lowercase()),
            );
        }
        if !yuanbao_dm_allow_from.is_empty() {
            extra.insert("dm_allow_from".to_string(), json!(yuanbao_dm_allow_from));
        }
        if !yuanbao_group_policy.is_empty() {
            extra.insert(
                "group_policy".to_string(),
                json!(yuanbao_group_policy.trim().to_lowercase()),
            );
        }
        if !yuanbao_group_allow_from.is_empty() {
            extra.insert(
                "group_allow_from".to_string(),
                json!(yuanbao_group_allow_from),
            );
        }
    }

    // Session settings
    let idle_minutes = ge("SESSION_IDLE_MINUTES");
    if !idle_minutes.is_empty() {
        if let Some(v) = py_int(&idle_minutes) {
            config.default_reset_policy.idle_minutes = json!(v);
        }
    }

    let reset_hour = ge("SESSION_RESET_HOUR");
    if !reset_hour.is_empty() {
        if let Some(v) = py_int(&reset_hour) {
            config.default_reset_policy.at_hour = json!(v);
        }
    }

    // Registry-driven enable for plugin platforms sits here in Python. It
    // discovers plugin entries and calls their Python callables; there is no
    // plugin registry in this port, and the whole Python block is wrapped in
    // `try/except Exception -> logger.debug`, so the faithful stand-in is a
    // no-op. It reads no env vars. See the module docstring.

    // Relay (generic connector-fronted platform, EXPERIMENTAL). Enabled when a
    // connector relay URL is configured via GATEWAY_RELAY_URL (env) or
    // gateway.relay_url (config.yaml). The adapter dials OUT to the connector,
    // so, like Telegram/Matrix, it has no public inbound port and just needs
    // Platform::Relay present+enabled in config.platforms for the connect loop
    // to bring it up. The connected-checker keys on extra["relay_url"], so
    // mirror the URL into extra here.
    let relay_url_env = ge("GATEWAY_RELAY_URL").trim().to_string();
    let mut relay_url_yaml = String::new();
    if let Some(existing_relay) = config.platforms.get(&Platform::Relay) {
        let raw = existing_relay
            .extra
            .get("relay_url")
            .cloned()
            .unwrap_or(Value::Null);
        // str(x or "")
        let as_str = if py_truthy(&raw) {
            py_str(&raw)
        } else {
            String::new()
        };
        relay_url_yaml = as_str.trim().to_string();
    }
    let relay_url_val = if relay_url_env.is_empty() {
        relay_url_yaml.clone()
    } else {
        relay_url_env.clone()
    };
    if !relay_url_val.is_empty() {
        let relay_config = enable_from_env(config, Platform::Relay);
        relay_config.extra.insert(
            "relay_url".to_string(),
            json!(relay_url_val.trim_end_matches('/')),
        );
    }

    // Relay-exclusive: a GATEWAY_RELAY_URL env stamp marks a connector-fronted
    // deployment where the connector owns every platform connection. Any
    // directly-connected messaging adapter in the same process would be a
    // second, unmanaged ingress path (duplicate deliveries, split sessions, and
    // a live socket that disarms scale-to-zero), so the env stamp disables all
    // other messaging platforms, including ones explicitly enabled in
    // config.yaml. Non-messaging surfaces (local, api_server, webhook) are
    // untouched. Deployments that configure relay only via gateway.relay_url in
    // config.yaml keep the old additive behavior.
    //
    // Opt-out: GATEWAY_RELAY_ALLOW_DIRECT_PLATFORMS=true keeps direct adapters
    // running beside the relay. Both reads go through the profile-scope-aware
    // getenv so multiplexed profiles see their own values.
    let allow_direct = is_truthy_str(&ge("GATEWAY_RELAY_ALLOW_DIRECT_PLATFORMS"));
    if !relay_url_env.is_empty() && !allow_direct {
        let non_messaging = [Platform::Local, Platform::ApiServer, Platform::Webhook];
        for (platform, platform_config) in config.platforms.iter_mut() {
            if *platform == Platform::Relay || non_messaging.contains(platform) {
                continue;
            }
            if !platform_config.enabled {
                continue;
            }
            if platform_config
                .extra
                .get("_enabled_explicit")
                .map(py_truthy)
                .unwrap_or(false)
            {
                tracing::warn!(
                    "Relay connector is configured via GATEWAY_RELAY_URL; \
disabling directly-connected platform '{}' even though it is explicitly \
enabled in this profile's configuration. All messaging goes through the \
connector on this deployment. Set GATEWAY_RELAY_ALLOW_DIRECT_PLATFORMS=true \
to keep direct platforms alongside the relay.",
                    platform.value(),
                );
            } else {
                tracing::info!(
                    "Relay connector is configured via GATEWAY_RELAY_URL; \
disabling directly-connected platform '{}'.",
                    platform.value(),
                );
            }
            platform_config.enabled = false;
        }
    }

    // Final cleanup: the `_enabled_explicit` marker is internal bookkeeping, so
    // scrub it from every platform once all passes are done.
    for platform_config in config.platforms.values_mut() {
        platform_config.extra.remove("_enabled_explicit");
    }
}

/// Convenience for reading a platform's `extra` map in tests / callers.
fn extra_of(config: &GatewayConfig, platform: Platform) -> Option<&Map<String, Value>> {
    config.platforms.get(&platform).map(|p| &p.extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env vars this suite touches. Cleared before every case so a leftover
    /// value from an earlier test cannot leak in.
    const MANAGED: &[&str] = &[
        "TELEGRAM_BOT_TOKEN",
        "TELEGRAM_REPLY_TO_MODE",
        "TELEGRAM_FALLBACK_IPS",
        "TELEGRAM_HOME_CHANNEL",
        "TELEGRAM_HOME_CHANNEL_NAME",
        "TELEGRAM_HOME_CHANNEL_THREAD_ID",
        "DISCORD_BOT_TOKEN",
        "WECOM_CALLBACK_CORP_ID",
        "WECOM_CALLBACK_CORP_SECRET",
        "WECOM_CALLBACK_PORT",
        "BLUEBUBBLES_SERVER_URL",
        "BLUEBUBBLES_PASSWORD",
        "GATEWAY_RELAY_URL",
        "GATEWAY_RELAY_ALLOW_DIRECT_PLATFORMS",
        "SESSION_IDLE_MINUTES",
    ];

    fn clear_managed() {
        for name in MANAGED {
            std::env::remove_var(name);
        }
    }

    fn set(name: &str, value: &str) {
        std::env::set_var(name, value);
    }

    fn extra(config: &GatewayConfig, platform: Platform) -> &Map<String, Value> {
        extra_of(config, platform).expect("platform present")
    }

    // Golden vectors below were captured from the real Python:
    //   python3 -c "... from gateway.config import GatewayConfig, \
    //   _apply_env_overrides; c = GatewayConfig(); _apply_env_overrides(c); \
    //   print(json.dumps(c.to_dict(), sort_keys=True, default=str))"

    #[test]
    fn token_enables_platform_and_stores_token() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        clear_managed();
        set("TELEGRAM_BOT_TOKEN", "t");

        let mut config = GatewayConfig::default();
        apply_env_overrides(&mut config);

        let telegram = config.platforms.get(&Platform::Telegram).unwrap();
        assert!(telegram.enabled);
        assert_eq!(telegram.token, Some(json!("t")));
        // Untouched default, matching Python's "reply_to_mode": "first".
        assert_eq!(telegram.reply_to_mode, json!("first"));
        clear_managed();
    }

    #[test]
    fn reply_to_mode_rejects_invalid_and_accepts_valid() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        clear_managed();
        set("TELEGRAM_BOT_TOKEN", "t");
        set("TELEGRAM_REPLY_TO_MODE", "Bogus");

        let mut config = GatewayConfig::default();
        apply_env_overrides(&mut config);
        // Python golden: reply_to_mode stays "first" for an unrecognized value.
        assert_eq!(
            config.platforms[&Platform::Telegram].reply_to_mode,
            json!("first")
        );

        // A mixed-case value is lowercased into the allowed set.
        set("TELEGRAM_REPLY_TO_MODE", "AlL");
        let mut config = GatewayConfig::default();
        apply_env_overrides(&mut config);
        assert_eq!(
            config.platforms[&Platform::Telegram].reply_to_mode,
            json!("all")
        );
        clear_managed();
    }

    #[test]
    fn comma_list_splits_strips_and_drops_empties() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        clear_managed();
        set("TELEGRAM_BOT_TOKEN", "t");
        set("TELEGRAM_FALLBACK_IPS", "1.1.1.1, ,2.2.2.2");

        let mut config = GatewayConfig::default();
        apply_env_overrides(&mut config);

        // Python golden: ["1.1.1.1", "2.2.2.2"].
        assert_eq!(
            extra(&config, Platform::Telegram)["fallback_ips"],
            json!(["1.1.1.1", "2.2.2.2"])
        );
        clear_managed();
    }

    #[test]
    fn int_override_and_defaults_match_python() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        clear_managed();
        set("WECOM_CALLBACK_CORP_ID", "c");
        set("WECOM_CALLBACK_CORP_SECRET", "s");
        set("WECOM_CALLBACK_PORT", "9999");
        set("BLUEBUBBLES_SERVER_URL", "http://x/");
        set("BLUEBUBBLES_PASSWORD", "p");

        let mut config = GatewayConfig::default();
        apply_env_overrides(&mut config);

        // Python golden for wecom_callback.extra.
        let wecom = extra(&config, Platform::WecomCallback);
        assert_eq!(wecom["corp_id"], json!("c"));
        assert_eq!(wecom["corp_secret"], json!("s"));
        assert_eq!(wecom["port"], json!(9999));
        assert_eq!(wecom["agent_id"], json!(""));
        assert_eq!(wecom["token"], json!(""));
        assert_eq!(wecom["encoding_aes_key"], json!(""));
        assert_eq!(wecom["host"], json!(""));

        // Python golden for bluebubbles.extra, including the always-written
        // require_mention: false and the rstrip("/") on server_url.
        let bb = extra(&config, Platform::Bluebubbles);
        assert_eq!(bb["server_url"], json!("http://x"));
        assert_eq!(bb["password"], json!("p"));
        assert_eq!(bb["webhook_host"], json!("127.0.0.1"));
        assert_eq!(bb["webhook_port"], json!(8645));
        assert_eq!(bb["webhook_path"], json!("/bluebubbles-webhook"));
        assert_eq!(bb["send_read_receipts"], json!(true));
        assert_eq!(bb["require_mention"], json!(false));
        assert!(!bb.contains_key("mention_patterns"));
        clear_managed();
    }

    #[test]
    fn session_idle_minutes_override() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        clear_managed();
        set("SESSION_IDLE_MINUTES", "45");

        let mut config = GatewayConfig::default();
        apply_env_overrides(&mut config);
        assert_eq!(config.default_reset_policy.idle_minutes, json!(45));

        // Non-numeric is swallowed (Python `except ValueError: pass`).
        let before = config.default_reset_policy.idle_minutes.clone();
        set("SESSION_IDLE_MINUTES", "nope");
        let mut config2 = GatewayConfig::default();
        apply_env_overrides(&mut config2);
        assert_ne!(config2.default_reset_policy.idle_minutes, json!(45));
        assert_eq!(
            config2.default_reset_policy.idle_minutes,
            GatewayConfig::default().default_reset_policy.idle_minutes
        );
        let _ = before;
        clear_managed();
    }

    #[test]
    fn home_channel_is_built_only_when_platform_present() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        clear_managed();
        // No token, so telegram is absent and the home block is skipped.
        set("TELEGRAM_HOME_CHANNEL", "-100");
        let mut config = GatewayConfig::default();
        apply_env_overrides(&mut config);
        assert!(!config.platforms.contains_key(&Platform::Telegram));

        // With a token, telegram exists and the home channel is built.
        set("TELEGRAM_BOT_TOKEN", "t");
        let mut config = GatewayConfig::default();
        apply_env_overrides(&mut config);
        let home = config.platforms[&Platform::Telegram]
            .home_channel
            .clone()
            .expect("home channel built");
        // Python golden: {"chat_id": "-100", "name": "Home", "platform": "telegram"}
        assert_eq!(home.platform, Platform::Telegram);
        assert_eq!(home.chat_id, "-100");
        assert_eq!(home.name, "Home");
        assert_eq!(home.thread_id, None);
        clear_managed();
    }

    #[test]
    fn explicit_disable_beats_env_and_marker_is_scrubbed() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        clear_managed();
        reset_explicit_disable_warned();
        set("TELEGRAM_BOT_TOKEN", "t");
        set("TELEGRAM_REPLY_TO_MODE", "AlL");

        let mut config = GatewayConfig::default();
        let mut disabled_extra = Map::new();
        disabled_extra.insert("_enabled_explicit".to_string(), json!(true));
        config.platforms.insert(
            Platform::Telegram,
            PlatformConfig {
                enabled: false,
                extra: disabled_extra.clone(),
                ..Default::default()
            },
        );
        let mut enabled_extra = Map::new();
        enabled_extra.insert("_enabled_explicit".to_string(), json!(true));
        config.platforms.insert(
            Platform::Discord,
            PlatformConfig {
                enabled: true,
                extra: enabled_extra,
                ..Default::default()
            },
        );

        apply_env_overrides(&mut config);

        // Python golden:
        // telegram -> enabled false, token "t", reply_to_mode "all", extra {}
        // discord  -> enabled true, extra {}
        let telegram = &config.platforms[&Platform::Telegram];
        assert!(!telegram.enabled, "explicit disable wins over env creds");
        assert_eq!(telegram.token, Some(json!("t")));
        assert_eq!(telegram.reply_to_mode, json!("all"));
        assert!(telegram.extra.is_empty(), "_enabled_explicit scrubbed");

        let discord = &config.platforms[&Platform::Discord];
        assert!(discord.enabled);
        assert!(discord.extra.is_empty(), "_enabled_explicit scrubbed");
        clear_managed();
    }

    #[test]
    fn relay_env_stamp_disables_direct_platforms() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        clear_managed();
        set("GATEWAY_RELAY_URL", "https://relay.example/");
        set("TELEGRAM_BOT_TOKEN", "t");

        let mut config = GatewayConfig::default();
        apply_env_overrides(&mut config);

        let relay = &config.platforms[&Platform::Relay];
        assert!(relay.enabled);
        // rstrip("/") applied.
        assert_eq!(relay.extra["relay_url"], json!("https://relay.example"));
        assert!(!config.platforms[&Platform::Telegram].enabled);

        // Opt-out keeps direct adapters alive.
        set("GATEWAY_RELAY_ALLOW_DIRECT_PLATFORMS", "true");
        let mut config = GatewayConfig::default();
        apply_env_overrides(&mut config);
        assert!(config.platforms[&Platform::Telegram].enabled);
        clear_managed();
    }
}
