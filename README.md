# alc-redact

PDF の 1 ページ目を黒塗りした JPEG を返す Cloud Run (rust-alc-api の W8d、Refs ippoan/rust-alc-api#680)。

1 ページ目を pdfium で 300 dpi に描画し、その画像を Gemini に送って「N円」の金額の枠を検出させ、
同じ画像の枠を黒で塗って JPEG (品質 85) で返す。検出は既定で 2 段 (全セルの列挙 → 画像なしで
金額のセルだけを選ぶ。失敗したら 1 段に落ちる)。

## 口

### `POST /redact`

- body: PDF のバイト列 (`Content-Type: application/pdf`)。上限 32 MiB
- 200: `image/jpeg`。ヘッダー `X-Redactions-Applied` (塗った枠の数)、`X-Redact-Llm-Ms`・`X-Redact-Render-Ms`・`X-Redact-Total-Ms` (所要時間)
- エラー: `{"error": "<code>", "detail": "..."}`。呼び手は status でなく `error` で見分ける (Cloud Run 自体の 503 と区別するため)

| `error` | status | 意味 | rust の状態 |
|---|---|---|---|
| `bad_request` | 400 | body が空 | — |
| `not_pdf` | 415 | body が `%PDF-` で始まらない | `skipped` |
| `gemini_key_missing` | 503 | `GEMINI_API_KEY` が無い | `skipped` |
| `gemini` | 502 | Gemini の失敗 (2 段 → 1 段の fallback も失敗) | `failed` (`gemini:`) |
| `apply` | 500 | pdfium の描画・枠の検証・JPEG 化の失敗 | `failed` (`apply:`) |

### `POST /extract`

配車系 PDF から 14 項目 (積地・卸地の会社名/住所/電話/担当、積み/卸し日時、注意事項、相手先の会社名/担当者/電話) を
Gemini で抽出する (rust の `crates/alc-notify/src/extract.rs` から移した。W8c-2)。PDF はそのまま Gemini に送る (描画しない)。

- body: PDF のバイト列。上限 32 MiB
- header `X-Self-Company` (任意): 自社名 (`tenants.name`) を UTF-8 で percent-encode したもの。連絡先から自社を除かせる
- 200: `{"logistics": {...}}`。null の項目はキーごと省く (配車系でない PDF は `{}`)
- エラー: `/redact` と同じ形

| `error` | status | 意味 | rust の状態 |
|---|---|---|---|
| `bad_request` | 400 | body が空 / `X-Self-Company` が UTF-8 の percent-encode でない | — |
| `not_pdf` | 415 | body が `%PDF-` で始まらない | (呼び手は拡張子で先に弾く) |
| `gemini_key_missing` | 503 | `GEMINI_API_KEY` が無い | `completed` (logistics なし) |
| `gemini` | 502 | Gemini の呼び出しの失敗 (やり直さない) | `failed` (`gemini:`) |
| `parse` | 502 | Gemini の応答が schema の形でない | `failed` (`parse:`) |

### `GET /health`

200 `ok` (`/healthz` は Cloud Run が使うので避けた)。

## 環境変数

| 名前 | 既定 | |
|---|---|---|
| `GEMINI_API_KEY` | (無し → 503) | Secret Manager の `gemini-api-key` を secretKeyRef で |
| `REDACT_2STAGE` | 2 段 | `0` で 1 段 |
| `GEMINI_ENDPOINT` / `GEMINI_MODEL` | `src/gemini.rs` の `DEFAULT_*` | 手元の確認で差し替える用 |
| `PORT` | 8080 | Cloud Run が入れる |
| `RUST_LOG` | `info` | ログは JSON |

## 配信

`.github/workflows/ci.yml`: PR = 検査と image の build / main への merge = staging / タグ `v*` = 本番。
タグは Tag Release (`tag-release.yml`、手動、`bump` = patch / minor / major) で打つ。手で `v*` を push しない。

image は GHCR (`ghcr.io/ippoan/alc-redact`) に push し、ippoan/ci-workflows の `cloud-run-deploy.yml` が
Artifact Registry の remote 経由で digest を固定して `services update` する。配信の job は repo の variables で
止めてあり、次が揃ってから有効にする:

| variable | 置き場 | 中身 |
|---|---|---|
| `GCP_REGION` / `GCP_WIF_PROVIDER` / `GCP_PROJECT_ID_STAGING` / `GCP_WIF_SERVICE_ACCOUNT_STAGING` | org (既存) | |
| `ALC_REDACT_AR_REPO` | repo | GHCR の image に対応する Artifact Registry の remote のパス (タグなし) |
| `CLOUD_RUN_SERVICE_STAGING` / `CLOUD_RUN_SERVICE_PRODUCTION` | repo | Cloud Run の service 名 |
| `GCP_PROJECT_ID_PRODUCTION` / `GCP_WIF_SERVICE_ACCOUNT_PRODUCTION` | repo | 本番の project と、WIF で impersonate する deployer の SA |
| `STAGING_DEPLOY_ENABLED` / `PRODUCTION_DEPLOY_ENABLED` | repo | `true` で配信の job が動く |

### 初回 (GCP への書き込みは PAM の申請 → オーナーの承認)

1. GHCR の package `alc-redact` を public にする (AR の remote が認証なしで引けるように)
2. staging・本番の service を作る: `--no-allow-unauthenticated`、runtime SA、`GEMINI_API_KEY` を secret `gemini-api-key` から、
   memory 512Mi・timeout 300s・concurrency は低め (pdfium は Mutex で直列)。`--allow-unauthenticated` と
   `--no-invoker-iam-check` は使わない
3. runtime SA に `gemini-api-key` の secretAccessor、呼び手の SA に service の `roles/run.invoker`、deployer の SA に WIF の impersonate
4. 上の variables を入れ、`*_DEPLOY_ENABLED=true`

## 開発

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
LD_LIBRARY_PATH=<libpdfium.so のあるディレクトリ> cargo test
docker build -t alc-redact .
```

pdfium は [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) の linux-x64。版と sha256 は Dockerfile の
`PDFIUM_VERSION` / `PDFIUM_SHA256` が正本 (CI もそこから読む)。
