#[cfg(unix)]
use super::read_file_wait_for_fifo_writer;
use super::{
    MAX_READ_BYTES, ReadBudget, acquire_path_lock, read_file, read_to_string_sync,
    run_blocking_with_path_lock, run_blocking_with_pool, write_output_file,
};
use std::{
    fs::{self, OpenOptions},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

fn test_worker_pool() -> Arc<tokio::sync::Semaphore> {
    Arc::new(tokio::sync::Semaphore::new(super::MAX_BLOCKING_WORKERS))
}

/// Creates a fresh, empty named pipe (Unix `mkfifo`) at a unique temp path
/// under `prefix`, for the several tests below that exercise a read against
/// a FIFO with no writer yet connected (or one that closes/retries). Panics
/// on failure rather than returning `Result` — every call site would just
/// `.unwrap()` it anyway, and a `mkfifo` failure means the test environment
/// itself is broken, not that the test found a bug.
#[cfg(unix)]
fn make_fifo(prefix: &str) -> std::path::PathBuf {
    let path = crate::test_support::unique_temp_path(prefix, "");
    let status = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .expect("mkfifo should be available on Unix");
    assert!(status.success(), "mkfifo {path:?} failed");
    path
}

#[tokio::test]
async fn cancellation_cleanup_does_not_wait_for_an_uncooperative_worker() {
    let token = CancellationToken::new();
    let started = Arc::new(AtomicBool::new(false));
    let worker_started = Arc::clone(&started);
    let task = tokio::spawn(run_blocking_with_pool(
        move |_| {
            worker_started.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(500));
            Ok(())
        },
        Some(token.clone()),
        test_worker_pool(),
    ));

    let wait_started = Instant::now();
    while !started.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }
    token.cancel();
    let result = task.await.unwrap();

    assert!(result.is_err());
    assert!(
        wait_started.elapsed() < Duration::from_millis(400),
        "cancellation waited for the worker: {:?}",
        wait_started.elapsed()
    );
}

#[tokio::test]
async fn saturated_worker_acquisition_recovers_after_workers_release() {
    let mut workers = Vec::new();
    let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let worker_pool = test_worker_pool();

    for _ in 0..super::MAX_BLOCKING_WORKERS {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        workers.push(tokio::spawn(run_blocking_with_pool(
            move |_| {
                started.fetch_add(1, Ordering::AcqRel);
                while !release.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            },
            None,
            Arc::clone(&worker_pool),
        )));
    }

    let wait_started = Instant::now();
    while started.load(Ordering::Acquire) < super::MAX_BLOCKING_WORKERS {
        tokio::task::yield_now().await;
    }
    let token = CancellationToken::new();
    let result = run_blocking_with_pool(move |_| Ok(()), Some(token), Arc::clone(&worker_pool));
    let result = tokio::time::timeout(
        super::BLOCKING_WORKER_ACQUIRE_TIMEOUT + Duration::from_millis(100),
        result,
    )
    .await
    .expect("a saturated worker pool must not wait forever")
    .unwrap_err();
    assert!(result.to_string().contains("saturated"));
    assert!(wait_started.elapsed() < Duration::from_secs(2));

    // The failed acquisition must not consume or permanently lose a
    // permit. Once the existing workers finish, a new operation should
    // be admitted and run normally.
    release.store(true, Ordering::Release);
    for worker in workers {
        worker.await.unwrap().unwrap();
    }

    let ran = Arc::new(AtomicBool::new(false));
    let worker_ran = Arc::clone(&ran);
    let result = tokio::time::timeout(
        super::BLOCKING_WORKER_ACQUIRE_TIMEOUT + Duration::from_millis(100),
        run_blocking_with_pool(
            move |_| {
                worker_ran.store(true, Ordering::Release);
                Ok(42_u8)
            },
            None,
            Arc::clone(&worker_pool),
        ),
    )
    .await
    .expect("a released worker permit must admit a subsequent operation")
    .unwrap();
    assert_eq!(result, 42);
    assert!(ran.load(Ordering::Acquire));
}

#[tokio::test]
async fn an_already_cancelled_operation_does_not_spawn_a_worker() {
    let token = CancellationToken::new();
    token.cancel();
    let ran = Arc::new(AtomicBool::new(false));
    let worker_ran = Arc::clone(&ran);

    let result = run_blocking_with_pool(
        move |_| {
            worker_ran.store(true, Ordering::Release);
            Ok(())
        },
        Some(token),
        test_worker_pool(),
    )
    .await;

    assert!(result.is_err());
    assert!(
        !ran.load(Ordering::Acquire),
        "an already-cancelled operation must stop before spawning a worker"
    );
}

#[tokio::test]
async fn a_path_lease_stays_with_an_uncooperative_worker_after_cancellation() {
    let path = crate::test_support::unique_temp_path("lait-test-path-lease", ".out");
    let token = CancellationToken::new();
    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let worker_started = Arc::clone(&started);
    let worker_finished = Arc::clone(&finished);
    let worker_path = path.clone();
    let task_token = token.clone();
    let task = tokio::spawn(async move {
        run_blocking_with_path_lock(
            &worker_path,
            move |_| {
                // This models a network/FUSE write that ignores the
                // cooperative cancellation flag while the kernel call is
                // in progress. The injected closure makes ownership
                // behavior deterministic without relying on a particular
                // filesystem.
                worker_started.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(500));
                worker_finished.store(true, Ordering::Release);
                Ok(())
            },
            Some(task_token),
        )
        .await
    });

    while !started.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }
    token.cancel();
    let cancelled = tokio::time::timeout(Duration::from_millis(250), task)
        .await
        .expect("cancellation cleanup must remain bounded")
        .unwrap();
    assert!(cancelled.is_err());
    assert!(!finished.load(Ordering::Acquire));

    let retry_token = CancellationToken::new();
    let retry = tokio::time::timeout(
        super::BLOCKING_WORKER_ACQUIRE_TIMEOUT + Duration::from_millis(100),
        acquire_path_lock(&path, Some(&retry_token)),
    )
    .await
    .expect("a retry must not wait indefinitely for a stuck writer")
    .unwrap_err();
    assert!(retry.to_string().contains("previous write"));

    // Once the injected worker returns, its closure drops the lease and
    // the same path can be admitted normally again.
    let deadline = Instant::now() + Duration::from_secs(1);
    while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(finished.load(Ordering::Acquire));
    let retry_token = CancellationToken::new();
    let _permit = acquire_path_lock(&path, Some(&retry_token)).await.unwrap();
}

#[tokio::test]
async fn blocking_workers_are_limited() {
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let maximum = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut tasks = Vec::new();
    let worker_pool = test_worker_pool();

    for _ in 0..(super::MAX_BLOCKING_WORKERS * 2) {
        let active = Arc::clone(&active);
        let maximum = Arc::clone(&maximum);
        tasks.push(tokio::spawn(run_blocking_with_pool(
            move |_| {
                let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                maximum.fetch_max(current, Ordering::AcqRel);
                std::thread::sleep(Duration::from_millis(20));
                active.fetch_sub(1, Ordering::AcqRel);
                Ok(())
            },
            None,
            Arc::clone(&worker_pool),
        )));
    }

    let mut successful_tasks = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(()) => successful_tasks += 1,
            Err(error) if error.to_string().contains("saturated") => {}
            Err(error) => panic!("unexpected worker failure: {error:#}"),
        }
    }
    assert!(successful_tasks > 0, "at least one worker must be admitted");
    assert!(
        maximum.load(Ordering::Acquire) <= super::MAX_BLOCKING_WORKERS,
        "too many blocking workers ran concurrently: {}",
        maximum.load(Ordering::Acquire)
    );
}

#[test]
fn read_file_rejects_bytes_beyond_the_explicit_limit() {
    let path = crate::test_support::unique_temp_path("lait-test-read-limit", ".txt");
    fs::write(&path, b"1234").unwrap();
    let cancelled = AtomicBool::new(false);

    let error = read_file(&path, &cancelled, 3).unwrap_err();
    assert!(error.to_string().contains("read limit"));
    let _ = fs::remove_file(path);
}

/// `read_to_string_sync` (the sync-path counterpart every purely local
/// command's schema/config loader now goes through — see B1-G2's commit
/// message) enforces the same `MAX_READ_BYTES` bound as
/// `read_to_string_cancellable`'s async path, rather than the unbounded
/// `std::fs::read_to_string` it replaced. Mirrors
/// `read_file_rejects_bytes_beyond_the_explicit_limit` above, at the
/// actual configured limit instead of an explicit small one.
#[test]
fn read_to_string_sync_rejects_a_file_beyond_max_read_bytes() {
    let path = crate::test_support::unique_temp_path("lait-test-sync-read-limit", ".txt");
    fs::write(&path, vec![b'a'; MAX_READ_BYTES + 1]).unwrap();

    let error = read_to_string_sync(&path).unwrap_err();
    assert!(error.to_string().contains("read limit"));
    let _ = fs::remove_file(path);
}

#[test]
fn a_shared_read_budget_caps_the_combined_materialized_bytes() {
    let first = crate::test_support::unique_temp_path("lait-test-read-budget", "-first.txt");
    let second = crate::test_support::unique_temp_path("lait-test-read-budget", "-second.txt");
    fs::write(&first, b"123").unwrap();
    fs::write(&second, b"456").unwrap();
    let cancelled = AtomicBool::new(false);
    let budget = ReadBudget::new(5);

    assert_eq!(
        super::read_file_with_budget(&first, &cancelled, 5, &budget, false).unwrap(),
        b"123"
    );
    let error = super::read_file_with_budget(&second, &cancelled, 5, &budget, false).unwrap_err();
    assert!(error.to_string().contains("read limit"));
    let _ = fs::remove_file(first);
    let _ = fs::remove_file(second);
}

#[cfg(unix)]
#[test]
fn empty_fifo_is_eof_without_waiting_for_a_writer_by_default() {
    let path = make_fifo("lait-test-empty-fifo");
    let cancelled = AtomicBool::new(false);
    let started = Instant::now();
    let result = read_file(&path, &cancelled, 16).unwrap();

    assert!(result.is_empty());
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "empty FIFO did not return EOF promptly: {:?}",
        started.elapsed()
    );
    let _ = fs::remove_file(path);
}

#[cfg(unix)]
#[tokio::test]
async fn waiting_for_a_fifo_writer_can_be_cancelled_before_a_writer_connects() {
    let path = make_fifo("lait-test-fifo-cancel");

    let token = CancellationToken::new();
    let worker_path = path.clone();
    let task = tokio::spawn(run_blocking_with_pool(
        move |cancelled| {
            read_file_wait_for_fifo_writer(&worker_path, cancelled, super::MAX_READ_BYTES)
        },
        Some(token.clone()),
        test_worker_pool(),
    ));

    tokio::time::sleep(Duration::from_millis(50)).await;
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("a FIFO read without a writer must react to cancellation")
        .unwrap();
    assert!(result.is_err());

    let _ = fs::remove_file(path);
}

#[cfg(unix)]
#[tokio::test]
async fn waiting_for_a_fifo_writer_returns_empty_eof_after_an_empty_writer_closes() {
    let path = make_fifo("lait-test-fifo-empty");

    let worker_path = path.clone();
    let task = tokio::spawn(run_blocking_with_pool(
        move |cancelled| {
            read_file_wait_for_fifo_writer(&worker_path, cancelled, super::MAX_READ_BYTES)
        },
        None,
        test_worker_pool(),
    ));

    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        // Keep the writer open long enough for poll to observe the
        // transition from POLLHUP to the connected/no-data state.
        std::thread::sleep(Duration::from_millis(50));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(writer_path)
            .expect("failed to open FIFO writer");
        std::thread::sleep(Duration::from_millis(50));
        drop(file);
    });

    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("an empty connected FIFO must eventually return EOF")
        .unwrap()
        .unwrap();
    writer.join().unwrap();
    assert!(result.is_empty());

    let _ = fs::remove_file(path);
}

#[tokio::test]
async fn a_cancelled_regular_write_does_not_truncate_an_existing_file() {
    let path = std::env::temp_dir().join(format!(
        "lait-cancelled-output-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos()
    ));
    std::fs::write(&path, "original").expect("failed to create output fixture");
    let token = CancellationToken::new();
    token.cancel();

    let result = write_output_file(&path, "replacement", Some(token)).await;
    let contents = std::fs::read_to_string(&path).expect("output fixture should remain");
    std::fs::remove_file(&path).expect("failed to remove output fixture");

    assert!(result.is_err(), "a cancelled write should fail");
    assert_eq!(contents, "original");
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn hardlink_writers_publish_one_complete_payload() {
    let dir = crate::test_support::unique_temp_path("lait-hardlink-writers", "");
    fs::create_dir(&dir).unwrap();
    let first_path = dir.join("first.txt");
    let second_path = dir.join("second.txt");
    fs::write(&first_path, "seed").unwrap();
    fs::hard_link(&first_path, &second_path).unwrap();

    let first_output = "A".repeat(2 * 1024 * 1024);
    let second_output = "B".repeat(2 * 1024 * 1024);
    let (first, second) = tokio::join!(
        write_output_file(&first_path, &first_output, None),
        write_output_file(&second_path, &second_output, None),
    );
    first.unwrap();
    second.unwrap();

    let written = fs::read(&first_path).unwrap();
    assert!(
        written == first_output.as_bytes() || written == second_output.as_bytes(),
        "hardlink writes must not interleave"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn cancelling_a_hardlink_writer_before_its_lease_keeps_original_contents() {
    let dir = crate::test_support::unique_temp_path("lait-hardlink-cancel", "");
    fs::create_dir(&dir).unwrap();
    let first_path = dir.join("first.txt");
    let second_path = dir.join("second.txt");
    fs::write(&first_path, "original").unwrap();
    fs::hard_link(&first_path, &second_path).unwrap();

    let held_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&first_path)
        .unwrap();
    let held_cancelled = AtomicBool::new(false);
    let _held_lease =
        crate::file_lock::ExclusiveLease::acquire(&held_file, &held_cancelled).unwrap();

    let cancellation = CancellationToken::new();
    let writer_path = second_path.clone();
    let writer_cancellation = cancellation.clone();
    let writer = tokio::spawn(async move {
        write_output_file(&writer_path, "replacement", Some(writer_cancellation)).await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), writer)
        .await
        .expect("a contended hardlink writer should cancel promptly")
        .unwrap();
    assert!(result.is_err());
    drop(_held_lease);
    drop(held_file);
    assert_eq!(fs::read_to_string(&first_path).unwrap(), "original");
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn a_cancelled_fifo_write_is_joined_before_a_retry_can_write() {
    use std::{
        fs::OpenOptions,
        io::{ErrorKind, Read},
        os::unix::fs::OpenOptionsExt,
        sync::mpsc,
    };

    let path = make_fifo("lait-retry-output-fifo");

    // Keep the reader open for the whole test. It pauses after observing
    // the first byte so the first writer fills the pipe, but it never
    // treats an EOF gap between writers as the end of the test.
    let reader = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .expect("FIFO reader should open without a writer");
    let first_output = "x".repeat(512 * 1024);
    let retry_output = "y".repeat(512 * 1024);
    let reader_may_drain = Arc::new(AtomicBool::new(false));
    let retry_done = Arc::new(AtomicBool::new(false));
    let stop_reader = Arc::new(AtomicBool::new(false));
    let reader_may_drain_flag = Arc::clone(&reader_may_drain);
    let reader_retry_done = Arc::clone(&retry_done);
    let reader_stop = Arc::clone(&stop_reader);
    let (reader_done, reader_result) = mpsc::channel();
    let (first_byte_sender, first_byte_receiver) = tokio::sync::oneshot::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut reader = reader;
        let mut received = Vec::new();
        let mut buffer = [0_u8; 16 * 1024];
        let mut first_byte_reported = false;
        let mut first_byte_sender = Some(first_byte_sender);
        let result = loop {
            if reader_stop.load(Ordering::Acquire) {
                break Ok(received);
            }
            match reader.read(&mut buffer) {
                Ok(0) => {
                    if reader_retry_done.load(Ordering::Acquire) {
                        break Ok(received);
                    }
                    // A FIFO reports EOF while the first writer has
                    // closed and before the retry writer opens it. Keep
                    // the descriptor alive until the retry is known done.
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(read) => {
                    received.extend_from_slice(&buffer[..read]);
                    if !first_byte_reported {
                        first_byte_reported = true;
                        let _ = first_byte_sender
                            .take()
                            .expect("first-byte notification sender is live")
                            .send(());
                        // Pause after the handshake byte. This leaves the
                        // first writer blocked in the FIFO, making the
                        // cancellation boundary deterministic.
                        while !reader_may_drain_flag.load(Ordering::Acquire)
                            && !reader_stop.load(Ordering::Acquire)
                        {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => break Err(format!("failed to read retry FIFO: {error}")),
            }
        };
        reader_done
            .send(result)
            .expect("reader result receiver is live");
    });

    let cancel_token = CancellationToken::new();
    let first_path = path.clone();
    let first_output_for_task = first_output.clone();
    let first_token = cancel_token.clone();
    let mut first = tokio::spawn(async move {
        write_output_file(&first_path, &first_output_for_task, Some(first_token)).await
    });

    let first_started = tokio::time::timeout(Duration::from_secs(1), first_byte_receiver)
        .await
        .is_ok_and(|result| result.is_ok());
    if !first_started {
        stop_reader.store(true, Ordering::Release);
        reader_may_drain.store(true, Ordering::Release);
        first.abort();
        let _ = first.await;
        let _ = reader_thread.join();
        let _ = std::fs::remove_file(&path);
        panic!("first FIFO writer did not publish the handshake byte");
    }
    cancel_token.cancel();
    let first_result = tokio::time::timeout(Duration::from_secs(1), &mut first)
        .await
        .map(|result| result.expect("first FIFO writer task should join"));
    if first_result.is_err() || first_result.as_ref().is_ok_and(|result| result.is_ok()) {
        stop_reader.store(true, Ordering::Release);
        reader_may_drain.store(true, Ordering::Release);
        if first_result.is_err() {
            first.abort();
            let _ = first.await;
        }
        let _ = reader_thread.join();
        let _ = std::fs::remove_file(&path);
        panic!("cancelling the first FIFO writer did not produce the expected failure");
    }

    // Start the retry while the reader is still open. It may now drain,
    // but a transient EOF cannot terminate it before this writer reports
    // completion.
    reader_may_drain.store(true, Ordering::Release);
    let second_path = path.clone();
    let second_output = retry_output.clone();
    let mut second =
        tokio::spawn(async move { write_output_file(&second_path, &second_output, None).await });

    let second_result = tokio::time::timeout(Duration::from_secs(2), &mut second)
        .await
        .map(|result| result.expect("retry FIFO writer task should join"));
    retry_done.store(true, Ordering::Release);
    if second_result.is_err() {
        stop_reader.store(true, Ordering::Release);
        second.abort();
        let _ = second.await;
        let _ = reader_thread.join();
        let _ = std::fs::remove_file(&path);
        let reader_diagnostic = match reader_result.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(bytes)) => format!(
                "reader received {} bytes (x={}, y={})",
                bytes.len(),
                bytes.iter().filter(|byte| **byte == b'x').count(),
                bytes.iter().filter(|byte| **byte == b'y').count(),
            ),
            Ok(Err(error)) => format!("reader failed: {error}"),
            Err(error) => format!("reader result unavailable: {error}"),
        };
        panic!("the retry FIFO writer timed out; {reader_diagnostic}");
    }
    if second_result.as_ref().is_ok_and(|result| result.is_err()) {
        let _ = reader_thread.join();
        let _ = std::fs::remove_file(&path);
        panic!("the retry FIFO writer failed");
    }
    let _ = reader_thread.join();
    let received = reader_result
        .recv()
        .expect("FIFO reader should report after the retry writer closes")
        .expect("FIFO reader should finish without an I/O error");

    assert!(
        received.len() >= retry_output.len(),
        "retry FIFO received {} bytes, less than the retry payload of {}",
        received.len(),
        retry_output.len()
    );
    let first_prefix_len = received.len() - retry_output.len();
    assert!(
        first_prefix_len <= first_output.len(),
        "cancelled FIFO writer published {} bytes after the retry started",
        first_prefix_len.saturating_sub(first_output.len())
    );
    assert!(
        received[..first_prefix_len]
            .iter()
            .all(|byte| *byte == b'x'),
        "the cancelled writer's bytes must precede the retry payload"
    );
    assert_eq!(&received[first_prefix_len..], retry_output.as_bytes());
    std::fs::remove_file(path).unwrap();
}
#[tokio::test]
async fn output_path_aliases_share_the_same_lease_before_and_after_creation() {
    let dir = crate::test_support::unique_temp_path("lait-output-alias", "");
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("output.txt");
    let alias = dir.join(".").join("output.txt");
    let lease = acquire_path_lock(&path, None).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), acquire_path_lock(&alias, None))
            .await
            .is_err()
    );
    std::fs::write(&path, "created while holding the lease").unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), acquire_path_lock(&alias, None))
            .await
            .is_err()
    );
    drop(lease);
    let _next = tokio::time::timeout(Duration::from_secs(1), acquire_path_lock(&alias, None))
        .await
        .unwrap()
        .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn a_dangling_output_symlink_and_its_target_share_a_lease() {
    let dir = crate::test_support::unique_temp_path("lait-output-symlink", "");
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("output.txt");
    let link = dir.join("link.txt");
    std::os::unix::fs::symlink("output.txt", &link).unwrap();
    let lease = acquire_path_lock(&link, None).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), acquire_path_lock(&path, None))
            .await
            .is_err()
    );
    drop(lease);
    let _next = tokio::time::timeout(Duration::from_secs(1), acquire_path_lock(&path, None))
        .await
        .unwrap()
        .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
#[cfg(unix)]
#[tokio::test]
async fn waiting_for_a_fifo_writer_needs_only_read_permission() {
    use std::os::unix::fs::PermissionsExt;
    let path = make_fifo("lait-read-only-fifo");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let mut read = Box::pin(super::read_to_string_cancellable(
        &path,
        Some(cancellation.clone()),
        1024,
    ));
    tokio::select! {
        result = &mut read => panic!("reader returned before any writer: {result:?}"),
        () = tokio::time::sleep(Duration::from_millis(50)) => cancellation.cancel(),
    }
    let error = tokio::time::timeout(Duration::from_secs(1), read)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.downcast_ref::<crate::error::Interrupted>().is_some());
    std::fs::remove_file(path).unwrap();
}
