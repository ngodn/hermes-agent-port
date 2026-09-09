//! Pure automatic compression policy and decision logic for conversation compaction.
//!
//! Authoritative reference:
//! - `agent/agent_init.py` (configuration parsing and defaults)
//! - `agent/context_compressor.py` (`resolve_model_threshold`, `_compute_threshold_tokens`, `should_compress_info`)
//! - `rust/analysis/automatic-compression-map-agy.md`

use serde_json::Value;

/// Default master switch for automatic compression.
pub const DEFAULT_ENABLED: bool = true;

/// Default context fraction threshold at which automatic compression triggers (50%).
pub const DEFAULT_THRESHOLD: f64 = 0.50;

/// Default count of most recent messages spared from summarization.
pub const DEFAULT_PROTECT_LAST_N: usize = 20;

/// Default count of initial non-system messages preserved at transcript head.
pub const DEFAULT_PROTECT_FIRST_N: usize = 3;
/// Default count of real actionable user messages guaranteed to survive in the tail.
pub const DEFAULT_MIN_TAIL_USER_MESSAGES: usize = 1;
pub const DEFAULT_TARGET_RATIO: f64 = 0.20;
pub const DEFAULT_TAIL_MODE: &str = "lean";
pub const LEAN_TAIL_FLOOR_TOKENS: u64 = 10_000;
pub const LEAN_TAIL_CAP_TOKENS: u64 = 25_000;

/// Default per-turn cap on compression retry passes.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
pub const DEFAULT_PROACTIVE_PRUNE_TOKENS: u64 = 0;
pub const DEFAULT_PROACTIVE_PRUNE_MIN_RESULT_CHARS: usize = 8_000;
pub const DEFAULT_PROACTIVE_PRUNE_MIN_RECLAIM_TOKENS: u64 = 4_096;
pub const DEFAULT_MICRO_COMPACT: bool = false;
pub const DEFAULT_MICRO_COMPACT_EVERY_N_TURNS: usize = 1;
pub const DEFAULT_MICRO_COMPACT_DEFRAG_THRESHOLD_TOKENS: u64 = 2_000;
pub const STRUCTURAL_NO_OP_BACKOFF: std::time::Duration = std::time::Duration::from_secs(300);

/// Minimum valid value for `max_attempts`.
pub const MIN_MAX_ATTEMPTS: u32 = 1;

/// Maximum allowed value for `max_attempts`.
pub const MAX_MAX_ATTEMPTS: u32 = 10;

pub const MINIMUM_CONTEXT_LENGTH: u64 = 64_000;
pub const SMALL_CONTEXT_WINDOW_LIMIT: u64 = 512_000;
pub const SMALL_CONTEXT_THRESHOLD: f64 = 0.75;
pub const MIN_CONTEXT_TRIGGER_RATIO: f64 = 0.85;

/// Automatic compression policy parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct AutomaticCompressionPolicy {
    /// Master switch. When false, automatic compression never triggers.
    pub enabled: bool,
    /// Fractional fill of context window at which compaction triggers.
    pub threshold: f64,
    /// Optional absolute token cap. When set and positive, triggers at the lower
    /// of ratio tokens and this absolute token count.
    pub threshold_tokens: Option<u64>,
    /// Per-model threshold overrides. Keys are substring-matched against the model name.
    /// Insertion order is preserved for deterministic tie-breaking.
    pub model_thresholds: Vec<(String, f64)>,
    /// Number of most recent messages unconditionally spared from summarization.
    pub protect_last_n: usize,
    /// Minimum real actionable user messages guaranteed to survive in uncompressed tail.
    pub min_tail_user_messages: usize,
    /// Number of initial non-system messages preserved at transcript head.
    pub protect_first_n: usize,
    /// Fraction of the trigger threshold retained by legacy tail mode.
    pub target_ratio: f64,
    /// Verbatim tail sizing strategy, either `lean` or `legacy`.
    pub tail_mode: String,
    /// Per-turn cap on compression retry passes. Clamped to [1, 10].
    pub max_attempts: u32,
    /// Keep compression in the current durable conversation. Active tool-loop
    /// compression requires this because its client identity is immutable.
    pub in_place: bool,
    /// Refuse full compression unless the active memory provider completes the
    /// versioned pre-compress checkpoint boundary.
    pub checkpoint_required: bool,
    /// Opt-in request-pressure trigger for deterministic tool-result pruning.
    /// Zero disables the path.
    pub proactive_prune_tokens: u64,
    /// Minimum tool-result size eligible for the lossy summary pass.
    pub proactive_prune_min_result_chars: usize,
    /// Minimum estimated savings required before a cache-breaking prune commits.
    pub proactive_prune_min_reclaim_tokens: u64,
    /// Opt-in rolling post-turn compaction. Disabled when checkpoint-required
    /// policy is armed because this path has no memory checkpoint hook yet.
    pub micro_compact: bool,
    /// Completed-turn cadence between rolling compaction attempts.
    pub micro_compact_every_n_turns: usize,
    /// Rolling-summary size that makes the next pass rewrite the summary
    /// instead of absorbing another exchange.
    pub micro_compact_defrag_threshold_tokens: u64,
}

impl Default for AutomaticCompressionPolicy {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ENABLED,
            threshold: DEFAULT_THRESHOLD,
            threshold_tokens: None,
            model_thresholds: Vec::new(),
            protect_last_n: DEFAULT_PROTECT_LAST_N,
            min_tail_user_messages: DEFAULT_MIN_TAIL_USER_MESSAGES,
            protect_first_n: DEFAULT_PROTECT_FIRST_N,
            target_ratio: DEFAULT_TARGET_RATIO,
            tail_mode: DEFAULT_TAIL_MODE.into(),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            in_place: true,
            checkpoint_required: false,
            proactive_prune_tokens: DEFAULT_PROACTIVE_PRUNE_TOKENS,
            proactive_prune_min_result_chars: DEFAULT_PROACTIVE_PRUNE_MIN_RESULT_CHARS,
            proactive_prune_min_reclaim_tokens: DEFAULT_PROACTIVE_PRUNE_MIN_RECLAIM_TOKENS,
            micro_compact: DEFAULT_MICRO_COMPACT,
            micro_compact_every_n_turns: DEFAULT_MICRO_COMPACT_EVERY_N_TURNS,
            micro_compact_defrag_threshold_tokens: DEFAULT_MICRO_COMPACT_DEFRAG_THRESHOLD_TOKENS,
        }
    }
}

/// High-level decision outcomes for automatic compression evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionDecision {
    /// Compression is disabled in configuration.
    Disabled,
    /// Current token pressure is below the effective threshold.
    BelowThreshold {
        pressure_tokens: u64,
        effective_threshold: u64,
    },
    /// Compression retry attempt budget for the current turn has been exhausted.
    AttemptsExhausted {
        attempts_used: u32,
        max_attempts: u32,
    },
    /// Compression is needed (over threshold) but blocked by an external condition
    /// such as an active LLM cooldown or circuit breaker.
    ExternallyBlocked,
    /// Compression should be attempted.
    Attempt {
        attempt_number: u32,
        max_attempts: u32,
        effective_threshold: u64,
    },
}

impl CompressionDecision {
    /// Return true if the decision permits initiating a compression pass.
    #[must_use]
    pub fn should_compress(&self) -> bool {
        matches!(self, Self::Attempt { .. })
    }

    /// Return true if compression is globally disabled.
    #[cfg(test)]
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled)
    }
}

impl AutomaticCompressionPolicy {
    /// Parse configuration from a `serde_json::Value` with conservative coercion.
    ///
    /// Accepts either a root configuration containing a `"compression"` object,
    /// or directly the `"compression"` object itself.
    pub fn from_value(value: &Value) -> Self {
        let section = match value {
            Value::Object(map) => {
                if let Some(comp) = map.get("compression") {
                    match comp {
                        Value::Object(_) => comp,
                        _ => return Self::default(),
                    }
                } else {
                    value
                }
            }
            _ => return Self::default(),
        };

        let map = match section {
            Value::Object(m) => m,
            _ => return Self::default(),
        };

        let enabled = parse_enabled(map.get("enabled"), DEFAULT_ENABLED);
        let threshold = parse_threshold(map.get("threshold"), DEFAULT_THRESHOLD);
        let threshold_tokens = parse_threshold_tokens(map.get("threshold_tokens"));
        let model_thresholds = parse_model_thresholds(map.get("model_thresholds"));
        let protect_last_n = parse_protect_count(map.get("protect_last_n"), DEFAULT_PROTECT_LAST_N);
        let min_tail_user_messages = parse_min_tail_user_messages(
            map.get("min_tail_user_messages"),
            DEFAULT_MIN_TAIL_USER_MESSAGES,
        );
        let protect_first_n =
            parse_protect_count(map.get("protect_first_n"), DEFAULT_PROTECT_FIRST_N);
        let target_ratio =
            parse_threshold(map.get("target_ratio"), DEFAULT_TARGET_RATIO).clamp(0.10, 0.80);
        let tail_mode = map
            .get("tail_mode")
            .and_then(Value::as_str)
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .filter(|mode| matches!(mode.as_str(), "lean" | "legacy"))
            .unwrap_or_else(|| DEFAULT_TAIL_MODE.into());
        let max_attempts = parse_max_attempts(map.get("max_attempts"), DEFAULT_MAX_ATTEMPTS);
        let in_place = parse_truthy_value(map.get("in_place"), true);
        let proactive_prune_tokens = u64::try_from(
            parse_prune_integer(
                map.get("proactive_prune_tokens"),
                DEFAULT_PROACTIVE_PRUNE_TOKENS as i128,
            )
            .max(0),
        )
        .unwrap_or(u64::MAX);
        let raw_min_chars = parse_prune_integer(
            map.get("proactive_prune_min_result_chars"),
            DEFAULT_PROACTIVE_PRUNE_MIN_RESULT_CHARS as i128,
        );
        let proactive_prune_min_result_chars = if raw_min_chars == 0 {
            DEFAULT_PROACTIVE_PRUNE_MIN_RESULT_CHARS
        } else {
            usize::try_from(raw_min_chars.max(crate::tool_result_prune::PRUNE_MIN_CHARS as i128))
                .unwrap_or(usize::MAX)
        };
        let proactive_prune_min_reclaim_tokens = u64::try_from(
            parse_prune_integer(
                map.get("proactive_prune_min_reclaim_tokens"),
                DEFAULT_PROACTIVE_PRUNE_MIN_RECLAIM_TOKENS as i128,
            )
            .max(0),
        )
        .unwrap_or(u64::MAX);
        let checkpoint_required = parse_truthy_value(map.get("checkpoint_required"), false);
        let micro_compact = parse_truthy_value(map.get("micro_compact"), DEFAULT_MICRO_COMPACT)
            && !checkpoint_required;
        let micro_compact_every_n_turns = usize::try_from(
            parse_prune_integer(
                map.get("micro_compact_every_n_turns"),
                DEFAULT_MICRO_COMPACT_EVERY_N_TURNS as i128,
            )
            .max(1),
        )
        .unwrap_or(usize::MAX);
        let micro_compact_defrag_threshold_tokens = u64::try_from(
            parse_prune_integer(
                map.get("micro_compact_defrag_threshold_tokens"),
                DEFAULT_MICRO_COMPACT_DEFRAG_THRESHOLD_TOKENS as i128,
            )
            .max(1),
        )
        .unwrap_or(u64::MAX);

        Self {
            enabled,
            threshold,
            threshold_tokens,
            model_thresholds,
            protect_last_n,
            min_tail_user_messages,
            protect_first_n,
            target_ratio,
            tail_mode,
            max_attempts,
            in_place,
            checkpoint_required,
            proactive_prune_tokens,
            proactive_prune_min_result_chars,
            proactive_prune_min_reclaim_tokens,
            micro_compact,
            micro_compact_every_n_turns,
            micro_compact_defrag_threshold_tokens,
        }
    }

    /// Resolve effective ratio for a given model name against configured overrides.
    #[must_use]
    pub fn resolve_ratio(&self, model: &str) -> f64 {
        resolve_model_threshold(model, &self.model_thresholds, self.threshold)
    }

    #[must_use]
    pub fn compute_effective_threshold_with_output_for(
        &self,
        context_length: u64,
        max_output_tokens: Option<u64>,
        model: Option<&str>,
    ) -> u64 {
        let ratio = self.resolve_ratio(model.unwrap_or_default());
        compute_effective_threshold_with_output(
            context_length,
            ratio,
            max_output_tokens,
            self.threshold_tokens,
        )
    }

    /// Resolve the token budget for the verbatim tail retained by full
    /// compression. Mirrors `ContextCompressor.tail_token_budget`.
    #[must_use]
    pub fn tail_token_budget(&self, context_length: u64, effective_threshold: u64) -> u64 {
        if self.tail_mode == "legacy" {
            ((effective_threshold as f64) * self.target_ratio).floor() as u64
        } else {
            ((context_length as f64) * 0.025)
                .floor()
                .clamp(LEAN_TAIL_FLOOR_TOKENS as f64, LEAN_TAIL_CAP_TOKENS as f64)
                as u64
        }
    }

    /// Pure decision evaluation given known effective threshold, pressure tokens, attempts, and blocked flag.
    #[must_use]
    pub fn decide(
        &self,
        effective_threshold: u64,
        pressure_tokens: u64,
        attempts_used: u32,
        blocked: bool,
    ) -> CompressionDecision {
        if !self.enabled {
            return CompressionDecision::Disabled;
        }

        if effective_threshold == 0 || pressure_tokens < effective_threshold {
            return CompressionDecision::BelowThreshold {
                pressure_tokens,
                effective_threshold,
            };
        }

        if attempts_used >= self.max_attempts {
            return CompressionDecision::AttemptsExhausted {
                attempts_used,
                max_attempts: self.max_attempts,
            };
        }

        if blocked {
            return CompressionDecision::ExternallyBlocked;
        }

        CompressionDecision::Attempt {
            attempt_number: attempts_used + 1,
            max_attempts: self.max_attempts,
            effective_threshold,
        }
    }
}

impl From<&Value> for AutomaticCompressionPolicy {
    fn from(value: &Value) -> Self {
        Self::from_value(value)
    }
}

impl From<Value> for AutomaticCompressionPolicy {
    fn from(value: Value) -> Self {
        Self::from_value(&value)
    }
}

/// Resolve the effective compression threshold ratio for a model name.
///
/// Overrides match when the configured key is a substring of the target model name.
/// The longest matching substring wins.
///
/// Deterministic tie behavior:
/// When multiple matching keys have identical length, the first matching key
/// in configuration order (insertion order) wins. This matches Python dict
/// iteration behavior exactly.
#[must_use]
pub fn resolve_model_threshold(
    model: &str,
    model_thresholds: &[(String, f64)],
    default_ratio: f64,
) -> f64 {
    if model.is_empty() || model_thresholds.is_empty() {
        return default_ratio;
    }

    let mut best_key: Option<&str> = None;
    let mut best_len: usize = 0;

    for (key, _) in model_thresholds {
        if !key.is_empty() && model.contains(key.as_str()) && key.len() > best_len {
            best_key = Some(key.as_str());
            best_len = key.len();
        }
    }

    if let Some(target) = best_key {
        for (key, val) in model_thresholds {
            if key == target {
                return *val;
            }
        }
    }

    default_ratio
}

/// Compute the effective token threshold from context length, ratio, and optional absolute cap.
///
/// Effective threshold is min(context_length * ratio, absolute_threshold when configured).
/// Safely handles zero and invalid inputs:
/// - Zero context length returns 0.
/// - NaN ratios return 0.
/// - Float overflow saturates at u64::MAX.
/// - Absolute threshold is ignored if zero or None.
#[must_use]
#[cfg(test)]
pub fn compute_effective_threshold(
    context_length: u64,
    ratio: f64,
    threshold_tokens: Option<u64>,
) -> u64 {
    compute_effective_threshold_with_output(context_length, ratio, None, threshold_tokens)
}

#[must_use]
pub fn compute_effective_threshold_with_output(
    context_length: u64,
    ratio: f64,
    max_output_tokens: Option<u64>,
    threshold_tokens: Option<u64>,
) -> u64 {
    if context_length == 0 || !ratio.is_finite() {
        return 0;
    }
    let effective_window = max_output_tokens
        .filter(|reserved| *reserved < context_length)
        .map_or(context_length, |reserved| context_length - reserved);
    let ratio = if context_length < SMALL_CONTEXT_WINDOW_LIMIT {
        ratio.max(SMALL_CONTEXT_THRESHOLD)
    } else {
        ratio
    };
    let scaled = |fraction: f64| -> u64 {
        let value = (effective_window as f64) * fraction;
        if value <= 0.0 {
            0
        } else if value >= u64::MAX as f64 {
            u64::MAX
        } else {
            value.floor() as u64
        }
    };
    let percentage = scaled(ratio);
    let trigger_cap = scaled(MIN_CONTEXT_TRIGGER_RATIO);
    let mut effective = percentage.max(MINIMUM_CONTEXT_LENGTH);
    if effective > percentage && effective > trigger_cap {
        effective = percentage.max(trigger_cap);
    }
    if effective >= effective_window {
        effective = trigger_cap.min(effective_window.saturating_sub(1)).max(1);
    }
    if let Some(cap) = threshold_tokens.filter(|cap| *cap > 0) {
        effective = effective.min(cap.min(context_length));
    }
    effective
}

// ===========================================================================
// Conservative coercion helpers matching Python semantics
// ===========================================================================

fn parse_enabled(raw: Option<&Value>, default: bool) -> bool {
    match raw {
        None | Some(Value::Null) => default,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.to_string() == "1",
        Some(Value::String(s)) => {
            matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes")
        }
        _ => false,
    }
}

fn parse_truthy_value(raw: Option<&Value>, default: bool) -> bool {
    match raw {
        None | Some(Value::Null) => default,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

fn parse_threshold(raw: Option<&Value>, default: f64) -> f64 {
    match raw {
        None | Some(Value::Null) => default,
        Some(Value::Bool(value)) => f64::from(*value),
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()).unwrap_or(default),
        Some(Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .unwrap_or(default),
        _ => default,
    }
}

fn parse_threshold_tokens(raw: Option<&Value>) -> Option<u64> {
    match raw {
        None | Some(Value::Null) => None,
        Some(Value::Bool(value)) => (*value).then_some(1),
        Some(Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                if u > 0 {
                    Some(u)
                } else {
                    None
                }
            } else if let Some(i) = n.as_i64() {
                if i > 0 {
                    Some(i as u64)
                } else {
                    None
                }
            } else if let Some(f) = n.as_f64() {
                (f.is_finite() && f >= 1.0 && f <= u64::MAX as f64).then_some(f as u64)
            } else {
                None
            }
        }
        Some(Value::String(s)) => match s.trim().parse::<i64>() {
            Ok(i) if i > 0 => Some(i as u64),
            _ => None,
        },
        _ => None,
    }
}

fn parse_model_thresholds(raw: Option<&Value>) -> Vec<(String, f64)> {
    let mut result = Vec::new();
    if let Some(Value::Object(map)) = raw {
        for (k, v) in map {
            let ratio_opt = match v {
                Value::Bool(_) => None,
                Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
                _ => None,
            };
            if let Some(ratio) = ratio_opt {
                result.push((k.clone(), ratio));
            }
        }
    }
    result
}

fn parse_protect_count(raw: Option<&Value>, default: usize) -> usize {
    match raw {
        None | Some(Value::Null) => default,
        Some(Value::Bool(value)) => usize::from(*value),
        Some(Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                u as usize
            } else if let Some(i) = n.as_i64() {
                if i < 0 {
                    0
                } else {
                    i as usize
                }
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() {
                    if f < 0.0 {
                        0
                    } else {
                        f as usize
                    }
                } else {
                    default
                }
            } else {
                default
            }
        }
        Some(Value::String(s)) => match s.trim().parse::<i64>() {
            Ok(i) => {
                if i < 0 {
                    0
                } else {
                    i as usize
                }
            }
            Err(_) => default,
        },
        _ => default,
    }
}

fn parse_min_tail_user_messages(raw: Option<&Value>, default: usize) -> usize {
    let fallback = default.max(1);
    let parsed = match raw {
        None | Some(Value::Null) => return fallback,
        Some(Value::Bool(_)) => 1,
        Some(Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                u as usize
            } else if let Some(i) = n.as_i64() {
                if i < 1 {
                    1
                } else {
                    i as usize
                }
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() && f.fract() == 0.0 {
                    if f < 1.0 {
                        1
                    } else {
                        f as usize
                    }
                } else {
                    1
                }
            } else {
                1
            }
        }
        Some(Value::String(s)) => match s.trim().parse::<i64>() {
            Ok(i) => {
                if i < 1 {
                    1
                } else {
                    i as usize
                }
            }
            Err(_) => 1,
        },
        _ => 1,
    };
    parsed.max(1)
}

fn parse_max_attempts(raw: Option<&Value>, default: u32) -> u32 {
    let parsed = match raw {
        None | Some(Value::Null) | Some(Value::Bool(_)) => default as i64,
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i
            } else if let Some(u) = n.as_u64() {
                u as i64
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() && f.fract() == 0.0 {
                    f as i64
                } else {
                    default as i64
                }
            } else {
                default as i64
            }
        }
        Some(Value::String(s)) => match s.trim().parse::<i64>() {
            Ok(i) => i,
            Err(_) => default as i64,
        },
        _ => default as i64,
    };

    if parsed < (MIN_MAX_ATTEMPTS as i64) {
        default
    } else {
        parsed.min(MAX_MAX_ATTEMPTS as i64) as u32
    }
}

fn parse_prune_integer(raw: Option<&Value>, default: i128) -> i128 {
    match raw {
        None | Some(Value::Null) | Some(Value::Bool(_)) => default,
        Some(Value::Number(number)) => {
            if let Some(value) = number.as_i64() {
                i128::from(value)
            } else if let Some(value) = number.as_u64() {
                i128::from(value)
            } else if let Some(value) = number.as_f64() {
                if value.is_finite() && value.fract() == 0.0 {
                    value as i128
                } else {
                    default
                }
            } else {
                default
            }
        }
        Some(Value::String(value)) => value.trim().parse().unwrap_or(default),
        Some(Value::Array(_) | Value::Object(_)) => default,
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

fn complete_region(messages: &[crate::session_db::CompressionHistoryMessage]) -> bool {
    let rows = messages
        .iter()
        .map(|item| {
            (
                item.message.role.clone(),
                item.tool_calls.clone(),
                item.tool_call_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    crate::session_db::complete_turn_sequence(&rows)
}

fn compression_boundaries(
    messages: &[crate::session_db::CompressionHistoryMessage],
    protect_first_n: usize,
    protect_last_n: usize,
    min_tail_user_messages: usize,
    tail_token_budget: u64,
    charge_all_thinking: bool,
) -> Option<(usize, usize)> {
    compression_boundaries_with_tail(
        messages,
        protect_first_n,
        protect_last_n,
        min_tail_user_messages,
        tail_token_budget,
        charge_all_thinking,
        false,
    )
}

pub(crate) fn compression_boundaries_after_tool_batch(
    messages: &[crate::session_db::CompressionHistoryMessage],
    protect_first_n: usize,
    protect_last_n: usize,
    min_tail_user_messages: usize,
    tail_token_budget: u64,
    charge_all_thinking: bool,
) -> Option<(usize, usize)> {
    compression_boundaries_with_tail(
        messages,
        protect_first_n,
        protect_last_n,
        min_tail_user_messages,
        tail_token_budget,
        charge_all_thinking,
        true,
    )
}

fn compression_boundaries_with_tail(
    messages: &[crate::session_db::CompressionHistoryMessage],
    protect_first_n: usize,
    protect_last_n: usize,
    min_tail_user_messages: usize,
    tail_token_budget: u64,
    charge_all_thinking: bool,
    allow_completed_tool_batch: bool,
) -> Option<(usize, usize)> {
    // Python needs only a complete protected head plus three tail messages
    // and one compressible row. The token walk, not the configured count,
    // decides how much recent history survives.
    if messages.len() <= protect_first_n.saturating_add(4) {
        return None;
    }
    let prefix_limit = protect_first_n.min(messages.len());
    let prefix_end = (0..=prefix_limit)
        .rev()
        .find(|end| complete_region(&messages[..*end]))?;
    let initial_tail = crate::tool_result_prune::find_tail_cut_by_tokens(
        messages,
        prefix_end,
        protect_last_n,
        tail_token_budget,
        charge_all_thinking,
        min_tail_user_messages,
    );
    if initial_tail <= prefix_end {
        return None;
    }
    let tail_start = (prefix_end + 1..=initial_tail).rev().find(|start| {
        if allow_completed_tool_batch {
            let rows = messages[*start..]
                .iter()
                .map(|item| {
                    (
                        item.message.role.clone(),
                        item.tool_calls.clone(),
                        item.tool_call_id.clone(),
                    )
                })
                .collect::<Vec<_>>();
            crate::session_db::prunable_turn_sequence(&rows)
        } else {
            complete_region(&messages[*start..])
        }
    })?;
    (prefix_end < tail_start).then_some((prefix_end, tail_start))
}

pub(crate) fn structured_chars(messages: &[crate::session_db::CompressionHistoryMessage]) -> usize {
    messages
        .iter()
        .map(|item| {
            serde_json::to_string(&item.message.model_content()).map_or(0, |value| value.len())
                + item.message.api_content.as_deref().map_or(0, str::len)
                + item.tool_calls.as_deref().map_or(0, str::len)
                + item.tool_call_id.as_deref().map_or(0, str::len)
                + item.tool_name.as_deref().map_or(0, str::len)
        })
        .sum()
}

pub(crate) fn estimate_history_tokens(
    messages: &[crate::session_db::CompressionHistoryMessage],
) -> u64 {
    u64::try_from(
        structured_chars(messages)
            .saturating_add(messages.len().saturating_mul(40))
            .saturating_add(3)
            / 4,
    )
    .unwrap_or(u64::MAX)
}

/// Run automatic maintenance while admission still owns the route,
/// transcript and durable lineage leases. The inbound message is measured but
/// is not persisted until this function returns.
pub async fn compress_before_turn(
    deps: &crate::session_admission::AdmissionDeps,
    agent: &std::sync::Arc<dyn crate::agent::AgentClient>,
    source: &crate::session::SessionSource,
    message: &mut hermes_core::Message,
    user_config: &Value,
    admitted: &mut crate::session_admission::AdmittedSession,
) -> anyhow::Result<u32> {
    let policy = AutomaticCompressionPolicy::from_value(user_config);
    if !policy.enabled || admitted.database.is_none() || admitted.route_lease.is_none() {
        return Ok(0);
    }
    let database = admitted.database.as_ref().expect("checked").clone();
    let in_place = policy.in_place;
    let mut attempts = 0;
    // Once an over-threshold pass has committed its deterministic Phase 1,
    // finish that same compression attempt even if pruning alone drops the
    // reloaded request below the trigger. Python does not re-run should_compress
    // between Phase 1 and summary selection.
    let mut phase_one_committed = false;

    while attempts < policy.max_attempts {
        let session_id = admitted.entry.session_id.clone();
        let context = crate::agent::TurnContext::from_database(Some(&database))
            .with_session_finalizable(admitted.finalizable);
        let read_database = database.clone();
        let read_id = session_id.clone();
        let (snapshot, has_checkpoint, guard, prune_rearm) =
            tokio::task::spawn_blocking(move || {
                Ok::<_, rusqlite::Error>((
                    read_database.load_compression_snapshot(&read_id)?,
                    read_database.has_compression_checkpoint(&read_id)?,
                    read_database.load_compression_guard_state(&read_id)?,
                    read_database.proactive_prune_rearm_tokens(&read_id)?,
                ))
            })
            .await
            .map_err(|error| anyhow::anyhow!("automatic compression reader failed: {error}"))??;
        let history = snapshot
            .messages
            .iter()
            .map(|item| item.message.clone())
            .collect::<Vec<_>>();
        let Some(preflight) = agent
            .compression_preflight(context, message, &history)
            .await?
        else {
            return Ok(attempts);
        };
        let threshold = policy.compute_effective_threshold_with_output_for(
            preflight.context_length,
            preflight.max_output_tokens,
            Some(&preflight.model),
        );
        let protect_first = if has_checkpoint {
            0
        } else {
            policy.protect_first_n
        };

        // The Python loop runs this deterministic pass after tool results and
        // before its next provider call. Native ingress currently performs the
        // same maintenance at the next admitted request boundary, while the
        // transcript and route leases are still held. It remains opt-in.
        if policy.proactive_prune_tokens > 0
            && preflight.request_tokens >= policy.proactive_prune_tokens
            && snapshot.messages.len()
                > protect_first
                    .saturating_add(policy.protect_last_n)
                    .saturating_add(1)
        {
            let before_tokens = estimate_history_tokens(&snapshot.messages);
            let rearm_open = before_tokens >= prune_rearm || preflight.request_tokens >= threshold;
            if rearm_open {
                let candidate = crate::tool_result_prune::prune_old_tool_results(
                    &snapshot.messages,
                    policy.protect_last_n,
                    policy.proactive_prune_min_result_chars,
                );
                let reclaimed_tokens =
                    u64::try_from(candidate.reclaimed_chars.saturating_add(3) / 4)
                        .unwrap_or(u64::MAX);
                if candidate.changed
                    && candidate.pruned_count > 0
                    && reclaimed_tokens >= policy.proactive_prune_min_reclaim_tokens
                {
                    let after_tokens = before_tokens.saturating_sub(reclaimed_tokens);
                    let runway = reclaimed_tokens
                        .max(policy.proactive_prune_tokens)
                        .max(policy.proactive_prune_min_reclaim_tokens);
                    let next_rearm = after_tokens.saturating_add(runway);
                    let holder = admitted.durable_lease.as_ref().map(|lease| lease.holder());
                    let committed = deps.store.publish_tool_prune(
                        source,
                        &admitted.entry,
                        &snapshot.messages,
                        &candidate.messages,
                        next_rearm,
                        holder,
                    )?;
                    if !committed {
                        return Ok(attempts);
                    }
                    tracing::info!(
                        %session_id,
                        pruned = candidate.pruned_count,
                        reclaimed_tokens,
                        next_rearm,
                        "proactive native tool-result prune committed"
                    );
                    // Reload the fresh row ids and size the exact rewritten
                    // request before deciding whether an LLM summary is also needed.
                    continue;
                }
            }
        }
        let now = now_secs();
        let cooldown_active = guard.cooldown_until.is_some_and(|deadline| deadline > now);
        let breaker_active = guard.ineffective_count >= 2 && guard.recovery_deadline > now;
        let structural_remaining =
            agent.compression_structural_backoff_remaining(context, &session_id);
        let blocked = cooldown_active || structural_remaining.is_some() || breaker_active;
        if !phase_one_committed
            && !policy
                .decide(threshold, preflight.request_tokens, attempts, blocked)
                .should_compress()
        {
            if !cooldown_active {
                if let Some(remaining) = structural_remaining {
                    tracing::debug!(
                        %session_id,
                        remaining_seconds = remaining.as_secs_f64(),
                        "automatic compression deferred by structural no-op backoff"
                    );
                }
            }
            return Ok(attempts);
        }
        if guard.ineffective_count >= 2 && guard.recovery_deadline <= now {
            database.set_compression_breaker(&session_id, 1, 0.0)?;
        }

        // Python full compression starts with a token-budget-aware deterministic
        // prune. Publish that rewrite under the same route, snapshot and lease
        // guards, then reload before summary selection. No provider request is
        // made between the two maintenance phases.
        let tail_token_budget = policy.tail_token_budget(preflight.context_length, threshold);
        if !phase_one_committed {
            let candidate = crate::tool_result_prune::prune_old_tool_results_with_budget(
                &snapshot.messages,
                policy.protect_last_n,
                tail_token_budget,
                crate::tool_result_prune::PRUNE_MIN_CHARS,
                preflight.stale_thinking_on_wire,
            );
            if candidate.changed {
                let holder = admitted.durable_lease.as_ref().map(|lease| lease.holder());
                let committed = deps.store.publish_tool_prune(
                    source,
                    &admitted.entry,
                    &snapshot.messages,
                    &candidate.messages,
                    prune_rearm,
                    holder,
                )?;
                if !committed {
                    return Ok(attempts);
                }
                tracing::info!(
                    %session_id,
                    pruned = candidate.pruned_count,
                    tail_token_budget,
                    "native pre-compression token-budget prune committed"
                );
                phase_one_committed = true;
                continue;
            }
        }
        phase_one_committed = false;

        let Some((prefix_end, tail_start)) = compression_boundaries(
            &snapshot.messages,
            protect_first,
            policy.protect_last_n,
            policy.min_tail_user_messages,
            tail_token_budget,
            preflight.stale_thinking_on_wire,
        ) else {
            tracing::warn!(%session_id, "automatic compression found no complete compressible region");
            agent.record_compression_structural_no_op(
                context,
                &session_id,
                "no complete compressible region",
            );
            return Ok(attempts);
        };
        let middle = &snapshot.messages[prefix_end..tail_start];
        let source_chars = structured_chars(middle);
        let Some(summary_history) = crate::compression_prompt::history_for_window(
            &snapshot.messages,
            prefix_end,
            tail_start,
        ) else {
            return Ok(attempts);
        };
        let mut summary_message = message.clone();
        summary_message.resolved_session_id = Some(session_id.clone());
        let checkpoint = match agent
            .prepare_pre_compression_checkpoint(
                context,
                &summary_message,
                &snapshot.messages,
                policy.checkpoint_required,
            )
            .await
        {
            Ok(checkpoint) => checkpoint,
            Err(error) if policy.checkpoint_required => {
                let error = crate::compression_redact::redact(&error.to_string());
                tracing::warn!(%error, %session_id, "required automatic compression checkpoint failed closed");
                return Ok(attempts);
            }
            Err(error) => {
                let error = crate::compression_redact::redact(&error.to_string());
                tracing::warn!(%error, %session_id, "optional automatic compression checkpoint failed open");
                crate::agent::PreCompressionCheckpoint::default()
            }
        };
        if policy.checkpoint_required && !checkpoint.checkpoint_supported {
            tracing::warn!(%session_id, "required automatic compression checkpoint is unsupported");
            return Ok(attempts);
        }
        let summary_body = match agent
            .summarize_context_with_memory(
                context,
                &summary_message,
                &summary_history,
                None,
                checkpoint.memory_context.as_deref(),
            )
            .await
        {
            Ok(Some(summary)) if !summary.trim().is_empty() => {
                crate::compression_redact::redact(summary.trim())
            }
            Ok(_) => return Ok(attempts),
            Err(error) => {
                let redacted = crate::compression_redact::redact(&error.to_string());
                database.record_compression_failure_cooldown(
                    &session_id,
                    now + 600.0,
                    Some(&redacted),
                )?;
                tracing::warn!(error = %redacted, %session_id, "automatic compression summary failed");
                return Ok(attempts);
            }
        };
        attempts += 1;
        if source_chars == 0 || summary_body.len() >= source_chars {
            let strikes = guard.ineffective_count.saturating_add(1);
            let recovery = if strikes >= 2 { now + 300.0 } else { 0.0 };
            database.set_compression_breaker(&session_id, strikes, recovery)?;
            tracing::warn!(%session_id, strikes, "automatic compression refused a non-shrinking summary");
            return Ok(attempts);
        }
        let Some(replacement) = crate::compression_handoff::plan_replacement(
            &snapshot.messages,
            prefix_end,
            tail_start,
            &summary_body,
        ) else {
            return Ok(attempts);
        };
        let holder = admitted.durable_lease.as_ref().map(|lease| lease.holder());
        if in_place {
            let committed = deps.store.publish_in_place_compression(
                source,
                &admitted.entry,
                &snapshot.messages,
                &replacement,
                holder,
            )?;
            if !committed {
                return Ok(attempts);
            }
            if let Err(error) = agent
                .notify_compression_boundary(context, &session_id, &session_id, true)
                .await
            {
                let error = crate::compression_redact::redact(&error.to_string());
                tracing::warn!(%error, %session_id, "in-place automatic compression boundary notification failed after commit");
            }
        } else {
            let Some(published) = deps.store.publish_compression(
                source,
                &admitted.entry,
                &snapshot.messages,
                &replacement,
                holder,
            )?
            else {
                return Ok(attempts);
            };
            if let Some(lease) = admitted.lease.as_mut() {
                if !deps
                    .transcript_leases
                    .rebind(lease, &published.entry.session_id)
                {
                    tracing::error!(
                        old_session = %session_id,
                        new_session = %published.entry.session_id,
                        "automatic compression could not rebind transcript lease"
                    );
                }
            }
            if let Err(error) = agent
                .notify_compression_boundary(
                    context,
                    &session_id,
                    &published.entry.session_id,
                    false,
                )
                .await
            {
                let error = crate::compression_redact::redact(&error.to_string());
                tracing::warn!(%error, old_session = %session_id, new_session = %published.entry.session_id, "rotating automatic compression boundary notification failed after commit");
            }
            admitted.entry = published.entry;
            admitted.finalizable = deps.store.is_session_finalizable(&admitted.entry);
            message.resolved_session_id = Some(admitted.entry.session_id.clone());
        }
        agent.clear_compression_structural_backoff(context, &admitted.entry.session_id);
        database.clear_compression_failure_cooldown(&admitted.entry.session_id)?;
        database.set_compression_breaker(&admitted.entry.session_id, 0, 0.0)?;
    }
    Ok(attempts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 1. Defaults tests
    #[test]
    fn test_parse_defaults_table() {
        struct TestCase {
            name: &'static str,
            input: Value,
        }

        let cases = vec![
            TestCase {
                name: "empty object",
                input: json!({}),
            },
            TestCase {
                name: "empty nested compression object",
                input: json!({"compression": {}}),
            },
            TestCase {
                name: "null value",
                input: Value::Null,
            },
            TestCase {
                name: "null nested compression value",
                input: json!({"compression": null}),
            },
            TestCase {
                name: "primitive string",
                input: json!("not a dict"),
            },
            TestCase {
                name: "empty array",
                input: json!([]),
            },
        ];

        for case in cases {
            let policy = AutomaticCompressionPolicy::from_value(&case.input);
            assert!(policy.enabled, "{}: enabled", case.name);
            assert_eq!(policy.threshold, 0.50, "{}: threshold", case.name);
            assert_eq!(
                policy.threshold_tokens, None,
                "{}: threshold_tokens",
                case.name
            );
            assert!(
                policy.model_thresholds.is_empty(),
                "{}: model_thresholds",
                case.name
            );
            assert_eq!(policy.protect_last_n, 20, "{}: protect_last_n", case.name);
            assert_eq!(policy.protect_first_n, 3, "{}: protect_first_n", case.name);
            assert_eq!(policy.target_ratio, 0.20, "{}: target_ratio", case.name);
            assert_eq!(policy.tail_mode, "lean", "{}: tail_mode", case.name);
            assert_eq!(policy.max_attempts, 3, "{}: max_attempts", case.name);
            assert_eq!(
                policy.proactive_prune_tokens, 0,
                "{}: prune trigger",
                case.name
            );
            assert_eq!(
                policy.proactive_prune_min_result_chars, 8_000,
                "{}: prune result floor",
                case.name
            );
            assert_eq!(
                policy.proactive_prune_min_reclaim_tokens, 4_096,
                "{}: prune reclaim floor",
                case.name
            );
        }
    }

    #[test]
    fn proactive_prune_config_matches_python_coercion() {
        let policy = AutomaticCompressionPolicy::from_value(&json!({"compression": {
            "proactive_prune_tokens": "48000",
            "proactive_prune_min_result_chars": -5,
            "proactive_prune_min_reclaim_tokens": 0.0
        }}));
        assert_eq!(policy.proactive_prune_tokens, 48_000);
        assert_eq!(policy.proactive_prune_min_result_chars, 200);
        assert_eq!(policy.proactive_prune_min_reclaim_tokens, 0);

        let malformed = AutomaticCompressionPolicy::from_value(&json!({"compression": {
            "proactive_prune_tokens": true,
            "proactive_prune_min_result_chars": 12.5,
            "proactive_prune_min_reclaim_tokens": "bad"
        }}));
        assert_eq!(malformed.proactive_prune_tokens, 0);
        assert_eq!(malformed.proactive_prune_min_result_chars, 8_000);
        assert_eq!(malformed.proactive_prune_min_reclaim_tokens, 4_096);
    }

    #[test]
    fn micro_compaction_config_matches_python_coercion_and_checkpoint_gate() {
        let enabled = AutomaticCompressionPolicy::from_value(&json!({"compression": {
            "micro_compact": "on",
            "micro_compact_every_n_turns": "5",
            "micro_compact_defrag_threshold_tokens": 4096.0
        }}));
        assert!(enabled.micro_compact);
        assert_eq!(enabled.micro_compact_every_n_turns, 5);
        assert_eq!(enabled.micro_compact_defrag_threshold_tokens, 4_096);

        let malformed = AutomaticCompressionPolicy::from_value(&json!({"compression": {
            "micro_compact": 2,
            "micro_compact_every_n_turns": true,
            "micro_compact_defrag_threshold_tokens": -20
        }}));
        assert!(malformed.micro_compact);
        assert_eq!(malformed.micro_compact_every_n_turns, 1);
        assert_eq!(malformed.micro_compact_defrag_threshold_tokens, 1);

        let checkpointed = AutomaticCompressionPolicy::from_value(&json!({"compression": {
            "micro_compact": true,
            "checkpoint_required": true
        }}));
        assert!(!checkpointed.micro_compact);
    }

    #[test]
    fn full_compression_mode_and_checkpoint_gate_match_python_coercion() {
        let configured = AutomaticCompressionPolicy::from_value(&json!({"compression": {
            "in_place": "off",
            "checkpoint_required": "yes"
        }}));
        assert!(!configured.in_place);
        assert!(configured.checkpoint_required);

        let unknown = AutomaticCompressionPolicy::from_value(&json!({"compression": {
            "in_place": "unknown",
            "checkpoint_required": "unknown"
        }}));
        assert!(!unknown.in_place);
        assert!(!unknown.checkpoint_required);

        let defaults = AutomaticCompressionPolicy::from_value(&json!({"compression": {}}));
        assert!(defaults.in_place);
        assert!(!defaults.checkpoint_required);
    }

    #[test]
    fn tail_budget_config_and_modes_match_python() {
        let default = AutomaticCompressionPolicy::default();
        assert_eq!(default.tail_token_budget(100_000, 75_000), 10_000);
        assert_eq!(default.tail_token_budget(1_000_000, 500_000), 25_000);

        let legacy = AutomaticCompressionPolicy::from_value(&json!({"compression": {
            "tail_mode": " LEGACY ",
            "target_ratio": 0.6
        }}));
        assert_eq!(legacy.tail_mode, "legacy");
        assert_eq!(legacy.target_ratio, 0.6);
        assert_eq!(legacy.tail_token_budget(1_000_000, 400_000), 240_000);

        let clamped = AutomaticCompressionPolicy::from_value(&json!({"compression": {
            "tail_mode": "unknown",
            "target_ratio": 99
        }}));
        assert_eq!(clamped.tail_mode, "lean");
        assert_eq!(clamped.target_ratio, 0.8);
    }

    // 2. Malformed values and conservative coercion tests
    #[test]
    fn test_parse_enabled_coercion() {
        struct Case {
            raw: Value,
            expected: bool,
        }

        let cases = [
            Case {
                raw: json!(true),
                expected: true,
            },
            Case {
                raw: json!(false),
                expected: false,
            },
            Case {
                raw: json!("true"),
                expected: true,
            },
            Case {
                raw: json!("TRUE"),
                expected: true,
            },
            Case {
                raw: json!("1"),
                expected: true,
            },
            Case {
                raw: json!("yes"),
                expected: true,
            },
            Case {
                raw: json!("on"),
                expected: false,
            },
            Case {
                raw: json!(1),
                expected: true,
            },
            Case {
                raw: json!(0),
                expected: false,
            },
            Case {
                raw: json!("false"),
                expected: false,
            },
            Case {
                raw: json!("0"),
                expected: false,
            },
            Case {
                raw: json!("no"),
                expected: false,
            },
            Case {
                raw: json!("off"),
                expected: false,
            },
            Case {
                raw: json!("random"),
                expected: false,
            },
            Case {
                raw: json!(123),
                expected: false,
            },
            Case {
                raw: json!([]),
                expected: false,
            },
            Case {
                raw: json!({}),
                expected: false,
            },
        ];

        for (idx, c) in cases.iter().enumerate() {
            let input = json!({"enabled": c.raw.clone()});
            let policy = AutomaticCompressionPolicy::from_value(&input);
            assert_eq!(policy.enabled, c.expected, "case {} with {:?}", idx, c.raw);
        }
    }

    #[test]
    fn test_parse_threshold_coercion() {
        struct Case {
            raw: Value,
            expected: f64,
        }

        let cases = vec![
            Case {
                raw: json!(0.75),
                expected: 0.75,
            },
            Case {
                raw: json!("0.85"),
                expected: 0.85,
            },
            Case {
                raw: json!(1.0),
                expected: 1.0,
            },
            Case {
                raw: json!(-0.5),
                expected: -0.5,
            },
            Case {
                raw: json!(0.0),
                expected: 0.0,
            },
            Case {
                raw: json!("invalid"),
                expected: 0.50,
            },
            Case {
                raw: json!(true),
                expected: 1.0,
            },
            Case {
                raw: json!(null),
                expected: 0.50,
            },
            Case {
                raw: json!([]),
                expected: 0.50,
            },
        ];

        for (idx, c) in cases.iter().enumerate() {
            let input = json!({"threshold": c.raw.clone()});
            let policy = AutomaticCompressionPolicy::from_value(&input);
            assert!(
                (policy.threshold - c.expected).abs() < f64::EPSILON,
                "case {} with {:?}",
                idx,
                c.raw
            );
        }
    }

    #[test]
    fn test_parse_threshold_tokens_coercion() {
        struct Case {
            raw: Value,
            expected: Option<u64>,
        }

        let cases = vec![
            Case {
                raw: json!(50000),
                expected: Some(50000),
            },
            Case {
                raw: json!("40000"),
                expected: Some(40000),
            },
            Case {
                raw: json!(60000.0),
                expected: Some(60000),
            },
            Case {
                raw: json!(0),
                expected: None,
            },
            Case {
                raw: json!(-100),
                expected: None,
            },
            Case {
                raw: json!("0"),
                expected: None,
            },
            Case {
                raw: json!("-50"),
                expected: None,
            },
            Case {
                raw: json!(true),
                expected: Some(1),
            },
            Case {
                raw: json!(false),
                expected: None,
            },
            Case {
                raw: json!("invalid"),
                expected: None,
            },
            Case {
                raw: json!(60000.5),
                expected: Some(60000),
            },
            Case {
                raw: json!(null),
                expected: None,
            },
        ];

        for (idx, c) in cases.iter().enumerate() {
            let input = json!({"threshold_tokens": c.raw.clone()});
            let policy = AutomaticCompressionPolicy::from_value(&input);
            assert_eq!(
                policy.threshold_tokens, c.expected,
                "case {} with {:?}",
                idx, c.raw
            );
        }
    }

    #[test]
    fn test_parse_model_thresholds_coercion() {
        let input = json!({
            "model_thresholds": {
                "glm-5.2": 0.80,
                "gpt-4": "0.70",
                "bad_bool": true,
                "bad_negative": -0.2,
                "bad_str": "not_a_number"
            }
        });
        let policy = AutomaticCompressionPolicy::from_value(&input);
        assert_eq!(policy.model_thresholds.len(), 2);
        assert_eq!(policy.model_thresholds[0], ("glm-5.2".to_string(), 0.80));
        assert_eq!(
            policy.model_thresholds[1],
            ("bad_negative".to_string(), -0.20)
        );

        let non_obj = json!({"model_thresholds": "invalid"});
        let policy_non_obj = AutomaticCompressionPolicy::from_value(&non_obj);
        assert!(policy_non_obj.model_thresholds.is_empty());
    }

    #[test]
    fn test_parse_protect_counts_coercion() {
        struct Case {
            raw_last: Value,
            raw_first: Value,
            expected_last: usize,
            expected_first: usize,
        }

        let cases = [
            Case {
                raw_last: json!(25),
                raw_first: json!(5),
                expected_last: 25,
                expected_first: 5,
            },
            Case {
                raw_last: json!("30"),
                raw_first: json!("2"),
                expected_last: 30,
                expected_first: 2,
            },
            Case {
                raw_last: json!(-10),
                raw_first: json!(-5),
                expected_last: 0,
                expected_first: 0,
            },
            Case {
                raw_last: json!(true),
                raw_first: json!(false),
                expected_last: 1,
                expected_first: 0,
            },
            Case {
                raw_last: json!("bad"),
                raw_first: json!("bad"),
                expected_last: 20,
                expected_first: 3,
            },
        ];

        for (idx, c) in cases.iter().enumerate() {
            let input = json!({
                "protect_last_n": c.raw_last.clone(),
                "protect_first_n": c.raw_first.clone(),
            });
            let policy = AutomaticCompressionPolicy::from_value(&input);
            assert_eq!(policy.protect_last_n, c.expected_last, "case {} last", idx);
            assert_eq!(
                policy.protect_first_n, c.expected_first,
                "case {} first",
                idx
            );
        }
    }

    #[test]
    fn test_parse_max_attempts_coercion() {
        struct Case {
            raw: Value,
            expected: u32,
        }

        let cases = vec![
            Case {
                raw: json!(1),
                expected: 1,
            },
            Case {
                raw: json!(5),
                expected: 5,
            },
            Case {
                raw: json!(10),
                expected: 10,
            },
            Case {
                raw: json!(15),
                expected: 10,
            },
            Case {
                raw: json!(100),
                expected: 10,
            },
            Case {
                raw: json!(0),
                expected: 3,
            },
            Case {
                raw: json!(-3),
                expected: 3,
            },
            Case {
                raw: json!(true),
                expected: 3,
            },
            Case {
                raw: json!(false),
                expected: 3,
            },
            Case {
                raw: json!(4.7),
                expected: 3,
            },
            Case {
                raw: json!(6.0),
                expected: 6,
            },
            Case {
                raw: json!("7"),
                expected: 7,
            },
            Case {
                raw: json!("12"),
                expected: 10,
            },
            Case {
                raw: json!("4.7"),
                expected: 3,
            },
            Case {
                raw: json!("invalid"),
                expected: 3,
            },
            Case {
                raw: json!(null),
                expected: 3,
            },
        ];

        for (idx, c) in cases.iter().enumerate() {
            let input = json!({"max_attempts": c.raw.clone()});
            let policy = AutomaticCompressionPolicy::from_value(&input);
            assert_eq!(
                policy.max_attempts, c.expected,
                "case {} with {:?}",
                idx, c.raw
            );
        }
    }

    // 3. Longest model match tests
    #[test]
    fn test_longest_model_match_table() {
        struct TestCase {
            name: &'static str,
            model: &'static str,
            overrides: Vec<(&'static str, f64)>,
            default_ratio: f64,
            expected: f64,
        }

        let cases = vec![
            TestCase {
                name: "no overrides returns default",
                model: "glm-5.2",
                overrides: vec![],
                default_ratio: 0.50,
                expected: 0.50,
            },
            TestCase {
                name: "exact single match",
                model: "glm-5.2",
                overrides: vec![("glm-5.2", 0.70)],
                default_ratio: 0.50,
                expected: 0.70,
            },
            TestCase {
                name: "substring match",
                model: "openai/gpt-5.5-preview",
                overrides: vec![("gpt-5.5", 0.85)],
                default_ratio: 0.50,
                expected: 0.85,
            },
            TestCase {
                name: "longest substring match wins",
                model: "glm-5.2-1M",
                overrides: vec![("glm-5.2", 0.80), ("glm-5.2-1M", 0.25), ("glm", 0.90)],
                default_ratio: 0.50,
                expected: 0.25,
            },
            TestCase {
                name: "shorter substring wins when longer does not match",
                model: "glm-5.2-chat",
                overrides: vec![("glm-5.2", 0.80), ("glm-5.2-1M", 0.25)],
                default_ratio: 0.50,
                expected: 0.80,
            },
            TestCase {
                name: "no matching substring returns default",
                model: "claude-3-5-sonnet",
                overrides: vec![("glm-5.2", 0.80), ("gpt", 0.60)],
                default_ratio: 0.50,
                expected: 0.50,
            },
            TestCase {
                name: "empty model name returns default",
                model: "",
                overrides: vec![("gpt", 0.60)],
                default_ratio: 0.50,
                expected: 0.50,
            },
            TestCase {
                name: "deterministic tie breaking: first key in config order wins",
                model: "alpha_bravo_charlie",
                overrides: vec![("alpha", 0.60), ("bravo", 0.70)],
                default_ratio: 0.50,
                expected: 0.60,
            },
            TestCase {
                name: "deterministic tie breaking: reversed order",
                model: "alpha_bravo_charlie",
                overrides: vec![("bravo", 0.70), ("alpha", 0.60)],
                default_ratio: 0.50,
                expected: 0.70,
            },
        ];

        for case in cases {
            let overrides = case
                .overrides
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<Vec<_>>();
            let result = resolve_model_threshold(case.model, &overrides, case.default_ratio);
            assert!(
                (result - case.expected).abs() < f64::EPSILON,
                "{}: expected {}, got {}",
                case.name,
                case.expected,
                result
            );
        }
    }

    // 4. Absolute cap tests
    #[test]
    fn test_compute_effective_threshold_table() {
        struct TestCase {
            name: &'static str,
            context_length: u64,
            ratio: f64,
            threshold_tokens: Option<u64>,
            expected: u64,
        }

        let cases = vec![
            TestCase {
                name: "ratio only without cap",
                context_length: 100_000,
                ratio: 0.50,
                threshold_tokens: None,
                expected: 75_000,
            },
            TestCase {
                name: "cap lower than ratio tokens binds",
                context_length: 100_000,
                ratio: 0.50,
                threshold_tokens: Some(30_000),
                expected: 30_000,
            },
            TestCase {
                name: "cap below adjusted small-window ratio binds",
                context_length: 100_000,
                ratio: 0.50,
                threshold_tokens: Some(70_000),
                expected: 70_000,
            },
            TestCase {
                name: "cap higher than context window lets ratio win",
                context_length: 100_000,
                ratio: 0.50,
                threshold_tokens: Some(150_000),
                expected: 75_000,
            },
            TestCase {
                name: "zero context length returns zero safely",
                context_length: 0,
                ratio: 0.50,
                threshold_tokens: Some(40_000),
                expected: 0,
            },
            TestCase {
                name: "zero ratio returns zero safely",
                context_length: 100_000,
                ratio: 0.0,
                threshold_tokens: Some(40_000),
                expected: 40_000,
            },
            TestCase {
                name: "negative ratio returns zero safely",
                context_length: 100_000,
                ratio: -0.50,
                threshold_tokens: Some(40_000),
                expected: 40_000,
            },
            TestCase {
                name: "nan ratio returns zero safely",
                context_length: 100_000,
                ratio: f64::NAN,
                threshold_tokens: Some(40_000),
                expected: 0,
            },
            TestCase {
                name: "zero cap is treated as unconfigured",
                context_length: 100_000,
                ratio: 0.50,
                threshold_tokens: Some(0),
                expected: 75_000,
            },
        ];

        for case in cases {
            let res =
                compute_effective_threshold(case.context_length, case.ratio, case.threshold_tokens);
            assert_eq!(res, case.expected, "{}", case.name);
        }
    }

    // 5. Attempt limit tests
    #[test]
    fn test_attempt_limit_table() {
        let policy = AutomaticCompressionPolicy {
            enabled: true,
            max_attempts: 3,
            ..Default::default()
        };
        let effective_threshold = 10_000;
        let pressure_tokens = 12_000;
        let blocked = false;

        struct TestCase {
            attempts_used: u32,
            expected: CompressionDecision,
        }

        let cases = vec![
            TestCase {
                attempts_used: 0,
                expected: CompressionDecision::Attempt {
                    attempt_number: 1,
                    max_attempts: 3,
                    effective_threshold: 10_000,
                },
            },
            TestCase {
                attempts_used: 1,
                expected: CompressionDecision::Attempt {
                    attempt_number: 2,
                    max_attempts: 3,
                    effective_threshold: 10_000,
                },
            },
            TestCase {
                attempts_used: 2,
                expected: CompressionDecision::Attempt {
                    attempt_number: 3,
                    max_attempts: 3,
                    effective_threshold: 10_000,
                },
            },
            TestCase {
                attempts_used: 3,
                expected: CompressionDecision::AttemptsExhausted {
                    attempts_used: 3,
                    max_attempts: 3,
                },
            },
            TestCase {
                attempts_used: 4,
                expected: CompressionDecision::AttemptsExhausted {
                    attempts_used: 4,
                    max_attempts: 3,
                },
            },
        ];

        for case in cases {
            let decision = policy.decide(
                effective_threshold,
                pressure_tokens,
                case.attempts_used,
                blocked,
            );
            assert_eq!(
                decision, case.expected,
                "attempts_used = {}",
                case.attempts_used
            );
        }
    }

    // 6. Boundary equality tests
    #[test]
    fn test_boundary_equality_table() {
        let policy = AutomaticCompressionPolicy {
            enabled: true,
            max_attempts: 3,
            ..Default::default()
        };
        let threshold = 50_000;

        struct TestCase {
            name: &'static str,
            pressure: u64,
            attempts: u32,
            blocked: bool,
            expected: CompressionDecision,
        }

        let cases = vec![
            TestCase {
                name: "one token below threshold",
                pressure: threshold - 1,
                attempts: 0,
                blocked: false,
                expected: CompressionDecision::BelowThreshold {
                    pressure_tokens: threshold - 1,
                    effective_threshold: threshold,
                },
            },
            TestCase {
                name: "exact threshold equality triggers attempt",
                pressure: threshold,
                attempts: 0,
                blocked: false,
                expected: CompressionDecision::Attempt {
                    attempt_number: 1,
                    max_attempts: 3,
                    effective_threshold: threshold,
                },
            },
            TestCase {
                name: "one token above threshold triggers attempt",
                pressure: threshold + 1,
                attempts: 0,
                blocked: false,
                expected: CompressionDecision::Attempt {
                    attempt_number: 1,
                    max_attempts: 3,
                    effective_threshold: threshold,
                },
            },
            TestCase {
                name: "exact threshold equality with blocked flag yields externally blocked",
                pressure: threshold,
                attempts: 0,
                blocked: true,
                expected: CompressionDecision::ExternallyBlocked,
            },
            TestCase {
                name: "below threshold with blocked flag remains below threshold",
                pressure: threshold - 1,
                attempts: 0,
                blocked: true,
                expected: CompressionDecision::BelowThreshold {
                    pressure_tokens: threshold - 1,
                    effective_threshold: threshold,
                },
            },
            TestCase {
                name: "exact attempt limit boundary: attempts == max_attempts gives exhausted",
                pressure: threshold,
                attempts: 3,
                blocked: false,
                expected: CompressionDecision::AttemptsExhausted {
                    attempts_used: 3,
                    max_attempts: 3,
                },
            },
            TestCase {
                name: "attempts exhausted takes precedence over blocked flag",
                pressure: threshold,
                attempts: 3,
                blocked: true,
                expected: CompressionDecision::AttemptsExhausted {
                    attempts_used: 3,
                    max_attempts: 3,
                },
            },
        ];

        for case in cases {
            let decision = policy.decide(threshold, case.pressure, case.attempts, case.blocked);
            assert_eq!(decision, case.expected, "{}", case.name);
        }
    }

    #[test]
    fn test_disabled_policy_boundary() {
        let policy = AutomaticCompressionPolicy {
            enabled: false,
            ..Default::default()
        };

        let res1 = policy.decide(1000, 2000, 0, false);
        assert_eq!(res1, CompressionDecision::Disabled);
        assert!(res1.is_disabled());
        assert!(!res1.should_compress());

        let res2 = policy.decide(1000, 500, 0, false);
        assert_eq!(res2, CompressionDecision::Disabled);
    }

    #[test]
    fn test_cap_and_ratio_equality() {
        let context_length = 100_000;
        let ratio = 0.50;
        let cap = Some(50_000);
        let res = compute_effective_threshold(context_length, ratio, cap);
        assert_eq!(res, 50_000);
    }

    #[test]
    fn min_tail_user_messages_config_matches_python_coercion() {
        struct Case {
            raw: Value,
            expected: usize,
        }
        let cases = vec![
            Case {
                raw: json!({}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": null}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": true}}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": false}}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": 0}}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": -5}}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": 1}}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": 3}}),
                expected: 3,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": 4.0}}),
                expected: 4,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": 4.5}}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": -2.0}}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": "5"}}),
                expected: 5,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": "  4  "}}),
                expected: 4,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": "0"}}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": "-2"}}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": "not_a_number"}}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": "3.5"}}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": [1, 2]}}),
                expected: 1,
            },
            Case {
                raw: json!({"compression": {"min_tail_user_messages": {"nested": 1}}}),
                expected: 1,
            },
        ];

        for case in cases {
            let policy = AutomaticCompressionPolicy::from_value(&case.raw);
            assert_eq!(
                policy.min_tail_user_messages, case.expected,
                "input: {:?}",
                case.raw
            );
        }

        let default_policy = AutomaticCompressionPolicy::default();
        assert_eq!(default_policy.min_tail_user_messages, 1);
    }

    #[test]
    fn compression_boundaries_honors_min_tail_user_messages() {
        use crate::session_db::{CompressionHistoryMessage, HistoryMessage};

        fn test_msg(id: i64, role: &str, content: &str) -> CompressionHistoryMessage {
            CompressionHistoryMessage {
                id,
                message: HistoryMessage {
                    role: role.to_string(),
                    content: content.to_string(),
                    api_content: None,
                },
                tool_call_id: None,
                tool_calls: None,
                tool_name: None,
                effect_disposition: None,
                finish_reason: None,
                reasoning: None,
                reasoning_content: None,
                reasoning_details: None,
                codex_reasoning_items: None,
                codex_message_items: None,
                display_kind: None,
                display_metadata: None,
                timestamp: 0.0,
                compressed_summary: false,
            }
        }

        let mut messages = Vec::new();
        // Head: index 0 (user), index 1 (assistant)
        messages.push(test_msg(1, "user", "head prompt"));
        messages.push(test_msg(2, "assistant", "head reply"));

        for i in 1..=4 {
            messages.push(test_msg((i * 2 + 1) as i64, "user", &format!("task {i}")));
            messages.push(test_msg((i * 2 + 2) as i64, "assistant", &"A".repeat(4000)));
        }

        let cut_n1 = compression_boundaries(&messages, 2, 2, 1, 200, false);
        assert!(cut_n1.is_some());
        let (_prefix1, tail_start1) = cut_n1.unwrap();

        let cut_n3 = compression_boundaries(&messages, 2, 2, 3, 200, false);
        assert!(cut_n3.is_some());
        let (_prefix3, tail_start3) = cut_n3.unwrap();

        assert!(
            tail_start3 < tail_start1,
            "tail_start3 ({tail_start3}) must be pulled back before tail_start1 ({tail_start1})"
        );
    }
}
