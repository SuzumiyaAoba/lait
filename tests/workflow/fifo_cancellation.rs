use super::*;

#[cfg(unix)]
#[test]
fn a_prompt_input_schema_read_from_a_fifo_is_cancelled_by_the_step_timeout() {
    let config = ConfigDirectory::empty();
    let schema_path = config.path().join("prompt-input-schema.fifo");
    create_fifo(&schema_path);
    let workflow = timeout_workflow(&format!(
        "    type: prompt\n    prompt: \"{{{{ input }}}}\"\n    input_schema: \"{}\"\n    timeout: 1",
        schema_path.display()
    ));

    assert_fifo_read_times_out(&workflow.path);
}

#[cfg(unix)]
#[test]
fn an_agent_input_schema_read_from_a_fifo_is_cancelled_by_the_step_timeout() {
    let config = ConfigDirectory::empty();
    let schema_path = config.path().join("agent-input-schema.fifo");
    create_fifo(&schema_path);
    let agent = AgentMarkdownFile::new(&format!(
        "---\ninput_schema:\n  file_path: \"{}\"\n---\nExtract the input.\n",
        schema_path.display()
    ));
    let workflow = timeout_workflow(&format!(
        "    type: agent\n    agent: \"{}\"\n    timeout: 1",
        agent.path.display()
    ));

    assert_fifo_read_times_out(&workflow.path);
}

#[cfg(unix)]
#[test]
fn an_agent_file_read_from_a_fifo_is_cancelled_by_the_step_timeout() {
    let config = ConfigDirectory::empty();
    let agent_path = config.path().join("blocked-agent.md.fifo");
    create_fifo(&agent_path);
    let workflow = timeout_workflow(&format!(
        "    type: agent\n    agent: \"{}\"\n    timeout: 1",
        agent_path.display()
    ));

    assert_fifo_read_times_out(&workflow.path);
}

#[cfg(unix)]
#[test]
fn an_agent_output_schema_read_from_a_fifo_is_cancelled_by_the_step_timeout() {
    let config = ConfigDirectory::empty();
    let schema_path = config.path().join("agent-output-schema.fifo");
    create_fifo(&schema_path);
    let agent = AgentMarkdownFile::new(&format!(
        "---\nstructured_output: true\noutput_schema:\n  file_path: \"{}\"\n---\nExtract the answer.\n",
        schema_path.display()
    ));
    let workflow = timeout_workflow(&format!(
        "    type: agent\n    agent: \"{}\"\n    timeout: 1",
        agent.path.display()
    ));

    assert_fifo_read_times_out(&workflow.path);
}

#[cfg(unix)]
#[test]
fn a_prompt_output_schema_read_from_a_fifo_is_cancelled_by_the_step_timeout() {
    let config = ConfigDirectory::empty();
    let schema_path = config.path().join("prompt-output-schema.fifo");
    create_fifo(&schema_path);
    let workflow = timeout_workflow(&format!(
        "    type: prompt\n    prompt: \"{{{{ input }}}}\"\n    output_schema: \"{}\"\n    schema_name: answer\n    timeout: 1",
        schema_path.display()
    ));

    assert_fifo_read_times_out(&workflow.path);
}

#[cfg(unix)]
#[test]
fn a_single_file_attachment_read_from_a_fifo_is_cancelled_by_the_step_timeout() {
    let config = ConfigDirectory::empty();
    let file_path = config.path().join("single-file.fifo");
    create_fifo(&file_path);
    let workflow = timeout_workflow(&format!(
        "    type: prompt\n    prompt: \"{{{{ input }}}}\"\n    files: [\"{}\"]\n    timeout: 1",
        file_path.display()
    ));

    assert_fifo_read_times_out(&workflow.path);
}

#[cfg(unix)]
#[test]
fn multiple_file_attachments_read_from_fifos_are_cancelled_by_the_step_timeout() {
    let config = ConfigDirectory::empty();
    let first_path = config.path().join("first-file.fifo");
    let second_path = config.path().join("second-file.fifo");
    create_fifo(&first_path);
    create_fifo(&second_path);
    let workflow = timeout_workflow(&format!(
        "    type: prompt\n    prompt: \"{{{{ input }}}}\"\n    files: [\"{}\", \"{}\"]\n    timeout: 1",
        first_path.display(),
        second_path.display()
    ));

    assert_fifo_read_times_out(&workflow.path);
}

#[cfg(unix)]
#[test]
fn a_single_image_attachment_read_from_a_fifo_is_cancelled_by_the_step_timeout() {
    let config = ConfigDirectory::empty();
    let image_path = config.path().join("single-image.fifo");
    create_fifo(&image_path);
    let workflow = timeout_workflow(&format!(
        "    type: prompt\n    prompt: \"{{{{ input }}}}\"\n    images: [\"{}\"]\n    timeout: 1",
        image_path.display()
    ));

    assert_fifo_read_times_out(&workflow.path);
}

#[cfg(unix)]
#[test]
fn multiple_image_attachments_read_from_fifos_are_cancelled_by_the_step_timeout() {
    let config = ConfigDirectory::empty();
    let first_path = config.path().join("first-image.fifo");
    let second_path = config.path().join("second-image.fifo");
    create_fifo(&first_path);
    create_fifo(&second_path);
    let workflow = timeout_workflow(&format!(
        "    type: prompt\n    prompt: \"{{{{ input }}}}\"\n    images: [\"{}\", \"{}\"]\n    timeout: 1",
        first_path.display(),
        second_path.display()
    ));

    assert_fifo_read_times_out(&workflow.path);
}
