mod support;

use support::{ConfigDirectory, MockServer, test_command};

const RESPONSE: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;

fn config_with_translate_prompt() -> ConfigDirectory {
    ConfigDirectory::new(
        r#"
prompts:
  translate:
    template: "translate {{ input }} into {{ vars.lang }}"
    model: config-model
    vars:
      lang: 日本語
"#,
    )
}

/// Like `config_with_translate_prompt`, but also sets a top-level `base_url:`
/// so the `lait prompt <name>` subcommand — which has no `--base-url` flag of
/// its own (see `docs/usage/ja/prompts.md`) — still reaches the mock server.
fn config_with_translate_prompt_and_base_url(base_url: &str) -> ConfigDirectory {
    ConfigDirectory::new(&format!(
        r#"
base_url: "{base_url}"
prompts:
  translate:
    template: "translate {{{{ input }}}} into {{{{ vars.lang }}}}"
    model: config-model
    vars:
      lang: 日本語
"#
    ))
}

#[test]
fn dash_p_renders_the_named_prompt_against_the_positional_input() {
    let dir = config_with_translate_prompt();
    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .current_dir(dir.path())
        .args(["--base-url", &server.base_url])
        .args(["-p", "translate"])
        .arg("Hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(request.body.contains("translate Hello into"));
}

#[test]
fn dash_p_uses_the_prompts_own_model_when_no_override_is_given() {
    let dir = config_with_translate_prompt();
    let server = MockServer::start("200 OK", RESPONSE);
    test_command()
        .current_dir(dir.path())
        .args(["--base-url", &server.base_url])
        .args(["-p", "translate"])
        .arg("Hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(request.body.contains(r#""model":"config-model""#));
}

#[test]
fn dash_p_var_overrides_the_prompts_default_var() {
    let dir = config_with_translate_prompt();
    let server = MockServer::start("200 OK", RESPONSE);
    let output = test_command()
        .current_dir(dir.path())
        .args(["--base-url", &server.base_url])
        .args(["-p", "translate", "--var", "lang=英語"])
        .arg("Hello")
        .output()
        .expect("failed to execute lait");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(request.body.contains("translate Hello into 英語"));
}

#[test]
fn an_undefined_prompt_name_is_a_clear_error() {
    let dir = config_with_translate_prompt();
    let output = test_command()
        .current_dir(dir.path())
        .args(["-p", "nope"])
        .arg("Hello")
        .output()
        .expect("failed to execute lait");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no prompt named 'nope'"));
    assert!(stderr.contains("translate"));
}

#[test]
fn prompt_subcommand_renders_and_runs_the_named_prompt() {
    let server = MockServer::start("200 OK", RESPONSE);
    let dir = config_with_translate_prompt_and_base_url(&server.base_url);
    let output = test_command()
        .current_dir(dir.path())
        .arg("prompt")
        .arg("translate")
        .arg("Hello")
        .output()
        .expect("failed to execute lait prompt");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(request.body.contains("translate Hello into"));
    assert!(request.body.contains(r#""model":"config-model""#));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ok\n");
}

#[test]
fn prompt_list_reports_configured_prompts() {
    let dir = config_with_translate_prompt();
    let output = test_command()
        .current_dir(dir.path())
        .args(["prompt", "list"])
        .output()
        .expect("failed to execute lait prompt list");

    assert!(output.status.success(), "lait failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("translate"));
    assert!(stdout.contains("config-model"));
}

#[test]
fn prompt_list_reports_none_defined_when_empty() {
    let dir = ConfigDirectory::empty();
    let output = test_command()
        .current_dir(dir.path())
        .args(["prompt", "list"])
        .output()
        .expect("failed to execute lait prompt list");

    assert!(output.status.success(), "lait failed: {output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("no prompts defined"));
}
