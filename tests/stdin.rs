mod support;

use support::{LaitCommand, MockServer, without_json_whitespace};

const RESPONSE: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#;

#[test]
fn uses_piped_stdin_as_the_prompt_when_no_argument_is_given() {
    let server = MockServer::start("200 OK", RESPONSE);
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .opt_prompt(None)
        .spawn_with_stdin("piped prompt\n");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""content":"pipedprompt""#),
        "request should carry the piped prompt: {body}"
    );
}

#[test]
fn appends_piped_stdin_to_the_prompt_argument_as_context() {
    let server = MockServer::start("200 OK", RESPONSE);
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .opt_prompt(Some("review this"))
        .spawn_with_stdin("diff text\n");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""content":"reviewthis\n\ndifftext""#),
        "request should join the prompt and the piped context: {body}"
    );
}

#[test]
fn a_dash_argument_reads_the_prompt_from_stdin() {
    let server = MockServer::start("200 OK", RESPONSE);
    let output = LaitCommand::new()
        .base_url(Some(&server.base_url))
        .opt_prompt(Some("-"))
        .spawn_with_stdin("dash prompt\n");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""content":"dashprompt""#),
        "request should carry the stdin prompt: {body}"
    );
}

#[test]
fn errors_when_stdin_is_empty_and_no_prompt_is_given() {
    // No server: the request must never be sent.
    let output = LaitCommand::new()
        .base_url(Some("http://127.0.0.1:9"))
        .opt_prompt(None)
        .spawn_with_stdin("");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("PROMPT is required"),
        "stderr should explain the missing prompt: {stderr}"
    );
}
