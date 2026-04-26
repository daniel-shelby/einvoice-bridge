//! SQLite-backed persistence for the LHDN OAuth token.
//!
//! Survives restarts so we don't burn an OAuth round-trip on every boot.

use sqlx::SqlitePool;

use super::oauth::CachedToken;

#[derive(Clone)]
pub struct OauthTokenStore {
    pool: SqlitePool,
}

impl OauthTokenStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn get(&self, env: &str) -> Result<Option<CachedToken>, sqlx::Error> {
        let row = sqlx::query!(
            r#"
            SELECT access_token AS "access_token!: String",
                   expires_at   AS "expires_at!: i64"
            FROM oauth_tokens
            WHERE env = ?
            "#,
            env,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| CachedToken {
            access_token: r.access_token,
            expires_at: r.expires_at,
        }))
    }

    pub async fn upsert(
        &self,
        env: &str,
        access_token: &str,
        expires_at: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO oauth_tokens (env, access_token, expires_at)
            VALUES (?, ?, ?)
            ON CONFLICT(env) DO UPDATE SET
                access_token = excluded.access_token,
                expires_at   = excluded.expires_at
            "#,
            env,
            access_token,
            expires_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
