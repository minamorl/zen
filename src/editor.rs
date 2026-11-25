//! Editor core subsystems.

pub mod collab;
pub mod edit;
pub mod interop;
pub mod text;
pub mod workspace;

#[cfg(test)]
pub(crate) mod test_support;
