//! The diagnostic-finding shape and its text/JSON rendering: [`Status`],
//! [`Check`] and its `ok`/`warn`/`error` constructors, and [`emit`]. Split
//! out of `doctor.rs` — the check functions that build a `Vec<Check>` stay
//! in the parent module; this half only owns what a finding *is* and how it
//! is printed.

use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Status {
    Ok,
    Warn,
    Error,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Error => "NG",
        })
    }
}

/// One diagnostic finding. `category` groups related checks in the report
/// (`config`/`env`/`model`/`connectivity`/`models_on_server`/`mcp`/`files`);
/// `name` identifies what was checked within that category (a config key, a
/// base URL, a server name, ...).
#[derive(Debug, Serialize)]
pub(super) struct Check {
    pub(super) category: String,
    pub(super) name: String,
    pub(super) status: Status,
    pub(super) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) hint: Option<String>,
}

impl Check {
    pub(super) fn ok(category: &str, name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            category: category.to_owned(),
            name: name.into(),
            status: Status::Ok,
            message: message.into(),
            hint: None,
        }
    }

    pub(super) fn warn(
        category: &str,
        name: impl Into<String>,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self {
            category: category.to_owned(),
            name: name.into(),
            status: Status::Warn,
            message: message.into(),
            hint,
        }
    }

    pub(super) fn error(
        category: &str,
        name: impl Into<String>,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self {
            category: category.to_owned(),
            name: name.into(),
            status: Status::Error,
            message: message.into(),
            hint,
        }
    }
}

pub(super) fn emit(checks: &[Check], json: bool) -> Result<()> {
    let ok = checks.iter().filter(|c| c.status == Status::Ok).count();
    let warn = checks.iter().filter(|c| c.status == Status::Warn).count();
    let error = checks.iter().filter(|c| c.status == Status::Error).count();

    if json {
        let output = serde_json::json!({
            "checks": checks,
            "summary": {"ok": ok, "warn": warn, "error": error},
        });
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    let mut last_category: Option<&str> = None;
    for check in checks {
        if last_category != Some(check.category.as_str()) {
            println!("== {} ==", check.category);
            last_category = Some(&check.category);
        }
        println!("  [{}] {}: {}", check.status, check.name, check.message);
        if let Some(hint) = &check.hint {
            println!("        hint: {hint}");
        }
    }
    println!("\n{ok} OK, {warn} WARN, {error} NG");
    Ok(())
}
