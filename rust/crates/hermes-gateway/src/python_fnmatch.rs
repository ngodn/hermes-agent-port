//! Whole-string glob matching compatible with the Python `fnmatchcase` subset
//! used by Hermes configuration.

/// Match `text` against a case-sensitive Python-style glob.
///
/// Callers that need case-insensitive behavior normalize both inputs first.
/// Slash and leading dots are ordinary characters. Dynamic programming keeps
/// repeated stars and literals bounded.
pub fn matches(text: &str, pattern: &str) -> bool {
    enum Token {
        Star,
        Any,
        Literal(char),
        Class(bool, Vec<(char, char)>),
    }
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let character = chars[i];
        i += 1;
        tokens.push(match character {
            '*' => Token::Star,
            '?' => Token::Any,
            '[' => {
                let start = i;
                let negated = chars.get(i) == Some(&'!');
                let mut end = i + usize::from(negated);
                if chars.get(end) == Some(&']') {
                    end += 1;
                }
                while end < chars.len() && chars[end] != ']' {
                    end += 1;
                }
                if end == chars.len() {
                    Token::Literal('[')
                } else {
                    let mut ranges = Vec::new();
                    let mut at = start + usize::from(negated);
                    while at < end {
                        if at + 2 < end && chars[at + 1] == '-' {
                            if chars[at] <= chars[at + 2] {
                                ranges.push((chars[at], chars[at + 2]));
                            }
                            at += 3;
                        } else {
                            ranges.push((chars[at], chars[at]));
                            at += 1;
                        }
                    }
                    i = end + 1;
                    Token::Class(negated, ranges)
                }
            }
            character => Token::Literal(character),
        });
    }
    let text: Vec<_> = text.chars().collect();
    let mut previous = vec![false; text.len() + 1];
    previous[0] = true;
    for token in tokens {
        let mut current = vec![false; text.len() + 1];
        if matches!(token, Token::Star) {
            current[0] = previous[0];
        }
        for (i, character) in text.iter().enumerate() {
            current[i + 1] = match &token {
                Token::Star => previous[i + 1] || current[i],
                Token::Any => previous[i],
                Token::Literal(expected) => previous[i] && character == expected,
                Token::Class(negated, ranges) => {
                    previous[i]
                        && (ranges
                            .iter()
                            .any(|(start, end)| start <= character && character <= end)
                            != *negated)
                }
            };
        }
        previous = current;
    }
    previous[text.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_python_wildcards_classes_and_full_string_anchoring() {
        for (text, pattern, expected) in [
            ("git push origin", "git push*", true),
            ("git push origin", "git push", false),
            ("rm -r foo", "rm -? foo", true),
            ("rm -rf foo", "rm -? foo", false),
            ("rm -r foo", "rm -[rf]* foo", true),
            ("rm -f foo", "rm -[!f]* foo", false),
            ("path/to/file", "path*", true),
        ] {
            assert_eq!(matches(text, pattern), expected, "{text:?} {pattern:?}");
        }
    }
}
