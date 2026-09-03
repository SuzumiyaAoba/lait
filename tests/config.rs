mod support;

use support::{
    ConfigDirectory, GlobalConfigDirectory, MockServer, test_command, without_json_whitespace,
};

#[test]
fn loads_options_from_cwd_config_when_cli_and_environment_are_unset() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"config-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "default:\n  model: config-model\n  reasoning_effort: high\nbase_url: \"{}\"\napi_key: config-key\n",
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
        "default:\n  model: config-model\nbase_url: \"{}\"\n",
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
        "default:\n  model: config-model\n  reasoning_effort: high\nbase_url: http://127.0.0.1:65535/v1\napi_key: config-key\n",
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
        "default:\n  model: config-model\n  reasoning_effort: high\nbase_url: http://127.0.0.1:65535/v1\napi_key: config-key\n",
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
        "default:\n  model: local-alias\n  reasoning_effort: low\nbase_url: \"{}/legacy\"\napi_key: legacy-key\nmodels:\n  local-alias:\n    - provider:\n        base_url: \"{}\"\n        api_key: model-key\n      model_id: resolved-first\n      default_reasoning_effort: high\n    - provider:\n        base_url: \"{}\"\n        api_key: second-key\n      model_id: resolved-second\n      default_reasoning_effort: none\n",
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
        "default:\n  model: config-model\n  reasoning_effort: low\nbase_url: \"{}/legacy\"\napi_key: legacy-key\nmodels:\n  cli-alias:\n    - provider:\n        base_url: \"{}/definition\"\n        api_key: model-key\n      model_id: cli-resolved-model\n      default_reasoning_effort: high\n",
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
fn resolves_temperature_top_p_and_max_tokens_from_a_model_definition_for_an_alias() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"sampling-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "default:\n  model: sampling-alias\nmodels:\n  sampling-alias:\n    - provider:\n        base_url: \"{}\"\n      model_id: sampling-model\n      default_temperature: 0.4\n      default_top_p: 0.6\n      default_max_tokens: 128\n",
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
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""temperature":0.4"#),
        "request body: {body}"
    );
    assert!(body.contains(r#""top_p":0.6"#), "request body: {body}");
    assert!(
        body.contains(r#""max_completion_tokens":128"#),
        "request body: {body}"
    );
}

#[test]
fn cli_temperature_top_p_and_max_tokens_override_a_model_definition_for_an_alias() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"sampling-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "default:\n  model: sampling-alias\nmodels:\n  sampling-alias:\n    - provider:\n        base_url: \"{}\"\n      model_id: sampling-model\n      default_temperature: 0.4\n      default_top_p: 0.6\n      default_max_tokens: 128\n",
        server.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .args([
            "--temperature",
            "0.9",
            "--top-p",
            "0.99",
            "--max-tokens",
            "512",
            "hello",
        ])
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""temperature":0.9"#),
        "request body: {body}"
    );
    assert!(body.contains(r#""top_p":0.99"#), "request body: {body}");
    assert!(
        body.contains(r#""max_completion_tokens":512"#),
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
        "default:\n  model: config-model\n  reasoning_effort: low\nbase_url: \"{}/legacy\"\napi_key: legacy-key\nmodels:\n  env-alias:\n    - provider:\n        base_url: \"{}/definition\"\n        api_key: model-key\n      model_id: env-resolved-model\n      default_reasoning_effort: high\n",
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
        "default:\n  model: fallback-alias\n  reasoning_effort: medium\nbase_url: \"{}/legacy\"\napi_key: legacy-key\nmodels:\n  fallback-alias:\n    - provider:\n        base_url: \"{}\"\n      model_id: fallback-model\n",
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
        "default:\n  model: default-key-alias\nmodels:\n  default-key-alias:\n    - provider:\n        base_url: \"{}\"\n      model_id: default-key-model\n",
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
        "default:\n  model: unknown-alias\nbase_url: \"{}\"\nmodels:\n  known-alias:\n    - provider:\n        base_url: \"{}\"\n      model_id: known-model\n",
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
    let config = ConfigDirectory::new("default: [\n");

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

#[test]
fn finds_lait_config_yml_in_an_ancestor_directory() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"config-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "default:\n  model: config-model\nbase_url: \"{}\"\n",
        server.base_url
    ));
    let subdir = config.path().join("nested").join("deeper");
    std::fs::create_dir_all(&subdir).expect("failed to create nested test directory");

    let output = test_command()
        .current_dir(&subdir)
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"config-model""#),
        "request body: {body}"
    );
}

#[test]
fn dash_dash_config_reads_an_explicit_path_ignoring_cwd() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"explicit-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let explicit_config = ConfigDirectory::new(&format!(
        "default:\n  model: explicit-model\nbase_url: \"{}\"\n",
        server.base_url
    ));
    // An unrelated, config-less cwd: proves `--config` is used instead of
    // (not merely in addition to) any cwd-relative search.
    let cwd = ConfigDirectory::empty();

    let output = test_command()
        .current_dir(cwd.path())
        .args(["--config"])
        .arg(explicit_config.config_path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"explicit-model""#),
        "request body: {body}"
    );
}

#[test]
fn dash_dash_config_errors_clearly_on_a_missing_path() {
    let cwd = ConfigDirectory::empty();

    let output = test_command()
        .current_dir(cwd.path())
        .args(["--config", "does-not-exist.yml", "--no-history", "hello"])
        .output()
        .expect("failed to execute lait");

    assert!(
        !output.status.success(),
        "expected --config with a missing path to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("does-not-exist.yml"), "stderr: {stderr}");
}

#[test]
fn dash_dash_config_conflicts_with_no_config() {
    let output = test_command()
        .args(["--config", "some.yml", "--no-config", "hello"])
        .output()
        .expect("failed to execute lait");

    assert!(
        !output.status.success(),
        "expected --config and --no-config together to be a usage error"
    );
}

#[test]
fn global_config_alone_resolves_a_model() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"global-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let global = GlobalConfigDirectory::new(&format!(
        "default:\n  model: global-model\nbase_url: \"{}\"\n",
        server.base_url
    ));
    // No project lait.config.yml anywhere in this cwd's ancestry — the
    // global file alone must be enough to resolve a model.
    let cwd = ConfigDirectory::empty();

    let output = test_command()
        .current_dir(cwd.path())
        .env("XDG_CONFIG_HOME", global.xdg_config_home())
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"global-model""#),
        "request body: {body}"
    );
}

#[test]
fn project_config_overrides_a_same_named_global_default() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"project-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    // The global file sets both `default.model` and `base_url`; the project
    // file only overrides the model, so the base_url merge (falling back to
    // the global value) is exercised too.
    let global = GlobalConfigDirectory::new(&format!(
        "default:\n  model: global-model\nbase_url: \"{}\"\n",
        server.base_url
    ));
    let project = ConfigDirectory::new("default:\n  model: project-model\n");

    let output = test_command()
        .current_dir(project.path())
        .env("XDG_CONFIG_HOME", global.xdg_config_home())
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"project-model""#),
        "request body: {body}"
    );
}

#[test]
fn dash_dash_config_does_not_read_the_global_config() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"explicit-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    // If `--config` leaked into merging with the global file, the request
    // would carry `global-model` instead.
    let global = GlobalConfigDirectory::new("default:\n  model: global-model\n");
    let explicit_config = ConfigDirectory::new(&format!(
        "default:\n  model: explicit-model\nbase_url: \"{}\"\n",
        server.base_url
    ));
    let cwd = ConfigDirectory::empty();

    let output = test_command()
        .current_dir(cwd.path())
        .env("XDG_CONFIG_HOME", global.xdg_config_home())
        .args(["--config"])
        .arg(explicit_config.config_path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"explicit-model""#),
        "request body: {body}"
    );
}

#[test]
fn no_config_ignores_the_global_config_too() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"cli-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let global = GlobalConfigDirectory::new(&format!(
        "default:\n  model: global-model\nbase_url: \"{}\"\napi_key: global-key\n",
        server.base_url
    ));
    let cwd = ConfigDirectory::empty();

    let output = test_command()
        .current_dir(cwd.path())
        .env("XDG_CONFIG_HOME", global.xdg_config_home())
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
    assert!(
        !request.headers.to_ascii_lowercase().contains("global-key"),
        "expected --no-config to ignore the global config's api_key, headers: {}",
        request.headers
    );
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"cli-model""#),
        "request body: {body}"
    );
}

#[test]
fn agent_list_shows_a_globally_registered_agent() {
    let global = GlobalConfigDirectory::new("agents:\n  greeter: ./agents/greeter.md\n");
    let agents_dir = global.config_dir().join("agents");
    std::fs::create_dir_all(&agents_dir).expect("failed to create test agents directory");
    std::fs::write(
        agents_dir.join("greeter.md"),
        "---\ndescription: greets the user\n---\nHello {{ input }}\n",
    )
    .expect("failed to write test agent file");
    let cwd = ConfigDirectory::empty();

    let output = test_command()
        .current_dir(cwd.path())
        .env("XDG_CONFIG_HOME", global.xdg_config_home())
        .args(["agent", "list"])
        .output()
        .expect("failed to execute lait agent list");

    assert!(output.status.success(), "agent list failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("greeter"), "stdout: {stdout}");
    assert!(stdout.contains("greets the user"), "stdout: {stdout}");
}
