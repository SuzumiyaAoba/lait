mod support;

use support::{ConfigDirectory, MINIMAL_PNG_BYTES, MockServer, test_command};

const RESPONSE: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;

#[test]
fn image_flag_sends_a_base64_data_url_alongside_the_text() {
    let dir = ConfigDirectory::empty();
    let image_path = dir.path().join("photo.png");
    std::fs::write(&image_path, MINIMAL_PNG_BYTES).unwrap();

    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--image")
        .arg(&image_path)
        .arg("what is this?")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    let content = body["messages"][0]["content"]
        .as_array()
        .expect("content should be an array when an image is attached");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "what is this?");
    assert_eq!(content[1]["type"], "image_url");
    let url = content[1]["image_url"]["url"].as_str().unwrap();
    assert!(url.starts_with("data:image/png;base64,"));
}

#[test]
fn image_flag_passes_an_http_url_through_unchanged() {
    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--image")
        .arg("https://example.com/cat.png")
        .arg("describe it")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    let content = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(
        content[1]["image_url"]["url"],
        "https://example.com/cat.png"
    );
}

#[test]
fn multiple_image_flags_are_all_attached_in_order() {
    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--image")
        .arg("https://example.com/a.png")
        .arg("--image")
        .arg("https://example.com/b.png")
        .arg("compare these")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    let content = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert_eq!(content[1]["image_url"]["url"], "https://example.com/a.png");
    assert_eq!(content[2]["image_url"]["url"], "https://example.com/b.png");
}

#[test]
fn without_image_the_message_content_stays_a_plain_string() {
    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["messages"][0]["content"], "hello");
}

#[test]
fn an_unrecognized_local_image_format_is_a_clear_error() {
    let dir = ConfigDirectory::empty();
    let bogus_path = dir.path().join("notes.xyz");
    std::fs::write(&bogus_path, b"not an image").unwrap();

    let output = test_command()
        .args(["--model", "test-model"])
        .arg("--image")
        .arg(&bogus_path)
        .arg("hello")
        .output()
        .expect("failed to execute lait");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not determine"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
