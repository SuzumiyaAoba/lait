# 設定ファイル

[ドキュメント目次に戻る](./README.md)

CLI 引数や環境変数で指定していない値は、コマンドを実行したディレクトリの
`lait.config.yml` からデフォルトとして読み込まれます。設定できる基本項目は次のとおりです。

```yaml
# lait.config.yml
base_url: http://localhost:1234/v1
api_key: lm-studio
default:
  model: local-model
  reasoning_effort: medium
```

## モデル定義と alias

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

# alias は `default.model`、CLI、環境変数から参照できます。
default:
  model: local
```

`default.model`、`--model`、`LLM_MODEL` には alias または生のモデル ID を指定できます。alias を指定した
場合は、対応する配列の先頭要素が使用され、その要素の `model_id` とプロバイダー設定が
リクエストに適用されます。生のモデル ID を指定した場合は、従来どおりトップレベル設定の
`base_url` などが使用されます。

## 設定値の優先順位

設定値は項目ごとに、次の優先順位で解決されます。CLI 引数と環境変数の間では CLI 引数が優先されます。

`CLI 引数 > 環境変数 > モデル定義 > 既存トップレベル設定 > 組み込み既定値`

たとえば alias のモデル定義が `provider.base_url` を持つ場合、その値はトップレベルの `base_url`
より優先されます。CLI の `--base-url` や `OPENAI_BASE_URL` を指定した場合は、それらがモデル定義を
上書きします。`provider.api_key` と `default_reasoning_effort` を省略した場合は、対応する
トップレベルの `api_key`、`default.reasoning_effort` がフォールバックとして使用されます。

`base_url`、`api_key` はトップレベルの項目として、フォールバック用の `model`、`reasoning_effort`
は `default:` の配下にまとめて指定します。設定ファイルの自動読込を
無効にする場合は `--no-config` を指定してください。この場合は設定ファイルを読み込まず、CLI
引数、環境変数、既定値だけが使用されます。
