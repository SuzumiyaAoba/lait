//! The CLI-facing output layer: `run_text`/`run_structured` (text/JSON/
//! GitHub-Actions-annotation formats) render a `LintRun` snapshot that
//! `lint.rs`'s own `run` builds and never re-derive a check result — see
//! `LintRun`'s doc comment on why. Named `report.rs`, distinct from the
//! unrelated top-level `crate::report` (the `run_*` subcommands' shared
//! end-of-run helper) — this one is lint-specific and lives under `lint/`.

use anyhow::{Context, Result, bail};

use crate::config;

use super::{LintFormat, LintRun, Severity};

/// Runs `lait lint`'s `--format text` (the default) rendering.
pub(super) fn run_text(run: &LintRun) -> Result<()> {
    if !run.registry.is_empty() {
        println!("{} (workflows:):", config::CONFIG_FILE_NAME);
        for entry in &run.registry {
            if entry.exists {
                println!("  {}: OK ({})", entry.name, entry.path.display());
            } else {
                println!(
                    "  {}: error: no such file '{}'",
                    entry.name,
                    entry.path.display()
                );
            }
        }
    }
    print_config_errors("api_key/api_key_cmd:", &run.api_key_errors);
    print_config_errors("tools:", &run.tool_errors);
    for report in &run.reports {
        if report.issues.is_empty() {
            println!("{}: OK", report.file.display());
        } else {
            println!("{}:", report.file.display());
            for issue in &report.issues {
                println!("  {}: {}", issue.severity, issue.message);
            }
        }
    }
    if run.has_errors() {
        let mut suffix = String::new();
        for (ok, section) in [
            (run.registry_ok(), "'workflows:'"),
            (run.api_key_errors.is_empty(), "api_key/api_key_cmd"),
            (run.tool_errors.is_empty(), "'tools:'"),
        ] {
            if !ok {
                suffix.push_str(&format!(
                    "; {} {section} also has errors",
                    config::CONFIG_FILE_NAME
                ));
            }
        }
        bail!(
            "{} of {} file(s) had errors{suffix}",
            run.failed_files(),
            run.reports.len()
        );
    }
    Ok(())
}

/// One machine-readable finding attributed to a file and optional source line.
struct Finding {
    file: String,
    line: Option<usize>,
    severity: Severity,
    message: String,
}

impl Finding {
    fn config(config_display: &str, severity: Severity, message: String) -> Self {
        Self {
            file: config_display.to_owned(),
            line: None,
            severity,
            message,
        }
    }
}

/// Runs `lait lint`'s `--format json`/`--format github` rendering.
pub(super) fn run_structured(run: &LintRun, format: LintFormat) -> Result<()> {
    let findings = findings(run);
    match format {
        LintFormat::Json => print_json_findings(&findings)?,
        LintFormat::Github => print_github_findings(&findings),
        LintFormat::Text => unreachable!("text has its own renderer"),
    }
    if run.has_errors() {
        bail!(
            "lint found {} error(s) across {} finding(s) in {} file(s)",
            findings
                .iter()
                .filter(|finding| finding.severity == Severity::Error)
                .count(),
            findings.len(),
            run.reports.len(),
        );
    }
    Ok(())
}

fn findings(run: &LintRun) -> Vec<Finding> {
    let mut findings = Vec::new();
    for report in &run.reports {
        let mut source = None;
        for issue in &report.issues {
            let line = issue.line.or_else(|| {
                let text = source.get_or_insert_with(|| {
                    std::fs::read_to_string(&report.file).unwrap_or_else(|error| {
                        eprintln!(
                            "warning: failed to read '{}' to guess a line number ({error}); this finding will report no line",
                            report.file.display()
                        );
                        String::new()
                    })
                });
                guess_line(text, &issue.message)
            });
            findings.push(Finding {
                file: report.file.display().to_string(),
                line,
                severity: issue.severity,
                message: issue.message.clone(),
            });
        }
    }
    for entry in run.registry.iter().filter(|entry| !entry.exists) {
        findings.push(Finding::config(
            &run.config_display,
            Severity::Error,
            format!(
                "workflows.{} resolves to '{}', which does not exist",
                entry.name,
                entry.path.display()
            ),
        ));
    }
    for message in run.api_key_errors.iter().chain(&run.tool_errors) {
        findings.push(Finding::config(
            &run.config_display,
            Severity::Error,
            message.clone(),
        ));
    }
    findings
}

fn print_config_errors(section: &str, errors: &[String]) {
    if !errors.is_empty() {
        println!("{} ({section}):", config::CONFIG_FILE_NAME);
        for error in errors {
            println!("  error: {error}");
        }
    }
}

fn print_json_findings(findings: &[Finding]) -> Result<()> {
    let records: Vec<serde_json::Value> = findings
        .iter()
        .map(|finding| {
            serde_json::json!({
                "file": finding.file,
                "line": finding.line,
                "severity": match finding.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                },
                "message": finding.message,
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&records).context("failed to serialize lint findings")?
    );
    Ok(())
}

fn print_github_findings(findings: &[Finding]) {
    for finding in findings {
        let level = match finding.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let message = escape_github_annotation(&finding.message);
        match finding.line {
            Some(line) => println!("::{level} file={},line={line}::{message}", finding.file),
            None => println!("::{level} file={}::{message}", finding.file),
        }
    }
}

/// Escapes a message for a GitHub Actions workflow command
/// (`::error ...::<message>`), per GitHub's documented `%`/CR/LF escaping —
/// otherwise a message containing one of these could corrupt the annotation
/// or be misread as a second command.
fn escape_github_annotation(message: &str) -> String {
    message
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Best-effort line lookup for an issue that has no line of its own (i.e.
/// everything except a YAML parse failure — see `yaml_error_line`): most
/// lint messages name the offending thing in single quotes (`node 'x'`,
/// `unknown MCP server 'y'`, ...), which is usually also how it appears
/// literally in the source (a YAML mapping key, a list entry, ...). Returns
/// the 1-based line of the first line containing that quoted text, or `None`
/// when the message has no quoted identifier or nothing in `source` matches
/// it. A heuristic, not a real position — good enough for an editor/CI
/// annotation to land a reader in the right neighborhood, not a guarantee.
fn guess_line(source: &str, message: &str) -> Option<usize> {
    let needle = first_quoted_identifier(message)?;
    source
        .lines()
        .position(|line| line.contains(needle))
        .map(|index| index + 1)
}

fn first_quoted_identifier(message: &str) -> Option<&str> {
    let start = message.find('\'')? + 1;
    let end = message[start..].find('\'')?;
    let candidate = &message[start..start + end];
    (!candidate.is_empty()).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::{first_quoted_identifier, guess_line};

    #[test]
    fn first_quoted_identifier_extracts_the_first_single_quoted_span() {
        assert_eq!(
            first_quoted_identifier("node 'extract': unknown skill 'nope'"),
            Some("extract")
        );
    }

    #[test]
    fn first_quoted_identifier_is_none_without_quotes() {
        assert_eq!(first_quoted_identifier("no quotes here"), None);
    }

    #[test]
    fn guess_line_finds_the_line_containing_the_quoted_identifier() {
        let source = "nodes:\n  extract:\n    type: prompt\n    prompt: hi\n";
        assert_eq!(guess_line(source, "node 'extract' is unused"), Some(2));
    }

    #[test]
    fn guess_line_is_none_when_nothing_matches() {
        let source = "nodes:\n  extract:\n    type: prompt\n";
        assert_eq!(guess_line(source, "node 'missing' is unused"), None);
    }
}
