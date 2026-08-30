mod support;

use support::{ConfigDirectory, MockServer, test_command};

fn response_with(content: &str) -> String {
    format!(
        r#"{{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{{"index":0,"message":{{"role":"assistant","content":"{content}"}},"finish_reason":"stop"}}]}}"#
    )
}

#[test]
fn a_second_call_with_the_same_session_sends_the_first_turn_as_history() {
    let dir = ConfigDirectory::empty();

    let server = MockServer::start("200 OK", &response_with("hi there"));
    let output = test_command()
        .current_dir(dir.path())
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .args(["--session", "demo"])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();
    assert!(output.status.success(), "lait failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "hi there\n");

    let session_file = dir.path().join(".lait/sessions/demo.jsonl");
    assert!(session_file.exists());

    let server = MockServer::start("200 OK", &response_with("doing well"));
    let output = test_command()
        .current_dir(dir.path())
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .args(["--session", "demo"])
        .arg("how are you?")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();
    assert!(output.status.success(), "lait failed: {output:?}");

    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    let messages = body["messages"].as_array().unwrap();
    // [user "hello", assistant "hi there", user "how are you?"]
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "hello");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "hi there");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], "how are you?");
}

#[test]
fn sessions_show_prints_every_recorded_turn() {
    let dir = ConfigDirectory::empty();

    let server = MockServer::start("200 OK", &response_with("hi there"));
    test_command()
        .current_dir(dir.path())
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .args(["--session", "demo"])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    let output = test_command()
        .current_dir(dir.path())
        .args(["sessions", "show", "demo"])
        .output()
        .expect("failed to execute lait sessions show");
    assert!(output.status.success(), "lait failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("user: hello"));
    assert!(stdout.contains("assistant: hi there"));
}

#[test]
fn sessions_list_reports_the_session_and_its_turn_count() {
    let dir = ConfigDirectory::empty();

    let server = MockServer::start("200 OK", &response_with("hi there"));
    test_command()
        .current_dir(dir.path())
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .args(["--session", "demo"])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    let output = test_command()
        .current_dir(dir.path())
        .args(["sessions", "list"])
        .output()
        .expect("failed to execute lait sessions list");
    assert!(output.status.success(), "lait failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("demo"));
    assert!(stdout.contains("1 turns"));
}

#[test]
fn sessions_list_reports_none_saved_yet_when_empty() {
    let dir = ConfigDirectory::empty();
    let output = test_command()
        .current_dir(dir.path())
        .args(["sessions", "list"])
        .output()
        .expect("failed to execute lait sessions list");
    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("no sessions saved yet"));
}

#[test]
fn sessions_delete_removes_a_session() {
    let dir = ConfigDirectory::empty();

    let server = MockServer::start("200 OK", &response_with("hi there"));
    test_command()
        .current_dir(dir.path())
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .args(["--session", "demo"])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    let output = test_command()
        .current_dir(dir.path())
        .args(["sessions", "delete", "demo"])
        .output()
        .expect("failed to execute lait sessions delete");
    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(!dir.path().join(".lait/sessions/demo.jsonl").exists());
}

#[test]
fn sessions_delete_fails_clearly_for_a_missing_session() {
    let dir = ConfigDirectory::empty();
    let output = test_command()
        .current_dir(dir.path())
        .args(["sessions", "delete", "missing"])
        .output()
        .expect("failed to execute lait sessions delete");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no such session"));
}

#[test]
fn an_invalid_session_name_is_rejected() {
    let dir = ConfigDirectory::empty();
    let output = test_command()
        .current_dir(dir.path())
        .args(["--model", "test-model"])
        .args(["--session", "../escape"])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid session name"));
}
