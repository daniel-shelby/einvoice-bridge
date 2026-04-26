//! OAuth token cache types.
//!
//! The actual fetch + cache-flow lives on `LhdnClient`; this file just
//! holds the data shape and the freshness check.

use std::time::Duration;

use time::OffsetDateTime;

/// Refresh threshold: a token is treated as "expiring" if it expires
/// within this many seconds. Refreshing slightly early avoids a race
/// where we send a request with a token that expires mid-flight.
pub const TOKEN_LEEWAY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct CachedToken {
    pub access_token: String,
    /// Unix seconds, UTC.
    pub expires_at: i64,
}

impl CachedToken {
    /// True if the token is still valid for at least `TOKEN_LEEWAY`.
    pub fn is_fresh(&self) -> bool {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        self.expires_at - now > TOKEN_LEEWAY.as_secs() as i64
    }
}
