//! Pure domain layer for einvoice-bridge.
//!
//! No IO, no async. Everything here is deterministic and unit-testable
//! from JSON/XML fixtures.

// `serde_json::json!` recursion when building the deeply-nested UBL
// extension/signature tree exceeds the default 128.
#![recursion_limit = "256"]

pub mod canonicalize;
pub mod digest;
pub mod signer;
pub mod ubl;

pub use signer::Signer;
pub use ubl::{SignedDocument, build_signed_document};

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("canonicalisation failed: {0}")]
    Canonicalize(String),
    #[error("signing failed: {0}")]
    Sign(String),
    #[error("invalid invoice payload: {0}")]
    InvalidInvoice(String),
}
