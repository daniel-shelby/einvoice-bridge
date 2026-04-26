//! Background workers driven by the `outbox_events` queue.
//!
//! Three loops, each owning one outbox `kind`:
//! - [`Submitter`] handles `submit` events: build + sign + POST to LHDN.
//! - [`Poller`] handles `poll` events: fetch document details, transition
//!   `Submitted` → `Valid`/`Invalid`/`Cancelled`, set `qr_url` and the 72h
//!   cancellation window.
//! - [`Canceller`] handles `cancel` events: PUT the cancel to LHDN, then
//!   transition the invoice to `Cancelled`.

use std::time::Duration;

use crate::lhdn::LhdnError;

pub mod canceller;
pub mod poller;
pub mod submitter;

pub use canceller::{Canceller, CancellerConfig};
pub use poller::{Poller, PollerConfig};
pub use submitter::{Submitter, SubmitterConfig};

/// Backoff schedule for delivery-style retries (submit + cancel calls).
/// 30s → 2m → 10m → 1h (capped). Argument is the 1-based attempt counter
/// *after* the failure was recorded.
pub fn submit_backoff_for(attempt: i64) -> Duration {
    match attempt {
        1 => Duration::from_secs(30),
        2 => Duration::from_secs(120),
        3 => Duration::from_secs(600),
        _ => Duration::from_secs(3600),
    }
}

/// Backoff schedule for the poller. LHDN typically validates within
/// seconds-to-minutes, so the early steps are quick. After ~10 polls
/// (≈30 minutes elapsed) we settle into a coarser cadence.
pub fn poll_backoff_for(attempt: i64) -> Duration {
    match attempt {
        0 | 1 => Duration::from_secs(5),
        2 => Duration::from_secs(15),
        3 => Duration::from_secs(30),
        4 => Duration::from_secs(60),
        5..=10 => Duration::from_secs(300),
        _ => Duration::from_secs(900),
    }
}

/// Stable, machine-readable name for an `LhdnError` variant. Used in the
/// `error_json` written to the invoice row so dashboards can group by it.
pub fn lhdn_variant_name(err: &LhdnError) -> &'static str {
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
    fn submit_backoff_progression_matches_plan() {
        assert_eq!(submit_backoff_for(1), Duration::from_secs(30));
        assert_eq!(submit_backoff_for(2), Duration::from_secs(120));
        assert_eq!(submit_backoff_for(3), Duration::from_secs(600));
        assert_eq!(submit_backoff_for(4), Duration::from_secs(3600));
        assert_eq!(submit_backoff_for(8), Duration::from_secs(3600));
    }

    #[test]
    fn poll_backoff_starts_short_and_caps() {
        assert_eq!(poll_backoff_for(0), Duration::from_secs(5));
        assert_eq!(poll_backoff_for(1), Duration::from_secs(5));
        assert_eq!(poll_backoff_for(4), Duration::from_secs(60));
        assert_eq!(poll_backoff_for(7), Duration::from_secs(300));
        assert_eq!(poll_backoff_for(50), Duration::from_secs(900));
    }
}
