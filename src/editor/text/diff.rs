//! Minimal single-replacement differences between UTF-8 texts.

use super::span::{ByteSpan, SpanError};
use super::utf8::Text;

/// A replacement of a half-open span in the original text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diff {
    pub span: ByteSpan,
    pub replacement: Text,
}

/// Strips the longest common prefix, then the longest non-overlapping suffix.
/// All positions are Unicode scalar boundaries, measured in bytes.
pub fn diff(before: &Text, after: &Text) -> Option<Diff> {
    let a = before.as_str();
    let b = after.as_str();
    if a == b {
        return None;
    }
    let prefix: usize = a
        .chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(ch, _)| ch.len_utf8())
        .sum();
    let suffix: usize = a[prefix..]
        .chars()
        .rev()
        .zip(b[prefix..].chars().rev())
        .take_while(|(x, y)| x == y)
        .map(|(ch, _)| ch.len_utf8())
        .sum();
    Some(Diff {
        span: ByteSpan::validate_in(before, prefix, a.len() - suffix)
            .expect("common character boundaries are valid"),
        replacement: Text::from_str(&b[prefix..b.len() - suffix]),
    })
}

impl Diff {
    /// Applies the replacement, revalidating both endpoints in the supplied text.
    /// A valid span alone does not establish that the text is the original input.
    pub fn apply(&self, text: &Text) -> Result<Text, SpanError> {
        self.span.slice_in(text)?;
        let start = self.span.start().as_usize();
        let end = self.span.end().as_usize();
        let mut result = String::new();
        result.push_str(&text.as_str()[..start]);
        result.push_str(self.replacement.as_str());
        result.push_str(&text.as_str()[end..]);
        Ok(Text::from_str(&result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    fn check(a: &str, b: &str) {
        let before = Text::from_str(a);
        let after = Text::from_str(b);
        let change = diff(&before, &after);
        assert_eq!(change.is_none(), a == b, "{a:?} -> {b:?}");
        let Some(change) = change else { return };
        assert_eq!(change.apply(&before), Ok(after));

        // Enumerate every boundary to independently establish maximality.
        let prefix = before
            .char_boundaries()
            .into_iter()
            .map(|p| p.as_usize())
            .filter(|&p| b.is_char_boundary(p) && a[..p] == b[..p])
            .max()
            .unwrap();
        let suffix = before
            .char_boundaries()
            .into_iter()
            .map(|p| a.len() - p.as_usize())
            .filter(|&n| {
                n <= a.len() - prefix
                    && n <= b.len() - prefix
                    && b.is_char_boundary(b.len() - n)
                    && a[a.len() - n..] == b[b.len() - n..]
            })
            .max()
            .unwrap();
        assert_eq!(change.span.start().as_usize(), prefix, "{a:?} -> {b:?}");
        assert_eq!(change.span.end().as_usize(), a.len() - suffix);
        assert_eq!(change.replacement.as_str(), &b[prefix..b.len() - suffix]);
    }

    #[test]
    fn deterministic_corpus_laws() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for a in corpus {
                for b in corpus {
                    check(a, b);
                }
            }
        });
    }

    #[test]
    fn exhaustive_short_strings_and_overlaps() {
        let mut strings = vec![String::new()];
        let mut frontier = strings.clone();
        for _ in 0..3 {
            frontier = frontier
                .iter()
                .flat_map(|s| ['a', 'b', '界', '😀'].map(|c| format!("{s}{c}")))
                .collect();
            strings.extend(frontier.iter().cloned());
        }
        for a in &strings {
            for b in &strings {
                check(a, b);
            }
        }
        for (a, b) in [("aa", "aaa"), ("aba", "aa"), ("é", "ê"), ("é", "©")] {
            check(a, b);
        }
    }

    #[test]
    fn rejects_incompatible_spans() {
        let foreign = Text::from_str("界");
        let inside_start = diff(&Text::from_str("ab"), &Text::from_str("ac")).unwrap();
        assert_eq!(
            inside_start.apply(&foreign),
            Err(SpanError::NotCharBoundary { offset: 1 })
        );
        let inside_end = diff(&Text::from_str("ab"), &Text::from_str("xb")).unwrap();
        assert_eq!(
            inside_end.apply(&foreign),
            Err(SpanError::NotCharBoundary { offset: 1 })
        );
        let beyond = diff(&Text::from_str("abcd"), &Text::from_str("abcx")).unwrap();
        assert_eq!(
            beyond.apply(&foreign),
            Err(SpanError::OutOfBounds { offset: 4, len: 3 })
        );
    }
}
