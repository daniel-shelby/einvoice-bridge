//! Submitter worker.
//!
//! Polls `outbox_events` for due `submit` rows. For each: load the
//! invoice, build + sign the UBL document, post it to LHDN, and update
//! the row. Transient errors get rescheduled with exponential backoff;
//! non-transient errors and exhausted retries terminate the row in
//! `Failed` state.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use einvoice_domain::{Signer, SignedDocument, build_signed_document};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::watch;
use tracing::{error, info, instrument, warn};

use crate::lhdn::{
    LhdnClient, LhdnError, SubmissionDocument, SubmissionFormat, SubmissionResponse,
};
use crate::repo::{DueSubmitEvent, InvoiceForSubmit, InvoiceRepo, SubmissionCompletion};

/// Worker tunables.
#[derive(Debug, Clone)]
pub struct SubmitterConfig {
    /// How long to sleep when there's no work to do.
    pub poll_interval: Duration,
    /// Max events handled per tick. SQLite serializes writers, so keep
    /// this small enough that one tick doesn't starve the API.
    pub batch_size: i64,
    /// Hard ceiling on retry attempts before giving up.
    pub max_attempts: i64,
}

impl Default for SubmitterConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            batch_size: 16,
            max_attempts: 8,
        }
    }
}

pub struct Submitter {
    repo: InvoiceRepo,
    lhdn: LhdnClient,
    signer: Arc<Signer>,
    config: SubmitterConfig,
}

impl Submitter {
    pub fn new(repo: InvoiceRepo, lhdn: LhdnClient, signer: Arc<Signer>) -> Self {
        Self {
            repo,
            lhdn,
            signer,
            config: SubmitterConfig::default(),
        }
    }

    pub fn with_config(mut self, config: SubmitterConfig) -> Self {
        self.config = config;
        self
    }

    /// Run the polling loop until `shutdown` flips to `true`.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        info!(
            poll_interval_secs = self.config.poll_interval.as_secs(),
            batch_size = self.config.batch_size,
            max_attempts = self.config.max_attempts,
            "submitter started"
        );
        loop {
            // If we processed a full batch, immediately try again — there
            // are likely more events ready. Otherwise sleep until the
            // next poll, with shutdown awareness.
            let processed = match self.tick().await {
                Ok(n) => n,
                Err(err) => {
                    error!(error = %err, "submitter tick failed");
                    0
                }
            };
            if processed > 0 {
                continue;
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        info!("submitter shutting down");
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }
    }

    /// One pass over due outbox events. Returns the count handled.
    pub async fn tick(&self) -> anyhow::Result<usize> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let due = self
            .repo
            .due_submit_events(now, self.config.batch_size)
            .await?;
        let count = due.len();
        for event in due {
            // Per-event errors are logged but don't abort the tick — one
            // bad row shouldn't take the worker down.
            if let Err(err) = self.process_event(&event).await {
                error!(
                    invoice_id = %event.invoice_id,
                    outbox_id = event.outbox_id,
                    error = %err,
                    "event handler errored"
                );
            }
        }
        Ok(count)
    }

    #[instrument(skip(self), fields(invoice_id = %event.invoice_id, outbox_id = event.outbox_id))]
    async fn process_event(&self, event: &DueSubmitEvent) -> anyhow::Result<()> {
        let invoice = match self.repo.load_for_submit(&event.invoice_id).await? {
            Some(i) => i,
            None => {
                // Defensive — schema has FK ON DELETE CASCADE, so an outbox
                // event without its invoice should be impossible. If we ever
                // see it, the UPDATE in fail_permanently is a no-op (no row
                // matches); only the DELETE on the outbox event runs.
                warn!("invoice row missing for outbox event; cleaning up orphan event");
                let body = json!({ "error": "invoice row missing" }).to_string();
                self.repo
                    .fail_permanently(&event.invoice_id, event.outbox_id, 0, &body)
                    .await?;
                return Ok(());
            }
        };

        let signed = match self.build_doc(&invoice) {
            Ok(d) => d,
            Err(err) => {
                let body = json!({
                    "stage": "build_signed_document",
                    "error": err.to_string(),
                })
                .to_string();
                warn!(error = %err, "domain rejected payload; marking failed");
                // Local validation never reached LHDN — keep attempts as-is.
                self.repo
                    .fail_permanently(
                        &event.invoice_id,
                        event.outbox_id,
                        invoice.attempts,
                        &body,
                    )
                    .await?;
                return Ok(());
            }
        };

        let docs = vec![SubmissionDocument {
            format: SubmissionFormat::Json,
            document_hash: signed.document_hash.clone(),
            code_number: signed.code_number.clone(),
            document: B64.encode(&signed.canonical_bytes),
        }];

        match self.lhdn.submit_documents(&docs).await {
            Ok(resp) => {
                self.handle_success(event, &signed, resp).await?;
            }
            Err(err) => {
                self.handle_failure(event, invoice.attempts, err).await?;
            }
        }
        Ok(())
    }

    fn build_doc(&self, invoice: &InvoiceForSubmit) -> anyhow::Result<SignedDocument> {
        let payload: Value = serde_json::from_str(&invoice.payload_json)?;
        let signed = build_signed_document(&payload, &self.signer, OffsetDateTime::now_utc())?;
        Ok(signed)
    }

    async fn handle_success(
        &self,
        event: &DueSubmitEvent,
        signed: &SignedDocument,
        resp: SubmissionResponse,
    ) -> anyhow::Result<()> {
        // Did LHDN reject our specific document?
        if let Some(rej) = resp
            .rejected_documents
            .iter()
            .find(|r| r.invoice_code_number == signed.code_number)
        {
            let body = serde_json::to_string(&json!({
                "stage": "lhdn_rejection",
                "code": rej.error.code,
                "message": rej.error.message,
                "details": rej.error.details,
            }))?;
            warn!(code = %rej.error.code, message = %rej.error.message, "lhdn rejected document");
            // The submission round-trip itself succeeded; the document was
            // rejected on validation. Don't bump attempts.
            let invoice = self.repo.load_for_submit(&event.invoice_id).await?;
            let attempts = invoice.map(|i| i.attempts).unwrap_or(0);
            self.repo
                .fail_permanently(&event.invoice_id, event.outbox_id, attempts, &body)
                .await?;
            return Ok(());
        }

        let lhdn_uuid = resp
            .accepted_documents
            .iter()
            .find(|a| a.invoice_code_number == signed.code_number)
            .map(|a| a.uuid.as_str())
            .unwrap_or("");

        let signed_doc_utf8 = std::str::from_utf8(&signed.canonical_bytes)?;
        self.repo
            .complete_submission(
                &event.invoice_id,
                event.outbox_id,
                SubmissionCompletion {
                    submission_uid: &resp.submission_uid,
                    lhdn_uuid,
                    signature_b64: &signed.signature,
                    signed_document_utf8: signed_doc_utf8,
                    document_hash_b64: &signed.document_hash,
                },
            )
            .await?;
        info!(submission_uid = %resp.submission_uid, lhdn_uuid, "submitted");
        Ok(())
    }

    async fn handle_failure(
        &self,
        event: &DueSubmitEvent,
        current_attempts: i64,
        err: LhdnError,
    ) -> anyhow::Result<()> {
        let new_attempts = current_attempts + 1;
        let last_error = err.to_string();
        let body = serde_json::to_string(&json!({
            "kind": variant_name(&err),
            "message": last_error,
            "attempts": new_attempts,
        }))?;

        if !err.is_transient() || new_attempts >= self.config.max_attempts {
            warn!(
                attempts = new_attempts,
                transient = err.is_transient(),
                error = %err,
                "permanent failure; marking Failed"
            );
            self.repo
                .fail_permanently(&event.invoice_id, event.outbox_id, new_attempts, &body)
                .await?;
        } else {
            let backoff = backoff_for(new_attempts);
            let next_at = OffsetDateTime::now_utc().unix_timestamp() + backoff.as_secs() as i64;
            warn!(
                attempts = new_attempts,
                backoff_secs = backoff.as_secs(),
                error = %err,
                "transient failure; rescheduling"
            );
            self.repo
                .reschedule(
                    &event.invoice_id,
                    event.outbox_id,
                    new_attempts,
                    next_at,
                    &body,
                    &last_error,
                )
                .await?;
        }
        Ok(())
    }
}

/// Backoff schedule: 30s, 2m, 10m, 1h (capped). Argument is the
/// 1-based attempt counter *after* the failure was recorded.
pub fn backoff_for(attempt: i64) -> Duration {
    match attempt {
        1 => Duration::from_secs(30),
        2 => Duration::from_secs(120),
        3 => Duration::from_secs(600),
        _ => Duration::from_secs(3600),
    }
}

fn variant_name(err: &LhdnError) -> &'static str {
    match err {
        LhdnError::Auth(_) => "Auth",
        LhdnError::BadRequest(_) => "BadRequest",
        LhdnError::NotFound => "NotFound",
        LhdnError::Conflict(_) => "Conflict",
        LhdnError::RateLimited { .. } => "RateLimited",
        LhdnError::Server { .. } => "Server",
        LhdnError::Transport(_) => "Transport",
        LhdnError::Schema(_) => "Schema",
        LhdnError::Storage(_) => "Storage",
        LhdnError::Config(_) => "Config",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_progression_matches_plan() {
        assert_eq!(backoff_for(1), Duration::from_secs(30));
        assert_eq!(backoff_for(2), Duration::from_secs(120));
        assert_eq!(backoff_for(3), Duration::from_secs(600));
        assert_eq!(backoff_for(4), Duration::from_secs(3600));
        assert_eq!(backoff_for(8), Duration::from_secs(3600));
    }
}
