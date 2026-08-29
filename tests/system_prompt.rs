mod support;

use support::{ConfigDirectory, MockServer, test_command, without_json_whitespace};

const RESPONSE: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#;

#[test]
fn sends_the_system_flag_as_a_system_message() {
    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .args(["--system", "you are a translator"])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#"{"role":"system","content":"youareatranslator"}"#),
        "request should carry the system message first: {body}"
    );
    assert!(
        body.contains(r#"{"role":"user","content":"hello"}"#),
        "request should still carry the user prompt: {body}"
    );
}

#[test]
fn reads_the_system_prompt_from_a_file() {
    let config_dir = ConfigDirectory::empty();
    let system_path = config_dir.path().join("system.txt");
    std::fs::write(&system_path, "system from file\n").expect("failed to write system file");

    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--system-file")
        .arg(&system_path)
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#"{"role":"system","content":"systemfromfile"}"#),
        "request should carry the file's system message: {body}"
    );
}

#[test]
fn rejects_system_and_system_file_together() {
    let output = test_command()
        .args(["--model", "test-model"])
        .args(["--system", "a", "--system-file", "b.txt"])
        .arg("hello")
        .output()
        .expect("failed to execute lait");

    assert!(!output.status.success());
}

#[test]
fn falls_back_to_default_system_from_the_config_file() {
    let config_dir = ConfigDirectory::new("default:\n  system: config system prompt\n");

    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .current_dir(config_dir.path())
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#"{"role":"system","content":"configsystemprompt"}"#),
        "request should carry the config's default system prompt: {body}"
    );
}

#[test]
fn the_system_flag_overrides_the_config_default() {
    let config_dir = ConfigDirectory::new("default:\n  system: config system prompt\n");

    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .current_dir(config_dir.path())
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .args(["--system", "cli wins"])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#"{"role":"system","content":"cliwins"}"#),
        "the CLI flag should win over default.system: {body}"
    );
    assert!(!body.contains("configsystemprompt"));
}
