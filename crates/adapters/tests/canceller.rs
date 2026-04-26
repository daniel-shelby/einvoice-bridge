//! Integration tests for the canceller worker + the cancel API.

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use einvoice_adapters::{
    api::{ApiState, router},
    lhdn::{LhdnClient, LhdnConfig, OauthTokenStore},
    repo::InvoiceRepo,
    worker::Canceller,
};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use time::OffsetDateTime;
use tower::ServiceExt;
use wiremock::matchers::{body_partial_json, method, path};
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

/// Insert a `Valid` invoice with a 72h cancellation window — the only
/// state that can be cancelled.
async fn seed_valid(
    pool: &SqlitePool,
    invoice_ref: &str,
    lhdn_uuid: &str,
    cancellable_until: i64,
) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    sqlx::query(
        "INSERT INTO invoices (id, invoice_ref, payload_json, lhdn_status, lhdn_uuid, \
         qr_url, cancellable_until, attempts, created_at, updated_at, \
         submitted_at, validated_at) \
         VALUES (?, ?, '{}', 'Valid', ?, 'http://qr', ?, 0, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(invoice_ref)
    .bind(lhdn_uuid)
    .bind(cancellable_until)
    .bind(now)
    .bind(now)
    .bind(now)
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

async fn read_json(body: Body) -> Value {
    let bytes = to_bytes(body, 1 << 20).await.expect("read body");
    serde_json::from_slice(&bytes).expect("parse json body")
}

fn post(uri: &str, body: &str) -> Request<Body> {
    Request::post(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

// --- API tests -----------------------------------------------------------

#[tokio::test]
async fn cancel_endpoint_enqueues_event_and_returns_202() {
    let pool = fresh_pool().await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    seed_valid(&pool, "INV-CAN", "UUID-CAN", now + 60_000).await;

    let app = router(ApiState {
        repo: InvoiceRepo::new(pool.clone()),
    });

    let resp = app
        .oneshot(post(
            "/v1/invoices/INV-CAN/cancel",
            r#"{"reason":"wrong customer"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = read_json(resp.into_body()).await;
    assert_eq!(body["invoice_ref"], "INV-CAN");

    let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM outbox_events")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(kinds, vec!["cancel".to_string()]);

    let reason: Option<String> =
        sqlx::query_scalar("SELECT cancellation_reason FROM invoices WHERE invoice_ref = ?")
            .bind("INV-CAN")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("wrong customer"));
}

#[tokio::test]
async fn cancel_endpoint_rejects_pending_invoice() {
    let pool = fresh_pool().await;
    InvoiceRepo::new(pool.clone())
        .create_pending("INV-PEN", r#"{"invoice_ref":"INV-PEN"}"#)
        .await
        .unwrap();
    let app = router(ApiState {
        repo: InvoiceRepo::new(pool),
    });

    let resp = app
        .oneshot(post("/v1/invoices/INV-PEN/cancel", r#"{"reason":"x"}"#))
        .await
        .unwrap();
    // Pending isn't a cancellable state — caller should fix that, not retry.
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn cancel_endpoint_rejects_past_window() {
    let pool = fresh_pool().await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    seed_valid(&pool, "INV-OLD", "UUID-OLD", now - 60).await;
    let app = router(ApiState {
        repo: InvoiceRepo::new(pool),
    });

    let resp = app
        .oneshot(post("/v1/invoices/INV-OLD/cancel", r#"{"reason":"x"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn cancel_endpoint_409s_on_duplicate_request() {
    let pool = fresh_pool().await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    seed_valid(&pool, "INV-DUP", "UUID-DUP", now + 60_000).await;
    let app = router(ApiState {
        repo: InvoiceRepo::new(pool),
    });

    let r1 = app
        .clone()
        .oneshot(post("/v1/invoices/INV-DUP/cancel", r#"{"reason":"a"}"#))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::ACCEPTED);

    let r2 = app
        .oneshot(post("/v1/invoices/INV-DUP/cancel", r#"{"reason":"b"}"#))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn cancel_endpoint_400s_on_missing_reason() {
    let pool = fresh_pool().await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    seed_valid(&pool, "INV-NR", "UUID-NR", now + 60_000).await;
    let app = router(ApiState {
        repo: InvoiceRepo::new(pool),
    });

    let resp = app
        .oneshot(post("/v1/invoices/INV-NR/cancel", r#"{}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// --- Worker tests --------------------------------------------------------

async fn build_worker(server: &MockServer) -> (Canceller, InvoiceRepo, SqlitePool) {
    let pool = fresh_pool().await;
    let repo = InvoiceRepo::new(pool.clone());
    let lhdn = LhdnClient::new(cfg(server), OauthTokenStore::new(pool.clone()));
    let canceller = Canceller::new(repo.clone(), lhdn);
    (canceller, repo, pool)
}

#[tokio::test]
async fn canceller_happy_path_calls_lhdn_and_marks_cancelled() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("PUT"))
        .and(path("/api/v1.0/documents/state/UUID-W/state"))
        .and(body_partial_json(
            json!({ "status": "cancelled", "reason": "duplicated" }),
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let (canceller, repo, pool) = build_worker(&server).await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    seed_valid(&pool, "INV-W", "UUID-W", now + 60_000).await;

    repo.request_cancellation("INV-W", "duplicated")
        .await
        .unwrap();

    let processed = canceller.tick().await.unwrap();
    assert_eq!(processed, 1);

    let row = repo.find_by_ref("INV-W").await.unwrap().unwrap();
    assert_eq!(row.lhdn_status, "Cancelled");
    assert_eq!(outbox_count(&pool).await, 0);
}

#[tokio::test]
async fn canceller_transient_error_reschedules() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("PUT"))
        .and(path("/api/v1.0/documents/state/UUID-T/state"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;

    let (canceller, repo, pool) = build_worker(&server).await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    seed_valid(&pool, "INV-T", "UUID-T", now + 60_000).await;
    repo.request_cancellation("INV-T", "x").await.unwrap();

    canceller.tick().await.unwrap();

    let row = repo.find_by_ref("INV-T").await.unwrap().unwrap();
    assert_eq!(row.lhdn_status, "Valid"); // unchanged on transient
    let attempts: i64 =
        sqlx::query_scalar("SELECT attempts FROM outbox_events WHERE kind = 'cancel'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 1);
}

#[tokio::test]
async fn canceller_non_transient_clears_outbox_keeps_status() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("PUT"))
        .and(path("/api/v1.0/documents/state/UUID-N/state"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": { "code": "Conflict", "message": "already cancelled" }
        })))
        .mount(&server)
        .await;

    let (canceller, repo, pool) = build_worker(&server).await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    seed_valid(&pool, "INV-N", "UUID-N", now + 60_000).await;
    repo.request_cancellation("INV-N", "x").await.unwrap();

    canceller.tick().await.unwrap();

    let row = repo.find_by_ref("INV-N").await.unwrap().unwrap();
    // We didn't actually cancel it — leave status alone, surface error.
    assert_eq!(row.lhdn_status, "Valid");
    assert!(row.error_json.unwrap().contains("Conflict"));
    assert_eq!(outbox_count(&pool).await, 0);
}

#[tokio::test]
async fn canceller_skips_event_past_window() {
    // Window slipped between API enqueue and worker pickup. No HTTP call:
    // /connect/token is mounted with `.expect(0)` so the test fails if the
    // worker hits LHDN.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/connect/token"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let (canceller, repo, pool) = build_worker(&server).await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    // Seed a Valid row with a still-open window so request_cancellation
    // accepts it, then move the deadline into the past before the worker
    // picks the event up.
    seed_valid(&pool, "INV-EXP", "UUID-EXP", now + 60_000).await;
    repo.request_cancellation("INV-EXP", "x").await.unwrap();
    sqlx::query("UPDATE invoices SET cancellable_until = ? WHERE invoice_ref = ?")
        .bind(now - 60)
        .bind("INV-EXP")
        .execute(&pool)
        .await
        .unwrap();

    canceller.tick().await.unwrap();

    let row = repo.find_by_ref("INV-EXP").await.unwrap().unwrap();
    assert_eq!(row.lhdn_status, "Valid"); // unchanged
    assert!(row.error_json.unwrap().contains("past cancellation window"));
    assert_eq!(outbox_count(&pool).await, 0);
}
