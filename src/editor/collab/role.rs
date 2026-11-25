//! Collaboration participant roles.
//!
//! # Laws
//!
//! * **Exhaustive classification:** every role occurs in [`ActorRole::ALL`],
//!   and its string and byte representations are distinct. The enum has only
//!   two variants, both listed in `ALL`; their representations are respectively
//!   `human`/`0` and `agent`/`1`.
//! * **Lossless roundtrip:** parsing a role's string or byte representation
//!   returns that role. Each encoder and decoder uses the same two pairs.
//! * **Strict rejection:** all other strings, including case mismatches, and
//!   all byte codes from 2 through 255 return [`UnknownRole`]. The decoders
//!   accept only the two exact pairs listed above.

use std::fmt;
use std::str::FromStr;

/// Classification of a collaboration participant.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActorRole {
    /// A person participating in the collaboration.
    Human,
    /// An agent participating in the collaboration.
    Agent,
}

/// An unrecognized role representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownRole;

impl fmt::Display for UnknownRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown actor role")
    }
}

impl std::error::Error for UnknownRole {}

impl ActorRole {
    /// All role variants, in their wire-code order.
    pub const ALL: [Self; 2] = [Self::Human, Self::Agent];

    /// Returns the exact string representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
        }
    }

    /// Parses an exact, case-sensitive string representation.
    pub fn parse_code(value: &str) -> Result<Self, UnknownRole> {
        match value {
            "human" => Ok(Self::Human),
            "agent" => Ok(Self::Agent),
            _ => Err(UnknownRole),
        }
    }

    /// Returns the byte representation.
    pub const fn code(self) -> u8 {
        match self {
            Self::Human => 0,
            Self::Agent => 1,
        }
    }

    /// Parses a byte representation.
    pub const fn from_code(value: u8) -> Result<Self, UnknownRole> {
        match value {
            0 => Ok(Self::Human),
            1 => Ok(Self::Agent),
            _ => Err(UnknownRole),
        }
    }

    /// Returns whether this is a human role.
    pub const fn is_human(self) -> bool {
        matches!(self, Self::Human)
    }

    /// Returns whether this is an agent role.
    pub const fn is_agent(self) -> bool {
        matches!(self, Self::Agent)
    }
}

impl fmt::Display for ActorRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ActorRole {
    type Err = UnknownRole;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse_code(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};
    use std::collections::HashSet;

    #[test]
    fn all_roles_have_distinct_representations_and_roundtrip() {
        assert_eq!(ActorRole::ALL, [ActorRole::Human, ActorRole::Agent]);
        let mut strings = HashSet::new();
        let mut codes = HashSet::new();
        for (role, string, code) in [
            (ActorRole::Human, "human", 0),
            (ActorRole::Agent, "agent", 1),
        ] {
            assert!(ActorRole::ALL.contains(&role));
            assert_eq!(role.as_str(), string);
            assert_eq!(role.to_string(), string);
            assert_eq!(role.code(), code);
            assert!(strings.insert(role.as_str()));
            assert!(codes.insert(role.code()));
            assert_eq!(ActorRole::parse_code(role.as_str()), Ok(role));
            assert_eq!(role.as_str().parse::<ActorRole>(), Ok(role));
            assert_eq!(ActorRole::from_code(role.code()), Ok(role));
            assert_eq!(role.is_human(), role == ActorRole::Human);
            assert_eq!(role.is_agent(), role == ActorRole::Agent);
        }
    }

    #[test]
    fn every_byte_is_classified_strictly() {
        for code in u8::MIN..=u8::MAX {
            let expected = match code {
                0 => Ok(ActorRole::Human),
                1 => Ok(ActorRole::Agent),
                _ => Err(UnknownRole),
            };
            assert_eq!(ActorRole::from_code(code), expected, "code: {code}");
        }
    }

    #[test]
    fn string_parsing_accepts_only_exact_codes() {
        for value in [
            "", "Human", "HUMAN", "Agent", "AGENT", " human", "agent ", "0", "1", "human\0",
            "unknown",
        ] {
            assert_eq!(ActorRole::parse_code(value), Err(UnknownRole));
            assert_eq!(value.parse::<ActorRole>(), Err(UnknownRole));
        }
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for value in corpus {
                let expected = match value.as_str() {
                    "human" => Ok(ActorRole::Human),
                    "agent" => Ok(ActorRole::Agent),
                    _ => Err(UnknownRole),
                };
                assert_eq!(ActorRole::parse_code(value), expected);
                assert_eq!(value.parse::<ActorRole>(), expected);
            }
        });
    }

    #[test]
    fn error_implements_error_and_display() {
        fn assert_error<T: std::error::Error>() {}
        assert_error::<UnknownRole>();
        assert_eq!(UnknownRole.to_string(), "unknown actor role");
    }
}
