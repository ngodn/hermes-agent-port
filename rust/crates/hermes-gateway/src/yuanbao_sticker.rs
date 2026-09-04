//! Port of gateway/platforms/yuanbao_sticker.py.
//!
// Public API is ahead of its callers (the Yuanbao sticker send path wires it).
#![allow(dead_code)]
//!
//! Yuanbao sticker (TIMFaceElem) support, ported from the openclaw plugin's
//! builtin sticker catalogue plus the fuzzy search used by sticker-cache.ts.
//!
//! TIMFaceElem wire format:
//! ```json
//! {"msg_type": "TIMFaceElem", "msg_content": {"index": 0, "data": "<json>"}}
//! ```
//! The `data` field carries a JSON string with the sticker metadata so the
//! receiver can look up the right asset in the emoji pack.
//!
//! Unicode note: the Python `_normalize_text` runs `unicodedata.normalize("NFKC", ...)`
//! before `.strip().lower()`. That is the ONLY unicodedata call in the module
//! (no `category`, no `name`). There is no NFKC crate dependency available here,
//! and the port instructions forbid adding one or hand-rolling the tables, so
//! `normalize_text` below does strip + lowercase only. For all-ASCII and CJK
//! input (which is everything in the catalogue and every realistic query) NFKC
//! is a no-op, so behaviour matches Python. Queries carrying compatibility
//! forms (full-width ASCII, ligatures, circled digits, etc.) would diverge:
//! Python folds them to their canonical form first, this port does not. That is
//! the one faithfulness gap and it needs an NFKC dep to close.
//!
//! All string length/indexing here is over Unicode code points (Rust `char`),
//! matching Python `str` semantics, so astral-plane emoji count as one unit.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

/// One built-in sticker (a row of the ported builtin-stickers catalogue).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sticker {
    pub sticker_id: &'static str,
    pub package_id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub width: i64,
    pub height: i64,
    pub formats: &'static str,
}

/// Sticker catalogue, ported from builtin-stickers.json.
///
/// Order is the Python dict insertion order, which is load-bearing: empty-query
/// search and random selection both walk it in order, and stable sort ties fall
/// back to it.
pub static STICKER_MAP: &[Sticker] = &[
    Sticker {
        sticker_id: "278",
        package_id: "1003",
        name: "六六六",
        description: "666 厉害 牛 棒 绝了 好强 awesome",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "262",
        package_id: "1003",
        name: "我想开了",
        description: "想开 佛系 释怀 顿悟 看淡了 无所谓",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "130",
        package_id: "1003",
        name: "害羞",
        description: "腼腆 不好意思 脸红 娇羞 羞涩 捂脸",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "252",
        package_id: "1003",
        name: "比心",
        description: "笔芯 爱你 爱心手势 love heart 喜欢你",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "125",
        package_id: "1003",
        name: "委屈",
        description: "难过 想哭 可怜巴巴 瘪嘴 受伤 被欺负",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "146",
        package_id: "1003",
        name: "亲亲",
        description: "么么 mua 亲一下 kiss 飞吻 啵",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "131",
        package_id: "1003",
        name: "酷",
        description: "帅 墨镜 cool 高冷 有型 swagger",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "145",
        package_id: "1003",
        name: "睡",
        description: "睡觉 困 zzZ 打盹 躺平 休眠 sleepy",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "152",
        package_id: "1003",
        name: "发呆",
        description: "懵 愣住 放空 呆滞 出神 脑子空白",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "157",
        package_id: "1003",
        name: "可怜",
        description: "卖萌 求饶 委屈巴巴 弱小 拜托 眼巴巴",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "200",
        package_id: "1003",
        name: "摊手",
        description: "无奈 没办法 耸肩 随便 那咋整 whatever",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "213",
        package_id: "1003",
        name: "头大",
        description: "头疼 烦恼 郁闷 难搞 崩溃 一团乱",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "256",
        package_id: "1003",
        name: "吓",
        description: "害怕 惊恐 震惊 吓一跳 恐怖 怂",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "203",
        package_id: "1003",
        name: "吐血",
        description: "无语 崩溃 被雷 内伤 一口老血 屮",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "185",
        package_id: "1003",
        name: "哼",
        description: "傲娇 生气 不满 撇嘴 不理 赌气",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "220",
        package_id: "1003",
        name: "嘿嘿",
        description: "坏笑 猥琐笑 偷笑 憨笑 得意 你懂的",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "218",
        package_id: "1003",
        name: "头秃",
        description: "程序员 加班 焦虑 没头发 秃了 肝爆",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "221",
        package_id: "1003",
        name: "暗中观察",
        description: "窥屏 潜水 偷偷看 角落 围观 屏住呼吸",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "224",
        package_id: "1003",
        name: "我酸了",
        description: "嫉妒 柠檬精 羡慕 吃柠檬 眼红 恰柠檬",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "246",
        package_id: "1003",
        name: "打call",
        description: "应援 加油 支持 喝彩 助威 call",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "251",
        package_id: "1003",
        name: "庆祝",
        description: "祝贺 开心 耶 party 胜利 干杯",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "151",
        package_id: "1003",
        name: "奋斗",
        description: "努力 加油 拼搏 冲 干劲 卷起来",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "143",
        package_id: "1003",
        name: "惊讶",
        description: "震惊 哇 不敢相信 OMG 居然 这么离谱",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "144",
        package_id: "1003",
        name: "疑问",
        description: "问号 不懂 啥 为什么 啥情况 懵逼问",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "248",
        package_id: "1003",
        name: "仔细分析",
        description: "思考 推敲 认真 研究 琢磨 让我想想",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "184",
        package_id: "1003",
        name: "撅嘴",
        description: "嘟嘴 卖萌 不高兴 撒娇 嘴翘",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "199",
        package_id: "1003",
        name: "泪奔",
        description: "大哭 伤心 破防 感动哭 泪流满面 呜呜",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "276",
        package_id: "1003",
        name: "尊嘟假嘟",
        description: "真的假的 真假 可爱问 你骗我 是不是",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "113",
        package_id: "1003",
        name: "略略略",
        description: "调皮 吐舌 不服 略 气死你 鬼脸",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "180",
        package_id: "1003",
        name: "困",
        description: "想睡 倦 打哈欠 睁不开眼 好困啊 sleepy",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "181",
        package_id: "1003",
        name: "折磨",
        description: "难受 痛苦 煎熬 蚌埠住了 受不了 要命",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "182",
        package_id: "1003",
        name: "抠鼻",
        description: "不屑 无聊 淡定 无所谓 鄙视 挖鼻",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "183",
        package_id: "1003",
        name: "鼓掌",
        description: "拍手 叫好 赞同 666 喝彩 掌声",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "204",
        package_id: "1003",
        name: "斜眼笑",
        description: "滑稽 坏笑 doge 意味深长 阴阳怪气 嘿嘿嘿",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "216",
        package_id: "1003",
        name: "辣眼睛",
        description: "看不下去 cringe 毁三观 太丑了 瞎了",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "217",
        package_id: "1003",
        name: "哦哟",
        description: "惊讶 起哄 哇哦 有戏 不简单 哟",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "222",
        package_id: "1003",
        name: "吃瓜",
        description: "围观 看戏 八卦 路人 看热闹 板凳",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "225",
        package_id: "1003",
        name: "狗头",
        description: "doge 保命 开玩笑 滑稽 反讽 懂的都懂",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "227",
        package_id: "1003",
        name: "敬礼",
        description: "salute 尊重 收到 遵命 致敬 报告",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "231",
        package_id: "1003",
        name: "哦",
        description: "知道了 明白 敷衍 嗯 这样啊 收到",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "236",
        package_id: "1003",
        name: "拿到红包",
        description: "红包 谢谢老板 发财 开心 抢到了 欧气",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "239",
        package_id: "1003",
        name: "牛吖",
        description: "牛 厉害 强 666 佩服 大佬",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "272",
        package_id: "1003",
        name: "贴贴",
        description: "抱抱 亲昵 蹭蹭 亲密 靠靠 撒娇贴",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "138",
        package_id: "1003",
        name: "爱心",
        description: "心 love 喜欢你 红心 示爱 么么哒",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "170",
        package_id: "1003",
        name: "晚安",
        description: "好梦 睡了 night 早点休息 安啦 moon",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "176",
        package_id: "1003",
        name: "太阳",
        description: "晴天 早上好 阳光 morning 好天气 日",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "266",
        package_id: "1003",
        name: "柠檬",
        description: "酸 嫉妒 柠檬精 羡慕 我酸 恰柠檬",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "267",
        package_id: "1003",
        name: "大冤种",
        description: "倒霉 吃亏 自嘲 好心没好报 背锅 工具人",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "132",
        package_id: "1003",
        name: "吐了",
        description: "恶心 yue 受不了 嫌弃 想吐 生理不适",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "134",
        package_id: "1003",
        name: "怒",
        description: "生气 愤怒 火大 暴躁 气炸 怼",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "165",
        package_id: "1003",
        name: "玫瑰",
        description: "花 示爱 表白 浪漫 送你花 情人节",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "119",
        package_id: "1003",
        name: "凋谢",
        description: "花谢 失恋 难过 枯萎 心碎 凉了",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "159",
        package_id: "1003",
        name: "点赞",
        description: "赞 认同 好棒 good like 大拇指 顶",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "164",
        package_id: "1003",
        name: "握手",
        description: "合作 你好 商务 hello deal 成交 友好",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "163",
        package_id: "1003",
        name: "抱拳",
        description: "谢谢 失敬 江湖 承让 拜托 有礼",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "169",
        package_id: "1003",
        name: "ok",
        description: "好的 收到 没问题 okay 行 可以 懂了",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "174",
        package_id: "1003",
        name: "拳头",
        description: "加油 干 冲 fight 力量 击拳 硬气",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "191",
        package_id: "1003",
        name: "鞭炮",
        description: "过年 喜庆 爆竹 春节 噼里啪啦 红",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "258",
        package_id: "1003",
        name: "烟花",
        description: "庆典 漂亮 新年 嘭 绽放 节日快乐",
        width: 128,
        height: 128,
        formats: "png",
    },
];

// ---------------------------------------------------------------------------
// Lookups
// ---------------------------------------------------------------------------

/// Find a sticker by name with fuzzy fallback (port of `get_sticker_by_name`).
///
/// Match priority: exact name, then name/query substring either way, then
/// description substring, then the top fuzzy search hit. Empty `name` returns
/// `None`; a whitespace-only name strips to `""`, which substring-matches the
/// first catalogue entry (Python `"" in key` is always true), so the first
/// sticker is returned.
pub fn get_sticker_by_name(name: &str) -> Option<&'static Sticker> {
    if name.is_empty() {
        return None;
    }
    let query = name.trim();

    if let Some(s) = STICKER_MAP.iter().find(|s| s.name == query) {
        return Some(s);
    }

    for s in STICKER_MAP.iter() {
        if s.name.contains(query) || query.contains(s.name) {
            return Some(s);
        }
    }

    for s in STICKER_MAP.iter() {
        if s.description.contains(query) {
            return Some(s);
        }
    }

    search_stickers(query, 1).into_iter().next()
}

/// Random sticker, optionally restricted to a category keyword (port of
/// `get_random_sticker`). If `category` is `Some(non-empty)` and any sticker's
/// description or name contains it, one of those is chosen; otherwise a sticker
/// from the whole table. Randomness comes from the kernel CSPRNG.
pub fn get_random_sticker(category: Option<&str>) -> &'static Sticker {
    if let Some(cat) = category {
        if !cat.is_empty() {
            let candidates: Vec<&'static Sticker> = STICKER_MAP
                .iter()
                .filter(|s| s.description.contains(cat) || s.name.contains(cat))
                .collect();
            if !candidates.is_empty() {
                let idx = (rand_u64() as usize) % candidates.len();
                return candidates[idx];
            }
        }
    }
    let idx = (rand_u64() as usize) % STICKER_MAP.len();
    &STICKER_MAP[idx]
}

/// Exact lookup by sticker id (port of `get_sticker_by_id`). Empty id is `None`;
/// the id is trimmed before comparison.
pub fn get_sticker_by_id(sticker_id: &str) -> Option<&'static Sticker> {
    if sticker_id.is_empty() {
        return None;
    }
    let sid = sticker_id.trim();
    STICKER_MAP.iter().find(|s| s.sticker_id == sid)
}

// ---------------------------------------------------------------------------
// Fuzzy search (aligned with sticker-cache.ts searchStickers)
// ---------------------------------------------------------------------------

/// True for any char stripped by the Python `_PUNCT_RE` character class, or any
/// Unicode whitespace (Python `\s` on a str pattern matches Unicode whitespace).
fn is_punct_or_space(c: char) -> bool {
    if c.is_whitespace() {
        return true;
    }
    matches!(
        c,
        '\u{3000}'   // ideographic space (also whitespace, listed for parity)
            | '-'
            | '_'
            | '\u{00B7}' // middle dot
            | '.'
            | ','
            | '\u{FF0C}' // fullwidth comma
            | '\u{3002}' // ideographic full stop
            | '!'
            | '\u{FF01}' // fullwidth exclamation
            | '?'
            | '\u{FF1F}' // fullwidth question
            | '"'
            | '\u{201C}' // left double quote
            | '\u{201D}' // right double quote
            | '\''
            | '\u{2018}' // left single quote
            | '\u{2019}' // right single quote
            | '\u{3001}' // ideographic comma
            | '/'
            | '\\'
    )
}

/// Port of `_normalize_text`. See the module note: NFKC is not applied because
/// no NFKC crate dependency is available. Strip then lowercase only.
fn normalize_text(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// Port of `_compact_text`: normalize, then drop every whitespace/punct char.
fn compact_text(raw: &str) -> String {
    normalize_text(raw)
        .chars()
        .filter(|c| !is_punct_or_space(*c))
        .collect()
}

/// Port of `_multiset_char_hit_ratio`: fraction of needle chars covered by the
/// haystack's character multiset (each haystack char consumed once).
fn multiset_char_hit_ratio(needle: &str, haystack: &str) -> f64 {
    let ncount = needle.chars().count();
    if ncount == 0 {
        return 0.0;
    }
    let mut bag: HashMap<char, i64> = HashMap::new();
    for ch in haystack.chars() {
        *bag.entry(ch).or_insert(0) += 1;
    }
    let mut hits = 0i64;
    for ch in needle.chars() {
        let n = bag.get(&ch).copied().unwrap_or(0);
        if n > 0 {
            hits += 1;
            bag.insert(ch, n - 1);
        }
    }
    hits as f64 / ncount as f64
}

/// Port of `_bigram_jaccard`: Jaccard over adjacent code-point bigrams.
fn bigram_jaccard(a: &str, b: &str) -> f64 {
    let av: Vec<char> = a.chars().collect();
    let bv: Vec<char> = b.chars().collect();
    if av.len() < 2 || bv.len() < 2 {
        return 0.0;
    }
    let sa: HashSet<(char, char)> = (0..av.len() - 1).map(|i| (av[i], av[i + 1])).collect();
    let sb: HashSet<(char, char)> = (0..bv.len() - 1).map(|i| (bv[i], bv[i + 1])).collect();
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.len() as f64 + sb.len() as f64 - inter;
    if union > 0.0 {
        inter / union
    } else {
        0.0
    }
}

/// Port of `_longest_subsequence_ratio`: greedy in-order coverage of the needle.
fn longest_subsequence_ratio(needle: &str, haystack: &str) -> f64 {
    let nv: Vec<char> = needle.chars().collect();
    if nv.is_empty() {
        return 0.0;
    }
    let mut j = 0usize;
    for ch in haystack.chars() {
        if j >= nv.len() {
            break;
        }
        if ch == nv[j] {
            j += 1;
        }
    }
    j as f64 / nv.len() as f64
}

/// Port of `_score_field`: best score of one field against the query.
fn score_field(haystack: &str, query: &str) -> f64 {
    let hay = normalize_text(haystack);
    let q = normalize_text(query);
    if hay.is_empty() || q.is_empty() {
        return 0.0;
    }
    let hay_c = compact_text(haystack);
    let q_c = compact_text(query);
    let qlen = q.chars().count() as i64;
    let mut best = 0.0f64;
    if hay == q {
        best = best.max(100.0);
    }
    if hay.contains(&q) {
        best = best.max(92.0 + qlen.min(6) as f64);
    }
    if qlen >= 2 && hay.starts_with(&q) {
        best = best.max(88.0);
    }
    if !q_c.is_empty() && hay_c.contains(&q_c) {
        best = best.max(86.0);
    }
    best = best.max(multiset_char_hit_ratio(&q_c, &hay_c) * 62.0);
    best = best.max(bigram_jaccard(&q_c, &hay_c) * 58.0);
    best = best.max(longest_subsequence_ratio(&q_c, &hay_c) * 52.0);
    if qlen == 1 && hay.contains(&q) {
        best = best.max(68.0);
    }
    best
}

/// Port of `search_stickers`: fuzzy-rank the catalogue, return the top matches.
///
/// `limit` follows Python `int(limit) if limit else 10` then clamp to 1..=500,
/// so `limit == 0` becomes 10 and a negative limit becomes 1. An empty or
/// whitespace-only query returns the first `limit` stickers in catalogue order.
pub fn search_stickers(query: &str, limit: i64) -> Vec<&'static Sticker> {
    let eff = if limit != 0 { limit } else { 10 };
    let safe_limit = 1.max(500.min(eff)) as usize;

    if query.is_empty() || normalize_text(query).is_empty() {
        return STICKER_MAP.iter().take(safe_limit).collect();
    }

    let q_norm = normalize_text(query);
    let mut scored: Vec<(f64, &'static Sticker)> = Vec::with_capacity(STICKER_MAP.len());
    for s in STICKER_MAP.iter() {
        let name_s = score_field(s.name, query);
        let desc_s = score_field(s.description, query) * 0.88;
        let sid = s.sticker_id.trim();
        let mut id_s = 0.0f64;
        if !sid.is_empty() && !q_norm.is_empty() {
            let sid_norm = normalize_text(sid);
            if sid_norm == q_norm {
                id_s = 100.0;
            } else if sid_norm.contains(&q_norm) {
                id_s = 84.0;
            }
        }
        scored.push((name_s.max(desc_s).max(id_s), s));
    }

    // Stable descending sort: `sort_by` is stable, so equal scores keep
    // catalogue order, matching Python's stable `list.sort(reverse=True)`.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let top = scored.first().map(|x| x.0).unwrap_or(0.0);
    if top <= 0.0 {
        return scored
            .into_iter()
            .take(safe_limit)
            .map(|(_, s)| s)
            .collect();
    }

    let floor = if top >= 22.0 {
        18.0
    } else if top >= 12.0 {
        10.0f64.max(top * 0.5)
    } else {
        6.0f64.max(top * 0.35)
    };

    let filtered: Vec<(f64, &'static Sticker)> =
        scored.iter().copied().filter(|p| p.0 >= floor).collect();
    let out = if !filtered.is_empty() {
        filtered
    } else {
        scored
    };
    out.into_iter().take(safe_limit).map(|(_, s)| s).collect()
}

// ---------------------------------------------------------------------------
// TIMFaceElem message body
// ---------------------------------------------------------------------------

/// The `msg_content` object inside a TIMFaceElem. `data` is omitted when absent,
/// matching Python leaving the key out when `data is None`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FaceMsgContent {
    pub index: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// One TIMFaceElem element. Field order (msg_type, msg_content) is preserved by
/// serde struct serialization; do not switch to a map (serde_json is sorted-key
/// backed here).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FaceMsg {
    pub msg_type: &'static str,
    pub msg_content: FaceMsgContent,
}

/// Port of `build_face_msg_body`. `face_type` is a retained no-op field (Python
/// never uses it). `data` is dropped from the wire when `None`.
pub fn build_face_msg_body(face_index: i64, _face_type: i64, data: Option<String>) -> Vec<FaceMsg> {
    vec![FaceMsg {
        msg_type: "TIMFaceElem",
        msg_content: FaceMsgContent {
            index: face_index,
            data,
        },
    }]
}

/// Serialized sticker metadata that rides in the TIMFaceElem `data` field.
/// Field order matches the Python `json.dumps` dict:
/// sticker_id, package_id, width, height, formats, name.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct StickerData {
    sticker_id: String,
    package_id: String,
    width: i64,
    height: i64,
    formats: String,
    name: String,
}

/// Port of `build_sticker_msg_body`. Serializes the sticker metadata to a
/// compact JSON string (no spaces, non-ASCII left raw, mirroring
/// `json.dumps(..., ensure_ascii=False, separators=(",", ":"))`) and wraps it in
/// a TIMFaceElem with index 0.
pub fn build_sticker_msg_body(sticker: &Sticker) -> Vec<FaceMsg> {
    let payload = StickerData {
        sticker_id: sticker.sticker_id.to_string(),
        package_id: sticker.package_id.to_string(),
        width: sticker.width,
        height: sticker.height,
        formats: sticker.formats.to_string(),
        name: sticker.name.to_string(),
    };
    // serde_json::to_string is compact and does not escape non-ASCII, which is
    // exactly json.dumps(ensure_ascii=False, separators=(",", ":")).
    let data = serde_json::to_string(&payload).expect("StickerData serializes");
    build_face_msg_body(0, 1, Some(data))
}

// ---------------------------------------------------------------------------
// Randomness: kernel CSPRNG, best effort
// ---------------------------------------------------------------------------

/// 8 bytes from the kernel CSPRNG (/dev/urandom). Falls back to a time-derived
/// value if the device cannot be read. Not security-sensitive (sticker pick),
/// but the project convention is to source randomness from the kernel.
fn rand_u64() -> u64 {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut buf = [0u8; 8];
        if f.read_exact(&mut buf).is_ok() {
            return u64::from_le_bytes(buf);
        }
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests: golden values captured from the Python module.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&'static Sticker]) -> Vec<&'static str> {
        v.iter().map(|s| s.sticker_id).collect()
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
    }

    #[test]
    fn catalogue_size() {
        assert_eq!(STICKER_MAP.len(), 59);
    }

    // Golden 1-8+: get_sticker_by_name
    #[test]
    fn get_by_name_golden() {
        assert_eq!(
            get_sticker_by_name("六六六").map(|s| s.sticker_id),
            Some("278")
        );
        assert_eq!(
            get_sticker_by_name("666").map(|s| s.sticker_id),
            Some("278")
        );
        assert_eq!(
            get_sticker_by_name("害羞").map(|s| s.sticker_id),
            Some("130")
        );
        assert_eq!(
            get_sticker_by_name("比心 love").map(|s| s.sticker_id),
            Some("252")
        );
        assert_eq!(
            get_sticker_by_name("awesome").map(|s| s.sticker_id),
            Some("278")
        );
        assert_eq!(
            get_sticker_by_name("doge").map(|s| s.sticker_id),
            Some("204")
        );
        assert_eq!(get_sticker_by_name("ok").map(|s| s.sticker_id), Some("169"));
        assert_eq!(
            get_sticker_by_name("cool").map(|s| s.sticker_id),
            Some("131")
        );
        assert_eq!(
            get_sticker_by_name("不存在xyz").map(|s| s.sticker_id),
            Some("145")
        );
        // empty -> None; whitespace-only strips to "" and matches first entry
        assert_eq!(get_sticker_by_name(""), None);
        assert_eq!(
            get_sticker_by_name("   ").map(|s| s.sticker_id),
            Some("278")
        );
    }

    // Golden: get_sticker_by_id
    #[test]
    fn get_by_id_golden() {
        assert_eq!(get_sticker_by_id("278").map(|s| s.sticker_id), Some("278"));
        assert_eq!(get_sticker_by_id("258").map(|s| s.sticker_id), Some("258"));
        assert_eq!(get_sticker_by_id("113").map(|s| s.sticker_id), Some("113"));
        assert_eq!(
            get_sticker_by_id(" 130 ").map(|s| s.sticker_id),
            Some("130")
        );
        assert_eq!(get_sticker_by_id("999"), None);
        assert_eq!(get_sticker_by_id(""), None);
        assert_eq!(get_sticker_by_id("   "), None); // trims to "", no match
    }

    // Golden: search_stickers ranked ids (multibyte CJK + ASCII + astral-safe)
    #[test]
    fn search_golden() {
        assert_eq!(ids(&search_stickers("狗头", 3)), ["225", "213", "218"]);
        assert_eq!(ids(&search_stickers("哭", 3)), ["125", "199"]);
        assert_eq!(
            ids(&search_stickers("生气", 5)),
            ["185", "134", "113", "204", "236"]
        );
        assert_eq!(ids(&search_stickers("", 3)), ["278", "262", "130"]);
        assert_eq!(ids(&search_stickers("666", 3)), ["278", "183", "239"]);
        assert_eq!(ids(&search_stickers("ok", 3)), ["169", "159", "278"]);
        assert_eq!(ids(&search_stickers("xyznotfound", 3)), ["170"]);
        assert_eq!(ids(&search_stickers("cool", 3)), ["131", "159", "246"]);
        // limit == 0 -> treated as 10; negative -> 1
        assert_eq!(
            ids(&search_stickers("狗头", 0)),
            ["225", "213", "218", "174"]
        );
        assert_eq!(ids(&search_stickers("狗头", -3)), ["225"]);
    }

    // Golden: _score_field values
    #[test]
    fn score_field_golden() {
        approx(score_field("六六六", "六六六"), 100.0);
        approx(score_field("狗头", "狗头"), 100.0);
        approx(score_field("666 厉害 牛 棒 绝了 好强 awesome", "666"), 95.0);
        approx(score_field("ok", "ok"), 100.0);
        approx(score_field("害羞", "羞"), 93.0); // substring bonus 92+1, beats single-char 68
        approx(score_field("玫瑰", "花"), 0.0);
        approx(score_field("abcdef", "ab"), 94.0); // 92 + min(6, 2)
        approx(score_field("abcdefghij", "abcdefgh"), 98.0); // 92 + min(6, 8)
    }

    // Golden: scoring helpers, incl. astral-plane emoji as a single code point
    #[test]
    fn helpers_golden() {
        assert_eq!(normalize_text("  Hello 六六六  "), "hello 六六六");
        assert_eq!(compact_text("a-b_c·d.,，。!！?？\"“”'‘’、/\\e"), "abcde");

        approx(multiset_char_hit_ratio("aab", "abxab"), 1.0);
        approx(multiset_char_hit_ratio("aab", "ab"), 2.0 / 3.0);
        approx(multiset_char_hit_ratio("", "x"), 0.0);
        approx(multiset_char_hit_ratio("abc", "axc"), 2.0 / 3.0);

        approx(bigram_jaccard("abcd", "bcde"), 0.5);
        approx(bigram_jaccard("ab", "ab"), 1.0);
        approx(bigram_jaccard("a", "abc"), 0.0);
        approx(bigram_jaccard("abc", "abd"), 1.0 / 3.0);

        approx(longest_subsequence_ratio("ace", "abcde"), 1.0);
        approx(longest_subsequence_ratio("abc", "aXbXc"), 1.0);
        approx(longest_subsequence_ratio("", "x"), 0.0);
        approx(longest_subsequence_ratio("abc", "acb"), 2.0 / 3.0);

        // astral emoji counts as one code point in every op
        approx(score_field("a\u{1F600}b", "\u{1F600}"), 93.0);
        approx(multiset_char_hit_ratio("\u{1F600}", "x\u{1F600}y"), 1.0);
    }

    // Golden: TIMFaceElem message bodies serialized as JSON
    #[test]
    fn face_body_golden() {
        assert_eq!(
            serde_json::to_string(&build_face_msg_body(0, 1, None)).unwrap(),
            r#"[{"msg_type":"TIMFaceElem","msg_content":{"index":0}}]"#
        );
        assert_eq!(
            serde_json::to_string(&build_face_msg_body(0, 1, Some("hi".to_string()))).unwrap(),
            r#"[{"msg_type":"TIMFaceElem","msg_content":{"index":0,"data":"hi"}}]"#
        );
        assert_eq!(
            serde_json::to_string(&build_face_msg_body(5, 2, None)).unwrap(),
            r#"[{"msg_type":"TIMFaceElem","msg_content":{"index":5}}]"#
        );
    }

    // Golden: build_sticker_msg_body data payload (compact, non-ASCII raw)
    #[test]
    fn sticker_body_golden() {
        let liu = get_sticker_by_id("278").unwrap();
        assert_eq!(
            serde_json::to_string(&build_sticker_msg_body(liu)).unwrap(),
            r#"[{"msg_type":"TIMFaceElem","msg_content":{"index":0,"data":"{\"sticker_id\":\"278\",\"package_id\":\"1003\",\"width\":128,\"height\":128,\"formats\":\"png\",\"name\":\"六六六\"}"}}]"#
        );
        let yanhua = get_sticker_by_id("258").unwrap();
        assert_eq!(
            serde_json::to_string(&build_sticker_msg_body(yanhua)).unwrap(),
            r#"[{"msg_type":"TIMFaceElem","msg_content":{"index":0,"data":"{\"sticker_id\":\"258\",\"package_id\":\"1003\",\"width\":128,\"height\":128,\"formats\":\"png\",\"name\":\"烟花\"}"}}]"#
        );
    }

    // get_random_sticker: category filter restricts to matching candidates
    #[test]
    fn random_respects_category() {
        // "doge" appears in 斜眼笑(204) and 狗头(225) descriptions only
        for _ in 0..50 {
            let s = get_random_sticker(Some("doge"));
            assert!(s.description.contains("doge") || s.name.contains("doge"));
            assert!(s.sticker_id == "204" || s.sticker_id == "225");
        }
        // empty category -> whole table; no-match category -> whole table
        for _ in 0..20 {
            let s = get_random_sticker(Some(""));
            assert!(STICKER_MAP.iter().any(|x| x.sticker_id == s.sticker_id));
            let s2 = get_random_sticker(Some("zzz_no_such_keyword"));
            assert!(STICKER_MAP.iter().any(|x| x.sticker_id == s2.sticker_id));
            let s3 = get_random_sticker(None);
            assert!(STICKER_MAP.iter().any(|x| x.sticker_id == s3.sticker_id));
        }
    }
}
