//! Pure parsing and range selection for manual conversation compression.

use crate::session_db::HistoryMessage;

pub const DEFAULT_KEEP_LAST: usize = 2;
pub const MAX_KEEP_LAST: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompressArgs {
    pub partial: bool,
    pub keep_last: usize,
    pub focus_topic: Option<String>,
    pub preview: bool,
    pub aggressive: bool,
}

pub fn parse(raw_args: &str) -> CompressArgs {
    let mut preview = false;
    let mut aggressive = false;
    let kept = raw_args
        .split_whitespace()
        .filter(|token| match token.to_ascii_lowercase().as_str() {
            "--preview" | "--dry-run" | "--dryrun" => {
                preview = true;
                false
            }
            "--aggressive" => {
                aggressive = true;
                false
            }
            _ => true,
        })
        .collect::<Vec<_>>()
        .join(" ");

    let mut text = kept.trim();
    if text
        .get(..10)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("up to here"))
    {
        text = text.get(6..).unwrap_or_default();
    }
    let tokens = text.split_whitespace().collect::<Vec<_>>();
    let first = tokens.first().map(|token| token.to_ascii_lowercase());
    let (partial, keep_last, focus_topic) = match first.as_deref() {
        Some("here") => (
            true,
            tokens
                .get(1)
                .map_or(DEFAULT_KEEP_LAST, |value| coerce_keep(value)),
            None,
        ),
        Some("--keep" | "-k") if tokens.len() >= 2 => (true, coerce_keep(tokens[1]), None),
        Some(first) if first.starts_with("--keep=") => (
            true,
            coerce_keep(first.split_once('=').map_or("", |(_, value)| value)),
            None,
        ),
        _ => (
            false,
            DEFAULT_KEEP_LAST,
            (!text.is_empty()).then(|| text.to_owned()),
        ),
    };
    CompressArgs {
        partial,
        keep_last,
        focus_topic,
        preview,
        aggressive,
    }
}

fn coerce_keep(value: &str) -> usize {
    value
        .parse::<i64>()
        .unwrap_or(DEFAULT_KEEP_LAST as i64)
        .clamp(1, MAX_KEEP_LAST as i64) as usize
}

/// Return the index where the verbatim tail starts. `None` means the caller
/// should fall back to full compression because no useful head remains.
pub fn partial_boundary(history: &[HistoryMessage], keep_last: usize) -> Option<usize> {
    let mut seen = 0;
    let mut earliest_user = None;
    for index in (0..history.len()).rev() {
        if history[index].role == "user" {
            seen += 1;
            earliest_user = Some(index);
            if seen >= keep_last.max(1) {
                return (index > 0).then_some(index);
            }
        }
    }
    earliest_user.filter(|index| *index > 0)
}

/// Python's preview estimator is deliberately rough. This keeps the same
/// useful order of magnitude without making tokenizer availability a command
/// dependency.
pub fn estimate_tokens(history: &[&HistoryMessage]) -> usize {
    let chars = history
        .iter()
        .map(|message| message.content.chars().count() + message.role.len() + 4)
        .sum::<usize>();
    chars.div_ceil(4)
}

pub fn preview_lines(history: &[HistoryMessage], args: &CompressArgs) -> Vec<String> {
    let visible = history
        .iter()
        .filter(|message| {
            matches!(message.role.as_str(), "user" | "assistant") && !message.content.is_empty()
        })
        .collect::<Vec<_>>();
    let total = visible.len();
    let boundary = args
        .partial
        .then(|| {
            let owned = visible
                .iter()
                .map(|item| (*item).clone())
                .collect::<Vec<_>>();
            partial_boundary(&owned, args.keep_last)
        })
        .flatten();
    let head_count = boundary.unwrap_or(total);
    let tail_count = total.saturating_sub(head_count);
    let effective_partial = args.partial && boundary.is_some();
    let mut lines = vec![
        "Preview - no changes made.".to_owned(),
        format!(
            "Would compress {head_count} of {total} message(s) (~{} tokens currently in context).",
            estimate_tokens(&visible)
        ),
    ];
    if effective_partial {
        lines.push(format!(
            "Boundary: keeping the last {} exchange(s) ({tail_count} message(s)) verbatim.",
            args.keep_last
        ));
    } else if args.partial {
        lines.push(
            "Boundary: 'here' split would keep everything - falling back to full compression."
                .into(),
        );
    }
    if let Some(focus) = &args.focus_topic {
        lines.push(format!("Focus topic: \"{focus}\""));
    }
    lines.push("Run the command again without --preview to apply.".into());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, content: &str) -> HistoryMessage {
        HistoryMessage {
            role: role.into(),
            content: content.into(),
            api_content: None,
        }
    }

    fn history(pairs: usize) -> Vec<HistoryMessage> {
        (0..pairs)
            .flat_map(|index| {
                [
                    message("user", &format!("u{index}")),
                    message("assistant", &format!("a{index}")),
                ]
            })
            .collect()
    }

    #[test]
    fn parses_flags_partial_forms_and_focus() {
        assert_eq!(
            parse("--preview up to here 4 --aggressive"),
            CompressArgs {
                partial: true,
                keep_last: 4,
                focus_topic: None,
                preview: true,
                aggressive: true,
            }
        );
        assert_eq!(parse("-k nope").keep_last, DEFAULT_KEEP_LAST);
        assert_eq!(parse("here 0").keep_last, 1);
        assert_eq!(parse("--keep=999").keep_last, MAX_KEEP_LAST);
        assert_eq!(
            parse("preserve database decisions").focus_topic.as_deref(),
            Some("preserve database decisions")
        );
    }

    #[test]
    fn partial_boundary_starts_on_the_nth_newest_user() {
        let mut messages = history(3);
        messages.insert(4, message("tool", "result"));
        assert_eq!(partial_boundary(&messages, 1), Some(5));
        assert_eq!(partial_boundary(&messages, 2), Some(2));
        assert_eq!(partial_boundary(&messages, 3), None);
        assert_eq!(
            partial_boundary(
                &[
                    message("assistant", "preface"),
                    message("user", "only turn")
                ],
                2
            ),
            Some(1)
        );
    }

    #[test]
    fn preview_matches_the_python_message_range_contract() {
        let lines = preview_lines(&history(4), &parse("--preview here 2"));
        assert!(lines.iter().any(|line| line.contains("4 of 8")));
        assert!(lines.iter().any(|line| line.contains("last 2 exchange")));
        assert!(lines.iter().any(|line| line.contains("no changes made")));
    }
}
