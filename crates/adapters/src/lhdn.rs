//! LHDN MyInvois HTTP client.
//!
//! Submodules:
//! - `error` — typed `LhdnError` with `is_transient()` for retry policy.
//! - `models` — request/response shapes (camelCase wire format).
//! - `oauth` — token cache types + the 60s refresh leeway.
//! - `token_repo` — SQLite-backed token persistence so we don't re-auth on
//!   every restart.
//! - `client` — `LhdnClient` (the public surface).

pub mod client;
pub mod error;
pub mod models;
pub mod oauth;
pub mod token_repo;

pub use client::{LhdnClient, LhdnConfig};
pub use error::LhdnError;
pub use models::{
    AcceptedDocument, DocumentDetails, DocumentStatus, IdScheme, LhdnErrorEnvelope,
    RejectedDocument, SubmissionDocument, SubmissionFormat, SubmissionResponse,
};
pub use oauth::CachedToken;
pub use token_repo::OauthTokenStore;

/// Which LHDN environment to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LhdnEnv {
    Preprod,
    Prod,
}

impl LhdnEnv {
    pub fn base_url(self) -> &'static str {
        match self {
            LhdnEnv::Preprod => "https://preprod-api.myinvois.hasil.gov.my",
            LhdnEnv::Prod => "https://api.myinvois.hasil.gov.my",
        }
    }

    /// Public-facing MyInvois portal — used to build the QR share URL once
    /// LHDN has validated a document and returned a `longId`.
    pub fn portal_base_url(self) -> &'static str {
        match self {
            LhdnEnv::Preprod => "https://preprod.myinvois.hasil.gov.my",
            LhdnEnv::Prod => "https://myinvois.hasil.gov.my",
        }
    }

    /// String key used to scope cached tokens in the `oauth_tokens` table.
    pub fn name(self) -> &'static str {
        match self {
            LhdnEnv::Preprod => "preprod",
            LhdnEnv::Prod => "prod",
        }
    }
}
