# lait

Lightweight AI Tool (lait) は、YAML で定義したハーネス、Agent Loop、Flow を CLI から実行・制御するためのツールです。

## 必要な環境

- Rust stable
- `rustfmt` と `clippy`（`rust-toolchain.toml` により自動的に指定されます）

## 使い方

リポジトリのルートで次のコマンドを実行します。

```sh
cargo run -- --model <MODEL_ID_OR_ALIAS> "プロンプト"
```

`makers`（cargo-make）を使う場合は、CLI の引数を `run` タスクの後ろに渡します。

```sh
makers run -- --model <MODEL_ID> "プロンプト"
```

`makers run` は macOS では Apple Clang（`/usr/bin/clang`、`/usr/bin/clang++`）と
`xcrun --sdk macosx --show-sdk-path` の SDK を自動的に設定してから `cargo run` を実行します。
Nix など別の `cc` が PATH にあっても、`aws-lc-sys` のリンクに必要な macOS SDK が使われます。

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
| `--model <MODEL_ID_OR_ALIAS>` | `LLM_MODEL` | モデル ID または設定ファイルの alias。CLI 引数、環境変数、または設定ファイルで指定できます。 |
| `--api-key <KEY>` | `OPENAI_API_KEY` | API キー。任意。認証を有効にしたサーバーで指定します。 |
| `--show-reasoning` | — | 対応サーバーが返す `reasoning`（旧形式の `reasoning_content` にも対応）を回答前に表示します。既定では非表示です。 |
| `--json` | — | CLI の応答を JSON 形式で出力します。API の Structured Outputs を指定する `--json-schema` とは別の機能です。 |
| `--json-schema <FILE>` | — | API の Structured Outputs に使用する JSON Schema ファイル。指定時は `response_format` の `type` を `json_schema`、`strict` を `true` として送信します。 |
| `--schema-name <NAME>` | — | Structured Outputs のスキーマ名。既定値は `structured_output` です。`--json-schema` と組み合わせて使用します。 |
| `--reasoning-effort <EFFORT>` | `LLM_REASONING_EFFORT` | 推論の実行レベル。`none`、`minimal`、`low`、`medium`、`high`、`xhigh` のいずれかを指定します。未指定時は API リクエストにフィールドを追加しません。 |
| `<PROMPT>` | — | 送信する単一のプロンプト。 |

### 設定ファイル

CLI 引数や環境変数で指定していない値は、コマンドを実行したディレクトリの
`lait.config.yml` からデフォルトとして読み込まれます。設定できる基本項目は次のとおりです。

```yaml
# lait.config.yml
base_url: http://localhost:1234/v1
model: local-model
api_key: lm-studio
reasoning_effort: medium
```

#### モデル定義と alias

複数の呼び出しモデルを設定ファイルに定義し、alias で使い回せます。`models` は alias をキー、
モデル定義の配列を値にするマップです。各要素には `provider.base_url` と `model_id` を指定し、
`provider.api_key` と `default_reasoning_effort` は任意で指定できます。プロバイダーのキーは
正式名称の `provider` を使用してください。

```yaml
# lait.config.yml
models:
  local:
    - provider:
        base_url: http://localhost:1234/v1
      model_id: local-model
      default_reasoning_effort: medium
  cloud:
    - provider:
        base_url: https://api.example.com/v1
        api_key: your-api-key
      model_id: cloud-model
      default_reasoning_effort: high

# alias はトップレベルの `model`、CLI、環境変数から参照できます。
model: local
```

`model`、`--model`、`LLM_MODEL` には alias または生のモデル ID を指定できます。alias を指定した
場合は、対応する配列の先頭要素が使用され、その要素の `model_id` とプロバイダー設定が
リクエストに適用されます。生のモデル ID を指定した場合は、従来どおりトップレベル設定の
`base_url` などが使用されます。

設定値は項目ごとに、次の優先順位で解決されます。CLI 引数と環境変数の間では CLI 引数が優先されます。

`CLI 引数 > 環境変数 > モデル定義 > 既存トップレベル設定 > 組み込み既定値`

たとえば alias のモデル定義が `provider.base_url` を持つ場合、その値はトップレベルの `base_url`
より優先されます。CLI の `--base-url` や `OPENAI_BASE_URL` を指定した場合は、それらがモデル定義を
上書きします。`provider.api_key` と `default_reasoning_effort` を省略した場合は、対応する
トップレベルの `api_key`、`reasoning_effort` がフォールバックとして使用されます。

`base_url`、`model`、`api_key`、`reasoning_effort` の既存トップレベル形式も互換性のため引き続き
使用できます。設定ファイルの自動読込を
無効にする場合は `--no-config` を指定してください。この場合は設定ファイルを読み込まず、CLI
引数、環境変数、既定値だけが使用されます。

詳細なオプションは次のコマンドで確認できます。

```sh
cargo run -- --help
cargo run -- --version
makers --list-all-steps
```

### ビルドと実行

開発中は `cargo run` で実行できます。

```sh
cargo run -- --model "モデル ID" "Rustについて一文で説明してください。"
```

同じリクエストを `makers` から実行する場合は次のようにします。`--` より後ろの引数は
そのまま `lait` に転送されるため、プロンプト中の空白や日本語も保持されます。

```sh
makers run -- --model "モデル ID" "Rustについて一文で説明してください。"
```

推論モデルに推論の実行レベルを指定する場合は、`--reasoning-effort` に
`none`、`minimal`、`low`、`medium`、`high`、`xhigh` のいずれかを渡します。
`none` は推論を行わない指定です（サーバーとモデルが対応している場合）。未指定時は
`reasoning_effort` フィールドを送信せず、サーバー側の既定値を使用します。

```sh
cargo run -- --model "モデル ID" --reasoning-effort high "複雑な問題を解いてください。"
```

環境変数でも指定できます。CLI 引数と環境変数の両方を指定した場合は CLI 引数が優先されます。

```sh
export LLM_REASONING_EFFORT="medium"
cargo run -- --model "モデル ID" "複雑な問題を解いてください。"
```

ビルド、テスト、フォーマット確認、Clippy も `makers` のタスクとして実行できます。

```sh
makers build
makers test
makers fmt-check
makers clippy
```

対応サーバーから返された推論内容を回答前に表示する場合は、`--show-reasoning` を指定します。推論内容が返されない場合は、従来どおり回答のみが表示されます。

```sh
cargo run -- --show-reasoning --model "モデル ID" "Rustについて一文で説明してください。"
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

`makers` で実行する場合も同じ環境変数を利用できます。

```sh
export OPENAI_API_KEY="your-api-key"
makers run -- --model "モデル ID" "認証付きでリクエストしてください。"
```

一時的に CLI 引数で指定することもできます。

```sh
cargo run -- --api-key "your-api-key" --model "モデル ID" "こんにちは。"
```

一時的な指定は `makers` でも可能です。

```sh
makers run -- --api-key "your-api-key" --model "モデル ID" "こんにちは。"
```

### 出力例

リクエストが成功すると、モデルの応答テキストが標準出力に表示されます。応答内容はロードしたモデルによって異なります。

```text
こんにちは。今日はどのようなお手伝いができますか？
```

`--json-schema` を指定すると、API の Structured Outputs に JSON Schema を渡せます。`--json-schema` はモデルの応答形式を指定するオプションで、CLI の標準出力形式は変更しません。スキーマは JSON ファイルに記述します。

```json
{
  "type": "object",
  "properties": {
    "answer": { "type": "string" }
  },
  "required": ["answer"],
  "additionalProperties": false
}
```

```sh
cargo run -- --model "モデル ID" --json-schema response.schema.json --schema-name answer_response "JSON Schema に従って回答してください。"
```

`--schema-name` を省略した場合の名前は `structured_output` です。Structured Outputs は常に strict モード（`strict: true`）でリクエストされます。

`--json` を指定すると、API の応答を CLI 用の JSON オブジェクトとして標準出力に表示します。これは `--json-schema` とは別の機能で、`content` と `reasoning` のキーを常に含みます。

| キー | 型 | 説明 |
| --- | --- | --- |
| `content` | `string` | 回答テキスト。 |
| `reasoning` | `string` または `null` | 推論テキスト。推論がない場合は `null`。 |

```sh
cargo run -- --json --model "モデル ID" "Rustについて一文で説明してください。"
```

```json
{
  "content": "Rustは安全性と性能を両立したシステムプログラミング言語です。",
  "reasoning": null
}
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

`makers` で同じ検証を行う場合は `makers test`、`makers fmt-check`、`makers clippy`、
`makers build` を使用できます。macOS arm64 で発生する `ld: library not found for -liconv`
には、`makers` の cargo ラッパーが Apple Clang と Xcode SDK を設定して対応します。

GitHub Actions でも同じチェックを実行します。
