# 開発

[ドキュメント目次に戻る](./README.md)

## 前提

Rust stable を使用します。`Cargo.toml` の `rust-version` は 1.88 で、
`rust-toolchain.toml` により `rustfmt` と `clippy` も自動で有効になります。

## Rust の検証

リポジトリのルートで、次のコマンドを実行します。

```sh
cargo check --locked
cargo test --locked
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

`makers`（cargo-make）を使う場合は、対応するタスクを実行できます。

```sh
makers check
makers test
makers fmt-check
makers clippy
makers build
```

macOS arm64 で `ld: library not found for -liconv` が発生する場合は、`makers` の cargo
ラッパーが Apple Clang と Xcode SDK を設定して対応します。

## 実装の責務と拡張時の確認点

- `app` は CLI 引数から実行環境を組み立て、結果の出力と履歴記録を調整します。
  モデルへの要求・ツール呼び出しは `engine`、workflow の制御構造は `workflow` が担当します。
  `app/workflow_run.rs` はトップレベルの進捗・再開・チェックポイント・実行期限を管理します。
- `workflow::NodeSettings` はノード共通の設定を借用して参照するための型です。
  YAML の読み込み型はノード種別ごとに保ち、不適切なフィールドを引き続き拒否します。
  各制御構造の実行処理は `workflow/exec.rs` の専用関数に分かれています。
- `engine::ToolLoop` はモデルとの会話履歴、ツール集合、ラウンド上限、ツール実行を管理します。
  ストリームの有無にかかわらず同じ状態管理を使います。
- `engine/stream.rs` はストリームの集約と表示を扱います。出力先を `AsyncWrite` として
  渡せるため、標準出力の差し替えや実 API を使わずに reasoning、ツール呼び出し、キャンセルを
  検証できます。
- `frontmatter::parse` は agent・skill の文書構造、`async_cache::AsyncCache` は
  同じキーの読み込み共有とキャンセル、`registry` は一覧表示を担当します。
  各ファイルの内容の検証やパス解決は agent・skill 側に残します。
- `error::Interrupted` はキャンセルと実行期限超過を表します。中断を返すときはこの型を
  エラーチェーンに保持してください。終了コードはエラーの型から決まり、ファイル名や
  エラーメッセージに含まれる単語には依存しません。
- `lint` はファイル・設定の検査結果を `LintRun` に集め、テキスト・JSON・GitHub 向けに
  表示します。検査項目を追加する際は、表示形式ごとに検査処理を追加せず、共通の解析結果に
  追加します。
- `storage::write_atomic` は cache・cassette・checkpoint の保存を担当します。
  各書き込みが専用の一時ファイルを作り、完成後に同じディレクトリ内で rename します。
  同時に保存しても途中の内容を公開せず、一時ファイルの後始末も共通処理が行います。
  `fsync` による電源断時の永続性保証は行いません。

統合テストは `tests/support::test_command` を使い、実ユーザーのグローバル設定と履歴から
隔離します。履歴を複数コマンド間で共有するテストでは、一時ディレクトリを用意して
`XDG_DATA_HOME` を明示的に上書きします。並行処理の変更では、成功時だけでなく、待機中の
キャンセル・初期化失敗後の再試行・保存失敗時の後始末も検証してください。

## ドキュメントサイト

日本語ドキュメントの正本は `docs/usage/ja/` です。文書を変更したら、サイト側の生成コピーを
同期してから型チェックを実行します。

```sh
cd website
pnpm sync-docs
pnpm types:check
```

`pnpm build` は同期と Astro のチェック・ビルドをまとめて実行します。

GitHub Actions でも Rust の検証と `pnpm build` を実行します。
