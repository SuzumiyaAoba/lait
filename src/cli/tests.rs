use super::{AgentAction, AgentCommand, Cli, Command, EvalFormat, TestFormat};
use crate::reasoning::ReasoningEffort;

#[test]
fn parses_prompt_and_options() {
    let cli = Cli::try_parse_from([
        "lait",
        "--model",
        "local-model",
        "--base-url",
        "http://localhost:1234/v1",
        "--api-key",
        "test-key",
        "--show-reasoning",
        "--reasoning-effort",
        "high",
        "hello",
    ])
    .expect("valid CLI arguments should parse");

    assert!(cli.command.is_none());
    assert_eq!(cli.chat.shared.model.as_deref(), Some("local-model"));
    assert_eq!(
        cli.chat.shared.endpoint.base_url.as_deref(),
        Some("http://localhost:1234/v1")
    );
    assert_eq!(
        cli.chat.shared.endpoint.api_key.as_deref(),
        Some("test-key")
    );
    assert!(cli.chat.shared.show_reasoning);
    assert_eq!(
        cli.chat.shared.reasoning_effort,
        Some(ReasoningEffort::High)
    );
    assert_eq!(cli.chat.prompt.as_deref(), Some("hello"));
    assert!(cli.chat.json_schema.is_none());
    assert_eq!(cli.chat.schema_name, "structured_output");
}

#[test]
fn parses_json_schema_options_with_default_name() {
    let cli = Cli::try_parse_from([
        "lait",
        "--model",
        "local-model",
        "--json-schema",
        "schema.json",
        "hello",
    ])
    .expect("valid JSON schema arguments should parse");

    assert_eq!(
        cli.chat
            .json_schema
            .as_deref()
            .and_then(|path| path.to_str()),
        Some("schema.json")
    );
    assert_eq!(cli.chat.schema_name, "structured_output");
}

#[test]
fn hides_reasoning_by_default() {
    let cli = Cli::try_parse_from(["lait", "--model", "local-model", "hello"])
        .expect("valid CLI arguments should parse");

    assert!(!cli.chat.shared.show_reasoning);
    assert_eq!(cli.chat.shared.reasoning_effort, None);
}

#[test]
fn accepts_all_reasoning_effort_values() {
    for effort in ["none", "minimal", "low", "medium", "high", "xhigh"] {
        let cli = Cli::try_parse_from([
            "lait",
            "--model",
            "local-model",
            "--reasoning-effort",
            effort,
            "hello",
        ])
        .expect("reasoning effort should be accepted");

        assert_eq!(
            cli.chat.shared.reasoning_effort,
            Some(match effort {
                "none" => ReasoningEffort::None,
                "minimal" => ReasoningEffort::Minimal,
                "low" => ReasoningEffort::Low,
                "medium" => ReasoningEffort::Medium,
                "high" => ReasoningEffort::High,
                "xhigh" => ReasoningEffort::Xhigh,
                _ => unreachable!(),
            })
        );
    }
}

#[test]
fn parses_temperature_top_p_and_max_tokens() {
    let cli = Cli::try_parse_from([
        "lait",
        "--model",
        "local-model",
        "--temperature",
        "0.7",
        "--top-p",
        "0.9",
        "--max-tokens",
        "256",
        "hello",
    ])
    .expect("valid sampling options should parse");

    assert_eq!(cli.chat.shared.temperature, Some(0.7));
    assert_eq!(cli.chat.shared.top_p, Some(0.9));
    assert_eq!(cli.chat.shared.max_tokens, Some(256));
}

#[test]
fn leaves_temperature_top_p_and_max_tokens_unset_by_default() {
    let cli = Cli::try_parse_from(["lait", "--model", "local-model", "hello"])
        .expect("valid CLI arguments should parse");

    assert!(cli.chat.shared.temperature.is_none());
    assert!(cli.chat.shared.top_p.is_none());
    assert!(cli.chat.shared.max_tokens.is_none());
}

#[test]
fn parses_stream_flag() {
    let cli = Cli::try_parse_from(["lait", "--model", "local-model", "--stream", "hello"])
        .expect("valid CLI arguments should parse");

    assert!(cli.chat.stream);
}

#[test]
fn leaves_stream_off_by_default() {
    let cli = Cli::try_parse_from(["lait", "--model", "local-model", "hello"])
        .expect("valid CLI arguments should parse");

    assert!(!cli.chat.stream);
}

#[test]
fn rejects_stream_combined_with_json() {
    assert!(
        Cli::try_parse_from([
            "lait",
            "--model",
            "local-model",
            "--json",
            "--stream",
            "hello",
        ])
        .is_err()
    );
}

#[test]
fn rejects_unknown_reasoning_effort_value() {
    assert!(
        Cli::try_parse_from([
            "lait",
            "--model",
            "local-model",
            "--reasoning-effort",
            "extreme",
            "hello",
        ])
        .is_err()
    );
}

#[test]
fn allows_model_from_config_and_leaves_prompt_optional_for_app_level_validation() {
    // PROMPT is optional at the clap level so that a subcommand (e.g. `run`) can be
    // used instead; app-level code enforces that chat mode requires it.
    assert!(Cli::try_parse_from(["lait", "hello"]).is_ok());
    let cli = Cli::try_parse_from(["lait", "--model", "local-model"])
        .expect("prompt-less invocation should still parse");
    assert!(cli.chat.prompt.is_none());
}

#[test]
fn parses_run_subcommand() {
    let cli = Cli::try_parse_from(["lait", "run", "workflow.yml", "hello world"])
        .expect("valid run subcommand arguments should parse");

    match cli.command {
        Some(Command::Run(run_args)) => {
            assert_eq!(run_args.file.to_str(), Some("workflow.yml"));
            assert_eq!(run_args.prompt.as_deref(), Some("hello world"));
        }
        _ => panic!("expected the run subcommand to be selected"),
    }
}

#[test]
fn parses_run_subcommand_with_record() {
    let cli = Cli::try_parse_from(["lait", "run", "workflow.yml", "hello", "--record", "dir"])
        .expect("valid run subcommand arguments should parse");

    match cli.command {
        Some(Command::Run(run_args)) => {
            assert_eq!(
                run_args.record.as_deref(),
                Some(std::path::Path::new("dir"))
            );
            assert!(run_args.replay.is_none());
        }
        _ => panic!("expected the run subcommand to be selected"),
    }
}

#[test]
fn parses_run_subcommand_with_replay() {
    let cli = Cli::try_parse_from(["lait", "run", "workflow.yml", "hello", "--replay", "dir"])
        .expect("valid run subcommand arguments should parse");

    match cli.command {
        Some(Command::Run(run_args)) => {
            assert_eq!(
                run_args.replay.as_deref(),
                Some(std::path::Path::new("dir"))
            );
            assert!(run_args.record.is_none());
        }
        _ => panic!("expected the run subcommand to be selected"),
    }
}

#[test]
fn rejects_run_subcommand_with_both_record_and_replay() {
    assert!(
        Cli::try_parse_from([
            "lait",
            "run",
            "workflow.yml",
            "hello",
            "--record",
            "a",
            "--replay",
            "b",
        ])
        .is_err()
    );
}

#[test]
fn parses_agent_run_subcommand() {
    let cli = Cli::try_parse_from(["lait", "agent", "run", "agent.md", "hello"])
        .expect("valid agent run subcommand arguments should parse");

    match cli.command {
        Some(Command::Agent(AgentCommand {
            action: AgentAction::Run(run_args),
        })) => {
            assert_eq!(run_args.file.to_str(), Some("agent.md"));
            assert_eq!(run_args.input.as_deref(), Some("hello"));
        }
        _ => panic!("expected the agent run subcommand to be selected"),
    }
}

#[test]
fn agent_run_subcommand_requires_a_file_but_not_an_input() {
    // INPUT is optional at the clap level so it can come from piped
    // stdin instead; app-level code enforces that one of the two exists.
    assert!(Cli::try_parse_from(["lait", "agent", "run"]).is_err());
    let cli = Cli::try_parse_from(["lait", "agent", "run", "agent.md"])
        .expect("input-less agent run should still parse");
    match cli.command {
        Some(Command::Agent(AgentCommand {
            action: AgentAction::Run(run_args),
        })) => assert!(run_args.input.is_none()),
        _ => panic!("expected the agent run subcommand to be selected"),
    }
}

#[test]
fn run_subcommand_accepts_global_no_config_after_its_args() {
    let cli = Cli::try_parse_from(["lait", "run", "workflow.yml", "hello", "--no-config"])
        .expect("global flags should be accepted after subcommand arguments");

    assert!(cli.no_config);
}

#[test]
fn run_subcommand_accepts_global_flags_before_the_subcommand() {
    let cli = Cli::try_parse_from(["lait", "--no-env", "--no-config", "run", "workflow.yml"])
        .expect("global flags should be accepted before the subcommand");

    assert!(cli.no_env);
    assert!(cli.no_config);
    assert!(matches!(cli.command, Some(Command::Run(_))));
}

#[test]
fn test_subcommand_accepts_global_flags_before_the_subcommand() {
    let cli = Cli::try_parse_from([
        "lait",
        "--no-env",
        "--no-config",
        "test",
        "test-definition.yml",
    ])
    .expect("global flags should be accepted before the subcommand");

    assert!(cli.no_env);
    assert!(cli.no_config);
    assert!(matches!(cli.command, Some(Command::Test(_))));
}

#[test]
fn rejects_chat_arguments_before_a_subcommand() {
    let error = Cli::try_parse_from(["lait", "--model", "local-model", "run", "workflow.yml"])
        .expect_err("chat arguments before a subcommand should be rejected");
    assert!(error.to_string().contains("subcommand 'run'"));
    assert!(Cli::try_parse_from(["lait", "--stream", "run", "workflow.yml"]).is_err());
}

#[test]
fn double_dash_keeps_a_subcommand_name_as_a_chat_prompt() {
    let cli = Cli::try_parse_from(["lait", "--", "run"])
        .expect("a literal prompt should be accepted after '--'");

    assert!(cli.command.is_none());
    assert_eq!(cli.chat.prompt.as_deref(), Some("run"));
}

#[test]
fn run_subcommand_accepts_global_config_after_its_args() {
    let cli = Cli::try_parse_from([
        "lait",
        "run",
        "workflow.yml",
        "hello",
        "--config",
        "custom.yml",
    ])
    .expect("global flags should be accepted after subcommand arguments");

    assert_eq!(
        cli.config.as_deref(),
        Some(std::path::Path::new("custom.yml"))
    );
}

#[test]
fn rejects_config_combined_with_no_config() {
    assert!(
        Cli::try_parse_from(["lait", "--config", "custom.yml", "--no-config", "hello"]).is_err()
    );
}

#[test]
fn run_subcommand_requires_a_file_but_not_a_prompt() {
    // PROMPT is optional at the clap level so it can come from piped
    // stdin instead; app-level code enforces that one of the two exists.
    assert!(Cli::try_parse_from(["lait", "run"]).is_err());
    let cli = Cli::try_parse_from(["lait", "run", "workflow.yml"])
        .expect("prompt-less run should still parse");
    match cli.command {
        Some(Command::Run(run_args)) => assert!(run_args.prompt.is_none()),
        _ => panic!("expected the run subcommand to be selected"),
    }
}

#[test]
fn parses_lint_subcommand_with_a_single_file() {
    let cli = Cli::try_parse_from(["lait", "lint", "workflow.yml"])
        .expect("valid lint subcommand arguments should parse");

    match cli.command {
        Some(Command::Lint(lint_args)) => {
            assert_eq!(lint_args.files.len(), 1);
            assert_eq!(lint_args.files[0].to_str(), Some("workflow.yml"));
        }
        _ => panic!("expected the lint subcommand to be selected"),
    }
}

#[test]
fn parses_lint_subcommand_with_multiple_files() {
    let cli = Cli::try_parse_from(["lait", "lint", "workflow.yml", "agent.md"])
        .expect("valid lint subcommand arguments should parse");

    match cli.command {
        Some(Command::Lint(lint_args)) => {
            assert_eq!(
                lint_args
                    .files
                    .iter()
                    .filter_map(|path| path.to_str())
                    .collect::<Vec<_>>(),
                vec!["workflow.yml", "agent.md"]
            );
        }
        _ => panic!("expected the lint subcommand to be selected"),
    }
}

#[test]
fn lint_subcommand_requires_at_least_one_file() {
    assert!(Cli::try_parse_from(["lait", "lint"]).is_err());
}

#[test]
fn lint_subcommand_accepts_global_no_config() {
    let cli = Cli::try_parse_from(["lait", "lint", "workflow.yml", "--no-config"])
        .expect("global flags should be accepted after subcommand arguments");

    assert!(cli.no_config);
}

#[test]
fn parses_doctor_subcommand() {
    let cli = Cli::try_parse_from(["lait", "doctor"]).expect("valid doctor arguments should parse");

    match cli.command {
        Some(Command::Doctor(doctor_args)) => assert!(!doctor_args.json),
        _ => panic!("expected the doctor subcommand to be selected"),
    }
}

#[test]
fn parses_doctor_subcommand_with_json() {
    let cli = Cli::try_parse_from(["lait", "doctor", "--json"])
        .expect("valid doctor arguments should parse");

    match cli.command {
        Some(Command::Doctor(doctor_args)) => assert!(doctor_args.json),
        _ => panic!("expected the doctor subcommand to be selected"),
    }
}

#[test]
fn parses_compare_subcommand_with_repeated_model_flags() {
    let cli = Cli::try_parse_from(["lait", "compare", "--model", "a", "--model", "b", "hello"])
        .expect("valid compare subcommand arguments should parse");

    match cli.command {
        Some(Command::Compare(compare_args)) => {
            assert_eq!(compare_args.models, vec!["a".to_owned(), "b".to_owned()]);
            assert_eq!(compare_args.prompt.as_deref(), Some("hello"));
            assert!(!compare_args.json);
        }
        _ => panic!("expected the compare subcommand to be selected"),
    }
}

#[test]
fn compare_subcommand_prompt_is_optional_for_app_level_validation() {
    // PROMPT is optional at the clap level so it can come from piped
    // stdin instead; app-level code enforces that one of the two exists
    // (see `chat::resolve_input_with_stdin_cancellable`).
    let cli = Cli::try_parse_from(["lait", "compare", "--model", "a", "--model", "b"])
        .expect("prompt-less compare should still parse");
    match cli.command {
        Some(Command::Compare(compare_args)) => assert!(compare_args.prompt.is_none()),
        _ => panic!("expected the compare subcommand to be selected"),
    }
}

#[test]
fn compare_subcommand_requires_at_least_one_model_flag() {
    // clap only enforces "at least one"; `app::compare::run` enforces
    // the real "at least two" requirement at the app layer.
    assert!(Cli::try_parse_from(["lait", "compare", "hello"]).is_err());
}

#[test]
fn parses_compare_subcommand_with_json_and_sampling_overrides() {
    let cli = Cli::try_parse_from([
        "lait",
        "compare",
        "--model",
        "a",
        "--model",
        "b",
        "--json",
        "--temperature",
        "0.5",
        "--max-tokens",
        "128",
        "hello",
    ])
    .expect("valid compare subcommand arguments should parse");

    match cli.command {
        Some(Command::Compare(compare_args)) => {
            assert!(compare_args.json);
            assert_eq!(compare_args.temperature, Some(0.5));
            assert_eq!(compare_args.max_tokens, Some(128));
        }
        _ => panic!("expected the compare subcommand to be selected"),
    }
}

#[test]
fn parses_test_subcommand_with_multiple_paths() {
    let cli = Cli::try_parse_from(["lait", "test", "tests/", "one.yml"])
        .expect("valid test subcommand arguments should parse");

    match cli.command {
        Some(Command::Test(test_args)) => {
            assert_eq!(
                test_args
                    .paths
                    .iter()
                    .filter_map(|path| path.to_str())
                    .collect::<Vec<_>>(),
                vec!["tests/", "one.yml"]
            );
            assert_eq!(test_args.format, TestFormat::Text);
        }
        _ => panic!("expected the test subcommand to be selected"),
    }
}

#[test]
fn test_subcommand_requires_at_least_one_path() {
    assert!(Cli::try_parse_from(["lait", "test"]).is_err());
}

#[test]
fn parses_test_subcommand_with_json_format() {
    let cli = Cli::try_parse_from(["lait", "test", "--format", "json", "tests/"])
        .expect("valid test subcommand arguments should parse");

    match cli.command {
        Some(Command::Test(test_args)) => assert_eq!(test_args.format, TestFormat::Json),
        _ => panic!("expected the test subcommand to be selected"),
    }
}

#[test]
fn parses_eval_subcommand_with_defaults() {
    let cli = Cli::try_parse_from(["lait", "eval", "eval.yml"])
        .expect("valid eval subcommand arguments should parse");

    match cli.command {
        Some(Command::Eval(eval_args)) => {
            assert_eq!(eval_args.file.to_str(), Some("eval.yml"));
            assert_eq!(eval_args.repeat, 1);
            assert_eq!(eval_args.format, EvalFormat::Text);
        }
        _ => panic!("expected the eval subcommand to be selected"),
    }
}

#[test]
fn parses_eval_subcommand_with_repeat_and_json_format() {
    let cli = Cli::try_parse_from([
        "lait", "eval", "--repeat", "5", "--format", "json", "eval.yml",
    ])
    .expect("valid eval subcommand arguments should parse");

    match cli.command {
        Some(Command::Eval(eval_args)) => {
            assert_eq!(eval_args.repeat, 5);
            assert_eq!(eval_args.format, EvalFormat::Json);
        }
        _ => panic!("expected the eval subcommand to be selected"),
    }
}

#[test]
fn eval_subcommand_requires_a_file() {
    assert!(Cli::try_parse_from(["lait", "eval"]).is_err());
}
