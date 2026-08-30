mod support;

use support::{ConfigDirectory, MockServer, test_command};

const RESPONSE: &str = r##"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"# Heading\n\nplain text"}}]}"##;

/// `cargo test` captures the child process's stdout, so it is never a real
/// TTY; `--render` should therefore always fall back to the raw response
/// text in these tests, exactly like piping to a file would. Actual ANSI
/// decoration is covered by `render::maybe_render`'s own unit test.
#[test]
fn render_falls_back_to_raw_text_when_stdout_is_not_a_terminal() {
    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--render")
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "# Heading\n\nplain text\n"
    );
}

#[test]
fn render_does_not_affect_json_output() {
    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .args(["--render", "--json"])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
    assert_eq!(parsed["content"], "# Heading\n\nplain text");
}

#[test]
fn default_render_from_config_enables_rendering_without_the_flag() {
    let dir = ConfigDirectory::new("default:\n  render: true\n");
    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .current_dir(dir.path())
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    // Still not a TTY under the test harness, so still falls back to raw
    // text — this only confirms `default.render: true` doesn't error and
    // reaches the same non-TTY fallback `--render` does.
    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "# Heading\n\nplain text\n"
    );
}

#[test]
fn render_has_no_effect_on_streaming_output() {
    let server = MockServer::start_stream(&[
        r##"{"id":"x","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{"index":0,"delta":{"content":"# Heading"}}]}"##,
    ]);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .args(["--render", "--stream"])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "# Heading\n");
}
