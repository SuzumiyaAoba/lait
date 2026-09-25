use super::*;

const DECISION_BODY: &str = r#"{"model":"jev-1.13.0","answers":{"urgent":{"type":"noul","noul":0.95},"team":{"type":"choice","choice":"billing","probabilities":{"billing":0.88,"sales":0.12},"confidence":0.76}},"usage":{"input_tokens":30,"output_tokens":2}}"#;

fn decide_workflow(extra_step_fields: &str) -> String {
    format!(
        r#"
inputs:
  ticket:
    type: object
steps:
  - id: triage
    input: $inputs.ticket
    decide:
      urgent:
        type: noul
        instructions: Does this need action today?
      team:
        type: choice
        criteria:
          billing: payments and invoices
          sales: null
{extra_step_fields}
  - switch:
      - when: .urgent.noul > 0.5
        steps:
          - jq: '"escalate to " + $steps.triage.team.choice'
    else:
      - jq: '"queue"'
"#
    )
}

#[test]
fn a_decide_step_sends_its_input_as_state_and_yields_the_answers() {
    let server = MockServer::start("200 OK", DECISION_BODY);
    let config = ConfigDirectory::new(&format!(
        "jev:\n  base_url: \"{}\"\n  api_key: jev-secret\n",
        server.base_url
    ));
    let workflow = config.write("triage.yml", &decide_workflow("    model: jev-pinned"));

    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg(&workflow)
        .args(["--input", r#"ticket={"subject":"charged twice"}"#])
        .output()
        .expect("failed to execute lait run");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "escalate to billing"
    );
    assert!(request.target.ends_with("/systemone"), "{}", request.target);
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer jev-secret"),
        "headers: {}",
        request.headers
    );
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["model"], "jev-pinned");
    assert_eq!(
        body["state"],
        serde_json::json!({"subject": "charged twice"})
    );
    assert_eq!(body["questions"]["team"]["type"], "choice");
}

#[test]
fn a_decide_step_retries_a_failed_request_under_its_retry_policy() {
    let server = MockServer::start_sequence(&[
        (
            "503 Service Unavailable",
            r#"{"detail":{"error_type":"overloaded","message":"try again"}}"#,
        ),
        ("200 OK", DECISION_BODY),
    ]);
    let config = ConfigDirectory::new(&format!("jev:\n  base_url: \"{}\"\n", server.base_url));
    let workflow = config.write(
        "triage.yml",
        &decide_workflow("    retry:\n      max_attempts: 2"),
    );

    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg(&workflow)
        .args(["--input", r#"ticket={"subject":"x"}"#])
        .output()
        .expect("failed to execute lait run");
    server.receive_request();
    server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait run failed: {output:?}");
}

#[test]
fn a_malformed_decide_step_is_rejected_by_lint() {
    let config = ConfigDirectory::empty();
    let workflow = config.write(
        "bad.yml",
        "steps:\n  - decide:\n      level:\n        type: score\n        criteria: [only-one]\n",
    );

    let output = test_command()
        .current_dir(config.path())
        .arg("lint")
        .arg(&workflow)
        .output()
        .expect("failed to execute lait lint");

    assert!(!output.status.success(), "lint should fail: {output:?}");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("2 to 10 levels"), "output: {text}");
}

#[test]
fn a_decide_step_refuses_to_reach_the_network_under_replay() {
    let config = ConfigDirectory::new("jev:\n  base_url: http://127.0.0.1:1/v1\n");
    let workflow = config.write("triage.yml", &decide_workflow(""));
    let cassette = config.path().join("cassette");
    std::fs::create_dir_all(&cassette).unwrap();

    let output = test_command()
        .current_dir(config.path())
        .arg("run")
        .arg(&workflow)
        .args(["--input", r#"ticket={"subject":"x"}"#, "--replay"])
        .arg(&cassette)
        .output()
        .expect("failed to execute lait run");

    assert!(!output.status.success(), "run should fail: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot run under --replay"),
        "stderr: {stderr}"
    );
}
