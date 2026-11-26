//! Composed coordinates for one immutable UTF-8 text.
//!
//! LF separates lines, CR is content, and whole-text UTF-16 offsets count
//! separators. Surrogate interiors and UTF-8 scalar interiors are rejected.

use super::lines::{LineError, LineIndex, LinePosition};
use super::span::{ByteSpan, SpanError};
use super::utf16::{self, LineColUtf16, Utf16Error};
use super::utf8::{ByteOffset, Text, Utf8Error};

/// Coordinate conversions tied to the same text and line index.
#[derive(Clone, Debug)]
pub struct Boundaries {
    text: Text,
    lines: LineIndex,
}

impl Boundaries {
    /// Construct coordinates for `text`.
    pub fn new(text: Text) -> Self {
        let lines = LineIndex::from_text(&text);
        Self { text, lines }
    }

    /// Borrow the indexed text.
    pub fn text(&self) -> &Text {
        &self.text
    }

    /// Validate a byte offset.
    pub fn byte(&self, offset: usize) -> Result<ByteOffset, Utf8Error> {
        self.text.validate_offset(offset)
    }

    /// Validate a half-open byte span.
    pub fn span(&self, start: usize, end: usize) -> Result<ByteSpan, SpanError> {
        ByteSpan::validate_in(&self.text, start, end)
    }

    /// Convert a byte offset to a line and byte column.
    pub fn byte_to_line(&self, offset: usize) -> Result<LinePosition, LineError> {
        let offset = self.byte(offset).map_err(|e| match e {
            Utf8Error::NotCharBoundary { offset } => LineError::NotCharBoundary { offset },
            Utf8Error::OutOfBounds { offset, len } => LineError::OutOfBounds { offset, len },
            _ => unreachable!("offset validation only returns offset errors"),
        })?;
        self.lines.offset_to_line_col(&self.text, offset)
    }

    /// Convert a line and byte column to a byte offset.
    pub fn line_to_byte(&self, pos: LinePosition) -> Result<ByteOffset, LineError> {
        self.lines.line_col_to_offset(&self.text, pos)
    }

    /// Convert a byte offset to a whole-text UTF-16 offset.
    pub fn byte_to_utf16(&self, offset: usize) -> Result<usize, Utf8Error> {
        let offset = self.byte(offset)?.as_usize();
        Ok(self.text.as_str()[..offset].encode_utf16().count())
    }

    /// Convert a whole-text UTF-16 offset to a byte offset.
    pub fn utf16_to_byte(&self, offset: usize) -> Result<ByteOffset, Utf16Error> {
        let byte = utf16::utf16_to_byte(self.text.as_str(), offset)?;
        Ok(self
            .byte(byte)
            .expect("UTF-16 conversion returns a boundary"))
    }

    /// Convert a byte offset to a line and UTF-16 column.
    pub fn byte_to_line_utf16(&self, offset: usize) -> Result<LineColUtf16, Utf16Error> {
        let pos = self.byte_to_line(offset).map_err(Utf16Error::Line)?;
        self.line_to_line_utf16(pos)
    }

    /// Convert a line and UTF-16 column to a byte offset.
    pub fn line_utf16_to_byte(&self, pos: LineColUtf16) -> Result<ByteOffset, Utf16Error> {
        utf16::line_col_to_offset(&self.lines, &self.text, pos)
    }

    /// Convert a line and byte column to a line and UTF-16 column.
    pub fn line_to_line_utf16(&self, pos: LinePosition) -> Result<LineColUtf16, Utf16Error> {
        let byte = self.line_to_byte(pos).map_err(Utf16Error::Line)?;
        utf16::offset_to_line_col(&self.lines, &self.text, byte)
    }

    /// Convert a line and UTF-16 column to a line and byte column.
    pub fn line_utf16_to_line(&self, pos: LineColUtf16) -> Result<LinePosition, Utf16Error> {
        let byte = self.line_utf16_to_byte(pos)?;
        self.lines
            .offset_to_line_col(&self.text, byte)
            .map_err(Utf16Error::Line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts, Rng};
    use crate::editor::text::line_endings::{join_lines, split_lines};
    use crate::editor::text::payload::{decode, encode, PayloadError, PayloadMode};

    // Each entry is a scalar boundary and its UTF-16 position. A missing
    // interior position between a surrogate pair is therefore detectable.
    fn coordinates(source: &str) -> Vec<(usize, usize)> {
        let mut result = vec![(0, 0)];
        let mut units = 0;
        for (byte, ch) in source.char_indices() {
            units += if (ch as u32) > 0xffff { 2 } else { 1 };
            result.push((byte + ch.len_utf8(), units));
        }
        result
    }

    fn unit_to_byte(source: &str, unit: usize) -> Result<usize, Utf16Error> {
        let coords = coordinates(source);
        let total = coords.last().unwrap().1;
        if let Some(&(byte, _)) = coords.iter().find(|&&(_, u)| u == unit) {
            Ok(byte)
        } else if unit > total {
            Err(Utf16Error::Utf16ColumnOutOfRange {
                col_utf16: unit,
                line_len: total,
            })
        } else {
            // Only a high/low surrogate pair can leave an interior unit.
            assert!(coords
                .windows(2)
                .any(|pair| pair[0].1 + 1 == unit && pair[1].1 == unit + 1));
            Err(Utf16Error::SurrogateInterior { col_utf16: unit })
        }
    }

    fn offsets(len: usize) -> Vec<usize> {
        (0..=len).chain([len + 1, len + 2, usize::MAX]).collect()
    }

    fn check(source: &str) {
        let text = Text::from_str(source);
        let map = Boundaries::new(text.clone());
        let coords = coordinates(source);
        let boundaries: Vec<_> = coords.iter().map(|&(byte, _)| byte).collect();
        let mut lines = Vec::new();
        let mut start = 0;
        for (end, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                lines.push((start, end));
                start = end + 1;
            }
        }
        lines.push((start, source.len()));
        let trailing_lf = source.as_bytes().last() == Some(&b'\n');
        let split = split_lines(&text);
        let split_count = if source.is_empty() || trailing_lf {
            lines.len() - 1
        } else {
            lines.len()
        };
        let expected_lines: Vec<_> = lines[..split_count]
            .iter()
            .map(|&(s, e)| &source[s..e])
            .collect();
        assert_eq!(
            split.lines.iter().map(String::as_str).collect::<Vec<_>>(),
            expected_lines
        );
        assert_eq!(split.trailing_lf, trailing_lf);
        for byte in offsets(source.len()) {
            if boundaries.contains(&byte) {
                let offset = map.byte(byte).unwrap();
                let line_no = lines
                    .iter()
                    .position(|&(s, e)| s <= byte && byte <= e)
                    .unwrap();
                let start = lines[line_no].0;
                let line = LinePosition {
                    line: line_no,
                    col_byte: byte - start,
                };
                let line16 = LineColUtf16 {
                    line: line_no,
                    col_utf16: coordinates(&source[start..byte]).last().unwrap().1,
                };
                let unit = map.byte_to_utf16(byte).unwrap();
                assert_eq!(unit, coords.iter().find(|&&(b, _)| b == byte).unwrap().1);
                assert_eq!(map.byte_to_line(byte), Ok(line));
                assert_eq!(map.byte_to_line_utf16(byte), Ok(line16));
                assert_eq!(map.line_to_byte(line), Ok(offset));
                assert_eq!(map.utf16_to_byte(unit), Ok(offset));
                assert_eq!(map.line_utf16_to_byte(line16), Ok(offset));
                assert_eq!(map.line_to_line_utf16(line), Ok(line16));
                assert_eq!(map.line_utf16_to_line(line16), Ok(line));
            } else {
                let error = if byte > source.len() {
                    Utf8Error::OutOfBounds {
                        offset: byte,
                        len: source.len(),
                    }
                } else {
                    Utf8Error::NotCharBoundary { offset: byte }
                };
                assert_eq!(map.byte(byte), Err(error.clone()));
                assert_eq!(map.byte_to_utf16(byte), Err(error));
                let error = if byte > source.len() {
                    LineError::OutOfBounds {
                        offset: byte,
                        len: source.len(),
                    }
                } else {
                    LineError::NotCharBoundary { offset: byte }
                };
                assert_eq!(map.byte_to_line(byte), Err(error));
                assert_eq!(map.byte_to_line_utf16(byte), Err(Utf16Error::Line(error)));
            }
        }
        let total = coords.last().unwrap().1;
        for unit in offsets(total) {
            assert_eq!(
                map.utf16_to_byte(unit).map(|b| b.as_usize()),
                unit_to_byte(source, unit)
            );
        }
        for (line, &(start, end)) in lines.iter().enumerate() {
            let content = &source[start..end];
            for col in offsets(content.len()) {
                let pos = LinePosition {
                    line,
                    col_byte: col,
                };
                let result = map.line_to_byte(pos);
                if col > content.len() {
                    assert_eq!(
                        result,
                        Err(LineError::ColumnOutOfRange {
                            line,
                            col_byte: col,
                            line_len: content.len()
                        })
                    );
                } else if !boundaries.contains(&(start + col)) {
                    assert_eq!(
                        result,
                        Err(LineError::NotCharBoundary {
                            offset: start + col
                        })
                    );
                } else {
                    assert_eq!(result.unwrap().as_usize(), start + col);
                }
                if let Err(error) = result {
                    assert_eq!(map.line_to_line_utf16(pos), Err(Utf16Error::Line(error)));
                }
            }
            let len16 = coordinates(content).last().unwrap().1;
            for col in offsets(len16) {
                let pos = LineColUtf16 {
                    line,
                    col_utf16: col,
                };
                match unit_to_byte(content, col) {
                    Ok(byte) => assert_eq!(
                        map.line_utf16_to_byte(pos).unwrap().as_usize(),
                        start + byte
                    ),
                    Err(error) => {
                        assert_eq!(map.line_utf16_to_byte(pos), Err(error));
                        assert_eq!(map.line_utf16_to_line(pos), Err(error));
                    }
                }
            }
        }
        let count = lines.len();
        for line in [count, count + 1, usize::MAX] {
            let err = LineError::LineOutOfRange {
                line,
                line_count: count,
            };
            for col in [0, 1, usize::MAX] {
                let invalid = LinePosition {
                    line,
                    col_byte: col,
                };
                assert_eq!(map.line_to_byte(invalid), Err(err));
                assert_eq!(map.line_to_line_utf16(invalid), Err(Utf16Error::Line(err)));
                let invalid = LineColUtf16 {
                    line,
                    col_utf16: col,
                };
                assert_eq!(map.line_utf16_to_byte(invalid), Err(Utf16Error::Line(err)));
                assert_eq!(map.line_utf16_to_line(invalid), Err(Utf16Error::Line(err)));
            }
        }
        let endpoint_error = |offset| {
            if offset > source.len() {
                Some(SpanError::OutOfBounds {
                    offset,
                    len: source.len(),
                })
            } else if !boundaries.contains(&offset) {
                Some(SpanError::NotCharBoundary { offset })
            } else {
                None
            }
        };
        for start in offsets(source.len()) {
            for end in offsets(source.len()) {
                let expected = endpoint_error(start)
                    .or_else(|| endpoint_error(end))
                    .or_else(|| (start > end).then_some(SpanError::InvertedRange { start, end }));
                if let Some(error) = expected {
                    assert_eq!(map.span(start, end), Err(error));
                } else {
                    let span = map.span(start, end).unwrap();
                    assert_eq!(
                        (span.start().as_usize(), span.end().as_usize()),
                        (start, end)
                    );
                    assert_eq!(span.slice_in(&text), Ok(&source[start..end]));
                }
            }
        }
        let mut restored = vec![join_lines(&split).unwrap()];
        assert_eq!(
            encode(&text, PayloadMode::Verbatim),
            Ok(source.as_bytes().to_vec())
        );
        restored.push(decode(source.as_bytes(), PayloadMode::Verbatim).unwrap());
        if let Some(offset) = source.bytes().position(|b| b == b'\r') {
            for mode in [PayloadMode::Lf, PayloadMode::CrLf] {
                assert_eq!(
                    encode(&text, mode),
                    Err(PayloadError::UnexpectedCr { offset })
                );
            }
        } else {
            let mut crlf = Vec::new();
            for &byte in source.as_bytes() {
                if byte == b'\n' {
                    crlf.push(b'\r');
                }
                crlf.push(byte);
            }
            assert_eq!(
                encode(&text, PayloadMode::Lf),
                Ok(source.as_bytes().to_vec())
            );
            assert_eq!(encode(&text, PayloadMode::CrLf), Ok(crlf.clone()));
            restored.push(decode(source.as_bytes(), PayloadMode::Lf).unwrap());
            restored.push(decode(&crlf, PayloadMode::CrLf).unwrap());
            let wire = Boundaries::new(Text::from_str(std::str::from_utf8(&crlf).unwrap()));
            for &byte in &boundaries {
                let shift = source.as_bytes()[..byte]
                    .iter()
                    .filter(|&&b| b == b'\n')
                    .count();
                let shifted = byte + shift;
                assert_eq!(wire.byte(shifted).unwrap().as_usize(), shifted);
                assert_eq!(
                    wire.byte_to_utf16(shifted),
                    Ok(coords.iter().find(|&&(b, _)| b == byte).unwrap().1 + shift)
                );
                assert_eq!(wire.byte_to_line(shifted), map.byte_to_line(byte));
            }
        }
        for copy in restored {
            assert_eq!(copy, text);
            let other = Boundaries::new(copy);
            for &byte in &boundaries {
                assert_eq!(other.byte_to_line(byte), map.byte_to_line(byte));
                assert_eq!(other.byte_to_utf16(byte), map.byte_to_utf16(byte));
                assert_eq!(other.byte_to_line_utf16(byte), map.byte_to_line_utf16(byte));
            }
        }
    }

    #[test]
    fn exhaustive_fixtures() {
        for source in [
            "",
            "\n",
            "\n\n",
            "ascii",
            "a\n",
            "a\r\nb",
            "é界😀e\u{301}\n次😀",
            "é界😀\r\n",
        ] {
            check(source);
        }
    }

    #[test]
    fn generated_coordinate_laws() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| texts(rng),
            |corpus| {
                for source in corpus {
                    check(source);
                }
            },
        );
    }

    #[test]
    fn payload_rejections() {
        for mode in [PayloadMode::Verbatim, PayloadMode::Lf, PayloadMode::CrLf] {
            for bytes in [&[0xff][..], &[0xe3, 0x81], &[0xed, 0xa0, 0x80]] {
                assert_eq!(decode(bytes, mode), Err(PayloadError::InvalidUtf8));
            }
        }
        assert_eq!(
            decode(b"a\rb", PayloadMode::Lf),
            Err(PayloadError::UnexpectedCr { offset: 1 })
        );
        assert_eq!(
            decode(b"a\rb", PayloadMode::CrLf),
            Err(PayloadError::UnexpectedCr { offset: 1 })
        );
        assert_eq!(
            decode(b"a\nb", PayloadMode::CrLf),
            Err(PayloadError::UnexpectedLf { offset: 1 })
        );
    }
}
