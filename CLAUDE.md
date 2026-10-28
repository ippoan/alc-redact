# alc-redact

PDF の 1 ページ目を、金額の枠 (Gemini で検出) を黒で塗った JPEG にして返す Cloud Run の service。
rust-alc-api の `crates/alc-notify/src/redact.rs` から移した (W8d、Refs ippoan/rust-alc-api#680)。口と運用は README。

## コマンド

`cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test` (tests/http.rs は libpdfium.so が要る。手元は `LD_LIBRARY_PATH` に置く) / `docker build .`

## 規範 (must / never)

- 識別子 (GCP の project 名・番号・SA のメール・Artifact Registry のパス・run.app の URL) を repo・commit・PR に書かない。workflow には `vars.*`・`secrets.*` の名前で渡す
- Cloud Run は IAM の invoker で守る。`--allow-unauthenticated`・`--no-invoker-iam-check` を書かない
- GCP への書き込み (service の作成・IAM・secret の付与) は PAM の申請 → オーナーの承認で行う (`gcp-pam-ops` skill)。workflow の配信は `services update` だけ
- secret を足さない。Gemini の鍵は既存の `gemini-api-key` を secretKeyRef で `GEMINI_API_KEY` に入れる
- Gemini の `generationConfig` は必ず `responseMimeType` と `responseSchema` を両方。endpoint は const にせず `GeminiClient` のフィールド。鍵は URL でなく `x-goog-api-key` ヘッダー。テストで本物の Gemini を叩かない (wiremock)
- tracing のマクロは 1 行。Gemini の応答の text (金額の OCR) をログに出さない
- handler で `tokio::spawn` の投げっ放しをしない (Cloud Run は応答後に CPU を絞る)。pdfium は `spawn_blocking` を await する
- pdfium の版を上げるときは Dockerfile の `PDFIUM_VERSION` と `PDFIUM_SHA256` を両方 (CI もそこから読む)
- DB・R2・状態の列・WS の push はこの service に持たせない (呼び手の notify-worker が持つ)
