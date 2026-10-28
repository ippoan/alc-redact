//! alc-redact: PDF の 1 ページ目を、金額の枠を黒で塗った JPEG にして返す Cloud Run の service
//! (rust-alc-api の W8d。Refs ippoan/rust-alc-api#680)。配車系 PDF の 14 項目の抽出 (`/extract`) も持つ (W8c-2)。

pub mod app;
pub mod extract;
pub mod gemini;
pub mod redact;
