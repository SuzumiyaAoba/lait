use super::*;

#[test]
fn allows_a_node_with_only_write_file() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: transform
    write_file: out.txt
steps:
  - use: n
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn parses_a_node_with_a_command() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    type: command
    command: ["wc", "-l"]
steps:
  - use: n
"#,
    )
    .expect("workflow with a command node should parse");

    let NodeDefinition::Command(n) = workflow.nodes["n"].as_ref() else {
        panic!("expected a command node");
    };
    assert_eq!(n.command, vec!["wc".to_owned(), "-l".to_owned()]);
}

#[test]
fn rejects_a_node_with_an_empty_command_list() {
    let result =
        parse_workflow("nodes:\n  n:\n    type: command\n    command: []\nsteps:\n  - use: n\n");
    assert!(result.is_err());
}

#[test]
fn a_command_node_can_have_a_jq_filter_and_write_file() {
    let result = parse_workflow(
        r#"
nodes:
  n:
    type: command
    command: ["wc", "-l"]
    jq: "tonumber"
    write_file: out.txt
steps:
  - use: n
"#,
    );
    assert!(result.is_ok());
}

#[test]
fn parses_a_node_with_files_and_images() {
    let workflow = parse_workflow(
        r#"
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
    files: [notes.txt, more.txt]
    images: [photo.png, "https://example.com/cat.png"]
steps:
  - use: n
"#,
    )
    .expect("workflow with files/images should parse");

    let NodeDefinition::Prompt(n) = workflow.nodes["n"].as_ref() else {
        panic!("expected a prompt node");
    };
    assert_eq!(
        n.files.as_deref(),
        Some(
            [
                std::path::PathBuf::from("notes.txt"),
                std::path::PathBuf::from("more.txt")
            ]
            .as_slice()
        )
    );
    assert_eq!(
        n.images.as_deref(),
        Some(
            [
                "photo.png".to_owned(),
                "https://example.com/cat.png".to_owned()
            ]
            .as_slice()
        )
    );
}

#[test]
fn parses_a_workflow_default_retry_and_timeout() {
    let workflow = parse_workflow(
        r#"
default:
  retry:
    max_attempts: 3
    delay_seconds: 1
    backoff: 2.0
  timeout: 30
nodes:
  n:
    type: prompt
    prompt: "{{ input }}"
steps:
  - use: n
"#,
    )
    .expect("workflow with default retry/timeout should parse");

    let retry = workflow.default.retry.as_ref().unwrap();
    assert_eq!(retry.max_attempts, Some(3));
    assert_eq!(workflow.default.timeout, Some(30));
}
