//! LHDN error type.
//!
//! Distinct variants exist for the buckets a retry policy actually cares
//! about: auth (don't retry, fix credentials), bad-request (don't retry,
//! the document is wrong), conflict (don't retry, terminal state),
//! rate-limited / 5xx / transport (retry with backoff).

use std::time::Duration;

use super::models::LhdnErrorEnvelope;

#[derive(Debug, thiserror::Error)]
pub enum LhdnError {
    /// 401 — credentials rejected, or token expired between cache check and use.
    #[error("auth: {0}")]
    Auth(String),

    /// 400 / 422 — LHDN parsed the request and rejected it on validation.
    /// The envelope is preserved so the worker can persist the reason.
    #[error("bad request: {}", .0.message)]
    BadRequest(LhdnErrorEnvelope),

    /// 404 — document or taxpayer not found.
    #[error("not found")]
    NotFound,

    /// 409 — terminal state conflict (e.g. trying to cancel a doc that's
    /// already cancelled, or past the cancellation window).
    #[error("conflict: {}", .0.message)]
    Conflict(LhdnErrorEnvelope),

    /// 429 — back off and retry. The optional hint comes from the
    /// `Retry-After` header.
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },

    /// 5xx — server problem; safe to retry.
    #[error("server error: status={status}")]
    Server { status: u16, body: String },

    /// Network / TLS / DNS — retry candidate.
    #[error("transport: {0}")]
    Transport(String),

    /// Response had unexpected shape.
    #[error("response schema: {0}")]
    Schema(String),

    /// Local DB problem (token cache). Preserves the typed source for
    /// chained logging via `tracing`'s error formatter.
    #[error("storage: {0}")]
    Storage(#[from] sqlx::Error),

    /// Misconfiguration discovered at runtime (e.g. invalid base URL).
    #[error("config: {0}")]
    Config(String),
}

impl LhdnError {
    /// Whether the worker should retry the call later. Auth/BadRequest
    /// /Conflict/NotFound are *not* transient — fix credentials or the
    /// document, don't bang the retry button.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            LhdnError::RateLimited { .. }
                | LhdnError::Server { .. }
                | LhdnError::Transport(_)
                | LhdnError::Storage(_)
        )
    }
}

impl From<reqwest::Error> for LhdnError {
    fn from(err: reqwest::Error) -> Self {
        LhdnError::Transport(err.to_string())
    }
}

impl From<serde_json::Error> for LhdnError {
    fn from(err: serde_json::Error) -> Self {
        LhdnError::Schema(err.to_string())
    }
}
