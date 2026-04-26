//! Integration tests for the poller worker.
//!
//! Pre-seeds a `Submitted` invoice + a `poll` outbox row, then drives
//! `poller.tick()` against a wiremock LHDN.

use std::time::Duration;

use einvoice_adapters::{
    lhdn::{LhdnClient, LhdnConfig, LhdnEnv, OauthTokenStore},
    repo::InvoiceRepo,
    worker::{Poller, PollerConfig},
};
use serde_json::json;
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use time::OffsetDateTime;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// --- helpers -------------------------------------------------------------

async fn fresh_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
    pool
}

fn cfg(server: &MockServer) -> LhdnConfig {
    LhdnConfig {
        base_url: server.uri(),
        client_id: "tc".into(),
        client_secret: "ts".into(),
        env_name: "preprod".into(),
        scope: "InvoicingAPI".into(),
    }
}

async fn mount_token(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/connect/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "tok",
            "token_type": "Bearer",
            "expires_in": 3600
        })))
        .mount(server)
        .await;
}

async fn build(server: &MockServer) -> (Poller, InvoiceRepo, SqlitePool) {
    let pool = fresh_pool().await;
    let repo = InvoiceRepo::new(pool.clone());
    let lhdn = LhdnClient::new(cfg(server), OauthTokenStore::new(pool.clone()));
    let poller = Poller::new(repo.clone(), lhdn, LhdnEnv::Preprod);
    (poller, repo, pool)
}

/// Insert a fully-`Submitted` invoice plus a due `poll` outbox row.
async fn seed_submitted(pool: &SqlitePool, invoice_ref: &str, lhdn_uuid: &str) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    sqlx::query(
        "INSERT INTO invoices (id, invoice_ref, payload_json, lhdn_status, lhdn_uuid, \
         attempts, created_at, updated_at, submitted_at) \
         VALUES (?, ?, '{}', 'Submitted', ?, 0, ?, ?, ?)",
    )
    .bind(&id)
    .bind(invoice_ref)
    .bind(lhdn_uuid)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO outbox_events (invoice_id, kind, available_at) VALUES (?, 'poll', ?)")
        .bind(&id)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn outbox_count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM outbox_events")
        .fetch_one(pool)
        .await
        .unwrap()
}

// --- tests ---------------------------------------------------------------

#[tokio::test]
async fn valid_response_sets_qr_url_and_cancellation_window() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1.0/documents/UUID-V/details"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uuid": "UUID-V",
            "longId": "LONG-1",
            "internalId": "INV-V",
            "status": "Valid",
            "dateTimeValidated": "2026-04-26T14:30:00Z"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let (poller, repo, pool) = build(&server).await;
    seed_submitted(&pool, "INV-V", "UUID-V").await;

    let processed = poller.tick().await.unwrap();
    assert_eq!(processed, 1);

    let row = repo.find_by_ref("INV-V").await.unwrap().expect("row");
    assert_eq!(row.lhdn_status, "Valid");
    assert_eq!(
        row.qr_url.as_deref(),
        Some("https://preprod.myinvois.hasil.gov.my/UUID-V/share/LONG-1")
    );

    // 72h cancellation window from the *validated* timestamp (2026-04-26 14:30Z).
    let cancellable_until: i64 =
        sqlx::query_scalar("SELECT cancellable_until FROM invoices WHERE invoice_ref = ?")
            .bind("INV-V")
            .fetch_one(&pool)
            .await
            .unwrap();
    let validated_at = OffsetDateTime::parse(
        "2026-04-26T14:30:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap()
    .unix_timestamp();
    assert_eq!(cancellable_until, validated_at + 72 * 60 * 60);

    assert_eq!(outbox_count(&pool).await, 0);
}

#[tokio::test]
async fn invalid_response_marks_invalid_and_clears_outbox() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1.0/documents/UUID-I/details"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uuid": "UUID-I",
            "longId": null,
            "internalId": "INV-I",
            "status": "Invalid",
            "documentStatusReason": "issuer TIN does not match"
        })))
        .mount(&server)
        .await;

    let (poller, repo, pool) = build(&server).await;
    seed_submitted(&pool, "INV-I", "UUID-I").await;

    poller.tick().await.unwrap();

    let row = repo.find_by_ref("INV-I").await.unwrap().unwrap();
    assert_eq!(row.lhdn_status, "Invalid");
    assert!(
        row.error_json.unwrap().contains("issuer TIN"),
        "reason should be persisted in error_json"
    );
    assert_eq!(outbox_count(&pool).await, 0);
}

#[tokio::test]
async fn lhdn_side_cancellation_observed_via_poll() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1.0/documents/UUID-C/details"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uuid": "UUID-C",
            "longId": null,
            "internalId": "INV-C",
            "status": "Cancelled",
            "cancelDateTime": "2026-04-26T15:00:00Z"
        })))
        .mount(&server)
        .await;

    let (poller, repo, pool) = build(&server).await;
    seed_submitted(&pool, "INV-C", "UUID-C").await;

    poller.tick().await.unwrap();

    let row = repo.find_by_ref("INV-C").await.unwrap().unwrap();
    assert_eq!(row.lhdn_status, "Cancelled");
    assert_eq!(outbox_count(&pool).await, 0);
}

#[tokio::test]
async fn still_submitted_reschedules_with_backoff() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1.0/documents/UUID-S/details"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uuid": "UUID-S",
            "longId": null,
            "internalId": "INV-S",
            "status": "Submitted"
        })))
        .mount(&server)
        .await;

    let (poller, repo, pool) = build(&server).await;
    seed_submitted(&pool, "INV-S", "UUID-S").await;

    let now_before = OffsetDateTime::now_utc().unix_timestamp();
    poller.tick().await.unwrap();

    let row = repo.find_by_ref("INV-S").await.unwrap().unwrap();
    assert_eq!(row.lhdn_status, "Submitted");

    // Outbox event preserved, attempts incremented, available_at pushed
    // forward by the first poll backoff step (5s).
    let (attempts, available_at): (i64, i64) =
        sqlx::query_as("SELECT attempts, available_at FROM outbox_events WHERE kind = 'poll'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 1);
    assert!(
        available_at >= now_before + 5,
        "available_at should advance by at least 5s"
    );
}

#[tokio::test]
async fn transient_error_reschedules_without_changing_status() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1.0/documents/UUID-503/details"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let (poller, repo, pool) = build(&server).await;
    seed_submitted(&pool, "INV-503", "UUID-503").await;

    poller.tick().await.unwrap();

    let row = repo.find_by_ref("INV-503").await.unwrap().unwrap();
    assert_eq!(row.lhdn_status, "Submitted"); // unchanged
    assert_eq!(outbox_count(&pool).await, 1); // outbox still around
}

#[tokio::test]
async fn auth_error_clears_outbox_without_changing_status() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1.0/documents/UUID-401/details"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": { "code": "Unauthorized", "message": "token rejected" }
        })))
        .mount(&server)
        .await;

    let (poller, repo, pool) = build(&server).await;
    seed_submitted(&pool, "INV-401", "UUID-401").await;

    poller.tick().await.unwrap();

    let row = repo.find_by_ref("INV-401").await.unwrap().unwrap();
    // Auth failure on poll: don't lie about LHDN's view of the doc, just
    // record the error and surface to ops.
    assert_eq!(row.lhdn_status, "Submitted");
    assert!(row.error_json.unwrap().contains("Auth"));
    assert_eq!(outbox_count(&pool).await, 0);
}

#[tokio::test]
async fn poll_attempt_exhaustion_marks_invalid() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1.0/documents/UUID-X/details"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uuid": "UUID-X",
            "longId": null,
            "internalId": "INV-X",
            "status": "Submitted"
        })))
        .mount(&server)
        .await;

    let (poller, repo, pool) = build(&server).await;
    let poller = poller.with_config(PollerConfig {
        poll_interval: Duration::from_millis(0),
        batch_size: 16,
        max_attempts: 2,
    });
    seed_submitted(&pool, "INV-X", "UUID-X").await;

    // First tick: attempts 0 → 1, reschedule.
    poller.tick().await.unwrap();
    sqlx::query("UPDATE outbox_events SET available_at = 0")
        .execute(&pool)
        .await
        .unwrap();
    // Second tick: attempts 1 → 2 == max_attempts → mark Invalid.
    poller.tick().await.unwrap();

    let row = repo.find_by_ref("INV-X").await.unwrap().unwrap();
    assert_eq!(row.lhdn_status, "Invalid");
    assert_eq!(outbox_count(&pool).await, 0);
}
