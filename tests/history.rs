mod support;

use support::{ConfigDirectory, MockServer, next_temp_path, test_command};

const RESPONSE: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"the answer"}}]}"#;

/// Points a freshly created, empty directory at `XDG_DATA_HOME` for the
/// child process, so `lait history`'s file (`$XDG_DATA_HOME/lait/history.jsonl`)
/// never touches the real invoking user's home directory. Dropped (and its
/// directory removed) at the end of the test.
struct IsolatedHistoryHome {
    path: std::path::PathBuf,
}

impl IsolatedHistoryHome {
    fn new() -> Self {
        let path = next_temp_path("lait-test-xdg-data-home", "");
        std::fs::create_dir_all(&path).expect("failed to create isolated XDG_DATA_HOME");
        Self { path }
    }

    fn history_path(&self) -> std::path::PathBuf {
        self.path.join("lait").join("history.jsonl")
    }
}

impl Drop for IsolatedHistoryHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn command_with_isolated_history(home: &IsolatedHistoryHome) -> std::process::Command {
    let mut command = test_command();
    command.env("XDG_DATA_HOME", &home.path);
    command
}

#[test]
fn a_successful_chat_appends_one_history_entry() {
    let home = IsolatedHistoryHome::new();
    let server = MockServer::start("200 OK", RESPONSE);
    let output = command_with_isolated_history(&home)
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("what is the answer?")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    let contents = std::fs::read_to_string(home.history_path()).expect("history file should exist");
    let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1);
    let entry: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(entry["kind"], "chat");
    assert_eq!(entry["model"], "test-model");
    assert_eq!(entry["prompt"], "what is the answer?");
    assert_eq!(entry["response"], "the answer");
}

#[test]
fn no_history_flag_skips_recording() {
    let home = IsolatedHistoryHome::new();
    let server = MockServer::start("200 OK", RESPONSE);
    command_with_isolated_history(&home)
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("--no-history")
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(!home.history_path().exists());
}

#[test]
fn default_history_false_in_config_skips_recording() {
    let home = IsolatedHistoryHome::new();
    let dir = ConfigDirectory::new("default:\n  history: false\n");
    let server = MockServer::start("200 OK", RESPONSE);
    command_with_isolated_history(&home)
        .current_dir(dir.path())
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("hello")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    assert!(!home.history_path().exists());
}

#[test]
fn lait_history_lists_the_most_recent_entry_first() {
    let home = IsolatedHistoryHome::new();
    let server = MockServer::start_sequence(&[("200 OK", RESPONSE), ("200 OK", RESPONSE)]);
    for prompt in ["first prompt", "second prompt"] {
        command_with_isolated_history(&home)
            .args(["--model", "test-model", "--base-url", &server.base_url])
            .arg(prompt)
            .output()
            .expect("failed to execute lait");
    }
    server.receive_request();
    server.receive_request();
    server.finish();

    let output = command_with_isolated_history(&home)
        .arg("history")
        .output()
        .expect("failed to execute lait history");
    assert!(output.status.success(), "lait failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with('1'));
    assert!(lines[0].contains("second prompt"));
    assert!(lines[1].starts_with('2'));
    assert!(lines[1].contains("first prompt"));
}

#[test]
fn lait_history_show_prints_the_full_entry() {
    let home = IsolatedHistoryHome::new();
    let server = MockServer::start("200 OK", RESPONSE);
    command_with_isolated_history(&home)
        .args(["--model", "test-model", "--base-url", &server.base_url])
        .arg("what is the answer?")
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    server.finish();

    let output = command_with_isolated_history(&home)
        .args(["history", "show", "1"])
        .output()
        .expect("failed to execute lait history show");
    assert!(output.status.success(), "lait failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("what is the answer?"));
    assert!(stdout.contains("the answer"));
}

#[test]
fn lait_history_show_fails_clearly_out_of_range() {
    let home = IsolatedHistoryHome::new();
    let output = command_with_isolated_history(&home)
        .args(["history", "show", "1"])
        .output()
        .expect("failed to execute lait history show");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no history entry"));
}

#[test]
fn lait_history_search_finds_a_matching_entry() {
    let home = IsolatedHistoryHome::new();
    let server = MockServer::start_sequence(&[("200 OK", RESPONSE), ("200 OK", RESPONSE)]);
    for prompt in ["translate this", "summarize that"] {
        command_with_isolated_history(&home)
            .args(["--model", "test-model", "--base-url", &server.base_url])
            .arg(prompt)
            .output()
            .expect("failed to execute lait");
    }
    server.receive_request();
    server.receive_request();
    server.finish();

    let output = command_with_isolated_history(&home)
        .args(["history", "search", "TRANSLATE"])
        .output()
        .expect("failed to execute lait history search");
    assert!(output.status.success(), "lait failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("translate this"));
    assert!(!stdout.contains("summarize that"));
}

#[test]
fn lait_history_reports_none_recorded_when_empty() {
    let home = IsolatedHistoryHome::new();
    let output = command_with_isolated_history(&home)
        .arg("history")
        .output()
        .expect("failed to execute lait history");
    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("no history recorded yet"));
}
