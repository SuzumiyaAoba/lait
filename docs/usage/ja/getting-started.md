# はじめに

[ドキュメント目次に戻る](./README.md)

`lait` は `async-openai` を使って OpenAI Compatible API に接続し、単体のプロンプトをチャット補完として送信する CLI です。LM Studio のローカルサーバーを利用した動作確認や、LLM API 接続のサンプルとして使えます。

## 必要な環境

- Rust stable
- `rustfmt` と `clippy`（`rust-toolchain.toml` により自動的に指定されます）

## 基本的な使い方

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

## LM Studio の準備

実行前に、次の状態にしてください。

1. LM Studio を起動し、使用するモデルをロードする。
2. LM Studio の Developer（Local Server）画面からサーバーを起動する。
3. ロードしたモデルの ID（`<MODEL_ID>`）を確認する。

LM Studio の既定のエンドポイントは `http://localhost:1234/v1` です。別のホストやポートで起動している場合は、`--base-url` または `OPENAI_BASE_URL` で変更できます。

## CLI 引数と環境変数

| CLI 引数 | 環境変数 | 説明 |
| --- | --- | --- |
| `--base-url <URL>` | `OPENAI_BASE_URL` | OpenAI Compatible API のベース URL。既定値は `http://localhost:1234/v1`。 |
| `--model <MODEL_ID_OR_ALIAS>` | `LLM_MODEL` | モデル ID または設定ファイルの alias。CLI 引数、環境変数、または設定ファイルで指定できます。 |
| `--api-key <KEY>` | `OPENAI_API_KEY` | API キー。任意。認証を有効にしたサーバーで指定します。 |
| `--show-reasoning` | — | 対応サーバーが返す `reasoning`（旧形式の `reasoning_content` にも対応）を回答前に表示します。既定では非表示です。 |
| `--stream` | — | 応答が生成され次第、標準出力へ逐次書き出します。API へは `stream: true` を送信します。完全な応答をまとめて JSON 化する `--json` とは同時に指定できません。 |
| `--json` | — | CLI の応答を JSON 形式で出力します。API の Structured Outputs を指定する `--json-schema` とは別の機能です。`--stream` とは同時に指定できません。 |
| `--json-schema <FILE>` | — | API の Structured Outputs に使用する JSON Schema ファイル。指定時は `response_format` の `type` を `json_schema`、`strict` を `true` として送信します。 |
| `--schema-name <NAME>` | — | Structured Outputs のスキーマ名。既定値は `structured_output` です。`--json-schema` と組み合わせて使用します。 |
| `--reasoning-effort <EFFORT>` | `LLM_REASONING_EFFORT` | 推論の実行レベル。`none`、`minimal`、`low`、`medium`、`high`、`xhigh` のいずれかを指定します。未指定時は API リクエストにフィールドを追加しません。 |
| `--temperature <FLOAT>` | `LLM_TEMPERATURE` | サンプリング温度（`0.0`〜`2.0`）。低いほど決定的、高いほどランダムな応答になります。未指定時は API リクエストにフィールドを追加しません。 |
| `--top-p <FLOAT>` | `LLM_TOP_P` | nucleus sampling の確率質量（`0.0`〜`1.0`）。`--temperature` の代替として使います。未指定時は API リクエストにフィールドを追加しません。 |
| `--max-tokens <INT>` | `LLM_MAX_TOKENS` | 応答として生成するトークン数の上限（`1`以上）。API へは（非推奨の `max_tokens` ではなく）`max_completion_tokens` として送信されます。未指定時は API リクエストにフィールドを追加しません。 |
| `--mcp <NAME>` | — | `lait.config.yml` の `mcp_servers:` エントリ名。繰り返し指定可能。指定した MCP サーバーのツールをモデルに渡します（詳細は [MCP サーバーのツールを使う](./mcp.md)）。`--stream` とは同時に指定できません。 |
| `--subagent <NAME>` | — | `lait.config.yml` の `agents:` エントリ名。繰り返し指定可能。指定したエージェント Markdown ファイルを、モデル自身が呼び出すかどうか判断できる「サブエージェント」ツールとして渡します（詳細は [サブエージェントを使う](./subagents.md)）。`--stream` とは同時に指定できません。 |
| `--system <TEXT>` | — | ユーザープロンプトの前に送るシステムプロンプト。未指定時は `lait.config.yml` の `default.system` にフォールバックします。`--system-file` とは同時に指定できません。 |
| `--system-file <FILE>` | — | システムプロンプトをファイルから読み込みます。 |
| `--show-usage` | — | 応答後にトークン使用量（`prompt`/`completion`/`total`）を標準エラー出力へ表示します（標準出力のパイプ利用を壊しません）。`--stream` 指定時はサーバーに `stream_options: {"include_usage": true}` を要求します。`lait run`/`lait agent run` でも使え、ワークフローではステップごとの内訳と合計を表示します。 |
| `-o, --output <PATH>` | — | 応答本文を標準出力ではなく PATH に書き込みます（`--json` 併用時は JSON を書き込み、`-o -` は標準出力の明示指定）。`--stream` 以外では成功後にのみ書き込むため、失敗時に空ファイルが残りません。 |
| `--quiet` | — | 応答本文以外の注記（reasoning 表示・usage 表示）をすべて抑制します（`--show-reasoning`/`--show-usage` より優先）。 |
| `--no-config` | — | カレントディレクトリの `lait.config.yml` を読み込みません。 |
| `--no-env` | — | カレントディレクトリの `.env` を読み込みません（詳細は [設定ファイル](./config.md)）。 |
| `<PROMPT>` | — | 送信する単一のプロンプト。省略して標準入力から渡すこともできます（下記）。 |

### 標準入力からのプロンプト（パイプ対応）

標準入力が TTY でない場合、lait は標準入力を読み込みます。

- プロンプト引数が**ない**場合: 標準入力全体をプロンプトとして送信します。
- プロンプト引数が**ある**場合: 標準入力をコンテキストとしてプロンプトの後ろに連結します。
- `lait -` と書くと、標準入力をプロンプトとして明示的に読み込みます。

```sh
git diff | lait "この変更をレビューして"
cat question.txt | lait
```

同じ規則は `lait run <FILE> [PROMPT]` と `lait agent run <FILE> [INPUT]` の入力（`{{ input }}`）にも適用されます。

### そのほかのサブコマンド

| コマンド | 説明 |
| --- | --- |
| `lait models` | `lait.config.yml` の `models:` alias を一覧表示します（`default.model` に `*` マーク）。`--remote` でサーバーの `GET /v1/models` を照会、`--json` で機械可読出力。 |
| `lait init` | 最小の `lait.config.yml` を生成します。`lait init workflow [PATH]` / `lait init agent [PATH]` はコメント付きの雛形を生成します（既存ファイルは上書きしません）。 |
| `lait completions <SHELL>` | bash / zsh / fish / powershell / elvish の補完スクリプトを標準出力へ生成します。 |
| `lait man --dir <DIR>` | `lait.1`、`lait-run.1` などの man ページを生成します。 |

詳細なオプションは次のコマンドで確認できます。

```sh
cargo run -- --help
cargo run -- --version
makers --list-all-steps
```

## ビルドと実行

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

サンプリング温度・nucleus sampling・最大トークン数を指定する場合は、`--temperature`・`--top-p`・
`--max-tokens`（環境変数では `LLM_TEMPERATURE`・`LLM_TOP_P`・`LLM_MAX_TOKENS`）をそれぞれ渡します。
いずれも未指定時は API リクエストにフィールドを追加せず、サーバー側の既定値を使用します。範囲外の
値（`temperature` が `0.0`〜`2.0` の外、`top_p` が `0.0`〜`1.0` の外、`max_tokens` が `0`）はモデルを
呼び出す前にエラーになります。

```sh
cargo run -- --model "モデル ID" --temperature 0.7 --top-p 0.9 --max-tokens 512 "アイデアを出してください。"
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

## 認証なし／認証あり

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
