//! Integration tests for the LHDN HTTP client. Uses `wiremock` to stand
//! in for the real MyInvois API so we exercise the full token-cache +
//! request/response path without ever touching the internet.

use einvoice_adapters::lhdn::{
    IdScheme, LhdnClient, LhdnConfig, LhdnError, OauthTokenStore, SubmissionDocument,
    SubmissionFormat,
};
use serde_json::json;
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use time::OffsetDateTime;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

// --- helpers --------------------------------------------------------------

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
        client_id: "test-client".into(),
        client_secret: "test-secret".into(),
        env_name: "preprod".into(),
        scope: "InvoicingAPI".into(),
    }
}

async fn mount_token(server: &MockServer, expect_calls: u64) {
    Mock::given(method("POST"))
        .and(path("/connect/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "tok-abc",
            "token_type": "Bearer",
            "expires_in": 3600
        })))
        .expect(expect_calls)
        .mount(server)
        .await;
}

async fn mount_details_ok(server: &MockServer, uuid: &str, bearer: &str, expect_calls: u64) {
    Mock::given(method("GET"))
        .and(path(format!("/api/v1.0/documents/{uuid}/details")))
        .and(header("authorization", format!("Bearer {bearer}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uuid": uuid,
            "longId": null,
            "internalId": "INV",
            "status": "Valid"
        })))
        .expect(expect_calls)
        .mount(server)
        .await;
}

// --- tests ---------------------------------------------------------------

#[tokio::test]
async fn first_call_fetches_token_then_reuses_it_across_requests() {
    let server = MockServer::start().await;
    mount_token(&server, 1).await; // exactly one OAuth fetch
    mount_details_ok(&server, "uuid-1", "tok-abc", 2).await;

    let client = LhdnClient::new(cfg(&server), OauthTokenStore::new(fresh_pool().await));

    let d1 = client.get_document_details("uuid-1").await.unwrap();
    assert_eq!(d1.uuid, "uuid-1");
    let d2 = client.get_document_details("uuid-1").await.unwrap();
    assert_eq!(d2.uuid, "uuid-1");
    // wiremock asserts .expect() counts when MockServer drops.
}

#[tokio::test]
async fn token_persisted_to_oauth_tokens_after_refresh() {
    let server = MockServer::start().await;
    mount_token(&server, 1).await;
    mount_details_ok(&server, "u", "tok-abc", 1).await;

    let pool = fresh_pool().await;
    let store = OauthTokenStore::new(pool);
    let client = LhdnClient::new(cfg(&server), store.clone());

    client.get_document_details("u").await.unwrap();

    let stored = store.get("preprod").await.unwrap().expect("token row");
    assert_eq!(stored.access_token, "tok-abc");
    assert!(stored.expires_at > OffsetDateTime::now_utc().unix_timestamp());
}

#[tokio::test]
async fn fresh_client_with_valid_db_token_skips_oauth_fetch() {
    let server = MockServer::start().await;
    // Strict: any hit on /connect/token fails the test.
    Mock::given(method("POST"))
        .and(path("/connect/token"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    mount_details_ok(&server, "u", "prefilled", 1).await;

    let pool = fresh_pool().await;
    let store = OauthTokenStore::new(pool);
    let future = OffsetDateTime::now_utc().unix_timestamp() + 3600;
    store.upsert("preprod", "prefilled", future).await.unwrap();

    let client = LhdnClient::new(cfg(&server), store);
    let d = client.get_document_details("u").await.unwrap();
    assert_eq!(d.uuid, "u");
}

#[tokio::test]
async fn near_expiry_db_token_triggers_oauth_refresh() {
    let server = MockServer::start().await;
    mount_token(&server, 1).await; // refresh expected
    mount_details_ok(&server, "u", "tok-abc", 1).await;

    let pool = fresh_pool().await;
    let store = OauthTokenStore::new(pool);
    // Within the 60s leeway → considered stale.
    let near = OffsetDateTime::now_utc().unix_timestamp() + 30;
    store.upsert("preprod", "old-token", near).await.unwrap();

    let client = LhdnClient::new(cfg(&server), store);
    client.get_document_details("u").await.unwrap();
}

#[tokio::test]
async fn submit_documents_happy_path() {
    let server = MockServer::start().await;
    mount_token(&server, 1).await;

    Mock::given(method("POST"))
        .and(path("/api/v1.0/documentsubmissions"))
        .and(header("authorization", "Bearer tok-abc"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "submissionUid": "sub-1",
            "acceptedDocuments": [
                { "uuid": "u-1", "invoiceCodeNumber": "INV-1" }
            ],
            "rejectedDocuments": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = LhdnClient::new(cfg(&server), OauthTokenStore::new(fresh_pool().await));
    let docs = vec![SubmissionDocument {
        format: SubmissionFormat::Json,
        document_hash: "hash".into(),
        code_number: "INV-1".into(),
        document: "base64-doc".into(),
    }];
    let resp = client.submit_documents(&docs).await.unwrap();
    assert_eq!(resp.submission_uid, "sub-1");
    assert_eq!(resp.accepted_documents.len(), 1);
    assert_eq!(resp.accepted_documents[0].invoice_code_number, "INV-1");
    assert!(resp.rejected_documents.is_empty());
}

#[tokio::test]
async fn cancel_document_sends_correct_payload() {
    let server = MockServer::start().await;
    mount_token(&server, 1).await;

    Mock::given(method("PUT"))
        .and(path("/api/v1.0/documents/state/u/state"))
        .and(body_partial_json(
            json!({ "status": "cancelled", "reason": "wrong customer" }),
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = LhdnClient::new(cfg(&server), OauthTokenStore::new(fresh_pool().await));
    client.cancel_document("u", "wrong customer").await.unwrap();
}

#[tokio::test]
async fn validate_taxpayer_200_is_true_and_404_is_false() {
    let server = MockServer::start().await;
    mount_token(&server, 1).await;

    Mock::given(method("GET"))
        .and(path("/api/v1.0/taxpayer/validate/C123"))
        .and(query_param("idType", "BRN"))
        .and(query_param("idValue", "BRN-1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1.0/taxpayer/validate/C404"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let client = LhdnClient::new(cfg(&server), OauthTokenStore::new(fresh_pool().await));
    assert!(
        client
            .validate_taxpayer("C123", IdScheme::Brn, "BRN-1")
            .await
            .unwrap()
    );
    assert!(
        !client
            .validate_taxpayer("C404", IdScheme::Brn, "BRN-X")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn endpoint_401_maps_to_auth_error_not_transient() {
    let server = MockServer::start().await;
    mount_token(&server, 1).await;

    Mock::given(method("GET"))
        .and(path("/api/v1.0/documents/u/details"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": { "code": "Unauthorized", "message": "token rejected" }
        })))
        .mount(&server)
        .await;

    let client = LhdnClient::new(cfg(&server), OauthTokenStore::new(fresh_pool().await));
    let err = client.get_document_details("u").await.unwrap_err();
    assert!(matches!(err, LhdnError::Auth(_)), "got {err:?}");
    assert!(!err.is_transient());
}

#[tokio::test]
async fn server_5xx_classified_as_transient() {
    let server = MockServer::start().await;
    mount_token(&server, 1).await;

    Mock::given(method("GET"))
        .and(path("/api/v1.0/documents/u/details"))
        .respond_with(ResponseTemplate::new(503).set_body_string("backend down"))
        .mount(&server)
        .await;

    let client = LhdnClient::new(cfg(&server), OauthTokenStore::new(fresh_pool().await));
    let err = client.get_document_details("u").await.unwrap_err();
    match &err {
        LhdnError::Server { status, .. } => assert_eq!(*status, 503),
        other => panic!("expected Server, got {other:?}"),
    }
    assert!(err.is_transient());
}

#[tokio::test]
async fn bad_request_preserves_lhdn_error_envelope() {
    let server = MockServer::start().await;
    mount_token(&server, 1).await;

    Mock::given(method("POST"))
        .and(path("/api/v1.0/documentsubmissions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {
                "code": "DocumentInvalid",
                "message": "TIN format wrong",
                "target": "issuerTin",
                "details": [{ "code": "FormatError", "message": "expected C-prefix" }]
            }
        })))
        .mount(&server)
        .await;

    let client = LhdnClient::new(cfg(&server), OauthTokenStore::new(fresh_pool().await));
    let docs = vec![SubmissionDocument {
        format: SubmissionFormat::Json,
        document_hash: "h".into(),
        code_number: "I".into(),
        document: "d".into(),
    }];
    let err = client.submit_documents(&docs).await.unwrap_err();
    match &err {
        LhdnError::BadRequest(env) => {
            assert_eq!(env.code, "DocumentInvalid");
            assert!(env.message.contains("TIN"));
            assert_eq!(env.details.len(), 1);
        }
        other => panic!("expected BadRequest, got {other:?}"),
    }
    assert!(!err.is_transient());
}
