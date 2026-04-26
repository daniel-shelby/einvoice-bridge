//! SQLite repository for the invoice lifecycle.
//!
//! All SQL queries are checked at compile time by `sqlx::query!`. The
//! offline metadata in `.sqlx/` is what makes this work in CI without a
//! live database.

use serde::Serialize;
use sqlx::SqlitePool;
use time::OffsetDateTime;
use uuid::Uuid;

/// Subset of the `invoices` row that the API surfaces. Bulk fields
/// (raw payload, ubl_xml, signature) stay in the DB.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct InvoiceRow {
    pub id: String,
    pub invoice_ref: String,
    pub lhdn_status: String,
    pub lhdn_uuid: Option<String>,
    pub qr_url: Option<String>,
    pub error_json: Option<String>,
    pub attempts: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("invoice_ref already exists")]
    DuplicateRef,
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

#[derive(Clone)]
pub struct InvoiceRepo {
    pool: SqlitePool,
}

impl InvoiceRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Insert a fresh `Pending` invoice and enqueue a `submit` outbox
    /// event in the same transaction. Returns the inserted row.
    ///
    /// On a unique-constraint violation against `invoice_ref`, returns
    /// `RepoError::DuplicateRef` so the API can map it to a 409.
    pub async fn create_pending(
        &self,
        invoice_ref: &str,
        payload_json: &str,
    ) -> Result<InvoiceRow, RepoError> {
        let id = Uuid::now_v7().to_string();
        let now = OffsetDateTime::now_utc().unix_timestamp();

        let mut tx = self.pool.begin().await?;

        let row = sqlx::query_as!(
            InvoiceRow,
            r#"
            INSERT INTO invoices (
                id, invoice_ref, payload_json, lhdn_status, attempts,
                created_at, updated_at, next_attempt_at
            ) VALUES (?, ?, ?, 'Pending', 0, ?, ?, ?)
            RETURNING
                id            AS "id!: String",
                invoice_ref   AS "invoice_ref!: String",
                lhdn_status   AS "lhdn_status!: String",
                lhdn_uuid     AS "lhdn_uuid: String",
                qr_url        AS "qr_url: String",
                error_json    AS "error_json: String",
                attempts      AS "attempts!: i64",
                created_at    AS "created_at!: i64",
                updated_at    AS "updated_at!: i64"
            "#,
            id,
            invoice_ref,
            payload_json,
            now,
            now,
            now,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(map_unique_violation)?;

        sqlx::query!(
            "INSERT INTO outbox_events (invoice_id, kind, available_at) VALUES (?, 'submit', ?)",
            id,
            now,
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(row)
    }

    pub async fn find_by_ref(&self, invoice_ref: &str) -> Result<Option<InvoiceRow>, RepoError> {
        let row = sqlx::query_as!(
            InvoiceRow,
            r#"
            SELECT
                id            AS "id!: String",
                invoice_ref   AS "invoice_ref!: String",
                lhdn_status   AS "lhdn_status!: String",
                lhdn_uuid     AS "lhdn_uuid: String",
                qr_url        AS "qr_url: String",
                error_json    AS "error_json: String",
                attempts      AS "attempts!: i64",
                created_at    AS "created_at!: i64",
                updated_at    AS "updated_at!: i64"
            FROM invoices
            WHERE invoice_ref = ?
            "#,
            invoice_ref,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    // ---------------------------------------------------------------------
    // Submitter-worker queries.
    //
    // These deliberately don't go through the public API; they're the
    // worker's local lifecycle. The worker pulls due events, loads the row
    // (with payload_json), and either marks the invoice submitted/failed
    // or reschedules with a backoff.
    // ---------------------------------------------------------------------

    /// Outbox events of kind `submit` whose `available_at` has elapsed.
    pub async fn due_submit_events(
        &self,
        now: i64,
        limit: i64,
    ) -> Result<Vec<DueSubmitEvent>, RepoError> {
        let rows = sqlx::query_as!(
            DueSubmitEvent,
            r#"
            SELECT
                id          AS "outbox_id!: i64",
                invoice_id  AS "invoice_id!: String"
            FROM outbox_events
            WHERE kind = 'submit' AND available_at <= ?
            ORDER BY available_at
            LIMIT ?
            "#,
            now,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Load the fields the submitter actually needs.
    pub async fn load_for_submit(
        &self,
        id: &str,
    ) -> Result<Option<InvoiceForSubmit>, RepoError> {
        let row = sqlx::query_as!(
            InvoiceForSubmit,
            r#"
            SELECT
                id            AS "id!: String",
                invoice_ref   AS "invoice_ref!: String",
                payload_json  AS "payload_json!: String",
                attempts      AS "attempts!: i64"
            FROM invoices
            WHERE id = ?
            "#,
            id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Atomically: mark the invoice as `Submitted`, persist the signed
    /// UBL + signature + doc digest, and remove the outbox event.
    pub async fn complete_submission(
        &self,
        invoice_id: &str,
        outbox_id: i64,
        completion: SubmissionCompletion<'_>,
    ) -> Result<(), RepoError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            r#"
            UPDATE invoices
            SET lhdn_status         = 'Submitted',
                lhdn_submission_uid = ?,
                lhdn_uuid           = ?,
                signature           = ?,
                ubl_xml             = ?,
                doc_digest          = ?,
                error_json          = NULL,
                submitted_at        = ?,
                next_attempt_at     = NULL,
                updated_at          = ?
            WHERE id = ?
            "#,
            completion.submission_uid,
            completion.lhdn_uuid,
            completion.signature_b64,
            completion.signed_document_utf8,
            completion.document_hash_b64,
            now,
            now,
            invoice_id,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!("DELETE FROM outbox_events WHERE id = ?", outbox_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Atomically: mark the invoice as `Failed`, persist the final
    /// attempts count + error JSON, and remove the outbox event.
    /// Caller supplies the final `attempts` value — pass the existing
    /// invoice.attempts when the failure is upstream of LHDN (e.g. local
    /// validation), or an incremented value when LHDN itself rejected.
    pub async fn fail_permanently(
        &self,
        invoice_id: &str,
        outbox_id: i64,
        attempts: i64,
        error_json: &str,
    ) -> Result<(), RepoError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            r#"
            UPDATE invoices
            SET lhdn_status     = 'Failed',
                attempts        = ?,
                error_json      = ?,
                next_attempt_at = NULL,
                updated_at      = ?
            WHERE id = ?
            "#,
            attempts,
            error_json,
            now,
            invoice_id,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!("DELETE FROM outbox_events WHERE id = ?", outbox_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Reschedule a transient failure: bump attempts, update
    /// next_attempt_at + the outbox event's `available_at`, and stash
    /// the latest error for ops visibility.
    ///
    /// `error_json` is the structured body that lands in
    /// `invoices.error_json` (matching the shape `fail_permanently`
    /// uses). `last_error` is a short human-readable string written to
    /// `outbox_events.last_error` for queue debugging.
    pub async fn reschedule(
        &self,
        invoice_id: &str,
        outbox_id: i64,
        new_attempts: i64,
        next_attempt_at: i64,
        error_json: &str,
        last_error: &str,
    ) -> Result<(), RepoError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            r#"
            UPDATE invoices
            SET attempts        = ?,
                error_json      = ?,
                next_attempt_at = ?,
                updated_at      = ?
            WHERE id = ?
            "#,
            new_attempts,
            error_json,
            next_attempt_at,
            now,
            invoice_id,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE outbox_events SET available_at = ?, attempts = ?, last_error = ? WHERE id = ?",
            next_attempt_at,
            new_attempts,
            last_error,
            outbox_id,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct DueSubmitEvent {
    pub outbox_id: i64,
    pub invoice_id: String,
}

#[derive(Debug, Clone)]
pub struct InvoiceForSubmit {
    pub id: String,
    pub invoice_ref: String,
    pub payload_json: String,
    pub attempts: i64,
}

/// Inputs for the happy-path completion update. Borrowed so the worker
/// doesn't have to clone the (potentially large) signed document.
#[derive(Debug, Clone, Copy)]
pub struct SubmissionCompletion<'a> {
    pub submission_uid: &'a str,
    pub lhdn_uuid: &'a str,
    pub signature_b64: &'a str,
    /// The signed canonical UBL document as UTF-8 — stored in `ubl_xml`
    /// (the column name is a misnomer; we use JSON UBL).
    pub signed_document_utf8: &'a str,
    pub document_hash_b64: &'a str,
}

fn map_unique_violation(err: sqlx::Error) -> RepoError {
    if let sqlx::Error::Database(db) = &err {
        if db.is_unique_violation() {
            return RepoError::DuplicateRef;
        }
    }
    RepoError::Db(err)
}
