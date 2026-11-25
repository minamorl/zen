//! UTF-8 payloads with verbatim or strict explicit line endings.
//!
//! Explicit modes use LF-only canonical text. CRLF decoding removes only CRs
//! immediately preceding LF; encoding inserts exactly those CRs. Consequently
//! both canonical text and valid explicit streams roundtrip without loss.

use super::utf8::Text;
use std::fmt;

/// The line-ending policy for a payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadMode {
    /// Preserve all characters, including CR and LF, without interpretation.
    Verbatim,
    /// Require LF separators and no CR characters.
    Lf,
    /// Require CRLF separators and no unpaired CR or LF characters.
    CrLf,
}

/// An invalid UTF-8 payload or unexpected line-ending byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadError {
    /// Input bytes are not valid UTF-8.
    InvalidUtf8,
    /// A CR is not permitted at this byte offset.
    UnexpectedCr { offset: usize },
    /// An LF is not paired with a preceding CR at this byte offset.
    UnexpectedLf { offset: usize },
}

impl fmt::Display for PayloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 => write!(f, "invalid UTF-8"),
            Self::UnexpectedCr { offset } => write!(f, "unexpected CR at byte {offset}"),
            Self::UnexpectedLf { offset } => write!(f, "unexpected LF at byte {offset}"),
        }
    }
}

impl std::error::Error for PayloadError {}

/// Decodes UTF-8 bytes according to the selected line-ending policy.
pub fn decode(bytes: &[u8], mode: PayloadMode) -> Result<Text, PayloadError> {
    let source = std::str::from_utf8(bytes).map_err(|_| PayloadError::InvalidUtf8)?;
    match mode {
        PayloadMode::Verbatim => Ok(Text::from_str(source)),
        PayloadMode::Lf => {
            validate_canonical(source)?;
            Ok(Text::from_str(source))
        }
        PayloadMode::CrLf => {
            let mut chars = source.char_indices().peekable();
            let mut result = String::with_capacity(source.len());
            while let Some((offset, ch)) = chars.next() {
                match ch {
                    '\r' => {
                        if chars.peek().is_some_and(|&(_, next)| next == '\n') {
                            chars.next();
                            result.push('\n');
                        } else {
                            return Err(PayloadError::UnexpectedCr { offset });
                        }
                    }
                    '\n' => return Err(PayloadError::UnexpectedLf { offset }),
                    _ => result.push(ch),
                }
            }
            Ok(Text::from_str(&result))
        }
    }
}

/// Encodes text; explicit modes reject CR in the canonical input.
pub fn encode(text: &Text, mode: PayloadMode) -> Result<Vec<u8>, PayloadError> {
    let source = text.as_str();
    if mode != PayloadMode::Verbatim {
        validate_canonical(source)?;
    }
    match mode {
        PayloadMode::Verbatim | PayloadMode::Lf => Ok(source.as_bytes().to_vec()),
        PayloadMode::CrLf => Ok(source.replace('\n', "\r\n").into_bytes()),
    }
}

fn validate_canonical(source: &str) -> Result<(), PayloadError> {
    if let Some(offset) = source.find('\r') {
        return Err(PayloadError::UnexpectedCr { offset });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    #[test]
    fn verbatim_roundtrip_law() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let text = decode(source.as_bytes(), PayloadMode::Verbatim).unwrap();
                assert_eq!(
                    encode(&text, PayloadMode::Verbatim).unwrap(),
                    source.as_bytes()
                );
            }
        });
    }

    #[test]
    fn explicit_roundtrip_laws() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let canonical = source.replace('\r', "");
                let text = Text::from_str(&canonical);
                for mode in [PayloadMode::Lf, PayloadMode::CrLf] {
                    let wire = match mode {
                        PayloadMode::Lf => canonical.clone(),
                        PayloadMode::CrLf => canonical.replace('\n', "\r\n"),
                        PayloadMode::Verbatim => unreachable!(),
                    };
                    let decoded = decode(wire.as_bytes(), mode).unwrap();
                    assert_eq!(decoded, text);
                    assert_eq!(encode(&decoded, mode).unwrap(), wire.as_bytes());
                    let bytes = encode(&text, mode).unwrap();
                    assert_eq!(decode(&bytes, mode), Ok(text.clone()));
                    assert_eq!(encode(&decode(&bytes, mode).unwrap(), mode), Ok(bytes));
                }
            }
        });
        for canonical in ["", "\n", "\n\n", "a\n", "界\n😀", "界\n\n😀\n"] {
            let text = Text::from_str(canonical);
            let crlf = canonical.replace('\n', "\r\n");
            assert_eq!(encode(&text, PayloadMode::CrLf).unwrap(), crlf.as_bytes());
            assert_eq!(decode(crlf.as_bytes(), PayloadMode::CrLf), Ok(text));
        }
    }

    #[test]
    fn rejects_invalid_utf8_and_line_endings() {
        for mode in [PayloadMode::Verbatim, PayloadMode::Lf, PayloadMode::CrLf] {
            for bytes in [&[0xff][..], &[0xe3, 0x81], &[0xc0, 0x80]] {
                assert_eq!(decode(bytes, mode), Err(PayloadError::InvalidUtf8));
            }
        }
        for (source, expected) in [
            ("\r", PayloadError::UnexpectedCr { offset: 0 }),
            ("界\rX", PayloadError::UnexpectedCr { offset: 3 }),
            ("\n", PayloadError::UnexpectedLf { offset: 0 }),
            ("界\r\n\n", PayloadError::UnexpectedLf { offset: 5 }),
            ("\r\n\r", PayloadError::UnexpectedCr { offset: 2 }),
        ] {
            assert_eq!(decode(source.as_bytes(), PayloadMode::CrLf), Err(expected));
        }
        for source in ["\r", "a\r\n", "a\nb\r\nc"] {
            let error = PayloadError::UnexpectedCr {
                offset: source.find('\r').unwrap(),
            };
            assert_eq!(decode(source.as_bytes(), PayloadMode::Lf), Err(error));
            for mode in [PayloadMode::Lf, PayloadMode::CrLf] {
                assert_eq!(encode(&Text::from_str(source), mode), Err(error));
            }
        }
    }
}
