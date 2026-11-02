use std::net::SocketAddr;
use std::sync::Arc;

pub mod byok;
pub mod chat;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod limits;
pub mod model_sync;
pub mod routes;
pub mod search;

pub use config::Config;
pub use routes::{http_client, router, AppState};

pub async fn serve(cfg: Config) -> Result<(), Box<dyn std::error::Error>> {
    let pool = db::connect(&cfg.database_url).await?;
    db::prepare(&pool, &cfg).await?;
    let bind = cfg.bind.clone();
    let state = AppState::new(pool, Arc::new(cfg), http_client()?);

    let sync_state = state.clone();
    tokio::spawn(async move {
        model_sync::run_periodic_sync(sync_state, std::time::Duration::from_secs(300)).await;
    });

    let addr: SocketAddr = bind.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "varynth-api listening");
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
