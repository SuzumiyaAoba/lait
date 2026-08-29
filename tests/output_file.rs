mod support;

use support::{ConfigDirectory, MockServer, test_command};

const RESPONSE: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response","reasoning":"step one"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#;

#[test]
fn output_flag_writes_the_body_to_a_file_instead_of_stdout() {
    let dir = ConfigDirectory::empty();
    let out_path = dir.path().join("out.txt");

    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("-o")
        .arg(&out_path)
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    let written = std::fs::read_to_string(&out_path).expect("output file should exist");
    assert_eq!(written, "mock response\n");
}

#[test]
fn output_dash_writes_to_stdout() {
    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .args(["-o", "-"])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "mock response\n");
}

#[test]
fn output_flag_with_json_writes_the_json_object_to_the_file() {
    let dir = ConfigDirectory::empty();
    let out_path = dir.path().join("out.json");

    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--json")
        .arg("-o")
        .arg(&out_path)
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let written = std::fs::read_to_string(&out_path).expect("output file should exist");
    let json: serde_json::Value =
        serde_json::from_str(&written).expect("the file should hold valid JSON");
    assert_eq!(json["content"], "mock response");
}

#[test]
fn output_flag_sends_reasoning_to_stderr_keeping_the_file_body_only() {
    let dir = ConfigDirectory::empty();
    let out_path = dir.path().join("out.txt");

    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--show-reasoning")
        .arg("-o")
        .arg(&out_path)
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let written = std::fs::read_to_string(&out_path).expect("output file should exist");
    assert_eq!(written, "mock response\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Reasoning:\nstep one"),
        "reasoning should go to stderr: {stderr}"
    );
}

#[test]
fn a_failed_request_leaves_no_output_file_behind() {
    let dir = ConfigDirectory::empty();
    let out_path = dir.path().join("out.txt");

    let server = MockServer::start(
        "500 Internal Server Error",
        r#"{"error":{"message":"boom"}}"#,
    );
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("-o")
        .arg(&out_path)
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(!output.status.success());
    assert!(
        !out_path.exists(),
        "a failed request must not create the output file"
    );
}

#[test]
fn quiet_suppresses_reasoning_and_usage_notes() {
    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .args(["--quiet", "--show-reasoning", "--show-usage"])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "mock response\n");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

#[test]
fn streaming_writes_the_body_to_the_output_file() {
    let dir = ConfigDirectory::empty();
    let out_path = dir.path().join("out.txt");

    let server = MockServer::start_stream(&[
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"Hello, "},"finish_reason":null}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"world!"},"finish_reason":null}]}"#,
    ]);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--stream")
        .arg("-o")
        .arg(&out_path)
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    let written = std::fs::read_to_string(&out_path).expect("output file should exist");
    assert_eq!(written, "Hello, world!\n");
}
