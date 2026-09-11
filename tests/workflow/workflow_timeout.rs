use super::*;

#[cfg(unix)]
#[test]
fn a_child_workflow_waiting_on_a_fifo_observes_the_workflow_deadline() {
    let dir = support::ScratchDir::new();
    create_fifo(&dir.path().join("child.yml"));
    let root = dir.write("root.yml", "default: {workflow_timeout: 1}\nnodes:\n  child: {type: workflow, workflow: child.yml}\nsteps: [{use: child}]\n");
    let (output, elapsed) = run_workflow_until_timeout(&root);
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    assert!(elapsed < Duration::from_secs(3));
}

#[cfg(unix)]
#[test]
fn a_recursive_fifo_workflow_is_rejected_before_waiting_for_another_writer() {
    use std::io::Write;
    let dir = support::ScratchDir::new();
    let path = dir.path().join("cycle.yml");
    create_fifo(&path);
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .open(writer_path)
            .unwrap();
        writer.write_all(b"default: {workflow_timeout: 1}\nnodes:\n  again: {type: workflow, workflow: cycle.yml}\nsteps: [{use: again}]\n").unwrap();
    });
    let (output, _) = run_workflow_until_timeout(&path);
    writer.join().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("would create a cycle"), "{stderr}");
}

#[test]
fn on_error_receives_the_underlying_cause_not_only_the_step_label() {
    let workflow = WorkflowFile::new(
        "nodes:\n  call: {type: command, command: [nonexistent-lait-test-executable]}\n  recover: {type: transform, jq: '.error'}\nsteps:\n  - use: call\n    on_error: {steps: [{use: recover}]}\n",
    );
    let output = test_command()
        .arg("run")
        .arg(&workflow.path)
        .args(["input", "--no-config", "--no-env", "--no-history"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let message = String::from_utf8(output.stdout).unwrap();
    assert!(
        message.contains("nonexistent-lait-test-executable"),
        "{message}"
    );
    assert!(message.contains("failed to run command"), "{message}");
}
