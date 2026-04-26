//! Integration tests for the inbound HTTP API.
//!
//! Uses an in-memory SQLite pool with a single connection so the
//! `:memory:` database persists across queries inside one test.

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use einvoice_adapters::{
    api::{ApiState, router},
    repo::InvoiceRepo,
};
use serde_json::{Value, json};
use sqlx::sqlite::SqlitePoolOptions;
use tower::ServiceExt;

async fn test_app() -> Router {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .expect("apply migrations");
    router(ApiState {
        repo: InvoiceRepo::new(pool),
    })
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

#[tokio::test]
async fn submit_returns_202_with_pending_row() {
    let app = test_app().await;
    let payload = json!({ "invoice_ref": "INV-T1", "amount": 100 }).to_string();

    let resp = app.oneshot(post("/v1/invoices", &payload)).await.unwrap();

    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = read_json(resp.into_body()).await;
    assert_eq!(body["invoice_ref"], "INV-T1");
    assert_eq!(body["lhdn_status"], "Pending");
    assert_eq!(body["attempts"], 0);
    assert!(body["id"].as_str().is_some_and(|s| s.len() >= 32));
    assert!(body["lhdn_uuid"].is_null());
    assert!(body["qr_url"].is_null());
}

#[tokio::test]
async fn submit_then_get_returns_same_row() {
    let app = test_app().await;
    let payload = json!({ "invoice_ref": "INV-T2" }).to_string();

    let r1 = app
        .clone()
        .oneshot(post("/v1/invoices", &payload))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::ACCEPTED);
    let posted = read_json(r1.into_body()).await;

    let r2 = app
        .oneshot(
            Request::get("/v1/invoices/INV-T2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let fetched = read_json(r2.into_body()).await;

    assert_eq!(fetched["id"], posted["id"]);
    assert_eq!(fetched["lhdn_status"], "Pending");
}

#[tokio::test]
async fn missing_invoice_ref_returns_400() {
    let app = test_app().await;
    let resp = app
        .oneshot(post("/v1/invoices", r#"{"amount":100}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn empty_invoice_ref_returns_400() {
    let app = test_app().await;
    let resp = app
        .oneshot(post("/v1/invoices", r#"{"invoice_ref":""}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn duplicate_invoice_ref_returns_409() {
    let app = test_app().await;
    let payload = json!({ "invoice_ref": "INV-DUP" }).to_string();

    let r1 = app
        .clone()
        .oneshot(post("/v1/invoices", &payload))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::ACCEPTED);

    let r2 = app.oneshot(post("/v1/invoices", &payload)).await.unwrap();
    assert_eq!(r2.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn unknown_invoice_ref_returns_404() {
    let app = test_app().await;
    let resp = app
        .oneshot(
            Request::get("/v1/invoices/NOPE")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn submit_writes_outbox_event() {
    // The submit handler must enqueue a 'submit' outbox row in the same
    // transaction so the worker can pick it up.
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
    let app = router(ApiState {
        repo: InvoiceRepo::new(pool.clone()),
    });

    let payload = json!({ "invoice_ref": "INV-OUTBOX" }).to_string();
    let r = app.oneshot(post("/v1/invoices", &payload)).await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM outbox_events oe
         JOIN invoices i ON i.id = oe.invoice_id
         WHERE i.invoice_ref = ? AND oe.kind = 'submit'",
    )
    .bind("INV-OUTBOX")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn healthz_ok() {
    let app = test_app().await;
    let resp = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
