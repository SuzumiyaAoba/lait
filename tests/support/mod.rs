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

        let thread = thread::spawn(move || {
            for (status, response_body) in responses {
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
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind mock server");
        let address = listener
            .local_addr()
            .expect("failed to get mock server address");
        let (request_sender, requests) = mpsc::channel();
        let mut body = String::new();
        for event in events {
            body.push_str("data: ");
            body.push_str(event);
            body.push_str("\n\n");
        }
        body.push_str("data: [DONE]\n\n");

        let thread = thread::spawn(move || {
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

    pub(crate) fn finish(self) {
        self.thread
            .join()
            .expect("mock server thread panicked")
            .expect("mock server failed");
    }
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

pub(crate) fn run_lait(base_url: Option<&str>, api_key: Option<&str>, prompt: &str) -> Output {
    run_lait_with_request_options(base_url, api_key, prompt, false, None, None)
}

pub(crate) fn run_lait_with_options(
    base_url: Option<&str>,
    api_key: Option<&str>,
    prompt: &str,
    show_reasoning: bool,
) -> Output {
    run_lait_with_request_options(base_url, api_key, prompt, show_reasoning, None, None)
}

pub(crate) fn run_lait_with_json(
    base_url: Option<&str>,
    api_key: Option<&str>,
    prompt: &str,
) -> Output {
    let mut command = test_command();
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

pub(crate) fn run_lait_with_json_schema(
    base_url: Option<&str>,
    api_key: Option<&str>,
    prompt: &str,
    schema_path: &Path,
    schema_name: Option<&str>,
) -> Output {
    let mut command = test_command();
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

pub(crate) fn run_lait_with_request_options(
    base_url: Option<&str>,
    api_key: Option<&str>,
    prompt: &str,
    show_reasoning: bool,
    cli_reasoning_effort: Option<&str>,
    env_reasoning_effort: Option<&str>,
) -> Output {
    let mut command = test_command();
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

pub(crate) fn run_lait_with_sampling_options(
    base_url: Option<&str>,
    api_key: Option<&str>,
    prompt: &str,
    temperature: Option<&str>,
    top_p: Option<&str>,
    max_tokens: Option<&str>,
) -> Output {
    let mut command = test_command();
    command.args(["--model", "test-model"]);
    command.env_remove("LLM_REASONING_EFFORT");
    if let Some(base_url) = base_url {
        command.args(["--base-url", base_url]);
    }
    if let Some(api_key) = api_key {
        command.args(["--api-key", api_key]);
    }
    if let Some(temperature) = temperature {
        command.args(["--temperature", temperature]);
    }
    if let Some(top_p) = top_p {
        command.args(["--top-p", top_p]);
    }
    if let Some(max_tokens) = max_tokens {
        command.args(["--max-tokens", max_tokens]);
    }
    command.arg(prompt);
    command.output().expect("failed to execute lait")
}

pub(crate) fn run_lait_with_stream(
    base_url: Option<&str>,
    api_key: Option<&str>,
    prompt: &str,
    show_reasoning: bool,
) -> Output {
    let mut command = test_command();
    command.args(["--model", "test-model", "--stream"]);
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
    command.arg(prompt);
    command.output().expect("failed to execute lait")
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
