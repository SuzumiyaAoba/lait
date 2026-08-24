mod support;

use support::{ConfigDirectory, MockServer, test_command, without_json_whitespace};

#[test]
fn loads_options_from_cwd_config_when_cli_and_environment_are_unset() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"config-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "model: config-model\nbase_url: \"{}\"\napi_key: config-key\nreasoning_effort: high\n",
        server.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/v1/chat/completions");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer config-key")
    );
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"config-model""#),
        "request body: {body}"
    );
    assert!(
        body.contains(r#""reasoning_effort":"high""#),
        "request body: {body}"
    );
}

#[test]
fn config_completes_an_omitted_base_url_when_model_is_given_on_cli() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"cli-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "model: config-model\nbase_url: \"{}\"\n",
        server.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .args(["--model", "cli-model", "hello"])
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(request.target, "/v1/chat/completions");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"cli-model""#),
        "request body: {body}"
    );
}

#[test]
fn cli_options_override_values_from_config() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"cli-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(
        "model: config-model\nbase_url: http://127.0.0.1:65535/v1\napi_key: config-key\nreasoning_effort: high\n",
    );

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "cli-model",
            "--base-url",
            server.base_url.as_str(),
            "--api-key",
            "cli-key",
            "--reasoning-effort",
            "none",
            "hello",
        ])
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer cli-key")
    );
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"cli-model""#),
        "request body: {body}"
    );
    assert!(
        body.contains(r#""reasoning_effort":"none""#),
        "request body: {body}"
    );
    assert!(
        !body.contains(r#""reasoning_effort":"high""#),
        "request body: {body}"
    );
}

#[test]
fn environment_options_override_values_from_config() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"env-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(
        "model: config-model\nbase_url: http://127.0.0.1:65535/v1\napi_key: config-key\nreasoning_effort: high\n",
    );

    let output = test_command()
        .current_dir(config.path())
        .env("LLM_MODEL", "env-model")
        .env("OPENAI_BASE_URL", server.base_url.as_str())
        .env("OPENAI_API_KEY", "env-key")
        .env("LLM_REASONING_EFFORT", "minimal")
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer env-key")
    );
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"env-model""#),
        "request body: {body}"
    );
    assert!(
        body.contains(r#""reasoning_effort":"minimal""#),
        "request body: {body}"
    );
}

#[test]
fn resolves_the_first_model_definition_for_a_top_level_alias() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"resolved-first","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "model: local-alias\nbase_url: \"{}/legacy\"\napi_key: legacy-key\nreasoning_effort: low\nmodels:\n  local-alias:\n    - provider:\n        base_url: \"{}\"\n        api_key: model-key\n      model_id: resolved-first\n      default_reasoning_effort: high\n    - provider:\n        base_url: \"{}\"\n        api_key: second-key\n      model_id: resolved-second\n      default_reasoning_effort: none\n",
        server.base_url, server.base_url, server.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(request.target, "/v1/chat/completions");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer model-key")
    );
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"resolved-first""#),
        "request body: {body}"
    );
    assert!(
        body.contains(r#""reasoning_effort":"high""#),
        "request body: {body}"
    );
    assert!(
        !body.contains(r#""model":"resolved-second""#),
        "request body should use the first model definition: {body}"
    );
}

#[test]
fn cli_options_override_a_model_definition_for_an_alias() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"cli-resolved-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "model: config-model\nbase_url: \"{}/legacy\"\napi_key: legacy-key\nreasoning_effort: low\nmodels:\n  cli-alias:\n    - provider:\n        base_url: \"{}/definition\"\n        api_key: model-key\n      model_id: cli-resolved-model\n      default_reasoning_effort: high\n",
        server.base_url, server.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "cli-alias",
            "--base-url",
            server.base_url.as_str(),
            "--api-key",
            "cli-key",
            "--reasoning-effort",
            "none",
            "hello",
        ])
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(request.target, "/v1/chat/completions");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer cli-key")
    );
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"cli-resolved-model""#),
        "request body: {body}"
    );
    assert!(
        body.contains(r#""reasoning_effort":"none""#),
        "request body: {body}"
    );
}

#[test]
fn environment_options_override_a_model_definition_for_an_alias() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"env-resolved-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "model: config-model\nbase_url: \"{}/legacy\"\napi_key: legacy-key\nreasoning_effort: low\nmodels:\n  env-alias:\n    - provider:\n        base_url: \"{}/definition\"\n        api_key: model-key\n      model_id: env-resolved-model\n      default_reasoning_effort: high\n",
        server.base_url, server.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .env("LLM_MODEL", "env-alias")
        .env("OPENAI_BASE_URL", server.base_url.as_str())
        .env("OPENAI_API_KEY", "env-key")
        .env("LLM_REASONING_EFFORT", "minimal")
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(request.target, "/v1/chat/completions");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer env-key")
    );
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"env-resolved-model""#),
        "request body: {body}"
    );
    assert!(
        body.contains(r#""reasoning_effort":"minimal""#),
        "request body: {body}"
    );
}

#[test]
fn falls_back_to_legacy_top_level_values_when_model_definition_fields_are_missing() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"fallback-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "model: fallback-alias\nbase_url: \"{}/legacy\"\napi_key: legacy-key\nreasoning_effort: medium\nmodels:\n  fallback-alias:\n    - provider:\n        base_url: \"{}\"\n      model_id: fallback-model\n",
        server.base_url, server.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(request.target, "/v1/chat/completions");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer legacy-key")
    );
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"fallback-model""#),
        "request body: {body}"
    );
    assert!(
        body.contains(r#""reasoning_effort":"medium""#),
        "request body: {body}"
    );
}

#[test]
fn uses_the_default_api_key_when_no_provider_or_legacy_key_is_configured() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"default-key-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "model: default-key-alias\nmodels:\n  default-key-alias:\n    - provider:\n        base_url: \"{}\"\n      model_id: default-key-model\n",
        server.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer lm-studio")
    );
}

#[test]
fn sends_an_unknown_top_level_alias_to_the_api_unchanged() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"unknown-alias","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "model: unknown-alias\nbase_url: \"{}\"\nmodels:\n  known-alias:\n    - provider:\n        base_url: \"{}\"\n      model_id: known-model\n",
        server.base_url, server.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"unknown-alias""#),
        "request body: {body}"
    );
    assert!(
        !body.contains(r#""model":"known-model""#),
        "request body should preserve an unknown alias: {body}"
    );
}

#[test]
fn sends_an_unknown_cli_model_id_to_the_api_unchanged() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"raw-model-id","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "models:\n  known-alias:\n    - provider:\n        base_url: \"{}\"\n      model_id: known-model\n",
        server.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--model",
            "raw-model-id",
            "--base-url",
            server.base_url.as_str(),
            "hello",
        ])
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"raw-model-id""#),
        "request body: {body}"
    );
}

#[test]
fn no_config_option_skips_a_malformed_config_file() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"cli-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new("model: [\n");

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--no-config",
            "--model",
            "cli-model",
            "--base-url",
            server.base_url.as_str(),
            "--api-key",
            "cli-key",
            "hello",
        ])
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(request.target, "/v1/chat/completions");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"cli-model""#),
        "request body: {body}"
    );
}
