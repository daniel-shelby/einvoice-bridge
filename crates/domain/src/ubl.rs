//! UBL 2.1 invoice document construction and signed-properties embedding.
//!
//! Stub — real implementation lands with the signing milestone.

use crate::DomainError;

pub struct UblInvoice;

impl UblInvoice {
    pub fn from_pos_payload(_payload: &serde_json::Value) -> Result<Self, DomainError> {
        Err(DomainError::InvalidInvoice("not yet implemented".into()))
    }
}
