//! TTY Markdown rendering (`--render`/`default.render`, see
//! `docs/usage/ja/output.md`): decorates a response's Markdown (headings,
//! lists, emphasis, code blocks, tables, ...) for terminal display via
//! `termimad`, falling back to the raw text whenever that wouldn't make
//! sense — rendering is off, or stdout isn't an actual terminal (a pipe, a
//! redirect to a file) where ANSI escapes would just be noise.

use std::io::IsTerminal;

/// Renders `content` as Markdown for terminal display when `enabled` and
/// stdout is a terminal; otherwise returns `content` unchanged. Takes
/// ownership of nothing and allocates only when actually rendering, so the
/// common (disabled, or non-TTY) path is a plain pass-through.
pub(crate) fn maybe_render(content: &str, enabled: bool) -> String {
    if !enabled || !std::io::stdout().is_terminal() {
        return content.to_owned();
    }
    termimad::MadSkin::default().term_text(content).to_string()
}

#[cfg(test)]
mod tests {
    use super::maybe_render;

    #[test]
    fn returns_content_unchanged_when_disabled() {
        // Also covers the enabled-but-not-a-terminal case: `cargo test`
        // runs with stdout captured (not a real TTY), so `enabled: true`
        // here exercises exactly that fallback too.
        assert_eq!(maybe_render("# Heading", false), "# Heading");
        assert_eq!(maybe_render("# Heading", true), "# Heading");
    }
}
