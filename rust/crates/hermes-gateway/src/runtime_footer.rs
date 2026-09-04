//! Port of gateway/runtime_footer.py.
//!
// Public API is ahead of its callers (gateway/run.py's final-message path).
#![allow(dead_code)]
//!
//! Gateway runtime-metadata footer. Renders a compact footer showing runtime
//! state (model, context %, cwd, latency) and appends it to the FINAL message
//! of an agent turn when enabled. Off by default to keep replies minimal.
//!
//! Config (`~/.hermes/config.yaml`):
//! ```yaml
//! display:
//!   runtime_footer:
//!     enabled: true                     # off by default
//!     fields: [model, context_pct, cwd] # order shown; drop any to hide
//! ```
//!
//! Fields: `model` (bare id, vendor prefix dropped), `context_pct` (last-call
//! context occupancy), `latency` (turn wall-clock, opt-in), `cwd` (home-relative
//! working dir). Per-platform overrides live under
//! `display.platforms.<platform>.runtime_footer`.

use serde_json::Value;

const DEFAULT_FIELDS: [&str; 3] = ["model", "context_pct", "cwd"];
const SEP: &str = " \u{b7} "; // " · "

/// Effective footer config after merging defaults, global, and platform layers.
#[derive(Debug, Clone, PartialEq)]
pub struct FooterConfig {
    pub enabled: bool,
    pub fields: Vec<String>,
}

impl Default for FooterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fields: DEFAULT_FIELDS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Return `cwd` with `$HOME` collapsed to `~`. Empty string if unset.
fn home_relative_cwd(cwd: &str) -> String {
    if cwd.is_empty() {
        return String::new();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    // abspath: resolve relative to the process cwd like Python os.path.abspath.
    let p = std::path::Path::new(cwd);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    let p = abs.to_string_lossy().to_string();
    if !home.is_empty() {
        let sep = std::path::MAIN_SEPARATOR;
        if p == home {
            return "~".to_string();
        }
        let home_prefix = format!("{home}{sep}");
        if let Some(rest) = p.strip_prefix(&home_prefix) {
            return format!("~{sep}{rest}");
        }
    }
    p
}

/// Drop the `vendor/` prefix (`openai/gpt-5.4` -> `gpt-5.4`).
fn model_short(model: Option<&str>) -> String {
    match model {
        None => String::new(),
        Some(m) => m.rsplit('/').next().unwrap_or(m).to_string(),
    }
}

fn footer_dict(section: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    match section {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// Apply one `runtime_footer` layer (enabled / fields) onto `resolved`.
fn apply_layer(resolved: &mut FooterConfig, layer: &serde_json::Map<String, Value>) {
    if let Some(enabled) = layer.get("enabled") {
        resolved.enabled = truthy(enabled);
    }
    if let Some(Value::Array(fields)) = layer.get("fields") {
        if !fields.is_empty() {
            resolved.fields = fields.iter().map(value_to_field_string).collect();
        }
    }
}

/// Python `str(f)` for a field entry: strings verbatim, others stringified.
fn value_to_field_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => {
            // Python str(True) == "True".
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

/// Python truthiness for a JSON value in a boolean context (`bool(x)`).
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Resolve effective runtime-footer config for `platform_key`.
///
/// Merge order (later wins): built-in defaults (enabled=false),
/// `display.runtime_footer`, then
/// `display.platforms.<platform_key>.runtime_footer`.
pub fn resolve_footer_config(user_config: &Value, platform_key: Option<&str>) -> FooterConfig {
    let mut resolved = FooterConfig::default();
    let display = match user_config.get("display") {
        Some(Value::Object(m)) => m,
        _ => return resolved,
    };

    if let Some(global_cfg) = footer_dict(display.get("runtime_footer")) {
        apply_layer(&mut resolved, global_cfg);
    }

    if let Some(platform_key) = platform_key {
        if let Some(Value::Object(platforms)) = display.get("platforms") {
            if let Some(Value::Object(plat_cfg)) = platforms.get(platform_key) {
                if let Some(plat_footer) = footer_dict(plat_cfg.get("runtime_footer")) {
                    apply_layer(&mut resolved, plat_footer);
                }
            }
        }
    }

    resolved
}

/// Humanize a turn duration: `<1s`, `22s`, `1m05s`.
fn format_latency(seconds: f64) -> String {
    if seconds < 1.0 {
        return "<1s".to_string();
    }
    let total = seconds.round() as i64;
    if total < 60 {
        return format!("{total}s");
    }
    let m = total / 60;
    let sec = total % 60;
    format!("{m}m{sec:02}s")
}

/// Terminal working dir from the environment (`TERMINAL_CWD`).
fn env_terminal_cwd() -> String {
    std::env::var("TERMINAL_CWD").unwrap_or_default()
}

/// Render the footer line, or `""` if no fields have data.
///
/// Fields are skipped silently when their underlying data is missing: a
/// partially-populated footer beats a line with `?%` or empty slots.
#[allow(clippy::too_many_arguments)]
pub fn format_runtime_footer(
    model: Option<&str>,
    context_tokens: i64,
    context_length: Option<i64>,
    cwd: Option<&str>,
    turn_seconds: Option<f64>,
    fields: &[String],
) -> String {
    let mut parts: Vec<String> = Vec::new();
    for field in fields {
        match field.as_str() {
            "model" => {
                let m = model_short(model);
                if !m.is_empty() {
                    parts.push(m);
                }
            }
            "context_pct" => {
                if let Some(len) = context_length {
                    if len > 0 && context_tokens >= 0 {
                        let raw = (context_tokens as f64 / len as f64) * 100.0;
                        let pct = raw.round().clamp(0.0, 100.0) as i64;
                        parts.push(format!("{pct}%"));
                    }
                }
            }
            "latency" => {
                if let Some(ts) = turn_seconds {
                    if ts >= 0.0 {
                        parts.push(format_latency(ts));
                    }
                }
            }
            "cwd" => {
                let env_cwd = env_terminal_cwd();
                let source = match cwd {
                    Some(c) if !c.is_empty() => c,
                    _ => &env_cwd,
                };
                let rel = home_relative_cwd(source);
                if !rel.is_empty() {
                    parts.push(rel);
                }
            }
            _ => {} // Unknown field names are silently ignored.
        }
    }

    if parts.is_empty() {
        String::new()
    } else {
        parts.join(SEP)
    }
}

/// Top-level entry point used by the final-message path. Returns the footer
/// text (empty when disabled or no data). Callers append it to the final
/// response themselves.
#[allow(clippy::too_many_arguments)]
pub fn build_footer_line(
    user_config: &Value,
    platform_key: Option<&str>,
    model: Option<&str>,
    context_tokens: i64,
    context_length: Option<i64>,
    cwd: Option<&str>,
    turn_seconds: Option<f64>,
) -> String {
    let cfg = resolve_footer_config(user_config, platform_key);
    if !cfg.enabled {
        return String::new();
    }
    let fields = if cfg.fields.is_empty() {
        DEFAULT_FIELDS.iter().map(|s| s.to_string()).collect()
    } else {
        cfg.fields
    };
    format_runtime_footer(
        model,
        context_tokens,
        context_length,
        cwd,
        turn_seconds,
        &fields,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn model_short_drops_vendor() {
        assert_eq!(model_short(Some("openai/gpt-5.4")), "gpt-5.4");
        assert_eq!(model_short(Some("gpt-5.4")), "gpt-5.4");
        assert_eq!(model_short(None), "");
    }

    #[test]
    fn latency_humanizes() {
        assert_eq!(format_latency(0.4), "<1s");
        assert_eq!(format_latency(22.0), "22s");
        assert_eq!(format_latency(65.0), "1m05s");
        assert_eq!(format_latency(600.0), "10m00s");
    }

    #[test]
    fn context_pct_clamped_and_rounded() {
        let f = format_runtime_footer(None, 50, Some(200), None, None, &fields(&["context_pct"]));
        assert_eq!(f, "25%");
        // Over 100% clamps.
        let f2 = format_runtime_footer(None, 500, Some(200), None, None, &fields(&["context_pct"]));
        assert_eq!(f2, "100%");
        // Missing context length -> field skipped -> empty footer.
        let f3 = format_runtime_footer(None, 50, None, None, None, &fields(&["context_pct"]));
        assert_eq!(f3, "");
    }

    #[test]
    fn footer_joins_with_separator() {
        let f = format_runtime_footer(
            Some("openai/gpt-5.4"),
            50,
            Some(200),
            None,
            Some(22.0),
            &fields(&["model", "context_pct", "latency"]),
        );
        assert_eq!(f, "gpt-5.4 \u{b7} 25% \u{b7} 22s");
    }

    #[test]
    fn home_relative_collapses_home() {
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert_eq!(home_relative_cwd(&home), "~");
            let sub = format!("{}/projects/x", home);
            let rel = home_relative_cwd(&sub);
            assert!(rel.starts_with("~/projects/x") || rel.starts_with("~\\projects\\x"));
        }
    }

    #[test]
    fn resolve_merges_global_then_platform() {
        let cfg = serde_json::json!({
            "display": {
                "runtime_footer": {"enabled": true, "fields": ["model"]},
                "platforms": {
                    "telegram": {"runtime_footer": {"fields": ["model", "cwd"]}},
                    "discord": {"runtime_footer": {"enabled": false}}
                }
            }
        });
        // Global only.
        let g = resolve_footer_config(&cfg, None);
        assert!(g.enabled);
        assert_eq!(g.fields, fields(&["model"]));
        // Platform overrides fields, inherits enabled.
        let t = resolve_footer_config(&cfg, Some("telegram"));
        assert!(t.enabled);
        assert_eq!(t.fields, fields(&["model", "cwd"]));
        // Platform disables, keeps global fields.
        let d = resolve_footer_config(&cfg, Some("discord"));
        assert!(!d.enabled);
        assert_eq!(d.fields, fields(&["model"]));
    }

    #[test]
    fn disabled_returns_empty() {
        let cfg = serde_json::json!({"display": {"runtime_footer": {"enabled": false}}});
        assert_eq!(
            build_footer_line(&cfg, None, Some("m"), 0, Some(100), None, None),
            ""
        );
    }

    #[test]
    fn enabled_builds_line() {
        let cfg = serde_json::json!({
            "display": {"runtime_footer": {"enabled": true, "fields": ["model", "context_pct"]}}
        });
        let line = build_footer_line(
            &cfg,
            None,
            Some("anthropic/claude"),
            10,
            Some(100),
            None,
            None,
        );
        assert_eq!(line, "claude \u{b7} 10%");
    }
}
