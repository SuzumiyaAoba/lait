//! Behavioral coverage for `lait decide` (`src/app/decide.rs`) and the
//! Jev-compatible client behind it (`src/jev.rs`): the request shape sent to
//! `{jev.base_url}/systemone`, key/endpoint resolution from `jev:`, output
//! modes, and how server errors surface.

mod support;

use std::io::Write;
use std::process::Stdio;

use support::{ConfigDirectory, MockServer, test_command};

const QUESTIONS: &str = r#"
urgent:
  type: noul
  instructions: Does this need action today?
  criteria:
    true: needs action today
    false: can wait
team:
  type: choice
  criteria:
    billing: payments and invoices
    sales: null
"#;

const DECISION_BODY: &str = r#"{"model":"jev-1.13.0","answers":{"team":{"type":"choice","choice":"billing","probabilities":{"billing":0.88,"sales":0.12},"confidence":0.76},"urgent":{"type":"noul","noul":0.95}},"usage":{"input_tokens":30,"output_tokens":2}}"#;

fn jev_config(base_url: &str) -> String {
    format!(
        "jev:\n  base_url: \"{base_url}\"\n  api_key: \"${{TEST_JEV_KEY}}\"\n  model: jev-test\n"
    )
}

#[test]
fn sends_a_systemone_request_and_prints_the_answers_in_question_order() {
    let server = MockServer::start("200 OK", DECISION_BODY);
    let config = ConfigDirectory::new(&jev_config(&server.base_url));
    let questions = config.write("questions.yml", QUESTIONS);

    let output = test_command()
        .current_dir(config.path())
        .env("TEST_JEV_KEY", "jev-secret")
        .args(["decide", "-q"])
        .arg(&questions)
        .arg("The invoice was charged twice!")
        .output()
        .expect("failed to execute lait decide");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait decide failed: {output:?}");
    assert_eq!(request.method, "POST");
    assert!(
        request.target.ends_with("/systemone"),
        "target: {}",
        request.target
    );
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer jev-secret"),
        "headers: {}",
        request.headers
    );
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["model"], "jev-test");
    assert_eq!(body["state"], "The invoice was charged twice!");
    assert_eq!(
        body["questions"]["urgent"]["criteria"],
        serde_json::json!({"true": "needs action today", "false": "can wait"})
    );
    assert_eq!(
        body["questions"]["team"]["criteria"]["sales"],
        serde_json::Value::Null
    );

    let answers: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let keys: Vec<&String> = answers.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["urgent", "team"], "stdout: {answers}");
    assert_eq!(answers["team"]["choice"], "billing");
}

#[test]
fn full_prints_the_whole_response_and_json_state_is_sent_structured() {
    let server = MockServer::start("200 OK", DECISION_BODY);
    let config = ConfigDirectory::new(&format!("jev:\n  base_url: \"{}\"\n", server.base_url));
    let questions = config.write("questions.yml", QUESTIONS);

    let mut child = test_command()
        .current_dir(config.path())
        .args([
            "decide",
            "--json",
            "--full",
            "--model",
            "jev-override",
            "-q",
        ])
        .arg(&questions)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn lait decide");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"subject": "refund", "body": "charged twice"}"#)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait decide failed: {output:?}");
    assert!(
        !request
            .headers
            .to_ascii_lowercase()
            .contains("authorization"),
        "no key configured means no Authorization header: {}",
        request.headers
    );
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["model"], "jev-override");
    assert_eq!(body["state"]["subject"], "refund");

    let printed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(printed["usage"]["input_tokens"], 30);
    assert_eq!(printed["model"], "jev-1.13.0");
}

#[test]
fn surfaces_the_servers_error_detail() {
    let server = MockServer::start(
        "401 Unauthorized",
        r#"{"detail":{"error_type":"authentication_error","message":"invalid API key"}}"#,
    );
    let config = ConfigDirectory::new(&format!("jev:\n  base_url: \"{}\"\n", server.base_url));
    let questions = config.write("questions.yml", QUESTIONS);

    let output = test_command()
        .current_dir(config.path())
        .args(["decide", "-q"])
        .arg(&questions)
        .arg("state")
        .output()
        .expect("failed to execute lait decide");
    server.receive_request();
    server.finish();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("401") && stderr.contains("invalid API key"),
        "stderr: {stderr}"
    );
}

#[test]
fn rejects_an_answer_set_that_does_not_match_the_questions() {
    let server = MockServer::start(
        "200 OK",
        r#"{"model":"jev","answers":{"urgent":{"type":"noul","noul":0.4}},"usage":{"input_tokens":1,"output_tokens":1}}"#,
    );
    let config = ConfigDirectory::new(&format!("jev:\n  base_url: \"{}\"\n", server.base_url));
    let questions = config.write("questions.yml", QUESTIONS);

    let output = test_command()
        .current_dir(config.path())
        .args(["decide", "-q"])
        .arg(&questions)
        .arg("state")
        .output()
        .expect("failed to execute lait decide");
    server.receive_request();
    server.finish();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no answer for question 'team'"),
        "stderr: {stderr}"
    );
}

#[test]
fn invalid_questions_fail_before_any_request() {
    let config = ConfigDirectory::new("jev:\n  base_url: http://127.0.0.1:1/v1\n");
    let questions = config.write(
        "questions.yml",
        "team:\n  type: choice\n  criteria:\n    only: null\n",
    );

    let output = test_command()
        .current_dir(config.path())
        .args(["decide", "-q"])
        .arg(&questions)
        .arg("state")
        .output()
        .expect("failed to execute lait decide");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("question 'team'") && stderr.contains("2 to 255 options"),
        "stderr: {stderr}"
    );
}
