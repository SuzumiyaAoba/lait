mod support;

use std::time::Duration;

use support::{ConfigDirectory, MockServer, test_command};

const OK_BODY: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"model-a","choices":[{"index":0,"message":{"role":"assistant","content":"cached answer"},"finish_reason":"stop"}]}"#;

fn config_with(base_url: &str, cache_default: bool) -> ConfigDirectory {
    ConfigDirectory::new(&format!(
        "default:\n  model: m\n  cache: {cache_default}\nmodels:\n  m:\n    - provider:\n        base_url: \"{base_url}\"\n      model_id: model-a\n"
    ))
}

#[test]
fn a_second_identical_request_is_served_from_the_cache() {
    let server = MockServer::start("200 OK", OK_BODY);
    let config = config_with(&server.base_url, false);

    let first = test_command()
        .current_dir(config.path())
        .args(["--cache", "hello"])
        .output()
        .expect("failed to execute lait");
    server.receive_request();
    assert!(first.status.success(), "first run failed: {first:?}");
    assert_eq!(
        String::from_utf8_lossy(&first.stdout).trim(),
        "cached answer"
    );

    let second = test_command()
        .current_dir(config.path())
        .args(["--cache", "hello"])
        .output()
        .expect("failed to execute lait");
    assert!(second.status.success(), "second run failed: {second:?}");
    assert_eq!(
        String::from_utf8_lossy(&second.stdout).trim(),
        "cached answer"
    );
    let leaked = server.try_receive_request(Duration::from_millis(600));
    server.finish();
    assert!(
        leaked.is_none(),
        "the second identical run should have been served from the cache, not the network"
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("note:") && stderr.contains("cache"),
        "expected a cache-hit note on stderr, stderr: {stderr}"
    );
}

#[test]
fn no_cache_overrides_default_cache_true_and_always_hits_the_network() {
    let server = MockServer::start_sequence(&[("200 OK", OK_BODY), ("200 OK", OK_BODY)]);
    let config = config_with(&server.base_url, true);

    for _ in 0..2 {
        let output = test_command()
            .current_dir(config.path())
            .args(["--no-cache", "hello"])
            .output()
            .expect("failed to execute lait");
        assert!(output.status.success(), "run failed: {output:?}");
    }
    // Both runs must have reached the network — `receive_request` blocks
    // (with a timeout) until a connection arrives, so this hangs/fails if
    // `--no-cache` didn't actually disable the cache `default.cache: true`
    // would otherwise enable.
    server.receive_request();
    server.receive_request();
    server.finish();
}

#[test]
fn cache_clear_forces_the_next_request_back_to_the_network() {
    let server = MockServer::start_sequence(&[("200 OK", OK_BODY), ("200 OK", OK_BODY)]);
    let config = config_with(&server.base_url, false);

    let first = test_command()
        .current_dir(config.path())
        .args(["--cache", "hello"])
        .output()
        .expect("failed to execute lait");
    assert!(first.status.success(), "first run failed: {first:?}");
    server.receive_request();

    let clear = test_command()
        .current_dir(config.path())
        .args(["cache", "clear"])
        .output()
        .expect("failed to execute lait cache clear");
    assert!(clear.status.success(), "cache clear failed: {clear:?}");

    let second = test_command()
        .current_dir(config.path())
        .args(["--cache", "hello"])
        .output()
        .expect("failed to execute lait");
    assert!(second.status.success(), "second run failed: {second:?}");
    // Must reach the network again now that the cache was cleared.
    server.receive_request();
    server.finish();
}

#[test]
fn a_different_api_key_still_hits_the_cache() {
    let server = MockServer::start("200 OK", OK_BODY);
    let config = config_with(&server.base_url, false);

    let first = test_command()
        .current_dir(config.path())
        .args(["--cache", "--api-key", "key-a", "hello"])
        .output()
        .expect("failed to execute lait");
    assert!(first.status.success(), "first run failed: {first:?}");
    server.receive_request();

    let second = test_command()
        .current_dir(config.path())
        .args(["--cache", "--api-key", "key-b", "hello"])
        .output()
        .expect("failed to execute lait");
    assert!(second.status.success(), "second run failed: {second:?}");
    assert_eq!(
        String::from_utf8_lossy(&second.stdout).trim(),
        "cached answer"
    );
    let leaked = server.try_receive_request(Duration::from_millis(600));
    server.finish();
    assert!(
        leaked.is_none(),
        "a different --api-key should not bypass the cache — the cache key deliberately excludes it"
    );
}
