use anyhow::{Context, Result};
use einvoice_adapters::{
    api::{self, ApiState},
    repo::InvoiceRepo,
};
use sqlx::sqlite::SqlitePoolOptions;
use std::net::SocketAddr;
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL must be set (see .env.example)")?;
    let bind_addr: SocketAddr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()
        .context("BIND_ADDR is not a valid socket address")?;

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await
        .context("connecting to SQLite")?;

    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .context("running migrations")?;

    let state = ApiState {
        repo: InvoiceRepo::new(pool),
    };
    let app = api::router(state);

    tracing::info!(%bind_addr, "einvoice-bridge listening");
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    axum::serve(listener, app).await.context("axum::serve")?;

    Ok(())
}
