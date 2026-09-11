use super::*;

#[test]
fn write_file_writes_the_steps_output_without_changing_what_flows_downstream() {
    let server = MockServer::start_sequence(&[
        ("200 OK", CHAT_COMPLETION_BODY),
        ("200 OK", CHAT_COMPLETION_BODY),
    ]);
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let output_path = std::env::temp_dir().join(format!("lait-test-write-file-{unique}.txt"));
    let workflow = WorkflowFile::new(&format!(
        r#"
default:
  model: local
models:
  local:
    - provider:
        base_url: "{}"
      model_id: workflow-model
nodes:
  written:
    type: prompt
    prompt: "{{{{ input }}}}"
    write_file: "{}"
  echo:
    type: prompt
    prompt: "echo: {{{{ steps.written }}}}"
steps:
  - id: written
    use: written
  - use: echo
"#,
        server.base_url,
        output_path.display()
    ));

    let output = run_lait_workflow(&workflow.path, "hello");
    server.receive_request();
    server.receive_request();
    server.finish();

    let written = std::fs::read_to_string(&output_path);
    std::fs::remove_file(&output_path).ok();

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        written.expect("write_file should have created the output file"),
        "mock response"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response",
        "the next step should still see write_file's own (unmodified) output"
    );
}

#[cfg(unix)]
#[test]
fn write_file_preserves_existing_inode_and_permissions() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let config = ConfigDirectory::empty();
    let output_path = config.path().join("output.txt");
    let hard_link_path = config.path().join("output-hard-link.txt");
    std::fs::write(&output_path, "before").expect("failed to create output fixture");
    let mut permissions = std::fs::metadata(&output_path)
        .expect("output fixture should exist")
        .permissions();
    permissions.set_mode(0o640);
    std::fs::set_permissions(&output_path, permissions)
        .expect("failed to set output fixture permissions");
    std::fs::hard_link(&output_path, &hard_link_path).expect("failed to create hard link");
    let original_inode = std::fs::metadata(&output_path)
        .expect("output fixture should exist")
        .ino();

    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  emit:
    type: transform
    write_file: "{}"
steps:
  - use: emit
"#,
        output_path.display()
    ));
    let output = test_command()
        .current_dir(config.path())
        .args([
            "run",
            workflow.path.to_str().unwrap(),
            "after",
            "--no-history",
        ])
        .output()
        .expect("failed to execute lait run");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        std::fs::read_to_string(&output_path).expect("output file should exist"),
        "after"
    );
    assert_eq!(
        std::fs::read_to_string(&hard_link_path).expect("hard link should still exist"),
        "after",
        "write_file should truncate the existing inode rather than replacing it"
    );
    assert_eq!(
        std::fs::metadata(&output_path)
            .expect("output file should exist")
            .ino(),
        original_inode,
        "write_file should preserve the existing inode"
    );
    assert_eq!(
        std::fs::metadata(&output_path)
            .expect("output file should exist")
            .permissions()
            .mode()
            & 0o777,
        0o640,
        "write_file should preserve existing permissions"
    );
}

#[cfg(unix)]
#[test]
fn write_file_follows_a_symlink_to_an_existing_file() {
    use std::os::unix::fs::symlink;

    let config = ConfigDirectory::empty();
    let target_path = config.path().join("target.txt");
    let link_path = config.path().join("output-link.txt");
    std::fs::write(&target_path, "before").expect("failed to create target fixture");
    symlink(&target_path, &link_path).expect("failed to create output symlink");

    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  emit:
    type: transform
    write_file: "{}"
steps:
  - use: emit
"#,
        link_path.display()
    ));
    let output = test_command()
        .current_dir(config.path())
        .args([
            "run",
            workflow.path.to_str().unwrap(),
            "after",
            "--no-history",
        ])
        .output()
        .expect("failed to execute lait run");

    assert!(output.status.success(), "lait run failed: {output:?}");
    assert_eq!(
        std::fs::read_to_string(&target_path).expect("target file should exist"),
        "after"
    );
    assert!(
        std::fs::symlink_metadata(&link_path)
            .expect("output symlink should still exist")
            .file_type()
            .is_symlink(),
        "write_file should follow, not replace, an output symlink"
    );
}

#[cfg(unix)]
#[test]
fn write_file_respects_an_existing_read_only_file() {
    use std::os::unix::fs::PermissionsExt;

    let config = ConfigDirectory::empty();
    let output_path = config.path().join("read-only-output.txt");
    std::fs::write(&output_path, "original").expect("failed to create output fixture");
    let mut permissions = std::fs::metadata(&output_path)
        .expect("output fixture should exist")
        .permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(&output_path, permissions)
        .expect("failed to make output fixture read-only");

    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  emit:
    type: transform
    write_file: "{}"
steps:
  - use: emit
"#,
        output_path.display()
    ));
    let output = test_command()
        .current_dir(config.path())
        .args([
            "run",
            workflow.path.to_str().unwrap(),
            "replacement",
            "--no-history",
        ])
        .output()
        .expect("failed to execute lait run");

    assert!(!output.status.success(), "a read-only output should fail");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("failed to write output"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&output_path).expect("output file should remain readable"),
        "original",
        "a failed read-only write must not alter the existing content"
    );
    let mut writable_permissions = std::fs::metadata(&output_path)
        .expect("output file should still exist")
        .permissions();
    writable_permissions.set_mode(0o644);
    std::fs::set_permissions(&output_path, writable_permissions)
        .expect("failed to restore output fixture permissions");
}

#[cfg(windows)]
#[test]
fn write_file_respects_an_existing_read_only_file() {
    let config = ConfigDirectory::empty();
    let output_path = config.path().join("read-only-output.txt");
    std::fs::write(&output_path, "original").expect("failed to create output fixture");
    let mut permissions = std::fs::metadata(&output_path)
        .expect("output fixture should exist")
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&output_path, permissions)
        .expect("failed to make output fixture read-only");

    let workflow = WorkflowFile::new(&format!(
        r#"
nodes:
  emit:
    type: transform
    write_file: "{}"
steps:
  - use: emit
"#,
        output_path.display()
    ));
    let output = test_command()
        .current_dir(config.path())
        .args([
            "run",
            workflow.path.to_str().unwrap(),
            "replacement",
            "--no-history",
        ])
        .output()
        .expect("failed to execute lait run");

    assert!(!output.status.success(), "a read-only output should fail");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("failed to write output"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&output_path).expect("output file should remain readable"),
        "original",
        "a failed read-only write must not alter the existing content"
    );
    let mut writable_permissions = std::fs::metadata(&output_path)
        .expect("output file should still exist")
        .permissions();
    writable_permissions.set_readonly(false);
    std::fs::set_permissions(&output_path, writable_permissions)
        .expect("failed to restore output fixture permissions");
}
