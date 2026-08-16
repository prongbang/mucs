mod api;
mod config;
mod crypto;
mod db;
mod error;
mod storage;
mod web;
mod worker;

use std::sync::Arc;

use anyhow::Context;
use axum::extract::DefaultBodyLimit;
use tokio::sync::Notify;
use tower_http::trace::TraceLayer;

use crate::api::AppState;
use crate::config::Config;
use crate::db::Store;
use crate::storage::Storage;
use crate::worker::Worker;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,demucs_service=debug".into()),
        )
        .init();

    let cfg = Arc::new(Config::from_env()?);
    tokio::fs::create_dir_all(&cfg.work_dir).await.ok();
    if let Some(dir) = cfg
        .database_url
        .strip_prefix("sqlite://")
        .and_then(|p| std::path::Path::new(p.split('?').next().unwrap_or(p)).parent())
    {
        tokio::fs::create_dir_all(dir).await.ok();
    }

    let storage = Storage::new(&cfg);
    storage
        .ensure_bucket()
        .await
        .context("cannot reach RustFS — check S3_ENDPOINT and credentials")?;

    let store = Store::connect(&cfg.database_url).await?;
    let requeued = store.requeue_orphans().await?;
    if requeued > 0 {
        tracing::warn!(count = requeued, "re-queued jobs orphaned by a previous crash");
    }

    let notify = Arc::new(Notify::new());

    // One worker. Demucs on CPU uses every core it can get, so running two in
    // parallel on a 4-core box halves the throughput of each without helping.
    tokio::spawn(
        Worker {
            cfg: cfg.clone(),
            store: store.clone(),
            storage: storage.clone(),
            notify: notify.clone(),
        }
        .run(),
    );

    let state = AppState {
        cfg: cfg.clone(),
        store,
        storage,
        notify,
    };

    let app = api::router(state)
        .layer(DefaultBodyLimit::max(cfg.max_upload_bytes))
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr)
        .await
        .with_context(|| format!("binding {}", cfg.bind_addr))?;

    tracing::info!(addr = %cfg.bind_addr, model = %cfg.demucs_model, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };

    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };

    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }

    tracing::info!("shutting down");
}
