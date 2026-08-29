use anyhow::{Result, anyhow, bail};

/// Splits `---\n<frontmatter yaml>\n---\n<body>` into the frontmatter YAML and
/// the body. The file must start with a `---` delimiter line; the frontmatter
/// block ends at the next line that is exactly `---`. Shared by `agent::load_agent`
/// and `skill::load_skill`, since both file kinds use the same shape; `kind`
/// (e.g. `"agent file"`/`"skill file"`) names what's being parsed in the error
/// messages.
pub(crate) fn split<'a>(contents: &'a str, kind: &str) -> Result<(&'a str, &'a str)> {
    let mut lines = contents.split_inclusive('\n');
    let first = lines.next().unwrap_or("");
    if first.trim_end_matches(['\n', '\r']) != "---" {
        bail!("{kind} must start with a '---' frontmatter delimiter");
    }

    let mut offset = first.len();
    for line in lines {
        if line.trim_end_matches(['\n', '\r']) == "---" {
            let frontmatter_end = offset;
            let body_start = offset + line.len();
            return Ok((
                &contents[first.len()..frontmatter_end],
                &contents[body_start..],
            ));
        }
        offset += line.len();
    }
    Err(anyhow!("{kind} frontmatter has no closing '---' delimiter"))
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn splits_frontmatter_and_body() {
        let (frontmatter, body) = split("---\nname: x\n---\nbody\n", "test file").unwrap();
        assert_eq!(frontmatter, "name: x\n");
        assert_eq!(body, "body\n");
    }

    #[test]
    fn rejects_a_file_without_a_leading_frontmatter_delimiter() {
        let error = split("no frontmatter here\n", "test file").unwrap_err();
        assert!(error.to_string().contains("test file"));
    }

    #[test]
    fn rejects_a_file_with_unterminated_frontmatter() {
        assert!(
            split(
                "---\nname: x\nbody without closing delimiter\n",
                "test file"
            )
            .is_err()
        );
    }
}
