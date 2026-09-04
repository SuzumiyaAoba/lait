#![allow(dead_code)]

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MAX_TEMP_PATH_ATTEMPTS: usize = 100;
static TEMP_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A minimal, valid 1x1 PNG (the PNG signature plus arbitrary trailing bytes
/// — lait only sniffs the leading magic bytes, it never decodes the image).
pub(crate) const MINIMAL_PNG_BYTES: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x01, 0x02,
];

#[derive(Debug)]
pub(crate) struct HttpRequest {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) headers: String,
    pub(crate) body: String,
}

pub(crate) struct MockServer {
    pub(crate) base_url: String,
    requests: Receiver<HttpRequest>,
    thread: JoinHandle<io::Result<()>>,
}

pub(crate) struct JsonSchemaFile {
    pub(crate) path: PathBuf,
}

pub(crate) struct WorkflowFile {
    pub(crate) path: PathBuf,
}

pub(crate) struct AgentMarkdownFile {
    pub(crate) path: PathBuf,
}

pub(crate) struct ConfigDirectory {
    path: PathBuf,
}

/// A temporary `$XDG_CONFIG_HOME` holding a global `lait/config.yml`, for
/// testing the global config file (see `config::global_config_path`).
/// Distinct from `ConfigDirectory`, which writes a project-local
/// `lait.config.yml` instead.
pub(crate) struct GlobalConfigDirectory {
    xdg_config_home: PathBuf,
}

impl JsonSchemaFile {
    pub(crate) fn new(contents: &str) -> Self {
        let mut path = None;
        for _ in 0..MAX_TEMP_PATH_ATTEMPTS {
            let candidate = next_temp_path("lait-test-schema", ".json");
            let mut file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("failed to write test JSON schema: {error}"),
            };
            file.write_all(contents.as_bytes())
                .expect("failed to write test JSON schema");
            path = Some(candidate);
            break;
        }
        let path = path.unwrap_or_else(|| {
            panic!(
                "failed to create a unique test JSON schema path after {MAX_TEMP_PATH_ATTEMPTS} attempts"
            )
        });
        Self { path }
    }
}

impl Drop for JsonSchemaFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl WorkflowFile {
    pub(crate) fn new(contents: &str) -> Self {
        let mut path = None;
        for _ in 0..MAX_TEMP_PATH_ATTEMPTS {
            let candidate = next_temp_path("lait-test-workflow", ".yml");
            let mut file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("failed to write test workflow file: {error}"),
            };
            file.write_all(contents.as_bytes())
                .expect("failed to write test workflow file");
            path = Some(candidate);
            break;
        }
        let path = path.unwrap_or_else(|| {
            panic!(
                "failed to create a unique test workflow path after {MAX_TEMP_PATH_ATTEMPTS} attempts"
            )
        });
        Self { path }
    }
}

impl Drop for WorkflowFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl AgentMarkdownFile {
    pub(crate) fn new(contents: &str) -> Self {
        let mut path = None;
        for _ in 0..MAX_TEMP_PATH_ATTEMPTS {
            let candidate = next_temp_path("lait-test-agent", ".md");
            let mut file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("failed to write test agent file: {error}"),
            };
            file.write_all(contents.as_bytes())
                .expect("failed to write test agent file");
            path = Some(candidate);
            break;
        }
        let path = path.unwrap_or_else(|| {
            panic!(
                "failed to create a unique test agent path after {MAX_TEMP_PATH_ATTEMPTS} attempts"
            )
        });
        Self { path }
    }
}

impl Drop for AgentMarkdownFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl ConfigDirectory {
    pub(crate) fn empty() -> Self {
        let mut path = None;
        for _ in 0..MAX_TEMP_PATH_ATTEMPTS {
            let candidate = next_temp_path("lait-test-config", "");
            match fs::create_dir(&candidate) {
                Ok(()) => {
                    path = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("failed to create test config directory: {error}"),
            }
        }
        let path = path.unwrap_or_else(|| {
            panic!(
                "failed to create a unique test config directory after {MAX_TEMP_PATH_ATTEMPTS} attempts"
            )
        });
        Self { path }
    }

    pub(crate) fn new(contents: &str) -> Self {
        let directory = Self::empty();
        fs::write(directory.config_path(), contents).expect("failed to write test YAML config");
        directory
    }

    pub(crate) fn config_path(&self) -> PathBuf {
        self.path.join("lait.config.yml")
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) fn next_temp_path(prefix: &str, suffix: &str) -> PathBuf {
    let unique_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_nanos();
    let counter = TEMP_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{unique_id}-{counter}{suffix}",
        std::process::id()
    ))
}

impl Drop for ConfigDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl GlobalConfigDirectory {
    /// Writes `contents` to a fresh temporary directory's `lait/config.yml`,
    /// the layout `config::global_config_path` expects under `$XDG_CONFIG_HOME`.
    pub(crate) fn new(contents: &str) -> Self {
        let xdg_config_home = next_temp_path("lait-test-global-config", "");
        fs::create_dir_all(xdg_config_home.join("lait"))
            .expect("failed to create test global config directory");
        fs::write(xdg_config_home.join("lait").join("config.yml"), contents)
            .expect("failed to write test global config file");
        Self { xdg_config_home }
    }

    /// The value to set `XDG_CONFIG_HOME` to for a child process to pick up
    /// this directory's `lait/config.yml` as its global config.
    pub(crate) fn xdg_config_home(&self) -> &Path {
        &self.xdg_config_home
    }

    /// The directory the global `config.yml` itself lives in
    /// (`$XDG_CONFIG_HOME/lait`) — where a registry entry (`workflows:`/
    /// `agents:`/`skills:`) defined in it resolves relative paths against.
    pub(crate) fn config_dir(&self) -> PathBuf {
        self.xdg_config_home.join("lait")
    }
}

impl Drop for GlobalConfigDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.xdg_config_home);
    }
}

pub(crate) fn test_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lait"));
    for variable in [
        "LLM_MODEL",
        "OPENAI_BASE_URL",
        "OPENAI_API_KEY",
        "LLM_REASONING_EFFORT",
    ] {
        command.env_remove(variable);
    }
    // Isolates every test from the machine's real global config
    // (`$XDG_CONFIG_HOME/lait/config.yml`, or `~/.config/lait/config.yml`
    // when `XDG_CONFIG_HOME` is unset — see `config::global_config_path`).
    // Without this, a developer's own global config (models/mcp_servers/
    // default: entries) would silently merge into every test run. The path
    // need not exist: `global_config_path`'s `.is_file()` check simply
    // returns false, so this needs no filesystem setup and is race-free
    // under parallel test execution. Tests exercising the global config
    // itself (`tests/config.rs`) override this with `GlobalConfigDirectory`
    // and `.env("XDG_CONFIG_HOME", ..)`.
    command.env(
        "XDG_CONFIG_HOME",
        std::env::temp_dir().join("lait-test-no-global-config"),
    );
    command
}

impl MockServer {
    pub(crate) fn start(status: &str, response_body: &str) -> Self {
        Self::start_sequence(&[(status, response_body)])
    }

    /// Starts a mock server that accepts `responses.len()` connections in
    /// order, replying to the Nth one with `responses[n]` (a `(status,
    /// body)` pair) and recording every request it received. Used to test
    /// retry: e.g. `&[("500 Internal Server Error", "..."), ("200 OK", "...")]`
    /// simulates one transient failure followed by a success.
    ///
    /// Tolerates *more* connections than `responses.len()` — an HTTP-level
    /// retry (async-openai's built-in `OpenAIRetryLayer`, see the doc
    /// comment on `llm::client`) attempting again after the last configured
    /// response, most notably — by repeating the last response for each
    /// one, on a background thread `finish()` never joins (so a test that
    /// never sends an extra connection isn't slowed down waiting for one
    /// that never arrives). A test that wants to assert on the exact number
    /// of connections still can, by calling `receive_request()` exactly
    /// `responses.len()` times: extras are still recorded on the same
    /// channel, just never required.
    pub(crate) fn start_sequence(responses: &[(&str, &str)]) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind mock server");
        let address = listener
            .local_addr()
            .expect("failed to get mock server address");
        let (request_sender, requests) = mpsc::channel();
        let responses: Vec<(String, String)> = responses
            .iter()
            .map(|(status, body)| ((*status).to_owned(), (*body).to_owned()))
            .collect();
        let last_response = responses.last().cloned();

        let thread = thread::spawn(move || {
            for (status, response_body) in &responses {
                let (mut stream, _) = listener.accept()?;
                let request = read_request(&mut stream)?;
                request_sender
                    .send(request)
                    .map_err(|_| io::Error::other("test receiver was dropped"))?;
                write_response(&mut stream, status, response_body)?;
            }

            if let Some((status, response_body)) = last_response {
                thread::spawn(move || {
                    while let Ok((mut stream, _)) = listener.accept() {
                        let Ok(request) = read_request(&mut stream) else {
                            continue;
                        };
                        let _ = request_sender.send(request);
                        let _ = write_response(&mut stream, &status, &response_body);
                    }
                });
            }
            Ok(())
        });

        Self {
            base_url: format!("http://{address}/v1"),
            requests,
            thread,
        }
    }

    /// Starts a mock server that accepts one connection, reads its request,
    /// then waits `delay` before writing the response. Used to test
    /// `timeout`: a step whose `timeout` is shorter than `delay` should fail
    /// (and its request future should be dropped) before the response is
    /// ever written.
    pub(crate) fn start_delayed(delay: Duration, status: &str, response_body: &str) -> Self {
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

            thread::sleep(delay);

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

    /// Starts a mock server that accepts one connection and replies with a
    /// `text/event-stream` body built from `events` (each written as its own
    /// `data: <event>\n\n` frame, in order), followed by the terminating
    /// `data: [DONE]\n\n` frame every OpenAI-compatible SSE stream ends with.
    /// Used to test `--stream`: the whole body is written up front (this is
    /// a canned response, not truly incremental), which is enough since the
    /// client parses events out of the byte stream as they're read either way.
    pub(crate) fn start_stream(events: &[&str]) -> Self {
        Self::start_stream_sequence(&[events])
    }

    /// Like `start_stream`, but accepts `rounds.len()` connections in order,
    /// replying to the Nth one with an SSE body built from `rounds[n]` (the
    /// same per-event framing `start_stream` uses) — for a streamed
    /// `--stream`/`--mcp`/`--subagent` tool loop, where each round is its
    /// own HTTP request/response over the same base URL. Unlike
    /// `start_sequence`, extra connections beyond `rounds.len()` are not
    /// tolerated (a streamed request is never retried by async-openai's own
    /// retry layer once bytes start arriving, so a test using this doesn't
    /// need that headroom).
    pub(crate) fn start_stream_sequence(rounds: &[&[&str]]) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind mock server");
        let address = listener
            .local_addr()
            .expect("failed to get mock server address");
        let (request_sender, requests) = mpsc::channel();
        let bodies: Vec<String> = rounds
            .iter()
            .map(|events| {
                let mut body = String::new();
                for event in *events {
                    body.push_str("data: ");
                    body.push_str(event);
                    body.push_str("\n\n");
                }
                body.push_str("data: [DONE]\n\n");
                body
            })
            .collect();

        let thread = thread::spawn(move || {
            for body in &bodies {
                let (mut stream, _) = listener.accept()?;
                let request = read_request(&mut stream)?;
                request_sender
                    .send(request)
                    .map_err(|_| io::Error::other("test receiver was dropped"))?;

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                stream.write_all(response.as_bytes())?;
                stream.flush()?;
            }
            Ok(())
        });

        Self {
            base_url: format!("http://{address}/v1"),
            requests,
            thread,
        }
    }

    pub(crate) fn receive_request(&self) -> HttpRequest {
        self.requests
            .recv_timeout(Duration::from_secs(5))
            .expect("mock server did not receive a request")
    }

    /// Like `receive_request`, but returns `None` instead of panicking when
    /// no request arrives within `timeout` — for asserting a request was
    /// *not* retried without paying `receive_request`'s full 5-second
    /// timeout on every such assertion.
    pub(crate) fn try_receive_request(&self, timeout: Duration) -> Option<HttpRequest> {
        self.requests.recv_timeout(timeout).ok()
    }

    pub(crate) fn finish(self) {
        self.thread
            .join()
            .expect("mock server thread panicked")
            .expect("mock server failed");
    }
}

/// Writes a canned `status`/`response_body` HTTP response to `stream`,
/// shared by every `MockServer` constructor that replies with a fixed JSON
/// body.
fn write_response(stream: &mut TcpStream, status: &str, response_body: &str) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
        response_body.len(),
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

pub(crate) fn read_request(stream: &mut TcpStream) -> io::Result<HttpRequest> {
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

/// A hand-rolled streamable-HTTP MCP server for integration tests: routes on
/// the JSON-RPC `method` field (something `MockServer` can't do, since it
/// just replays canned bodies in connection order) and answers `initialize`
/// / `notifications/initialized` / `tools/list` / `tools/call` — the four
/// requests one tool-call round trip makes. Exposes a single tool, `echo`,
/// that always returns a fixed string regardless of its arguments. Shared by
/// every integration test binary that needs a minimal MCP tool-call
/// round trip (`tests/mcp.rs`, `tests/streaming.rs`).
pub(crate) fn start_mock_mcp_server() -> (String, JoinHandle<()>) {
    start_mock_mcp_server_with_list_cursor(None)
}

/// Starts the same test server but includes `nextCursor` in every
/// `tools/list` response when `next_cursor` is `Some`. This exercises a
/// client's pagination safety checks without involving an LLM request: a
/// repeated cursor must be rejected while listing tools.
pub(crate) fn start_mock_mcp_server_with_list_cursor(
    next_cursor: Option<&'static str>,
) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind mock MCP server");
    let addr = listener
        .local_addr()
        .expect("failed to read mock MCP server address");
    let handle = thread::spawn(move || {
        let mut expected_cursor = None;
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().expect("failed to accept connection");
            let request = read_request(&mut stream).expect("failed to read MCP request");
            let request: serde_json::Value =
                serde_json::from_str(&request.body).expect("mock MCP server got non-JSON body");
            let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let id = request.get("id").cloned();
            if method == "tools/list"
                && let Some(expected_cursor) = expected_cursor
            {
                let actual_cursor = request
                    .get("params")
                    .and_then(|params| params.get("cursor"))
                    .and_then(serde_json::Value::as_str);
                assert_eq!(actual_cursor, Some(expected_cursor));
            }

            let (status, response_body) = match method {
                "initialize" => (
                    "200 OK",
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "serverInfo": {"name": "mock-mcp", "version": "0.0.1"}
                        }
                    })
                    .to_string(),
                ),
                "notifications/initialized" => ("202 Accepted", String::new()),
                "tools/list" => {
                    let mut body = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": "echo",
                                "description": "echoes the input back",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {"text": {"type": "string"}}
                                }
                            }]
                        }
                    });
                    if let Some(next_cursor) = next_cursor {
                        body["result"]["nextCursor"] = serde_json::json!(next_cursor);
                        expected_cursor = Some(next_cursor);
                    }
                    ("200 OK", body.to_string())
                }
                "tools/call" => (
                    "200 OK",
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": "42"}]
                        }
                    })
                    .to_string(),
                ),
                other => panic!("mock MCP server received an unexpected method '{other}'"),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len(),
            );
            stream
                .write_all(response.as_bytes())
                .expect("failed to write mock response");
            stream.flush().expect("failed to flush mock response");
        }
    });
    (format!("http://{addr}/mcp"), handle)
}

/// Builds a `lait` single-shot chat invocation for tests: `--model
/// test-model` and a cleared `LLM_REASONING_EFFORT` are the fixed baseline
/// every caller wants, and `.run()`/`.spawn_with_stdin(..)` execute it.
/// Replaces what used to be seven near-duplicate `run_lait_with_*`
/// functions (one per flag combination a test needed), each repeating the
/// same base-url/api-key/prompt plumbing around one or two extra flags.
pub(crate) struct LaitCommand {
    command: Command,
    prompt: Option<String>,
}

impl LaitCommand {
    pub(crate) fn new() -> Self {
        let mut command = test_command();
        command.args(["--model", "test-model"]);
        command.env_remove("LLM_REASONING_EFFORT");
        Self {
            command,
            prompt: None,
        }
    }

    pub(crate) fn base_url(mut self, base_url: Option<&str>) -> Self {
        if let Some(base_url) = base_url {
            self.command.args(["--base-url", base_url]);
        }
        self
    }

    pub(crate) fn api_key(mut self, api_key: Option<&str>) -> Self {
        if let Some(api_key) = api_key {
            self.command.args(["--api-key", api_key]);
        }
        self
    }

    pub(crate) fn arg(mut self, arg: impl AsRef<std::ffi::OsStr>) -> Self {
        self.command.arg(arg);
        self
    }

    /// Only adds the flag when `value` is `Some` — the common shape for an
    /// optional CLI override (`--show-reasoning`/`--reasoning-effort`/
    /// `--temperature`/... in these tests).
    pub(crate) fn opt_arg(mut self, flag: &str, value: Option<&str>) -> Self {
        if let Some(value) = value {
            self.command.args([flag, value]);
        }
        self
    }

    pub(crate) fn flag_if(mut self, flag: &str, enabled: bool) -> Self {
        if enabled {
            self.command.arg(flag);
        }
        self
    }

    pub(crate) fn env(mut self, key: &str, value: &str) -> Self {
        self.command.env(key, value);
        self
    }

    /// Sets PROMPT, appended last so flag order in the tests reads naturally
    /// without callers needing to remember PROMPT must come after flags.
    pub(crate) fn prompt(mut self, prompt: &str) -> Self {
        self.prompt = Some(prompt.to_owned());
        self
    }

    /// Like `.prompt(..)`, but leaves PROMPT unset for `None` — for the
    /// piped-stdin tests, where "no PROMPT argument" is itself the case
    /// under test.
    pub(crate) fn opt_prompt(self, prompt: Option<&str>) -> Self {
        match prompt {
            Some(prompt) => self.prompt(prompt),
            None => self,
        }
    }

    pub(crate) fn run(mut self) -> Output {
        if let Some(prompt) = &self.prompt {
            self.command.arg(prompt);
        }
        self.command.output().expect("failed to execute lait")
    }

    /// Like `.run()`, but pipes `stdin_text` into the child's stdin instead
    /// of inheriting the test process's — for the piped-input rules (stdin
    /// as the whole prompt, or appended to a PROMPT argument as context).
    pub(crate) fn spawn_with_stdin(mut self, stdin_text: &str) -> Output {
        if let Some(prompt) = &self.prompt {
            self.command.arg(prompt);
        }
        self.command.stdin(std::process::Stdio::piped());
        self.command.stdout(std::process::Stdio::piped());
        self.command.stderr(std::process::Stdio::piped());
        let mut child = self.command.spawn().expect("failed to spawn lait");
        child
            .stdin
            .take()
            .expect("child stdin should be piped")
            .write_all(stdin_text.as_bytes())
            .expect("failed to write to lait's stdin");
        child
            .wait_with_output()
            .expect("failed to wait for lait to finish")
    }
}

pub(crate) fn run_lait(base_url: Option<&str>, api_key: Option<&str>, prompt: &str) -> Output {
    LaitCommand::new()
        .base_url(base_url)
        .api_key(api_key)
        .prompt(prompt)
        .run()
}

pub(crate) fn run_lait_workflow(workflow_path: &Path, prompt: &str) -> Output {
    test_command()
        .arg("run")
        .arg(workflow_path)
        .arg(prompt)
        .output()
        .expect("failed to execute lait run")
}

pub(crate) fn run_lait_agent(agent_path: &Path, input: &str) -> Output {
    test_command()
        .arg("agent")
        .arg("run")
        .arg(agent_path)
        .arg(input)
        .output()
        .expect("failed to execute lait agent run")
}

pub(crate) fn run_lait_lint(files: &[&Path]) -> Output {
    let mut command = test_command();
    command.arg("lint");
    for file in files {
        command.arg(file);
    }
    command.output().expect("failed to execute lait lint")
}

pub(crate) fn without_json_whitespace(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect()
}
