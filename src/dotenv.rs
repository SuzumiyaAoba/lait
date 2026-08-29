use std::path::Path;

use anyhow::{Context, Result, bail};

/// The file `load_from_current_dir` reads, always resolved against the
/// current directory (like `config::CONFIG_FILE_NAME`).
pub(crate) const DOTENV_FILE_NAME: &str = ".env";

/// Loads `.env` from the current directory into the process environment,
/// setting **only variables that are not already set** — a variable exported
/// by the shell (or set by direnv etc.) always wins over the file. A missing
/// `.env` is not an error; a present-but-malformed one is, so a typo in a
/// secrets file fails loudly instead of silently sending no key.
///
/// # Safety
///
/// Calls `std::env::set_var`, which must not race with environment reads
/// from other threads. The caller must ensure no other threads exist yet —
/// in practice: call this at the very top of `main`, before the tokio
/// runtime (whose worker threads read the environment freely) is built.
pub(crate) unsafe fn load_from_current_dir() -> Result<()> {
    let contents = match std::fs::read_to_string(DOTENV_FILE_NAME) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read '{DOTENV_FILE_NAME}'"));
        }
    };
    for (key, value) in parse(&contents, Path::new(DOTENV_FILE_NAME))? {
        if std::env::var_os(&key).is_none() {
            // SAFETY: guaranteed single-threaded by this function's contract.
            unsafe { std::env::set_var(&key, &value) };
        }
    }
    Ok(())
}

/// Parses dotenv-style `KEY=VALUE` lines: blank lines and `#` comments are
/// skipped, an optional `export ` prefix is accepted, and a value may be
/// single-quoted (taken literally), double-quoted (with `\n`/`\r`/`\t`/`\\`/
/// `\"` escapes), or bare (trailing ` # comment` stripped). Multi-line
/// values are not supported. `path` only names the file in error messages.
fn parse(contents: &str, path: &Path) -> Result<Vec<(String, String)>> {
    let mut variables = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        let line_number = index + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            bail!(
                "{}:{line_number}: expected 'KEY=VALUE', got {line:?}",
                path.display()
            );
        };
        let key = key.trim_end();
        let valid_key = !key.is_empty()
            && !key.starts_with(|c: char| c.is_ascii_digit())
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid_key {
            bail!(
                "{}:{line_number}: invalid variable name {key:?} (must be alphanumeric/underscore, not starting with a digit)",
                path.display()
            );
        }
        let value = parse_value(value.trim_start()).with_context(|| {
            format!(
                "{}:{line_number}: invalid value for '{key}'",
                path.display()
            )
        })?;
        variables.push((key.to_owned(), value));
    }
    Ok(variables)
}

fn parse_value(raw: &str) -> Result<String> {
    if let Some(rest) = raw.strip_prefix('"') {
        let (inner, remainder) = split_at_closing_double_quote(rest)?;
        check_nothing_but_comment(remainder)?;
        return unescape_double_quoted(inner);
    }
    if let Some(rest) = raw.strip_prefix('\'') {
        let Some((inner, remainder)) = rest.split_once('\'') else {
            bail!("unterminated single-quoted value (multi-line values are not supported)");
        };
        check_nothing_but_comment(remainder)?;
        return Ok(inner.to_owned());
    }
    // A bare value runs until a ` #` comment (a `#` with no preceding
    // whitespace stays part of the value, e.g. `COLOR=a#b`).
    let mut value = raw;
    let mut search_from = 0;
    while let Some(offset) = value[search_from..].find('#') {
        let position = search_from + offset;
        if value[..position].ends_with([' ', '\t']) {
            value = &value[..position];
            break;
        }
        search_from = position + 1;
    }
    Ok(value.trim_end().to_owned())
}

/// Splits `rest` (the text after an opening `"`) at its closing `"`,
/// skipping escaped `\"` pairs, into `(inner, remainder)`.
fn split_at_closing_double_quote(rest: &str) -> Result<(&str, &str)> {
    let mut escaped = false;
    for (index, c) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => return Ok((&rest[..index], &rest[index + 1..])),
            _ => {}
        }
    }
    bail!("unterminated double-quoted value (multi-line values are not supported)");
}

/// Rejects anything but whitespace or a `# comment` after a closing quote.
fn check_nothing_but_comment(remainder: &str) -> Result<()> {
    let trimmed = remainder.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(());
    }
    bail!("unexpected text {trimmed:?} after the closing quote");
}

fn unescape_double_quoted(inner: &str) -> Result<String> {
    let mut result = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            result.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => result.push('\n'),
            Some('r') => result.push('\r'),
            Some('t') => result.push('\t'),
            Some('\\') => result.push('\\'),
            Some('"') => result.push('"'),
            Some(other) => bail!("unsupported escape sequence '\\{other}' in double-quoted value"),
            None => bail!("dangling '\\' at the end of a double-quoted value"),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{parse, parse_value};
    use std::path::Path;

    fn parse_ok(contents: &str) -> Vec<(String, String)> {
        parse(contents, Path::new(".env")).expect("contents should parse")
    }

    #[test]
    fn parses_simple_assignments() {
        assert_eq!(
            parse_ok("FOO=bar\nBAZ=qux\n"),
            vec![
                ("FOO".to_owned(), "bar".to_owned()),
                ("BAZ".to_owned(), "qux".to_owned()),
            ]
        );
    }

    #[test]
    fn skips_blank_lines_and_comments() {
        assert_eq!(
            parse_ok("\n# comment\n  \nFOO=bar\n"),
            vec![("FOO".to_owned(), "bar".to_owned())]
        );
    }

    #[test]
    fn accepts_an_export_prefix() {
        assert_eq!(
            parse_ok("export FOO=bar\n"),
            vec![("FOO".to_owned(), "bar".to_owned())]
        );
    }

    #[test]
    fn strips_matching_quotes() {
        assert_eq!(
            parse_ok("A=\"double\"\nB='single'\n"),
            vec![
                ("A".to_owned(), "double".to_owned()),
                ("B".to_owned(), "single".to_owned()),
            ]
        );
    }

    #[test]
    fn unescapes_double_quoted_values_only() {
        assert_eq!(
            parse_ok("A=\"line1\\nline2\"\nB='literal\\n'\n"),
            vec![
                ("A".to_owned(), "line1\nline2".to_owned()),
                ("B".to_owned(), "literal\\n".to_owned()),
            ]
        );
    }

    #[test]
    fn strips_a_trailing_comment_from_a_bare_value() {
        assert_eq!(
            parse_ok("FOO=bar # comment\n"),
            vec![("FOO".to_owned(), "bar".to_owned())]
        );
    }

    #[test]
    fn keeps_a_hash_without_preceding_whitespace() {
        assert_eq!(
            parse_ok("FOO=a#b\n"),
            vec![("FOO".to_owned(), "a#b".to_owned())]
        );
    }

    #[test]
    fn keeps_a_hash_inside_quotes() {
        assert_eq!(
            parse_ok("FOO=\"a # b\"\n"),
            vec![("FOO".to_owned(), "a # b".to_owned())]
        );
    }

    #[test]
    fn strips_a_comment_after_a_quoted_value() {
        assert_eq!(
            parse_ok("FOO=\"bar\" # comment\nBAZ='qux' # comment\n"),
            vec![
                ("FOO".to_owned(), "bar".to_owned()),
                ("BAZ".to_owned(), "qux".to_owned()),
            ]
        );
    }

    #[test]
    fn rejects_trailing_text_after_a_quoted_value() {
        assert!(parse_value("\"bar\" baz").is_err());
    }

    #[test]
    fn allows_an_empty_value() {
        assert_eq!(parse_ok("FOO=\n"), vec![("FOO".to_owned(), String::new())]);
    }

    #[test]
    fn allows_equals_signs_in_the_value() {
        assert_eq!(
            parse_ok("FOO=a=b=c\n"),
            vec![("FOO".to_owned(), "a=b=c".to_owned())]
        );
    }

    #[test]
    fn rejects_a_line_without_an_equals_sign() {
        let error = parse("JUST_A_WORD\n", Path::new(".env")).unwrap_err();
        assert!(error.to_string().contains(":1:"));
    }

    #[test]
    fn rejects_an_invalid_variable_name() {
        assert!(parse("FOO-BAR=x\n", Path::new(".env")).is_err());
        assert!(parse("1FOO=x\n", Path::new(".env")).is_err());
        assert!(parse("=x\n", Path::new(".env")).is_err());
    }

    #[test]
    fn rejects_an_unterminated_quote() {
        assert!(parse("FOO=\"unterminated\n", Path::new(".env")).is_err());
        assert!(parse("FOO='unterminated\n", Path::new(".env")).is_err());
    }

    #[test]
    fn rejects_an_unknown_escape() {
        assert!(parse_value("\"bad \\x escape\"").is_err());
    }

    #[test]
    fn treats_a_lone_quote_as_unterminated() {
        assert!(parse_value("\"").is_err());
        assert!(parse_value("'").is_err());
    }
}
