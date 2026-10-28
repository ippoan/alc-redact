//! alc-redact: PDF の 1 ページ目を、金額の枠を黒で塗った JPEG にして返す Cloud Run の service
//! (rust-alc-api の W8d。Refs ippoan/rust-alc-api#680)。

pub mod app;
pub mod gemini;
pub mod redact;
