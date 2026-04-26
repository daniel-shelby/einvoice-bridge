//! Canceller worker.
//!
//! Picks up `cancel` outbox events (enqueued by the cancel API endpoint),
//! calls LHDN's PUT cancel endpoint, and transitions the invoice to
//! `Cancelled` on success.
//!
//! Failure modes:
//! - Transient (5xx, rate limit, transport): reschedule with the standard
//!   delivery backoff.
//! - Non-transient (auth, bad request, conflict): record the error and
//!   drop the outbox event. `lhdn_status` is left at `Valid` because LHDN
//!   did not in fact cancel the document.
//! - Past the cancellation window at dispatch time: refuse to call LHDN,
//!   record the error, drop the outbox event.

use std::time::Duration;

use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::watch;
use tracing::{error, info, instrument, warn};

use super::{lhdn_variant_name, submit_backoff_for};
use crate::lhdn::{LhdnClient, LhdnError};
use crate::repo::{DueCancelEvent, InvoiceForCancel, InvoiceRepo};

#[derive(Debug, Clone)]
pub struct CancellerConfig {
    pub poll_interval: Duration,
    pub batch_size: i64,
    pub max_attempts: i64,
}

impl Default for CancellerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            batch_size: 16,
            max_attempts: 8,
        }
    }
}

pub struct Canceller {
    repo: InvoiceRepo,
    lhdn: LhdnClient,
    config: CancellerConfig,
}

impl Canceller {
    pub fn new(repo: InvoiceRepo, lhdn: LhdnClient) -> Self {
        Self {
            repo,
            lhdn,
            config: CancellerConfig::default(),
        }
    }

    pub fn with_config(mut self, config: CancellerConfig) -> Self {
        self.config = config;
        self
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        info!(
            poll_interval_secs = self.config.poll_interval.as_secs(),
            batch_size = self.config.batch_size,
            max_attempts = self.config.max_attempts,
            "canceller started"
        );
        loop {
            let processed = match self.tick().await {
                Ok(n) => n,
                Err(err) => {
                    error!(error = %err, "canceller tick failed");
                    0
                }
            };
            if processed > 0 {
                continue;
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        info!("canceller shutting down");
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }
    }

    pub async fn tick(&self) -> anyhow::Result<usize> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let due = self
            .repo
            .due_cancel_events(now, self.config.batch_size)
            .await?;
        let count = due.len();
        for event in due {
            if let Err(err) = self.process_event(&event).await {
                error!(
                    invoice_id = %event.invoice_id,
                    outbox_id = event.outbox_id,
                    error = %err,
                    "cancel handler errored"
                );
            }
        }
        Ok(count)
    }

    #[instrument(skip(self), fields(invoice_id = %event.invoice_id, outbox_id = event.outbox_id))]
    async fn process_event(&self, event: &DueCancelEvent) -> anyhow::Result<()> {
        let invoice = match self.repo.load_for_cancel(&event.invoice_id).await? {
            Some(i) => i,
            None => {
                warn!("invoice row missing for cancel event; cleaning up orphan event");
                let body = json!({ "error": "invoice row missing" }).to_string();
                self.repo
                    .fail_cancel(&event.invoice_id, event.outbox_id, &body)
                    .await?;
                return Ok(());
            }
        };

        if let Some(reason) = check_preconditions(&invoice) {
            let body = serde_json::to_string(&json!({
                "stage": "cancel_precondition",
                "reason": reason,
            }))?;
            warn!(reason, "cancel precondition failed");
            self.repo
                .fail_cancel(&invoice.id, event.outbox_id, &body)
                .await?;
            return Ok(());
        }

        let uuid = invoice.lhdn_uuid.as_deref().unwrap_or_default();
        let cancellation_reason = invoice.cancellation_reason.as_deref().unwrap_or("");

        match self.lhdn.cancel_document(uuid, cancellation_reason).await {
            Ok(()) => {
                let now = OffsetDateTime::now_utc().unix_timestamp();
                info!(
                    invoice_ref = %invoice.invoice_ref,
                    "lhdn confirmed cancellation"
                );
                self.repo
                    .complete_cancel(&invoice.id, event.outbox_id, now)
                    .await?;
            }
            Err(err) => self.handle_failure(event, err).await?,
        }
        Ok(())
    }

    async fn handle_failure(&self, event: &DueCancelEvent, err: LhdnError) -> anyhow::Result<()> {
        let new_attempts = event.attempts + 1;
        let last_error = err.to_string();
        let body = serde_json::to_string(&json!({
            "stage": "cancel",
            "kind": lhdn_variant_name(&err),
            "message": last_error,
            "attempts": new_attempts,
        }))?;

        if !err.is_transient() || new_attempts >= self.config.max_attempts {
            warn!(
                attempts = new_attempts,
                transient = err.is_transient(),
                error = %err,
                "permanent cancel failure; clearing cancel event"
            );
            self.repo
                .fail_cancel(&event.invoice_id, event.outbox_id, &body)
                .await?;
        } else {
            let backoff = submit_backoff_for(new_attempts);
            let next_at = OffsetDateTime::now_utc().unix_timestamp() + backoff.as_secs() as i64;
            warn!(
                attempts = new_attempts,
                backoff_secs = backoff.as_secs(),
                error = %err,
                "transient cancel failure; rescheduling"
            );
            self.repo
                .reschedule_cancel(event.outbox_id, new_attempts, next_at, &last_error)
                .await?;
        }
        Ok(())
    }
}

fn check_preconditions(invoice: &InvoiceForCancel) -> Option<&'static str> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    if invoice
        .lhdn_uuid
        .as_deref()
        .map(str::is_empty)
        .unwrap_or(true)
    {
        return Some("missing lhdn_uuid");
    }
    if invoice
        .cancellation_reason
        .as_deref()
        .map(str::is_empty)
        .unwrap_or(true)
    {
        return Some("missing cancellation_reason");
    }
    match invoice.cancellable_until {
        Some(deadline) if deadline > now => None,
        _ => Some("past cancellation window"),
    }
}
