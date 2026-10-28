//! 環境変数:
//! - `PORT` (既定 8080。Cloud Run が入れる)
//! - `GEMINI_API_KEY` (Secret Manager の `gemini-api-key` を secretKeyRef で。無ければ /redact は 503)
//! - `GEMINI_ENDPOINT`・`GEMINI_MODEL` (既定は `gemini::DEFAULT_*`。手元の確認で差し替える用)
//! - `REDACT_2STAGE` (既定は 2 段。`0` で 1 段)
//! - `RUST_LOG` (既定 info)

use alc_redact::app::{router, AppState};
use alc_redact::gemini::{self, GeminiClient};

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let gemini = env_nonempty("GEMINI_API_KEY").map(|key| {
        GeminiClient::new(
            env_nonempty("GEMINI_ENDPOINT").unwrap_or_else(|| gemini::DEFAULT_ENDPOINT.into()),
            env_nonempty("GEMINI_MODEL").unwrap_or_else(|| gemini::DEFAULT_MODEL.into()),
            key,
        )
    });
    if gemini.is_none() {
        tracing::warn!("GEMINI_API_KEY is not set; POST /redact returns 503 gemini_key_missing");
    }
    let two_stage = env_nonempty("REDACT_2STAGE").as_deref() != Some("0");

    let port = env_nonempty("PORT").unwrap_or_else(|| "8080".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("bind");
    tracing::info!(port = %port, two_stage, "alc-redact listening");
    axum::serve(listener, router(AppState { gemini, two_stage }))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("serve");
}

/// Cloud Run は SIGTERM で止める。処理中の request を返し切ってから終わる。
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
