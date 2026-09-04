mod support;

use std::time::Duration;

use support::{ConfigDirectory, MockServer, test_command, without_json_whitespace};

#[test]
fn falls_back_to_the_second_provider_after_a_persistent_5xx() {
    let primary = MockServer::start(
        "503 Service Unavailable",
        r#"{"error":{"message":"mock outage","type":"server_error"}}"#,
    );
    let secondary = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"model-b","choices":[{"index":0,"message":{"role":"assistant","content":"from the second provider"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "default:\n  model: multi\nmodels:\n  multi:\n    - provider:\n        base_url: \"{}\"\n      model_id: model-a\n    - provider:\n        base_url: \"{}\"\n      model_id: model-b\n",
        primary.base_url, secondary.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");

    // async-openai retries the primary's persistent 503 a few times before
    // ever giving up and letting lait's own fallback see the error.
    while primary
        .try_receive_request(Duration::from_secs(2))
        .is_some()
    {}
    let second_request = secondary.receive_request();
    primary.finish();
    secondary.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "from the second provider"
    );
    let body = without_json_whitespace(&second_request.body);
    assert!(
        body.contains(r#""model":"model-b""#),
        "request body: {body}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warning:") && stderr.contains("model-b"),
        "expected a fallback note on stderr, stderr: {stderr}"
    );
}

#[test]
fn does_not_fall_back_on_a_400_response() {
    let primary = MockServer::start(
        "400 Bad Request",
        r#"{"error":{"message":"bad request","type":"invalid_request_error"}}"#,
    );
    // Deliberately never contacted (see the assertion below) — `.finish()`
    // is never called on it, since that joins the mock server's accept
    // thread and would block forever waiting for a connection that must
    // never arrive.
    let secondary = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"model-b","choices":[{"index":0,"message":{"role":"assistant","content":"should never be reached"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "default:\n  model: multi\nmodels:\n  multi:\n    - provider:\n        base_url: \"{}\"\n      model_id: model-a\n    - provider:\n        base_url: \"{}\"\n      model_id: model-b\n",
        primary.base_url, secondary.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = primary.receive_request();
    let leaked_to_secondary = secondary.try_receive_request(Duration::from_millis(600));
    primary.finish();

    assert!(
        !output.status.success(),
        "lait unexpectedly succeeded: {output:?}"
    );
    assert_eq!(request.target, "/v1/chat/completions");
    assert!(
        leaked_to_secondary.is_none(),
        "a 400 response is not retryable and should not trigger a fallback"
    );
}

#[test]
fn an_explicit_base_url_override_disables_fallback_between_model_definitions() {
    let primary = MockServer::start(
        "503 Service Unavailable",
        r#"{"error":{"message":"mock outage","type":"server_error"}}"#,
    );
    // Deliberately never contacted — see `does_not_fall_back_on_a_400_response`'s
    // comment on why `.finish()` is never called on it.
    let secondary = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"model-b","choices":[{"index":0,"message":{"role":"assistant","content":"should never be reached"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "models:\n  multi:\n    - provider:\n        base_url: \"{}\"\n      model_id: model-a\n    - provider:\n        base_url: \"{}\"\n      model_id: model-b\n",
        primary.base_url, secondary.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .args(["--model", "multi", "--base-url", &primary.base_url])
        .arg("hello")
        .output()
        .expect("failed to execute lait");

    while primary
        .try_receive_request(Duration::from_secs(2))
        .is_some()
    {}
    let leaked_to_secondary = secondary.try_receive_request(Duration::from_millis(600));
    primary.finish();

    assert!(
        !output.status.success(),
        "lait unexpectedly succeeded: {output:?}"
    );
    assert!(
        leaked_to_secondary.is_none(),
        "a --base-url override should collapse every candidate into one, no fallback"
    );
}
