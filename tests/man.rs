mod support;

use support::{ConfigDirectory, test_command};

#[test]
fn generates_a_man_page_per_subcommand() {
    let dir = ConfigDirectory::empty();
    let man_dir = dir.path().join("man");

    let output = test_command()
        .arg("man")
        .arg("--dir")
        .arg(&man_dir)
        .output()
        .expect("failed to execute lait man");

    assert!(output.status.success(), "lait man failed: {output:?}");
    for page in [
        "lait.1",
        "lait-run.1",
        "lait-agent.1",
        "lait-agent-run.1",
        "lait-lint.1",
        "lait-models.1",
        "lait-completions.1",
        "lait-man.1",
    ] {
        assert!(
            man_dir.join(page).is_file(),
            "expected man page '{page}' to be generated"
        );
    }
    let top_page =
        std::fs::read_to_string(man_dir.join("lait.1")).expect("lait.1 should be readable");
    assert!(
        top_page.contains(".TH lait 1"),
        "lait.1 should be a roff man page"
    );
}
