//! Optional OpenAI-compatible provider adapter.
//!
//! The default `CurlTransport` sends its generated curl config, including the
//! bearer token and request body, over stdin rather than process arguments.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use agent_harness_core::{
    BoxFuture, CallId, ModelAdapter, ModelError, ModelRequest, ModelResponse, StopReason,
    TokenUsage, ToolCall, TranscriptMessage,
};
use serde_json::{Map, Value, json};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpRequest {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Value,
    pub timeout_seconds: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Value,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum TransportError {
    #[error("HTTP transport I/O error: {0}")]
    Io(String),
    #[error("HTTP transport protocol error: {0}")]
    Protocol(String),
    #[error("HTTP endpoint returned status {status}: {body}")]
    Http { status: u16, body: String },
}

pub trait HttpTransport: Send + Sync {
    fn post_json(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;
}

#[derive(Clone, Debug)]
pub struct CurlTransport {
    binary: PathBuf,
}

impl Default for CurlTransport {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("curl"),
        }
    }
}

impl CurlTransport {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = binary.into();
        self
    }
}

impl HttpTransport for CurlTransport {
    fn post_json(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        if !(request.url.starts_with("https://") || request.url.starts_with("http://")) {
            return Err(TransportError::Protocol(
                "only http:// and https:// endpoints are supported".to_owned(),
            ));
        }
        let body = serde_json::to_string(&request.body)
            .map_err(|error| TransportError::Protocol(error.to_string()))?;
        let mut config = String::new();
        config.push_str("silent\nshow-error\nrequest = \"POST\"\n");
        config.push_str(&format!("url = \"{}\"\n", curl_config_escape(&request.url)));
        config.push_str("proto = \"=http,https\"\n");
        config.push_str(&format!(
            "max-time = \"{}\"\n",
            request.timeout_seconds.max(1)
        ));
        for (name, value) in request.headers {
            if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
                return Err(TransportError::Protocol(
                    "HTTP headers cannot contain CR or LF".to_owned(),
                ));
            }
            config.push_str(&format!(
                "header = \"{}\"\n",
                curl_config_escape(&format!("{name}: {value}"))
            ));
        }
        config.push_str(&format!(
            "data-binary = \"{}\"\n",
            curl_config_escape(&body)
        ));
        config.push_str("write-out = \"\\n%{http_code}\"\n");

        let mut child = Command::new(&self.binary)
            .arg("--config")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| TransportError::Io(error.to_string()))?;
        child
            .stdin
            .take()
            .ok_or_else(|| TransportError::Io("curl stdin was unavailable".to_owned()))?
            .write_all(config.as_bytes())
            .map_err(|error| TransportError::Io(error.to_string()))?;
        let output = child
            .wait_with_output()
            .map_err(|error| TransportError::Io(error.to_string()))?;
        if !output.status.success() {
            return Err(TransportError::Protocol(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        let output = String::from_utf8(output.stdout)
            .map_err(|_| TransportError::Protocol("curl returned non-UTF-8 output".to_owned()))?;
        let (body, status) = output.rsplit_once('\n').ok_or_else(|| {
            TransportError::Protocol("curl response did not contain an HTTP status".to_owned())
        })?;
        let status = status
            .trim()
            .parse::<u16>()
            .map_err(|error| TransportError::Protocol(error.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(TransportError::Http {
                status,
                body: body.chars().take(4096).collect(),
            });
        }
        let body = serde_json::from_str(body)
            .map_err(|error| TransportError::Protocol(error.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

#[derive(Clone)]
pub struct OpenAiConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: Option<String>,
    pub organization: Option<String>,
    pub timeout_seconds: u64,
    pub extra_body: Map<String, Value>,
}

impl OpenAiConfig {
    #[must_use]
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
            api_key: None,
            organization: None,
            timeout_seconds: 120,
            extra_body: Map::new(),
        }
    }
}

pub struct OpenAiCompatibleAdapter {
    config: OpenAiConfig,
    transport: Arc<dyn HttpTransport>,
}

impl OpenAiCompatibleAdapter {
    #[must_use]
    pub fn new(config: OpenAiConfig, transport: Arc<dyn HttpTransport>) -> Self {
        Self { config, transport }
    }

    fn request_body(&self, request: &ModelRequest) -> Result<Value, ModelError> {
        let mut messages = Vec::new();
        for segment in &request.context.prompt {
            messages.push(json!({
                "role": "system",
                "content": format!("[{}]\n{}", segment.name, segment.text)
            }));
        }
        for message in &request.context.messages {
            match message {
                TranscriptMessage::User { content } => {
                    messages.push(json!({ "role": "user", "content": content }));
                }
                TranscriptMessage::Assistant {
                    content,
                    tool_calls,
                } => {
                    let calls = tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.id.as_str(),
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": serde_json::to_string(&call.arguments)
                                        .unwrap_or_else(|_| "{}".to_owned())
                                }
                            })
                        })
                        .collect::<Vec<_>>();
                    let mut value = json!({ "role": "assistant", "content": content });
                    if !calls.is_empty() {
                        value["tool_calls"] = Value::Array(calls);
                    }
                    messages.push(value);
                }
                TranscriptMessage::Tool { receipt } => {
                    let content = serde_json::to_string(receipt)
                        .map_err(|error| ModelError::new(error.to_string(), false))?;
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": receipt.call_id.as_str(),
                        "content": content
                    }));
                }
            }
        }
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema
                    }
                })
            })
            .collect::<Vec<_>>();
        let mut body = self.config.extra_body.clone();
        body.insert("model".to_owned(), Value::String(self.config.model.clone()));
        body.insert("messages".to_owned(), Value::Array(messages));
        if !tools.is_empty() {
            body.insert("tools".to_owned(), Value::Array(tools));
        }
        Ok(Value::Object(body))
    }

    fn complete_blocking(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        let body = self.request_body(&request)?;
        let mut headers =
            BTreeMap::from([("Content-Type".to_owned(), "application/json".to_owned())]);
        if let Some(api_key) = &self.config.api_key {
            headers.insert("Authorization".to_owned(), format!("Bearer {api_key}"));
        }
        if let Some(organization) = &self.config.organization {
            headers.insert("OpenAI-Organization".to_owned(), organization.clone());
        }
        let response = self
            .transport
            .post_json(HttpRequest {
                url: self.config.endpoint.clone(),
                headers,
                body,
                timeout_seconds: self.config.timeout_seconds,
            })
            .map_err(|error| ModelError::new(error.to_string(), true))?;
        parse_response(response.body)
    }
}

impl ModelAdapter for OpenAiCompatibleAdapter {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move { self.complete_blocking(request) })
    }
}

fn parse_response(body: Value) -> Result<ModelResponse, ModelError> {
    if let Some(error) = body.get("error") {
        return Err(ModelError::new(format!("provider error: {error}"), false));
    }
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| ModelError::new("response has no choices", false))?;
    let message = choice
        .get("message")
        .ok_or_else(|| ModelError::new("choice has no message", false))?;
    let content = parse_content(message.get("content"));
    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| ModelError::new("tool call has no id", false))?;
            let function = call
                .get("function")
                .ok_or_else(|| ModelError::new("tool call has no function", false))?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ModelError::new("tool call has no function name", false))?;
            let arguments = match function.get("arguments") {
                Some(Value::String(arguments)) => {
                    serde_json::from_str(arguments).map_err(|error| {
                        ModelError::new(
                            format!("tool call `{id}` has invalid JSON arguments: {error}"),
                            false,
                        )
                    })?
                }
                Some(arguments) => arguments.clone(),
                None => Value::Null,
            };
            tool_calls.push(ToolCall {
                id: CallId::from_string(id),
                name: name.to_owned(),
                arguments,
            });
        }
    }
    let stop_reason = match choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        _ => StopReason::Other,
    };
    let usage = body.get("usage").cloned().unwrap_or(Value::Null);
    Ok(ModelResponse {
        content,
        tool_calls,
        stop_reason,
        usage: TokenUsage {
            input_tokens: usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        },
    })
}

fn parse_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(content)) => content.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn curl_config_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use agent_harness_core::{
        CompiledContext, PromptLayer, PromptSegment, ThreadId, ToolDefinition, TurnId,
    };

    use super::*;

    struct MockTransport {
        requests: Mutex<Vec<HttpRequest>>,
        response: Value,
    }

    impl HttpTransport for MockTransport {
        fn post_json(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            self.requests.lock().unwrap().push(request);
            Ok(HttpResponse {
                status: 200,
                body: self.response.clone(),
            })
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, Waker};
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn maps_prompt_tools_and_tool_calls() {
        let transport = Arc::new(MockTransport {
            requests: Mutex::new(Vec::new()),
            response: json!({
                "choices": [{
                    "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call-1",
                            "type": "function",
                            "function": { "name": "read_file", "arguments": "{\"path\":\"a\"}" }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 4 }
            }),
        });
        let adapter = OpenAiCompatibleAdapter::new(
            OpenAiConfig::new("https://example.test/v1/chat/completions", "model"),
            transport.clone(),
        );
        let response = block_on(adapter.complete(ModelRequest {
            thread_id: ThreadId::new(),
            turn_id: TurnId::new(),
            iteration: 0,
            context: CompiledContext {
                prompt: vec![PromptSegment::new(PromptLayer::Stable, "base", "system")],
                messages: vec![TranscriptMessage::User {
                    content: "read".to_owned(),
                }],
            },
            tools: vec![ToolDefinition::new(
                "read_file",
                "read",
                json!({ "type": "object" }),
            )],
        }))
        .unwrap();

        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.tool_calls[0].arguments, json!({ "path": "a" }));
        let request = &transport.requests.lock().unwrap()[0];
        assert_eq!(request.body["messages"][0]["role"], "system");
        assert_eq!(request.body["tools"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn curl_config_escaping_preserves_json_backslashes() {
        assert_eq!(
            curl_config_escape("{\"x\":\"a\\nb\"}"),
            "{\\\"x\\\":\\\"a\\\\nb\\\"}"
        );
    }
}
