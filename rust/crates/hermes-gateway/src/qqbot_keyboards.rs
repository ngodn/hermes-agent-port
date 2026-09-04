//! Port of gateway/platforms/qqbot/keyboards.py.
//!
// Public API is ahead of its callers (the adapter wires it in a later slice).
#![allow(dead_code)]
//!
//! QQ Bot v2 inline keyboards plus approval / update-prompt text builders.
//!
//! QQ Bot v2 supports attaching inline keyboards to outbound messages. When a
//! user clicks a button, the platform dispatches an `INTERACTION_CREATE`
//! gateway event carrying the button's `data` payload. The bot must ACK the
//! interaction promptly via `PUT /interactions/{id}` or the user sees an error
//! indicator on the button.
//!
//! This module provides:
//!
//! - `InlineKeyboard` plus its button structs, serialized into the `keyboard`
//!   field of the outbound message body.
//! - `build_approval_keyboard` — 3-button allow-once / allow-always / deny
//!   keyboard for tool-approval flows.
//! - `build_update_prompt_keyboard` — Yes/No keyboard for update confirms.
//! - `parse_approval_button_data` / `parse_update_prompt_button_data` — decode
//!   the `button_data` payload from `INTERACTION_CREATE`.
//! - `ApprovalRequest` plus `build_approval_text` — render the message body.
//! - `InteractionEvent` plus `parse_interaction_event` — the parsed event shape.
//!
//! `button_data` formats:
//!
//! ```text
//! approve:<session_key>:<decision>      # decision = allow-once|allow-always|deny
//! update_prompt:<answer>                # answer = y|n
//! ```
//!
//! Field order matters for a byte-exact match with the Python `json.dumps`
//! output, so the wire types below are serde structs (fields serialize in
//! declaration order), not `serde_json::Value` maps (which sort keys because
//! this workspace has no `preserve_order` feature).
//!
//! `ApprovalSender` from the Python module is intentionally NOT ported here: it
//! only wires `build_approval_text` + `build_approval_keyboard` into the adapter's
//! `_send_message_with_keyboard` c2c / group POST helpers, which are adapter/runner
//! internals that live outside this self-contained slice. It gets ported with the
//! QQ adapter's send path.
//!
//! Ported from WideLee's qqbot-agent-sdk v1.2.2 (approval.py + dto.py keyboard
//! types). Authorship preserved via Co-authored-by.

use serde::Serialize;

// ── button_data prefixes ─────────────────────────────────────────────

pub const APPROVAL_BUTTON_PREFIX: &str = "approve:";
pub const UPDATE_PROMPT_PREFIX: &str = "update_prompt:";

// The three approval decisions, exactly as they appear at the tail of an
// approval button_data string.
const APPROVAL_DECISIONS: [&str; 3] = ["allow-once", "allow-always", "deny"];

// ── Keyboard structs ─────────────────────────────────────────────────

/// Button permission metadata. `type=2` means all users can click.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeyboardButtonPermission {
    #[serde(rename = "type")]
    pub type_: i64,
}

impl Default for KeyboardButtonPermission {
    fn default() -> Self {
        KeyboardButtonPermission { type_: 2 }
    }
}

/// What happens when the button is clicked.
///
/// `type` is `1` (Callback, triggers `INTERACTION_CREATE`) or `2` (Link, opens
/// a URL). `data` is the payload delivered in `data.resolved.button_data` when
/// `type=1`. `click_limit` is the max clicks per user (`1` = single-use).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeyboardButtonAction {
    #[serde(rename = "type")]
    pub type_: i64,
    pub data: String,
    pub permission: KeyboardButtonPermission,
    pub click_limit: i64,
}

impl KeyboardButtonAction {
    /// Matches the Python defaults: a `KeyboardButtonPermission()` and
    /// `click_limit=1`.
    pub fn new(type_: i64, data: String) -> Self {
        KeyboardButtonAction {
            type_,
            data,
            permission: KeyboardButtonPermission::default(),
            click_limit: 1,
        }
    }
}

/// Visual rendering of a button. `style` is `0` = grey, `1` = blue.
/// `visited_label` is the post-click label (the button stays greyed in place).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeyboardButtonRenderData {
    pub label: String,
    pub visited_label: String,
    pub style: i64,
}

/// One button in a keyboard. Buttons sharing a `group_id` are mutually
/// exclusive: clicking one greys the rest.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeyboardButton {
    pub id: String,
    pub render_data: KeyboardButtonRenderData,
    pub action: KeyboardButtonAction,
    pub group_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeyboardRow {
    pub buttons: Vec<KeyboardButton>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeyboardContent {
    pub rows: Vec<KeyboardRow>,
}

/// Top-level keyboard payload — goes into `MessageToCreate.keyboard`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InlineKeyboard {
    pub content: KeyboardContent,
}

// ── INTERACTION_CREATE parsing ───────────────────────────────────────

/// Parse approval `button_data` into `(session_key, decision)`.
///
/// Returns `None` if the string is not a well-formed approval button.
///
/// Mirrors the Python regex `^approve:(.+):(allow-once|allow-always|deny)$`.
/// The `.+` session-key group is greedy, so with a nested-colon session key the
/// decision is whichever known token sits at the very end. This hand-rolled
/// version reproduces that: strip the `approve:` prefix, then peel a
/// `:<decision>` suffix, and require the remaining session key to be non-empty.
pub fn parse_approval_button_data(button_data: &str) -> Option<(String, String)> {
    let rest = button_data.strip_prefix(APPROVAL_BUTTON_PREFIX)?;
    for decision in APPROVAL_DECISIONS {
        // Suffix must be ":<decision>" and leave at least one char of session
        // key before it (the regex's `.+` requires one or more).
        if let Some(session_key) = rest
            .strip_suffix(decision)
            .and_then(|s| s.strip_suffix(':'))
        {
            if !session_key.is_empty() {
                return Some((session_key.to_string(), decision.to_string()));
            }
        }
    }
    None
}

/// Parse update-prompt `button_data` into `"y"` or `"n"`.
///
/// Mirrors the Python regex `^update_prompt:(y|n)$`.
pub fn parse_update_prompt_button_data(button_data: &str) -> Option<String> {
    let answer = button_data.strip_prefix(UPDATE_PROMPT_PREFIX)?;
    if answer == "y" || answer == "n" {
        Some(answer.to_string())
    } else {
        None
    }
}

// ── Keyboard builders ────────────────────────────────────────────────

fn make_callback_button(
    btn_id: &str,
    label: &str,
    visited_label: &str,
    data: String,
    style: i64,
    group_id: &str,
) -> KeyboardButton {
    KeyboardButton {
        id: btn_id.to_string(),
        render_data: KeyboardButtonRenderData {
            label: label.to_string(),
            visited_label: visited_label.to_string(),
            style,
        },
        action: KeyboardButtonAction::new(1, data),
        group_id: group_id.to_string(),
    }
}

/// Build the approval keyboard, hiding the persistent scope when unavailable.
///
/// Layout: `[✅ 允许一次] [⭐ 始终允许] [❌ 拒绝]`. All three share
/// `group_id="approval"` so clicking one greys out the rest. The `session_key`
/// is embedded into `button_data` so the decision routes back to the right
/// pending approval.
pub fn build_approval_keyboard(session_key: &str, allow_permanent: bool) -> InlineKeyboard {
    let mut buttons = vec![make_callback_button(
        "allow",
        "✅ 允许一次",
        "已允许",
        format!("{APPROVAL_BUTTON_PREFIX}{session_key}:allow-once"),
        1,
        "approval",
    )];
    if allow_permanent {
        buttons.push(make_callback_button(
            "always",
            "⭐ 始终允许",
            "已始终允许",
            format!("{APPROVAL_BUTTON_PREFIX}{session_key}:allow-always"),
            1,
            "approval",
        ));
    }
    buttons.push(make_callback_button(
        "deny",
        "❌ 拒绝",
        "已拒绝",
        format!("{APPROVAL_BUTTON_PREFIX}{session_key}:deny"),
        0,
        "approval",
    ));
    InlineKeyboard {
        content: KeyboardContent {
            rows: vec![KeyboardRow { buttons }],
        },
    }
}

/// Build a Yes/No keyboard for update confirmation prompts.
pub fn build_update_prompt_keyboard() -> InlineKeyboard {
    InlineKeyboard {
        content: KeyboardContent {
            rows: vec![KeyboardRow {
                buttons: vec![
                    make_callback_button(
                        "yes",
                        "✓ 确认",
                        "已确认",
                        format!("{UPDATE_PROMPT_PREFIX}y"),
                        1,
                        "update_prompt",
                    ),
                    make_callback_button(
                        "no",
                        "✗ 取消",
                        "已取消",
                        format!("{UPDATE_PROMPT_PREFIX}n"),
                        0,
                        "update_prompt",
                    ),
                ],
            }],
        },
    }
}

// ── ApprovalRequest + text builder ───────────────────────────────────

/// Structured approval-request display data.
///
/// `severity` is `"critical" | "info" | ""`. `timeout_sec` defaults to 120 in
/// the Python `default()` below.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub session_key: String,
    pub title: String,
    pub description: String,
    pub command_preview: String,
    pub cwd: String,
    pub tool_name: String,
    pub severity: String,
    pub timeout_sec: i64,
    pub allow_permanent: bool,
}

impl Default for ApprovalRequest {
    fn default() -> Self {
        ApprovalRequest {
            session_key: String::new(),
            title: String::new(),
            description: String::new(),
            command_preview: String::new(),
            cwd: String::new(),
            tool_name: String::new(),
            severity: String::new(),
            timeout_sec: 120,
            allow_permanent: true,
        }
    }
}

/// Render an `ApprovalRequest` into the message body (markdown).
pub fn build_approval_text(req: &ApprovalRequest) -> String {
    if !req.command_preview.is_empty() || !req.cwd.is_empty() {
        build_exec_text(req)
    } else {
        build_plugin_text(req)
    }
}

fn build_exec_text(req: &ApprovalRequest) -> String {
    let mut lines: Vec<String> = vec!["🔐 **命令执行审批**".to_string(), String::new()];
    if !req.command_preview.is_empty() {
        // Python slices `command_preview[:300]`; that is a character (code
        // point) slice, so take at most 300 chars, not 300 bytes.
        let preview: String = req.command_preview.chars().take(300).collect();
        lines.push(format!("```\n{preview}\n```"));
    }
    if !req.cwd.is_empty() {
        lines.push(format!("📁 目录: {}", req.cwd));
    }
    if !req.title.is_empty() && req.title != req.command_preview {
        lines.push(format!("📋 {}", req.title));
    }
    if !req.description.is_empty() {
        lines.push(format!("📝 {}", req.description));
    }
    lines.push(String::new());
    lines.push(format!("⏱️ 超时: {} 秒", req.timeout_sec));
    lines.join("\n")
}

fn build_plugin_text(req: &ApprovalRequest) -> String {
    let icon = if req.severity == "critical" {
        "🔴"
    } else if req.severity == "info" {
        "🔵"
    } else {
        "🟡"
    };
    let mut lines: Vec<String> = vec![format!("{icon} **审批请求**"), String::new()];
    lines.push(format!("📋 {}", req.title));
    if !req.description.is_empty() {
        lines.push(format!("📝 {}", req.description));
    }
    if !req.tool_name.is_empty() {
        lines.push(format!("🔧 工具: {}", req.tool_name));
    }
    lines.push(String::new());
    lines.push(format!("⏱️ 超时: {} 秒", req.timeout_sec));
    lines.join("\n")
}

// ── INTERACTION_CREATE event shape ───────────────────────────────────

/// Parsed `INTERACTION_CREATE` event payload.
///
/// See the QQ Bot v2 event-emit docs for the raw dispatch shape.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct InteractionEvent {
    /// Interaction event id, required for the `PUT /interactions/{id}` ACK.
    pub id: String,
    /// Event type code (`11` = message button).
    pub type_: i64,
    /// `0` = guild, `1` = group, `2` = c2c.
    pub chat_type: i64,
    /// `"guild" | "group" | "c2c"` — human-readable scene.
    pub scene: String,
    pub group_openid: String,
    pub group_member_openid: String,
    pub user_openid: String,
    pub channel_id: String,
    pub guild_id: String,
    pub button_data: String,
    pub button_id: String,
    pub resolver_user_id: String,
}

impl InteractionEvent {
    /// Best available operator openid (group -> member; c2c -> user).
    pub fn operator_openid(&self) -> &str {
        if !self.group_member_openid.is_empty() {
            &self.group_member_openid
        } else if !self.user_openid.is_empty() {
            &self.user_openid
        } else {
            &self.resolver_user_id
        }
    }
}

// Coerce a JSON value to a string the way Python's `str(raw.get(k, ""))` does
// for the values that show up here: a JSON string passes through, a number is
// stringified, null/missing becomes "".
fn coerce_str(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => {
            // Python str(True) is "True"; unlikely for these fields but kept faithful.
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        _ => String::new(),
    }
}

// Coerce a JSON value to i64 the way Python's `int(raw.get(k, 0) or 0)` does:
// a number is truncated to int, a missing/null/zero falls back to 0.
fn coerce_i64(v: Option<&serde_json::Value>) -> i64 {
    match v {
        Some(serde_json::Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// Parse a raw `INTERACTION_CREATE` dispatch payload (the `d` object).
pub fn parse_interaction_event(raw: &serde_json::Value) -> InteractionEvent {
    let empty = serde_json::Value::Null;
    let data_raw = raw.get("data").unwrap_or(&empty);
    let resolved = data_raw.get("resolved").unwrap_or(&empty);
    let scene_code = coerce_i64(raw.get("chat_type"));
    let scene = match scene_code {
        0 => "guild",
        1 => "group",
        2 => "c2c",
        _ => "",
    }
    .to_string();
    InteractionEvent {
        id: coerce_str(raw.get("id")),
        type_: coerce_i64(data_raw.get("type")),
        chat_type: scene_code,
        scene,
        group_openid: coerce_str(raw.get("group_openid")),
        group_member_openid: coerce_str(raw.get("group_member_openid")),
        user_openid: coerce_str(raw.get("user_openid")),
        channel_id: coerce_str(raw.get("channel_id")),
        guild_id: coerce_str(raw.get("guild_id")),
        button_data: coerce_str(resolved.get("button_data")),
        button_id: coerce_str(resolved.get("button_id")),
        resolver_user_id: coerce_str(resolved.get("user_id")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors below were locked byte-exact against the reference Python:
    //   cd /home/eins0fx/development/hermes-agent-port && python3 -c "
    //   import json; from gateway.platforms.qqbot import keyboards as k
    //   print(json.dumps(k.build_approval_keyboard('agent:main:qqbot:c2c:OPENID').to_dict(),
    //                    ensure_ascii=False, separators=(',', ':')))"

    const APPROVAL_PERM_JSON: &str = r#"{"content":{"rows":[{"buttons":[{"id":"allow","render_data":{"label":"✅ 允许一次","visited_label":"已允许","style":1},"action":{"type":1,"data":"approve:agent:main:qqbot:c2c:OPENID:allow-once","permission":{"type":2},"click_limit":1},"group_id":"approval"},{"id":"always","render_data":{"label":"⭐ 始终允许","visited_label":"已始终允许","style":1},"action":{"type":1,"data":"approve:agent:main:qqbot:c2c:OPENID:allow-always","permission":{"type":2},"click_limit":1},"group_id":"approval"},{"id":"deny","render_data":{"label":"❌ 拒绝","visited_label":"已拒绝","style":0},"action":{"type":1,"data":"approve:agent:main:qqbot:c2c:OPENID:deny","permission":{"type":2},"click_limit":1},"group_id":"approval"}]}]}}"#;

    const APPROVAL_NOPERM_JSON: &str = r#"{"content":{"rows":[{"buttons":[{"id":"allow","render_data":{"label":"✅ 允许一次","visited_label":"已允许","style":1},"action":{"type":1,"data":"approve:sk:allow-once","permission":{"type":2},"click_limit":1},"group_id":"approval"},{"id":"deny","render_data":{"label":"❌ 拒绝","visited_label":"已拒绝","style":0},"action":{"type":1,"data":"approve:sk:deny","permission":{"type":2},"click_limit":1},"group_id":"approval"}]}]}}"#;

    const UPDATE_KB_JSON: &str = r#"{"content":{"rows":[{"buttons":[{"id":"yes","render_data":{"label":"✓ 确认","visited_label":"已确认","style":1},"action":{"type":1,"data":"update_prompt:y","permission":{"type":2},"click_limit":1},"group_id":"update_prompt"},{"id":"no","render_data":{"label":"✗ 取消","visited_label":"已取消","style":0},"action":{"type":1,"data":"update_prompt:n","permission":{"type":2},"click_limit":1},"group_id":"update_prompt"}]}]}}"#;

    #[test]
    fn approval_keyboard_with_permanent_byte_exact() {
        let kb = build_approval_keyboard("agent:main:qqbot:c2c:OPENID", true);
        assert_eq!(serde_json::to_string(&kb).unwrap(), APPROVAL_PERM_JSON);
    }

    #[test]
    fn approval_keyboard_without_permanent_byte_exact() {
        let kb = build_approval_keyboard("sk", false);
        assert_eq!(serde_json::to_string(&kb).unwrap(), APPROVAL_NOPERM_JSON);
    }

    #[test]
    fn update_prompt_keyboard_byte_exact() {
        let kb = build_update_prompt_keyboard();
        assert_eq!(serde_json::to_string(&kb).unwrap(), UPDATE_KB_JSON);
    }

    #[test]
    fn permission_default_is_type_2() {
        assert_eq!(KeyboardButtonPermission::default().type_, 2);
    }

    // ── parse_approval_button_data ───────────────────────────────────

    #[test]
    fn parse_approval_nested_colons_greedy() {
        assert_eq!(
            parse_approval_button_data("approve:agent:main:qqbot:c2c:OPENID:allow-once"),
            Some((
                "agent:main:qqbot:c2c:OPENID".to_string(),
                "allow-once".to_string()
            ))
        );
    }

    #[test]
    fn parse_approval_simple() {
        assert_eq!(
            parse_approval_button_data("approve:sk:deny"),
            Some(("sk".to_string(), "deny".to_string()))
        );
        assert_eq!(
            parse_approval_button_data("approve:sk:allow-always"),
            Some(("sk".to_string(), "allow-always".to_string()))
        );
    }

    #[test]
    fn parse_approval_greedy_takes_trailing_decision() {
        // Python: ('foo:deny', 'deny') — the last token wins because `.+` is greedy.
        assert_eq!(
            parse_approval_button_data("approve:foo:deny:deny"),
            Some(("foo:deny".to_string(), "deny".to_string()))
        );
    }

    #[test]
    fn parse_approval_rejects_bad_inputs() {
        // All of these are None in Python.
        assert_eq!(parse_approval_button_data("approve:sk:bad"), None);
        assert_eq!(parse_approval_button_data("approve:sk:allow"), None);
        assert_eq!(parse_approval_button_data("nope"), None);
        assert_eq!(parse_approval_button_data(""), None);
        // Empty session key (`.+` needs at least one char).
        assert_eq!(parse_approval_button_data("approve::deny"), None);
        // Decision with no session key at all.
        assert_eq!(parse_approval_button_data("approve:allow-once"), None);
    }

    // ── parse_update_prompt_button_data ──────────────────────────────

    #[test]
    fn parse_update_prompt_round_trips() {
        assert_eq!(
            parse_update_prompt_button_data("update_prompt:y"),
            Some("y".to_string())
        );
        assert_eq!(
            parse_update_prompt_button_data("update_prompt:n"),
            Some("n".to_string())
        );
        assert_eq!(parse_update_prompt_button_data("update_prompt:x"), None);
        assert_eq!(parse_update_prompt_button_data(""), None);
        assert_eq!(parse_update_prompt_button_data("update_prompt:"), None);
    }

    // ── build_approval_text ──────────────────────────────────────────

    #[test]
    fn exec_text_full() {
        let req = ApprovalRequest {
            session_key: "sk".to_string(),
            title: "Run build".to_string(),
            command_preview: "cargo build --release".to_string(),
            cwd: "/home/x/proj".to_string(),
            description: "builds the thing".to_string(),
            timeout_sec: 90,
            ..Default::default()
        };
        let expected = "🔐 **命令执行审批**\n\n```\ncargo build --release\n```\n📁 目录: /home/x/proj\n📋 Run build\n📝 builds the thing\n\n⏱️ 超时: 90 秒";
        assert_eq!(build_approval_text(&req), expected);
    }

    #[test]
    fn exec_text_suppresses_title_equal_to_command() {
        let req = ApprovalRequest {
            session_key: "sk".to_string(),
            title: "ls".to_string(),
            command_preview: "ls".to_string(),
            timeout_sec: 120,
            ..Default::default()
        };
        let expected = "🔐 **命令执行审批**\n\n```\nls\n```\n\n⏱️ 超时: 120 秒";
        assert_eq!(build_approval_text(&req), expected);
    }

    #[test]
    fn plugin_text_critical() {
        let req = ApprovalRequest {
            session_key: "sk".to_string(),
            title: "Use tool".to_string(),
            description: "desc".to_string(),
            tool_name: "web_search".to_string(),
            severity: "critical".to_string(),
            timeout_sec: 60,
            ..Default::default()
        };
        let expected =
            "🔴 **审批请求**\n\n📋 Use tool\n📝 desc\n🔧 工具: web_search\n\n⏱️ 超时: 60 秒";
        assert_eq!(build_approval_text(&req), expected);
    }

    #[test]
    fn plugin_text_info_minimal() {
        let req = ApprovalRequest {
            session_key: "sk".to_string(),
            title: "Just a title".to_string(),
            severity: "info".to_string(),
            timeout_sec: 30,
            ..Default::default()
        };
        let expected = "🔵 **审批请求**\n\n📋 Just a title\n\n⏱️ 超时: 30 秒";
        assert_eq!(build_approval_text(&req), expected);
    }

    #[test]
    fn plugin_text_default_severity_icon() {
        let req = ApprovalRequest {
            session_key: "sk".to_string(),
            title: "T".to_string(),
            ..Default::default()
        };
        let expected = "🟡 **审批请求**\n\n📋 T\n\n⏱️ 超时: 120 秒";
        assert_eq!(build_approval_text(&req), expected);
    }

    // ── parse_interaction_event ──────────────────────────────────────

    #[test]
    fn interaction_event_parse_and_operator() {
        let raw: serde_json::Value = serde_json::json!({
            "id": "INT123",
            "data": {"type": 11, "resolved": {"button_data": "approve:sk:deny", "button_id": "deny", "user_id": "U9"}},
            "chat_type": 2,
            "user_openid": "UO",
            "group_openid": "GO",
            "group_member_openid": "GMO",
            "channel_id": "CH",
            "guild_id": "GU"
        });
        let ev = parse_interaction_event(&raw);
        assert_eq!(ev.id, "INT123");
        assert_eq!(ev.type_, 11);
        assert_eq!(ev.chat_type, 2);
        assert_eq!(ev.scene, "c2c");
        assert_eq!(ev.button_data, "approve:sk:deny");
        assert_eq!(ev.button_id, "deny");
        assert_eq!(ev.resolver_user_id, "U9");
        // group_member_openid wins.
        assert_eq!(ev.operator_openid(), "GMO");
    }

    #[test]
    fn interaction_event_operator_falls_through() {
        let ev = InteractionEvent {
            user_openid: "UO".to_string(),
            resolver_user_id: "RU".to_string(),
            ..Default::default()
        };
        assert_eq!(ev.operator_openid(), "UO");

        let ev2 = InteractionEvent {
            resolver_user_id: "RU".to_string(),
            ..Default::default()
        };
        assert_eq!(ev2.operator_openid(), "RU");
    }

    #[test]
    fn interaction_event_empty_raw_defaults() {
        let ev = parse_interaction_event(&serde_json::json!({}));
        assert_eq!(ev.id, "");
        assert_eq!(ev.type_, 0);
        assert_eq!(ev.chat_type, 0);
        assert_eq!(ev.scene, "guild");
        assert_eq!(ev.button_data, "");
    }
}
