//! Newline-delimited JSON-RPC transport for an `AsyncRead`/`AsyncWrite` pair (MCP stdio framing).
//!
//! rmcp's own `AsyncRwTransport` is close to what we need, but this transport fixes four behaviours
//! that matter for a robust stdio server:
//!
//! * a line that is not JSON gets a JSON-RPC `-32700 Parse error` (rmcp silently ignores it);
//! * well-formed JSON that is not a JSON-RPC message gets `-32600 Invalid Request` carrying the request's
//!   `id` when it has one, so the client can correlate it instead of waiting for a timeout; a request
//!   whose `id` is null, fractional, boolean, an array or an object gets `-32600` without an id (rmcp
//!   would take it for a notification and never answer; a `notifications/…` method with a stray `id` is
//!   still handled as the notification it is); `params` that are not an object get `-32602`
//!   (`-32601` for an unknown method); malformed notifications are dropped (a notification never gets a
//!   response), and an `initialize` whose params are malformed gets `-32602` naming the required fields
//!   and the supported protocol versions;
//! * input lines are capped ([`MAX_INPUT_LINE_BYTES`]) and every outgoing message is one compact line,
//!   written atomically under a lock and flushed; a response too large for clients' line limits is
//!   replaced by an error ([`MAX_OUTPUT_LINE_BYTES`]);
//! * before a session exists, stray notifications and responses are dropped instead of being handed
//!   to rmcp, whose pre-`initialize` loop would otherwise abort the connection on them.
//!
//! Requests, notifications and responses that parse are passed to rmcp untouched, so lifecycle
//! negotiation (both protocol eras), dispatch, cancellation and progress remain rmcp's.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::model::{
    ClientRequest, ErrorData, GetMeta, JsonRpcMessage, NumberOrString, ProtocolVersion, RequestId,
    ServerResult,
};
use rmcp::service::{RoleServer, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// Largest accepted input line (one JSON-RPC message). Timelines are at most 5 MiB, so this leaves room
/// for JSON escaping of an inline `yaml_content`.
pub(crate) const MAX_INPUT_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Largest message written. Codex's stdio client rejects lines over 8 MiB; tool results are kept far
/// below this by construction, so this is only a last-resort guard.
pub(crate) const MAX_OUTPUT_LINE_BYTES: usize = 7 * 1024 * 1024;

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// Protocol versions the server speaks (rmcp's full list; see `ServerHandler::supported_protocol_versions`).
pub(crate) fn supported_versions() -> &'static [ProtocolVersion] {
    ProtocolVersion::KNOWN_VERSIONS
}

/// The transport. See the module docs.
pub(crate) struct LineTransport<R, W> {
    reader: BufReader<R>,
    line: Vec<u8>,
    /// The current line exceeded [`MAX_INPUT_LINE_BYTES`]; its bytes are skipped up to the newline.
    discarding: bool,
    eof: bool,
    writer: Arc<Mutex<Option<W>>>,
    /// True once a lifecycle exists: an `initialize` response was sent, or a request carrying complete
    /// 2026-07-28 `_meta` with a supported version was passed on.
    session_open: bool,
}

/// One line read from the input.
enum LineRead {
    /// A complete line (or the final unterminated line at end of input) is in `self.line`.
    Line,
    /// A line longer than [`MAX_INPUT_LINE_BYTES`] was skipped.
    Oversized,
    /// End of input.
    Eof,
}

/// What to do with one input line.
#[derive(Debug)]
enum Decoded {
    Message(Box<RxJsonRpcMessage<RoleServer>>),
    Reply(ErrorData, Option<RequestId>),
    Ignore(&'static str),
}

impl<R, W> LineTransport<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    pub(crate) fn new(reader: R, writer: W) -> Self {
        LineTransport {
            reader: BufReader::with_capacity(64 * 1024, reader),
            line: Vec::new(),
            discarding: false,
            eof: false,
            writer: Arc::new(Mutex::new(Some(writer))),
            session_open: false,
        }
    }

    /// Read the next line into `self.line`. Cancellation-safe: the only await is `fill_buf`, which consumes
    /// nothing, and partial lines persist in `self.line` across calls (rmcp polls `receive` in a
    /// `select!`).
    async fn read_line(&mut self) -> std::io::Result<LineRead> {
        loop {
            let buf = self.reader.fill_buf().await?;
            if buf.is_empty() {
                self.eof = true;
                return Ok(if self.discarding {
                    self.discarding = false;
                    LineRead::Oversized
                } else if self.line.is_empty() {
                    LineRead::Eof
                } else {
                    LineRead::Line
                });
            }
            let (take, complete) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (buf.len(), false),
            };
            if !self.discarding {
                if self.line.len() + take > MAX_INPUT_LINE_BYTES {
                    self.discarding = true;
                    self.line = Vec::new();
                } else {
                    self.line.extend_from_slice(&buf[..take]);
                }
            }
            self.reader.consume(take);
            if complete {
                if self.discarding {
                    self.discarding = false;
                    return Ok(LineRead::Oversized);
                }
                return Ok(LineRead::Line);
            }
        }
    }

    /// Answer a line that cannot be handed to rmcp. The write runs as its own task so `receive` stays
    /// cancellation-safe.
    fn reply(&self, error: ErrorData, id: Option<RequestId>) {
        tracing::debug!(code = error.code.0, message = %error.message, "replying to an invalid message");
        let write = write_message(
            self.writer.clone(),
            TxJsonRpcMessage::<RoleServer>::error(error, id),
        );
        tokio::spawn(async move {
            if let Err(error) = write.await {
                tracing::warn!(%error, "failed to write an error response");
            }
        });
    }

    /// Before a session exists, pass on only requests (rmcp's handshake loop accepts nothing else), and
    /// note when a request starts the stateless 2026-07-28 lifecycle.
    fn admit(
        &mut self,
        message: RxJsonRpcMessage<RoleServer>,
    ) -> Option<RxJsonRpcMessage<RoleServer>> {
        if self.session_open {
            return Some(message);
        }
        match &message {
            JsonRpcMessage::Request(request) => {
                match &request.request {
                    ClientRequest::InitializeRequest(_)
                    | ClientRequest::PingRequest(_)
                    | ClientRequest::DiscoverRequest(_) => {}
                    other => {
                        let meta = other.get_meta();
                        let complete = meta
                            .missing_required_keys(&ProtocolVersion::V_2026_07_28)
                            .is_empty();
                        let supported = meta
                            .protocol_version()
                            .is_some_and(|v| supported_versions().contains(&v));
                        if complete && supported {
                            self.session_open = true;
                        }
                    }
                }
                Some(message)
            }
            JsonRpcMessage::Notification(notification) => {
                tracing::debug!(
                    ?notification,
                    "ignoring a notification received before initialization"
                );
                None
            }
            JsonRpcMessage::Response(_) | JsonRpcMessage::Error(_) => {
                tracing::debug!("ignoring a response received before initialization");
                None
            }
        }
    }
}

impl<R, W> Transport<RoleServer> for LineTransport<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    type Error = std::io::Error;

    fn name() -> Cow<'static, str> {
        Cow::Borrowed("soniccraft-line")
    }

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        if let JsonRpcMessage::Response(response) = &item
            && matches!(response.result, ServerResult::InitializeResult(_))
        {
            self.session_open = true;
        }
        write_message(self.writer.clone(), item)
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        loop {
            if self.eof && self.line.is_empty() {
                return None;
            }
            match self.read_line().await {
                Ok(LineRead::Line) => {}
                Ok(LineRead::Oversized) => {
                    self.reply(
                        ErrorData::invalid_request(
                            format!(
                                "Message too large: lines are limited to {MAX_INPUT_LINE_BYTES} bytes. Send \
                                 large timelines as files (yaml_path) instead of inline content."
                            ),
                            None,
                        ),
                        None,
                    );
                    continue;
                }
                Ok(LineRead::Eof) => return None,
                Err(error) => {
                    tracing::error!(%error, "failed to read from the client");
                    return None;
                }
            }
            let line = std::mem::take(&mut self.line);
            match decode(&line) {
                Decoded::Message(message) => {
                    if let Some(message) = self.admit(*message) {
                        return Some(message);
                    }
                }
                Decoded::Reply(error, id) => self.reply(error, id),
                Decoded::Ignore(reason) => tracing::debug!(reason, "ignoring an input line"),
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        let mut guard = self.writer.lock().await;
        if let Some(mut writer) = guard.take() {
            writer.flush().await?;
            writer.shutdown().await?;
        }
        Ok(())
    }
}

/// Serialize `item` as one line and write it atomically.
fn write_message<W>(
    writer: Arc<Mutex<Option<W>>>,
    item: TxJsonRpcMessage<RoleServer>,
) -> impl Future<Output = std::io::Result<()>> + Send + 'static
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let encoded = encode(item);
    async move {
        let bytes = encoded?;
        if bytes.is_empty() {
            return Ok(());
        }
        let mut guard = writer.lock().await;
        let Some(writer) = guard.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "the transport is closed",
            ));
        };
        writer.write_all(&bytes).await?;
        writer.flush().await
    }
}

/// Compact JSON plus `\n`. serde_json escapes control characters inside strings, so the line never
/// contains an embedded newline. Oversized responses become an error for the same id; oversized
/// notifications are dropped (empty output).
fn encode(item: TxJsonRpcMessage<RoleServer>) -> std::io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(&item).map_err(std::io::Error::other)?;
    if bytes.len() > MAX_OUTPUT_LINE_BYTES {
        let size = bytes.len();
        match item {
            JsonRpcMessage::Response(response) => {
                tracing::error!(size, id = %response.id, "result too large to send; replying with an error");
                let error = ErrorData::internal_error(
                    format!(
                        "The result is {size} bytes, above the {MAX_OUTPUT_LINE_BYTES}-byte message limit. \
                         Split the timeline into smaller parts."
                    ),
                    None,
                );
                let replacement = TxJsonRpcMessage::<RoleServer>::error(error, Some(response.id));
                bytes = serde_json::to_vec(&replacement).map_err(std::io::Error::other)?;
            }
            other => {
                tracing::error!(size, message = ?std::mem::discriminant(&other), "dropping an oversized message");
                return Ok(Vec::new());
            }
        }
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// Classify one input line.
fn decode(line: &[u8]) -> Decoded {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let line = line.strip_prefix(UTF8_BOM).unwrap_or(line);
    if line.iter().all(u8::is_ascii_whitespace) {
        return Decoded::Ignore("blank line");
    }
    let Ok(text) = std::str::from_utf8(line) else {
        return Decoded::Reply(
            ErrorData::parse_error("Parse error: the message is not valid UTF-8", None),
            None,
        );
    };
    match serde_json::from_str::<RxJsonRpcMessage<RoleServer>>(text) {
        Ok(message) => check_parsed(message, text),
        Err(error) if error.is_syntax() || error.is_eof() => Decoded::Reply(
            ErrorData::parse_error(format!("Parse error: {error}"), None),
            None,
        ),
        Err(error) => invalid_message(text, &error),
    }
}

/// Requests that parsed only as rmcp's catch-all `CustomRequest` although their method is `initialize`
/// have malformed params; answer them precisely instead of letting the handshake report missing `_meta`.
/// A "notification" that has an `id` is a request whose id rmcp could not read (null, `1.5`, `true`, an
/// array or an object): JSON-RPC requires an Invalid Request error for it, without an id. A
/// `notifications/…` method is a notification whatever stray `id` it carries, so it is passed on (and
/// never answered) rather than lost.
fn check_parsed(message: RxJsonRpcMessage<RoleServer>, text: &str) -> Decoded {
    if let JsonRpcMessage::Notification(_) = &message
        && is_request_with_unusable_id(text)
    {
        return Decoded::Reply(ErrorData::invalid_request(INVALID_ID_MESSAGE, None), None);
    }
    if let JsonRpcMessage::Request(request) = &message
        && let ClientRequest::CustomRequest(custom) = &request.request
        && custom.method == "initialize"
    {
        let supported: Vec<&str> = supported_versions()
            .iter()
            .filter(|v| v.has_initialize())
            .map(ProtocolVersion::as_str)
            .collect();
        let error = ErrorData::invalid_params(
            format!(
                "Invalid initialize params: `protocolVersion` (string), `capabilities` (object) and \
                 `clientInfo` ({{name, version}}) are required. Supported versions: {} (and 2026-07-28 \
                 without initialize, using per-request _meta).",
                supported.join(", ")
            ),
            Some(serde_json::json!({ "supported": supported })),
        );
        return Decoded::Reply(error, Some(request.id.clone()));
    }
    Decoded::Message(Box::new(message))
}

const INVALID_ID_MESSAGE: &str = "Invalid Request: `id` must be a string or an integer";

/// Methods this server answers; other methods are `-32601` whatever their params.
const KNOWN_METHODS: [&str; 9] = [
    "initialize",
    "ping",
    "server/discover",
    "tools/list",
    "tools/call",
    "prompts/list",
    "resources/list",
    "resources/templates/list",
    "completion/complete",
];

/// True when the JSON object in `text` has an `id` member (of any type, `null` included) although rmcp
/// read it as a notification, and its method is not a `notifications/…` one.
fn is_request_with_unusable_id(text: &str) -> bool {
    serde_json::from_str::<serde_json::Map<String, Value>>(text).is_ok_and(|object| {
        object.contains_key("id")
            && !object
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|m| m.starts_with("notifications/"))
    })
}

/// `an array`, `a string`, ... for messages about the JSON type of `params`.
fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Well-formed JSON that is not a valid JSON-RPC 2.0 message.
fn invalid_message(text: &str, error: &serde_json::Error) -> Decoded {
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            return Decoded::Reply(
                ErrorData::parse_error(format!("Parse error: {error}"), None),
                None,
            );
        }
    };
    if value.is_array() {
        return Decoded::Reply(
            ErrorData::invalid_request(
                "Invalid Request: JSON-RPC batches are not supported (MCP removed batching in 2025-06-18); \
                 send one message per line",
                None,
            ),
            None,
        );
    }
    let Some(object) = value.as_object() else {
        return Decoded::Reply(
            ErrorData::invalid_request(
                "Invalid Request: a JSON-RPC message must be an object",
                None,
            ),
            None,
        );
    };
    let id = object.get("id").and_then(request_id);
    if object.contains_key("method") && !object.contains_key("id") {
        return Decoded::Ignore("malformed notification");
    }
    if object.contains_key("result") || object.contains_key("error") {
        return Decoded::Ignore("malformed response");
    }
    let method = object.get("method").and_then(Value::as_str);
    let detail = if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        "`jsonrpc` must be \"2.0\"".to_string()
    } else if method.is_none() {
        "`method` must be a string".to_string()
    } else if object.contains_key("id") && id.is_none() {
        "`id` must be a string or an integer".to_string()
    } else if let (Some(method), Some(params)) = (method, object.get("params"))
        && !params.is_object()
        && !params.is_null()
    {
        let error = if KNOWN_METHODS.contains(&method) {
            ErrorData::invalid_params(
                format!(
                    "Invalid params for `{method}`: `params` must be an object (MCP passes parameters by \
                     name), got {}.",
                    json_type(params)
                ),
                None,
            )
        } else {
            ErrorData::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                format!(
                    "Method not found: `{method}`. This server implements initialize, ping, \
                     server/discover, tools/list and tools/call."
                ),
                Some(serde_json::json!({ "method": method })),
            )
        };
        return Decoded::Reply(error, id);
    } else {
        error.to_string()
    };
    Decoded::Reply(
        ErrorData::invalid_request(format!("Invalid Request: {detail}"), None),
        id,
    )
}

fn request_id(value: &Value) -> Option<RequestId> {
    match value {
        Value::String(s) => Some(NumberOrString::String(s.as_str().into())),
        Value::Number(n) => n.as_i64().map(NumberOrString::Number),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(line: &str) -> (i32, Option<RequestId>, String) {
        match decode(line.as_bytes()) {
            Decoded::Reply(error, id) => (error.code.0, id, error.message.into_owned()),
            other => panic!("expected a reply for {line:?}, got {other:?}"),
        }
    }

    #[test]
    fn parse_errors() {
        assert_eq!(reply("{not json").0, -32700);
        assert_eq!(reply("{\"jsonrpc\":\"2.0\",\"id\":1,").0, -32700);
        assert_eq!(decode(b"\xff\xfe").reply_code(), Some(-32700));
    }

    #[test]
    fn invalid_requests_keep_their_id() {
        let (code, id, message) = reply(r#"{"jsonrpc":"1.0","id":7,"method":"ping"}"#);
        assert_eq!((code, id), (-32600, Some(NumberOrString::Number(7))));
        assert!(message.contains("jsonrpc"), "{message}");
        let (code, id, _) = reply(r#"{"jsonrpc":"2.0","id":"x","method":5}"#);
        assert_eq!(
            (code, id),
            (-32600, Some(NumberOrString::String("x".into())))
        );
        let (code, id, message) = reply(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#);
        assert_eq!((code, id), (-32600, None));
        assert!(message.contains("batch"), "{message}");
        assert_eq!(reply("42").0, -32600);
    }

    #[test]
    fn requests_with_unusable_ids_are_invalid_requests_without_an_id() {
        for id in ["null", "1.5", "true", "[1]", "{\"a\":1}", "2.0"] {
            for method in ["ping", "tools/list", "tools/call"] {
                let line = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}"}}"#);
                let (code, reply_id, message) = reply(&line);
                assert_eq!((code, reply_id), (-32600, None), "{line}");
                assert_eq!(
                    message, "Invalid Request: `id` must be a string or an integer",
                    "{line}"
                );
            }
        }
        // Real notifications and valid ids are untouched; a notification with a stray id is still one.
        assert!(matches!(
            decode(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Decoded::Message(_)
        ));
        assert!(matches!(
            decode(br#"{"jsonrpc":"2.0","id":null,"method":"notifications/cancelled","params":{"requestId":3}}"#),
            Decoded::Message(_)
        ));
        assert!(matches!(
            decode(br#"{"jsonrpc":"2.0","id":-7,"method":"ping"}"#),
            Decoded::Message(_)
        ));
        assert!(matches!(
            decode(br#"{"jsonrpc":"2.0","id":"","method":"ping"}"#),
            Decoded::Message(_)
        ));
    }

    #[test]
    fn positional_params_are_invalid_params() {
        let (code, id, message) =
            reply(r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":[1,2]}"#);
        assert_eq!((code, id), (-32602, Some(NumberOrString::Number(9))));
        assert_eq!(
            message,
            "Invalid params for `tools/call`: `params` must be an object (MCP passes parameters by name), \
             got an array."
        );
        assert!(!message.contains("untagged"), "{message}");
        let (code, _, message) =
            reply(r#"{"jsonrpc":"2.0","id":"a","method":"tools/list","params":"x"}"#);
        assert_eq!(code, -32602);
        assert!(message.contains("got a string"), "{message}");
        let (code, id, _) = reply(r#"{"jsonrpc":"2.0","id":3,"method":"foo/bar","params":[]}"#);
        assert_eq!((code, id), (-32601, Some(NumberOrString::Number(3))));
        // A notification with positional params is still never answered.
        assert!(matches!(
            decode(br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":[1]}"#),
            Decoded::Ignore(_) | Decoded::Message(_)
        ));
    }

    #[test]
    fn malformed_initialize_names_required_fields() {
        let (code, id, message) =
            reply(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#);
        assert_eq!((code, id), (-32602, Some(NumberOrString::Number(0))));
        assert!(
            message.contains("protocolVersion") && message.contains("2025-11-25"),
            "{message}"
        );
    }

    #[test]
    fn blank_lines_bom_and_crlf() {
        assert!(matches!(decode(b"  \r\n"), Decoded::Ignore(_)));
        let line = b"\xEF\xBB\xBF{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\r\n";
        assert!(matches!(decode(line), Decoded::Message(_)));
    }

    #[test]
    fn notifications_are_never_answered() {
        assert!(matches!(
            decode(br#"{"jsonrpc":"2.0","method":"notifications/whatever"}"#),
            Decoded::Message(_) | Decoded::Ignore(_)
        ));
        assert!(matches!(
            decode(br#"{"jsonrpc":"1.0","method":"notifications/cancelled"}"#),
            Decoded::Ignore(_)
        ));
    }

    #[test]
    fn oversized_responses_become_errors() {
        use rmcp::model::{CallToolResult, ContentBlock};
        let big = "x".repeat(MAX_OUTPUT_LINE_BYTES + 10);
        let result =
            ServerResult::CallToolResult(CallToolResult::success(vec![ContentBlock::text(big)]));
        let item = TxJsonRpcMessage::<RoleServer>::response(result, NumberOrString::Number(3));
        let bytes = encode(item).unwrap();
        assert!(bytes.len() < 4096);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["id"], 3);
        assert_eq!(value["error"]["code"], -32603);
        assert_eq!(bytes.last(), Some(&b'\n'));
    }

    impl Decoded {
        fn reply_code(&self) -> Option<i32> {
            match self {
                Decoded::Reply(error, _) => Some(error.code.0),
                _ => None,
            }
        }
    }
}
