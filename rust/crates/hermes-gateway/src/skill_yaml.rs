//! YAML construction for skill metadata. The event parser preserves quoting;
//! scalar resolution follows PyYAML's safe loader rather than YAML 1.2 defaults.

use serde_yaml_ng::{Mapping, Value};
use std::collections::HashMap;
use std::sync::LazyLock;
use yaml_rust2::parser::{Event, EventReceiver, Parser, Tag};
use yaml_rust2::scanner::TScalarStyle;

struct Events(Vec<Event>);
impl EventReceiver for Events {
    fn on_event(&mut self, event: Event) {
        self.0.push(event);
    }
}

#[derive(Clone)]
struct Node {
    value: Value,
    merge: bool,
}

pub fn parse(text: &str) -> Result<Value, &'static str> {
    // LibYAML ignores column-zero BOMs before the first content token, even
    // after blank/comment lines. BOMs in later keys and quoted text are data.
    let mut prefix = true;
    let text = text
        .split_inclusive('\n')
        .map(|line| {
            let line = if prefix {
                line.trim_start_matches('\u{feff}')
            } else {
                line
            };
            let trimmed = line.trim_start_matches([' ', '\t', '\r', '\n']);
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                prefix = false;
            }
            line
        })
        .collect::<String>();
    let mut events = Events(Vec::new());
    Parser::new_from_str(&text)
        .load(&mut events, true)
        .map_err(|_| "invalid YAML")?;
    let documents = events
        .0
        .iter()
        .filter(|event| matches!(event, Event::DocumentStart))
        .count();
    if documents > 1 {
        return Err("multiple YAML documents");
    }
    let mut events = events
        .0
        .into_iter()
        .filter(|event| {
            !matches!(
                event,
                Event::StreamStart
                    | Event::StreamEnd
                    | Event::DocumentStart
                    | Event::DocumentEnd
                    | Event::Nothing
            )
        })
        .peekable();
    if events.peek().is_none() {
        return Ok(Value::Null);
    }
    node(&mut events, &mut HashMap::new()).map(|node| node.value)
}

fn tag_name(tag: Option<&Tag>) -> Result<Option<&str>, &'static str> {
    match tag {
        None => Ok(None),
        Some(tag) if tag.handle == "tag:yaml.org,2002:" || tag.handle == "!!" => {
            Ok(Some(&tag.suffix))
        }
        Some(tag) if tag.handle == "!" && tag.suffix.is_empty() => Ok(None),
        _ => Err("unknown YAML tag"),
    }
}

fn node(
    events: &mut std::iter::Peekable<impl Iterator<Item = Event>>,
    anchors: &mut HashMap<usize, Node>,
) -> Result<Node, &'static str> {
    let (anchor, result) = match events.next().ok_or("missing YAML node")? {
        Event::Alias(id) => {
            return anchors
                .get(&id)
                .cloned()
                .ok_or("recursive or missing alias")
        }
        Event::Scalar(text, style, anchor, tag) => {
            let tag = tag_name(tag.as_ref())?;
            let merge = tag == Some("merge")
                || (tag.is_none() && style == TScalarStyle::Plain && text == "<<");
            let value = scalar(&text, style == TScalarStyle::Plain, tag)?;
            (anchor, Node { value, merge })
        }
        Event::SequenceStart(anchor, tag) => {
            if !matches!(tag_name(tag.as_ref())?, None | Some("seq")) {
                return Err("unsupported sequence tag");
            }
            let mut values = Vec::new();
            while !matches!(events.peek(), Some(Event::SequenceEnd)) {
                values.push(node(events, anchors)?.value);
            }
            events.next();
            (
                anchor,
                Node {
                    value: Value::Sequence(values),
                    merge: false,
                },
            )
        }
        Event::MappingStart(anchor, tag) => {
            if !matches!(tag_name(tag.as_ref())?, None | Some("map")) {
                return Err("unsupported mapping tag");
            }
            let mut inherited = Mapping::new();
            let mut explicit = Vec::new();
            while !matches!(events.peek(), Some(Event::MappingEnd)) {
                let key = node(events, anchors)?;
                let value = node(events, anchors)?.value;
                if key.merge {
                    let maps = match value {
                        Value::Mapping(map) => vec![map],
                        Value::Sequence(values) => values
                            .into_iter()
                            .map(|value| match value {
                                Value::Mapping(map) => Ok(map),
                                _ => Err("merge sequence member must be a mapping"),
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                        _ => return Err("merge value must be a mapping or sequence"),
                    };
                    // Earlier sequence members win. Explicit keys always
                    // override inherited keys, regardless of textual order.
                    for map in maps.into_iter().rev() {
                        for (key, value) in map {
                            inherited.insert(key, value);
                        }
                    }
                } else {
                    if matches!(key.value, Value::Mapping(_) | Value::Sequence(_)) {
                        return Err("unhashable YAML key");
                    }
                    explicit.push((key.value, value));
                }
            }
            events.next();
            for (key, value) in explicit {
                inherited.insert(key, value);
            }
            (
                anchor,
                Node {
                    value: Value::Mapping(inherited),
                    merge: false,
                },
            )
        }
        _ => return Err("unexpected YAML event"),
    };
    if anchor != 0 {
        anchors.insert(anchor, result.clone());
    }
    Ok(result)
}

fn scalar(text: &str, plain: bool, tag: Option<&str>) -> Result<Value, &'static str> {
    static INTEGER: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(r"\A(?:[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)\z").unwrap()
    });
    static FLOAT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(r"\A(?:[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))\z").unwrap()
    });
    if tag == Some("str") || (!plain && tag.is_none()) {
        return Ok(Value::String(text.into()));
    }
    let resolved = tag.unwrap_or_else(|| {
        if matches!(text, "" | "~" | "null" | "Null" | "NULL") {
            "null"
        } else if matches!(
            text,
            "yes"
                | "Yes"
                | "YES"
                | "no"
                | "No"
                | "NO"
                | "true"
                | "True"
                | "TRUE"
                | "false"
                | "False"
                | "FALSE"
                | "on"
                | "On"
                | "ON"
                | "off"
                | "Off"
                | "OFF"
        ) {
            "bool"
        } else if INTEGER.is_match(text).unwrap_or(false) {
            "int"
        } else if FLOAT.is_match(text).unwrap_or(false) {
            "float"
        } else {
            "str"
        }
    });
    match resolved {
        "null" => Ok(Value::Null),
        "str" | "merge" | "value" => Ok(Value::String(text.into())),
        "bool" => match text.to_ascii_lowercase().as_str() {
            "yes" | "true" | "on" => Ok(Value::Bool(true)),
            "no" | "false" | "off" => Ok(Value::Bool(false)),
            _ => Err("invalid boolean"),
        },
        "int" => {
            let text = text.replace('_', "");
            let negative = text.starts_with('-');
            let digits = text.strip_prefix(['+', '-']).unwrap_or(&text);
            let number = if let Some(binary) = digits.strip_prefix("0b") {
                i128::from_str_radix(binary, 2).ok()
            } else if let Some(hex) = digits.strip_prefix("0x") {
                i128::from_str_radix(hex, 16).ok()
            } else if digits.starts_with('0') {
                i128::from_str_radix(digits, 8).ok()
            } else if digits.contains(':') {
                digits.split(':').try_fold(0i128, |sum, digit| {
                    sum.checked_mul(60)?.checked_add(digit.parse().ok()?)
                })
            } else {
                digits.parse::<i128>().ok()
            }
            .ok_or("invalid or oversized integer")?;
            let number = if negative { -number } else { number };
            if let Ok(number) = i64::try_from(number) {
                Ok(Value::Number(number.into()))
            } else {
                u64::try_from(number)
                    .map(|number| Value::Number(number.into()))
                    .map_err(|_| "integer exceeds metadata representation")
            }
        }
        "float" => {
            let text = text.replace('_', "").to_ascii_lowercase();
            let sign = if text.starts_with('-') { -1.0 } else { 1.0 };
            let digits = text.strip_prefix(['+', '-']).unwrap_or(&text);
            let number = match digits {
                ".inf" => f64::INFINITY,
                ".nan" => f64::NAN,
                _ if digits.contains(':') => {
                    // Python accumulates from the least significant field.
                    let mut sum = 0.0;
                    let mut base = 1.0;
                    for digit in digits.rsplit(':') {
                        sum += digit.parse::<f64>().map_err(|_| "invalid float")? * base;
                        base *= 60.0;
                    }
                    sum
                }
                _ => digits.parse::<f64>().map_err(|_| "invalid float")?,
            };
            Ok(Value::Number((sign * number).into()))
        }
        _ => Err("unsupported YAML scalar tag"),
    }
}
