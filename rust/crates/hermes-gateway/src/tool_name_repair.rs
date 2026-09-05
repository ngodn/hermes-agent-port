//! Model-emitted tool-name recovery from agent_runtime_helpers.repair_tool_call.
use std::collections::{BTreeSet, HashMap};

fn normalized(name: &str) -> String {
    name.to_lowercase().replace(['-', ' '], "_")
}

fn camel_snake(name: &str) -> String {
    let mut out = String::new();
    for (index, character) in name.chars().enumerate() {
        if index > 0 && character.is_ascii_uppercase() {
            out.push('_');
        }
        out.push(character);
    }
    out.to_lowercase()
}

/// SequenceMatcher ratio with Python's default autojunk behavior. The query
/// is sequence B, as in difflib.get_close_matches; swapping inputs can change
/// longest-match tie breaking and therefore the ratio.
fn similarity(candidate: &str, query: &str) -> f64 {
    let a: Vec<char> = candidate.chars().collect();
    let b: Vec<char> = query.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut positions: HashMap<char, Vec<usize>> = HashMap::new();
    for (index, character) in b.iter().enumerate() {
        positions.entry(*character).or_default().push(index);
    }
    if b.len() >= 200 {
        positions.retain(|_, indexes| indexes.len() <= b.len() / 100 + 1);
    }
    let mut regions = vec![(0, a.len(), 0, b.len())];
    let mut total = 0;
    while let Some((alo, ahi, blo, bhi)) = regions.pop() {
        let (mut best_i, mut best_j, mut size) = (alo, blo, 0);
        let mut previous: HashMap<usize, usize> = HashMap::new();
        for (i, character) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut current = HashMap::new();
            if let Some(indexes) = positions.get(character) {
                for &j in indexes.iter().filter(|&&j| j >= blo && j < bhi) {
                    let length = j
                        .checked_sub(1)
                        .and_then(|j| previous.get(&j))
                        .copied()
                        .unwrap_or(0)
                        + 1;
                    current.insert(j, length);
                    if length > size {
                        best_i = i + 1 - length;
                        best_j = j + 1 - length;
                        size = length;
                    }
                }
            }
            previous = current;
        }
        // Popular elements are excluded from indexing, but can extend a match.
        while best_i > alo && best_j > blo && a[best_i - 1] == b[best_j - 1] {
            best_i -= 1;
            best_j -= 1;
            size += 1;
        }
        while best_i + size < ahi && best_j + size < bhi && a[best_i + size] == b[best_j + size] {
            size += 1;
        }
        if size > 0 {
            total += size;
            if alo < best_i && blo < best_j {
                regions.push((alo, best_i, blo, best_j));
            }
            if best_i + size < ahi && best_j + size < bhi {
                regions.push((best_i + size, ahi, best_j + size, bhi));
            }
        }
    }
    2.0 * total as f64 / (a.len() + b.len()) as f64
}

/// Return a registered name, or None when no sufficiently close match exists.
/// Exact candidate collisions use lexical order; Python iterates a hash-random
/// set here, so its choice between multiple exact candidates is not stable.
pub fn repair(name: &str, valid: &[String]) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let mut name = name;
    for separator in ['"', '\'', '<', '>'] {
        if let Some(index) = name.find(separator).filter(|index| *index > 0) {
            name = &name[..index];
        }
    }
    let lowered = name.to_lowercase();
    if valid.contains(&lowered) {
        return Some(lowered);
    }
    let norm = normalized(name);
    if valid.contains(&norm) {
        return Some(norm);
    }
    let mut candidates =
        BTreeSet::from([name.to_owned(), lowered.clone(), norm, camel_snake(name)]);
    for _ in 0..2 {
        let mut extra = BTreeSet::new();
        for candidate in &candidates {
            let lower = candidate.to_lowercase();
            if let Some(suffix) = ["_tool", "-tool", "tool"]
                .into_iter()
                .find(|suffix| lower.ends_with(suffix))
            {
                // Suffixes are ASCII; lowercasing may expand the prefix but the
                // same trailing ASCII bytes are removed from the original.
                let stripped =
                    candidate[..candidate.len() - suffix.len()].trim_end_matches(['_', '-']);
                if !stripped.is_empty() {
                    extra.extend([
                        stripped.to_owned(),
                        normalized(stripped),
                        camel_snake(stripped),
                    ]);
                }
            }
        }
        candidates.extend(extra);
    }
    if let Some(candidate) = candidates
        .into_iter()
        .find(|candidate| !candidate.is_empty() && valid.contains(candidate))
    {
        return Some(candidate);
    }
    valid
        .iter()
        .map(|candidate| (similarity(candidate, &lowered), candidate))
        .filter(|(ratio, _)| *ratio >= 0.7)
        .max_by(|(left_ratio, left), (right_ratio, right)| {
            left_ratio.total_cmp(right_ratio).then(left.cmp(right))
        })
        .map(|(_, name)| name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn python_name_repair_cases() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/tool-name-repair-goldens.json"))
                .unwrap();
        for row in rows.as_array().unwrap() {
            let valid: Vec<String> = serde_json::from_value(row["valid"].clone()).unwrap();
            assert_eq!(
                repair(row["name"].as_str().unwrap(), &valid),
                row["expected"].as_str().map(str::to_owned),
                "{row}"
            );
        }
    }
}
