//! Skattjakt API server.

use std::net::SocketAddr;

use skattjakt_api::{router, AppState};
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // JSON logs, and never at a level that would record request bodies:
    // uploaded documents and extracted amounts must not reach the log stream.
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "skattjakt=info,tower_http=info".into()),
        )
        .init();

    let state = AppState::from_env()
        .await
        .map_err(|e| format!("could not start: {e}"))?;
    tracing::info!(
        rule_set = state.engine.version(),
        model_configured = state.model_configured,
        persistent = state.store.is_some(),
        "skattjakt starting"
    );

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "listening");

    axum::serve(listener, router(state).layer(TraceLayer::new_for_http()))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Drains in-flight analyses on SIGTERM so a rolling deploy does not cut one
/// off mid-run.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
