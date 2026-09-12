use super::types::{DefaultSettings, ResolvedModel, ToolPolicy};
use super::{
    ApiKeySource, ConfigFile, ConfigSource, McpServerConfig, McpTransport, ShellToolDefinition,
    check_shell_tool_definition, load_config, load_config_cancellable, resolve_endpoint,
    resolve_model,
};
use std::collections::HashMap;

/// A [`ResolvedModel`] fixture with only `base_url`/`api_key` set — the two
/// fields `resolve_endpoint`'s own tests below exercise.
fn resolved_model_with(base_url: Option<&str>, api_key: Option<&str>) -> ResolvedModel {
    ResolvedModel {
        base_url: base_url.map(str::to_owned),
        api_key: api_key.map(str::to_owned),
        ..ResolvedModel::default()
    }
}

/// `load_config`'s project-file read goes through
/// `async_io::read_to_string_sync` (via `load_config_at` ->
/// `config_from_read_result`) rather than a bare `std::fs::read_to_string`
/// — pins that the crate-wide 16MiB read limit applies here too. Mirrors
/// `async_io::read_to_string_sync_rejects_a_file_beyond_max_read_bytes`.
#[test]
fn load_config_rejects_a_project_file_beyond_max_read_bytes() {
    let path = crate::test_support::unique_temp_path("lait-config-read-limit", ".yml");
    std::fs::write(&path, vec![b'a'; crate::async_io::MAX_READ_BYTES + 1]).unwrap();

    let error = load_config(&ConfigSource::Explicit(path.clone())).unwrap_err();
    assert!(
        format!("{error:#}").contains("read limit"),
        "error: {error:#}"
    );
    let _ = std::fs::remove_file(path);
}

#[cfg(unix)]
#[tokio::test]
async fn cancellable_config_load_stops_waiting_for_a_fifo() {
    let path = crate::test_support::unique_temp_path("lait-config-fifo", ".yml");
    let status = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .expect("mkfifo should be available on Unix");
    assert!(status.success());

    let token = tokio_util::sync::CancellationToken::new();
    let source = ConfigSource::Explicit(path.clone());
    let mut load = Box::pin(load_config_cancellable(&source, Some(token.clone())));
    tokio::select! {
        result = &mut load => panic!("FIFO config unexpectedly loaded: {result:?}"),
        () = tokio::time::sleep(std::time::Duration::from_millis(50)) => token.cancel(),
    }
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), load)
        .await
        .expect("cancellable config load should finish promptly")
        .expect_err("a config FIFO without a writer should be cancelled");
    assert!(
        error
            .chain()
            .any(|cause| cause.is::<crate::error::Interrupted>()),
        "config cancellation should remain typed: {error:#}"
    );
    std::fs::remove_file(path).expect("config FIFO should be removable");
}

#[test]
fn tool_policy_allows_everything_by_default() {
    let policy = ToolPolicy::default();
    assert!(policy.allows("mock__echo"));
    assert!(policy.allows("anything"));
}

#[test]
fn tool_policy_deny_rejects_a_matching_name_even_if_allow_is_empty() {
    let policy = ToolPolicy {
        allow: vec![],
        deny: vec!["mock__echo".to_owned()],
    };
    assert!(!policy.allows("mock__echo"));
    assert!(policy.allows("mock__other"));
}

#[test]
fn tool_policy_non_empty_allow_rejects_an_unlisted_name() {
    let policy = ToolPolicy {
        allow: vec!["mock__echo".to_owned()],
        deny: vec![],
    };
    assert!(policy.allows("mock__echo"));
    assert!(!policy.allows("mock__other"));
}

#[test]
fn tool_policy_deny_wins_over_a_matching_allow() {
    let policy = ToolPolicy {
        allow: vec!["mock__echo".to_owned()],
        deny: vec!["mock__echo".to_owned()],
    };
    assert!(!policy.allows("mock__echo"));
}

#[test]
fn tool_policy_glob_matches_a_trailing_wildcard() {
    let policy = ToolPolicy {
        allow: vec!["fetch_*".to_owned()],
        deny: vec![],
    };
    assert!(policy.allows("fetch_url"));
    assert!(!policy.allows("delete_url"));
}

#[test]
fn tool_policy_glob_matches_a_leading_wildcard() {
    let policy = ToolPolicy {
        allow: vec![],
        deny: vec!["*_delete".to_owned()],
    };
    assert!(!policy.allows("mock__file_delete"));
    assert!(policy.allows("mock__file_read"));
}

#[test]
fn tool_policy_glob_matches_a_wildcard_on_both_ends_as_a_substring() {
    let policy = ToolPolicy {
        allow: vec![],
        deny: vec!["*delete*".to_owned()],
    };
    assert!(!policy.allows("fs__delete_file"));
    assert!(!policy.allows("fs__soft_delete"));
    assert!(policy.allows("fs__read_file"));
}

#[test]
fn tool_policy_bare_wildcard_matches_every_name() {
    let policy = ToolPolicy {
        allow: vec!["*".to_owned()],
        deny: vec![],
    };
    assert!(policy.allows("anything"));
}

#[test]
fn default_settings_merge_keeps_unset_global_fallbacks() {
    let merged = DefaultSettings::merge(
        DefaultSettings {
            model: Some("global-model".to_owned()),
            temperature: Some(0.2),
            ..DefaultSettings::default()
        },
        DefaultSettings {
            model: Some("project-model".to_owned()),
            top_p: Some(0.8),
            ..DefaultSettings::default()
        },
    );

    assert_eq!(merged.model.as_deref(), Some("project-model"));
    assert_eq!(merged.temperature, Some(0.2));
    assert_eq!(merged.top_p, Some(0.8));
}

#[test]
fn check_shell_tool_definition_rejects_an_empty_command() {
    let definition = ShellToolDefinition {
        description: None,
        command: vec![],
        parameters: serde_json::json!({ "type": "object" }),
        timeout: None,
    };
    let error = check_shell_tool_definition("echo", &definition).unwrap_err();
    assert!(error.to_string().contains("empty"));
}

#[test]
fn check_shell_tool_definition_rejects_non_object_parameters() {
    let definition = ShellToolDefinition {
        description: None,
        command: vec!["echo".to_owned()],
        parameters: serde_json::json!("not an object"),
        timeout: None,
    };
    let error = check_shell_tool_definition("echo", &definition).unwrap_err();
    assert!(error.to_string().contains("JSON object"));
}

#[test]
fn check_shell_tool_definition_accepts_a_valid_definition() {
    let definition = ShellToolDefinition {
        description: Some("echoes input".to_owned()),
        command: vec!["echo".to_owned(), "{{ input.text }}".to_owned()],
        parameters: serde_json::json!({ "type": "object" }),
        timeout: Some(5),
    };
    assert!(check_shell_tool_definition("echo", &definition).is_ok());
}

#[test]
fn resolve_model_rejects_an_empty_model_name() {
    let config = ConfigFile::default();
    assert!(resolve_model(String::new(), &config).is_err());
    assert!(resolve_model("   ".to_owned(), &config).is_err());
}

#[test]
fn resolve_model_passes_an_unaliased_name_through() {
    let config = ConfigFile::default();
    let resolved = resolve_model("some-model".to_owned(), &config).unwrap();
    assert_eq!(resolved.model_id, "some-model");
    assert!(resolved.base_url.is_none());
}

#[test]
fn resolve_endpoint_selects_the_first_available_api_key_source() {
    let config = ConfigFile {
        api_key: Some("config-key".to_owned()),
        ..ConfigFile::default()
    };
    let model = resolved_model_with(Some("https://model.example/v1"), Some("model-key"));
    let endpoint = resolve_endpoint(None, None, Some(&model), &config).unwrap();
    assert_eq!(
        endpoint.api_key,
        ApiKeySource::Literal("model-key".to_owned())
    );

    let endpoint =
        resolve_endpoint(None, Some("override-key".to_owned()), Some(&model), &config).unwrap();
    assert_eq!(
        endpoint.api_key,
        ApiKeySource::Literal("override-key".to_owned())
    );
}

#[test]
fn resolve_endpoint_keeps_api_key_commands_inert() {
    let config = ConfigFile {
        api_key_cmd: Some(super::CommandSpec::Argv(vec![
            "command-that-must-not-run".to_owned(),
        ])),
        ..ConfigFile::default()
    };
    let model = resolved_model_with(Some("https://model.example/v1"), None);
    let endpoint = resolve_endpoint(None, None, Some(&model), &config).unwrap();
    assert_eq!(
        endpoint.api_key,
        ApiKeySource::Command(super::CommandSpec::Argv(vec![
            "command-that-must-not-run".to_owned(),
        ]))
    );
}

#[test]
fn resolve_endpoint_expands_only_the_winning_base_url_layer() {
    let config = ConfigFile {
        base_url: Some("${config-base-url-must-not-be-read}".to_owned()),
        api_key: Some("${config-api-key-must-not-be-read}".to_owned()),
        ..ConfigFile::default()
    };

    let model = resolved_model_with(
        Some("${model-base-url-must-not-be-read}"),
        Some("${model-api-key-must-not-be-read}"),
    );
    let endpoint = resolve_endpoint(
        Some("http://override.example/v1///".to_owned()),
        Some("override-key".to_owned()),
        Some(&model),
        &config,
    )
    .unwrap();

    assert_eq!(endpoint.base_url, "http://override.example/v1");
    assert_eq!(
        endpoint.api_key,
        ApiKeySource::Literal("override-key".to_owned())
    );
}

#[test]
fn resolve_endpoint_does_not_expand_config_when_model_base_url_wins() {
    let config = ConfigFile {
        base_url: Some("${config-base-url-must-not-be-read}".to_owned()),
        ..ConfigFile::default()
    };

    let model = resolved_model_with(Some("http://model.example/v1///"), None);
    let endpoint = resolve_endpoint(None, None, Some(&model), &config).unwrap();

    assert_eq!(endpoint.base_url, "http://model.example/v1");
    assert_eq!(endpoint.api_key, ApiKeySource::Absent);
}

fn stdio_config(command: &str) -> McpServerConfig {
    McpServerConfig {
        command: Some(command.to_owned()),
        args: vec![],
        env: HashMap::new(),
        cwd: None,
        url: None,
        headers: HashMap::new(),
        allowed_tools: None,
    }
}

fn http_config(url: &str) -> McpServerConfig {
    McpServerConfig {
        command: None,
        args: vec![],
        env: HashMap::new(),
        cwd: None,
        url: Some(url.to_owned()),
        headers: HashMap::new(),
        allowed_tools: None,
    }
}

#[test]
fn resolves_a_stdio_server() {
    let transport = stdio_config("npx").resolve_transport("test").unwrap();
    match transport {
        McpTransport::Stdio { command, .. } => assert_eq!(command, "npx"),
        McpTransport::Http { .. } => panic!("expected a stdio transport"),
    }
}

#[test]
fn resolves_an_http_server() {
    let transport = http_config("https://example.com/mcp")
        .resolve_transport("test")
        .unwrap();
    match transport {
        McpTransport::Http { url, .. } => assert_eq!(url, "https://example.com/mcp"),
        McpTransport::Stdio { .. } => panic!("expected an http transport"),
    }
}

#[test]
fn rejects_a_server_with_neither_command_nor_url() {
    let config = McpServerConfig {
        command: None,
        args: vec![],
        env: HashMap::new(),
        cwd: None,
        url: None,
        headers: HashMap::new(),
        allowed_tools: None,
    };
    let error = config.resolve_transport("test").unwrap_err();
    assert!(error.to_string().contains("neither"));
}

#[test]
fn rejects_a_server_with_both_command_and_url() {
    let mut config = stdio_config("npx");
    config.url = Some("https://example.com/mcp".to_owned());
    let error = config.resolve_transport("test").unwrap_err();
    assert!(error.to_string().contains("both"));
}

#[test]
fn expands_placeholders_in_stdio_env_and_args() {
    // SAFETY: single-threaded test-only env mutation, restored immediately.
    unsafe {
        std::env::set_var("LAIT_TEST_MCP_TOKEN", "secret");
    }
    let mut config = stdio_config("npx");
    config
        .env
        .insert("TOKEN".to_owned(), "${LAIT_TEST_MCP_TOKEN}".to_owned());
    let transport = config.resolve_transport("test").unwrap();
    unsafe {
        std::env::remove_var("LAIT_TEST_MCP_TOKEN");
    }
    match transport {
        McpTransport::Stdio { env, .. } => {
            assert_eq!(env.get("TOKEN").map(String::as_str), Some("secret"));
        }
        McpTransport::Http { .. } => panic!("expected a stdio transport"),
    }
}
