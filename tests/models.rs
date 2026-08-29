mod support;

use support::{ConfigDirectory, MockServer, test_command};

const CONFIG: &str = r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: http://localhost:1234/v1
      model_id: test-model-id
      default_reasoning_effort: high
  other:
    - provider:
        base_url: https://api.example.com/v1
        api_key: ${EXAMPLE_KEY}
      model_id: other-model
"#;

#[test]
fn lists_configured_aliases_and_marks_the_default() {
    let config_dir = ConfigDirectory::new(CONFIG);
    let output = test_command()
        .current_dir(config_dir.path())
        .arg("models")
        .output()
        .expect("failed to execute lait models");

    assert!(output.status.success(), "lait models failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("NAME"), "table header expected: {stdout}");
    assert!(
        stdout.contains("*local") && stdout.contains("test-model-id"),
        "the default alias should be marked: {stdout}"
    );
    assert!(
        stdout.contains("reasoning=high"),
        "per-model defaults should be listed: {stdout}"
    );
    assert!(
        stdout.contains("other") && stdout.contains("other-model"),
        "every alias should be listed: {stdout}"
    );
}

#[test]
fn lists_configured_aliases_as_json() {
    let config_dir = ConfigDirectory::new(CONFIG);
    let output = test_command()
        .current_dir(config_dir.path())
        .args(["models", "--json"])
        .output()
        .expect("failed to execute lait models");

    assert!(output.status.success(), "lait models failed: {output:?}");
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--json output should be valid JSON");
    assert_eq!(json["default_model"], "local");
    let models = json["models"].as_array().expect("models should be a list");
    assert_eq!(models.len(), 2);
    let local = models
        .iter()
        .find(|model| model["name"] == "local")
        .expect("the 'local' alias should be listed");
    assert_eq!(local["default"], true);
    assert_eq!(local["model_id"], "test-model-id");
    assert_eq!(local["reasoning_effort"], "high");
}

#[test]
fn says_so_when_no_aliases_are_configured() {
    let config_dir = ConfigDirectory::empty();
    let output = test_command()
        .current_dir(config_dir.path())
        .arg("models")
        .output()
        .expect("failed to execute lait models");

    assert!(output.status.success(), "lait models failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no model aliases"),
        "an empty config should be reported: {stdout}"
    );
}

#[test]
fn remote_queries_the_servers_model_list() {
    let server = MockServer::start(
        "200 OK",
        r#"{"object":"list","data":[{"id":"model-a","object":"model"},{"id":"model-b","object":"model"}]}"#,
    );
    let output = test_command()
        .args(["models", "--remote", "--base-url", &server.base_url])
        .output()
        .expect("failed to execute lait models");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait models failed: {output:?}");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/v1/models");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "model-a\nmodel-b");
}

#[test]
fn remote_json_passes_the_server_response_through() {
    let body = r#"{"object":"list","data":[{"id":"model-a","object":"model"}]}"#;
    let server = MockServer::start("200 OK", body);
    let output = test_command()
        .args([
            "models",
            "--remote",
            "--json",
            "--base-url",
            &server.base_url,
        ])
        .output()
        .expect("failed to execute lait models");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait models failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), body);
}

#[test]
fn remote_reports_a_server_error_with_its_status() {
    let server = MockServer::start("500 Internal Server Error", r#"{"error":"boom"}"#);
    let output = test_command()
        .args(["models", "--remote", "--base-url", &server.base_url])
        .output()
        .expect("failed to execute lait models");
    server.receive_request();
    server.finish();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("500"),
        "the failure should name the HTTP status: {stderr}"
    );
}
