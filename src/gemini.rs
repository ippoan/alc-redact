//! Gemini による金額の枠の検出 (rust-alc-api の `crates/alc-notify/src/redact.rs` から移植。
//! Refs ippoan/rust-alc-api#680)。
//!
//! 入力は `redact::render_first_page` で描画した 1 ページ目の JPEG。同じ画像に塗るので、
//! 返った枠は塗る位置と揃う。
//!
//! - 1 段 (`detect_amount_boxes`): 画像から「N円」の文字列の枠を直接返させる
//! - 2 段 (`detect_amount_boxes_v2`): Stage 1 で全セルを列挙 (幾何 + OCR、金額判定なし)、
//!   Stage 2 で画像なしにその JSON から金額のセルだけを選ばせる。表が密なとき 1 段だと
//!   ラベル列と値列の行の対応を誤る (3164 で消費税が 1 行下にずれた) のを、構造的に防ぐ。
//!   どちらかの段が失敗するか Stage 1 が 0 セルなら 1 段に落ちる
//!
//! `generationConfig` は必ず `responseMimeType` と `responseSchema` を両方入れる
//! (mime だけだと markdown の囲みや前置きの文で parse に失敗していた)。
//! API キーは URL ではなく `x-goog-api-key` ヘッダーで送り、エラーからも URL を外す。

use crate::redact::RedactionBox;
use base64::Engine;
use std::time::Duration;

pub const DEFAULT_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";
/// gemini-2.0-flash は 2026 年に新規向けの提供が終わった (404)。rust と同じ版。
pub const DEFAULT_MODEL: &str = "gemini-3.1-flash-lite-preview";
/// 1 回の呼び出しの上限 (Cloud Run の timeout 300s の内側に 2 段 + 1 段の fallback が収まる)。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

/// 1 段のプロンプト: 「円」を含む文字列のタイトな枠を返させる (セル全体だと隣の行に
/// 枠を置く誤りが出た。3164_001.pdf の 11,500円 が担当者の行に飛ぶ等)。
const REDACT_PROMPT: &str = r#"この画像は業務帳票 (1 ページ) の rasterize 画像です。
**画像中に印字または手書きされた「N円」表記の金額** を全て検出し、その文字列の
バウンディングボックスをそのまま返してください。表のセル全体ではなく、文字列の
タイトな矩形 (上下左右に最小限のマージン付き) で十分です。

対象パターン (これらに合致するテキストは全て検出):
- 「123,456円」「12,345円(税別)」「12,345円(税込)」
- 「￥123,456」「JPY 12,345」
- 数字とカンマと「円」「￥」「JPY」がセットで現れた箇所すべて

検出しないもの:
- 「3,200 kg」「9PL」「10t」「10:00」など「円/￥/JPY」が無い数字
- 氏名・電話番号・FAX 番号・郵便番号・車両ナンバー・住所
- 日付 (例: "2026/06/30")

出力形式 (Structured Output 強制、ストリーミング外文字は出さない):
{
  "redactions": [
    {
      "page": 1,
      "box_2d": [ymin, xmin, ymax, xmax],
      "text": "115,000円",
      "cell_label": "運賃 (任意)"
    }
  ]
}

ルール:
- box_2d は 0-1000 で正規化された [ymin, xmin, ymax, xmax] (左上原点、Gemini 標準形式)
- page は常に 1
- **bbox は必ず実際の文字列の上にあること** (空白セルを返さない)
- text に「円/￥/JPY」のいずれかが含まれること
- 該当が無ければ {"redactions": []} を返す"#;

/// Stage 1 のプロンプト: 表内の全セルを列挙させる。金額判定はしない。
const STAGE1_PROMPT: &str = r#"この PDF は表形式の業務帳票 (FAX スキャン画像) です。
表内の **全セル** を列挙してください。金額判定や種類分けは一切しないこと。

【セルの定義】
- セル = 罫線で囲まれた **最小** の矩形領域 (1 行 × 1 列)
- 1 つのセルに複数行のテキストが入っている場合は、**水平罫線を必ず疑う**
- 罫線が薄い / かすれていても、テキストが 2 行以上ある領域は **別セル** として分割する
- 1 セルの text に「\n (改行)」が入ったら粒度が粗すぎる → 行ごとに分割すること

【ラベル列と値列の完全分離 (重要)】
- 業務帳票では「ラベル文字列だけが入ったセル」(例: 運賃 / 消費税 / 合計 / 合計金額 /
  合算 / 総額 / 請求額 / 支払額 / 支払運賃 / 高速代 / 搬出料 / 燃料サーチャージ /
  駐車代 / 待機料 / 通関料 / 付帯作業料 / 入金予定日) とその隣の「金額が入った値セル」
  (例: 100,000円 / 11,500円 / 127,000円) は **必ず別の box_2d** として返すこと。
  1 つの box_2d にラベル文字 + 金額を同時に含めてはならない。
- 縦罫線が薄くて視覚的に連続して見えても、**列境界で必ず分割** する。
- ラベルセルの xmax と値セルの xmin の関係は **xmax_label <= xmin_value** を
  必ず満たす (両 bbox が水平方向に 1 ピクセルも重ならない)。
- 「N円」を含む text は **単独のセル** として返し、隣のラベル文字を絶対に
  同じ box_2d に含めないこと。
- これは特に消費税行 ("消費税" + "11,500円") のような **ラベルが短くて
  値セルが横に長い** 行で誤検出が発生しやすい。同じ row でも必ず 2 つの cell として返す。

出力形式 (これ以外の文字を一切出力しない):
{
  "page": 1,
  "title": "運賃明細",
  "cells": [
    {"box_2d": [120, 50, 180, 240], "text": "運賃"},
    {"box_2d": [120, 240, 180, 480], "text": "100,000円"},
    {"box_2d": [180, 50, 240, 240], "text": "消費税"},
    {"box_2d": [180, 240, 240, 480], "text": "11,500円"}
  ]
}
↑ この例で xmax_label=240 == xmin_value=240 となっており、ラベルと値の bbox は
  完全に分離している (重複ゼロ)。実際の出力でも必ずこの構造を守ること。

ルール:
- box_2d は 0-1000 で正規化された [ymin, xmin, ymax, xmax] (左上原点)
- page は 1-origin
- title は表の見出し (なければ最も近いテキスト、推測でよい)
- text はセル内の文字列を OCR したそのまま (空セルは text="")
- **text に改行 (\n) を含めてはならない**。複数行に見えるなら別セル
- **金額判定はしない**: ラベルセル ("運賃" / "消費税" / "合計") も値セル
  ("100,000円" / "11,500円") もすべて等しく列挙する
- **ラベルと値の bbox 重複禁止**: text に「円」を含むセルは、同じ row の
  隣接ラベルセル (運賃 / 消費税 / 合計額 等) の bbox と水平方向に重なって
  はならない。重なって見える場合は罫線位置を疑い、必ず column 境界で
  bbox を切り直すこと (xmax_label <= xmin_value)。
- 全セルを順番に左上から右下へ (行優先)
- 罫線をたどって表全体を網羅 (取りこぼし禁止)
- **「N円」パターンは絶対に取りこぼさない**: 表の右下や 1 行だけの小さい領域でも、
  「数字 + 円」を含むセルは必ず単独の box_2d として返す
  (例: 「合計金額: 127,000円」が 1 行で書かれていても、「合計金額」と「127,000円」を
  別セルとして 2 つ列挙する)"#;

/// Stage 2 のプロンプト。Stage 1 の JSON を埋め込む。
fn stage2_prompt(cells_json: &str) -> String {
    format!(
        r#"以下は業務帳票 PDF から抽出した全セルのリストです。この中から
「金額 (円) が記入されている値セル」だけを抽出してください。

入力 JSON:
{cells_json}

抽出対象:
- text に「円」を含むセル (例: "100,000円", "￥11,500", "JPY 5,000")
- 「N円(税抜)」「N円(税込)」など税表記付きも対象

抽出しないもの:
- ラベルセル ("運賃" / "消費税" / "合計" / "支払額" などの文字列のみ)
- 単位付き数値だが金額でないもの ("3,200kg" / "9PL" / "10t" / "10:00")
- 氏名・電話・FAX・郵便番号・車番・住所

出力形式 (これ以外の文字を一切出力しない):
{{
  "redactions": [
    {{"box_2d": [ymin, xmin, ymax, xmax], "text": "100,000円"}},
    {{"box_2d": [ymin, xmin, ymax, xmax], "text": "11,500円"}}
  ]
}}

box_2d は入力 JSON のまま使うこと。座標を変更してはならない。"#
    )
}

/// 4 要素の座標の配列 (3 つの schema で共通)。
fn box_2d_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "ARRAY",
        "items": { "type": "NUMBER" },
        "minItems": 4,
        "maxItems": 4
    })
}

/// 1 段: `{redactions: [{page, box_2d[4], text, cell_label?}]}`
fn redact_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "OBJECT",
        "properties": {
            "redactions": {
                "type": "ARRAY",
                "items": {
                    "type": "OBJECT",
                    "properties": {
                        "page": { "type": "INTEGER" },
                        "box_2d": box_2d_schema(),
                        "text": { "type": "STRING" },
                        "cell_label": { "type": "STRING", "nullable": true }
                    },
                    "required": ["page", "box_2d", "text"],
                    "propertyOrdering": ["page", "box_2d", "text", "cell_label"]
                }
            }
        },
        "required": ["redactions"]
    })
}

/// Stage 1: `{page, title?, cells: [{box_2d[4], text}]}` (単一ページの形に固定する)
fn stage1_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "OBJECT",
        "properties": {
            "page": { "type": "INTEGER" },
            "title": { "type": "STRING", "nullable": true },
            "cells": {
                "type": "ARRAY",
                "items": {
                    "type": "OBJECT",
                    "properties": {
                        "box_2d": box_2d_schema(),
                        "text": { "type": "STRING" }
                    },
                    "required": ["box_2d", "text"],
                    "propertyOrdering": ["box_2d", "text"]
                }
            }
        },
        "required": ["page", "cells"],
        "propertyOrdering": ["page", "title", "cells"]
    })
}

/// Stage 2: `{redactions: [{box_2d[4], text}]}` (page は呼び手が Stage 1 から補う)
fn stage2_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "OBJECT",
        "properties": {
            "redactions": {
                "type": "ARRAY",
                "items": {
                    "type": "OBJECT",
                    "properties": {
                        "box_2d": box_2d_schema(),
                        "text": { "type": "STRING" }
                    },
                    "required": ["box_2d", "text"],
                    "propertyOrdering": ["box_2d", "text"]
                }
            }
        },
        "required": ["redactions"]
    })
}

#[derive(Debug, serde::Deserialize)]
struct RedactionList {
    redactions: Vec<RedactionBox>,
}

/// Stage 1 が返すセル 1 件 (幾何 + OCR)
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct CellBox {
    /// [ymin, xmin, ymax, xmax] in 0..=1000, 左上原点
    pub box_2d: [f32; 4],
    pub text: String,
}

/// Stage 1 が返す 1 ページ分のセル
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct PageCells {
    pub page: usize,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub cells: Vec<CellBox>,
}

/// Stage 1 の応答を単一ページ / 複数 tables の両方の形で受ける
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum Stage1Response {
    Multi { tables: Vec<PageCells> },
    Single(PageCells),
}

#[derive(Debug, serde::Deserialize)]
struct Stage2Box {
    box_2d: [f32; 4],
    text: String,
}

#[derive(Debug, serde::Deserialize)]
struct Stage2List {
    redactions: Vec<Stage2Box>,
}

#[derive(thiserror::Error, Debug)]
pub enum GeminiError {
    /// URL は外してある (`reqwest::Error::without_url`)
    #[error("gemini http: {0}")]
    Http(reqwest::Error),
    #[error("gemini status {0}: {1}")]
    Status(u16, String),
    #[error("gemini empty response")]
    Empty,
    #[error("gemini json parse: {0}")]
    Parse(serde_json::Error),
    #[error("redaction json parse: {0}")]
    RedactionParse(serde_json::Error),
}

impl From<reqwest::Error> for GeminiError {
    fn from(e: reqwest::Error) -> Self {
        GeminiError::Http(e.without_url())
    }
}

/// Gemini の応答の text を JSON として読む。text は金額の OCR なのでログには長さだけ出す。
fn parse_output<T: serde::de::DeserializeOwned>(stage: &str, text: &str) -> Result<T, GeminiError> {
    serde_json::from_str(text).map_err(|e| {
        tracing::warn!(stage, text_len = text.len(), "redact: parse failed: {e}");
        GeminiError::RedactionParse(e)
    })
}

/// Gemini の generateContent の呼び手。endpoint と model はフィールド (テストは wiremock の URI を渡す)。
#[derive(Clone)]
pub struct GeminiClient {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
}

impl GeminiClient {
    pub fn new(endpoint: String, model: String, api_key: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client");
        Self {
            http,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            model,
            api_key,
        }
    }

    /// generateContent を 1 回呼び、`candidates[0].content.parts[0].text` を返す。
    async fn generate(
        &self,
        parts: serde_json::Value,
        schema: serde_json::Value,
        max_output_tokens: u32,
    ) -> Result<String, GeminiError> {
        let url = format!("{}/models/{}:generateContent", self.endpoint, self.model);
        let body = serde_json::json!({
            "contents": [{ "role": "user", "parts": parts }],
            "generationConfig": {
                "temperature": 0.0,
                "responseMimeType": "application/json",
                "responseSchema": schema,
                "maxOutputTokens": max_output_tokens
            }
        });
        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GeminiError::Status(
                status.as_u16(),
                body.chars().take(500).collect(),
            ));
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&resp.text().await?).map_err(GeminiError::Parse)?;
        parsed
            .pointer("/candidates/0/content/parts/0/text")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or(GeminiError::Empty)
    }

    fn image_parts(jpeg: &[u8], prompt: &str) -> serde_json::Value {
        let data = base64::engine::general_purpose::STANDARD.encode(jpeg);
        serde_json::json!([
            { "inlineData": { "mimeType": "image/jpeg", "data": data } },
            { "text": prompt }
        ])
    }

    /// 1 段: 画像から「N円」の文字列の枠を返させる。
    pub async fn detect_amount_boxes(&self, jpeg: &[u8]) -> Result<Vec<RedactionBox>, GeminiError> {
        let text = self
            .generate(
                Self::image_parts(jpeg, REDACT_PROMPT),
                redact_response_schema(),
                2048,
            )
            .await?;
        let list: RedactionList = parse_output("1stage", &text)?;
        Ok(list.redactions)
    }

    /// Stage 1: 表内の全セルを列挙させる (帳票によっては 50 セルを超えるので token は多め)。
    pub async fn detect_all_cells(&self, jpeg: &[u8]) -> Result<Vec<PageCells>, GeminiError> {
        let text = self
            .generate(
                Self::image_parts(jpeg, STAGE1_PROMPT),
                stage1_response_schema(),
                8192,
            )
            .await?;
        let stage1: Stage1Response = parse_output("stage1", &text)?;
        Ok(match stage1 {
            Stage1Response::Multi { tables } => tables,
            Stage1Response::Single(p) => vec![p],
        })
    }

    /// Stage 2: Stage 1 の JSON を画像なしで渡し、金額のセルだけを選ばせる。
    /// page は Stage 2 が返さないので `pages[0].page` を補う (通知の PDF はほぼ単一ページ)。
    pub async fn filter_amount_cells(
        &self,
        pages: &[PageCells],
    ) -> Result<Vec<RedactionBox>, GeminiError> {
        let Some(first) = pages.first() else {
            return Ok(Vec::new());
        };
        let cells_json = serde_json::to_string(pages).map_err(GeminiError::Parse)?;
        let text = self
            .generate(
                serde_json::json!([{ "text": stage2_prompt(&cells_json) }]),
                stage2_response_schema(),
                4096,
            )
            .await?;
        let list: Stage2List = parse_output("stage2", &text)?;
        Ok(list
            .redactions
            .into_iter()
            .map(|s| RedactionBox {
                page: first.page,
                box_2d: s.box_2d,
                text: s.text,
            })
            .collect())
    }

    /// 2 段。どちらかの段が失敗するか Stage 1 が 0 セルなら 1 段に落ちる。
    pub async fn detect_amount_boxes_v2(
        &self,
        jpeg: &[u8],
    ) -> Result<Vec<RedactionBox>, GeminiError> {
        let reason = match self.detect_all_cells(jpeg).await {
            Ok(pages) if pages.iter().any(|p| !p.cells.is_empty()) => {
                match self.filter_amount_cells(&pages).await {
                    Ok(boxes) => return Ok(boxes),
                    Err(e) => format!("stage2 failed: {e}"),
                }
            }
            Ok(_) => "stage1 returned 0 cells".to_string(),
            Err(e) => format!("stage1 failed: {e}"),
        };
        tracing::warn!("redact 2-stage: fallback to 1-stage: {reason}");
        self.detect_amount_boxes(jpeg).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const JPEG: &[u8] = b"\xFF\xD8\xFF fake jpeg";
    const GEN_PATH: &str = "/models/test-model:generateContent";

    fn client(server: &MockServer) -> GeminiClient {
        GeminiClient::new(server.uri(), "test-model".into(), "fake-key".into())
    }

    fn ok_text(text: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "candidates": [{"content": {"parts": [{"text": text}]}}]
        }))
    }

    /// 応答を順に 1 回ずつ返すモックを積む (wiremock は先に積んだものから当てる)。
    async fn mount_seq(server: &MockServer, responses: Vec<ResponseTemplate>) {
        for r in responses {
            Mock::given(method("POST"))
                .and(path(GEN_PATH))
                .respond_with(r)
                .up_to_n_times(1)
                .mount(server)
                .await;
        }
    }

    #[test]
    fn cell_box_and_page_cells_serde() {
        let c = CellBox {
            box_2d: [100.0, 200.0, 150.0, 400.0],
            text: "100,000円".into(),
        };
        let back: CellBox = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(c, back);

        let p: PageCells = serde_json::from_str(r#"{"page":1}"#).unwrap();
        assert_eq!((p.page, p.title.as_str(), p.cells.len()), (1, "", 0));
    }

    #[test]
    fn stage1_response_accepts_single_and_multi() {
        let single: Stage1Response = serde_json::from_str(
            r#"{"page":1,"title":"運賃明細","cells":[{"box_2d":[100,200,150,400],"text":"運賃"}]}"#,
        )
        .unwrap();
        assert!(
            matches!(single, Stage1Response::Single(ref p) if p.title == "運賃明細" && p.cells.len() == 1)
        );

        let multi: Stage1Response = serde_json::from_str(
            r#"{"tables":[{"page":1,"title":"a","cells":[]},{"page":2,"title":"b","cells":[]}]}"#,
        )
        .unwrap();
        assert!(
            matches!(multi, Stage1Response::Multi { ref tables } if tables.len() == 2 && tables[1].page == 2)
        );
    }

    #[test]
    fn stage2_prompt_embeds_cells_json() {
        let prompt =
            stage2_prompt(r#"{"page":1,"cells":[{"box_2d":[1,2,3,4],"text":"100,000円"}]}"#);
        assert!(prompt.contains("100,000円"));
        assert!(prompt.contains("box_2d は入力 JSON のまま使うこと"));
    }

    /// schema を消したり field の名前を変えたりしたら落ちるよう、構造を固定する。
    #[test]
    fn response_schemas_pin_structure() {
        let s = redact_response_schema();
        assert_eq!(s["required"][0], "redactions");
        let item = &s["properties"]["redactions"]["items"];
        assert_eq!(item["properties"]["box_2d"]["minItems"], 4);
        assert_eq!(item["properties"]["box_2d"]["maxItems"], 4);
        assert_eq!(
            item["required"],
            serde_json::json!(["page", "box_2d", "text"])
        );
        assert_eq!(item["properties"]["cell_label"]["nullable"], true);

        let s = stage1_response_schema();
        assert_eq!(s["required"], serde_json::json!(["page", "cells"]));
        assert_eq!(s["properties"]["title"]["nullable"], true);
        let cell = &s["properties"]["cells"]["items"];
        assert_eq!(cell["properties"]["box_2d"]["maxItems"], 4);
        assert_eq!(cell["required"], serde_json::json!(["box_2d", "text"]));

        let s = stage2_response_schema();
        assert_eq!(s["required"][0], "redactions");
        let r = &s["properties"]["redactions"]["items"];
        assert_eq!(r["properties"]["box_2d"]["minItems"], 4);
        assert_eq!(r["required"], serde_json::json!(["box_2d", "text"]));
    }

    /// 3164 の消費税の行ずれ対策 (ラベル列と値列の分離) がプロンプトから消えないこと。
    #[test]
    fn stage1_prompt_enforces_label_value_separation() {
        assert!(STAGE1_PROMPT.contains("ラベル列と値列の完全分離"));
        assert!(STAGE1_PROMPT.contains("xmax_label <= xmin_value"));
        assert!(STAGE1_PROMPT.contains(r#""text": "消費税""#));
        assert!(STAGE1_PROMPT.contains(r#""text": "11,500円""#));
        assert!(STAGE1_PROMPT.contains("ラベルと値の bbox 重複禁止"));
    }

    /// 送る body に responseMimeType と responseSchema が両方入り、キーは URL でなくヘッダーで渡ること。
    #[tokio::test]
    async fn request_carries_schema_and_key_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(GEN_PATH))
            .and(header("x-goog-api-key", "fake-key"))
            .respond_with(ok_text(r#"{"redactions":[]}"#))
            .mount(&server)
            .await;

        let boxes = client(&server).detect_amount_boxes(JPEG).await.unwrap();
        assert!(boxes.is_empty());

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].url.query().is_none(), "key must not be in the URL");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let gc = &body["generationConfig"];
        assert_eq!(gc["responseMimeType"], "application/json");
        assert_eq!(gc["responseSchema"]["required"][0], "redactions");
        assert_eq!(
            body["contents"][0]["parts"][0]["inlineData"]["mimeType"],
            "image/jpeg"
        );
    }

    #[tokio::test]
    async fn v2_two_stage_happy_path() {
        let server = MockServer::start().await;
        mount_seq(
            &server,
            vec![
                ok_text(r#"{"page":1,"title":"運賃明細","cells":[{"box_2d":[100,200,150,400],"text":"運賃"},{"box_2d":[100,400,150,600],"text":"100,000円"},{"box_2d":[150,200,200,400],"text":"消費税"},{"box_2d":[150,400,200,600],"text":"10,000円"}]}"#),
                ok_text(r#"{"redactions":[{"box_2d":[100,400,150,600],"text":"100,000円"},{"box_2d":[150,400,200,600],"text":"10,000円"}]}"#),
            ],
        )
        .await;

        let r = client(&server).detect_amount_boxes_v2(JPEG).await.unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].page, 1); // Stage 1 の page から補う
        assert_eq!(r[0].box_2d, [100.0, 400.0, 150.0, 600.0]);
        assert_eq!(r[1].text, "10,000円");

        // Stage 2 は画像を送らない
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
        assert!(body["contents"][0]["parts"][0].get("inlineData").is_none());
        assert_eq!(
            body["generationConfig"]["responseSchema"]["required"][0],
            "redactions"
        );
    }

    #[tokio::test]
    async fn v2_falls_back_when_stage1_fails() {
        let server = MockServer::start().await;
        mount_seq(
            &server,
            vec![
                ResponseTemplate::new(500).set_body_string("oops"),
                ok_text(
                    r#"{"redactions":[{"page":1,"box_2d":[100,200,150,400],"text":"fallback"}]}"#,
                ),
            ],
        )
        .await;
        let r = client(&server).detect_amount_boxes_v2(JPEG).await.unwrap();
        assert_eq!(r[0].text, "fallback");
    }

    #[tokio::test]
    async fn v2_falls_back_when_stage1_is_empty() {
        let server = MockServer::start().await;
        mount_seq(
            &server,
            vec![
                ok_text(r#"{"page":1,"title":"empty","cells":[]}"#),
                ok_text(r#"{"redactions":[{"page":1,"box_2d":[100,200,150,400],"text":"fb"}]}"#),
            ],
        )
        .await;
        let r = client(&server).detect_amount_boxes_v2(JPEG).await.unwrap();
        assert_eq!(r[0].text, "fb");
    }

    #[tokio::test]
    async fn v2_falls_back_when_stage2_fails() {
        let server = MockServer::start().await;
        mount_seq(
            &server,
            vec![
                ok_text(r#"{"page":1,"cells":[{"box_2d":[0,0,100,100],"text":"x"}]}"#),
                ResponseTemplate::new(500),
                ok_text(r#"{"redactions":[{"page":1,"box_2d":[0,0,100,100],"text":"fb2"}]}"#),
            ],
        )
        .await;
        let r = client(&server).detect_amount_boxes_v2(JPEG).await.unwrap();
        assert_eq!(r[0].text, "fb2");
    }

    #[tokio::test]
    async fn v2_errors_when_fallback_also_fails() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;
        let err = client(&server)
            .detect_amount_boxes_v2(JPEG)
            .await
            .unwrap_err();
        assert!(matches!(err, GeminiError::Status(403, ref b) if b == "forbidden"));
    }

    #[tokio::test]
    async fn detect_all_cells_multi_tables_form() {
        let server = MockServer::start().await;
        mount_seq(
            &server,
            vec![ok_text(r#"{"tables":[{"page":1,"title":"t1","cells":[]},{"page":2,"title":"t2","cells":[{"box_2d":[0,0,100,100],"text":"x"}]}]}"#)],
        )
        .await;
        let pages = client(&server).detect_all_cells(JPEG).await.unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[1].cells.len(), 1);
    }

    #[tokio::test]
    async fn detect_all_cells_empty_candidates_errors() {
        let server = MockServer::start().await;
        mount_seq(
            &server,
            vec![ResponseTemplate::new(200).set_body_json(serde_json::json!({"candidates": []}))],
        )
        .await;
        let err = client(&server).detect_all_cells(JPEG).await.unwrap_err();
        assert!(matches!(err, GeminiError::Empty));
    }

    #[tokio::test]
    async fn non_json_body_errors() {
        let server = MockServer::start().await;
        mount_seq(
            &server,
            vec![ResponseTemplate::new(200).set_body_string("<html>")],
        )
        .await;
        let err = client(&server).detect_amount_boxes(JPEG).await.unwrap_err();
        assert!(matches!(err, GeminiError::Parse(_)));
    }

    #[tokio::test]
    async fn filter_amount_cells_empty_input_short_circuits() {
        let c = GeminiClient::new("http://unreachable.invalid".into(), "m".into(), "k".into());
        assert!(c.filter_amount_cells(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn filter_amount_cells_invalid_json_errors() {
        let server = MockServer::start().await;
        mount_seq(&server, vec![ok_text("not a json")]).await;
        let pages = vec![PageCells {
            page: 1,
            title: String::new(),
            cells: vec![CellBox {
                box_2d: [0.0, 0.0, 100.0, 100.0],
                text: "x".into(),
            }],
        }];
        let err = client(&server)
            .filter_amount_cells(&pages)
            .await
            .unwrap_err();
        assert!(matches!(err, GeminiError::RedactionParse(_)));
    }

    /// 接続の失敗のエラー文に URL (= endpoint) が出ないこと。
    #[tokio::test]
    async fn http_error_does_not_carry_url() {
        let c = GeminiClient::new("http://127.0.0.1:1".into(), "m".into(), "k".into());
        let err = c.detect_amount_boxes(JPEG).await.unwrap_err();
        assert!(matches!(err, GeminiError::Http(_)));
        assert!(!err.to_string().contains("127.0.0.1"), "{err}");
    }
}
