mod support;

use support::{ConfigDirectory, MockServer, test_command};

const RESPONSE: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;

#[test]
fn file_flag_appends_the_files_content_as_a_fenced_block() {
    let dir = ConfigDirectory::empty();
    let file_path = dir.path().join("notes.txt");
    std::fs::write(&file_path, "line one\nline two\n").unwrap();

    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--file")
        .arg(&file_path)
        .arg("summarize this")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(request.body.contains("summarize this"));
    assert!(request.body.contains(&file_path.display().to_string()));
    assert!(request.body.contains("line one"));
    assert!(request.body.contains("line two"));
}

#[test]
fn multiple_file_flags_are_all_attached() {
    let dir = ConfigDirectory::empty();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, "aaa").unwrap();
    std::fs::write(&b, "bbb").unwrap();

    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--file")
        .arg(&a)
        .arg("--file")
        .arg(&b)
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(request.body.contains("aaa"));
    assert!(request.body.contains("bbb"));
}

#[test]
fn a_missing_file_is_a_clear_error_and_no_request_is_sent() {
    let output = test_command()
        .args(["--model", "test-model"])
        .arg("--file")
        .arg("/no/such/file/lait-test")
        .arg("hello")
        .output()
        .expect("failed to execute lait");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("failed to read file"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_binary_file_is_rejected() {
    let dir = ConfigDirectory::empty();
    let file_path = dir.path().join("binary.bin");
    std::fs::write(&file_path, [0xff, 0xfe, 0x00, 0xff]).unwrap();

    let output = test_command()
        .args(["--model", "test-model"])
        .arg("--file")
        .arg(&file_path)
        .arg("hello")
        .output()
        .expect("failed to execute lait");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not valid UTF-8"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn file_content_combines_with_piped_stdin() {
    let dir = ConfigDirectory::empty();
    let file_path = dir.path().join("notes.txt");
    std::fs::write(&file_path, "file content").unwrap();

    let server = MockServer::start("200 OK", RESPONSE);
    let mut command = test_command();
    command
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--file")
        .arg(&file_path)
        .arg("review this");
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command.spawn().expect("failed to spawn lait");
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"piped context")
        .unwrap();
    let output = child.wait_with_output().expect("failed to wait for lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(request.body.contains("review this"));
    assert!(request.body.contains("piped context"));
    assert!(request.body.contains("file content"));
}
