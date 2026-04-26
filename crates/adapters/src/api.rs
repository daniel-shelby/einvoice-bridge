//! Inbound POS-facing HTTP API.
//!
//! Submit handler is durable to the DB only — no LHDN call on the request
//! path. A background worker picks the row up via `outbox_events`.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::repo::{CancelError, InvoiceRepo, InvoiceRow, RepoError};

#[derive(Clone)]
pub struct ApiState {
    pub repo: InvoiceRepo,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/invoices", post(submit_invoice))
        .route("/v1/invoices/{invoice_ref}", get(get_invoice))
        .route("/v1/invoices/{invoice_ref}/cancel", post(cancel_invoice))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Debug, thiserror::Error)]
enum ApiError {
    #[error("missing or empty `invoice_ref`")]
    MissingInvoiceRef,
    #[error("missing or empty `reason`")]
    MissingReason,
    #[error("invoice not found")]
    NotFound,
    #[error("invoice with this reference already exists")]
    Conflict,
    #[error("invoice cannot be cancelled in state {0}")]
    NotCancellable(String),
    #[error("invoice is past the LHDN cancellation window")]
    PastWindow,
    #[error("a cancellation request is already pending")]
    AlreadyRequested,
    #[error("internal error")]
    Internal,
}

impl From<RepoError> for ApiError {
    fn from(err: RepoError) -> Self {
        match err {
            RepoError::DuplicateRef => ApiError::Conflict,
            RepoError::Db(inner) => {
                tracing::error!(error = %inner, "database error");
                ApiError::Internal
            }
        }
    }
}

impl From<CancelError> for ApiError {
    fn from(err: CancelError) -> Self {
        match err {
            CancelError::NotFound => ApiError::NotFound,
            CancelError::NotCancellable { state } => ApiError::NotCancellable(state),
            CancelError::PastWindow => ApiError::PastWindow,
            CancelError::AlreadyRequested => ApiError::AlreadyRequested,
            CancelError::Db(inner) => {
                tracing::error!(error = %inner, "database error");
                ApiError::Internal
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self {
            ApiError::MissingInvoiceRef | ApiError::MissingReason => StatusCode::BAD_REQUEST,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::Conflict | ApiError::AlreadyRequested => StatusCode::CONFLICT,
            ApiError::NotCancellable(_) | ApiError::PastWindow => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

async fn submit_invoice(
    State(state): State<ApiState>,
    Json(payload): Json<Value>,
) -> Result<(StatusCode, Json<InvoiceRow>), ApiError> {
    let invoice_ref = payload
        .get("invoice_ref")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(ApiError::MissingInvoiceRef)?
        .to_owned();

    // Store the raw POS payload as-is. Canonicalisation happens when we
    // build the UBL document for signing — not at ingest.
    let payload_str = payload.to_string();

    let row = state
        .repo
        .create_pending(&invoice_ref, &payload_str)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(row)))
}

async fn get_invoice(
    State(state): State<ApiState>,
    Path(invoice_ref): Path<String>,
) -> Result<Json<InvoiceRow>, ApiError> {
    state
        .repo
        .find_by_ref(&invoice_ref)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

async fn cancel_invoice(
    State(state): State<ApiState>,
    Path(invoice_ref): Path<String>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<InvoiceRow>), ApiError> {
    let reason = body
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(ApiError::MissingReason)?
        .to_owned();

    let row = state
        .repo
        .request_cancellation(&invoice_ref, &reason)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(row)))
}
