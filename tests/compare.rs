mod support;

use std::time::Duration;

use support::{ConfigDirectory, MockServer, test_command};

fn two_alias_config(base_url_a: &str, base_url_b: &str) -> String {
    format!(
        "models:\n  a:\n    - provider:\n        base_url: \"{base_url_a}\"\n      model_id: model-a\n  b:\n    - provider:\n        base_url: \"{base_url_b}\"\n      model_id: model-b\n"
    )
}

#[test]
fn compares_two_models_and_reports_both() {
    let server_a = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"model-a","choices":[{"index":0,"message":{"role":"assistant","content":"response from a"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
    );
    let server_b = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"model-b","choices":[{"index":0,"message":{"role":"assistant","content":"response from b"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":5,"total_tokens":9}}"#,
    );
    let config = ConfigDirectory::new(&two_alias_config(&server_a.base_url, &server_b.base_url));

    let output = test_command()
        .current_dir(config.path())
        .args(["compare", "--model", "a", "--model", "b", "hello"])
        .output()
        .expect("failed to execute lait compare");

    server_a.receive_request();
    server_b.receive_request();
    server_a.finish();
    server_b.finish();

    assert!(output.status.success(), "lait compare failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("response from a") && stdout.contains("response from b"),
        "expected both models' responses in the output: {stdout}"
    );
    assert!(
        stdout.contains("=== a ") && stdout.contains("=== b "),
        "expected both model names as section headers: {stdout}"
    );
    assert!(stdout.contains("time:"), "expected timing info: {stdout}");
    assert!(
        stdout.contains("prompt=1") && stdout.contains("prompt=4"),
        "expected per-model usage: {stdout}"
    );
}

#[test]
fn json_output_has_a_result_per_model() {
    let server_a = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"model-a","choices":[{"index":0,"message":{"role":"assistant","content":"response from a"},"finish_reason":"stop"}]}"#,
    );
    let server_b = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"model-b","choices":[{"index":0,"message":{"role":"assistant","content":"response from b"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&two_alias_config(&server_a.base_url, &server_b.base_url));

    let output = test_command()
        .current_dir(config.path())
        .args(["compare", "--model", "a", "--model", "b", "--json", "hello"])
        .output()
        .expect("failed to execute lait compare");

    server_a.receive_request();
    server_b.receive_request();
    server_a.finish();
    server_b.finish();

    assert!(output.status.success(), "lait compare failed: {output:?}");
    let results: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--json output should be valid JSON");
    let results = results.as_array().expect("results should be a JSON array");
    assert_eq!(results.len(), 2);
    for result in results {
        assert!(result["model"].is_string());
        assert!(result["model_id"].is_string());
        assert!(result["duration_ms"].is_u64());
        assert!(result["error"].is_null());
    }
    let model_a = results
        .iter()
        .find(|result| result["model"] == "a")
        .expect("model 'a' should be present");
    assert_eq!(model_a["content"], "response from a");
    assert_eq!(model_a["model_id"], "model-a");
}

#[test]
fn one_model_failing_does_not_prevent_the_other_from_reporting() {
    // A 400 response is not retried by async-openai's own retry layer, so
    // this test needs no `try_receive_request` draining loop (see
    // `tests/fallback.rs::does_not_fall_back_on_a_400_response`).
    let failing = MockServer::start(
        "400 Bad Request",
        r#"{"error":{"message":"bad request","type":"invalid_request_error"}}"#,
    );
    let succeeding = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"model-b","choices":[{"index":0,"message":{"role":"assistant","content":"response from b"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&two_alias_config(&failing.base_url, &succeeding.base_url));

    let output = test_command()
        .current_dir(config.path())
        .args(["compare", "--model", "a", "--model", "b", "hello"])
        .output()
        .expect("failed to execute lait compare");

    failing.receive_request();
    succeeding.receive_request();
    failing.finish();
    succeeding.finish();

    assert!(
        !output.status.success(),
        "lait compare should exit non-zero when a model fails: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("response from b"),
        "the succeeding model's response should still be reported: {stdout}"
    );
    assert!(
        stdout.contains("error:"),
        "the failing model's error should be reported: {stdout}"
    );
}

#[test]
fn requires_at_least_two_models() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"model-a","choices":[{"index":0,"message":{"role":"assistant","content":"unused"},"finish_reason":"stop"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "models:\n  a:\n    - provider:\n        base_url: \"{}\"\n      model_id: model-a\n",
        server.base_url
    ));

    let output = test_command()
        .current_dir(config.path())
        .args(["compare", "--model", "a", "hello"])
        .output()
        .expect("failed to execute lait compare");

    // No request is ever expected here, so the mock server's background
    // thread never returns from its first `accept()` — unlike every other
    // `try_receive_request` use in this suite, which follows a request the
    // thread already consumed. Calling `server.finish()` (which joins that
    // thread) would hang forever; `server` is simply dropped instead, same
    // as `MockServer::start_sequence`'s doc comment notes for an
    // untaken-but-tolerated extra connection.
    let leaked = server.try_receive_request(Duration::from_millis(300));

    assert!(
        !output.status.success(),
        "lait compare with one --model should fail: {output:?}"
    );
    assert!(
        leaked.is_none(),
        "no request should be sent when fewer than two models are given"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("at least two"),
        "expected a clear error about needing at least two models: {stderr}"
    );
}
