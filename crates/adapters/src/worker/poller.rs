//! Poller worker.
//!
//! For each `Submitted` invoice with a due `poll` outbox event, GET the
//! document details from LHDN and transition the row:
//!
//! - `Valid`     → set `long_id` + `qr_url` + 72h cancellation window,
//!   drop the poll event.
//! - `Invalid`   → record the reason, drop the poll event.
//! - `Cancelled` → record the cancellation timestamp, drop the poll
//!   event. (LHDN-initiated cancel — distinct from our own cancel flow.)
//! - `Submitted` → reschedule with the poll backoff. Validation is still
//!   in progress; this is normal.
//! - Transient HTTP error → reschedule with the poll backoff.
//! - Non-transient error → fail the poll (drop the outbox event, leave
//!   `lhdn_status` as `Submitted` so an operator sees something is off).

use std::time::Duration;

use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::watch;
use tracing::{error, info, instrument, warn};

use super::{lhdn_variant_name, poll_backoff_for};
use crate::lhdn::{DocumentDetails, DocumentStatus, LhdnClient, LhdnEnv, LhdnError};
use crate::repo::{DuePollEvent, InvoiceForPoll, InvoiceRepo};

const CANCELLATION_WINDOW: Duration = Duration::from_secs(72 * 60 * 60);

#[derive(Debug, Clone)]
pub struct PollerConfig {
    pub poll_interval: Duration,
    pub batch_size: i64,
    /// Hard ceiling on poll attempts before giving up. With the default
    /// backoff curve (5s, 15s, 30s, 60s, then 5min, then 15min) 60 attempts
    /// is roughly 13 hours of waiting.
    pub max_attempts: i64,
}

impl Default for PollerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(2),
            batch_size: 16,
            max_attempts: 60,
        }
    }
}

pub struct Poller {
    repo: InvoiceRepo,
    lhdn: LhdnClient,
    env: LhdnEnv,
    config: PollerConfig,
}

impl Poller {
    pub fn new(repo: InvoiceRepo, lhdn: LhdnClient, env: LhdnEnv) -> Self {
        Self {
            repo,
            lhdn,
            env,
            config: PollerConfig::default(),
        }
    }

    pub fn with_config(mut self, config: PollerConfig) -> Self {
        self.config = config;
        self
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        info!(
            poll_interval_secs = self.config.poll_interval.as_secs(),
            batch_size = self.config.batch_size,
            max_attempts = self.config.max_attempts,
            "poller started"
        );
        loop {
            let processed = match self.tick().await {
                Ok(n) => n,
                Err(err) => {
                    error!(error = %err, "poller tick failed");
                    0
                }
            };
            if processed > 0 {
                continue;
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        info!("poller shutting down");
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
            .due_poll_events(now, self.config.batch_size)
            .await?;
        let count = due.len();
        for event in due {
            if let Err(err) = self.process_event(&event).await {
                error!(
                    invoice_id = %event.invoice_id,
                    outbox_id = event.outbox_id,
                    error = %err,
                    "poll handler errored"
                );
            }
        }
        Ok(count)
    }

    #[instrument(skip(self), fields(invoice_id = %event.invoice_id, outbox_id = event.outbox_id))]
    async fn process_event(&self, event: &DuePollEvent) -> anyhow::Result<()> {
        let invoice = match self.repo.load_for_poll(&event.invoice_id).await? {
            Some(i) => i,
            None => {
                warn!("invoice row missing for poll event; cleaning up orphan event");
                // No invoice row → nothing to update. Drop the orphan
                // outbox row by faking an exhausted retry.
                self.repo
                    .reschedule_poll(event.outbox_id, self.config.max_attempts, 0, Some("orphan"))
                    .await?;
                return Ok(());
            }
        };

        let uuid = match invoice.lhdn_uuid.as_deref() {
            Some(u) if !u.is_empty() => u,
            _ => {
                warn!("invoice has no lhdn_uuid; cannot poll, marking as failed poll");
                self.repo
                    .reschedule_poll(
                        event.outbox_id,
                        self.config.max_attempts,
                        0,
                        Some("missing lhdn_uuid"),
                    )
                    .await?;
                return Ok(());
            }
        };

        match self.lhdn.get_document_details(uuid).await {
            Ok(details) => self.handle_details(event, &invoice, details).await?,
            Err(err) => self.handle_failure(event, err).await?,
        }
        Ok(())
    }

    async fn handle_details(
        &self,
        event: &DuePollEvent,
        invoice: &InvoiceForPoll,
        details: DocumentDetails,
    ) -> anyhow::Result<()> {
        match details.status {
            DocumentStatus::Valid => {
                let validated_at = details
                    .date_time_validated
                    .as_deref()
                    .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                    .map(|t| t.unix_timestamp())
                    .unwrap_or_else(|| OffsetDateTime::now_utc().unix_timestamp());
                let cancellable_until = validated_at + CANCELLATION_WINDOW.as_secs() as i64;
                let long_id = details.long_id.as_deref().unwrap_or("");
                let qr_url = build_qr_url(self.env, &details.uuid, long_id);
                info!(
                    invoice_ref = %invoice.invoice_ref,
                    lhdn_uuid = %details.uuid,
                    "lhdn validated document"
                );
                self.repo
                    .mark_valid(
                        &invoice.id,
                        event.outbox_id,
                        long_id,
                        &qr_url,
                        validated_at,
                        cancellable_until,
                    )
                    .await?;
            }
            DocumentStatus::Invalid => {
                let body = serde_json::to_string(&json!({
                    "stage": "lhdn_validation",
                    "status": "Invalid",
                    "reason": details.document_status_reason,
                }))?;
                warn!(
                    invoice_ref = %invoice.invoice_ref,
                    reason = ?details.document_status_reason,
                    "lhdn marked document Invalid"
                );
                self.repo
                    .mark_invalid(&invoice.id, event.outbox_id, &body)
                    .await?;
            }
            DocumentStatus::Cancelled => {
                let cancelled_at = details
                    .cancel_date_time
                    .as_deref()
                    .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                    .map(|t| t.unix_timestamp())
                    .unwrap_or_else(|| OffsetDateTime::now_utc().unix_timestamp());
                info!(
                    invoice_ref = %invoice.invoice_ref,
                    "lhdn-side cancellation observed via poll"
                );
                self.repo
                    .mark_cancelled_via_poll(&invoice.id, event.outbox_id, cancelled_at)
                    .await?;
            }
            DocumentStatus::Submitted => {
                // Still validating — back off and check again.
                let new_attempts = event.attempts + 1;
                if new_attempts >= self.config.max_attempts {
                    warn!(
                        attempts = new_attempts,
                        "exhausted poll attempts while still Submitted; giving up on this poll event"
                    );
                    let body = serde_json::to_string(&json!({
                        "stage": "poll_exhausted",
                        "status": "Submitted",
                        "attempts": new_attempts,
                    }))?;
                    self.repo
                        .mark_invalid(&invoice.id, event.outbox_id, &body)
                        .await?;
                    return Ok(());
                }
                let backoff = poll_backoff_for(new_attempts);
                let next_at = OffsetDateTime::now_utc().unix_timestamp() + backoff.as_secs() as i64;
                self.repo
                    .reschedule_poll(event.outbox_id, new_attempts, next_at, None)
                    .await?;
            }
        }
        Ok(())
    }

    async fn handle_failure(&self, event: &DuePollEvent, err: LhdnError) -> anyhow::Result<()> {
        let new_attempts = event.attempts + 1;
        if !err.is_transient() || new_attempts >= self.config.max_attempts {
            warn!(
                attempts = new_attempts,
                transient = err.is_transient(),
                error = %err,
                "permanent poll failure; clearing poll event"
            );
            let body = serde_json::to_string(&json!({
                "stage": "poll",
                "kind": lhdn_variant_name(&err),
                "message": err.to_string(),
                "attempts": new_attempts,
            }))?;
            self.repo
                .fail_poll(&event.invoice_id, event.outbox_id, &body)
                .await?;
            return Ok(());
        }
        let backoff = poll_backoff_for(new_attempts);
        let next_at = OffsetDateTime::now_utc().unix_timestamp() + backoff.as_secs() as i64;
        warn!(
            attempts = new_attempts,
            backoff_secs = backoff.as_secs(),
            error = %err,
            "transient poll failure; rescheduling"
        );
        self.repo
            .reschedule_poll(
                event.outbox_id,
                new_attempts,
                next_at,
                Some(&err.to_string()),
            )
            .await?;
        Ok(())
    }
}

fn build_qr_url(env: LhdnEnv, uuid: &str, long_id: &str) -> String {
    format!("{}/{}/share/{}", env.portal_base_url(), uuid, long_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_url_uses_env_specific_portal() {
        assert_eq!(
            build_qr_url(LhdnEnv::Preprod, "u-1", "L-1"),
            "https://preprod.myinvois.hasil.gov.my/u-1/share/L-1"
        );
        assert_eq!(
            build_qr_url(LhdnEnv::Prod, "u-2", "L-2"),
            "https://myinvois.hasil.gov.my/u-2/share/L-2"
        );
    }
}
