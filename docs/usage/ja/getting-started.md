# はじめに

[ドキュメント目次に戻る](./README.md)

`lait` は OpenAI Compatible API にプロンプトを送信する CLI です。LM Studio のようなローカルサーバーにも、認証が必要なクラウド API にも接続できます。

## インストール

通常利用では、次のいずれかを選んでください。

- バイナリ: [GitHub Releases](https://github.com/SuzumiyaAoba/lait/releases) から、お使いの OS 向けのアーカイブを取得します。
- Homebrew（macOS）:

  ```sh
  brew install SuzumiyaAoba/tap/lait
  ```

- `cargo binstall`:

  ```sh
  cargo binstall lait
  ```

ソースコードを変更しながら試す場合は、リポジトリを取得して `cargo run --` を使います。Rust の導入やリリース手順は [開発](./development.md) を参照してください。

## 最短手順

次の4段階で最初のリクエストを送れます。

1. `lait` をインストールする（開発中はこのページの例の `cargo run --` に置き換える）。
2. LM Studio などの OpenAI Compatible API を起動し、使用するモデル ID を確認する。
3. `lait --model "<MODEL_ID>" "こんにちは。"` を実行する。
4. 接続先やモデルを毎回指定したくなったら、`lait init` で `lait.config.yml` を作成する。

インストール済みのバイナリを実行する例:

```sh
lait --model "<MODEL_ID>" "Rustについて一文で説明してください。"
```

ソースから開発中に実行する例:

```sh
cargo run -- --model "<MODEL_ID>" "Rustについて一文で説明してください。"
```

## 必要な環境

- 実行時: LM Studio などの OpenAI Compatible API と、そこで利用できるモデル。
- ソースからビルドする場合: Rust stable、`rustfmt`、`clippy`（`rust-toolchain.toml` により自動的に指定されます）。

### `makers` を使う場合

`cargo-make` の `makers` をインストール済みなら、CLI の引数を `run` タスクの後ろに渡せます。通常の利用では `lait`、ソース開発では `cargo run --` を使えば十分です。

```sh
makers run -- --model <MODEL_ID> "プロンプト"
```

`makers run` は macOS では Apple Clang（`/usr/bin/clang`、`/usr/bin/clang++`）と
`xcrun --sdk macosx --show-sdk-path` の SDK を自動的に設定してから `cargo run` を実行します。
Nix など別の `cc` が PATH にあっても、`aws-lc-sys` のリンクに必要な macOS SDK が使われます。

## 接続先の準備（LM Studio の例）

実行前に、次の状態にしてください。

1. LM Studio を起動し、使用するモデルをロードする。
2. LM Studio の Developer（Local Server）画面からサーバーを起動する。
3. ロードしたモデルの ID（`<MODEL_ID>`）を確認する。

LM Studio の既定のエンドポイントは `http://localhost:1234/v1` です。別のホストやポートで起動している場合は、`--base-url` または `OPENAI_BASE_URL` で変更できます。LM Studio 以外を使う場合も、接続先が案内する OpenAI Compatible API のベース URL を同じ方法で指定してください。

## CLI 引数と環境変数

よく使うものから順にまとめています。ワークフローや agent 固有のオプションは、それぞれの専門ページを参照してください。

### 接続と入力

| CLI 引数 | 環境変数 | 説明 |
| --- | --- | --- |
| `--model <MODEL_ID_OR_ALIAS>` | `LLM_MODEL` | モデル ID または設定ファイルの alias。未指定時は `default.model` を使います。 |
| `--base-url <URL>` | `OPENAI_BASE_URL` | API のベース URL。未指定時は `http://localhost:1234/v1` です。 |
| `--api-key <KEY>` | `OPENAI_API_KEY` | 認証が必要なサーバーの API キー。不要なサーバーでは省略できます。 |
| `<PROMPT>` | — | 送信するプロンプト。省略時は標準入力から読み込みます（下記）。 |

### 出力・入力の形式

| CLI 引数 | 説明 |
| --- | --- |
| `--stream` | 応答を生成された部分から表示します。`--json` とは併用できません。MCP や subagent のツール呼び出しとも併用できます。 |
| `--json` | CLI の応答を JSON（`content`、`reasoning`、`usage`）で出力します。Structured Outputs の指定とは別の機能です。 |
| `--json-schema <FILE>` | API の Structured Outputs に使う JSON Schema ファイルです。スキーマ名は `--schema-name <NAME>`（既定値 `structured_output`）で変更できます。詳しくは [出力例](./output.md) を参照してください。 |
| `--show-reasoning` | 対応サーバーが返す `reasoning`（旧形式の `reasoning_content` を含む）を回答前に表示します。 |
| `--show-usage` | トークン使用量を標準エラー出力に表示します。`lait run` や `lait agent run` でも使えます。 |
| `-o, --output <PATH>` | 応答を PATH に書き込みます。`-o -` は標準出力です。非ストリーミング時は成功後に書き込み、ストリーミング時は生成中に書き込みます。 |
| `--render` | 端末で応答を Markdown として表示します。詳しくは [出力例](./output.md) を参照してください。 |
| `--quiet` | reasoning や usage など本文以外の注記を抑制します。 |
| `--file <PATH>` | ファイルの内容をプロンプトに添付します（繰り返し可）。詳しくは [ファイル・画像の添付](./attachments.md) を参照してください。 |
| `--image <PATH_OR_URL>` | 画像を添付します（繰り返し可）。vision 対応モデルが必要です。 |
| `--session <NAME>` | 会話を保存・再開します。詳しくは [会話セッションと対話モード](./chat.md) を参照してください。 |
| `-p, --prompt-name <NAME>` | `prompts:` の名前付きテンプレートを使います。`--var KEY=VALUE` で変数を渡せます（[名前付きプロンプト](./prompts.md)）。 |

### モデル・ツール・設定の詳細

| CLI 引数 | 環境変数 | 説明 |
| --- | --- | --- |
| `--reasoning-effort <EFFORT>` | `LLM_REASONING_EFFORT` | 推論レベル（`none`、`minimal`、`low`、`medium`、`high`、`xhigh`）。 |
| `--temperature <FLOAT>` | `LLM_TEMPERATURE` | サンプリング温度（`0.0`〜`2.0`）。 |
| `--top-p <FLOAT>` | `LLM_TOP_P` | nucleus sampling の確率質量（`0.0`〜`1.0`）。 |
| `--max-tokens <INT>` | `LLM_MAX_TOKENS` | 生成トークン数の上限（`1`以上）。 |
| `--system <TEXT>` | — | システムプロンプトを指定します。`--system-file` と同時には使えません。 |
| `--system-file <FILE>` | — | システムプロンプトをファイルから読み込みます。 |
| `--mcp <NAME>` | — | `mcp_servers:` の MCP サーバーをツールとして渡します（繰り返し可）。`--stream` と併用できます（[MCP](./mcp.md)）。 |
| `--subagent <NAME>` | — | `agents:` の agent を subagent ツールとして渡します（繰り返し可）。`--stream` と併用できます（[サブエージェント](./subagents.md)）。 |
| `--tool <NAME>` | — | `tools:` のカスタムシェルツールを渡します（繰り返し可、[カスタムシェルツール](./tools.md)）。 |
| `--var KEY=VALUE` | — | テンプレート変数を指定します。`lait run` や名前付きプロンプトで使えます。 |
| `--config <PATH>` | — | 指定した設定ファイルだけを読み込みます。 |
| `--no-config` | — | 設定ファイルを読み込みません。 |
| `--no-env` | — | カレントディレクトリの `.env` を読み込みません（[設定ファイル](./config.md)）。 |
| `--cache` / `--no-cache` | — | 応答キャッシュを有効化／無効化します（`--stream` はキャッシュされません）。 |
| `-v`, `--verbose` | `LAIT_LOG` | 詳細ログを標準エラー出力に出します。`-vv` ではリクエスト／レスポンス JSON も表示します（[トラブルシュート](./troubleshooting.md)）。 |
| `--approve-tools` | — | ツール実行前に対話的な承認を求めます（[MCP](./mcp.md)）。 |

### 標準入力からのプロンプト（パイプ対応）

標準入力が TTY でない場合、lait は標準入力を読み込みます。

- プロンプト引数が**ない**場合: 標準入力全体をプロンプトとして送信します。
- プロンプト引数が**ある**場合: 標準入力をコンテキストとしてプロンプトの後ろに連結します。
- `lait -` と書くと、標準入力をプロンプトとして明示的に読み込みます。

```sh
git diff | lait "この変更をレビューして"
cat question.txt | lait
```

同じ規則は `lait run <FILE> [PROMPT]` と `lait agent run <FILE> [INPUT]` にも適用されます。agent の入力は JSON として解釈できる場合があり、その場合はシステムプロンプトから `{{ input.field }}` のように参照できます。

### そのほかのサブコマンド

| コマンド | 説明 |
| --- | --- |
| `lait models` | `lait.config.yml` の `models:` alias を一覧表示します（`default.model` に `*` マーク）。`--remote` でサーバーの `GET /v1/models` を照会、`--json` で機械可読出力。 |
| `lait init` | 最小の `lait.config.yml` を生成します。`lait init workflow [PATH]` / `lait init agent [PATH]` はコメント付きの雛形を生成します（既存ファイルは上書きしません）。 |
| `lait completions <SHELL>` | bash / zsh / fish / powershell / elvish の補完スクリプトを標準出力へ生成します。 |
| `lait man --dir <DIR>` | `lait.1`、`lait-run.1` などの man ページを生成します。 |
| `lait cache clear` | `.lait/cache/` の内容をすべて削除します（詳細は [設定ファイル](./config.md)）。 |

詳細なオプションは次のコマンドで確認できます。

```sh
cargo run -- --help   # ソースから開発中の場合
cargo run -- --version
lait --help           # インストール済みの場合
lait --version
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

## Ctrl-C（SIGINT）による中断

実行中に Ctrl-C を押すと、`lait` は進行中のモデルリクエスト・MCP サーバーとの通信・
`command` の実行などをキャンセルしてから終了します（`--stream` で出力の途中だった場合、
そこまでの出力はそのまま残ります）。[`lait run --checkpoint`](./workflow.md)を付けていた
場合は、中断された時点までの状態がチェックポイントとして保存され、`--resume` で
再開できます。終了コードはシェルの慣習に合わせて `130` になります。

後始末が固まってしまった場合に備えて、Ctrl-C をもう一度押すと後始末を待たずに
即座に終了します。
