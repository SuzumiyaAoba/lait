# lait

Lightweight AI Tool (lait) は、YAML で定義したハーネス、Agent Loop、Flow を CLI から実行・制御するためのツールです。

## 必要な環境

- Rust stable
- `rustfmt` と `clippy`（`rust-toolchain.toml` により自動的に指定されます）

## 使い方

リポジトリのルートで次のコマンドを実行します。

```sh
cargo run -- --model <MODEL_ID> "プロンプト"
```

`lait` は `async-openai` を使って OpenAI Compatible API に接続し、単体のプロンプトをチャット補完として送信する CLI です。LM Studio のローカルサーバーを利用した動作確認や、LLM API 接続のサンプルとして使えます。

### LM Studio の準備

実行前に、次の状態にしてください。

1. LM Studio を起動し、使用するモデルをロードする。
2. LM Studio の Developer（Local Server）画面からサーバーを起動する。
3. ロードしたモデルの ID（`<MODEL_ID>`）を確認する。

LM Studio の既定のエンドポイントは `http://localhost:1234/v1` です。別のホストやポートで起動している場合は、`--base-url` または `OPENAI_BASE_URL` で変更できます。

### CLI 引数と環境変数

| CLI 引数 | 環境変数 | 説明 |
| --- | --- | --- |
| `--base-url <URL>` | `OPENAI_BASE_URL` | OpenAI Compatible API のベース URL。既定値は `http://localhost:1234/v1`。 |
| `--model <MODEL_ID>` | `LLM_MODEL` | LM Studio でロードしたモデル ID。CLI 引数または環境変数のいずれかが必要です。 |
| `--api-key <KEY>` | `OPENAI_API_KEY` | API キー。任意。認証を有効にしたサーバーで指定します。 |
| `<PROMPT>` | — | 送信する単一のプロンプト。 |

同じ項目を両方で指定した場合は CLI 引数が優先されます。詳細なオプションは次のコマンドで確認できます。

```sh
cargo run -- --help
cargo run -- --version
```

### ビルドと実行

開発中は `cargo run` で実行できます。

```sh
cargo run -- --model "モデル ID" "Rustについて一文で説明してください。"
```

モデル ID に空白が含まれる場合は、上の例のように引用符で囲んでください。環境変数を使う場合は、次のように設定してからプロンプトだけを指定できます。

```sh
export LLM_MODEL="モデル ID"
cargo run -- "Rustについて一文で説明してください。"
```

リリースビルドを作成して実行する場合は、次のコマンドを使います。

```sh
cargo build --release --locked
./target/release/lait --model "モデル ID" "こんにちは。"
```

### 認証なし／認証あり

LM Studio のローカルサーバーで認証を無効にしている場合は、API キーを指定せずに実行できます。

```sh
cargo run -- --model "モデル ID" "こんにちは。"
```

サーバー側で認証を有効にしている場合は、`OPENAI_API_KEY` または `--api-key` でキーを指定します。シェルの履歴にキーを残したくない場合は環境変数を使ってください。

```sh
export OPENAI_API_KEY="your-api-key"
cargo run -- --model "モデル ID" "認証付きでリクエストしてください。"
```

一時的に CLI 引数で指定することもできます。

```sh
cargo run -- --api-key "your-api-key" --model "モデル ID" "こんにちは。"
```

### 出力例

リクエストが成功すると、モデルの応答テキストが標準出力に表示されます。応答内容はロードしたモデルによって異なります。

```text
こんにちは。今日はどのようなお手伝いができますか？
```

### トラブルシュート

- `connection refused` になる場合は、LM Studio の Local Server が起動しているか、ホストとポートが正しいか確認してください。既定値と異なる場合は `--base-url http://ホスト:ポート/v1` または `OPENAI_BASE_URL` を指定します。
- `404` やモデルが見つからないエラーになる場合は、ベース URL に `/v1` が含まれていることと、LM Studio でロード済みのモデル ID と `<MODEL_ID>` が完全に一致することを確認してください。
- `401` や認証エラーになる場合は、サーバーの認証設定を確認し、`OPENAI_API_KEY` または `--api-key` に正しいキーを指定してください。認証なしの LM Studio では API キーを省略します。
- `--model` が不足しているというエラーになる場合は、`--model <MODEL_ID>` を指定するか、`LLM_MODEL` を設定してください。
- 応答に時間がかかる場合は、モデルが LM Studio にロード済みか、LM Studio のサーバーログにエラーが出ていないかを確認してください。初回リクエストはモデルの準備に時間がかかることがあります。

CLI のヘルプとバージョンは以下で確認できます。

```sh
cargo run -- --help
cargo run -- --version
```

## 開発

テスト、フォーマット、Lint、リリースビルドは次のコマンドで実行できます。

```sh
cargo test --locked
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo build --release --locked
```

GitHub Actions でも同じチェックを実行します。
