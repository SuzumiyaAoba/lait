mod support;

use std::io::Write;
use std::net::TcpListener;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use support::{HttpRequest, read_request, test_command};

/// A mock server that accepts one connection per entry in `contents`, in
/// order, replying to each with a single-chunk SSE stream carrying that
/// text — the shape `lait chat`'s default (streaming) turn expects. Unlike
/// `support::MockServer::start_stream`, this accepts more than one
/// connection, one per REPL turn the test drives.
struct MultiTurnStreamServer {
    base_url: String,
    requests: mpsc::Receiver<HttpRequest>,
    thread: JoinHandle<std::io::Result<()>>,
}

impl MultiTurnStreamServer {
    fn start(contents: &[&str]) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind mock server");
        let address = listener
            .local_addr()
            .expect("failed to get mock server address");
        let (sender, receiver) = mpsc::channel();
        let contents: Vec<String> = contents.iter().map(|text| (*text).to_owned()).collect();

        let thread = thread::spawn(move || {
            for content in contents {
                let (mut stream, _) = listener.accept()?;
                let request = read_request(&mut stream)?;
                sender
                    .send(request)
                    .map_err(|_| std::io::Error::other("test receiver was dropped"))?;

                let chunk = format!(
                    r#"{{"id":"x","object":"chat.completion.chunk","created":0,"model":"test-model","choices":[{{"index":0,"delta":{{"content":{content:?}}}}}]}}"#
                );
                let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
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
            requests: receiver,
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

#[test]
fn repl_streams_each_turn_and_sends_prior_turns_as_history() {
    let server = MultiTurnStreamServer::start(&["hi there", "doing well"]);
    let mut command = test_command();
    command.args([
        "chat",
        "--model",
        "test-model",
        "--base-url",
        &server.base_url,
    ]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().expect("failed to spawn lait chat");
    child
        .stdin
        .take()
        .expect("child stdin should be piped")
        .write_all(b"hello\nhow are you?\n/exit\n")
        .expect("failed to write to lait chat's stdin");

    let first_request = server.receive_request();
    let second_request = server.receive_request();
    let output = child
        .wait_with_output()
        .expect("failed to wait for lait chat");
    server.finish();

    assert!(output.status.success(), "lait chat failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hi there"));
    assert!(stdout.contains("doing well"));

    let first_body: serde_json::Value = serde_json::from_str(&first_request.body).unwrap();
    assert_eq!(first_body["messages"].as_array().unwrap().len(), 1);

    let second_body: serde_json::Value = serde_json::from_str(&second_request.body).unwrap();
    let messages = second_body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "hello");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "hi there");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], "how are you?");
}

#[test]
fn repl_exits_cleanly_on_end_of_input_without_an_explicit_exit_command() {
    let server = MultiTurnStreamServer::start(&["hi there"]);
    let mut command = test_command();
    command.args([
        "chat",
        "--model",
        "test-model",
        "--base-url",
        &server.base_url,
    ]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().expect("failed to spawn lait chat");
    child
        .stdin
        .take()
        .expect("child stdin should be piped")
        .write_all(b"hello\n")
        .expect("failed to write to lait chat's stdin");
    // Dropping `stdin` above already closed the write end; `wait_with_output`
    // reads until EOF, which is what ends the REPL loop here.

    server.receive_request();
    let output = child
        .wait_with_output()
        .expect("failed to wait for lait chat");
    server.finish();

    assert!(output.status.success(), "lait chat failed: {output:?}");
}

#[test]
fn repl_model_command_switches_the_model_for_later_turns() {
    let server = MultiTurnStreamServer::start(&["first reply", "second reply"]);
    let mut command = test_command();
    command.args([
        "chat",
        "--model",
        "test-model",
        "--base-url",
        &server.base_url,
    ]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().expect("failed to spawn lait chat");
    child
        .stdin
        .take()
        .expect("child stdin should be piped")
        .write_all(b"hello\n/model other-model\nhow are you?\n/exit\n")
        .expect("failed to write to lait chat's stdin");

    let first_request = server.receive_request();
    let second_request = server.receive_request();
    let output = child
        .wait_with_output()
        .expect("failed to wait for lait chat");
    server.finish();

    assert!(output.status.success(), "lait chat failed: {output:?}");

    let first_body: serde_json::Value = serde_json::from_str(&first_request.body).unwrap();
    assert_eq!(first_body["model"], "test-model");

    let second_body: serde_json::Value = serde_json::from_str(&second_request.body).unwrap();
    assert_eq!(second_body["model"], "other-model");
}

#[test]
fn repl_meta_commands_never_reach_the_network() {
    let mut command = test_command();
    command.args(["chat", "--model", "test-model"]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().expect("failed to spawn lait chat");
    child
        .stdin
        .take()
        .expect("child stdin should be piped")
        .write_all(b"/clear\n/nope\n/exit\n")
        .expect("failed to write to lait chat's stdin");
    let output = child
        .wait_with_output()
        .expect("failed to wait for lait chat");

    assert!(output.status.success(), "lait chat failed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("history cleared"));
    assert!(stderr.contains("unknown command: /nope"));
}

/// Runs `lait chat` against `server`, feeding it `stdin`, and returns the
/// finished process's output.
fn run_repl(server: &MultiTurnStreamServer, stdin: &[u8]) -> std::process::Child {
    let mut command = test_command();
    command.args([
        "chat",
        "--model",
        "test-model",
        "--base-url",
        &server.base_url,
    ]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().expect("failed to spawn lait chat");
    child
        .stdin
        .take()
        .expect("child stdin should be piped")
        .write_all(stdin)
        .expect("failed to write to lait chat's stdin");
    child
}

fn messages_of(request: &HttpRequest) -> Vec<serde_json::Value> {
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    body["messages"].as_array().unwrap().clone()
}

#[test]
fn repl_retry_resends_the_last_message_in_place_of_its_reply() {
    let server = MultiTurnStreamServer::start(&["first try", "second try", "after"]);
    let child = run_repl(&server, b"hello\n/retry\nnext\n/exit\n");

    let first = server.receive_request();
    let retried = server.receive_request();
    let after = server.receive_request();
    let output = child
        .wait_with_output()
        .expect("failed to wait for lait chat");
    server.finish();

    assert!(output.status.success(), "lait chat failed: {output:?}");
    assert_eq!(messages_of(&first), messages_of(&retried));
    let messages = messages_of(&after);
    assert_eq!(messages.len(), 3, "{messages:?}");
    assert_eq!(messages[1]["content"], "second try");
    assert_eq!(messages[2]["content"], "next");
}

#[test]
fn repl_undo_drops_the_last_exchange() {
    let server = MultiTurnStreamServer::start(&["one", "two"]);
    let child = run_repl(&server, b"first\n/undo\nsecond\n/exit\n");

    server.receive_request();
    let second = server.receive_request();
    let output = child
        .wait_with_output()
        .expect("failed to wait for lait chat");
    server.finish();

    assert!(output.status.success(), "lait chat failed: {output:?}");
    let messages = messages_of(&second);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["content"], "second");
}

#[test]
fn repl_sends_a_triple_quoted_block_as_one_message() {
    let server = MultiTurnStreamServer::start(&["ok"]);
    let child = run_repl(
        &server,
        b"\"\"\"\nline one\n/not a command\n\"\"\"\n/exit\n",
    );

    let request = server.receive_request();
    let output = child
        .wait_with_output()
        .expect("failed to wait for lait chat");
    server.finish();

    assert!(output.status.success(), "lait chat failed: {output:?}");
    let messages = messages_of(&request);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["content"], "line one\n/not a command");
}

#[test]
fn repl_help_and_nothing_to_undo_or_retry_are_reported_locally() {
    let server = MultiTurnStreamServer::start(&[]);
    let child = run_repl(&server, b"/help\n/undo\n/retry\n/usage\n/exit\n");
    let output = child
        .wait_with_output()
        .expect("failed to wait for lait chat");
    server.finish();

    assert!(output.status.success(), "lait chat failed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("/retry"), "{stderr}");
    assert!(stderr.contains("nothing to undo"), "{stderr}");
    assert!(stderr.contains("nothing to retry"), "{stderr}");
    assert!(stderr.contains("usage:"), "{stderr}");
}
