//! Pure processing steps: trimming silence off the audio, and applying literal text fixes.
//!
//! Both are deliberately simple and deterministic. Trimming uses an energy gate rather than a
//! neural VAD, and replacements are literal rather than fuzzy, because a step you can predict is a
//! step that cannot surprise you mid-sentence.

/// Trim leading and trailing near-silence from a 16 kHz mono buffer.
///
/// Speech is bracketed by dead air: the gap between pressing the key and starting to talk, and
/// between stopping and releasing. Sending it to the model wastes time and, on some models, invites
/// a hallucination at the quiet edges. This finds the first and last frames above a level and keeps
/// a small pad around them so no consonant is clipped.
///
/// Returns the input unchanged if it finds no speech (the silence check upstream will reject it).
pub fn trim_silence(samples: &[f32], rate: u32) -> Vec<f32> {
    const FRAME_MS: usize = 20;
    // Below this peak, a frame is treated as silence. Matches the app's silence-reject threshold.
    const GATE: f32 = 0.01;
    // Keep this much either side of detected speech so onsets and tails are never clipped.
    const PAD_MS: usize = 120;

    let frame = (rate as usize * FRAME_MS / 1000).max(1);
    let pad = rate as usize * PAD_MS / 1000;

    let loud = |chunk: &[f32]| chunk.iter().fold(0.0f32, |m, s| m.max(s.abs())) >= GATE;

    let first = samples
        .chunks(frame)
        .position(loud)
        .map(|i| i * frame);
    let last = samples
        .chunks(frame)
        .rposition(loud)
        .map(|i| (i + 1) * frame);

    match (first, last) {
        (Some(start), Some(end)) => {
            let start = start.saturating_sub(pad);
            let end = (end + pad).min(samples.len());
            samples[start..end].to_vec()
        }
        // No frame crossed the gate: leave it for the caller's silence check to reject.
        _ => samples.to_vec(),
    }
}

/// Apply literal, case-insensitive, whole-word replacements to a transcript.
///
/// Each rule is `[heard, wanted]`. Matching ignores case but preserves nothing else: the wanted
/// text is inserted verbatim, so `["github", "GitHub"]` produces exactly `GitHub`. Whole-word only,
/// so a rule for `"cat"` never touches `"category"`. Rules are applied in order.
pub fn apply_replacements(text: &str, rules: &[[String; 2]]) -> String {
    let mut out = text.to_string();
    for [from, to] in rules {
        if from.is_empty() {
            continue;
        }
        out = replace_whole_word_ci(&out, from, to);
    }
    out
}

/// Case-insensitive whole-word replacement. A match must be bounded by non-alphanumeric characters
/// (or the string ends) on both sides, so `from` embedded inside a larger word is left alone.
fn replace_whole_word_ci(haystack: &str, from: &str, to: &str) -> String {
    let hay_lower = haystack.to_lowercase();
    let needle = from.to_lowercase();
    let bytes = haystack.as_bytes();

    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0;

    while let Some(rel) = hay_lower[cursor..].find(&needle) {
        let start = cursor + rel;
        let end = start + needle.len();

        let left_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let right_ok = end == bytes.len() || !is_word_byte(bytes[end]);

        if left_ok && right_ok {
            out.push_str(&haystack[cursor..start]);
            out.push_str(to);
            cursor = end;
        } else {
            // Overlapping partial match: emit up to and including this char and keep scanning.
            out.push_str(&haystack[cursor..start + 1]);
            cursor = start + 1;
        }
    }
    out.push_str(&haystack[cursor..]);
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(a: &str, b: &str) -> [String; 2] {
        [a.to_string(), b.to_string()]
    }

    #[test]
    fn trims_silence_from_both_ends() {
        // 16 kHz: 0.5 s of silence, 0.2 s of tone, 0.5 s of silence.
        let mut s = vec![0.0f32; 8000];
        s.extend(std::iter::repeat(0.5).take(3200));
        s.extend(std::iter::repeat(0.0).take(8000));
        let trimmed = trim_silence(&s, 16_000);
        // Padding keeps some silence, but the bulk is gone.
        assert!(trimmed.len() < s.len());
        assert!(trimmed.len() >= 3200);
    }

    #[test]
    fn trim_leaves_all_silence_untouched() {
        let s = vec![0.0f32; 4000];
        assert_eq!(trim_silence(&s, 16_000).len(), s.len());
    }

    #[test]
    fn replacement_is_case_insensitive_but_output_is_verbatim() {
        let r = [rule("github", "GitHub")];
        assert_eq!(apply_replacements("push to Github now", &r), "push to GitHub now");
        assert_eq!(apply_replacements("GITHUB", &r), "GitHub");
    }

    #[test]
    fn replacement_respects_word_boundaries() {
        let r = [rule("cat", "dog")];
        assert_eq!(apply_replacements("the cat sat", &r), "the dog sat");
        assert_eq!(apply_replacements("category", &r), "category");
    }

    #[test]
    fn multi_word_phrase_replacement() {
        let r = [rule("k eight s", "k8s")];
        assert_eq!(apply_replacements("deploy to k eight s today", &r), "deploy to k8s today");
    }

    #[test]
    fn rules_apply_in_order_and_empty_is_skipped() {
        let r = [rule("", "x"), rule("foo", "bar")];
        assert_eq!(apply_replacements("foo", &r), "bar");
    }

    #[test]
    fn no_rules_returns_input() {
        assert_eq!(apply_replacements("unchanged", &[]), "unchanged");
    }
}
