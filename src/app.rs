//! HTTP の口。`POST /redact` (PDF → 1 ページ目を黒塗りした JPEG)、`POST /extract` (PDF → 配車の
//! 14 項目の JSON) と `GET /health`。
//!
//! エラーは `{"error": "<固定コード>", "detail": "..."}`。呼び手 (notify-worker) はコードで
//! rust の状態に対応づける (Cloud Run 自体の 503 と区別するため status だけに頼らない):
//!
//! | code | status | rust の状態 |
//! |---|---|---|
//! | `bad_request` | 400 | (body が空 / `X-Self-Company` が UTF-8 の percent-encode でない) |
//! | `not_pdf` | 415 | `skipped` |
//! | `gemini_key_missing` | 503 | `skipped` |
//! | `gemini` | 502 | `failed` (接頭辞 "gemini:") |
//! | `apply` | 500 | `failed` (接頭辞 "apply:")。pdfium の描画の失敗もここ (`/redact` だけ) |
//! | `parse` | 502 | `failed` (接頭辞 "parse:")。Gemini の応答が schema の形でない (`/extract` だけ) |
//!
//! `/extract` の `gemini_key_missing` は、rust では「logistics 無しで completed」だった。
//!
//! 処理は同期で返す (投げっ放しの `tokio::spawn` はしない)。pdfium は `spawn_blocking` を await する。

use crate::extract::ExtractError;
use crate::gemini::GeminiClient;
use crate::redact::{self, RedactionBox, RenderError};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use image::RgbImage;
use std::sync::Arc;
use std::time::Instant;

/// Cloud Run の HTTP/1 の request の上限 (32 MiB) に合わせる。
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

pub struct AppState {
    /// `GEMINI_API_KEY` が無いときは None (→ 503 `gemini_key_missing`)
    pub gemini: Option<GeminiClient>,
    /// 2 段の検出 (既定 true)。false で 1 段
    pub two_stage: bool,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/redact", post(redact))
        .route("/extract", post(extract))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(Arc::new(state))
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    detail: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, detail: impl ToString) -> Self {
        Self {
            status,
            code,
            detail: detail.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.code, "detail": self.detail });
        (self.status, Json(body)).into_response()
    }
}

fn apply_error(e: impl ToString) -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "apply", e)
}

/// pdfium の同期処理を blocking のスレッドで走らせて待つ。
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, RenderError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| apply_error(format!("blocking task: {e}")))?
        .map_err(apply_error)
}

/// 空の body・`%PDF-` で始まらない body・Gemini の鍵が無い、を弾く (`/redact` と `/extract` で共通)。
fn check_pdf<'a>(state: &'a AppState, body: &[u8]) -> Result<&'a GeminiClient, ApiError> {
    if body.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "empty body",
        ));
    }
    if !body.starts_with(b"%PDF-") {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "not_pdf",
            "body does not start with %PDF-",
        ));
    }
    let Some(gemini) = state.gemini.as_ref() else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "gemini_key_missing",
            "GEMINI_API_KEY is not set",
        ));
    };
    Ok(gemini)
}

async fn redact(State(state): State<Arc<AppState>>, body: Bytes) -> Result<Response, ApiError> {
    let t_total = Instant::now();
    let gemini = check_pdf(&state, &body)?;

    // 1) 1 ページ目を描画し、Gemini に送る JPEG を作る
    let t_render = Instant::now();
    let pdf = body.clone();
    let (page, jpeg): (RgbImage, Vec<u8>) = blocking(move || {
        let img = redact::render_first_page(&pdf)?;
        let jpeg = redact::encode_jpeg(&img, redact::GEMINI_JPEG_QUALITY)?;
        Ok((img, jpeg))
    })
    .await?;
    let mut render_ms = t_render.elapsed().as_millis() as u64;

    // 2) 金額の枠を検出
    let t_llm = Instant::now();
    let detected = if state.two_stage {
        gemini.detect_amount_boxes_v2(&jpeg).await
    } else {
        gemini.detect_amount_boxes(&jpeg).await
    };
    let llm_ms = t_llm.elapsed().as_millis() as u64;
    let boxes: Vec<RedactionBox> = detected.map_err(|e| {
        tracing::warn!(stage = "llm", llm_ms, "redact_failed: {e}");
        ApiError::new(StatusCode::BAD_GATEWAY, "gemini", e)
    })?;

    // 3) 同じ画像に塗って JPEG にする
    let t_apply = Instant::now();
    let n = boxes.len();
    let out = blocking(move || redact::apply_redactions(page, &boxes))
        .await
        .inspect_err(|e| tracing::warn!(stage = "apply", "redact_failed: {}", e.detail))?;
    render_ms += t_apply.elapsed().as_millis() as u64;
    let total_ms = t_total.elapsed().as_millis() as u64;

    tracing::info!(redactions = n, llm_ms, render_ms, total_ms, "redact_ok");

    let headers = [
        (header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg")),
        (
            header::HeaderName::from_static("x-redactions-applied"),
            n.into(),
        ),
        (
            header::HeaderName::from_static("x-redact-llm-ms"),
            llm_ms.into(),
        ),
        (
            header::HeaderName::from_static("x-redact-render-ms"),
            render_ms.into(),
        ),
        (
            header::HeaderName::from_static("x-redact-total-ms"),
            total_ms.into(),
        ),
    ];
    Ok((headers, out).into_response())
}

/// 自社名のヘッダー。HTTP のヘッダーは ASCII なので、UTF-8 を percent-encode して送らせる。
const SELF_COMPANY_HEADER: &str = "x-self-company";

fn self_company(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(v) = headers.get(SELF_COMPANY_HEADER) else {
        return Ok(None);
    };
    let bad = |detail: &str| ApiError::new(StatusCode::BAD_REQUEST, "bad_request", detail);
    let raw = v.to_str().map_err(|_| bad("X-Self-Company is not ASCII"))?;
    let name = percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| bad("X-Self-Company is not percent-encoded UTF-8"))?;
    Ok(Some(name.into_owned()))
}

async fn extract(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let t_total = Instant::now();
    let gemini = check_pdf(&state, &body)?;
    let hint = self_company(&headers)?;

    let fields = gemini
        .extract_logistics(&body, hint.as_deref())
        .await
        .map_err(|e| {
            let code = match e {
                ExtractError::Gemini(_) => "gemini",
                ExtractError::Parse(_) => "parse",
            };
            tracing::warn!(code, "extract_failed: {e}");
            ApiError::new(StatusCode::BAD_GATEWAY, code, e)
        })?;
    let total_ms = t_total.elapsed().as_millis() as u64;
    tracing::info!(total_ms, "extract_ok");
    Ok(Json(serde_json::json!({ "logistics": fields })).into_response())
}
