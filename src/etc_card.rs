//! 「ETC コーポレートカード発行明細書」の PDF から、カードの行を Gemini で抽出する
//! (`POST /extract/etc-card`。Refs ohishi-exp/nuxt-dtako-admin#1287)。
//!
//! 表の 1 行 = 区分 (新規 / 追加 / 再発行)・カード番号 (16 桁)・車両番号・車載器管理番号 (19 桁)。
//! 結果は呼び手 (auth-worker) を経て etc-meisai.jp へのカード登録に使われるので、番号は
//! Rust 側でも数字だけにして桁数を検査し、合わなければ null にする (誤った番号で登録しないため)。
//!
//! 呼び出し (PDF の inlineData・`responseMimeType` + `responseSchema`) は `/extract` と共通の
//! `GeminiClient::extract_from_pdf`。応答の text と番号はログに出さない (行数と null の個数だけ)。

use crate::extract::ExtractError;
use crate::gemini::GeminiClient;

/// カード番号の桁数 (例 `NNNNNN-NNNN-NNNNN-N`)
pub const CARD_NUMBER_DIGITS: usize = 16;
/// 車載器管理番号の桁数 (例 `NNNNN-NNNNNNNN-NNNNNN`、5-8-6)
pub const OBU_NUMBER_DIGITS: usize = 19;

const KUBUN: [&str; 3] = ["新規", "追加", "再発行"];

pub fn build_etc_card_prompt() -> String {
    format!(
        r#"この PDF が「ETC コーポレートカード発行明細書」であれば、明細の表から
カードの行をすべて抽出してください。表の 1 行には次の 4 項目があります:

  - kubun          : 区分。"新規" / "追加" / "再発行" のどれか (読めなければ null)
  - card_number    : カード番号 (数字 {card} 桁。原文は "NNNNNN-NNNN-NNNNN-N" のようにハイフン区切り)
  - vehicle_number : 車両番号 (例 "地名 100 か 1234")
  - obu_number     : 車載器管理番号 (数字 {obu} 桁。原文は "NNNNN-NNNNNNNN-NNNNNN" のようにハイフン区切り)

さらに、表の下にある「新規発行 N 枚 追加発行 N 枚 再発行 N 枚 計 N 枚」の **計** の枚数を total に入れる。

## ルール

  - card_number / obu_number は **数字だけ** を返す (ハイフン・空白・記号を除く。全角数字は半角にする)。
    読めない桁がある・桁数が合わないときは推測で埋めず null にする。
  - vehicle_number は原文の表記どおり (地名・分類番号・かな・一連番号を空白で区切る)。
  - 何も書かれていない空の行は返さない。行の順番は表の上から順。
  - この PDF が ETC コーポレートカード発行明細書でないときは rows を空の配列、total を null にする。
  - total が読めないときは null。
  - 出力はこの schema に厳密に従う JSON 1 個 (前後に余分なテキスト・markdown 装飾を一切付けない)。"#,
        card = CARD_NUMBER_DIGITS,
        obu = OBU_NUMBER_DIGITS,
    )
}

fn etc_card_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "OBJECT",
        "properties": {
            "rows": {
                "type": "ARRAY",
                "items": {
                    "type": "OBJECT",
                    "properties": {
                        "kubun":          { "type": "STRING", "nullable": true, "enum": KUBUN },
                        "card_number":    { "type": "STRING", "nullable": true },
                        "vehicle_number": { "type": "STRING", "nullable": true },
                        "obu_number":     { "type": "STRING", "nullable": true }
                    },
                    "propertyOrdering": ["kubun", "card_number", "vehicle_number", "obu_number"]
                }
            },
            "total": { "type": "INTEGER", "nullable": true }
        },
        "required": ["rows"],
        "propertyOrdering": ["rows", "total"]
    })
}

/// 明細の 1 行。null の項目もキーを残す (呼び手が「読めなかった」を見分けられるように)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
pub struct EtcCardRow {
    #[serde(default)]
    pub kubun: Option<String>,
    #[serde(default)]
    pub card_number: Option<String>,
    #[serde(default)]
    pub vehicle_number: Option<String>,
    #[serde(default)]
    pub obu_number: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
pub struct EtcCardStatement {
    #[serde(default)]
    pub rows: Vec<EtcCardRow>,
    #[serde(default)]
    pub total: Option<u32>,
}

/// 全角数字 (０-９) を半角にする。
fn halfwidth_digits(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '０'..='９' => char::from_u32(c as u32 - '０' as u32 + '0' as u32).unwrap_or(c),
            _ => c,
        })
        .collect()
}

/// 数字以外を落とし、桁数が `digits` のときだけ返す。
fn normalize_number(v: Option<String>, digits: usize) -> Option<String> {
    let d: String = halfwidth_digits(&v?)
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    (d.len() == digits).then_some(d)
}

fn normalize_text(v: Option<String>) -> Option<String> {
    let t = halfwidth_digits(v?.trim());
    (!t.is_empty()).then_some(t)
}

fn normalize_kubun(v: Option<String>) -> Option<String> {
    let t = v?;
    let t = t.trim();
    // 「再発行」は KUBUN そのもの。「発行」を落とすのは、そのままでは無いとき (「追加発行」→「追加」) だけ
    let found = [Some(t), t.strip_suffix("発行")]
        .into_iter()
        .flatten()
        .find(|c| KUBUN.contains(c));
    found.map(str::to_string)
}

impl EtcCardRow {
    fn normalize(self) -> Self {
        Self {
            kubun: normalize_kubun(self.kubun),
            card_number: normalize_number(self.card_number, CARD_NUMBER_DIGITS),
            vehicle_number: normalize_text(self.vehicle_number),
            obu_number: normalize_number(self.obu_number, OBU_NUMBER_DIGITS),
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn null_count(&self) -> usize {
        [
            &self.kubun,
            &self.card_number,
            &self.vehicle_number,
            &self.obu_number,
        ]
        .iter()
        .filter(|v| v.is_none())
        .count()
    }
}

impl EtcCardStatement {
    /// 各行を正規化し、全項目 null の行 (空行) を落とす。
    pub fn normalize(self) -> Self {
        Self {
            rows: self
                .rows
                .into_iter()
                .map(EtcCardRow::normalize)
                .filter(|r| !r.is_empty())
                .collect(),
            total: self.total,
        }
    }
}

impl GeminiClient {
    /// ETC コーポレートカード発行明細書の PDF からカードの行を抽出する。明細書でなければ rows は空。
    pub async fn extract_etc_card(&self, pdf: &[u8]) -> Result<EtcCardStatement, ExtractError> {
        // 行が多い明細書でも 1 行 ~100 token なので 4096 で足りる
        let raw: EtcCardStatement = self
            .extract_from_pdf(
                pdf,
                &build_etc_card_prompt(),
                etc_card_response_schema(),
                4096,
            )
            .await?;
        let out = raw.normalize();
        let nulls: usize = out.rows.iter().map(EtcCardRow::null_count).sum();
        tracing::info!(rows = out.rows.len(), nulls, total = ?out.total, "etc_card_extracted");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::GeminiError;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PDF: &[u8] = b"%PDF-1.5 fake";

    fn client(server: &MockServer) -> GeminiClient {
        GeminiClient::new(server.uri(), "test-model".into(), "fake-key".into())
    }

    async fn server_returning(text: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/test-model:generateContent"))
            .and(header("x-goog-api-key", "fake-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{"content": {"parts": [{"text": text.to_string()}]}}]
            })))
            .mount(&server)
            .await;
        server
    }

    #[test]
    fn schema_has_rows_and_total() {
        let s = etc_card_response_schema();
        let item = &s["properties"]["rows"]["items"]["properties"];
        for k in ["kubun", "card_number", "vehicle_number", "obu_number"] {
            assert_eq!(item[k]["type"], "STRING", "{k}");
            assert_eq!(item[k]["nullable"], true, "{k}");
        }
        assert_eq!(item["kubun"]["enum"], serde_json::json!(KUBUN));
        assert_eq!(s["properties"]["total"]["type"], "INTEGER");
        assert_eq!(s["required"], serde_json::json!(["rows"]));
    }

    #[test]
    fn prompt_mentions_fields_and_digit_counts() {
        let p = build_etc_card_prompt();
        for k in [
            "kubun",
            "card_number",
            "vehicle_number",
            "obu_number",
            "total",
        ] {
            assert!(p.contains(k), "{k}");
        }
        assert!(p.contains("16 桁") && p.contains("19 桁"));
        assert!(p.contains("rows を空の配列"));
    }

    #[test]
    fn kubun_keeps_reissue_and_strips_hakko_suffix() {
        let k = |s: &str| normalize_kubun(Some(s.into()));
        assert_eq!(k("再発行").as_deref(), Some("再発行"));
        assert_eq!(k(" 再発行 ").as_deref(), Some("再発行"));
        assert_eq!(k("新規発行").as_deref(), Some("新規"));
        assert_eq!(k("追加発行").as_deref(), Some("追加"));
        assert_eq!(k("新規").as_deref(), Some("新規"));
        assert_eq!(k("再"), None);
        assert_eq!(k("解約"), None);
        assert_eq!(normalize_kubun(None), None);
    }

    /// (a) 正常 2 行。PDF は inlineData、generationConfig に mime と schema の両方。
    #[tokio::test]
    async fn two_rows() {
        let server = server_returning(serde_json::json!({
            "rows": [
                {"kubun": "新規", "card_number": "0000000000000010",
                 "vehicle_number": "テスト 100 あ 1234", "obu_number": "0000000000000000001"},
                {"kubun": "再発行", "card_number": "0000000000000020",
                 "vehicle_number": "テスト 100 い 5678", "obu_number": "0000000000000000002"}
            ],
            "total": 2
        }))
        .await;
        let got = client(&server).extract_etc_card(PDF).await.unwrap();
        assert_eq!(got.total, Some(2));
        assert_eq!(got.rows.len(), 2);
        assert_eq!(got.rows[0].kubun.as_deref(), Some("新規"));
        assert_eq!(got.rows[0].card_number.as_deref(), Some("0000000000000010"));
        assert_eq!(got.rows[1].kubun.as_deref(), Some("再発行"));
        assert_eq!(
            got.rows[1].vehicle_number.as_deref(),
            Some("テスト 100 い 5678")
        );
        assert_eq!(
            got.rows[1].obu_number.as_deref(),
            Some("0000000000000000002")
        );

        let reqs = server.received_requests().await.unwrap();
        assert!(reqs[0].url.query().is_none(), "key must not be in the URL");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(
            body["contents"][0]["parts"][0]["inlineData"]["mimeType"],
            "application/pdf"
        );
        let gc = &body["generationConfig"];
        assert_eq!(gc["responseMimeType"], "application/json");
        assert_eq!(
            gc["responseSchema"]["properties"]["total"]["type"],
            "INTEGER"
        );
        assert_eq!(gc["maxOutputTokens"], 4096);
    }

    /// (b) 桁が足りない・多い番号は null。区分の表記ゆれと知らない区分。
    #[tokio::test]
    async fn wrong_digit_count_is_null() {
        let server = server_returning(serde_json::json!({
            "rows": [
                {"kubun": "追加発行", "card_number": "000000-0000-0000-0",
                 "vehicle_number": "テスト 100 あ 1234", "obu_number": "00000-00000000-0000001"},
                {"kubun": "解約", "card_number": null,
                 "vehicle_number": null, "obu_number": "00000-0000000-000001"}
            ],
            "total": null
        }))
        .await;
        let got = client(&server).extract_etc_card(PDF).await.unwrap();
        assert_eq!(got.total, None);
        assert_eq!(
            got.rows,
            vec![
                EtcCardRow {
                    kubun: Some("追加".into()),
                    card_number: None,
                    vehicle_number: Some("テスト 100 あ 1234".into()),
                    obu_number: None,
                },
                // 全部 null になった行は空行として落ちる
            ]
        );
    }

    /// (c) ハイフン・空白・全角数字の正規化。空行は落ちる。
    #[tokio::test]
    async fn normalizes_hyphens_and_fullwidth() {
        let server = server_returning(serde_json::json!({
            "rows": [
                {"kubun": " 新規 ", "card_number": "０００００１-0000-00001-0",
                 "vehicle_number": " テスト １００ あ １２３４ ",
                 "obu_number": "00000 - 00000000 - 000003"},
                {"kubun": null, "card_number": "", "vehicle_number": "  ", "obu_number": null}
            ],
            "total": 1
        }))
        .await;
        let got = client(&server).extract_etc_card(PDF).await.unwrap();
        assert_eq!(
            got.rows,
            vec![EtcCardRow {
                kubun: Some("新規".into()),
                card_number: Some("0000010000000010".into()),
                vehicle_number: Some("テスト 100 あ 1234".into()),
                obu_number: Some("0000000000000000003".into()),
            }]
        );
        assert_eq!(got.total, Some(1));
    }

    /// (d) 明細書でない → 空
    #[tokio::test]
    async fn not_a_statement_is_empty() {
        let server = server_returning(serde_json::json!({"rows": [], "total": null})).await;
        let got = client(&server).extract_etc_card(PDF).await.unwrap();
        assert_eq!(got, EtcCardStatement::default());
        assert_eq!(
            serde_json::to_value(&got).unwrap(),
            serde_json::json!({"rows": [], "total": null})
        );
    }

    #[tokio::test]
    async fn status_error_is_gemini_and_bad_shape_is_parse() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("x"))
            .mount(&server)
            .await;
        let err = client(&server).extract_etc_card(PDF).await.unwrap_err();
        assert!(matches!(
            err,
            ExtractError::Gemini(GeminiError::Status(500, _))
        ));

        let server = server_returning(serde_json::json!({"rows": "nope"})).await;
        let err = client(&server).extract_etc_card(PDF).await.unwrap_err();
        assert!(matches!(err, ExtractError::Parse(_)));
    }
}
