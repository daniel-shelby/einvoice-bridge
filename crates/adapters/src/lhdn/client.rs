//! LHDN MyInvois HTTP client.
//!
//! Single struct (`LhdnClient`) owns:
//!   - the `reqwest::Client` (rustls only — no system OpenSSL),
//!   - the OAuth token store + an in-memory cache,
//!   - the four endpoints we care about for v1.

use std::sync::Arc;
use std::time::Duration;

use reqwest::{Client, Response, StatusCode};
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, instrument};

use super::LhdnEnv;
use super::error::LhdnError;
use super::models::{
    CancelRequest, DocumentDetails, IdScheme, LhdnErrorEnvelope, LhdnErrorResponse,
    SubmissionDocument, SubmissionRequest, SubmissionResponse, TokenResponse,
};
use super::oauth::CachedToken;
use super::token_repo::OauthTokenStore;

/// Fail an HTTP call after this much time end-to-end (TLS handshake, request
/// send, response read). Picked conservatively — LHDN returns within a few
/// seconds in normal operation, and the worker can retry on transport errors.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Fail the TCP/TLS connect step alone after this; lets a stalled handshake
/// bail out before the full request budget is spent.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// LHDN-recommended OAuth scope.
const DEFAULT_SCOPE: &str = "InvoicingAPI";

#[derive(Debug, Clone)]
pub struct LhdnConfig {
    /// Full base URL with no trailing slash. Production: use
    /// `LhdnEnv::base_url()`. Tests: pass a wiremock URL.
    pub base_url: String,
    pub client_id: String,
    pub client_secret: String,
    /// Stable identifier used to scope cached tokens in the
    /// `oauth_tokens` table (`"preprod"` / `"prod"`).
    pub env_name: String,
    /// OAuth scope string. MyInvois currently uses `"InvoicingAPI"`.
    pub scope: String,
}

impl LhdnConfig {
    /// Build a config targeting one of the known LHDN environments.
    /// Caller mutates `scope` after the fact if they need a non-default
    /// scope.
    pub fn for_env(
        env: LhdnEnv,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Self {
        Self {
            base_url: env.base_url().to_string(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            env_name: env.name().to_string(),
            scope: DEFAULT_SCOPE.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct LhdnClient {
    inner: Arc<Inner>,
}

struct Inner {
    http: Client,
    config: LhdnConfig,
    tokens: OauthTokenStore,
    cache: RwLock<Option<CachedToken>>,
    /// Held during a token refresh so concurrent callers wait rather than
    /// stampeding the OAuth endpoint.
    refresh: Mutex<()>,
}

impl LhdnClient {
    pub fn new(config: LhdnConfig, tokens: OauthTokenStore) -> Self {
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client builder cannot fail with these defaults");

        Self {
            inner: Arc::new(Inner {
                http,
                config,
                tokens,
                cache: RwLock::new(None),
                refresh: Mutex::new(()),
            }),
        }
    }

    /// Return a token that's valid for at least `TOKEN_LEEWAY`. Uses
    /// in-memory cache → DB cache → OAuth fetch, in that order. Refresh
    /// attempts are serialized so we don't pummel the OAuth endpoint.
    #[instrument(skip(self))]
    pub async fn access_token(&self) -> Result<String, LhdnError> {
        if let Some(t) = self.peek_fresh_cache().await {
            return Ok(t);
        }

        let _guard = self.inner.refresh.lock().await;

        // Another task may have refreshed while we waited on the lock.
        if let Some(t) = self.peek_fresh_cache().await {
            return Ok(t);
        }

        // First call after a restart: try the persisted token.
        if let Some(stored) = self.inner.tokens.get(&self.inner.config.env_name).await? {
            if stored.is_fresh() {
                let value = stored.access_token.clone();
                *self.inner.cache.write().await = Some(stored);
                debug!("loaded fresh oauth token from db cache");
                return Ok(value);
            }
        }

        let new_token = self.fetch_token().await?;
        self.inner
            .tokens
            .upsert(
                &self.inner.config.env_name,
                &new_token.access_token,
                new_token.expires_at,
            )
            .await?;
        let value = new_token.access_token.clone();
        *self.inner.cache.write().await = Some(new_token);
        debug!("fetched fresh oauth token from lhdn");
        Ok(value)
    }

    async fn peek_fresh_cache(&self) -> Option<String> {
        self.inner
            .cache
            .read()
            .await
            .as_ref()
            .filter(|t| t.is_fresh())
            .map(|t| t.access_token.clone())
    }

    async fn fetch_token(&self) -> Result<CachedToken, LhdnError> {
        let url = format!("{}/connect/token", self.inner.config.base_url);
        let resp = self
            .inner
            .http
            .post(&url)
            .form(&[
                ("client_id", self.inner.config.client_id.as_str()),
                ("client_secret", self.inner.config.client_secret.as_str()),
                ("grant_type", "client_credentials"),
                ("scope", self.inner.config.scope.as_str()),
            ])
            .send()
            .await?;

        let (status, body, _) = read_response(resp).await?;
        if status == StatusCode::UNAUTHORIZED
            || status == StatusCode::FORBIDDEN
            || status == StatusCode::BAD_REQUEST
        {
            return Err(LhdnError::Auth(body));
        }
        if !status.is_success() {
            return Err(LhdnError::Server {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: TokenResponse = serde_json::from_str(&body)?;
        if !parsed.token_type.eq_ignore_ascii_case("Bearer") {
            return Err(LhdnError::Schema(format!(
                "expected token_type \"Bearer\", got {:?}",
                parsed.token_type
            )));
        }
        let now = OffsetDateTime::now_utc().unix_timestamp();
        Ok(CachedToken {
            access_token: parsed.access_token,
            expires_at: now + parsed.expires_in,
        })
    }

    /// Submit a batch of UBL documents for validation/registration.
    #[instrument(skip(self, docs), fields(count = docs.len()))]
    pub async fn submit_documents(
        &self,
        docs: &[SubmissionDocument],
    ) -> Result<SubmissionResponse, LhdnError> {
        let token = self.access_token().await?;
        let url = format!(
            "{}/api/v1.0/documentsubmissions",
            self.inner.config.base_url
        );
        let resp = self
            .inner
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&SubmissionRequest { documents: docs })
            .send()
            .await?;
        parse_json_success(resp).await
    }

    /// Fetch full validation status + metadata for a previously submitted document.
    #[instrument(skip(self))]
    pub async fn get_document_details(&self, uuid: &str) -> Result<DocumentDetails, LhdnError> {
        let token = self.access_token().await?;
        let url = format!(
            "{}/api/v1.0/documents/{}/details",
            self.inner.config.base_url, uuid
        );
        let resp = self.inner.http.get(&url).bearer_auth(&token).send().await?;
        parse_json_success(resp).await
    }

    /// Cancel a document. Only valid within LHDN's cancellation window.
    #[instrument(skip(self))]
    pub async fn cancel_document(&self, uuid: &str, reason: &str) -> Result<(), LhdnError> {
        let token = self.access_token().await?;
        let url = format!(
            "{}/api/v1.0/documents/state/{}/state",
            self.inner.config.base_url, uuid
        );
        let resp = self
            .inner
            .http
            .put(&url)
            .bearer_auth(&token)
            .json(&CancelRequest {
                status: "cancelled",
                reason,
            })
            .send()
            .await?;
        ensure_success(resp).await
    }

    /// Validate a taxpayer by TIN + secondary id (NRIC/PASSPORT/BRN/ARMY).
    /// Returns `true` for 2xx, `false` for 404, error otherwise.
    #[instrument(skip(self))]
    pub async fn validate_taxpayer(
        &self,
        tin: &str,
        id_scheme: IdScheme,
        id_value: &str,
    ) -> Result<bool, LhdnError> {
        let token = self.access_token().await?;
        let url = format!(
            "{}/api/v1.0/taxpayer/validate/{}",
            self.inner.config.base_url, tin
        );
        let resp = self
            .inner
            .http
            .get(&url)
            .bearer_auth(&token)
            .query(&[("idType", id_scheme.as_str()), ("idValue", id_value)])
            .send()
            .await?;

        let status = resp.status();
        if status.is_success() {
            return Ok(true);
        }
        if status == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        let (status, body, retry_after) = read_response(resp).await?;
        Err(classify_error(status, &body, retry_after))
    }
}

// --- helpers --------------------------------------------------------------

async fn read_response(resp: Response) -> Result<(StatusCode, String, Option<u64>), LhdnError> {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let body = resp.text().await?;
    Ok((status, body, retry_after))
}

async fn parse_json_success<T: DeserializeOwned>(resp: Response) -> Result<T, LhdnError> {
    let (status, body, retry_after) = read_response(resp).await?;
    if status.is_success() {
        return Ok(serde_json::from_str(&body)?);
    }
    Err(classify_error(status, &body, retry_after))
}

async fn ensure_success(resp: Response) -> Result<(), LhdnError> {
    let (status, body, retry_after) = read_response(resp).await?;
    if status.is_success() {
        return Ok(());
    }
    Err(classify_error(status, &body, retry_after))
}

fn classify_error(status: StatusCode, body: &str, retry_after: Option<u64>) -> LhdnError {
    let envelope = serde_json::from_str::<LhdnErrorResponse>(body)
        .ok()
        .map(|r| r.error)
        .unwrap_or_else(|| LhdnErrorEnvelope {
            code: format!("HTTP{}", status.as_u16()),
            message: body.to_string(),
            target: None,
            details: vec![],
        });

    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => LhdnError::Auth(envelope.message),
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
            LhdnError::BadRequest(envelope)
        }
        StatusCode::NOT_FOUND => LhdnError::NotFound,
        StatusCode::CONFLICT => LhdnError::Conflict(envelope),
        StatusCode::TOO_MANY_REQUESTS => LhdnError::RateLimited {
            retry_after: retry_after.map(Duration::from_secs),
        },
        s => LhdnError::Server {
            status: s.as_u16(),
            body: body.to_string(),
        },
    }
}
