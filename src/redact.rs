//! PDF の 1 ページ目を pdfium で描画し、金額の枠を黒で塗って JPEG にする。
//!
//! rust-alc-api の `crates/alc-notify/src/redact.rs` から移植 (Refs ippoan/rust-alc-api#680)。
//! rust では Gemini に送る JPEG と塗る画像を別々に描画していた (2 段の検出では最大 3 回) が、
//! ここでは 1 回だけ描画し、同じ画像を Gemini への入力にも塗りにも使う。座標系が同じなので
//! Gemini が返した枠は必ず塗る位置と揃う (rust がそうしていた理由と同じ)。
//!
//! FAX 由来の多層 PDF (JPEG + CCITT の ImageMask + ロゴ) も、pdfium が全レイヤーを合成した
//! 最終の画素に塗るので確実に隠れる。出力は PDF ではなく JPEG 1 枚 (配信先は画像を見せるだけ)。

use image::{ImageEncoder, RgbImage};
use std::sync::{Mutex, OnceLock};

/// 72 dpi の PDF の点を 300 dpi の画素にする倍率 (rust と同じ)。
const RENDER_SCALE: f32 = 300.0 / 72.0;
/// Gemini に送る JPEG の品質 (rust と同じ)。
pub const GEMINI_JPEG_QUALITY: u8 = 90;
/// 返す JPEG の品質 (rust と同じ)。
pub const OUTPUT_JPEG_QUALITY: u8 = 85;

/// Gemini が返す金額の枠 1 件。
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct RedactionBox {
    pub page: usize,
    /// [ymin, xmin, ymax, xmax] in 0..=1000, 左上原点
    pub box_2d: [f32; 4],
    pub text: String,
}

#[derive(thiserror::Error, Debug)]
pub enum RenderError {
    #[error("pdfium: {0}")]
    Pdfium(String),
    #[error("pdf has no page")]
    NoPage,
    #[error("page {0} not found in pdf")]
    PageNotFound(usize),
    #[error("invalid bbox in redaction: {0:?}")]
    InvalidBox([f32; 4]),
    #[error("image encode: {0}")]
    Image(#[from] image::ImageError),
}

/// `pdfium-render::Pdfium` は内部に `Box<dyn PdfiumLibraryBindings>` を持ち、自動では
/// `Send + Sync` にならないので `static` に置けない。実体は dlopen した libpdfium.so で、
/// ハンドルと関数ポインタはプロセス全体で有効。Mutex で排他するので並行呼び出しは起きない。
struct PdfiumGuard(pdfium_render::prelude::Pdfium);
// SAFETY: 上のとおり。PDFium 本体のスレッドアンセーフ性は Mutex で守る。
unsafe impl Send for PdfiumGuard {}
unsafe impl Sync for PdfiumGuard {}

/// 共有の Pdfium。最初の呼び出しで `libpdfium.so` を dlopen する
/// (image では `/usr/lib/libpdfium.so`、手元では `LD_LIBRARY_PATH`)。
fn pdfium_locked() -> Result<&'static Mutex<PdfiumGuard>, RenderError> {
    use pdfium_render::prelude::Pdfium;
    static INSTANCE: OnceLock<Result<Mutex<PdfiumGuard>, String>> = OnceLock::new();
    INSTANCE
        .get_or_init(|| {
            Pdfium::bind_to_system_library()
                .map(|b| Mutex::new(PdfiumGuard(Pdfium::new(b))))
                .map_err(|e| format!("bind_to_system_library: {e}"))
        })
        .as_ref()
        .map_err(|e| RenderError::Pdfium(e.clone()))
}

/// PDF の 1 ページ目を 300 dpi で描画する。pdfium は同期で重いので、呼び手は
/// `tokio::task::spawn_blocking` の中で呼ぶ。
pub fn render_first_page(pdf_bytes: &[u8]) -> Result<RgbImage, RenderError> {
    use pdfium_render::prelude::PdfRenderConfig;
    let pdfium = pdfium_locked()?
        .lock()
        .map_err(|e| RenderError::Pdfium(format!("mutex poisoned: {e}")))?;
    let document = pdfium
        .0
        .load_pdf_from_byte_slice(pdf_bytes, None)
        .map_err(|e| RenderError::Pdfium(format!("load_pdf: {e}")))?;
    let pages = document.pages();
    if pages.is_empty() {
        return Err(RenderError::NoPage);
    }
    let page = pages
        .get(0)
        .map_err(|e| RenderError::Pdfium(format!("get page 0: {e}")))?;
    let bitmap = page
        .render_with_config(&PdfRenderConfig::new().scale_page_by_factor(RENDER_SCALE))
        .map_err(|e| RenderError::Pdfium(format!("render p1: {e}")))?;
    Ok(bitmap.as_image().into_rgb8())
}

pub fn encode_jpeg(img: &RgbImage, quality: u8) -> Result<Vec<u8>, RenderError> {
    let (w, h) = img.dimensions();
    let mut jpeg = Vec::with_capacity((w as usize * h as usize) / 8);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality).write_image(
        img.as_raw(),
        w,
        h,
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(jpeg)
}

/// 枠の検証。1 ページ目以外 (多ページは未対応) と、0..=1000 の外・大小が逆の枠を弾く。
pub fn validate_boxes(redactions: &[RedactionBox]) -> Result<(), RenderError> {
    for r in redactions {
        if r.page != 1 {
            return Err(RenderError::PageNotFound(r.page));
        }
        let [ymin, xmin, ymax, xmax] = r.box_2d;
        let in_range = |v: f32| (0.0..=1000.0).contains(&v);
        if !(in_range(ymin) && in_range(xmin) && in_range(ymax) && in_range(xmax))
            || ymin >= ymax
            || xmin >= xmax
        {
            return Err(RenderError::InvalidBox(r.box_2d));
        }
    }
    Ok(())
}

/// 検証済みの枠を画素単位で黒く塗る。
pub fn paint_boxes(img: &mut RgbImage, redactions: &[RedactionBox]) {
    let (img_w, img_h) = img.dimensions();
    for r in redactions {
        let [ymin, xmin, ymax, xmax] = r.box_2d;
        let px = ((xmin / 1000.0) * img_w as f32).round().max(0.0) as u32;
        let py = ((ymin / 1000.0) * img_h as f32).round().max(0.0) as u32;
        let pw = ((xmax - xmin) / 1000.0 * img_w as f32).round().max(0.0) as u32;
        let ph = ((ymax - ymin) / 1000.0 * img_h as f32).round().max(0.0) as u32;
        let x_end = px.saturating_add(pw).min(img_w);
        let y_end = py.saturating_add(ph).min(img_h);
        for y in py.min(img_h)..y_end {
            for x in px.min(img_w)..x_end {
                img.put_pixel(x, y, image::Rgb([0, 0, 0]));
            }
        }
    }
}

/// 検証 → 塗る → JPEG (品質 85)。
pub fn apply_redactions(
    mut img: RgbImage,
    redactions: &[RedactionBox],
) -> Result<Vec<u8>, RenderError> {
    validate_boxes(redactions)?;
    paint_boxes(&mut img, redactions);
    encode_jpeg(&img, OUTPUT_JPEG_QUALITY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rb(page: usize, box_2d: [f32; 4]) -> RedactionBox {
        RedactionBox {
            page,
            box_2d,
            text: "x".into(),
        }
    }

    fn white(w: u32, h: u32) -> RgbImage {
        RgbImage::from_pixel(w, h, image::Rgb([255, 255, 255]))
    }

    #[test]
    fn redaction_box_serde_roundtrip() {
        let r = RedactionBox {
            page: 1,
            box_2d: [100.0, 200.0, 300.0, 400.0],
            text: "160,000円".into(),
        };
        let back: RedactionBox = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn validate_rejects_reversed_box() {
        let err = validate_boxes(&[rb(1, [500.0, 500.0, 100.0, 100.0])]).unwrap_err();
        assert!(matches!(err, RenderError::InvalidBox(_)));
    }

    #[test]
    fn validate_rejects_out_of_range_box() {
        let err = validate_boxes(&[rb(1, [0.0, 0.0, 1000.5, 10.0])]).unwrap_err();
        assert!(matches!(err, RenderError::InvalidBox(_)));
    }

    #[test]
    fn validate_rejects_other_pages() {
        assert!(matches!(
            validate_boxes(&[rb(99, [1.0, 1.0, 2.0, 2.0])]).unwrap_err(),
            RenderError::PageNotFound(99)
        ));
        assert!(matches!(
            validate_boxes(&[rb(0, [1.0, 1.0, 2.0, 2.0])]).unwrap_err(),
            RenderError::PageNotFound(0)
        ));
    }

    #[test]
    fn paint_blackens_only_the_box() {
        let mut img = white(100, 100);
        // 左上 1/4 だけ
        paint_boxes(&mut img, &[rb(1, [0.0, 0.0, 500.0, 500.0])]);
        assert_eq!(img.get_pixel(10, 10).0, [0, 0, 0]);
        assert_eq!(img.get_pixel(49, 49).0, [0, 0, 0]);
        assert_eq!(img.get_pixel(50, 50).0, [255, 255, 255]);
        assert_eq!(img.get_pixel(90, 10).0, [255, 255, 255]);
    }

    #[test]
    fn apply_full_box_returns_black_jpeg() {
        let out = apply_redactions(white(40, 40), &[rb(1, [0.0, 0.0, 1000.0, 1000.0])]).unwrap();
        assert_eq!(&out[0..3], &[0xFF, 0xD8, 0xFF], "expected JPEG SOI marker");
        let img = image::load_from_memory(&out).unwrap().to_rgb8();
        let c = img.get_pixel(20, 20).0;
        assert!(c.iter().all(|v| *v < 30), "center should be black: {c:?}");
    }

    #[test]
    fn apply_rejects_invalid_box_before_painting() {
        let err = apply_redactions(white(10, 10), &[rb(1, [5.0, 5.0, 5.0, 5.0])]).unwrap_err();
        assert!(matches!(err, RenderError::InvalidBox(_)));
    }
}
