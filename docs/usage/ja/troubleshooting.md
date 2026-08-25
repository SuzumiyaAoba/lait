# トラブルシュート

[ドキュメント目次に戻る](./README.md)

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
