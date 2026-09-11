//! File discovery: which paths `lait lint <DIR>` walks into and which of
//! those it treats as lintable (a `.yml`/`.yaml` workflow file, or a `.md`
//! file that looks like an agent file — see [`has_frontmatter_delimiter`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::storage;

/// Directory names `lait lint <DIR>` never descends into, even though they
/// don't start with `.` (dot-directories, e.g. `.git`, are always skipped
/// too) — scanning them would be slow, and their `.yml`/`.md` files
/// (dependency manifests, changelogs, CI configs belonging to a vendored
/// package, ...) are never lait workflow/agent files.
const SKIPPED_DIR_NAMES: &[&str] = &["target", "node_modules"];

/// Expands `paths` (files and/or directories, as `lait lint` accepts) into
/// the sorted, deduplicated list of files to actually lint: a file entry is
/// kept as-is (even one with an extension `lint_file` will go on to reject,
/// so that error is still reported per file); a directory entry is searched
/// recursively for `.yml`/`.yaml` files and `.md` files that start with a
/// `---` frontmatter delimiter (see `has_frontmatter_delimiter`), skipping
/// `SKIPPED_DIR_NAMES` and dot-directories along the way.
pub(super) fn expand_lint_targets(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            collect_lintable_files(path, &mut files)?;
        } else {
            files.push(path.clone());
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_lintable_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = std::fs::read_dir(dir)
        .with_context(|| storage::read_dir_context(dir))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| storage::read_dir_context(dir))?;
    // Deterministic traversal order, so directory expansion is stable across
    // runs/platforms (relied on by tests, and generally friendlier for CI
    // diffs than filesystem-dependent order).
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect '{}'", path.display()))?;
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIPPED_DIR_NAMES.contains(&name.as_ref()) {
                continue;
            }
            collect_lintable_files(&path, out)?;
        } else if file_type.is_file() {
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("yml") | Some("yaml") => out.push(path),
                Some("md") if has_frontmatter_delimiter(&path)? => out.push(path),
                _ => {}
            }
        }
    }
    Ok(())
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

    #[test]
    fn expand_lint_targets_passes_through_explicit_files_unchanged() {
        let files = expand_lint_targets(&[PathBuf::from("a.yml"), PathBuf::from("b.md")]).unwrap();
        assert_eq!(files, vec![PathBuf::from("a.yml"), PathBuf::from("b.md")]);
    }
}
