//! Port of gateway/platforms/yuanbao_proto.py.
//!
// Public API is ahead of its callers (the yuanbao WebSocket adapter wires it).
#![allow(dead_code)]
//!
//! Yuanbao (Tencent AI) WebSocket protocol codec, hand-written protobuf
//! wire-format with no third-party protobuf dependency.
//!
//! Protocol layers:
//!   WebSocket frame
//!     -> ConnMsg (protobuf trpc.yuanbao.conn_common.ConnMsg)
//!          head: Head (cmd_type, cmd, seq_no, msg_id, module, ...)
//!          data: bytes (biz payload, standard protobuf)
//!                -> InboundMessagePush / SendC2CMessageReq / SendGroupMessageReq / ...
//!
//! The conn layer (ConnMsg) is itself standard protobuf, not a custom binary
//! framing. WebSocket carries one ConnMsg per frame (no packet-boundary problem),
//! so there is no magic/head_len/body_len framing here (that lives on quic/tcp).
//!
//! Everything below is a pure codec: varint / protobuf field parsing and the
//! encoders/decoders for each Yuanbao message. The Python module depends only on
//! logging + threading + time (no internal imports), so it ports cleanly.
//!
//! Dict-shaped decode results (inbound push, forwarded chat history, group info,
//! member list) are returned as `serde_json::Value` so the exact Python
//! empty-value filtering and structure are preserved and golden-testable. The
//! two low-level frame decoders (`decode_conn_msg` / `decode_biz_msg`) return
//! typed structs because they carry raw `bytes` payloads.
//!
//! Faithfulness note: Python's `_parse_fields` raises `ValueError` on an unknown
//! wire type or an over-long varint. `decode_conn_msg`/`decode_biz_msg` don't
//! catch that, so Python would propagate. This port degrades a hard parse error
//! in those two functions to empty fields (default struct) instead of panicking;
//! the `Option`-returning decoders match Python's broad `except` -> `None`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Value};

// ============================================================
// Debug switch
// ============================================================

const DEBUG_MODE: bool = false;

fn dbg_bytes(label: &str, data: &[u8]) {
    if DEBUG_MODE {
        let hex: Vec<String> = data.iter().take(64).map(|b| format!("{:02x}", b)).collect();
        let ellipsis = if data.len() > 64 { "..." } else { "" };
        tracing::debug!(
            "[yuanbao_proto] {} ({}B): {}{}",
            label,
            data.len(),
            hex.join(" "),
            ellipsis
        );
    }
}

// ============================================================
// Constants
// ============================================================

/// conn-layer message-type -> full protobuf type name (ConnMsg.Head.cmd_type).
pub const PB_MSG_TYPES: &[(&str, &str)] = &[
    ("ConnMsg", "trpc.yuanbao.conn_common.ConnMsg"),
    ("AuthBindReq", "trpc.yuanbao.conn_common.AuthBindReq"),
    ("AuthBindRsp", "trpc.yuanbao.conn_common.AuthBindRsp"),
    ("PingReq", "trpc.yuanbao.conn_common.PingReq"),
    ("PingRsp", "trpc.yuanbao.conn_common.PingRsp"),
    ("KickoutMsg", "trpc.yuanbao.conn_common.KickoutMsg"),
    ("DirectedPush", "trpc.yuanbao.conn_common.DirectedPush"),
    ("PushMsg", "trpc.yuanbao.conn_common.PushMsg"),
];

// cmd_type enum
pub const CMD_TYPE_REQUEST: u32 = 0; // upstream request
pub const CMD_TYPE_RESPONSE: u32 = 1; // response to an upstream request
pub const CMD_TYPE_PUSH: u32 = 2; // downstream push
pub const CMD_TYPE_PUSH_ACK: u32 = 3; // ack of a downstream push

// Built-in command words
pub const CMD_AUTH_BIND: &str = "auth-bind";
pub const CMD_PING: &str = "ping";
pub const CMD_KICKOUT: &str = "kickout";
pub const CMD_UPDATE_META: &str = "update-meta";

// Built-in module names
pub const MODULE_CONN_ACCESS: &str = "conn_access";

// biz service/method mapping. The TS client uses the short package name.
const BIZ_PKG: &str = "yuanbao_openclaw_proxy";

/// biz service short-name -> `<pkg>.<name>` full path.
pub const BIZ_SERVICES: &[(&str, &str)] = &[
    (
        "InboundMessagePush",
        "yuanbao_openclaw_proxy.InboundMessagePush",
    ),
    (
        "SendC2CMessageReq",
        "yuanbao_openclaw_proxy.SendC2CMessageReq",
    ),
    (
        "SendC2CMessageRsp",
        "yuanbao_openclaw_proxy.SendC2CMessageRsp",
    ),
    (
        "SendGroupMessageReq",
        "yuanbao_openclaw_proxy.SendGroupMessageReq",
    ),
    (
        "SendGroupMessageRsp",
        "yuanbao_openclaw_proxy.SendGroupMessageRsp",
    ),
    (
        "QueryGroupInfoReq",
        "yuanbao_openclaw_proxy.QueryGroupInfoReq",
    ),
    (
        "QueryGroupInfoRsp",
        "yuanbao_openclaw_proxy.QueryGroupInfoRsp",
    ),
    (
        "GetGroupMemberListReq",
        "yuanbao_openclaw_proxy.GetGroupMemberListReq",
    ),
    (
        "GetGroupMemberListRsp",
        "yuanbao_openclaw_proxy.GetGroupMemberListRsp",
    ),
    (
        "SendPrivateHeartbeatReq",
        "yuanbao_openclaw_proxy.SendPrivateHeartbeatReq",
    ),
    (
        "SendPrivateHeartbeatRsp",
        "yuanbao_openclaw_proxy.SendPrivateHeartbeatRsp",
    ),
    (
        "SendGroupHeartbeatReq",
        "yuanbao_openclaw_proxy.SendGroupHeartbeatReq",
    ),
    (
        "SendGroupHeartbeatRsp",
        "yuanbao_openclaw_proxy.SendGroupHeartbeatRsp",
    ),
];

/// openclaw instance_id (fixed value 17).
pub const HERMES_INSTANCE_ID: u32 = 17;

// Reply-Heartbeat state constants
pub const WS_HEARTBEAT_RUNNING: u64 = 1;
pub const WS_HEARTBEAT_FINISH: u64 = 2;

// ============================================================
// Sequence-number generation
// ============================================================

// uint32 counter; wraps mod 2^32 like Python's `(counter + 1) & (2**32 - 1)`.
static SEQ_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Generate an increasing sequence number (thread-safe, wraps at uint32).
/// Returns the pre-increment value, matching the Python implementation.
pub fn next_seq_no() -> u32 {
    // fetch_add returns the previous value and wraps on overflow, which is
    // exactly `val = counter; counter = (counter + 1) & 0xFFFFFFFF; return val`.
    SEQ_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ============================================================
// Protobuf wire-format basics (hand-written)
// ============================================================

// wire types
const WT_VARINT: u8 = 0;
const WT_64BIT: u8 = 1;
const WT_LEN: u8 = 2;
const WT_32BIT: u8 = 5;

/// Raised for genuinely malformed protobuf (unknown wire type / over-long
/// varint), mirroring Python's `ValueError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoError(pub String);

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ProtoError {}

/// Encode a non-negative integer as a protobuf varint.
///
/// Python masks negatives to 64-bit two's complement before encoding; in Rust a
/// signed value is masked at the call site by casting to `u64`
/// (`-1i64 as u64 == 0xFFFF_FFFF_FFFF_FFFF`), matching that behaviour.
fn encode_varint(value: u64) -> Vec<u8> {
    let mut value = value;
    let mut out = Vec::new();
    loop {
        let bits = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            out.push(bits | 0x80);
        } else {
            out.push(bits);
            break;
        }
    }
    out
}

/// Decode a varint from `data[pos..]`, returning `(value, new_pos)`.
fn decode_varint(data: &[u8], pos: usize) -> Result<(u64, usize), ProtoError> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    let mut pos = pos;
    while pos < data.len() {
        let b = data[pos];
        pos += 1;
        result |= ((b & 0x7F) as u64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            break;
        }
        if shift >= 64 {
            return Err(ProtoError("varint too long".to_string()));
        }
    }
    Ok((result, pos))
}

/// Encode a protobuf field (tag + value bytes).
fn encode_field(field_number: u64, wire_type: u8, value: &[u8]) -> Vec<u8> {
    let tag = (field_number << 3) | (wire_type as u64);
    let mut out = encode_varint(tag);
    out.extend_from_slice(value);
    out
}

/// Encode the value part of a protobuf string field (length-prefixed UTF-8).
fn encode_string(s: &str) -> Vec<u8> {
    let encoded = s.as_bytes();
    let mut out = encode_varint(encoded.len() as u64);
    out.extend_from_slice(encoded);
    out
}

/// Encode the value part of a protobuf bytes field (length-prefixed).
fn encode_bytes(b: &[u8]) -> Vec<u8> {
    let mut out = encode_varint(b.len() as u64);
    out.extend_from_slice(b);
    out
}

/// Encode a nested message (length-prefixed). Same wire shape as bytes.
fn encode_message(b: &[u8]) -> Vec<u8> {
    encode_bytes(b)
}

/// A parsed value: a varint (int) or a length-delimited / fixed-width byte run.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FieldVal {
    Varint(u64),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone)]
struct RawField {
    number: u64,
    wire_type: u8,
    value: FieldVal,
}

/// Parse every field of a protobuf message.
fn parse_fields(data: &[u8]) -> Result<Vec<RawField>, ProtoError> {
    let mut fields = Vec::new();
    let mut pos = 0usize;
    let n = data.len();
    while pos < n {
        let (tag, p) = decode_varint(data, pos)?;
        pos = p;
        let field_number = tag >> 3;
        let wire_type = (tag & 0x07) as u8;
        match wire_type {
            WT_VARINT => {
                let (val, p) = decode_varint(data, pos)?;
                pos = p;
                fields.push(RawField {
                    number: field_number,
                    wire_type,
                    value: FieldVal::Varint(val),
                });
            }
            WT_LEN => {
                let (length, p) = decode_varint(data, pos)?;
                pos = p;
                // Python slices data[pos:pos+length] (clamped) then advances pos
                // by the full length, so an overshoot just ends the loop.
                let end = pos.saturating_add(length as usize);
                let slice_end = end.min(n);
                let val = data[pos..slice_end].to_vec();
                pos = end;
                fields.push(RawField {
                    number: field_number,
                    wire_type,
                    value: FieldVal::Bytes(val),
                });
            }
            WT_64BIT => {
                let end = pos.saturating_add(8);
                let slice_end = end.min(n);
                let val = data[pos..slice_end].to_vec();
                pos = end;
                fields.push(RawField {
                    number: field_number,
                    wire_type,
                    value: FieldVal::Bytes(val),
                });
            }
            WT_32BIT => {
                let end = pos.saturating_add(4);
                let slice_end = end.min(n);
                let val = data[pos..slice_end].to_vec();
                pos = end;
                fields.push(RawField {
                    number: field_number,
                    wire_type,
                    value: FieldVal::Bytes(val),
                });
            }
            other => {
                return Err(ProtoError(format!(
                    "unknown wire type {} at pos {}",
                    other,
                    pos - 1
                )));
            }
        }
    }
    Ok(fields)
}

/// `{field_number: [(wire_type, value), ...]}`. A repeated field has several
/// entries; entry order follows parse order (so "first" is the first on wire).
type FDict = std::collections::BTreeMap<u64, Vec<(u8, FieldVal)>>;

fn fields_to_dict(fields: Vec<RawField>) -> FDict {
    let mut d: FDict = FDict::new();
    for f in fields {
        d.entry(f.number).or_default().push((f.wire_type, f.value));
    }
    d
}

/// Parse + index in one shot, tolerating a hard parse error by yielding an empty
/// dict (used by the infallible conn/biz decoders).
fn parse_dict_lenient(data: &[u8]) -> FDict {
    match parse_fields(data) {
        Ok(fields) => fields_to_dict(fields),
        Err(_) => FDict::new(),
    }
}

fn get_string(f: &FDict, fnum: u64, default: &str) -> String {
    if let Some(entries) = f.get(&fnum) {
        if let Some((wt, val)) = entries.first() {
            if *wt == WT_LEN {
                if let FieldVal::Bytes(b) = val {
                    return String::from_utf8_lossy(b).into_owned();
                }
            }
        }
    }
    default.to_string()
}

fn get_varint(f: &FDict, fnum: u64, default: u64) -> u64 {
    if let Some(entries) = f.get(&fnum) {
        if let Some((wt, val)) = entries.first() {
            if *wt == WT_VARINT {
                if let FieldVal::Varint(v) = val {
                    return *v;
                }
            }
        }
    }
    default
}

fn get_bytes(f: &FDict, fnum: u64) -> Vec<u8> {
    if let Some(entries) = f.get(&fnum) {
        if let Some((wt, val)) = entries.first() {
            if *wt == WT_LEN {
                if let FieldVal::Bytes(b) = val {
                    return b.clone();
                }
            }
        }
    }
    Vec::new()
}

fn get_repeated_bytes(f: &FDict, fnum: u64) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if let Some(entries) = f.get(&fnum) {
        for (wt, val) in entries {
            if *wt == WT_LEN {
                if let FieldVal::Bytes(b) = val {
                    out.push(b.clone());
                }
            }
        }
    }
    out
}

// ============================================================
// serde_json helpers mirroring Python dict truthiness / str()/int()
// ============================================================

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i != 0
            } else if let Some(u) = n.as_u64() {
                u != 0
            } else {
                n.as_f64().map(|f| f != 0.0).unwrap_or(false)
            }
        }
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Mirror Python `str(v)` for the value kinds that appear in these dicts.
fn py_str(v: &Value) -> String {
    match v {
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
        _ => v.to_string(),
    }
}

/// Mirror Python `int(v)` for the value kinds that appear in these dicts.
fn py_int(v: &Value) -> u64 {
    match v {
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                u
            } else if let Some(i) = n.as_i64() {
                i as u64
            } else if let Some(f) = n.as_f64() {
                f as i64 as u64
            } else {
                0
            }
        }
        Value::String(s) => s.trim().parse::<i64>().map(|x| x as u64).unwrap_or(0),
        Value::Bool(b) if *b => 1,
        _ => 0,
    }
}

fn obj_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object().and_then(|o| o.get(key))
}

// ============================================================
// ConnMsg layer encode/decode
// ============================================================

/// Decoded ConnMsg.Head.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Head {
    pub cmd_type: u64,
    pub cmd: String,
    pub seq_no: u64,
    pub msg_id: String,
    pub module: String,
    pub need_ack: bool,
    pub status: u64,
}

/// Decoded ConnMsg (simplified view).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnMsg {
    pub msg_type: u64,
    pub seq_no: u64,
    pub data: Vec<u8>,
    pub head: Head,
}

/// Decoded biz-layer view of a ConnMsg.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BizMsg {
    pub service: String,
    pub method: String,
    pub req_id: String,
    pub body: Vec<u8>,
    pub is_response: bool,
    pub head: Head,
}

#[allow(clippy::too_many_arguments)]
fn encode_head(
    cmd_type: u32,
    cmd: &str,
    seq_no: u32,
    msg_id: &str,
    module: &str,
    need_ack: bool,
    status: i64,
) -> Vec<u8> {
    let mut buf = Vec::new();
    if cmd_type != 0 {
        buf.extend(encode_field(1, WT_VARINT, &encode_varint(cmd_type as u64)));
    }
    if !cmd.is_empty() {
        buf.extend(encode_field(2, WT_LEN, &encode_string(cmd)));
    }
    if seq_no != 0 {
        buf.extend(encode_field(3, WT_VARINT, &encode_varint(seq_no as u64)));
    }
    if !msg_id.is_empty() {
        buf.extend(encode_field(4, WT_LEN, &encode_string(msg_id)));
    }
    if !module.is_empty() {
        buf.extend(encode_field(5, WT_LEN, &encode_string(module)));
    }
    if need_ack {
        buf.extend(encode_field(6, WT_VARINT, &encode_varint(1)));
    }
    if status != 0 {
        // `status & 0xFFFF_FFFF_FFFF_FFFF` == `status as u64` in Rust.
        buf.extend(encode_field(10, WT_VARINT, &encode_varint(status as u64)));
    }
    buf
}

fn decode_head(data: &[u8]) -> Head {
    let fdict = parse_dict_lenient(data);
    Head {
        cmd_type: get_varint(&fdict, 1, 0),
        cmd: get_string(&fdict, 2, ""),
        seq_no: get_varint(&fdict, 3, 0),
        msg_id: get_string(&fdict, 4, ""),
        module: get_string(&fdict, 5, ""),
        need_ack: get_varint(&fdict, 6, 0) != 0,
        status: get_varint(&fdict, 10, 0),
    }
}

/// Encode a ConnMsg (simplified interface).
pub fn encode_conn_msg(msg_type: u32, seq_no: u32, data: &[u8]) -> Vec<u8> {
    let head_bytes = encode_head(msg_type, "", seq_no, "", "", false, 0);
    let mut buf = encode_field(1, WT_LEN, &encode_message(&head_bytes));
    if !data.is_empty() {
        buf.extend(encode_field(2, WT_LEN, &encode_bytes(data)));
    }
    dbg_bytes("encode_conn_msg", &buf);
    buf
}

/// Decode a ConnMsg into `{msg_type, seq_no, data, head}`.
pub fn decode_conn_msg(data: &[u8]) -> ConnMsg {
    dbg_bytes("decode_conn_msg", data);
    let fdict = parse_dict_lenient(data);
    let head_bytes = get_bytes(&fdict, 1);
    let payload = get_bytes(&fdict, 2);
    let head = if !head_bytes.is_empty() {
        decode_head(&head_bytes)
    } else {
        Head::default()
    };
    ConnMsg {
        msg_type: head.cmd_type,
        seq_no: head.seq_no,
        data: payload,
        head,
    }
}

/// Encode a full ConnMsg with all head fields.
#[allow(clippy::too_many_arguments)]
pub fn encode_conn_msg_full(
    cmd_type: u32,
    cmd: &str,
    seq_no: u32,
    msg_id: &str,
    module: &str,
    data: &[u8],
    need_ack: bool,
) -> Vec<u8> {
    let head_bytes = encode_head(cmd_type, cmd, seq_no, msg_id, module, need_ack, 0);
    let mut buf = encode_field(1, WT_LEN, &encode_message(&head_bytes));
    if !data.is_empty() {
        buf.extend(encode_field(2, WT_LEN, &encode_bytes(data)));
    }
    dbg_bytes("encode_conn_msg_full", &buf);
    buf
}

// ============================================================
// BizMsg layer (wraps a biz payload into a ConnMsg)
// ============================================================

/// Wrap a biz payload as ConnMsg bytes (`head.cmd = method`, `head.module = service`).
pub fn encode_biz_msg(service: &str, method: &str, req_id: &str, body: &[u8]) -> Vec<u8> {
    encode_conn_msg_full(
        CMD_TYPE_REQUEST,
        method,
        next_seq_no(),
        req_id,
        service,
        body,
        false,
    )
}

/// Decode ConnMsg bytes into a biz-layer view.
pub fn decode_biz_msg(data: &[u8]) -> BizMsg {
    let result = decode_conn_msg(data);
    let head = result.head;
    BizMsg {
        service: head.module.clone(),
        method: head.cmd.clone(),
        req_id: head.msg_id.clone(),
        body: result.data,
        is_response: head.cmd_type == CMD_TYPE_RESPONSE as u64,
        head,
    }
}

// ============================================================
// biz protobuf message encode/decode
// ============================================================

// ---------- MsgContent ----------

fn encode_map_entry(key: &str, value: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    if !key.is_empty() {
        buf.extend(encode_field(1, WT_LEN, &encode_string(key)));
    }
    if !value.is_empty() {
        buf.extend(encode_field(2, WT_LEN, &encode_string(value)));
    }
    buf
}

fn decode_map_entry(data: &[u8]) -> (String, String) {
    let fdict = parse_dict_lenient(data);
    (get_string(&fdict, 1, ""), get_string(&fdict, 2, ""))
}

fn encode_msg_content(content: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    // string fields
    for (fnum, key) in [
        (1u64, "text"),
        (2, "uuid"),
        (4, "data"),
        (5, "desc"),
        (6, "ext"),
        (7, "sound"),
        (10, "url"),
        (12, "file_name"),
    ] {
        if let Some(v) = obj_get(content, key) {
            if is_truthy(v) {
                buf.extend(encode_field(fnum, WT_LEN, &encode_string(&py_str(v))));
            }
        }
    }
    // varint fields
    for (fnum, key) in [(3u64, "image_format"), (9, "index"), (11, "file_size")] {
        if let Some(v) = obj_get(content, key) {
            if is_truthy(v) {
                buf.extend(encode_field(fnum, WT_VARINT, &encode_varint(py_int(v))));
            }
        }
    }
    // image_info_array (repeated)
    if let Some(Value::Array(imgs)) = obj_get(content, "image_info_array") {
        for img in imgs {
            let mut img_buf = Vec::new();
            for (ifn, ikey) in [(1u64, "type"), (2, "size"), (3, "width"), (4, "height")] {
                if let Some(iv) = obj_get(img, ikey) {
                    if is_truthy(iv) {
                        img_buf.extend(encode_field(ifn, WT_VARINT, &encode_varint(py_int(iv))));
                    }
                }
            }
            if let Some(u) = obj_get(img, "url") {
                if is_truthy(u) {
                    img_buf.extend(encode_field(5, WT_LEN, &encode_string(&py_str(u))));
                }
            }
            buf.extend(encode_field(8, WT_LEN, &encode_message(&img_buf)));
        }
    }
    // ext_map (map<string,string>, field 999) as repeated message entries
    if let Some(Value::Object(ext_map)) = obj_get(content, "ext_map") {
        for (k, v) in ext_map {
            let entry_bytes = encode_map_entry(k, &py_str(v));
            buf.extend(encode_field(999, WT_LEN, &encode_message(&entry_bytes)));
        }
    }
    buf
}

fn decode_msg_content(data: &[u8]) -> Value {
    let fdict = parse_dict_lenient(data);
    let mut content = Map::new();
    for (fnum, key) in [
        (1u64, "text"),
        (2, "uuid"),
        (4, "data"),
        (5, "desc"),
        (6, "ext"),
        (7, "sound"),
        (10, "url"),
        (12, "file_name"),
    ] {
        let v = get_string(&fdict, fnum, "");
        if !v.is_empty() {
            content.insert(key.to_string(), Value::String(v));
        }
    }
    for (fnum, key) in [(3u64, "image_format"), (9, "index"), (11, "file_size")] {
        let v = get_varint(&fdict, fnum, 0);
        if v != 0 {
            content.insert(key.to_string(), Value::from(v));
        }
    }
    let mut imgs: Vec<Value> = Vec::new();
    for img_bytes in get_repeated_bytes(&fdict, 8) {
        let ifdict = parse_dict_lenient(&img_bytes);
        let mut img = Map::new();
        for (ifn, ikey) in [(1u64, "type"), (2, "size"), (3, "width"), (4, "height")] {
            let iv = get_varint(&ifdict, ifn, 0);
            if iv != 0 {
                img.insert(ikey.to_string(), Value::from(iv));
            }
        }
        let url = get_string(&ifdict, 5, "");
        if !url.is_empty() {
            img.insert("url".to_string(), Value::String(url));
        }
        if !img.is_empty() {
            imgs.push(Value::Object(img));
        }
    }
    if !imgs.is_empty() {
        content.insert("image_info_array".to_string(), Value::Array(imgs));
    }
    let mut ext_map = Map::new();
    for entry_bytes in get_repeated_bytes(&fdict, 999) {
        let (k, v) = decode_map_entry(&entry_bytes);
        if !k.is_empty() {
            ext_map.insert(k, Value::String(v));
        }
    }
    if !ext_map.is_empty() {
        content.insert("ext_map".to_string(), Value::Object(ext_map));
    }
    Value::Object(content)
}

// ---------- MsgBodyElement ----------

fn encode_msg_body_element(element: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    let msg_type = obj_get(element, "msg_type").map(py_str).unwrap_or_default();
    if !msg_type.is_empty() {
        buf.extend(encode_field(1, WT_LEN, &encode_string(&msg_type)));
    }
    if let Some(content) = obj_get(element, "msg_content") {
        if is_truthy(content) {
            let content_bytes = encode_msg_content(content);
            buf.extend(encode_field(2, WT_LEN, &encode_message(&content_bytes)));
        }
    }
    buf
}

fn decode_msg_body_element(data: &[u8]) -> Value {
    let fdict = parse_dict_lenient(data);
    let msg_type = get_string(&fdict, 1, "");
    let content_bytes = get_bytes(&fdict, 2);
    let content = if !content_bytes.is_empty() {
        decode_msg_content(&content_bytes)
    } else {
        Value::Object(Map::new())
    };
    let mut m = Map::new();
    m.insert("msg_type".to_string(), Value::String(msg_type));
    m.insert("msg_content".to_string(), content);
    Value::Object(m)
}

// ---------- LogInfoExt / ImMsgSeq ----------

fn encode_log_ext(trace_id: &str) -> Vec<u8> {
    if trace_id.is_empty() {
        return Vec::new();
    }
    encode_field(1, WT_LEN, &encode_string(trace_id))
}

fn decode_log_ext(data: &[u8]) -> String {
    let fdict = parse_dict_lenient(data);
    get_string(&fdict, 1, "")
}

fn decode_im_msg_seq(data: &[u8]) -> Value {
    let fdict = parse_dict_lenient(data);
    let mut m = Map::new();
    m.insert("msg_seq".to_string(), Value::from(get_varint(&fdict, 1, 0)));
    m.insert(
        "msg_id".to_string(),
        Value::String(get_string(&fdict, 2, "")),
    );
    Value::Object(m)
}

// ============================================================
// Inbound message parsing
// ============================================================

/// Parse an InboundMessagePush biz payload. Returns `None` on any parse failure.
///
/// The returned object drops empty fields (mirroring Python's truthiness filter)
/// but always keeps `msg_body` and `msg_seq`.
pub fn decode_inbound_push(data: &[u8]) -> Option<Value> {
    dbg_bytes("decode_inbound_push input", data);
    let fields = match parse_fields(data) {
        Ok(f) => f,
        Err(e) => {
            if DEBUG_MODE {
                tracing::debug!("[yuanbao_proto] decode_inbound_push failed: {}", e);
            }
            return None;
        }
    };
    let fdict = fields_to_dict(fields);

    let mut msg_body: Vec<Value> = Vec::new();
    for el_bytes in get_repeated_bytes(&fdict, 13) {
        msg_body.push(decode_msg_body_element(&el_bytes));
    }

    let log_ext_bytes = get_bytes(&fdict, 20);
    let trace_id = if !log_ext_bytes.is_empty() {
        decode_log_ext(&log_ext_bytes)
    } else {
        String::new()
    };

    let recall_raw = get_repeated_bytes(&fdict, 17);
    let recall_list: Vec<Value> = recall_raw.iter().map(|b| decode_im_msg_seq(b)).collect();
    let recall_value = if recall_list.is_empty() {
        Value::Null
    } else {
        Value::Array(recall_list)
    };

    let mut result = Map::new();
    result.insert(
        "callback_command".to_string(),
        Value::String(get_string(&fdict, 1, "")),
    );
    result.insert(
        "from_account".to_string(),
        Value::String(get_string(&fdict, 2, "")),
    );
    result.insert(
        "to_account".to_string(),
        Value::String(get_string(&fdict, 3, "")),
    );
    result.insert(
        "sender_nickname".to_string(),
        Value::String(get_string(&fdict, 4, "")),
    );
    result.insert(
        "group_id".to_string(),
        Value::String(get_string(&fdict, 5, "")),
    );
    result.insert(
        "group_code".to_string(),
        Value::String(get_string(&fdict, 6, "")),
    );
    result.insert(
        "group_name".to_string(),
        Value::String(get_string(&fdict, 7, "")),
    );
    result.insert("msg_seq".to_string(), Value::from(get_varint(&fdict, 8, 0)));
    result.insert(
        "msg_random".to_string(),
        Value::from(get_varint(&fdict, 9, 0)),
    );
    result.insert(
        "msg_time".to_string(),
        Value::from(get_varint(&fdict, 10, 0)),
    );
    result.insert(
        "msg_key".to_string(),
        Value::String(get_string(&fdict, 11, "")),
    );
    result.insert(
        "msg_id".to_string(),
        Value::String(get_string(&fdict, 12, "")),
    );
    result.insert("msg_body".to_string(), Value::Array(msg_body));
    result.insert(
        "cloud_custom_data".to_string(),
        Value::String(get_string(&fdict, 14, "")),
    );
    result.insert(
        "event_time".to_string(),
        Value::from(get_varint(&fdict, 15, 0)),
    );
    result.insert(
        "bot_owner_id".to_string(),
        Value::String(get_string(&fdict, 16, "")),
    );
    result.insert("recall_msg_seq_list".to_string(), recall_value);
    result.insert(
        "claw_msg_type".to_string(),
        Value::from(get_varint(&fdict, 18, 0)),
    );
    result.insert(
        "private_from_group_code".to_string(),
        Value::String(get_string(&fdict, 19, "")),
    );
    result.insert("trace_id".to_string(), Value::String(trace_id));

    // Drop empty values, but always keep msg_body and msg_seq.
    let filtered: Map<String, Value> = result
        .into_iter()
        .filter(|(k, v)| is_truthy(v) || k == "msg_body" || k == "msg_seq")
        .collect();
    Some(Value::Object(filtered))
}

// ============================================================
// WeChat forwarded chat-history (ForwardMsgData)
// ============================================================

fn decode_forward_multimedia(data: &[u8]) -> Value {
    let fdict = parse_dict_lenient(data);
    let mut media = Map::new();
    let mtype = get_string(&fdict, 1, "");
    if !mtype.is_empty() {
        media.insert("type".to_string(), Value::String(mtype));
    }
    let url = get_string(&fdict, 2, "");
    if !url.is_empty() {
        media.insert("url".to_string(), Value::String(url));
    }
    let file_name = get_string(&fdict, 4, "");
    if !file_name.is_empty() {
        media.insert("file_name".to_string(), Value::String(file_name));
    }
    let file_size = get_varint(&fdict, 5, 0);
    if file_size != 0 {
        media.insert("file_size".to_string(), Value::from(file_size));
    }
    let media_id = get_string(&fdict, 15, "");
    if !media_id.is_empty() {
        media.insert("media_id".to_string(), Value::String(media_id));
    }
    Value::Object(media)
}

fn decode_forward_msg_content(data: &[u8]) -> Value {
    let fdict = parse_dict_lenient(data);
    let mut content = Map::new();
    content.insert("type".to_string(), Value::from(get_varint(&fdict, 1, 0)));
    let text = get_string(&fdict, 2, "");
    if !text.is_empty() {
        content.insert("text".to_string(), Value::String(text));
    }
    let multimedia: Vec<Value> = get_repeated_bytes(&fdict, 3)
        .iter()
        .map(|b| decode_forward_multimedia(b))
        .collect();
    if !multimedia.is_empty() {
        content.insert("multimedia".to_string(), Value::Array(multimedia));
    }
    Value::Object(content)
}

fn decode_forward_msg(data: &[u8]) -> Value {
    let fdict = parse_dict_lenient(data);
    let mut m = Map::new();
    m.insert(
        "sender".to_string(),
        Value::String(get_string(&fdict, 1, "")),
    );
    m.insert("time".to_string(), Value::from(get_varint(&fdict, 2, 0)));
    m.insert(
        "plainText".to_string(),
        Value::String(get_string(&fdict, 3, "")),
    );
    let msg_content: Vec<Value> = get_repeated_bytes(&fdict, 4)
        .iter()
        .map(|b| decode_forward_msg_content(b))
        .collect();
    m.insert("msgContent".to_string(), Value::Array(msg_content));
    Value::Object(m)
}

/// Parse ForwardMsgData protobuf bytes (the base64-decoded ext_map value).
/// Returns `None` on parse failure.
pub fn decode_forward_msg_data(data: &[u8]) -> Option<Value> {
    let fields = match parse_fields(data) {
        Ok(f) => f,
        Err(e) => {
            if DEBUG_MODE {
                tracing::debug!("[yuanbao_proto] decode_forward_msg_data failed: {}", e);
            }
            return None;
        }
    };
    let fdict = fields_to_dict(fields);
    let mut m = Map::new();
    m.insert(
        "sub_type".to_string(),
        Value::from(get_varint(&fdict, 1, 0)),
    );
    m.insert(
        "begin_time".to_string(),
        Value::from(get_varint(&fdict, 2, 0)),
    );
    m.insert(
        "end_time".to_string(),
        Value::from(get_varint(&fdict, 3, 0)),
    );
    m.insert(
        "nick_name".to_string(),
        Value::String(get_string(&fdict, 4, "")),
    );
    let msg: Vec<Value> = get_repeated_bytes(&fdict, 5)
        .iter()
        .map(|b| decode_forward_msg(b))
        .collect();
    m.insert("msg".to_string(), Value::Array(msg));
    Some(Value::Object(m))
}

fn encode_forward_multimedia(media: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    for (fnum, key) in [
        (1u64, "type"),
        (2, "url"),
        (4, "file_name"),
        (15, "media_id"),
    ] {
        if let Some(v) = obj_get(media, key) {
            if is_truthy(v) {
                buf.extend(encode_field(fnum, WT_LEN, &encode_string(&py_str(v))));
            }
        }
    }
    for (fnum, key) in [(5u64, "file_size"), (6, "width"), (7, "height")] {
        if let Some(v) = obj_get(media, key) {
            if is_truthy(v) {
                buf.extend(encode_field(fnum, WT_VARINT, &encode_varint(py_int(v))));
            }
        }
    }
    buf
}

fn encode_forward_msg_content(content: &Value) -> Vec<u8> {
    let type_v = obj_get(content, "type").map(py_int).unwrap_or(0);
    let mut buf = encode_field(1, WT_VARINT, &encode_varint(type_v));
    if let Some(text) = obj_get(content, "text") {
        if is_truthy(text) {
            buf.extend(encode_field(2, WT_LEN, &encode_string(&py_str(text))));
        }
    }
    if let Some(Value::Array(multimedia)) = obj_get(content, "multimedia") {
        for media in multimedia {
            buf.extend(encode_field(
                3,
                WT_LEN,
                &encode_message(&encode_forward_multimedia(media)),
            ));
        }
    }
    buf
}

fn encode_forward_msg(msg: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(sender) = obj_get(msg, "sender") {
        if is_truthy(sender) {
            buf.extend(encode_field(1, WT_LEN, &encode_string(&py_str(sender))));
        }
    }
    if let Some(time_val) = obj_get(msg, "time") {
        if is_truthy(time_val) {
            buf.extend(encode_field(2, WT_VARINT, &encode_varint(py_int(time_val))));
        }
    }
    if let Some(plain) = obj_get(msg, "plainText") {
        if is_truthy(plain) {
            buf.extend(encode_field(3, WT_LEN, &encode_string(&py_str(plain))));
        }
    }
    if let Some(Value::Array(contents)) = obj_get(msg, "msgContent") {
        for mc in contents {
            buf.extend(encode_field(
                4,
                WT_LEN,
                &encode_message(&encode_forward_msg_content(mc)),
            ));
        }
    }
    buf
}

/// Encode ForwardMsgData protobuf bytes (inverse of `decode_forward_msg_data`).
/// Mainly for building mock / test data.
pub fn encode_forward_msg_data(data: &Value) -> Vec<u8> {
    let sub_type = obj_get(data, "sub_type").map(py_int).unwrap_or(0);
    let mut buf = encode_field(1, WT_VARINT, &encode_varint(sub_type));
    for (fnum, key) in [(2u64, "begin_time"), (3, "end_time")] {
        if let Some(v) = obj_get(data, key) {
            if is_truthy(v) {
                buf.extend(encode_field(fnum, WT_VARINT, &encode_varint(py_int(v))));
            }
        }
    }
    if let Some(nick) = obj_get(data, "nick_name") {
        if is_truthy(nick) {
            buf.extend(encode_field(4, WT_LEN, &encode_string(&py_str(nick))));
        }
    }
    if let Some(Value::Array(msgs)) = obj_get(data, "msg") {
        for msg in msgs {
            buf.extend(encode_field(
                5,
                WT_LEN,
                &encode_message(&encode_forward_msg(msg)),
            ));
        }
    }
    buf
}

// ============================================================
// Outbound message encoding
// ============================================================

#[allow(clippy::too_many_arguments)]
fn encode_send_c2c_req(
    to_account: &str,
    from_account: &str,
    msg_body: &[Value],
    msg_id: &str,
    msg_random: u64,
    msg_seq: Option<u64>,
    group_code: &str,
    trace_id: &str,
) -> Vec<u8> {
    let mut buf = Vec::new();
    if !msg_id.is_empty() {
        buf.extend(encode_field(1, WT_LEN, &encode_string(msg_id)));
    }
    buf.extend(encode_field(2, WT_LEN, &encode_string(to_account)));
    if !from_account.is_empty() {
        buf.extend(encode_field(3, WT_LEN, &encode_string(from_account)));
    }
    if msg_random != 0 {
        buf.extend(encode_field(4, WT_VARINT, &encode_varint(msg_random)));
    }
    for el in msg_body {
        let el_bytes = encode_msg_body_element(el);
        buf.extend(encode_field(5, WT_LEN, &encode_message(&el_bytes)));
    }
    if !group_code.is_empty() {
        buf.extend(encode_field(6, WT_LEN, &encode_string(group_code)));
    }
    if let Some(seq) = msg_seq {
        buf.extend(encode_field(7, WT_VARINT, &encode_varint(seq)));
    }
    if !trace_id.is_empty() {
        let log_bytes = encode_log_ext(trace_id);
        buf.extend(encode_field(8, WT_LEN, &encode_message(&log_bytes)));
    }
    buf
}

#[allow(clippy::too_many_arguments)]
fn encode_send_group_req(
    group_code: &str,
    from_account: &str,
    msg_body: &[Value],
    msg_id: &str,
    to_account: &str,
    random: &str,
    msg_seq: Option<u64>,
    ref_msg_id: &str,
    trace_id: &str,
) -> Vec<u8> {
    let mut buf = Vec::new();
    if !msg_id.is_empty() {
        buf.extend(encode_field(1, WT_LEN, &encode_string(msg_id)));
    }
    buf.extend(encode_field(2, WT_LEN, &encode_string(group_code)));
    if !from_account.is_empty() {
        buf.extend(encode_field(3, WT_LEN, &encode_string(from_account)));
    }
    if !to_account.is_empty() {
        buf.extend(encode_field(4, WT_LEN, &encode_string(to_account)));
    }
    if !random.is_empty() {
        buf.extend(encode_field(5, WT_LEN, &encode_string(random)));
    }
    for el in msg_body {
        let el_bytes = encode_msg_body_element(el);
        buf.extend(encode_field(6, WT_LEN, &encode_message(&el_bytes)));
    }
    if !ref_msg_id.is_empty() {
        buf.extend(encode_field(7, WT_LEN, &encode_string(ref_msg_id)));
    }
    if let Some(seq) = msg_seq {
        buf.extend(encode_field(8, WT_VARINT, &encode_varint(seq)));
    }
    if !trace_id.is_empty() {
        let log_bytes = encode_log_ext(trace_id);
        buf.extend(encode_field(9, WT_LEN, &encode_message(&log_bytes)));
    }
    buf
}

/// Encode a C2C send-message request and return the full ConnMsg bytes.
#[allow(clippy::too_many_arguments)]
pub fn encode_send_c2c_message(
    to_account: &str,
    msg_body: &[Value],
    from_account: &str,
    msg_id: &str,
    msg_random: u64,
    msg_seq: Option<u64>,
    group_code: &str,
    trace_id: &str,
) -> Vec<u8> {
    let biz_bytes = encode_send_c2c_req(
        to_account,
        from_account,
        msg_body,
        msg_id,
        msg_random,
        msg_seq,
        group_code,
        trace_id,
    );
    dbg_bytes("encode_send_c2c biz payload", &biz_bytes);
    let req_id = if !msg_id.is_empty() {
        msg_id.to_string()
    } else {
        format!("c2c_{}", next_seq_no())
    };
    encode_conn_msg_full(
        CMD_TYPE_REQUEST,
        "send_c2c_message",
        next_seq_no(),
        &req_id,
        BIZ_PKG,
        &biz_bytes,
        false,
    )
}

/// Encode a group send-message request and return the full ConnMsg bytes.
#[allow(clippy::too_many_arguments)]
pub fn encode_send_group_message(
    group_code: &str,
    msg_body: &[Value],
    from_account: &str,
    msg_id: &str,
    to_account: &str,
    random: &str,
    msg_seq: Option<u64>,
    ref_msg_id: &str,
    trace_id: &str,
) -> Vec<u8> {
    let biz_bytes = encode_send_group_req(
        group_code,
        from_account,
        msg_body,
        msg_id,
        to_account,
        random,
        msg_seq,
        ref_msg_id,
        trace_id,
    );
    dbg_bytes("encode_send_group biz payload", &biz_bytes);
    let req_id = if !msg_id.is_empty() {
        msg_id.to_string()
    } else {
        format!("grp_{}", next_seq_no())
    };
    encode_conn_msg_full(
        CMD_TYPE_REQUEST,
        "send_group_message",
        next_seq_no(),
        &req_id,
        BIZ_PKG,
        &biz_bytes,
        false,
    )
}

// ============================================================
// AuthBind / Ping helpers
// ============================================================

/// Build an auth-bind request ConnMsg bytes.
#[allow(clippy::too_many_arguments)]
pub fn encode_auth_bind(
    biz_id: &str,
    uid: &str,
    source: &str,
    token: &str,
    msg_id: &str,
    app_version: &str,
    operation_system: &str,
    bot_version: &str,
    route_env: &str,
) -> Vec<u8> {
    // AuthInfo
    let mut auth_buf = Vec::new();
    auth_buf.extend(encode_field(1, WT_LEN, &encode_string(uid)));
    auth_buf.extend(encode_field(2, WT_LEN, &encode_string(source)));
    auth_buf.extend(encode_field(3, WT_LEN, &encode_string(token)));
    // DeviceInfo
    let mut dev_buf = Vec::new();
    if !app_version.is_empty() {
        dev_buf.extend(encode_field(1, WT_LEN, &encode_string(app_version)));
    }
    if !operation_system.is_empty() {
        dev_buf.extend(encode_field(2, WT_LEN, &encode_string(operation_system)));
    }
    dev_buf.extend(encode_field(
        10,
        WT_LEN,
        &encode_string(&HERMES_INSTANCE_ID.to_string()),
    ));
    if !bot_version.is_empty() {
        dev_buf.extend(encode_field(24, WT_LEN, &encode_string(bot_version)));
    }

    let mut req_buf = Vec::new();
    req_buf.extend(encode_field(1, WT_LEN, &encode_string(biz_id)));
    req_buf.extend(encode_field(2, WT_LEN, &encode_message(&auth_buf)));
    req_buf.extend(encode_field(3, WT_LEN, &encode_message(&dev_buf)));
    if !route_env.is_empty() {
        req_buf.extend(encode_field(5, WT_LEN, &encode_string(route_env)));
    }

    encode_conn_msg_full(
        CMD_TYPE_REQUEST,
        CMD_AUTH_BIND,
        next_seq_no(),
        msg_id,
        MODULE_CONN_ACCESS,
        &req_buf,
        false,
    )
}

/// Build a ping request ConnMsg bytes (PingReq is an empty message).
pub fn encode_ping(msg_id: &str) -> Vec<u8> {
    encode_conn_msg_full(
        CMD_TYPE_REQUEST,
        CMD_PING,
        next_seq_no(),
        msg_id,
        MODULE_CONN_ACCESS,
        b"",
        false,
    )
}

/// Build a push-ack reply from the original push head.
pub fn encode_push_ack(original_head: &Head) -> Vec<u8> {
    encode_conn_msg_full(
        CMD_TYPE_PUSH_ACK,
        &original_head.cmd,
        next_seq_no(),
        &original_head.msg_id,
        &original_head.module,
        b"",
        false,
    )
}

// ============================================================
// Heartbeat encoding
// ============================================================

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Encode a SendPrivateHeartbeatReq and return full ConnMsg bytes.
pub fn encode_send_private_heartbeat(
    from_account: &str,
    to_account: &str,
    heartbeat: u64,
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend(encode_field(1, WT_LEN, &encode_string(from_account)));
    buf.extend(encode_field(2, WT_LEN, &encode_string(to_account)));
    buf.extend(encode_field(3, WT_VARINT, &encode_varint(heartbeat)));
    let req_id = format!("hb_priv_{}", next_seq_no());
    encode_biz_msg(BIZ_PKG, "send_private_heartbeat", &req_id, &buf)
}

/// Encode a SendGroupHeartbeatReq and return full ConnMsg bytes.
/// `send_time == 0` uses the current epoch-ms timestamp.
pub fn encode_send_group_heartbeat(
    from_account: &str,
    group_code: &str,
    heartbeat: u64,
    send_time: u64,
) -> Vec<u8> {
    let ts = if send_time != 0 {
        send_time
    } else {
        now_millis()
    };
    let mut buf = Vec::new();
    buf.extend(encode_field(1, WT_LEN, &encode_string(from_account)));
    buf.extend(encode_field(2, WT_LEN, &encode_string(""))); // to_account empty for group
    buf.extend(encode_field(3, WT_LEN, &encode_string(group_code)));
    buf.extend(encode_field(4, WT_VARINT, &encode_varint(ts)));
    buf.extend(encode_field(5, WT_VARINT, &encode_varint(heartbeat)));
    let req_id = format!("hb_grp_{}", next_seq_no());
    encode_biz_msg(BIZ_PKG, "send_group_heartbeat", &req_id, &buf)
}

// ============================================================
// Group info query
// ============================================================

/// Encode a QueryGroupInfoReq and return full ConnMsg bytes.
pub fn encode_query_group_info(group_code: &str) -> Vec<u8> {
    let buf = encode_field(1, WT_LEN, &encode_string(group_code));
    let req_id = format!("qgi_{}", next_seq_no());
    encode_biz_msg(BIZ_PKG, "query_group_info", &req_id, &buf)
}

/// Decode a QueryGroupInfoRsp biz payload. Returns `None` on parse failure.
pub fn decode_query_group_info_rsp(data: &[u8]) -> Option<Value> {
    let fields = parse_fields(data).ok()?;
    let fdict = fields_to_dict(fields);
    let code = get_varint(&fdict, 1, 0);
    let msg = get_string(&fdict, 2, "");

    let mut result = Map::new();
    result.insert("code".to_string(), Value::from(code));
    if !msg.is_empty() {
        result.insert("message".to_string(), Value::String(msg));
    }

    // field 3 = nested GroupInfo message (take the first entry's bytes)
    let gi_bytes = get_bytes(&fdict, 3);
    if !gi_bytes.is_empty() {
        let gi = parse_dict_lenient(&gi_bytes);
        result.insert(
            "group_name".to_string(),
            Value::String(get_string(&gi, 1, "")),
        );
        result.insert(
            "owner_id".to_string(),
            Value::String(get_string(&gi, 2, "")),
        );
        result.insert(
            "owner_nickname".to_string(),
            Value::String(get_string(&gi, 3, "")),
        );
        result.insert(
            "member_count".to_string(),
            Value::from(get_varint(&gi, 4, 0)),
        );
    } else {
        result.insert("group_name".to_string(), Value::String(String::new()));
        result.insert("owner_id".to_string(), Value::String(String::new()));
        result.insert("owner_nickname".to_string(), Value::String(String::new()));
        result.insert("member_count".to_string(), Value::from(0u64));
    }
    Some(Value::Object(result))
}

// ============================================================
// Group member-list query
// ============================================================

/// Encode a GetGroupMemberListReq and return full ConnMsg bytes.
pub fn encode_get_group_member_list(group_code: &str, offset: u64, limit: u64) -> Vec<u8> {
    let mut buf = encode_field(1, WT_LEN, &encode_string(group_code));
    if offset != 0 {
        buf.extend(encode_field(2, WT_VARINT, &encode_varint(offset)));
    }
    buf.extend(encode_field(3, WT_VARINT, &encode_varint(limit)));
    let req_id = format!("gml_{}", next_seq_no());
    encode_biz_msg(BIZ_PKG, "get_group_member_list", &req_id, &buf)
}

/// Decode a GetGroupMemberListRsp biz payload. Returns `None` on parse failure.
pub fn decode_get_group_member_list_rsp(data: &[u8]) -> Option<Value> {
    let fields = parse_fields(data).ok()?;
    let fdict = fields_to_dict(fields);
    let code = get_varint(&fdict, 1, 0);

    let mut members: Vec<Value> = Vec::new();
    for member_bytes in get_repeated_bytes(&fdict, 3) {
        let mdict = parse_dict_lenient(&member_bytes);
        let mut member = Map::new();
        member.insert(
            "user_id".to_string(),
            Value::String(get_string(&mdict, 1, "")),
        );
        member.insert(
            "nickname".to_string(),
            Value::String(get_string(&mdict, 2, "")),
        );
        member.insert("role".to_string(), Value::from(get_varint(&mdict, 3, 0)));
        member.insert(
            "join_time".to_string(),
            Value::from(get_varint(&mdict, 4, 0)),
        );
        member.insert(
            "name_card".to_string(),
            Value::String(get_string(&mdict, 5, "")),
        );
        // Keep truthy fields, but always keep "role".
        let filtered: Map<String, Value> = member
            .into_iter()
            .filter(|(k, v)| is_truthy(v) || k == "role")
            .collect();
        members.push(Value::Object(filtered));
    }

    let mut result = Map::new();
    result.insert("code".to_string(), Value::from(code));
    result.insert(
        "message".to_string(),
        Value::String(get_string(&fdict, 2, "")),
    );
    result.insert("members".to_string(), Value::Array(members));
    result.insert(
        "next_offset".to_string(),
        Value::from(get_varint(&fdict, 4, 0)),
    );
    result.insert(
        "is_complete".to_string(),
        Value::Bool(get_varint(&fdict, 5, 0) != 0),
    );
    Some(Value::Object(result))
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02x}", x)).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Golden 1: varint encoding (from real Python _encode_varint).
    #[test]
    fn golden_varint() {
        let cases: &[(u64, &str)] = &[
            (0, "00"),
            (1, "01"),
            (127, "7f"),
            (128, "8001"),
            (300, "ac02"),
            (16384, "808001"),
            (4294967295, "ffffffff0f"),
            (1700000000000, "80d095ffbc31"),
        ];
        for (v, expect) in cases {
            assert_eq!(hex(&encode_varint(*v)), *expect, "varint({})", v);
        }
    }

    // Golden 2: encode_conn_msg (well-formed + empty payload).
    #[test]
    fn golden_encode_conn_msg() {
        assert_eq!(
            hex(&encode_conn_msg(2, 5, &[1, 2, 3])),
            "0a04080218051203010203"
        );
        assert_eq!(hex(&encode_conn_msg(0, 0, b"")), "0a00");
    }

    // Golden 3: encode_conn_msg_full with cmd/msg_id/module/need_ack.
    #[test]
    fn golden_encode_conn_msg_full() {
        let out = encode_conn_msg_full(
            0,
            "send_c2c_message",
            7,
            "req-1",
            "yuanbao_openclaw_proxy",
            b"hello",
            true,
        );
        assert_eq!(
            hex(&out),
            "0a35121073656e645f6332635f6d657373616765180722057265712d312a167975616e62616f5f6f70656e636c61775f70726f78793001120568656c6c6f"
        );
    }

    // Golden 4: encode_msg_content (strings + varints + ext_map).
    #[test]
    fn golden_encode_msg_content() {
        let mc = json!({"text":"hi","uuid":"u1","image_format":3,"index":2,"ext_map":{"k":"v"}});
        assert_eq!(
            hex(&encode_msg_content(&mc)),
            "0a0268691202753118034802ba3e060a016b120176"
        );
        // round-trip decode
        let decoded = decode_msg_content(&encode_msg_content(&mc));
        let expected =
            json!({"ext_map":{"k":"v"},"image_format":3,"index":2,"text":"hi","uuid":"u1"});
        assert_eq!(decoded, expected);
    }

    // Golden 5: SendC2CMessageReq biz payload (deterministic, no seq).
    #[test]
    fn golden_encode_send_c2c_req() {
        let body = vec![json!({"msg_type":"TIMTextElem","msg_content":{"text":"hello"}})];
        let out = encode_send_c2c_req("alice", "bot", &body, "m1", 42, Some(100), "", "t1");
        assert_eq!(
            hex(&out),
            "0a026d311205616c6963651a03626f74202a2a160a0b54494d54657874456c656d12070a0568656c6c6f386442040a027431"
        );
    }

    // Golden 6: SendGroupMessageReq biz payload (msg_seq None, ref_msg_id set).
    #[test]
    fn golden_encode_send_group_req() {
        let body = vec![json!({"msg_type":"TIMTextElem","msg_content":{"text":"hey"}})];
        let out = encode_send_group_req("grp1", "bot", &body, "m2", "", "rnd", None, "ref1", "");
        assert_eq!(
            hex(&out),
            "0a026d321204677270311a03626f742a03726e6432140a0b54494d54657874456c656d12050a036865793a0472656631"
        );
    }

    // Golden 7: decode_conn_msg / decode_biz_msg of the full frame + empty input.
    #[test]
    fn golden_decode_conn_and_biz() {
        let raw = unhex("0a35121073656e645f6332635f6d657373616765180722057265712d312a167975616e62616f5f6f70656e636c61775f70726f78793001120568656c6c6f");
        let cm = decode_conn_msg(&raw);
        assert_eq!(cm.msg_type, 0);
        assert_eq!(cm.seq_no, 7);
        assert_eq!(hex(&cm.data), "68656c6c6f");
        assert_eq!(
            cm.head,
            Head {
                cmd_type: 0,
                cmd: "send_c2c_message".to_string(),
                seq_no: 7,
                msg_id: "req-1".to_string(),
                module: "yuanbao_openclaw_proxy".to_string(),
                need_ack: true,
                status: 0,
            }
        );

        let bm = decode_biz_msg(&raw);
        assert_eq!(bm.service, "yuanbao_openclaw_proxy");
        assert_eq!(bm.method, "send_c2c_message");
        assert_eq!(bm.req_id, "req-1");
        assert_eq!(hex(&bm.body), "68656c6c6f");
        assert!(!bm.is_response);

        // empty / partial input
        let empty = decode_conn_msg(b"");
        assert_eq!(empty.msg_type, 0);
        assert_eq!(empty.seq_no, 0);
        assert!(empty.data.is_empty());
        assert_eq!(empty.head, Head::default());
    }

    // Golden 8: decode_inbound_push over a multi-field frame (incl. nested msg_body,
    // CJK text, log_ext) + empty input keeps only msg_body/msg_seq.
    #[test]
    fn golden_decode_inbound_push() {
        let raw = unhex("0a0c4332432e63616c6c6261636b1205616c6963651a03626f742205416c69636540375080e2cfaa0662076d736769642d316a170a0b54494d54657874456c656d12080a06e4bda0e5a5bd900101a2010b0a0974726163652d313233");
        let decoded = decode_inbound_push(&raw).expect("should decode");
        let expected = json!({
            "callback_command": "C2C.callback",
            "claw_msg_type": 1,
            "from_account": "alice",
            "msg_body": [{"msg_content": {"text": "你好"}, "msg_type": "TIMTextElem"}],
            "msg_id": "msgid-1",
            "msg_seq": 55,
            "msg_time": 1700000000,
            "sender_nickname": "Alice",
            "to_account": "bot",
            "trace_id": "trace-123"
        });
        assert_eq!(decoded, expected);

        let empty = decode_inbound_push(b"").expect("empty still decodes");
        assert_eq!(empty, json!({"msg_body": [], "msg_seq": 0}));
    }

    // Golden 9: ForwardMsgData encode -> hex, decode -> structure (round trip).
    #[test]
    fn golden_forward_msg_data() {
        let fdata = json!({
            "sub_type": 1, "begin_time": 100, "end_time": 200, "nick_name": "Bob",
            "msg": [{
                "sender": "Carol", "time": 123, "plainText": "hi there",
                "msgContent": [
                    {"type": 1, "text": "hi there"},
                    {"type": 2, "multimedia": [
                        {"type": "image", "url": "http://x/y.png", "file_name": "y.png", "file_size": 999, "media_id": "rid1"}
                    ]}
                ]
            }]
        });
        let enc = encode_forward_msg_data(&fdata);
        assert_eq!(
            hex(&enc),
            "0801106418c8012203426f622a4e0a054361726f6c107b1a086869207468657265220c080112086869207468657265222b08021a270a05696d616765120e687474703a2f2f782f792e706e672205792e706e677a047269643128e707"
        );
        let decoded = decode_forward_msg_data(&enc).expect("should decode");
        let expected = json!({
            "begin_time": 100,
            "end_time": 200,
            "msg": [{
                "msgContent": [
                    {"text": "hi there", "type": 1},
                    {"multimedia": [{"file_name": "y.png", "file_size": 999, "media_id": "rid1", "type": "image", "url": "http://x/y.png"}], "type": 2}
                ],
                "plainText": "hi there",
                "sender": "Carol",
                "time": 123
            }],
            "nick_name": "Bob",
            "sub_type": 1
        });
        assert_eq!(decoded, expected);
    }

    // Golden 10: decode_query_group_info_rsp (nested GroupInfo) + empty payload.
    #[test]
    fn golden_query_group_info_rsp() {
        let raw =
            unhex("080012026f6b1a1f0a084d792047726f757012066f776e6572311a094f776e65724e69636b202a");
        let decoded = decode_query_group_info_rsp(&raw).expect("decode");
        let expected = json!({
            "code": 0,
            "group_name": "My Group",
            "member_count": 42,
            "message": "ok",
            "owner_id": "owner1",
            "owner_nickname": "OwnerNick"
        });
        assert_eq!(decoded, expected);

        let empty = decode_query_group_info_rsp(b"").expect("empty decode");
        assert_eq!(
            empty,
            json!({"code": 0, "group_name": "", "member_count": 0, "owner_id": "", "owner_nickname": ""})
        );
    }

    // Golden 11: decode_get_group_member_list_rsp (repeated members, role-kept filter).
    #[test]
    fn golden_member_list_rsp() {
        let raw = unhex("080012026f6b1a170a02753112054e69636b31180220c00c2a0543617264311a060a027532180020c8012801");
        let decoded = decode_get_group_member_list_rsp(&raw).expect("decode");
        let expected = json!({
            "code": 0,
            "is_complete": true,
            "members": [
                {"join_time": 1600, "name_card": "Card1", "nickname": "Nick1", "role": 2, "user_id": "u1"},
                {"role": 0, "user_id": "u2"}
            ],
            "message": "ok",
            "next_offset": 200
        });
        assert_eq!(decoded, expected);
    }

    // Golden 12: structural check for a next_seq_no-dependent encoder. seq_no is
    // nondeterministic (shared counter), so decode the frame and assert the
    // stable parts: ping is a Request on conn_access with empty body.
    #[test]
    fn structural_encode_ping() {
        let bm = decode_biz_msg(&encode_ping("ping-1"));
        assert_eq!(bm.method, "ping");
        assert_eq!(bm.service, "conn_access");
        assert_eq!(bm.req_id, "ping-1");
        assert_eq!(bm.head.cmd_type, CMD_TYPE_REQUEST as u64);
        assert!(bm.body.is_empty());
        assert!(!bm.is_response);
    }

    // Structural check: full C2C send-message frame wraps the biz payload and
    // round-trips through decode_biz_msg + decode_send fields.
    #[test]
    fn structural_encode_send_c2c_message() {
        let body = vec![json!({"msg_type":"TIMTextElem","msg_content":{"text":"hello"}})];
        let frame = encode_send_c2c_message("alice", &body, "bot", "m1", 42, Some(100), "", "t1");
        let bm = decode_biz_msg(&frame);
        assert_eq!(bm.method, "send_c2c_message");
        assert_eq!(bm.service, "yuanbao_openclaw_proxy");
        assert_eq!(bm.req_id, "m1");
        // biz body is exactly the deterministic SendC2CMessageReq payload
        assert_eq!(
            hex(&bm.body),
            "0a026d311205616c6963651a03626f74202a2a160a0b54494d54657874456c656d12070a0568656c6c6f386442040a027431"
        );
    }

    // push_ack echoes cmd/msg_id/module from the original head as a PushAck.
    #[test]
    fn structural_push_ack() {
        let head = Head {
            cmd_type: CMD_TYPE_PUSH as u64,
            cmd: "some-cmd".to_string(),
            seq_no: 9,
            msg_id: "mid-9".to_string(),
            module: "conn_access".to_string(),
            need_ack: true,
            status: 0,
        };
        let bm = decode_biz_msg(&encode_push_ack(&head));
        assert_eq!(bm.head.cmd_type, CMD_TYPE_PUSH_ACK as u64);
        assert_eq!(bm.method, "some-cmd");
        assert_eq!(bm.req_id, "mid-9");
        assert_eq!(bm.service, "conn_access");
        assert!(bm.body.is_empty());
    }
}
