use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
    run_lait_with_options(base_url, api_key, prompt, false)
}

fn run_lait_with_options(
    base_url: Option<&str>,
    api_key: Option<&str>,
    prompt: &str,
    show_reasoning: bool,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lait"));
    command.args(["--model", "test-model"]);
    if let Some(base_url) = base_url {
        command.args(["--base-url", base_url]);
    }
    if let Some(api_key) = api_key {
        command.args(["--api-key", api_key]);
    }
    if show_reasoning {
        command.arg("--show-reasoning");
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
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "mock response"
    );
}

#[test]
fn hides_reasoning_content_without_show_reasoning_option() {
    let server = MockServer::start(
        "200 OK",
        r#"{"id":"chatcmpl-test","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"mock response","reasoning_content":"internal reasoning"},"finish_reason":"stop"}]}"#,
    );
    let output = run_lait(Some(&server.base_url), None, "hello");
    let request = server.receive_request();
    server.finish();

    assert!(output.status.success(), "lait failed: {:?}", output);
    assert_eq!(request.target, "/v1/chat/completions");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "mock response\n");
}

#[test]
fn shows_reasoning_content_with_show_reasoning_option() {
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
        .args(["hello"])
        .output()
        .expect("failed to execute lait");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("model"));
}
