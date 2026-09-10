//! TTY Markdown rendering (`--render`/`default.render`, see
//! `docs/usage/ja/output.md`): decorates a response's Markdown (headings,
//! lists, emphasis, code blocks, tables, ...) for terminal display via
//! `termimad`, falling back to the raw text whenever that wouldn't make
//! sense — rendering is off, or stdout isn't an actual terminal (a pipe, a
//! redirect to a file) where ANSI escapes would just be noise.

use std::{borrow::Cow, io::IsTerminal, sync::LazyLock};

/// `termimad::MadSkin::default()` is a fixed, stateless style table (no
/// per-render configuration ever varies it here), so it's built once and
/// shared instead of reconstructed on every rendered response.
/// `Send + Sync` holds because every field is either a `Copy` style/color
/// type or a `&'static` reference (`skin.rs`'s struct definition) — nothing
/// interior-mutable, so sharing one instance across renders is safe.
static SKIN: LazyLock<termimad::MadSkin> = LazyLock::new(termimad::MadSkin::default);

const _: fn() = || {
    fn assert_sync<T: Sync>() {}
    assert_sync::<termimad::MadSkin>();
};

/// Renders `content` as Markdown for terminal display when `enabled` and
/// stdout is a terminal; otherwise returns `content` unchanged. Returns a
/// borrow of `content` in the common (disabled, or non-TTY) path instead of
/// an owned copy — the caller (`report::emit_output`) only ever needs to
/// print the result once, immediately, so there is nothing for the copy to
/// buy.
pub(crate) fn maybe_render(content: &str, enabled: bool) -> Cow<'_, str> {
    if !enabled || !std::io::stdout().is_terminal() {
        return Cow::Borrowed(content);
    }
    Cow::Owned(SKIN.term_text(content).to_string())
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
