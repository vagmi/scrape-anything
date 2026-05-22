mod config;
mod db;
mod dsl;
mod error;
mod llm;
mod routes;
mod session;
mod signals;
mod state;
mod tools;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("scrape_backend=info,tower_http=info,axum=info")))
        .init();

    let config_path = std::env::var("SCRAPE_BACKEND_CONFIG")
        .unwrap_or_else(|_| "config.toml".into());
    let cfg = config::Config::load(&config_path)
        .with_context(|| format!("loading config from {config_path}"))?;

    db::ensure_parent_dir(&cfg.storage.path).ok();
    let pool = db::open(&cfg.storage.path).await?;
    tracing::info!(path = %cfg.storage.path, "sqlite ready");

    let llm = llm::LlmClient::from_config(&cfg.llm)?;
    tracing::info!(provider = %cfg.llm.provider, model = %cfg.llm.model, "llm client ready");

    let state = state::AppState::new(cfg.clone(), pool, llm);
    let app = routes::build(state);

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "scrape-backend listening");

    axum::serve(listener, app).await?;
    Ok(())
}
