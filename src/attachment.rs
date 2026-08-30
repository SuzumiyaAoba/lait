//! `--file` context attachment (see `docs/usage/ja/attachments.md`): reads
//! each path's contents and renders them as named fenced code blocks that get
//! appended after the prompt, so a shell command substitution
//! (`"$(cat file)"`) is never needed to give a request some file context.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The combined size limit across every `--file` attachment, chosen to keep a
/// request body well within what a local server accepts while still allowing
/// a handful of real source files. Exceeding it is a clear user error rather
/// than something to silently truncate.
const MAX_TOTAL_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;

/// Reads every path in `files` and renders it as a `` ```<path>\n<contents>\n``` ``
/// fenced code block, joined by blank lines. Returns `Ok(None)` when `files`
/// is empty, so a caller can skip the "no attachments" case without an empty
/// string to special-case. Fails if any file cannot be read, is not valid
/// UTF-8 text (binary attachments aren't supported), or the combined size of
/// every attachment exceeds `MAX_TOTAL_ATTACHMENT_BYTES`.
pub(crate) fn read_file_attachments(files: &[PathBuf]) -> Result<Option<String>> {
    if files.is_empty() {
        return Ok(None);
    }

    let mut total_bytes: u64 = 0;
    let mut blocks = Vec::with_capacity(files.len());
    for path in files {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("failed to read file '{}'", path.display()))?;
        total_bytes += metadata.len();
        if total_bytes > MAX_TOTAL_ATTACHMENT_BYTES {
            bail!(
                "'--file' attachments exceed the combined size limit of {} bytes",
                MAX_TOTAL_ATTACHMENT_BYTES
            );
        }

        let contents = std::fs::read(path)
            .with_context(|| format!("failed to read file '{}'", path.display()))?;
        let text = String::from_utf8(contents).map_err(|_| {
            anyhow::anyhow!(
                "'--file {}' is not valid UTF-8 text; binary files are not supported",
                path.display()
            )
        })?;
        blocks.push(fenced_block(path, &text));
    }

    Ok(Some(blocks.join("\n\n")))
}

/// Renders one attachment as a fenced code block named after its path. The
/// fence is widened past the longest run of backticks already in `contents`
/// (matching how Pandoc/CommonMark tooling avoids a fence prematurely closing
/// on content that itself contains a code fence), rather than always using a
/// fixed three-backtick fence.
fn fenced_block(path: &Path, contents: &str) -> String {
    let fence_len = longest_backtick_run(contents).max(2) + 1;
    let fence = "`".repeat(fence_len);
    format!("{fence}{}\n{contents}\n{fence}", path.display())
}

/// The length of the longest consecutive run of backtick characters in `text`.
fn longest_backtick_run(text: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for byte in text.bytes() {
        if byte == b'`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

#[cfg(test)]
mod tests {
    use super::{fenced_block, longest_backtick_run, read_file_attachments};
    use std::path::Path;

    #[test]
    fn returns_none_for_no_files() {
        assert!(read_file_attachments(&[]).unwrap().is_none());
    }

    #[test]
    fn reads_a_single_file_into_a_fenced_block() {
        let file = TempFile::new("hello world\n");
        let result = read_file_attachments(std::slice::from_ref(&file.path))
            .unwrap()
            .unwrap();
        assert!(result.starts_with("```"));
        assert!(result.contains(&file.path.display().to_string()));
        assert!(result.contains("hello world"));
    }

    #[test]
    fn joins_multiple_files_with_a_blank_line() {
        let a = TempFile::new("aaa\n");
        let b = TempFile::new("bbb\n");
        let result = read_file_attachments(&[a.path.clone(), b.path.clone()])
            .unwrap()
            .unwrap();
        assert!(result.contains("aaa"));
        assert!(result.contains("bbb"));
        assert!(result.contains("\n\n```"));
    }

    #[test]
    fn rejects_a_missing_file() {
        assert!(read_file_attachments(&["/no/such/file/lait-test".into()]).is_err());
    }

    #[test]
    fn rejects_binary_content() {
        let file = TempFile::new_bytes(&[0xff, 0xfe, 0x00, 0xff]);
        let error = read_file_attachments(std::slice::from_ref(&file.path)).unwrap_err();
        assert!(error.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn longest_backtick_run_finds_the_longest_run() {
        assert_eq!(longest_backtick_run("no backticks here"), 0);
        assert_eq!(longest_backtick_run("one ` run"), 1);
        assert_eq!(longest_backtick_run("```rust\ncode\n```"), 3);
        assert_eq!(longest_backtick_run("`` and ```` mixed"), 4);
    }

    #[test]
    fn fenced_block_widens_past_embedded_backtick_runs() {
        let block = fenced_block(Path::new("x.md"), "```\nnested\n```");
        assert!(block.starts_with("````"));
        assert!(block.ends_with("````"));
    }

    pub(super) struct TempFile {
        pub(super) path: std::path::PathBuf,
    }

    impl TempFile {
        pub(super) fn new(contents: &str) -> Self {
            Self::new_bytes(contents.as_bytes())
        }

        pub(super) fn new_bytes(contents: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "lait-test-attachment-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::write(&path, contents).expect("failed to write temp attachment file");
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
