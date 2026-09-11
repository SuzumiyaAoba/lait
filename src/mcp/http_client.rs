//! The HTTP (Streamable HTTP) transport: a `reqwest`-backed
//! [`StreamableHttpClient`] with its own Content-Length and SSE-event-size
//! enforcement, since `rmcp`'s stock adapter parses JSON responses with
//! `Response::json()` (no byte limit) and does not cap chunked/SSE bodies
//! either. `connect` (in the parent module) is the only external caller: it
//! builds a [`LimitedHttpClient`] via [`LimitedHttpClient::new`] and hands it
//! to `serve_with_timeout` alongside the stdio transport in `mcp/stdio.rs`.

use std::{borrow::Cow, collections::HashMap, sync::Arc};

use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream::BoxStream};
use http::{HeaderName, HeaderValue, header::ACCEPT};
use rmcp::transport::{
    common::http_header::{
        EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_SESSION_ID, JSON_MIME_TYPE,
    },
    streamable_http_client::{
        AuthRequiredError, InsufficientScopeError, SseError, StreamableHttpClient,
        StreamableHttpError, StreamableHttpPostResponse,
    },
};
use sse_stream::{Sse, SseStream};
use tokio_util::sync::CancellationToken;

use super::MAX_HTTP_RESPONSE_BODY_BYTES;

/// Errors raised by the reqwest adapter that enforces the response-body
/// budget before handing bytes to rmcp's JSON/SSE parsers.
#[derive(Debug)]
pub(super) enum LimitedHttpClientError {
    Request(reqwest::Error),
    BodyTooLarge { limit: usize },
    Cancelled,
}

impl std::fmt::Display for LimitedHttpClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(error) => write!(f, "HTTP request failed: {error}"),
            Self::BodyTooLarge { limit } => {
                write!(f, "HTTP response body exceeds {limit} bytes")
            }
            Self::Cancelled => f.write_str("HTTP request was cancelled"),
        }
    }
}

impl std::error::Error for LimitedHttpClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Request(error) => Some(error),
            Self::BodyTooLarge { .. } | Self::Cancelled => None,
        }
    }
}

impl From<reqwest::Error> for LimitedHttpClientError {
    fn from(error: reqwest::Error) -> Self {
        Self::Request(error)
    }
}

/// A reqwest-backed rmcp client with a finite budget for every HTTP response.
/// The stock rmcp adapter parses JSON responses with `Response::json()`, which
/// has no byte limit. Keeping this small adapter here lets us reject a large
/// `Content-Length` before allocation and count chunked/SSE bodies as they
/// arrive.
#[derive(Clone)]
pub(super) struct LimitedHttpClient {
    client: reqwest::Client,
    max_body_bytes: usize,
    cancellation: CancellationToken,
}

impl LimitedHttpClient {
    pub(super) fn new(
        client: reqwest::Client,
        max_body_bytes: usize,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            client,
            max_body_bytes,
            cancellation,
        }
    }

    /// Reqwest does not know about the lifecycle of the rmcp worker.  A
    /// transport cancellation must therefore abort an in-flight request here
    /// rather than waiting for reqwest's (deliberately long) request timeout.
    /// Otherwise `McpConnection::wait_closed` cannot complete before a retry
    /// starts and a timed-out workflow can stall for minutes.
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, StreamableHttpError<LimitedHttpClientError>> {
        let cancellation = self.cancellation.clone();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(StreamableHttpError::Client(
                LimitedHttpClientError::Cancelled,
            )),
            response = request.send() => response
                .map_err(LimitedHttpClientError::Request)
                .map_err(StreamableHttpError::Client),
        }
    }

    fn check_content_length(
        &self,
        response: &reqwest::Response,
    ) -> Result<(), StreamableHttpError<LimitedHttpClientError>> {
        if response
            .content_length()
            .is_some_and(|length| length > self.max_body_bytes as u64)
        {
            return Err(StreamableHttpError::Client(
                LimitedHttpClientError::BodyTooLarge {
                    limit: self.max_body_bytes,
                },
            ));
        }
        Ok(())
    }

    fn apply_custom_headers(
        &self,
        mut request: reqwest::RequestBuilder,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<reqwest::RequestBuilder, StreamableHttpError<LimitedHttpClientError>> {
        for (name, value) in custom_headers {
            // These headers are owned by the transport. The protocol-version
            // header is the one intentional exception: rmcp injects it into
            // the map after initialization and expects it to pass through.
            let reserved = [
                "accept",
                HEADER_SESSION_ID,
                HEADER_LAST_EVENT_ID,
                "mcp-protocol-version",
            ];
            if reserved
                .iter()
                .any(|reserved| name.as_str().eq_ignore_ascii_case(reserved))
                && !name.as_str().eq_ignore_ascii_case("mcp-protocol-version")
            {
                return Err(StreamableHttpError::ReservedHeaderConflict(
                    name.to_string(),
                ));
            }
            request = request.header(name, value);
        }
        Ok(request)
    }

    fn limited_stream(
        response: reqwest::Response,
        max_body_bytes: usize,
        cancellation: CancellationToken,
    ) -> impl Stream<Item = Result<Bytes, LimitedHttpClientError>> + Send + 'static {
        let stream = response.bytes_stream();
        futures_util::stream::unfold(
            (stream, 0usize, false, cancellation),
            move |(mut stream, total, failed, cancellation)| async move {
                if failed {
                    return None;
                }
                if cancellation.is_cancelled() {
                    return Some((
                        Err(LimitedHttpClientError::Cancelled),
                        (stream, total, true, cancellation),
                    ));
                }
                let cancellation_wait = cancellation.clone();
                let next = tokio::select! {
                    biased;
                    _ = cancellation_wait.cancelled() => {
                        return Some((
                            Err(LimitedHttpClientError::Cancelled),
                            (stream, total, true, cancellation),
                        ));
                    }
                    chunk = stream.next() => chunk,
                };
                match next {
                    None => None,
                    Some(Err(error)) => Some((
                        Err(LimitedHttpClientError::Request(error)),
                        (stream, total, true, cancellation),
                    )),
                    Some(Ok(chunk)) => {
                        let Some(next_total) = total.checked_add(chunk.len()) else {
                            return Some((
                                Err(LimitedHttpClientError::BodyTooLarge {
                                    limit: max_body_bytes,
                                }),
                                (stream, total, true, cancellation),
                            ));
                        };
                        if next_total > max_body_bytes {
                            Some((
                                Err(LimitedHttpClientError::BodyTooLarge {
                                    limit: max_body_bytes,
                                }),
                                (stream, total, true, cancellation),
                            ))
                        } else {
                            Some((Ok(chunk), (stream, next_total, false, cancellation)))
                        }
                    }
                }
            },
        )
    }

    /// Drains a bounded response body, tracking the running size against
    /// `max_body_bytes` either way. `collect` chooses whether chunks are also
    /// buffered: [`read_body`](Self::read_body) needs the bytes back,
    /// [`drain_body`](Self::drain_body) only needs the body off the wire, so
    /// it passes `false` to avoid holding a response it is about to discard
    /// in memory.
    async fn read_body_bounded(
        &self,
        response: reqwest::Response,
        collect: bool,
    ) -> Result<Vec<u8>, StreamableHttpError<LimitedHttpClientError>> {
        self.check_content_length(&response)?;
        let capacity = if collect {
            (response.content_length().unwrap_or(0) as usize).min(self.max_body_bytes)
        } else {
            0
        };
        let mut body = Vec::with_capacity(capacity);
        let mut total = 0usize;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => {
                return Err(StreamableHttpError::Client(
                    LimitedHttpClientError::Cancelled,
                ));
            }
            chunk = stream.next() => chunk,
        } {
            let chunk = chunk
                .map_err(LimitedHttpClientError::Request)
                .map_err(StreamableHttpError::Client)?;
            let Some(next_total) = total.checked_add(chunk.len()) else {
                return Err(StreamableHttpError::Client(
                    LimitedHttpClientError::BodyTooLarge {
                        limit: self.max_body_bytes,
                    },
                ));
            };
            if next_total > self.max_body_bytes {
                return Err(StreamableHttpError::Client(
                    LimitedHttpClientError::BodyTooLarge {
                        limit: self.max_body_bytes,
                    },
                ));
            }
            total = next_total;
            if collect {
                body.extend_from_slice(&chunk);
            }
        }
        Ok(body)
    }

    async fn read_body(
        &self,
        response: reqwest::Response,
    ) -> Result<Vec<u8>, StreamableHttpError<LimitedHttpClientError>> {
        self.read_body_bounded(response, true).await
    }

    async fn drain_body(
        &self,
        response: reqwest::Response,
    ) -> Result<(), StreamableHttpError<LimitedHttpClientError>> {
        self.read_body_bounded(response, false).await?;
        Ok(())
    }

    fn as_sse_stream(
        &self,
        response: reqwest::Response,
    ) -> BoxStream<'static, Result<Sse, SseError>> {
        SseStream::from_bytes_stream(Self::limited_stream(
            response,
            self.max_body_bytes,
            self.cancellation.clone(),
        ))
        .boxed()
    }
}

impl StreamableHttpClient for LimitedHttpClient {
    type Error = LimitedHttpClientError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            MAX_HTTP_RESPONSE_BODY_BYTES,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        _max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let mut request = self
            .client
            .post(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
        if let Some(auth_header) = auth_header {
            request = request.bearer_auth(auth_header);
        }
        request = self.apply_custom_headers(request, custom_headers)?;
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        let response = self.send(request.json(&message)).await?;
        self.check_content_length(&response)?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED
            && let Some(header) = response.headers().get(reqwest::header::WWW_AUTHENTICATE)
        {
            let header = header
                .to_str()
                .map_err(|_| {
                    StreamableHttpError::UnexpectedServerResponse(Cow::from(
                        "invalid www-authenticate header value",
                    ))
                })?
                .to_owned();
            return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                header,
            )));
        }
        if status == reqwest::StatusCode::FORBIDDEN
            && let Some(header) = response.headers().get(reqwest::header::WWW_AUTHENTICATE)
        {
            let header = header
                .to_str()
                .map_err(|_| {
                    StreamableHttpError::UnexpectedServerResponse(Cow::from(
                        "invalid www-authenticate header value",
                    ))
                })?
                .to_owned();
            return Err(StreamableHttpError::InsufficientScope(
                InsufficientScopeError::new(header, None),
            ));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
        let content_length = response.content_length();
        let response_session_id = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        if matches!(
            status,
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
        ) {
            self.drain_body(response).await?;
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == reqwest::StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        if status.is_success()
            && content_length == Some(0)
            && matches!(
                message,
                rmcp::model::ClientJsonRpcMessage::Notification(_)
                    | rmcp::model::ClientJsonRpcMessage::Response(_)
                    | rmcp::model::ClientJsonRpcMessage::Error(_)
            )
        {
            self.drain_body(response).await?;
            return Ok(StreamableHttpPostResponse::Accepted);
        }

        if !status.is_success() {
            let body = self.read_body(response).await?;
            if content_type
                .as_deref()
                .is_some_and(|value| value.starts_with(JSON_MIME_TYPE))
                && let Some(message) = parse_json_rpc_error(&body)
            {
                return Ok(StreamableHttpPostResponse::Json(
                    message,
                    response_session_id,
                ));
            }
            let body = String::from_utf8_lossy(&body);
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status}: {body}"),
            )));
        }

        match content_type.as_deref() {
            Some(value) if value.starts_with(EVENT_STREAM_MIME_TYPE) => Ok(
                StreamableHttpPostResponse::Sse(self.as_sse_stream(response), response_session_id),
            ),
            Some(value) if value.starts_with(JSON_MIME_TYPE) => {
                let body = self.read_body(response).await?;
                match serde_json::from_slice::<rmcp::model::ServerJsonRpcMessage>(&body) {
                    Ok(message) => Ok(StreamableHttpPostResponse::Json(
                        message,
                        response_session_id,
                    )),
                    Err(_error) => Ok(StreamableHttpPostResponse::Accepted),
                }
            }
            _ => Err(StreamableHttpError::UnexpectedContentType(content_type)),
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let mut request = self.client.delete(uri.as_ref());
        if let Some(auth_header) = auth_header {
            request = request.bearer_auth(auth_header);
        }
        request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        request = self.apply_custom_headers(request, custom_headers)?;
        let response = self.send(request).await?;
        self.check_content_length(&response)?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        let response = response
            .error_for_status()
            .map_err(LimitedHttpClientError::Request)
            .map_err(StreamableHttpError::Client)?;
        self.drain_body(response).await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            MAX_HTTP_RESPONSE_BODY_BYTES,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        _max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let mut request = self
            .client
            .get(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        if let Some(last_event_id) = last_event_id {
            request = request.header(HEADER_LAST_EVENT_ID, last_event_id);
        }
        if let Some(auth_header) = auth_header {
            request = request.bearer_auth(auth_header);
        }
        request = self.apply_custom_headers(request, custom_headers)?;
        let response = self.send(request).await?;
        self.check_content_length(&response)?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        let response = response
            .error_for_status()
            .map_err(LimitedHttpClientError::Request)
            .map_err(StreamableHttpError::Client)?;
        match response.headers().get(reqwest::header::CONTENT_TYPE) {
            Some(value)
                if value
                    .as_bytes()
                    .starts_with(EVENT_STREAM_MIME_TYPE.as_bytes())
                    || value.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()) =>
            {
                Ok(self.as_sse_stream(response))
            }
            Some(value) => Err(StreamableHttpError::UnexpectedContentType(Some(
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            ))),
            None => Err(StreamableHttpError::UnexpectedContentType(None)),
        }
    }
}

fn parse_json_rpc_error(body: &[u8]) -> Option<rmcp::model::ServerJsonRpcMessage> {
    serde_json::from_slice::<rmcp::model::ServerJsonRpcMessage>(body)
        .ok()
        .filter(|message| matches!(message, rmcp::model::ServerJsonRpcMessage::Error(_)))
}
