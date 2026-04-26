//! Wire-format request/response types for MyInvois.
//!
//! All fields use camelCase on the wire. Field shapes track the JSON
//! variant of the LHDN OpenAPI spec — when we hit the real preprod sandbox
//! we'll likely need to extend these (validation results, more status
//! reason fields, etc.). Keep the structs minimal and add fields as we
//! observe them, rather than guessing.

use serde::{Deserialize, Serialize};

/// What format the embedded document is in. UBL JSON or UBL XML.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum SubmissionFormat {
    Json,
    Xml,
}

/// One document within a submission batch. `document` carries the
/// base64-encoded UBL bytes (the signed document). `document_hash` is
/// base64(SHA-256(document_bytes)).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionDocument {
    pub format: SubmissionFormat,
    pub document_hash: String,
    pub code_number: String,
    pub document: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubmissionRequest<'a> {
    pub documents: &'a [SubmissionDocument],
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionResponse {
    pub submission_uid: String,
    #[serde(default)]
    pub accepted_documents: Vec<AcceptedDocument>,
    #[serde(default)]
    pub rejected_documents: Vec<RejectedDocument>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedDocument {
    pub uuid: String,
    pub invoice_code_number: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectedDocument {
    pub invoice_code_number: String,
    pub error: LhdnErrorEnvelope,
}

/// LHDN's standard error body. `details` keeps `serde_json::Value` so we
/// don't lose information when LHDN nests structured detail objects.
#[derive(Debug, Clone, Deserialize)]
pub struct LhdnErrorEnvelope {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub details: Vec<serde_json::Value>,
}

/// Wrapper for a 4xx response body of shape `{"error": {...}}`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LhdnErrorResponse {
    pub error: LhdnErrorEnvelope,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum DocumentStatus {
    Submitted,
    Valid,
    Invalid,
    Cancelled,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentDetails {
    pub uuid: String,
    pub long_id: Option<String>,
    pub internal_id: String,
    pub status: DocumentStatus,
    pub date_time_received: Option<String>,
    pub date_time_validated: Option<String>,
    pub cancel_date_time: Option<String>,
    pub document_status_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CancelRequest<'a> {
    pub status: &'a str,
    pub reason: &'a str,
}

// OAuth2 token endpoint follows RFC 6749 — snake_case fields, NOT camelCase.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    pub expires_in: i64,
    /// Always asserted to equal `"Bearer"` in `LhdnClient::fetch_token`.
    pub token_type: String,
}

/// Secondary identifier scheme for taxpayer validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdScheme {
    Nric,
    Passport,
    Brn,
    Army,
}

impl IdScheme {
    /// LHDN-recognised wire value used in `idType` query params.
    pub fn as_str(self) -> &'static str {
        match self {
            IdScheme::Nric => "NRIC",
            IdScheme::Passport => "PASSPORT",
            IdScheme::Brn => "BRN",
            IdScheme::Army => "ARMY",
        }
    }
}
