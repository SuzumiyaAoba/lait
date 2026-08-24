mod support;

use std::time::{SystemTime, UNIX_EPOCH};

use support::{ConfigDirectory, JsonSchemaFile, run_lait_with_json_schema, test_command};

#[test]
fn reports_invalid_json_schema_file_with_path_context() {
    let schema = JsonSchemaFile::new("{not valid JSON");
    let output = run_lait_with_json_schema(None, None, "hello", &schema.path, None);

    assert!(
        !output.status.success(),
        "lait unexpectedly succeeded: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to parse JSON schema file"));
    assert!(stderr.contains(schema.path.to_string_lossy().as_ref()));
}

#[test]
fn reports_missing_json_schema_file_with_path_context() {
    let path = std::env::temp_dir().join(format!(
        "lait-missing-schema-{}-{}.json",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos()
    ));
    assert!(
        !path.exists(),
        "test schema path unexpectedly exists: {path:?}"
    );

    let output = run_lait_with_json_schema(None, None, "hello", &path, None);

    assert!(
        !output.status.success(),
        "lait unexpectedly succeeded: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to read JSON schema file"));
    assert!(stderr.contains(path.to_string_lossy().as_ref()));
}

#[test]
fn rejects_schema_name_without_json_schema_file() {
    let output = test_command()
        .args([
            "--model",
            "test-model",
            "--schema-name",
            "custom_schema",
            "hello",
        ])
        .output()
        .expect("failed to execute lait");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("json-schema"));
}

#[test]
fn rejects_an_empty_model_alias_definition_with_context() {
    let config = ConfigDirectory::new("default_model: empty-alias\nmodels:\n  empty-alias: []\n");
    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");

    assert!(
        !output.status.success(),
        "lait unexpectedly succeeded: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lowercase_stderr = stderr.to_ascii_lowercase();
    assert!(
        stderr.contains("empty-alias"),
        "stderr should identify the empty alias: {stderr}"
    );
    assert!(
        lowercase_stderr.contains("model")
            && (lowercase_stderr.contains("empty") || lowercase_stderr.contains("definition")),
        "stderr should explain that the alias has no model definition: {stderr}"
    );
}

#[test]
fn rejects_an_empty_model_id_with_context() {
    let config = ConfigDirectory::new(
        "default_model: empty-id-alias\nmodels:\n  empty-id-alias:\n    - provider:\n        base_url: http://127.0.0.1:1/v1\n      model_id: \"\"\n",
    );
    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");

    assert!(
        !output.status.success(),
        "lait unexpectedly succeeded: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lowercase_stderr = stderr.to_ascii_lowercase();
    assert!(
        stderr.contains("empty-id-alias"),
        "stderr should identify the invalid alias: {stderr}"
    );
    assert!(
        lowercase_stderr.contains("model_id") || lowercase_stderr.contains("model id"),
        "stderr should identify the empty model_id: {stderr}"
    );
}

#[test]
fn reports_malformed_config_with_its_path() {
    let config = ConfigDirectory::new("default_model: [\n");
    let output = test_command()
        .current_dir(config.path())
        .arg("hello")
        .output()
        .expect("failed to execute lait");

    assert!(
        !output.status.success(),
        "lait unexpectedly succeeded: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(config.config_path().to_string_lossy().as_ref()),
        "stderr should contain config path: {stderr}"
    );
}

#[test]
fn requires_model_option() {
    let directory = ConfigDirectory::empty();
    let output = test_command()
        .current_dir(directory.path())
        .args(["hello"])
        .output()
        .expect("failed to execute lait");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("model"));
}
