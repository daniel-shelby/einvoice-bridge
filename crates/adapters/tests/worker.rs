//! Integration tests for the submitter worker.
//!
//! Drives an in-memory SQLite + a wiremock LHDN through the submitter's
//! `tick()`, asserting the row's terminal state and outbox bookkeeping.

use std::sync::Arc;
use std::time::Duration;

use einvoice_adapters::{
    lhdn::{LhdnClient, LhdnConfig, OauthTokenStore},
    repo::InvoiceRepo,
    worker::{Submitter, SubmitterConfig},
};
use einvoice_domain::Signer;
use rsa::RsaPrivateKey;
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

fn ephemeral_signer() -> Arc<Signer> {
    let mut rng = rand::thread_rng();
    let key = RsaPrivateKey::new(&mut rng, 1024).expect("generate test key");
    Arc::new(Signer::from_parts(key, b"placeholder-cert".to_vec()))
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

async fn build(server: &MockServer) -> (Submitter, InvoiceRepo, SqlitePool) {
    let pool = fresh_pool().await;
    let repo = InvoiceRepo::new(pool.clone());
    let lhdn = LhdnClient::new(cfg(server), OauthTokenStore::new(pool.clone()));
    let submitter = Submitter::new(repo.clone(), lhdn, ephemeral_signer());
    (submitter, repo, pool)
}

fn payload_for(invoice_ref: &str) -> String {
    json!({
        "invoice_ref": invoice_ref,
        "issue_date": "2026-04-26",
        "issue_time": "14:30:00",
        "currency": "MYR"
    })
    .to_string()
}

async fn outbox_count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM outbox_events")
        .fetch_one(pool)
        .await
        .unwrap()
}

// --- tests ---------------------------------------------------------------

#[tokio::test]
async fn happy_path_submits_persists_and_clears_outbox() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v1.0/documentsubmissions"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "submissionUid": "SUB-1",
            "acceptedDocuments": [
                { "uuid": "UUID-1", "invoiceCodeNumber": "INV-T1" }
            ],
            "rejectedDocuments": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let (submitter, repo, pool) = build(&server).await;
    repo.create_pending("INV-T1", &payload_for("INV-T1"))
        .await
        .unwrap();

    let processed = submitter.tick().await.unwrap();
    assert_eq!(processed, 1);

    let row = repo.find_by_ref("INV-T1").await.unwrap().expect("row");
    assert_eq!(row.lhdn_status, "Submitted");
    assert_eq!(row.lhdn_uuid.as_deref(), Some("UUID-1"));
    assert_eq!(row.attempts, 0); // happy path doesn't bump attempts
    assert!(row.error_json.is_none());

    // The submit outbox event is replaced by a fresh `poll` event so the
    // poller worker can take it from here.
    let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM outbox_events")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(kinds, vec!["poll".to_string()]);

    // The signed UBL doc + signature + doc digest land on the row.
    #[derive(sqlx::FromRow)]
    struct PersistedSignFields {
        ubl_xml: Option<String>,
        signature: Option<String>,
        doc_digest: Option<String>,
    }
    let persisted: PersistedSignFields =
        sqlx::query_as("SELECT ubl_xml, signature, doc_digest FROM invoices WHERE invoice_ref = ?")
            .bind("INV-T1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let ubl = persisted.ubl_xml.expect("ubl_xml populated");
    assert!(
        ubl.contains("UBLExtensions"),
        "stored doc should have signature block"
    );
    assert!(ubl.contains("INV-T1"));
    let sig = persisted.signature.expect("signature populated");
    assert!(!sig.is_empty(), "signature must not be empty");
    let digest = persisted.doc_digest.expect("doc_digest populated");
    assert!(!digest.is_empty(), "doc_digest must not be empty");
}

#[tokio::test]
async fn auth_error_marks_failed_without_retry() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v1.0/documentsubmissions"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": { "code": "Unauthorized", "message": "token rejected" }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let (submitter, repo, pool) = build(&server).await;
    repo.create_pending("INV-AUTH", &payload_for("INV-AUTH"))
        .await
        .unwrap();

    submitter.tick().await.unwrap();

    let row = repo.find_by_ref("INV-AUTH").await.unwrap().unwrap();
    // 401 is non-transient: do not retry, surface to ops immediately.
    assert_eq!(row.lhdn_status, "Failed");
    assert_eq!(row.attempts, 1);
    let err = row.error_json.unwrap();
    assert!(err.contains("Auth"), "got {err}");

    assert_eq!(outbox_count(&pool).await, 0);
}

#[tokio::test]
async fn transient_error_increments_attempts_and_keeps_outbox() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v1.0/documentsubmissions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("backend down"))
        .mount(&server)
        .await;

    let (submitter, repo, pool) = build(&server).await;
    repo.create_pending("INV-RET", &payload_for("INV-RET"))
        .await
        .unwrap();

    let now_before = OffsetDateTime::now_utc().unix_timestamp();
    submitter.tick().await.unwrap();

    let row = repo.find_by_ref("INV-RET").await.unwrap().unwrap();
    assert_eq!(row.lhdn_status, "Pending");
    assert_eq!(row.attempts, 1);
    let err = row.error_json.unwrap();
    assert!(err.contains("Server"), "got {err}");

    // Outbox row preserved, available_at advanced to now + 30s (the first
    // backoff step).
    let next: i64 = sqlx::query_scalar(
        "SELECT oe.available_at FROM outbox_events oe \
         JOIN invoices i ON oe.invoice_id = i.id WHERE i.invoice_ref = ?",
    )
    .bind("INV-RET")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(next >= now_before + 30, "available_at should advance ~30s");
}

#[tokio::test]
async fn non_transient_error_marks_failed_and_clears_outbox() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v1.0/documentsubmissions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": { "code": "BadDoc", "message": "TIN format wrong" }
        })))
        .mount(&server)
        .await;

    let (submitter, repo, pool) = build(&server).await;
    repo.create_pending("INV-BAD", &payload_for("INV-BAD"))
        .await
        .unwrap();

    submitter.tick().await.unwrap();

    let row = repo.find_by_ref("INV-BAD").await.unwrap().unwrap();
    assert_eq!(row.lhdn_status, "Failed");
    let err = row.error_json.unwrap();
    assert!(err.contains("BadRequest"), "got {err}");

    assert_eq!(outbox_count(&pool).await, 0);
}

#[tokio::test]
async fn lhdn_per_document_rejection_marks_failed() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v1.0/documentsubmissions"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "submissionUid": "SUB-2",
            "acceptedDocuments": [],
            "rejectedDocuments": [
                {
                    "invoiceCodeNumber": "INV-REJ",
                    "error": { "code": "Validation", "message": "issuer TIN missing" }
                }
            ]
        })))
        .mount(&server)
        .await;

    let (submitter, repo, pool) = build(&server).await;
    repo.create_pending("INV-REJ", &payload_for("INV-REJ"))
        .await
        .unwrap();

    submitter.tick().await.unwrap();

    let row = repo.find_by_ref("INV-REJ").await.unwrap().unwrap();
    assert_eq!(row.lhdn_status, "Failed");
    assert!(row.error_json.unwrap().contains("Validation"));
    assert_eq!(outbox_count(&pool).await, 0);
}

#[tokio::test]
async fn max_attempts_exhaustion_marks_failed() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v1.0/documentsubmissions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;

    let (submitter, repo, pool) = build(&server).await;
    let submitter = submitter.with_config(SubmitterConfig {
        poll_interval: Duration::from_millis(0),
        batch_size: 16,
        max_attempts: 2,
    });

    repo.create_pending("INV-MAX", &payload_for("INV-MAX"))
        .await
        .unwrap();

    // First tick: attempts 0 → 1, transient → reschedule.
    submitter.tick().await.unwrap();
    let row = repo.find_by_ref("INV-MAX").await.unwrap().unwrap();
    assert_eq!(row.lhdn_status, "Pending");
    assert_eq!(row.attempts, 1);

    // Pull the outbox available_at back to now so the next tick picks it up.
    sqlx::query("UPDATE outbox_events SET available_at = 0")
        .execute(&pool)
        .await
        .unwrap();

    // Second tick: attempts 1 → 2 == max_attempts → Failed.
    submitter.tick().await.unwrap();
    let row = repo.find_by_ref("INV-MAX").await.unwrap().unwrap();
    assert_eq!(row.lhdn_status, "Failed");
    assert_eq!(row.attempts, 2);
    assert_eq!(outbox_count(&pool).await, 0);
}

#[tokio::test]
async fn future_dated_outbox_event_is_skipped() {
    let server = MockServer::start().await;
    // Mount nothing — if the worker accidentally hits LHDN, the test fails
    // because there's no /connect/token mock.
    Mock::given(method("POST"))
        .and(path("/connect/token"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let (submitter, repo, pool) = build(&server).await;
    repo.create_pending("INV-FUT", &payload_for("INV-FUT"))
        .await
        .unwrap();

    // Push the row's outbox event 1h into the future.
    let future = OffsetDateTime::now_utc().unix_timestamp() + 3600;
    sqlx::query("UPDATE outbox_events SET available_at = ?")
        .bind(future)
        .execute(&pool)
        .await
        .unwrap();

    let processed = submitter.tick().await.unwrap();
    assert_eq!(processed, 0, "future events must not be picked up");
}
