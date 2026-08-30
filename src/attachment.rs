//! `--file`/`--image` context attachment (see
//! `docs/usage/ja/attachments.md`): `--file` reads each path's contents and
//! renders them as named fenced code blocks appended after the prompt, so a
//! shell command substitution (`"$(cat file)"`) is never needed to give a
//! request some file context; `--image` resolves each path/URL into an
//! `image_url` a vision-capable model's `content` array can carry (see
//! `llm::user_message`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;

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

/// Resolves every `--image` value into a URL a vision-capable model's
/// `image_url` content part can carry: an `http://`/`https://` value passes
/// through unchanged, everything else is treated as a local file path, read,
/// sniffed for its image format (see `sniff_image_mime`), and base64-encoded
/// into a `data:<mime>;base64,<data>` URL. Returns an empty `Vec` (not an
/// error) for an empty `images`, so a caller can pass the result straight to
/// `llm::initial_messages`'s `image_urls` without a separate empty check.
pub(crate) fn resolve_image_urls(images: &[String]) -> Result<Vec<String>> {
    images.iter().map(|image| resolve_one(image)).collect()
}

fn resolve_one(image: &str) -> Result<String> {
    if image.starts_with("http://") || image.starts_with("https://") {
        return Ok(image.to_owned());
    }

    let path = Path::new(image);
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read image file '{}'", path.display()))?;
    let mime = sniff_image_mime(&bytes, path).with_context(|| {
        format!(
            "could not determine the image format of '{}'; supported formats are PNG/JPEG/WebP/GIF",
            path.display()
        )
    })?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{encoded}"))
}

/// Identifies an image's MIME type from its leading magic bytes, falling back
/// to its file extension when the content doesn't match a known signature
/// (e.g. a truncated/malformed-but-still-openable file). Returns `Err` when
/// neither check recognizes the file.
fn sniff_image_mime(bytes: &[u8], path: &Path) -> Result<&'static str> {
    if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Ok("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Ok("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Ok("image/gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Ok("image/webp");
    }

    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Ok("image/png"),
        Some("jpg" | "jpeg") => Ok("image/jpeg"),
        Some("gif") => Ok("image/gif"),
        Some("webp") => Ok("image/webp"),
        _ => bail!("unrecognized image format"),
    }
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
    use super::{fenced_block, longest_backtick_run, read_file_attachments, resolve_image_urls};
    use std::path::Path;

    #[test]
    fn returns_none_for_no_files() {
        assert!(read_file_attachments(&[]).unwrap().is_none());
    }

    #[test]
    fn resolve_image_urls_returns_empty_for_no_images() {
        assert!(resolve_image_urls(&[]).unwrap().is_empty());
    }

    #[test]
    fn resolve_image_urls_passes_http_urls_through_unchanged() {
        let urls = resolve_image_urls(&["https://example.com/cat.png".to_owned()]).unwrap();
        assert_eq!(urls, ["https://example.com/cat.png"]);
    }

    #[test]
    fn resolve_image_urls_encodes_a_local_png_as_a_data_url() {
        let mut png_bytes = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        png_bytes.extend_from_slice(b"rest of file");
        let file = TempFile::with_suffix("lait-test-image.png", &png_bytes);

        let urls = resolve_image_urls(&[file.path.display().to_string()]).unwrap();
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with("data:image/png;base64,"));
    }

    #[test]
    fn resolve_image_urls_encodes_a_local_jpeg_as_a_data_url() {
        let mut jpeg_bytes = vec![0xff, 0xd8, 0xff, 0xe0];
        jpeg_bytes.extend_from_slice(b"rest of file");
        let file = TempFile::with_suffix("lait-test-image.jpg", &jpeg_bytes);

        let urls = resolve_image_urls(&[file.path.display().to_string()]).unwrap();
        assert!(urls[0].starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn resolve_image_urls_rejects_an_unrecognized_format() {
        let file = TempFile::with_suffix("lait-test-image.unknown", b"not an image");
        let error = resolve_image_urls(&[file.path.display().to_string()]).unwrap_err();
        assert!(error.to_string().contains("could not determine"));
    }

    #[test]
    fn resolve_image_urls_rejects_a_missing_file() {
        assert!(resolve_image_urls(&["/no/such/file/lait-test.png".to_owned()]).is_err());
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
            Self::with_suffix("attachment", contents)
        }

        /// Like [`Self::new_bytes`], but `suffix` is appended to the file
        /// name verbatim so an extension (e.g. `"lait-test-image.png"`) is
        /// preserved for MIME-sniffing fallback tests.
        fn with_suffix(suffix: &str, contents: &[u8]) -> Self {
            let path = crate::test_support::unique_temp_path("lait-test", &format!("-{suffix}"));
            std::fs::write(&path, contents).expect("failed to write temp file");
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
