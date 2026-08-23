use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
struct HttpRequest {
    method: String,
    target: String,
    headers: String,
    body: String,
}

struct MockServer {
    base_url: String,
    requests: Receiver<HttpRequest>,
    thread: JoinHandle<io::Result<()>>,
}

struct JsonSchemaFile {
    path: PathBuf,
}

impl JsonSchemaFile {
    fn new(contents: &str) -> Self {
        let unique_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "lait-test-schema-{}-{unique_id}.json",
            std::process::id()
        ));
        fs::write(&path, contents).expect("failed to write test JSON schema");
        Self { path }
    }
}

impl Drop for JsonSchemaFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl MockServer {
    fn start(status: &str, response_body: &str) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind mock server");
        let address = listener
            .local_addr()
            .expect("failed to get mock server address");
        let (request_sender, requests) = mpsc::channel();
        let status = status.to_owned();
        let response_body = response_body.to_owned();

        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept()?;
            let request = read_request(&mut stream)?;
            request_sender
                .send(request)
                .map_err(|_| io::Error::other("test receiver was dropped"))?;

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len(),
            );
            stream.write_all(response.as_bytes())?;
            stream.flush()
        });

        Self {
            base_url: format!("http://{address}/v1"),
            requests,
            thread,
        }
    }

    fn receive_request(&self) -> HttpRequest {
        self.requests
            .recv_timeout(Duration::from_secs(5))
            .expect("mock server did not receive a request")
    }

    fn finish(self) {
        self.thread
            .join()
            .expect("mock server thread panicked")
            .expect("mock server failed");
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<HttpRequest> {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 4096];
        let bytes_read = stream.read(&mut chunk)?;
        if bytes_read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended before headers were complete",
            ));
        }
        bytes.extend_from_slice(&chunk[..bytes_read]);

        if let Some(position) = find_subslice(&bytes, b"\r\n\r\n") {
            break position + 4;
        }
    };

    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);

    while bytes.len() < header_end + content_length {
        let mut chunk = [0; 4096];
        let bytes_read = stream.read(&mut chunk)?;
        if bytes_read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended before body was complete",
            ));
        }
        bytes.extend_from_slice(&chunk[..bytes_read]);
    }

    let request_line = headers.lines().next().unwrap_or_default();
    let mut request_line_parts = request_line.split_whitespace();
    let method = request_line_parts.next().unwrap_or_default().to_owned();
    let target = request_line_parts.next().unwrap_or_default().to_owned();
    let body =
        String::from_utf8_lossy(&bytes[header_end..header_end + content_length]).into_owned();

    Ok(HttpRequest {
        method,
        target,
        headers,
        body,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn run_lait(base_url: Option<&str>, api_key: Option<&str>, prompt: &str) -> Output {
    run_lait_with_request_options(base_url, api_key, prompt, false, None, None)
}

fn run_lait_with_options(
    base_url: Option<&str>,
    api_key: Option<&str>,
    prompt: &str,
    show_reasoning: bool,
) -> Output {
    run_lait_with_request_options(base_url, api_key, prompt, show_reasoning, None, None)
}

fn run_lait_with_json(base_url: Option<&str>, api_key: Option<&str>, prompt: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lait"));
    command.args(["--model", "test-model", "--json"]);
    command.env_remove("LLM_REASONING_EFFORT");
    if let Some(base_url) = base_url {
        command.args(["--base-url", base_url]);
    }
    if let Some(api_key) = api_key {
        command.args(["--api-key", api_key]);
    }
    command.arg(prompt);
    command.output().expect("failed to execute lait")
}

fn run_lait_with_json_schema(
    base_url: Option<&str>,
    api_key: Option<&str>,
    prompt: &str,
    schema_path: &Path,
    schema_name: Option<&str>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lait"));
    command.args(["--model", "test-model"]);
    command.env_remove("LLM_REASONING_EFFORT");
    if let Some(base_url) = base_url {
        command.args(["--base-url", base_url]);
    }
    if let Some(api_key) = api_key {
        command.args(["--api-key", api_key]);
    }
    command.arg("--json-schema").arg(schema_path);
    if let Some(schema_name) = schema_name {
        command.args(["--schema-name", schema_name]);
    }
    command.arg(prompt);
    command.output().expect("failed to execute lait")
}

fn run_lait_with_request_options(
    base_url: Option<&str>,
    api_key: Option<&str>,
    prompt: &str,
    show_reasoning: bool,
    cli_reasoning_effort: Option<&str>,
    env_reasoning_effort: Option<&str>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lait"));
    command.args(["--model", "test-model"]);
    command.env_remove("LLM_REASONING_EFFORT");
    if let Some(base_url) = base_url {
        command.args(["--base-url", base_url]);
    }
    if let Some(api_key) = api_key {
        command.args(["--api-key", api_key]);
    }
    if show_reasoning {
        command.arg("--show-reasoning");
    }
    if let Some(reasoning_effort) = cli_reasoning_effort {
        command.args(["--reasoning-effort", reasoning_effort]);
    }
    if let Some(reasoning_effort) = env_reasoning_effort {
        command.env("LLM_REASONING_EFFORT", reasoning_effort);
    }
    command.arg(prompt);
    command.output().expect("failed to execute lait")
}

fn without_json_whitespace(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect()
}

#[test]
fn sends_prompt_to_openai_compatible_chat_completions() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
    );
    let output = run_lait(Some(&server.base_url), Some("test-key"), "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/v1/chat/completions");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer test-key")
    );

    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""model":"test-model""#),
        "request body: {body}"
    );
    assert!(
        body.contains(r#""messages":[{"role":"user","content":"hello"}]"#),
        "request body: {body}"
    );
    assert!(body.contains(r#""stream":false"#), "request body: {body}");
    assert!(
        !body.contains(r#""reasoning_effort""#),
        "request body should omit reasoning_effort when unspecified: {body}"
    );
    assert!(
        !body.contains(r#""response_format""#),
        "request body should omit response_format when unspecified: {body}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response"
    );
}

#[test]
fn sends_strict_json_schema_response_format() {
    let schema = JsonSchemaFile::new(
        r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}"#,
    );
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"{\"answer\":\"mock response\"}"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_json_schema(
        Some(&server.base_url),
        None,
        "hello",
        &schema.path,
        Some("answer_schema"),
    );
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    let request_json: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be valid JSON");
    assert_eq!(
        request_json["response_format"],
        serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "answer_schema",
                "schema": {
                    "type": "object",
                    "properties": {"answer": {"type": "string"}},
                    "required": ["answer"],
                    "additionalProperties": false,
                },
                "strict": true,
            },
        })
    );
}

#[test]
fn reports_invalid_json_schema_file_with_path_context() {
    let schema = JsonSchemaFile::new("{not valid JSON");
    let output = run_lait_with_json_schema(None, None, "hello", &schema.path, None);

    assert!(
        !output.status.success(),
        "lait unexpectedly succeeded: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to parse JSON schema file"));
    assert!(stderr.contains(schema.path.to_string_lossy().as_ref()));
}

#[test]
fn reports_missing_json_schema_file_with_path_context() {
    let path = std::env::temp_dir().join(format!(
        "lait-missing-schema-{}-{}.json",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos()
    ));
    assert!(
        !path.exists(),
        "test schema path unexpectedly exists: {path:?}"
    );

    let output = run_lait_with_json_schema(None, None, "hello", &path, None);

    assert!(
        !output.status.success(),
        "lait unexpectedly succeeded: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to read JSON schema file"));
    assert!(stderr.contains(path.to_string_lossy().as_ref()));
}

#[test]
fn rejects_schema_name_without_json_schema_file() {
    let output = Command::new(env!("CARGO_BIN_EXE_lait"))
        .args([
            "--model",
            "test-model",
            "--schema-name",
            "custom_schema",
            "hello",
        ])
        .output()
        .expect("failed to execute lait");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("json-schema"));
}

#[test]
fn cli_reasoning_effort_overrides_environment() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_request_options(
        Some(&server.base_url),
        None,
        "hello",
        false,
        Some("high"),
        Some("none"),
    );
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""reasoning_effort":"high"#),
        "request body: {body}"
    );
}

#[test]
fn sends_none_reasoning_effort_when_explicitly_requested() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_request_options(
        Some(&server.base_url),
        None,
        "hello",
        false,
        Some("none"),
        None,
    );
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""reasoning_effort":"none"#),
        "request body: {body}"
    );
}

#[test]
fn sends_reasoning_effort_from_environment() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_request_options(
        Some(&server.base_url),
        None,
        "hello",
        false,
        None,
        Some("minimal"),
    );
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    let body = without_json_whitespace(&request.body);
    assert!(
        body.contains(r#""reasoning_effort":"minimal"#),
        "request body: {body}"
    );
}

#[test]
fn hides_reasoning_without_show_reasoning_option() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response","reasoning":"internal reasoning"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait(Some(&server.base_url), None, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "mock response\n");
}

#[test]
fn shows_reasoning_with_show_reasoning_option() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response","reasoning":"internal reasoning"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_options(Some(&server.base_url), None, "hello", true);
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Reasoning:\ninternal reasoning\n\nmock response\n"
    );
}

#[test]
fn shows_legacy_reasoning_content_with_show_reasoning_option() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response","reasoning_content":"internal reasoning"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_options(Some(&server.base_url), None, "hello", true);
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Reasoning:\ninternal reasoning\n\nmock response\n"
    );
}

#[test]
fn shows_only_final_content_when_reasoning_content_is_missing() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_options(Some(&server.base_url), None, "hello", true);
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "mock response\n");
}

#[test]
fn emits_json_with_null_reasoning_when_reasoning_is_missing() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock \"response\"\nsecond line"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_json(Some(&server.base_url), None, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--json output should be valid JSON");
    assert_eq!(
        json,
        serde_json::json!({
            "content": "mock \"response\"\nsecond line",
            "reasoning": null,
        })
    );
}

#[test]
fn emits_json_with_current_reasoning_in_preference_to_legacy_reasoning_content() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response","reasoning":"current reasoning","reasoning_content":"legacy reasoning"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_json(Some(&server.base_url), None, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--json output should be valid JSON");
    assert_eq!(
        json,
        serde_json::json!({
            "content": "mock response",
            "reasoning": "current reasoning",
        })
    );
}

#[test]
fn emits_json_with_legacy_reasoning_content_when_current_reasoning_is_blank() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response","reasoning":"  ","reasoning_content":"legacy reasoning"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait_with_json(Some(&server.base_url), None, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--json output should be valid JSON");
    assert_eq!(
        json,
        serde_json::json!({
            "content": "mock response",
            "reasoning": "legacy reasoning",
        })
    );
}

#[test]
fn reports_openai_api_errors() {
    let server = MockServer::start(
        "500 Internal Server Error",
        r#"{"error":{"message":"mock failure","type":"server_error"}}"#,
    );
    let output = run_lait(Some(&server.base_url), None, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(
        !output.status.success(),
        "lait unexpectedly succeeded: {:?}",
        output
    );
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/v1/chat/completions");
    assert!(
        !output.stderr.is_empty(),
        "API errors should be reported on stderr"
    );
}

#[test]
fn requires_model_option() {
    let output = Command::new(env!("CARGO_BIN_EXE_lait"))
        .env_remove("LLM_MODEL")
        .env_remove("OPENAI_BASE_URL")
        .env_remove("OPENAI_API_KEY")
        .env_remove("LLM_REASONING_EFFORT")
        .args(["hello"])
        .output()
        .expect("failed to execute lait");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("model"));
}
