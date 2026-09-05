mod support;

use support::{ConfigDirectory, MockServer, start_mock_mcp_server, test_command};

#[test]
fn reports_an_unset_env_var_placeholder() {
    let config = ConfigDirectory::new(
        "base_url: http://127.0.0.1:1\napi_key: ${LAIT_DOCTOR_TEST_MISSING_VAR}\n",
    );
    let output = test_command()
        .current_dir(config.path())
        .arg("doctor")
        .output()
        .expect("failed to execute lait doctor");

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("LAIT_DOCTOR_TEST_MISSING_VAR"),
        "the missing variable's name should be reported: {stdout}"
    );
    assert!(stdout.contains("== env =="), "{stdout}");
}

#[test]
fn skips_fields_with_no_placeholder_in_the_env_check() {
    let config = ConfigDirectory::new("base_url: http://127.0.0.1:1\n");
    let output = test_command()
        .current_dir(config.path())
        .arg("doctor")
        .output()
        .expect("failed to execute lait doctor");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("== env =="),
        "a base_url with no ${{VAR}} placeholder should not produce an env check: {stdout}"
    );
}

#[test]
fn reports_missing_agent_and_skill_files() {
    let config = ConfigDirectory::new(
        "base_url: http://127.0.0.1:1\nagents:\n  missing: ./no-such-agent.md\nskills:\n  missing: ./no-such-skill.md\n",
    );
    let output = test_command()
        .current_dir(config.path())
        .arg("doctor")
        .output()
        .expect("failed to execute lait doctor");

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[NG] agents.missing"),
        "a missing agents: path should be reported: {stdout}"
    );
    assert!(
        stdout.contains("[NG] skills.missing"),
        "a missing skills: path should be reported: {stdout}"
    );
}

/// A connectivity failure must not stop the checks that follow it (issue
/// #56's requirement that `doctor` reports everything wrong in one pass,
/// mirroring `lint::run`), and must not crash the process.
#[test]
fn reports_an_unreachable_base_url_without_aborting_other_checks() {
    let config = ConfigDirectory::new(
        "base_url: http://127.0.0.1:1\nagents:\n  missing: ./no-such-agent.md\n",
    );
    let output = test_command()
        .current_dir(config.path())
        .arg("doctor")
        .output()
        .expect("failed to execute lait doctor");

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("== connectivity =="), "{stdout}");
    assert!(
        stdout.contains("[NG] http://127.0.0.1:1"),
        "the unreachable base_url should be reported as NG: {stdout}"
    );
    assert!(
        stdout.contains("[NG] agents.missing"),
        "the files check should still run after the connectivity failure: {stdout}"
    );
}

#[test]
fn json_output_is_well_formed_and_matches_the_text_findings() {
    let config = ConfigDirectory::new("base_url: http://127.0.0.1:1\n");
    let output = test_command()
        .current_dir(config.path())
        .args(["doctor", "--json"])
        .output()
        .expect("failed to execute lait doctor");

    assert!(!output.status.success());
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--json output should be valid JSON");
    let checks = json["checks"].as_array().expect("checks should be a list");
    assert!(
        checks
            .iter()
            .any(|check| check["category"] == "connectivity" && check["status"] == "error"),
        "{json}"
    );
    assert_eq!(json["summary"]["error"].as_u64(), Some(1), "{json}");
}

#[test]
fn reports_connectivity_success_and_a_model_present_on_the_server() {
    let server = MockServer::start(
        "200 OK",
        r#"{"object":"list","data":[{"id":"test-model-id","object":"model"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "base_url: http://127.0.0.1:1\nmodels:\n  local:\n    - provider:\n        base_url: {}\n      model_id: test-model-id\n",
        server.base_url,
    ));
    let output = test_command()
        .current_dir(config.path())
        .arg("doctor")
        .output()
        .expect("failed to execute lait doctor");
    server.receive_request();
    server.finish();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[OK] models.local"),
        "the configured model id should be found on the server: {stdout}"
    );
}

#[test]
fn warns_when_a_configured_model_id_is_missing_from_the_server() {
    let server = MockServer::start(
        "200 OK",
        r#"{"object":"list","data":[{"id":"other-model","object":"model"}]}"#,
    );
    let config = ConfigDirectory::new(&format!(
        "base_url: http://127.0.0.1:1\nmodels:\n  local:\n    - provider:\n        base_url: {}\n      model_id: test-model-id\n",
        server.base_url,
    ));
    let output = test_command()
        .current_dir(config.path())
        .arg("doctor")
        .output()
        .expect("failed to execute lait doctor");
    server.receive_request();
    server.finish();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[WARN] models.local"),
        "a model id absent from the server's list should be a warning: {stdout}"
    );
}

#[test]
fn reports_a_broken_mcp_server_command() {
    let config = ConfigDirectory::new(
        "base_url: http://127.0.0.1:1\nmcp_servers:\n  broken:\n    command: /definitely/not/a/real/command\n",
    );
    let output = test_command()
        .current_dir(config.path())
        .arg("doctor")
        .output()
        .expect("failed to execute lait doctor");

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[NG] broken"),
        "a broken mcp_servers command should be reported: {stdout}"
    );
}

#[test]
fn reports_a_successful_mcp_server_startup() {
    let (mcp_url, _mcp_thread) = start_mock_mcp_server();
    // `doctor` only connects and lists tools (it never calls one), so
    // exactly 3 requests reach the mock server (initialize /
    // notifications/initialized / tools/list) — one short of the 4
    // `start_mock_mcp_server` always serves before it starts repeating its
    // last response forever. Not joining the handle avoids blocking on that
    // 4th connection, which never arrives.
    let config = ConfigDirectory::new(&format!(
        "base_url: http://127.0.0.1:1\nmcp_servers:\n  mock:\n    url: \"{mcp_url}\"\n"
    ));
    let output = test_command()
        .current_dir(config.path())
        .arg("doctor")
        .output()
        .expect("failed to execute lait doctor");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[OK] mock"),
        "a healthy mcp server should be reported OK: {stdout}"
    );
}

#[test]
fn default_model_unset_is_reported_as_a_warning_not_an_error() {
    let config = ConfigDirectory::new("base_url: http://127.0.0.1:1\n");
    let output = test_command()
        .current_dir(config.path())
        .arg("doctor")
        .output()
        .expect("failed to execute lait doctor");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[WARN] default.model"),
        "an unset default.model should warn, not error: {stdout}"
    );
}

/// A missing `lait.config.yml` is a warning, never an error, by itself —
/// unlike a config file that exists but fails to parse (see
/// `check_default_model`'s and `check_connectivity`'s own tests for those).
/// This intentionally does not assert on the overall exit status: `doctor`
/// still probes the default `http://localhost:1234/v1` endpoint even with no
/// config at all (a bare invocation would use it too), and whether that
/// probe succeeds depends on whatever happens to be listening there in the
/// environment the test runs in.
#[test]
fn missing_config_file_is_a_warning_not_an_error() {
    let config = ConfigDirectory::empty();
    let output = test_command()
        .current_dir(config.path())
        .arg("doctor")
        .output()
        .expect("failed to execute lait doctor");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[WARN] lait.config.yml"), "{stdout}");
}
