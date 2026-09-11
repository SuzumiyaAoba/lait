use super::*;

#[test]
fn eval_when_coerces_plain_text_input_to_a_json_string() {
    assert!(eval_when(". == \"hello\"", "hello", &StepOutputs::new()).unwrap());
    assert!(!eval_when(". == \"hello\"", "world", &StepOutputs::new()).unwrap());
}

#[test]
fn eval_when_evaluates_against_parsed_json_input() {
    assert!(eval_when(".flag", r#"{"flag":true}"#, &StepOutputs::new()).unwrap());
    assert!(!eval_when(".flag", r#"{"flag":false}"#, &StepOutputs::new()).unwrap());
}

#[test]
fn eval_when_can_reference_a_named_step_output_via_dollar_steps() {
    let mut steps = StepOutputs::new();
    steps.insert("check".to_owned(), serde_json::json!({"ok": true}));
    assert!(eval_when("$steps.check.ok", "null", &steps).unwrap());
}

pub(super) fn as_switch(step: &crate::workflow::FlowStep) -> &crate::workflow::SwitchDefinition {
    match step.router() {
        Some(crate::workflow::Router::Switch(router)) => router,
        _ => panic!("expected Switch router"),
    }
}

pub(super) fn as_parallel(
    step: &crate::workflow::FlowStep,
) -> &crate::workflow::ParallelDefinition {
    match step.router() {
        Some(crate::workflow::Router::Parallel(router)) => router,
        _ => panic!("expected Parallel router"),
    }
}

pub(super) fn as_loop(step: &crate::workflow::FlowStep) -> &crate::workflow::LoopDefinition {
    match step.router() {
        Some(crate::workflow::Router::Loop(router)) => router,
        _ => panic!("expected Loop router"),
    }
}

pub(super) fn as_foreach(step: &crate::workflow::FlowStep) -> &crate::workflow::ForEachDefinition {
    match step.router() {
        Some(crate::workflow::Router::ForEach(router)) => router,
        _ => panic!("expected ForEach router"),
    }
}

#[test]
fn rejects_inactive_bare_control_steps_before_execution() {
    for fields in [
        "stop: false",
        "break: false",
        "stop: false\n    break: false",
    ] {
        let source = format!("steps:\n  - {fields}\n");
        assert!(
            parse_workflow(&source).is_err(),
            "accepted empty control action: {source}"
        );
    }
}

#[test]
fn keeps_yaml_error_sources_when_explaining_missing_node_types() {
    let error =
        parse_workflow("nodes:\n  call:\n    prompt: hello\nsteps:\n  - use: call\n").unwrap_err();
    assert!(error.chain().any(|cause| cause.is::<serde_yaml::Error>()));
    assert!(error.to_string().contains("requires a 'type:'"));
}

#[test]
fn compiled_calls_keep_their_resolved_node_after_the_source_map_is_removed() {
    let mut workflow =
        parse_workflow("nodes:\n  n: {type: transform, jq: '.'}\nsteps:\n  - use: n\n").unwrap();
    let call = workflow.steps[0].call().unwrap();
    assert!(std::ptr::eq(call.definition, workflow.nodes["n"].as_ref()));
    workflow.nodes.clear();
    assert_eq!(
        workflow.steps[0].call().unwrap().definition.type_name(),
        "transform"
    );
    let graph =
        crate::workflow::graph::render(&workflow, crate::workflow::graph::GraphFormat::Mermaid)
            .unwrap();
    assert!(graph.contains("transform"));
}
