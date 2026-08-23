use std::process::Command;

#[test]
fn prints_hello_world() {
    let output = Command::new(env!("CARGO_BIN_EXE_lait"))
        .output()
        .expect("failed to execute lait");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"Hello, World!\n");
    assert!(output.stderr.is_empty());
}
