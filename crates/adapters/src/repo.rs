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
}

fn map_unique_violation(err: sqlx::Error) -> RepoError {
    if let sqlx::Error::Database(db) = &err {
        if db.is_unique_violation() {
            return RepoError::DuplicateRef;
        }
    }
    RepoError::Db(err)
}
