//! File discovery: which paths `lait lint <DIR>` walks into and which of
//! those it treats as lintable (a `.yml`/`.yaml` workflow file, or a `.md`
//! file that looks like an agent file — see [`has_frontmatter_delimiter`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{file_walk::DirWalker, storage};

/// Expands `paths` (files and/or directories, as `lait lint` accepts) into
/// the sorted, deduplicated list of files to actually lint: a file entry is
/// kept as-is (even one with an extension `lint_file` will go on to reject,
/// so that error is still reported per file); a directory entry is searched
/// recursively for `.yml`/`.yaml` files and `.md` files that start with a
/// `---` frontmatter delimiter (see `has_frontmatter_delimiter`), skipping
/// `storage::SKIPPED_DIR_NAMES` and dot-entries along the way. The walk
/// itself is shared with `lait test` — see `file_walk::DirWalker`.
pub(super) fn expand_lint_targets(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    // `lint` runs on the sync path with no async runtime behind it, so
    // there is no cancellation source to wire in — see `DirWalker`'s doc.
    let mut walker = DirWalker::new(
        &crate::cancellation::NEVER_SET,
        "lint target discovery was cancelled",
    );
    for path in paths {
        if path.is_dir() {
            walker.walk(path, &mut |path| {
                match path.extension().and_then(|extension| extension.to_str()) {
                    Some("yml") | Some("yaml") => files.push(path.to_path_buf()),
                    Some("md") if has_frontmatter_delimiter(path)? => {
                        files.push(path.to_path_buf());
                    }
                    _ => {}
                }
                Ok(())
            })?;
        } else {
            files.push(path.clone());
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Cheaply sniffs whether `path` starts with the `---` frontmatter
/// delimiter `frontmatter::split` requires, without fully parsing it as an
/// agent file — only the first line is read. Used by directory expansion to
/// skip ordinary (non-agent) Markdown files like a README.
fn has_frontmatter_delimiter(path: &Path) -> Result<bool> {
    use std::io::BufRead;

    let file = std::fs::File::open(path).with_context(|| storage::read_context(path))?;
    let mut first_line = String::new();
    std::io::BufReader::new(file)
        .read_line(&mut first_line)
        .with_context(|| storage::read_context(path))?;
    Ok(first_line.trim_end_matches(['\n', '\r']) == "---")
}

#[cfg(test)]
mod tests {
    use super::{expand_lint_targets, has_frontmatter_delimiter};
    use std::path::{Path, PathBuf};

    #[test]
    fn has_frontmatter_delimiter_detects_agent_style_files() {
        crate::test_support::in_temp_dir("lait-test-lint-frontmatter", || {
            std::fs::write("agent.md", "---\nname: x\n---\nbody\n").unwrap();
            std::fs::write("plain.md", "# Just a heading\n\nbody\n").unwrap();

            assert!(has_frontmatter_delimiter(Path::new("agent.md")).unwrap());
            assert!(!has_frontmatter_delimiter(Path::new("plain.md")).unwrap());
        });
    }

    #[test]
    fn expand_lint_targets_recurses_into_directories_and_skips_non_agent_markdown() {
        crate::test_support::in_temp_dir("lait-test-lint-expand", || {
            std::fs::create_dir_all("sub").unwrap();
            std::fs::write("sub/workflow.yml", "steps: []\n").unwrap();
            std::fs::write("sub/agent.md", "---\n---\nbody\n").unwrap();
            std::fs::write("sub/README.md", "# not an agent file\n").unwrap();
            std::fs::write("sub/notes.txt", "irrelevant\n").unwrap();

            let files = expand_lint_targets(&[PathBuf::from(".")]).unwrap();

            assert_eq!(
                files,
                vec![
                    PathBuf::from("./sub/agent.md"),
                    PathBuf::from("./sub/workflow.yml"),
                ]
            );
        });
    }

    #[test]
    fn expand_lint_targets_skips_target_and_node_modules_and_dot_directories() {
        crate::test_support::in_temp_dir("lait-test-lint-expand-skip", || {
            std::fs::write("top.yml", "steps: []\n").unwrap();
            std::fs::create_dir_all("target").unwrap();
            std::fs::write("target/build.yml", "steps: []\n").unwrap();
            std::fs::create_dir_all("node_modules/pkg").unwrap();
            std::fs::write("node_modules/pkg/ci.yml", "steps: []\n").unwrap();
            std::fs::create_dir_all(".git").unwrap();
            std::fs::write(".git/config.yml", "steps: []\n").unwrap();

            let files = expand_lint_targets(&[PathBuf::from(".")]).unwrap();

            assert_eq!(files, vec![PathBuf::from("./top.yml")]);
        });
    }

    /// Regression coverage for the normalization `file_walk::DirWalker`
    /// carries: a directory walk now skips dot-prefixed *files*, not just
    /// dot-prefixed directories — see that module's doc comment and
    /// `docs/usage/ja/lint.md`'s directory-walk paragraph. Explicitly named
    /// file arguments are unaffected (see the test just below).
    #[test]
    fn expand_lint_targets_skips_dot_prefixed_files_found_by_directory_expansion() {
        crate::test_support::in_temp_dir("lait-test-lint-expand-skip-dotfile", || {
            std::fs::write("top.yml", "steps: []\n").unwrap();
            std::fs::write(".hidden.yml", "steps: []\n").unwrap();
            std::fs::write(".hidden.md", "---\n---\nbody\n").unwrap();

            let files = expand_lint_targets(&[PathBuf::from(".")]).unwrap();

            assert_eq!(files, vec![PathBuf::from("./top.yml")]);
        });
    }

    #[test]
    fn expand_lint_targets_passes_through_explicit_files_unchanged() {
        let files = expand_lint_targets(&[PathBuf::from("a.yml"), PathBuf::from("b.md")]).unwrap();
        assert_eq!(files, vec![PathBuf::from("a.yml"), PathBuf::from("b.md")]);
    }

    /// A dot-prefixed file passed explicitly (not discovered by a directory
    /// walk) is still linted — the dot-prefix skip only ever applies to
    /// directory expansion, matching `expand_lint_targets`'s own doc.
    #[test]
    fn expand_lint_targets_passes_through_an_explicit_dot_prefixed_file() {
        let files = expand_lint_targets(&[PathBuf::from(".hidden.yml")]).unwrap();
        assert_eq!(files, vec![PathBuf::from(".hidden.yml")]);
    }
}
