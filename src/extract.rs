//! 配車系 PDF から Gemini で 14 項目を抽出する (rust-alc-api の `crates/alc-notify/src/extract.rs`
//! から移植。W8c-2、Refs ippoan/rust-alc-api#680)。
//!
//! 積地 (会社名/住所/電話/担当)・卸地 (会社名/住所/電話/担当)・積み/卸し日時・注意事項・
//! 相手先の連絡先 (会社名/担当者/電話)。呼び手 (notify-worker) は結果を
//! `notify_documents.extracted_data.logistics` に入れる。配車手配票でない PDF は全項目 null。
//!
//! 「自社」と「相手先」の区別: 連絡先 3 項目は相手先 (依頼元・荷主) のものが要るので、
//! 自社名 (`tenants.name`) をプロンプトに渡して自社の情報を除かせる。区別がつかなければ null。
//!
//! Gemini には PDF をそのまま (`application/pdf` の inlineData) 送る (黒塗りと違い描画しない)。
//! `generationConfig` は `responseMimeType` と `responseSchema` を両方入れる。

use crate::gemini::{GeminiClient, GeminiError};
use base64::Engine;

/// プロンプト本文を組み立てる。`self_company_hint` (テナント名) があれば自社除外の節を入れる。
///
/// 配車系でない PDF で「無理にどこかから値を埋める」のを防ぐため、「該当が無ければ null」を明示する。
/// 日付は ISO に正規化せず原文の表記のまま (画面に出すだけなので、ばらつきを受け入れる方が正確)。
pub fn build_logistics_prompt(self_company_hint: Option<&str>) -> String {
    let self_section = match self_company_hint {
        Some(name) if !name.trim().is_empty() => format!(
            "\n\n## 自社情報 (除外対象)\n\
             この PDF を処理するテナント (= 自社) は **「{}」** です。\n\
             連絡先 3 フィールド (contact_company / contact_person / contact_phone) は\n\
             **自社ではなく相手先 (依頼元・発注元・荷主・お客様) のものを抽出**してください。\n\
             - 自社名 (上記) が文書ヘッダ/フッタ/印鑑欄/受注欄にある場合は除外\n\
             - 自社の電話番号・FAX 番号・担当者名と一致するものは除外\n\
             - 自社しか書かれていない場合は連絡先 3 フィールドを全て null にする",
            name.trim()
        ),
        _ => String::from(
            "\n\n## 連絡先の判定\n\
             連絡先 3 フィールドは「相手先 (依頼元・発注元・荷主・お客様)」のものを抽出する。\n\
             ラベル例: 「依頼元」「お客様」「荷主」「発注元」「ご依頼元」「お得意様」「貴社」「申込元」。\n\
             逆に「受注」「ドライバー」「業務担当」「配車」「運転手」のラベル、文書ヘッダ/印鑑/\n\
             差出人欄の会社名は **自社側** なので除外する。\n\
             相手先と自社の区別がつかない場合は 3 フィールドとも null にする (誤抽出より null が良い)。",
        ),
    };

    format!(
        r#"この PDF は運送業務で受信した文書です。配車手配票・運行依頼書であれば
以下 14 フィールドを抽出してください:

【配車情報】
  - loading_place           : 積み込み場所の主要な識別 (会社名/施設名があればそれ、なければ住所)
  - loading_place_address   : 積み込み場所の住所 (loading_place が会社名のとき、その下に書かれた住所)
  - loading_place_phone     : 積み込み場所の電話番号 ("電話" "TEL" 等のラベル付きで書かれている時)
  - loading_place_person    : 積み込み場所の担当者氏名 ("担当" "ご担当" 等のラベル付きで書かれている時)
  - loading_at              : 積み日時 (PDF の表記をそのまま、例: "5/9 10:00" / "令和8年4月17日 19時")
  - unloading_place         : 卸し場所の主要な識別 (会社名/施設名があればそれ、なければ住所)
  - unloading_place_address : 卸し場所の住所 (unloading_place が会社名のとき、その下の住所)
  - unloading_place_phone   : 卸し場所の電話番号 (ラベル付きで書かれている時)
  - unloading_place_person  : 卸し場所の担当者氏名 (ラベル付きで書かれている時)
  - unloading_at            : 卸し日時 (同上)
  - notes                   : 注意事項 (冷凍便、要時間厳守、要連絡など配送上の留意点。複数あれば改行で結合)

【相手先 (依頼元) 連絡先】
  - contact_company         : 連絡先の会社名 (相手先 = 依頼元・発注元・お客様の会社名)
  - contact_person          : 担当者氏名 (相手先 = 依頼元・発注元の担当者)
  - contact_phone           : 電話番号 (相手先の TEL/連絡先電話、PDF の表記そのまま)

## ルール

  - PDF が配車手配票でない (請求書・報告書・通知文など) 場合、または該当情報がない場合は
    そのフィールドを null にする。**推測で埋めない、書かれていないものは null**。
  - 全フィールド null になっても OK (配信本文は既存テンプレにフォールバックする)。
  - 日付は ISO 8601 に正規化しない。PDF の表記を**そのまま**返す。
  - **loading_place / unloading_place** は会社名・施設名があれば優先 (例: "イオン関西RDC")、
    なければ住所 (例: "熊本県八代市新港町3-9-8") を入れる。両方ある時は会社名を選ぶ。
  - **loading_place_address / unloading_place_address** は loading_place が会社名のときに
    その下に並んで書かれた住所 (例: "京都府乙訓郡大山崎町字大山崎小字鏡田38")。
    住所しかない場合は null (loading_place 側に既に入っているので重複させない)。
  - **loading_place_phone / unloading_place_phone / loading_place_person / unloading_place_person**
    は積み込み/卸し場所のセル内に "電話/TEL" "担当/ご担当" 等のラベルで併記された情報。
    依頼元 (相手先) の `contact_*` と混ぜないこと。場所セルの **内側** にあるか、
    場所セルの**直下/直右**にあるものだけ場所側、独立した「ご依頼元」「お客様」セル内のものは
    contact_* 側。
  - 同じ担当者氏名が場所セルと相手先セルの両方に書かれていることはほぼ無いが、
    どちらか判断つかない場合は contact_person 側を優先 (場所側は null)。
  - notes は複数項目を改行 (\n) で連結する。
  - 電話番号はハイフンの有無や市外局番形式を変更せず、PDF の表記をそのまま。
  - 金額情報 (合計金額、運賃、消費税など)、支払方法、支払期日は **どのフィールドにも含めない**。
  - 出力はこの schema に厳密に従う JSON 1 個 (前後に余分なテキスト・markdown 装飾を一切付けない)。{self_section}"#,
        self_section = self_section
    )
}

/// 抽出結果。null の項目はキーごと省く (rust と同じ形で `extracted_data.logistics` に入る)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
pub struct LogisticsFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loading_place: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loading_place_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loading_place_phone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loading_place_person: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unloading_place: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unloading_place_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unloading_place_phone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unloading_place_person: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loading_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unloading_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_company: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_person: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_phone: Option<String>,
}

/// 14 項目は全部 nullable な STRING。`required` には入れない (該当が無いとき null にできるように)。
fn logistics_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "OBJECT",
        "properties": {
            "loading_place":           { "type": "STRING", "nullable": true },
            "loading_place_address":   { "type": "STRING", "nullable": true },
            "loading_place_phone":     { "type": "STRING", "nullable": true },
            "loading_place_person":    { "type": "STRING", "nullable": true },
            "unloading_place":         { "type": "STRING", "nullable": true },
            "unloading_place_address": { "type": "STRING", "nullable": true },
            "unloading_place_phone":   { "type": "STRING", "nullable": true },
            "unloading_place_person":  { "type": "STRING", "nullable": true },
            "loading_at":              { "type": "STRING", "nullable": true },
            "unloading_at":            { "type": "STRING", "nullable": true },
            "notes":                   { "type": "STRING", "nullable": true },
            "contact_company":         { "type": "STRING", "nullable": true },
            "contact_person":          { "type": "STRING", "nullable": true },
            "contact_phone":           { "type": "STRING", "nullable": true }
        },
        "propertyOrdering": [
            "loading_place", "loading_place_address", "loading_place_phone", "loading_place_person",
            "unloading_place", "unloading_place_address", "unloading_place_phone", "unloading_place_person",
            "loading_at", "unloading_at", "notes",
            "contact_company", "contact_person", "contact_phone"
        ]
    })
}

#[derive(thiserror::Error, Debug)]
pub enum ExtractError {
    /// 呼び出しの失敗 (HTTP・status・空の応答・応答が JSON でない) → `gemini`
    #[error(transparent)]
    Gemini(#[from] GeminiError),
    /// 応答の text が schema の形でない → `parse`
    #[error("logistics json parse: {0}")]
    Parse(serde_json::Error),
}

impl GeminiClient {
    /// PDF から 14 項目を抽出する。`self_company_hint` は自社除外用のテナント名 (None / 空でも動く)。
    pub async fn extract_logistics(
        &self,
        pdf: &[u8],
        self_company_hint: Option<&str>,
    ) -> Result<LogisticsFields, ExtractError> {
        let data = base64::engine::general_purpose::STANDARD.encode(pdf);
        let parts = serde_json::json!([
            { "inlineData": { "mimeType": "application/pdf", "data": data } },
            { "text": build_logistics_prompt(self_company_hint) }
        ]);
        // 14 項目 × 短い文字列なので 1024 で足りる
        let text = self
            .generate(parts, logistics_response_schema(), 1024)
            .await?;
        serde_json::from_str(&text).map_err(|e| {
            tracing::warn!(text_len = text.len(), "extract: parse failed: {e}");
            ExtractError::Parse(e)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PDF: &[u8] = b"%PDF-1.5 fake";
    const FIELDS: [&str; 14] = [
        "loading_place",
        "loading_place_address",
        "loading_place_phone",
        "loading_place_person",
        "unloading_place",
        "unloading_place_address",
        "unloading_place_phone",
        "unloading_place_person",
        "loading_at",
        "unloading_at",
        "notes",
        "contact_company",
        "contact_person",
        "contact_phone",
    ];

    fn client(server: &MockServer) -> GeminiClient {
        GeminiClient::new(server.uri(), "test-model".into(), "fake-key".into())
    }

    fn ok_text(text: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "candidates": [{"content": {"parts": [{"text": text}]}}]
        }))
    }

    #[test]
    fn schema_includes_all_fourteen_nullable_fields() {
        let schema = logistics_response_schema();
        let props = schema["properties"].as_object().unwrap();
        for k in FIELDS {
            assert_eq!(props[k]["type"], "STRING", "{k}");
            assert_eq!(props[k]["nullable"], true, "{k}");
        }
        assert_eq!(props.len(), 14);
        assert_eq!(schema["propertyOrdering"].as_array().unwrap().len(), 14);
        assert!(schema.get("required").is_none());
    }

    #[test]
    fn prompt_lists_fields_and_excludes_amounts() {
        let p = build_logistics_prompt(None);
        for k in FIELDS {
            assert!(p.contains(k), "prompt missing field: {k}");
        }
        assert!(p.contains("金額情報"));
        assert!(p.contains("## 連絡先の判定"));
        assert!(!p.contains("自社情報 (除外対象)"));
    }

    #[test]
    fn prompt_uses_self_company_hint() {
        let p = build_logistics_prompt(Some(" テスト運輸株式会社 "));
        assert!(p.contains("「テスト運輸株式会社」"));
        assert!(p.contains("自社情報 (除外対象)"));
        assert!(!p.contains("## 連絡先の判定\n"));
        // 空白だけは None と同じ
        assert!(build_logistics_prompt(Some("   ")).contains("## 連絡先の判定"));
    }

    #[test]
    fn logistics_omits_null_fields_when_serialized() {
        let f: LogisticsFields =
            serde_json::from_str(r#"{"loading_place":"A","notes":null,"extra":"x"}"#).unwrap();
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            serde_json::json!({"loading_place": "A"})
        );
    }

    /// PDF を inlineData で送り、generationConfig に mime と schema を両方、鍵はヘッダー。
    #[tokio::test]
    async fn request_carries_pdf_schema_and_key_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/test-model:generateContent"))
            .and(header("x-goog-api-key", "fake-key"))
            .respond_with(ok_text(
                r#"{"loading_place":"イオン関西RDC","contact_phone":"03-0000-0000"}"#,
            ))
            .mount(&server)
            .await;

        let f = client(&server)
            .extract_logistics(PDF, Some("自社"))
            .await
            .unwrap();
        assert_eq!(f.loading_place.as_deref(), Some("イオン関西RDC"));
        assert_eq!(f.contact_phone.as_deref(), Some("03-0000-0000"));

        let reqs = server.received_requests().await.unwrap();
        assert!(reqs[0].url.query().is_none(), "key must not be in the URL");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let inline = &body["contents"][0]["parts"][0]["inlineData"];
        assert_eq!(inline["mimeType"], "application/pdf");
        assert_eq!(
            inline["data"],
            base64::engine::general_purpose::STANDARD.encode(PDF)
        );
        assert!(body["contents"][0]["parts"][1]["text"]
            .as_str()
            .unwrap()
            .contains("「自社」"));
        let gc = &body["generationConfig"];
        assert_eq!(gc["temperature"], 0.0);
        assert_eq!(gc["maxOutputTokens"], 1024);
        assert_eq!(gc["responseMimeType"], "application/json");
        assert_eq!(
            gc["responseSchema"]["properties"]["contact_phone"]["type"],
            "STRING"
        );
    }

    #[tokio::test]
    async fn status_error_is_gemini() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(402).set_body_string("pay"))
            .mount(&server)
            .await;
        let err = client(&server)
            .extract_logistics(PDF, None)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ExtractError::Gemini(GeminiError::Status(402, _))
        ));
    }

    #[tokio::test]
    async fn non_schema_text_is_parse() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ok_text("```json\n{}\n```"))
            .mount(&server)
            .await;
        let err = client(&server)
            .extract_logistics(PDF, None)
            .await
            .unwrap_err();
        assert!(matches!(err, ExtractError::Parse(_)));
    }
}
