# トラブルシュート

[ドキュメント目次に戻る](./README.md)

まず、接続先が起動していることと、指定したモデル ID が正しいことを確認してください。インストール済みなら `lait`、ソースから試している場合は各コマンドの先頭を `cargo run --` に置き換えて実行します。

## 症状から調べる

### `connection refused` になる

サーバーが起動していないか、接続先のホスト・ポートが違います。

- LM Studio の Developer（Local Server）画面でサーバーが起動しているか確認します。
- 既定値は `http://localhost:1234/v1` です。別の接続先なら `--base-url` または `OPENAI_BASE_URL` を指定します。
- 設定済みの接続先に問い合わせるには、`lait models --remote` を使います。

```sh
lait --base-url http://localhost:1234/v1 --model "<MODEL_ID>" "接続を確認してください"
lait models --remote
```

### `404` や「モデルが見つからない」になる

接続先の API ベース URL とモデル名を確認します。

- ベース URL は API の仕様に合わせて指定してください。LM Studio の既定値は `/v1` 付きですが、すべてのサーバーが同じパスを使うとは限りません。
- LM Studio でモデルをロードし、モデル ID を完全に一致させます。設定ファイルの alias を使っている場合は `lait models` で alias と解決先を確認します。
- サーバーが公開しているモデルを確認するには `lait models --remote` を使います。

### `401` や認証エラーになる

サーバーの認証設定と、lait が使う API キーを確認します。

- 一時的には `OPENAI_API_KEY` または `--api-key` を指定します。
- 設定ファイルでは `api_key`、`${VAR_NAME}`、`api_key_cmd` を使えます（詳しくは [設定ファイル](./config.md)）。
- API キーをシェルの履歴に残したくない場合は、CLI 引数ではなく環境変数または `.env` を使ってください。
- 認証なしの LM Studio では API キーを省略できます。

```sh
export OPENAI_API_KEY="your-api-key"
lait --model "<MODEL_ID>" "認証付きでリクエストしてください。"
```

### `--model` が不足していると言われる

次のいずれかでモデルを指定します。

```sh
lait --model "<MODEL_ID>" "こんにちは。"
export LLM_MODEL="<MODEL_ID>"
lait "こんにちは。"
```

設定ファイルを使う場合は `default.model` を指定できます。alias を登録した場合は、`lait models` で名前を確認してください。

### 応答が遅い、またはタイムアウトする

- LM Studio のモデルがロード済みか、サーバーログにエラーがないかを確認します。初回リクエストはモデルの準備に時間がかかることがあります。
- クラウド API ではネットワーク、レート制限、モデルの混雑も確認します。
- 対応モデルでは `--reasoning-effort` を下げ、`--max-tokens` を小さくすると、生成時間を短くできる場合があります。`--temperature` は出力のランダムさを調整する値で、生成長の上限ではありません。
- ワークフローの `timeout:` は一つのステップの制限です。ワークフロー全体の制限は `default.workflow_timeout` で設定します（[ワークフロー](./workflow.md)）。

### JSON、添付ファイル、ツールで失敗する

- `--json` は CLI の出力形式、`--json-schema` は API に Structured Outputs を要求する機能です。目的に合う方を選びます（[出力例](./output.md)）。
- `--file` はファイルの内容、`--image` は vision 対応モデル向けの画像入力です（[ファイル・画像の添付](./attachments.md)）。
- MCP、subagent、カスタムシェルツールの設定名と参照名が一致しているか確認し、必要なら `--approve-tools` で実行前に確認します（[MCP](./mcp.md)、[サブエージェント](./subagents.md)、[カスタムシェルツール](./tools.md)）。

## 終了コード

スクリプトや CI から失敗の種類を区別できるよう、`lait` は終了コードを使い分けます。

| コード | 意味 |
|---|---|
| `0` | 成功（`--help` / `--version` を含む） |
| `1` | 上記以外の一般的なエラー |
| `2` | コマンドライン引数のエラー（未知のフラグ、必須引数の不足など） |
| `3` | `lait lint` の指摘、または workflow / agent Markdown の構文エラー |
| `4` | モデル API のエラー（接続失敗、認証エラー、レート制限など） |
| `5` | `timeout:` によるタイムアウト、またはアプリケーション内のキャンセル |
| `130` | Ctrl-C（SIGINT）による中断 |

エラー時は `lait: <エラーメッセージ>` を標準エラー出力に表示します。Ctrl-C の通常の終了コードは `5` ではなく `130` です。

## サーバーへ実際に何が送られたか確認する

原因が特定しづらいときは `-v` または `-vv` を付け、標準エラー出力をファイルに保存します。

```sh
lait --model my-model -vv "この変更をレビューして" 2>trace.log
```

- `-v` — 解決後の `model_id` / `base_url` / サンプリングパラメータ、有効な `mcp` / `skills` / `subagents` / `tools`、ワークフローのステップ開始・終了・リトライ、ツール呼び出しの名前と引数
- `-vv` — 上記に加えてリクエスト／レスポンス JSON 全体

`LAIT_LOG` 環境変数（`tracing_subscriber::EnvFilter` の書式、例: `LAIT_LOG=debug`）を設定すると `-v` / `-vv` より優先されます。ログは標準エラー出力に出るため、応答本文のパイプ利用を壊しません。API キーは先頭4文字以外をマスクして表示します。
