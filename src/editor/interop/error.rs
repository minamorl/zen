//! Typed errors at the editor-core boundary.
//!
//! # Laws
//!
//! * **Bijection:** every value in [`ErrorCode::ALL`] has a unique string,
//!   and parsing that string returns the original value. The exhaustive
//!   one-to-one mapping below establishes both directions.
//! * **Strict rejection:** parsing accepts only the exact strings in that
//!   mapping. Unknown, empty, whitespace-modified, and case-modified strings
//!   cannot match any arm and are rejected.
//! * **Total preservation:** [`BoundaryError::new`] stores its code and the
//!   converted message without modification; its accessors return those fields.

use std::fmt;
use std::str::FromStr;

macro_rules! error_codes {
    ($($variant:ident => $code:literal),+ $(,)?) => {
        /// A stable, machine-readable editor-core error code.
        #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum ErrorCode {
            $(#[doc = concat!("The `", $code, "` error code.")]
            $variant,)+
        }

        impl ErrorCode {
            /// All error codes, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            /// Returns the stable lowercase snake_case code.
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $code,)+ }
            }

            /// Parses an exact stable code, rejecting every other string.
            pub fn parse_code(code: &str) -> Result<Self, UnknownErrorCode> {
                match code { $($code => Ok(Self::$variant),)+ _ => Err(UnknownErrorCode) }
            }
        }
    };
}

error_codes! {
    InvalidUtf8 => "invalid_utf8",
    InvalidBoundary => "invalid_boundary",
    InvalidUtf16Boundary => "invalid_utf16_boundary",
    OutOfRange => "out_of_range",
    InvalidSpan => "invalid_span",
    RevisionOverflow => "revision_overflow",
    RevisionMismatch => "revision_mismatch",
    BaseExpired => "base_expired",
    UnknownBuffer => "unknown_buffer",
    BufferClosed => "buffer_closed",
    DuplicateBufferId => "duplicate_buffer_id",
    UnknownActor => "unknown_actor",
    RevokedActor => "revoked_actor",
    Conflict => "conflict",
    TxnIdReuse => "txn_id_reuse",
    EmptyPattern => "empty_pattern",
    PermissionDenied => "permission_denied",
    StaleApproval => "stale_approval",
    UnsupportedOperation => "unsupported_operation",
    InvalidCoordinate => "invalid_coordinate",
    InvalidLineEndings => "invalid_line_endings",
    CorruptSequence => "corrupt_sequence",
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error returned when a string is not a recognized error code.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UnknownErrorCode;

impl fmt::Display for UnknownErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown error code")
    }
}

impl std::error::Error for UnknownErrorCode {}

impl FromStr for ErrorCode {
    type Err = UnknownErrorCode;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_code(s)
    }
}

/// A typed boundary error with a human-readable message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundaryError {
    code: ErrorCode,
    message: String,
}

impl BoundaryError {
    /// Stores the code and message without changing either value.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Returns the original code.
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// Returns the original message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for BoundaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for BoundaryError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts, Rng};
    use std::collections::HashSet;

    #[test]
    fn codes_are_exhaustive_unique_and_round_trip() {
        use ErrorCode::*;
        let expected = [
            (InvalidUtf8, "invalid_utf8"),
            (InvalidBoundary, "invalid_boundary"),
            (InvalidUtf16Boundary, "invalid_utf16_boundary"),
            (OutOfRange, "out_of_range"),
            (InvalidSpan, "invalid_span"),
            (RevisionOverflow, "revision_overflow"),
            (RevisionMismatch, "revision_mismatch"),
            (BaseExpired, "base_expired"),
            (UnknownBuffer, "unknown_buffer"),
            (BufferClosed, "buffer_closed"),
            (DuplicateBufferId, "duplicate_buffer_id"),
            (UnknownActor, "unknown_actor"),
            (RevokedActor, "revoked_actor"),
            (Conflict, "conflict"),
            (TxnIdReuse, "txn_id_reuse"),
            (EmptyPattern, "empty_pattern"),
            (PermissionDenied, "permission_denied"),
            (StaleApproval, "stale_approval"),
            (UnsupportedOperation, "unsupported_operation"),
            (InvalidCoordinate, "invalid_coordinate"),
            (InvalidLineEndings, "invalid_line_endings"),
            (CorruptSequence, "corrupt_sequence"),
        ];
        assert_eq!(ErrorCode::ALL, expected.map(|(code, _)| code));
        let mut strings = HashSet::new();
        for (code, string) in expected {
            assert!(strings.insert(string));
            assert_eq!(code.as_str(), string);
            assert_eq!(code.to_string(), string);
            assert_eq!(ErrorCode::parse_code(string), Ok(code));
            assert_eq!(string.parse::<ErrorCode>(), Ok(code));
            for invalid in [
                format!(" {string}"),
                format!("{string} "),
                string.to_uppercase(),
            ] {
                assert_eq!(ErrorCode::parse_code(&invalid), Err(UnknownErrorCode));
            }
        }
        for invalid in ["", " ", "unknown", "invalid-utf8", "\n"] {
            assert_eq!(ErrorCode::parse_code(invalid), Err(UnknownErrorCode));
        }
    }

    #[test]
    fn generated_messages_and_unknown_codes() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| {
                let messages = texts(rng);
                let code = *rng.choose(ErrorCode::ALL).unwrap();
                (code, messages)
            },
            |(code, messages)| {
                for message in messages {
                    let error = BoundaryError::new(*code, message.clone());
                    assert_eq!(error.code(), *code);
                    assert_eq!(error.message(), message);
                    assert_eq!(error.to_string(), format!("{code}: {message}"));
                    let unknown = format!("{code}{message}\0");
                    assert_eq!(ErrorCode::parse_code(&unknown), Err(UnknownErrorCode));
                }
            },
        );
    }
}
