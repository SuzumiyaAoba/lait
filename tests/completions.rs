mod support;

use support::test_command;

#[test]
fn generates_a_zsh_completion_script() {
    let output = test_command()
        .args(["completions", "zsh"])
        .output()
        .expect("failed to execute lait completions");

    assert!(
        output.status.success(),
        "lait completions failed: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("#compdef lait"),
        "zsh completions should start with #compdef: {}",
        stdout.lines().next().unwrap_or_default()
    );
    assert!(stdout.contains("--show-usage"));
}

#[test]
fn generates_scripts_for_every_supported_shell() {
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = test_command()
            .args(["completions", shell])
            .output()
            .expect("failed to execute lait completions");
        assert!(
            output.status.success() && !output.stdout.is_empty(),
            "completions for {shell} should succeed with output"
        );
    }
}

#[test]
fn rejects_an_unknown_shell() {
    let output = test_command()
        .args(["completions", "tcsh"])
        .output()
        .expect("failed to execute lait completions");
    assert!(!output.status.success());
}
