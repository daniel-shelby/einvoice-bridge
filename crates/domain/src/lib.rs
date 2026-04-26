//! Pure domain layer for einvoice-bridge.
//!
//! No IO, no async. Everything here is deterministic and unit-testable
//! from JSON/XML fixtures.

pub mod canonicalize;
pub mod digest;
pub mod signer;
pub mod ubl;

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("canonicalisation failed: {0}")]
    Canonicalize(String),
    #[error("signing failed: {0}")]
    Sign(String),
    #[error("invalid invoice payload: {0}")]
    InvalidInvoice(String),
}
