//! Collision-resistant batch packing and unpacking for web translation providers.

use crate::TranslateError;

const BASE_DELIMITER: &str = "|||STS_SEP|||";

/// A single packed payload containing multiple text segments separated by a collision-free token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedBatch {
    /// Joined text payload ready to send in a single translation request.
    pub payload: String,
    /// Delimiter token guaranteed not to collide with any input segment.
    pub delimiter: String,
}

fn normalize_char(ch: char) -> char {
    match ch {
        '｜' | '¦' => '|',
        other => other.to_ascii_lowercase(),
    }
}

fn normalize_compact(input: &str) -> String {
    input
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .map(normalize_char)
        .collect()
}

fn candidate_delimiter(index: usize) -> String {
    if index == 0 {
        BASE_DELIMITER.to_owned()
    } else {
        format!("|||STS_SEP_{index}|||")
    }
}

fn select_collision_free_delimiter(texts: &[&str]) -> String {
    let normalized_inputs: Vec<String> = texts.iter().map(|text| normalize_compact(text)).collect();

    for index in 0usize.. {
        let candidate = candidate_delimiter(index);
        let normalized_candidate = normalize_compact(&candidate);
        let collides = texts.iter().any(|text| text.contains(&candidate))
            || normalized_inputs
                .iter()
                .any(|norm| norm.contains(&normalized_candidate));
        if !collides {
            return candidate;
        }
    }

    unreachable!("unbounded candidate index always terminates for finite inputs")
}

/// Packs multiple text segments into a single payload using a dynamically chosen delimiter
/// guaranteed not to collide with any input segment.
#[must_use]
pub fn pack_batch(texts: &[&str]) -> PackedBatch {
    if texts.is_empty() {
        return PackedBatch {
            payload: String::new(),
            delimiter: BASE_DELIMITER.to_owned(),
        };
    }

    let delimiter = select_collision_free_delimiter(texts);
    if texts.len() == 1 {
        return PackedBatch {
            payload: texts[0].to_owned(),
            delimiter,
        };
    }

    // Surrounding newlines prevent web translators from merging adjacent bubbles into one sentence.
    let joiner = format!("\n{delimiter}\n");
    let total_len = texts.iter().map(|s| s.len()).sum::<usize>()
        + joiner.len().saturating_mul(texts.len().saturating_sub(1));
    let mut payload = String::with_capacity(total_len);
    for (idx, text) in texts.iter().enumerate() {
        if idx > 0 {
            payload.push_str(&joiner);
        }
        payload.push_str(text);
    }

    PackedBatch { payload, delimiter }
}

fn find_flexible_delimiter_spans(text: &str, target: &[char]) -> Vec<(usize, usize)> {
    let Some(&first_target) = target.first() else {
        return Vec::new();
    };

    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut spans = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let (start_byte, ch) = chars[i];
        if normalize_char(ch) != first_target {
            i += 1;
            continue;
        }

        let mut j = i + 1;
        let mut t_idx = 1;
        let mut end_byte = start_byte + ch.len_utf8();

        while t_idx < target.len() && j < chars.len() {
            let (byte_pos, candidate_ch) = chars[j];
            if candidate_ch.is_whitespace() {
                j += 1;
                continue;
            }
            if normalize_char(candidate_ch) == target[t_idx] {
                end_byte = byte_pos + candidate_ch.len_utf8();
                t_idx += 1;
                j += 1;
            } else {
                break;
            }
        }

        if t_idx == target.len() {
            spans.push((start_byte, end_byte));
            i = j;
        } else {
            i += 1;
        }
    }

    spans
}

/// Splits a translated batch payload back into `expected_len` segments using `delimiter`,
/// tolerating spacing, casing, or full-width pipe variations introduced by upstream translators.
///
/// # Errors
///
/// Returns [`TranslateError::InvalidResponse`] if the delimiter is empty for multi-segment
/// batches or if the unpacked segment count does not match `expected_len`.
pub fn unpack_batch(
    translated: &str,
    delimiter: &str,
    expected_len: usize,
) -> Result<Vec<String>, TranslateError> {
    if expected_len == 0 {
        return if translated.trim().is_empty() {
            Ok(Vec::new())
        } else {
            Err(TranslateError::InvalidResponse(
                "expected 0 translated batch segments, got non-empty payload".into(),
            ))
        };
    }

    if expected_len == 1 {
        return Ok(vec![translated.trim().to_owned()]);
    }

    let target: Vec<char> = delimiter
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .map(normalize_char)
        .collect();
    if target.is_empty() {
        return Err(TranslateError::InvalidResponse(
            "batch delimiter cannot be empty".into(),
        ));
    }

    // Fast path: exact delimiter match preserves segment boundaries directly.
    let exact: Vec<String> = translated
        .split(delimiter)
        .map(|part| part.trim().to_owned())
        .collect();
    if exact.len() == expected_len {
        return Ok(exact);
    }

    // Resilient path: tolerate whitespace, case, and full-width pipe drift from web translators.
    let spans = find_flexible_delimiter_spans(translated, &target);
    let mut parts = Vec::with_capacity(spans.len() + 1);
    let mut cursor = 0;
    for (start, end) in spans {
        parts.push(translated[cursor..start].trim().to_owned());
        cursor = end;
    }
    parts.push(translated[cursor..].trim().to_owned());

    if parts.len() == expected_len {
        Ok(parts)
    } else {
        Err(TranslateError::InvalidResponse(format!(
            "expected {expected_len} translated batch segments for delimiter `{delimiter}`, got {}",
            parts.len()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_and_unpack_avoids_collision_when_input_contains_separator() {
        let inputs = vec!["Line 1 |||STS_SEP||| inside", "Line 2"];
        let packed = pack_batch(&inputs);
        let unpacked = match unpack_batch(&packed.payload, &packed.delimiter, inputs.len()) {
            Ok(items) => items,
            Err(err) => panic!("expected unpack_batch to succeed, got error: {err}"),
        };
        assert_eq!(unpacked, vec!["Line 1 |||STS_SEP||| inside", "Line 2"]);
        assert_ne!(packed.delimiter, "|||STS_SEP|||");
    }

    #[test]
    fn pack_and_unpack_increments_suffix_past_multiple_colliding_delimiters() {
        let inputs = vec![
            "Contains |||STS_SEP||| and |||STS_SEP_1|||",
            "Also ||| STS_SEP_2 ||| spaced",
            "Plain third line",
        ];
        let packed = pack_batch(&inputs);
        assert_eq!(packed.delimiter, "|||STS_SEP_3|||");
        let unpacked = match unpack_batch(&packed.payload, &packed.delimiter, inputs.len()) {
            Ok(items) => items,
            Err(err) => panic!("expected unpack_batch to succeed, got error: {err}"),
        };
        assert_eq!(
            unpacked,
            vec![
                "Contains |||STS_SEP||| and |||STS_SEP_1|||",
                "Also ||| STS_SEP_2 ||| spaced",
                "Plain third line",
            ]
        );
    }

    #[test]
    fn unpack_tolerates_upstream_spacing_casing_and_fullwidth_pipes() {
        let translated = "Xin chào  \n  ｜ ｜ ｜ sts_sep_1 ｜ ｜ ｜  \n  Thế giới";
        let unpacked = match unpack_batch(translated, "|||STS_SEP_1|||", 2) {
            Ok(items) => items,
            Err(err) => panic!("expected resilient unpack to succeed, got error: {err}"),
        };
        assert_eq!(unpacked, vec!["Xin chào", "Thế giới"]);
    }

    #[test]
    fn pack_and_unpack_handles_empty_and_single_item_batches() {
        let empty = pack_batch(&[]);
        assert!(empty.payload.is_empty());
        assert_eq!(
            unpack_batch(&empty.payload, &empty.delimiter, 0),
            Ok(vec![])
        );

        let single = pack_batch(&["おはよう"]);
        assert_eq!(single.payload, "おはよう");
        assert_eq!(
            unpack_batch("  Chào buổi sáng  ", &single.delimiter, 1),
            Ok(vec!["Chào buổi sáng".to_owned()])
        );
    }

    #[test]
    fn unpack_returns_invalid_response_when_upstream_drops_delimiter() {
        let err = unpack_batch("Merged line without delimiter", "|||STS_SEP|||", 2);
        assert!(matches!(err, Err(TranslateError::InvalidResponse(_))));
    }

    #[test]
    fn pack_and_unpack_preserves_internal_newlines_and_order() {
        let inputs = vec!["Bubble 1 line A\nBubble 1 line B", "Bubble 2", "Bubble 3"];
        let packed = pack_batch(&inputs);
        let unpacked = match unpack_batch(&packed.payload, &packed.delimiter, inputs.len()) {
            Ok(items) => items,
            Err(err) => panic!("expected unpack_batch to succeed, got error: {err}"),
        };
        assert_eq!(
            unpacked,
            vec!["Bubble 1 line A\nBubble 1 line B", "Bubble 2", "Bubble 3"]
        );
    }
}
