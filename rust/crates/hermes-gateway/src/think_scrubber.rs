//! Stateful reasoning suppression at the upstream text-delta boundary.
//!
//! Port of `agent/think_scrubber.py`. Closed pairs are removed anywhere;
//! unclosed openers only start a hidden block at a line boundary. Partial tags
//! survive between feeds, and flushing starts a fresh response boundary.
const OPEN: [&str; 5] = [
    "<think>",
    "<thinking>",
    "<reasoning>",
    "<thought>",
    "<reasoning_scratchpad>",
];
const CLOSE: [&str; 5] = [
    "</think>",
    "</thinking>",
    "</reasoning>",
    "</thought>",
    "</reasoning_scratchpad>",
];

pub struct ThinkScrubber {
    in_block: bool,
    pending: String,
    ended_newline: bool,
}

impl Default for ThinkScrubber {
    fn default() -> Self {
        Self {
            in_block: false,
            pending: String::new(),
            ended_newline: true,
        }
    }
}

impl ThinkScrubber {
    pub fn feed(&mut self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let mut buf = std::mem::take(&mut self.pending);
        buf.push_str(text);
        let mut output = String::new();
        while !buf.is_empty() {
            let lower = buf.to_ascii_lowercase();
            if self.in_block {
                if let Some((index, length)) = first_tag(&lower, &CLOSE) {
                    buf.drain(..index + length);
                    self.in_block = false;
                } else {
                    let held = partial_suffix(&lower, &CLOSE);
                    self.pending = buf[buf.len() - held..].to_owned();
                    return output;
                }
            } else {
                let pair = OPEN
                    .iter()
                    .zip(CLOSE)
                    .filter_map(|(open, close)| {
                        let start = lower.find(open)?;
                        let end = lower[start + open.len()..].find(close)?
                            + start
                            + open.len()
                            + close.len();
                        Some((start, end))
                    })
                    .min_by_key(|(start, _)| *start);
                let boundary = OPEN
                    .iter()
                    .filter_map(|tag| {
                        lower
                            .match_indices(tag)
                            .find(|(index, _)| self.at_boundary(&buf[..*index]))
                            .map(|(index, _)| (index, tag.len()))
                    })
                    .min_by_key(|(index, _)| *index);
                if let Some((start, end)) =
                    pair.filter(|(start, _)| boundary.is_none_or(|(index, _)| *start <= index))
                {
                    self.emit(&buf[..start], &mut output);
                    buf.drain(..end);
                } else if let Some((index, length)) = boundary {
                    self.emit(&buf[..index], &mut output);
                    self.in_block = true;
                    buf.drain(..index + length);
                } else {
                    let held = partial_suffix(&lower, &OPEN).max(partial_suffix(&lower, &CLOSE));
                    self.pending = buf[buf.len() - held..].to_owned();
                    self.emit(&buf[..buf.len() - held], &mut output);
                    return output;
                }
            }
        }
        output
    }

    /// End a model response. Never release text from an unfinished block.
    pub fn flush(&mut self) -> String {
        let tail = std::mem::take(&mut self.pending);
        let hidden = std::mem::replace(&mut self.in_block, false);
        self.ended_newline = true;
        if hidden {
            String::new()
        } else {
            strip_closers(&tail)
        }
    }

    fn at_boundary(&self, preceding: &str) -> bool {
        match preceding.rsplit_once('\n') {
            Some((_, line)) => line.chars().all(crate::python_value::python_whitespace),
            None => {
                self.ended_newline
                    && preceding
                        .chars()
                        .all(crate::python_value::python_whitespace)
            }
        }
    }

    fn emit(&mut self, text: &str, output: &mut String) {
        let text = strip_closers(text);
        if !text.is_empty() {
            self.ended_newline = text.ends_with('\n');
            output.push_str(&text);
        }
    }
}

fn first_tag(text: &str, tags: &[&str]) -> Option<(usize, usize)> {
    tags.iter()
        .filter_map(|tag| text.find(tag).map(|index| (index, tag.len())))
        .min_by_key(|(index, _)| *index)
}

fn partial_suffix(text: &str, tags: &[&str]) -> usize {
    tags.iter()
        .flat_map(|tag| (1..tag.len()).filter(|length| text.ends_with(&tag[..*length])))
        .max()
        .unwrap_or(0)
}

fn strip_closers(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut output = String::new();
    let mut index = 0;
    while index < text.len() {
        if let Some(tag) = CLOSE.iter().find(|tag| lower[index..].starts_with(**tag)) {
            index += tag.len();
            while index < text.len()
                && matches!(text.as_bytes()[index], b' ' | b'\t' | b'\n' | b'\r')
            {
                index += 1;
            }
        } else {
            let character = text[index..].chars().next().unwrap();
            output.push(character);
            index += character.len_utf8();
        }
    }
    output
}

#[cfg(test)]
mod tests {
    #[test]
    fn streamed_outputs_match_python_at_every_delta() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/think-stream-goldens.json")).unwrap();
        for case in cases.as_array().unwrap() {
            let mut scrubber = super::ThinkScrubber::default();
            for step in case.as_array().unwrap() {
                let actual = match step["input"].as_str() {
                    Some(text) => scrubber.feed(text),
                    None => scrubber.flush(),
                };
                assert_eq!(actual, step["output"].as_str().unwrap(), "{case}");
            }
        }
    }
}
