//! `POST /redact` を router ごと通すテスト。pdfium (libpdfium.so) が要る
//! (CI と Dockerfile は /usr/lib に置く。手元は `LD_LIBRARY_PATH`)。Gemini は wiremock。

use alc_redact::app::{router, AppState};
use alc_redact::gemini::GeminiClient;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use image::ImageEncoder;
use lopdf::{dictionary, Document, Object, Stream};
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A4 1 ページに赤一色の JPEG を全面に描いた PDF (rust の redact.rs のテストと同じ形)。
fn red_pdf() -> Vec<u8> {
    let (w, h) = (40u32, 40u32);
    let red = image::RgbImage::from_pixel(w, h, image::Rgb([255, 0, 0]));
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
        .write_image(red.as_raw(), w, h, image::ExtendedColorType::Rgb8)
        .unwrap();

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let image_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => w as i64,
            "Height" => h as i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
        },
        jpeg,
    ));
    let resources_id = doc.add_object(dictionary! {
        "XObject" => dictionary! { "Im0" => image_id },
    });
    // image-space (1×1) → page-space (595×842) の CTM で全面に描く
    let content = b"q\n595 0 0 842 0 0 cm\n/Im0 Do\nQ\n".to_vec();
    let content_id = doc.add_object(Stream::new(dictionary! {}, content));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        "Contents" => content_id,
    });
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Count" => 1,
            "Kids" => vec![page_id.into()],
        }),
    );
    doc.trailer.set("Root", catalog_id);
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

fn app(gemini: Option<&MockServer>, two_stage: bool) -> Router {
    router(AppState {
        gemini: gemini.map(|s| GeminiClient::new(s.uri(), "test-model".into(), "fake-key".into())),
        two_stage,
    })
}

fn ok_text(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "candidates": [{"content": {"parts": [{"text": text}]}}]
    }))
}

async fn post_redact(app: Router, body: Vec<u8>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let req = Request::post("/redact")
        .header("content-type", "application/pdf")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

fn error_code(body: &[u8]) -> String {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap();
    v["error"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn health_is_ok() {
    let resp = app(None, true)
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn empty_body_is_bad_request() {
    let (status, _, body) = post_redact(app(None, true), vec![]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "bad_request");
}

#[tokio::test]
async fn non_pdf_is_415() {
    let (status, _, body) = post_redact(app(None, true), b"\x89PNG....".to_vec()).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(error_code(&body), "not_pdf");
}

#[tokio::test]
async fn missing_key_is_503() {
    let (status, _, body) = post_redact(app(None, true), red_pdf()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error_code(&body), "gemini_key_missing");
}

/// 2 段で全面の枠が返る → 黒い JPEG。ヘッダーに件数と所要時間。
#[tokio::test]
async fn two_stage_paints_returned_box() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_text(
            r#"{"page":1,"cells":[{"box_2d":[0,0,1000,1000],"text":"100,000円"}]}"#,
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ok_text(
            r#"{"redactions":[{"box_2d":[0,0,1000,1000],"text":"100,000円"}]}"#,
        ))
        .mount(&server)
        .await;

    let (status, headers, body) = post_redact(app(Some(&server), true), red_pdf()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/jpeg");
    assert_eq!(headers["x-redactions-applied"], "1");
    for h in ["x-redact-llm-ms", "x-redact-render-ms", "x-redact-total-ms"] {
        assert!(headers[h].to_str().unwrap().parse::<u64>().is_ok(), "{h}");
    }
    let img = image::load_from_memory(&body).unwrap().to_rgb8();
    // A4 を 300 dpi で描いた大きさ
    assert_eq!(img.dimensions(), (2479, 3508));
    let c = img.get_pixel(1200, 1700).0;
    assert!(c.iter().all(|v| *v < 30), "should be black: {c:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

/// 1 段 (REDACT_2STAGE=0 相当) で枠 0 件 → 元の赤のまま返る。
#[tokio::test]
async fn one_stage_without_boxes_returns_clean_page() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_text(r#"{"redactions":[]}"#))
        .mount(&server)
        .await;

    let (status, headers, body) = post_redact(app(Some(&server), false), red_pdf()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-redactions-applied"], "0");
    let img = image::load_from_memory(&body).unwrap().to_rgb8();
    let c = img.get_pixel(1200, 1700).0;
    assert!(
        c[0] > 200 && c[1] < 60 && c[2] < 60,
        "should stay red: {c:?}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn gemini_failure_is_502() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let (status, _, body) = post_redact(app(Some(&server), true), red_pdf()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(error_code(&body), "gemini");
    // Stage 1 の失敗 → 1 段に落ちて、それも失敗
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn invalid_box_from_gemini_is_apply_500() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_text(
            r#"{"redactions":[{"page":1,"box_2d":[500,500,100,100],"text":"x"}]}"#,
        ))
        .mount(&server)
        .await;

    let (status, _, body) = post_redact(app(Some(&server), false), red_pdf()).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error_code(&body), "apply");
}

/// `%PDF-` で始まるが壊れている → pdfium が読めず apply 500 (Gemini は呼ばない)。
#[tokio::test]
async fn broken_pdf_is_apply_500() {
    let server = MockServer::start().await;
    let (status, _, body) =
        post_redact(app(Some(&server), true), b"%PDF-1.5\ngarbage".to_vec()).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error_code(&body), "apply");
    assert!(server.received_requests().await.unwrap().is_empty());
}

async fn post_extract(
    app: Router,
    body: Vec<u8>,
    self_company: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::post("/extract").header("content-type", "application/pdf");
    if let Some(v) = self_company {
        req = req.header("x-self-company", v);
    }
    let resp = app
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// 自社名は percent-encode で届き、decode してプロンプトに入る。null の項目は返さない。
#[tokio::test]
async fn extract_returns_logistics_and_decodes_self_company() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_text(
            r#"{"loading_place":"イオン関西RDC","unloading_at":null,"contact_person":"山田"}"#,
        ))
        .mount(&server)
        .await;

    // 「テスト運輸」の UTF-8 を percent-encode したもの
    let (status, body) = post_extract(
        app(Some(&server), true),
        red_pdf(),
        Some("%E3%83%86%E3%82%B9%E3%83%88%E9%81%8B%E8%BC%B8"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({"logistics": {"loading_place": "イオン関西RDC", "contact_person": "山田"}})
    );
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(sent["contents"][0]["parts"][1]["text"]
        .as_str()
        .unwrap()
        .contains("「テスト運輸」"));
}

#[tokio::test]
async fn extract_without_self_company_uses_label_inference() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_text("{}"))
        .mount(&server)
        .await;
    let (status, body) = post_extract(app(Some(&server), true), red_pdf(), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({"logistics": {}}));
    let reqs = server.received_requests().await.unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(sent["contents"][0]["parts"][1]["text"]
        .as_str()
        .unwrap()
        .contains("## 連絡先の判定"));
}

#[tokio::test]
async fn extract_rejects_like_redact() {
    let (status, body) = post_extract(app(None, true), vec![], None).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("bad_request"))
    );
    let (status, body) = post_extract(app(None, true), b"\x89PNG".to_vec(), None).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::UNSUPPORTED_MEDIA_TYPE, Some("not_pdf"))
    );
    let (status, body) = post_extract(app(None, true), red_pdf(), None).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("gemini_key_missing"))
    );
}

#[tokio::test]
async fn extract_bad_self_company_is_400() {
    let server = MockServer::start().await;
    // %FF は UTF-8 にならない
    let (status, body) = post_extract(app(Some(&server), true), red_pdf(), Some("%FF")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "bad_request");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn extract_gemini_failure_is_502_gemini() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(402).set_body_string("payment required"))
        .mount(&server)
        .await;
    let (status, body) = post_extract(app(Some(&server), true), red_pdf(), None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "gemini");
    // やり直さない (1 回だけ)
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn extract_unparsable_text_is_502_parse() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_text("not json"))
        .mount(&server)
        .await;
    let (status, body) = post_extract(app(Some(&server), true), red_pdf(), None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "parse");
}
