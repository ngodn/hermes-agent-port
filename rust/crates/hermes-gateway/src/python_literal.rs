//! Literal-only Python configuration parsing. The parser supplies grammar and
//! escape handling; evaluation accepts literal AST nodes and never runs code.
#![allow(dead_code)]
use rustpython_parser::{ast, Parse};

#[derive(Clone)]
enum Literal {
    Constant(ast::Constant),
    List(Vec<Literal>),
    Tuple(Vec<Literal>),
    Set(Vec<Literal>),
    Dict(Vec<(Literal, Literal)>),
}

fn constants_equal(left: &ast::Constant, right: &ast::Constant) -> bool {
    use ast::Constant;
    if let Constant::Complex { real, imag } = left {
        if *imag == 0.0 {
            return constants_equal(&Constant::Float(*real), right);
        }
        return matches!(right, Constant::Complex { real: other_real, imag: other_imag } if real == other_real && imag == other_imag);
    }
    if matches!(right, Constant::Complex { .. }) {
        return constants_equal(right, left);
    }
    let integer = |value: &Constant| match value {
        Constant::Bool(value) => Some(ast::bigint::BigInt::from(u8::from(*value))),
        Constant::Int(value) => Some(value.clone()),
        _ => None,
    };
    match (integer(left), integer(right)) {
        (Some(left), Some(right)) => left == right,
        (Some(integer), None) | (None, Some(integer)) => {
            let float = match (left, right) {
                (Constant::Float(value), _) | (_, Constant::Float(value)) => *value,
                _ => return false,
            };
            // Fixed precision zero renders the exact integral float as decimal.
            // Converting the arbitrary-size integer to f64 would conflate keys
            // above 2^53, including values just beside a representable float.
            float.is_finite()
                && float.fract() == 0.0
                && format!("{float:.0}")
                    .parse::<ast::bigint::BigInt>()
                    .is_ok_and(|value| value == integer)
        }
        _ => left == right,
    }
}

impl PartialEq for Literal {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Constant(left), Self::Constant(right)) => constants_equal(left, right),
            (Self::List(left), Self::List(right)) | (Self::Tuple(left), Self::Tuple(right)) => {
                left == right
            }
            (Self::Set(left), Self::Set(right)) => {
                left.len() == right.len() && left.iter().all(|value| right.contains(value))
            }
            (Self::Dict(left), Self::Dict(right)) => {
                left.len() == right.len()
                    && left.iter().all(|(key, value)| {
                        right.iter().any(|(other_key, other_value)| {
                            key == other_key && value == other_value
                        })
                    })
            }
            _ => false,
        }
    }
}

impl Literal {
    fn hashable(&self) -> bool {
        match self {
            Self::Constant(_) => true,
            Self::Tuple(items) => items.iter().all(Self::hashable),
            _ => false,
        }
    }

    fn repr(&self) -> String {
        let sequence =
            |items: &[Literal]| items.iter().map(Self::repr).collect::<Vec<_>>().join(", ");
        match self {
            Self::Constant(ast::Constant::Ellipsis) => "Ellipsis".into(),
            Self::Constant(ast::Constant::Complex { real, imag }) => {
                let component = |value| {
                    let text = ast::Constant::Float(value).to_string();
                    text.strip_suffix(".0").unwrap_or(&text).to_owned()
                };
                let imaginary = component(*imag);
                if *real == 0.0 && !real.is_sign_negative() {
                    format!("{imaginary}j")
                } else {
                    format!(
                        "({}{}{imaginary}j)",
                        component(*real),
                        if imaginary.starts_with('-') { "" } else { "+" }
                    )
                }
            }
            Self::Constant(value) => value.to_string(),
            Self::List(items) => format!("[{}]", sequence(items)),
            Self::Tuple(items) => format!(
                "({}{})",
                sequence(items),
                if items.len() == 1 { "," } else { "" }
            ),
            Self::Set(items) if items.is_empty() => "set()".into(),
            Self::Set(items) => format!("{{{}}}", sequence(items)),
            Self::Dict(items) => format!(
                "{{{}}}",
                items
                    .iter()
                    .map(|(key, value)| format!("{}: {}", key.repr(), value.repr()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

fn number(expr: &ast::Expr) -> Option<ast::Constant> {
    match expr {
        ast::Expr::Constant(c)
            if matches!(
                c.value,
                ast::Constant::Int(_) | ast::Constant::Float(_) | ast::Constant::Complex { .. }
            ) =>
        {
            Some(c.value.clone())
        }
        ast::Expr::UnaryOp(op) if matches!(op.op, ast::UnaryOp::UAdd | ast::UnaryOp::USub) => {
            let ast::Expr::Constant(c) = op.operand.as_ref() else {
                return None;
            };
            match &c.value {
                ast::Constant::Int(v) => Some(ast::Constant::Int(if op.op == ast::UnaryOp::USub {
                    -v
                } else {
                    v.clone()
                })),
                ast::Constant::Float(v) => {
                    Some(ast::Constant::Float(if op.op == ast::UnaryOp::USub {
                        -v
                    } else {
                        *v
                    }))
                }
                ast::Constant::Complex { real, imag } => Some(ast::Constant::Complex {
                    real: if op.op == ast::UnaryOp::USub {
                        -real
                    } else {
                        *real
                    },
                    imag: if op.op == ast::UnaryOp::USub {
                        -imag
                    } else {
                        *imag
                    },
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

fn evaluate(expr: &ast::Expr) -> Option<Literal> {
    let items = |items: &[ast::Expr]| items.iter().map(evaluate).collect::<Option<Vec<_>>>();
    match expr {
        ast::Expr::Constant(c) => Some(Literal::Constant(c.value.clone())),
        ast::Expr::List(list) => Some(Literal::List(items(&list.elts)?)),
        ast::Expr::Tuple(tuple) => Some(Literal::Tuple(items(&tuple.elts)?)),
        ast::Expr::Set(set) => {
            let mut unique = Vec::new();
            for value in items(&set.elts)? {
                if !value.hashable() {
                    return None;
                }
                if !unique.contains(&value) {
                    unique.push(value);
                }
            }
            Some(Literal::Set(unique))
        }
        ast::Expr::Dict(dict) => {
            let mut entries: Vec<(Literal, Literal)> = Vec::new();
            for (key, value) in dict.keys.iter().zip(&dict.values) {
                let key = evaluate(key.as_ref()?)?;
                if !key.hashable() {
                    return None;
                }
                let value = evaluate(value)?;
                if let Some(entry) = entries.iter_mut().find(|(existing, _)| *existing == key) {
                    entry.1 = value;
                } else {
                    entries.push((key, value));
                }
            }
            Some(Literal::Dict(entries))
        }
        ast::Expr::Call(call)
            if call.args.is_empty()
                && call.keywords.is_empty()
                && matches!(call.func.as_ref(), ast::Expr::Name(name) if name.id.as_str()=="set") =>
        {
            Some(Literal::Set(Vec::new()))
        }
        ast::Expr::BinOp(op) if matches!(op.op, ast::Operator::Add | ast::Operator::Sub) => {
            let left = number(&op.left)?;
            let ast::Expr::Constant(right) = op.right.as_ref() else {
                return None;
            };
            let ast::Constant::Complex { real, imag } = right.value else {
                return None;
            };
            let left = match left {
                ast::Constant::Int(value) => value.to_string().parse::<f64>().ok()?,
                ast::Constant::Float(value) => value,
                _ => return None,
            };
            let subtract = op.op == ast::Operator::Sub;
            Some(Literal::Constant(ast::Constant::Complex {
                real: if subtract { left - real } else { left + real },
                imag: if subtract { -imag } else { imag },
            }))
        }
        _ => number(expr).map(Literal::Constant),
    }
}

pub fn list_strings(source: &str) -> Option<Vec<String>> {
    let expression = ast::Expr::parse(source, "<config>").ok()?;
    let Literal::List(items) = evaluate(&expression)? else {
        return None;
    };
    Some(
        items
            .into_iter()
            .map(|item| match item {
                Literal::Constant(ast::Constant::Str(text)) => text,
                item => item.repr(),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dictionary_key_identity_matches_python_without_float_rounding() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../tools/literal-key-goldens.json")).unwrap();
        for (number, case) in cases.iter().enumerate() {
            assert_eq!(
                serde_json::json!(list_strings(case["source"].as_str().unwrap())),
                case["expected"],
                "case {number}: {}",
                case["source"]
            );
        }
    }
    #[test]
    fn accepts_literal_lists_without_executing_expressions() {
        assert_eq!(
            list_strings("['one', r'two\\three', '\\u4f60', 0xff, -2, True, None, ...]"),
            Some(
                vec![
                    "one",
                    "two\\three",
                    "你",
                    "255",
                    "-2",
                    "True",
                    "None",
                    "Ellipsis"
                ]
                .into_iter()
                .map(str::to_owned)
                .collect()
            )
        );
        assert_eq!(
            list_strings("['a' 'b', (1,), {'x': 1, 'x': 2}, [3], set()]"),
            Some(
                vec!["ab", "(1,)", "{'x': 2}", "[3]", "set()"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            )
        );
        for text in [
            "[f'x']",
            "[1 + 2]",
            "[name]",
            "[str(1)]",
            "[x for x in []]",
            "[true]",
            "[{[]: 1}]",
            "[--1]",
        ] {
            assert!(list_strings(text).is_none(), "{text}");
        }
    }
}
