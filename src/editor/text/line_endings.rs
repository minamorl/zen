//! Lossless line arrays and the final LF flag.
//!
//! LF alone separates lines; CR is ordinary content. Empty text is represented
//! by `([], false)`. Otherwise the array is nonempty, no line contains LF,
//! and its last line is nonempty unless `trailing_lf` is true. Thus `[""]`
//! with the flag set represents `"\n"`, while `[""]` without it is invalid.
//! This models a line array plus an end-of-line flag without losing the final
//! newline distinction used by editors such as Neovim and Emacs.
//!
//! Splitting partitions the bytes at each LF, removing only the final empty
//! suffix when there is a trailing LF. Joining restores those separators and
//! that suffix, so `join_lines(&split_lines(text)) == Ok(text.clone())`.
//! Conversely, the invariant rules out both alternate encodings of empty text
//! and a final empty line without a flag. Hence splitting a joined valid array
//! recovers exactly the array and flag: the two operations are a bijection.
//! UTF-8 is preserved because LF is an ASCII byte and cannot occur inside a
//! multibyte scalar.
//!
//! [`super::lines::LineIndex`] counts the final suffix even when it is empty.
//! Its contents therefore equal this array with one empty line appended when
//! the text is empty or has a trailing LF; otherwise the arrays already match.
//! Both scans split at the same LF bytes, so their line contents and counts
//! agree under this correspondence, including CR and multibyte content.

use super::utf8::Text;
use std::fmt;

/// A line array and whether its text ends in LF.
///
/// Public fields allow construction of candidates; [`join_lines`] validates
/// the canonical invariant described in the module documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineBreakdown {
    /// Line contents, without separators.
    pub lines: Vec<String>,
    /// Whether the original text ends in LF.
    pub trailing_lf: bool,
}

/// An invalid candidate line breakdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineEndingError {
    /// A supplied line contains an LF separator.
    EmbeddedLineFeed { line: usize },
    /// An empty array cannot have a trailing LF.
    EmptyWithTrailingLf,
    /// A final empty line without a trailing LF has another canonical encoding.
    NonCanonicalFinalEmptyLine,
}

impl fmt::Display for LineEndingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmbeddedLineFeed { line } => write!(f, "line {line} contains LF"),
            Self::EmptyWithTrailingLf => write!(f, "empty line array has trailing LF"),
            Self::NonCanonicalFinalEmptyLine => write!(f, "final empty line requires trailing LF"),
        }
    }
}

impl std::error::Error for LineEndingError {}

/// Splits text into its unique canonical line array and final LF flag.
pub fn split_lines(text: &Text) -> LineBreakdown {
    let source = text.as_str();
    if source.is_empty() {
        return LineBreakdown {
            lines: Vec::new(),
            trailing_lf: false,
        };
    }
    let trailing_lf = source.ends_with('\n');
    let contents = if trailing_lf {
        &source[..source.len() - 1]
    } else {
        source
    };
    LineBreakdown {
        lines: contents.split('\n').map(str::to_owned).collect(),
        trailing_lf,
    }
}

/// Validates and joins a canonical line array into UTF-8 text.
pub fn join_lines(breakdown: &LineBreakdown) -> Result<Text, LineEndingError> {
    for (line, content) in breakdown.lines.iter().enumerate() {
        if content.contains('\n') {
            return Err(LineEndingError::EmbeddedLineFeed { line });
        }
    }
    if breakdown.lines.is_empty() && breakdown.trailing_lf {
        return Err(LineEndingError::EmptyWithTrailingLf);
    }
    if !breakdown.trailing_lf && breakdown.lines.last().is_some_and(String::is_empty) {
        return Err(LineEndingError::NonCanonicalFinalEmptyLine);
    }
    let mut result = breakdown.lines.join("\n");
    if breakdown.trailing_lf {
        result.push('\n');
    }
    Ok(Text::from_str(&result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};
    use crate::editor::text::lines::LineIndex;

    fn check_text(source: &str) {
        let text = Text::from_str(source);
        let breakdown = split_lines(&text);
        assert_eq!(join_lines(&breakdown), Ok(text.clone()));
        let index = LineIndex::from_text(&text);
        let mut indexed = breakdown.lines.clone();
        if breakdown.trailing_lf || indexed.is_empty() {
            indexed.push(String::new());
        }
        assert_eq!(index.line_count(), indexed.len());
        for (i, expected) in indexed.iter().enumerate() {
            let span = index.line_span(i).unwrap();
            assert_eq!(text.slice(span.start(), span.end()).unwrap(), expected);
        }
    }

    #[test]
    fn exact_cases() {
        for (source, lines, trailing_lf) in [
            ("", vec![], false),
            ("\n", vec![""], true),
            ("a", vec!["a"], false),
            ("a\n", vec!["a"], true),
            ("\n\n", vec!["", ""], true),
            ("\r", vec!["\r"], false),
            ("a\r\n", vec!["a\r"], true),
            ("a\r\nb", vec!["a\r", "b"], false),
            ("界\ne\u{301}\n😀", vec!["界", "e\u{301}", "😀"], false),
        ] {
            let expected = LineBreakdown {
                lines: lines.into_iter().map(str::to_owned).collect(),
                trailing_lf,
            };
            assert_eq!(split_lines(&Text::from_str(source)), expected);
            assert_eq!(join_lines(&expected), Ok(Text::from_str(source)));
            check_text(source);
        }
    }

    #[test]
    fn exhaustive_corpus_and_line_index() {
        let alphabet = ["a", "\n", "\r", "界", "\u{301}", "😀"];
        let mut corpus = vec![String::new()];
        let mut layer = corpus.clone();
        for _ in 0..4 {
            layer = layer
                .iter()
                .flat_map(|prefix| alphabet.iter().map(move |s| format!("{prefix}{s}")))
                .collect();
            corpus.extend(layer.iter().cloned());
        }
        for source in &corpus {
            check_text(source);
        }
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                check_text(source);
            }
        });
    }

    #[test]
    fn canonical_candidates_and_rejections() {
        let choices = ["", "a", "\r", "界", "e\u{301}", "😀", "\n", "a\nb"];
        for count in 0..=3 {
            for mut n in 0..choices.len().pow(count) {
                let mut lines = Vec::new();
                for _ in 0..count {
                    lines.push(choices[n % choices.len()].to_owned());
                    n /= choices.len();
                }
                for trailing_lf in [false, true] {
                    let b = LineBreakdown {
                        lines: lines.clone(),
                        trailing_lf,
                    };
                    let result = join_lines(&b);
                    let embedded = lines.iter().position(|line| line.contains('\n'));
                    if let Some(line) = embedded {
                        assert_eq!(result, Err(LineEndingError::EmbeddedLineFeed { line }));
                    } else if lines.is_empty() && trailing_lf {
                        assert_eq!(result, Err(LineEndingError::EmptyWithTrailingLf));
                    } else if !trailing_lf && lines.last().is_some_and(String::is_empty) {
                        assert_eq!(result, Err(LineEndingError::NonCanonicalFinalEmptyLine));
                    } else {
                        assert_eq!(split_lines(&result.unwrap()), b);
                    }
                }
            }
        }
    }
}
