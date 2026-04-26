use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use einvoice_adapters::{
    api::{self, ApiState},
    lhdn::{LhdnClient, LhdnConfig, LhdnEnv, OauthTokenStore},
    repo::InvoiceRepo,
    worker::Submitter,
};
use einvoice_domain::Signer;
use sqlx::sqlite::SqlitePoolOptions;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let database_url =
        std::env::var("DATABASE_URL").context("DATABASE_URL must be set (see .env.example)")?;
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

    let repo = InvoiceRepo::new(pool.clone());
    let api = api::router(ApiState { repo: repo.clone() });

    // Submitter is optional in dev: LHDN_OFFLINE=true skips loading the
    // .p12 and OAuth credentials so `cargo run` works without preprod
    // creds. In production this is unset and we fail-fast on missing
    // config or an undecryptable certificate.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let worker_handle = if env_bool("LHDN_OFFLINE") {
        warn!("LHDN_OFFLINE=true — not spawning submitter; HTTP API only");
        None
    } else {
        let env = parse_lhdn_env(&env_required("LHDN_ENV")?)?;
        let client_id = env_required("LHDN_CLIENT_ID")?;
        let client_secret = env_required("LHDN_CLIENT_SECRET")?;
        let p12_path = env_required("LHDN_P12_PATH")?;
        let p12_pass = env_required("LHDN_P12_PASSWORD")?;

        let p12_bytes = std::fs::read(&p12_path)
            .with_context(|| format!("reading LHDN_P12_PATH={p12_path}"))?;
        let signer =
            Arc::new(Signer::from_p12(&p12_bytes, &p12_pass).context("decrypting .p12")?);

        let config = LhdnConfig::for_env(env, client_id, client_secret);
        let lhdn = LhdnClient::new(config, OauthTokenStore::new(pool.clone()));
        let submitter = Submitter::new(repo, lhdn, signer);

        info!(?env, "spawning submitter worker");
        Some(tokio::spawn(submitter.run(shutdown_rx.clone())))
    };

    info!(%bind_addr, "einvoice-bridge listening");
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;

    let shutdown_for_server = shutdown_tx.clone();
    axum::serve(listener, api)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            info!("ctrl-c received; initiating shutdown");
            let _ = shutdown_for_server.send(true);
        })
        .await
        .context("axum::serve")?;

    if let Some(handle) = worker_handle {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!(error = %err, "submitter exited with error"),
            Err(err) => warn!(error = %err, "submitter task panicked"),
        }
    }

    Ok(())
}

fn env_required(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} must be set"))
}

fn env_bool(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

fn parse_lhdn_env(s: &str) -> Result<LhdnEnv> {
    match s.trim().to_ascii_lowercase().as_str() {
        "preprod" => Ok(LhdnEnv::Preprod),
        "prod" => Ok(LhdnEnv::Prod),
        other => bail!("LHDN_ENV must be 'preprod' or 'prod', got {other:?}"),
    }
}
