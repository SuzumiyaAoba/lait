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

use crate::async_io;

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
///
/// The reads run concurrently (see `read_all`) since they are otherwise
/// independent. The shared read budget is enforced while bytes are
/// materialized, using the same descriptor that was opened for the read; this
/// avoids a metadata-then-open TOCTOU window for paths that are replaced while
/// attachments are being resolved.
pub(crate) async fn read_file_attachments(files: &[PathBuf]) -> Result<Option<String>> {
    read_file_attachments_cancellable(files, None).await
}

/// The cancellation-aware form used by workflow nodes. Every content read
/// runs on a dedicated worker, so a timeout can interrupt a FIFO or a slow
/// filesystem even before the first model request is made.
pub(crate) async fn read_file_attachments_cancellable(
    files: &[PathBuf],
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<Option<String>> {
    if files.is_empty() {
        return Ok(None);
    }

    let texts = read_all(files, cancellation).await?;
    let blocks: Vec<String> = files
        .iter()
        .zip(texts)
        .map(|(path, text)| fenced_block(path, &text))
        .collect();
    Ok(Some(blocks.join("\n\n")))
}

/// Reads and UTF-8-decodes every path in `files`, in order. Each independent
/// read has its own dedicated worker rather than a Tokio `spawn_blocking`
/// task: dropping one read after a timeout signals its worker instead of
/// leaving it running in the runtime's blocking pool.
async fn read_all(
    files: &[PathBuf],
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<Vec<String>> {
    let budget = async_io::ReadBudget::new(MAX_TOTAL_ATTACHMENT_BYTES as usize);
    // Preserve the historical `fs::read` behavior for all callers: a FIFO
    // attachment waits until a writer appears. The cancellation-aware worker
    // still polls its descriptor, so a timed workflow can interrupt that wait
    // through the shared flag.
    let wait_for_fifo_writer = true;
    let reads = files.iter().cloned().map(|path| {
        read_text_file_cancellable(
            path,
            cancellation.clone(),
            budget.clone(),
            wait_for_fifo_writer,
        )
    });
    futures_util::future::try_join_all(reads).await
}

async fn read_text_file_cancellable(
    path: PathBuf,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
    budget: async_io::ReadBudget,
    wait_for_fifo_writer: bool,
) -> Result<String> {
    let error_path = path.clone();
    let contents = async_io::run_blocking(
        move |cancelled| {
            async_io::read_file_with_budget(
                &path,
                cancelled,
                MAX_TOTAL_ATTACHMENT_BYTES as usize,
                &budget,
                wait_for_fifo_writer,
            )
        },
        cancellation,
    )
    .await
    .with_context(|| format!("failed to read file '{}'", error_path.display()))?;
    String::from_utf8(contents).map_err(|_| {
        anyhow::anyhow!(
            "'--file {}' is not valid UTF-8 text; binary files are not supported",
            error_path.display()
        )
    })
}

/// Resolves every `--image` value into a URL a vision-capable model's
/// `image_url` content part can carry: an `http://`/`https://` value passes
/// through unchanged, everything else is treated as a local file path, read,
/// sniffed for its image format (see `sniff_image_mime`), and base64-encoded
/// into a `data:<mime>;base64,<data>` URL. Returns an empty `Vec` (not an
/// error) for an empty `images`, so a caller can pass the result straight to
/// `llm::initial_messages`'s `image_urls` without a separate empty check. A
/// single image is resolved inline; two or more run concurrently, the same
/// way `read_all` above handles multiple `--file` attachments.
pub(crate) async fn resolve_image_urls(images: &[String]) -> Result<Vec<String>> {
    resolve_image_urls_cancellable(images, None).await
}

/// The cancellation-aware form used by workflow nodes. Local image files use
/// the same worker/non-blocking read path as text attachments; HTTP URLs still
/// pass through without touching the filesystem.
pub(crate) async fn resolve_image_urls_cancellable(
    images: &[String],
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<Vec<String>> {
    match images {
        [] => Ok(Vec::new()),
        _ => {
            let budget = async_io::ReadBudget::new(async_io::MAX_READ_BYTES);
            // A local image path follows the same FIFO semantics as a text
            // attachment. HTTP(S) values return immediately and never touch
            // the filesystem.
            let wait_for_fifo_writer = true;
            let resolutions = images.iter().cloned().map(|image| {
                resolve_one_cancellable(
                    image,
                    cancellation.clone(),
                    budget.clone(),
                    wait_for_fifo_writer,
                )
            });
            futures_util::future::try_join_all(resolutions).await
        }
    }
}

async fn resolve_one_cancellable(
    image: String,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
    budget: async_io::ReadBudget,
    wait_for_fifo_writer: bool,
) -> Result<String> {
    async_io::run_blocking(
        move |cancelled| resolve_one_blocking(&image, cancelled, &budget, wait_for_fifo_writer),
        cancellation,
    )
    .await
}

fn resolve_one_blocking(
    image: &str,
    cancelled: &std::sync::atomic::AtomicBool,
    budget: &async_io::ReadBudget,
    wait_for_fifo_writer: bool,
) -> Result<String> {
    if image.starts_with("http://") || image.starts_with("https://") {
        return Ok(image.to_owned());
    }

    let path = Path::new(image);
    let bytes = async_io::read_file_with_budget(
        path,
        cancelled,
        async_io::MAX_READ_BYTES,
        budget,
        wait_for_fifo_writer,
    )
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
    use super::{
        fenced_block, longest_backtick_run, read_file_attachments,
        read_file_attachments_cancellable, resolve_image_urls,
    };
    use std::path::Path;

    #[tokio::test]
    async fn returns_none_for_no_files() {
        assert!(read_file_attachments(&[]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn resolve_image_urls_returns_empty_for_no_images() {
        assert!(resolve_image_urls(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn resolve_image_urls_passes_http_urls_through_unchanged() {
        let urls = resolve_image_urls(&["https://example.com/cat.png".to_owned()])
            .await
            .unwrap();
        assert_eq!(urls, ["https://example.com/cat.png"]);
    }

    #[tokio::test]
    async fn resolve_image_urls_encodes_a_local_png_as_a_data_url() {
        let mut png_bytes = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        png_bytes.extend_from_slice(b"rest of file");
        let file = TempFile::with_suffix("lait-test-image.png", &png_bytes);

        let urls = resolve_image_urls(&[file.path.display().to_string()])
            .await
            .unwrap();
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with("data:image/png;base64,"));
    }

    #[tokio::test]
    async fn resolve_image_urls_encodes_a_local_jpeg_as_a_data_url() {
        let mut jpeg_bytes = vec![0xff, 0xd8, 0xff, 0xe0];
        jpeg_bytes.extend_from_slice(b"rest of file");
        let file = TempFile::with_suffix("lait-test-image.jpg", &jpeg_bytes);

        let urls = resolve_image_urls(&[file.path.display().to_string()])
            .await
            .unwrap();
        assert!(urls[0].starts_with("data:image/jpeg;base64,"));
    }

    #[tokio::test]
    async fn resolve_image_urls_rejects_an_unrecognized_format() {
        let file = TempFile::with_suffix("lait-test-image.unknown", b"not an image");
        let error = resolve_image_urls(&[file.path.display().to_string()])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("could not determine"));
    }

    #[tokio::test]
    async fn resolve_image_urls_rejects_a_missing_file() {
        assert!(
            resolve_image_urls(&["/no/such/file/lait-test.png".to_owned()])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn resolves_multiple_images_concurrently_and_preserves_order() {
        let mut png_bytes = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        png_bytes.extend_from_slice(b"rest of file");
        let png = TempFile::with_suffix("lait-test-image.png", &png_bytes);
        let mut jpeg_bytes = vec![0xff, 0xd8, 0xff, 0xe0];
        jpeg_bytes.extend_from_slice(b"rest of file");
        let jpeg = TempFile::with_suffix("lait-test-image.jpg", &jpeg_bytes);

        let urls = resolve_image_urls(&[
            png.path.display().to_string(),
            "https://example.com/cat.png".to_owned(),
            jpeg.path.display().to_string(),
        ])
        .await
        .unwrap();
        assert!(urls[0].starts_with("data:image/png;base64,"));
        assert_eq!(urls[1], "https://example.com/cat.png");
        assert!(urls[2].starts_with("data:image/jpeg;base64,"));
    }

    #[tokio::test]
    async fn reads_a_single_file_into_a_fenced_block() {
        let file = TempFile::new("hello world\n");
        let result = read_file_attachments(std::slice::from_ref(&file.path))
            .await
            .unwrap()
            .unwrap();
        assert!(result.starts_with("```"));
        assert!(result.contains(&file.path.display().to_string()));
        assert!(result.contains("hello world"));
    }

    #[tokio::test]
    async fn joins_multiple_files_with_a_blank_line() {
        let a = TempFile::new("aaa\n");
        let b = TempFile::new("bbb\n");
        let result = read_file_attachments(&[a.path.clone(), b.path.clone()])
            .await
            .unwrap()
            .unwrap();
        assert!(result.contains("aaa"));
        assert!(result.contains("bbb"));
        assert!(result.contains("\n\n```"));
    }

    #[tokio::test]
    async fn rejects_a_missing_file() {
        assert!(
            read_file_attachments(&["/no/such/file/lait-test".into()])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_binary_content() {
        let file = TempFile::new_bytes(&[0xff, 0xfe, 0x00, 0xff]);
        let error = read_file_attachments(std::slice::from_ref(&file.path))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not valid UTF-8"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_cancellable_fifo_attachment_waits_for_a_writer() {
        use std::{io::Write, sync::mpsc, time::Duration};

        let path = crate::test_support::unique_temp_path("lait-test-attachment-fifo", "");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());

        let read_path = path.clone();
        let read_task =
            tokio::spawn(
                async move { read_file_attachments(std::slice::from_ref(&read_path)).await },
            );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !read_task.is_finished(),
            "a non-cancellable FIFO attachment must wait for a writer"
        );

        // Open the writer in a separate thread because a normal blocking FIFO
        // open waits for the reader. The reader worker has already opened the
        // FIFO non-blocking by the time this thread can proceed.
        let writer_path = path.clone();
        let (writer_done, writer_result) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            let result = std::fs::OpenOptions::new()
                .write(true)
                .open(writer_path)
                .and_then(|mut file| file.write_all(b"from fifo\n"));
            writer_done.send(result).unwrap();
        });

        let result = read_task.await.unwrap().unwrap().unwrap();
        writer.join().unwrap();
        writer_result.recv().unwrap().unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(result.contains("from fifo"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellable_fifo_attachment_can_stop_while_waiting_for_a_writer() {
        use std::time::Duration;

        let path = crate::test_support::unique_temp_path("lait-test-cancellable-fifo", "");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());

        let (sender, receiver) = tokio::sync::watch::channel(false);
        let read_path = path.clone();
        let read_task = tokio::spawn(async move {
            read_file_attachments_cancellable(std::slice::from_ref(&read_path), Some(receiver))
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !read_task.is_finished(),
            "a FIFO attachment should wait for a writer before cancellation"
        );

        sender.send(true).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), read_task)
            .await
            .expect("cancelling a FIFO wait must finish promptly")
            .unwrap();
        assert!(result.is_err(), "a cancelled FIFO read must fail");
        std::fs::remove_file(path).unwrap();
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
