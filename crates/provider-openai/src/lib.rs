//! Optional OpenAI-compatible provider adapter.
//!
//! The default `CurlTransport` sends its generated curl config, including the
//! bearer token and request body, over stdin rather than process arguments.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_harness_core::{
    BoxFuture, CallId, CancellationToken, ModelAdapter, ModelControl, ModelError, ModelProgress,
    ModelRequest, ModelResponse, RequestTimings, StopReason, TokenUsage, ToolCall,
    TranscriptMessage,
};
use serde_json::{Map, Value, json};
use thiserror::Error;

mod stream;
mod transport;
mod worker;

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

    /// Legacy transports remain usable without streaming, but must override
    /// this method to interrupt an in-flight blocking call.
    fn post_json_controlled(
        &self,
        request: HttpRequest,
        control: &HttpControl,
        _on_text: &mut dyn FnMut(String),
        _timings: &mut RequestTimings,
    ) -> Result<HttpResponse, TransportError> {
        control.check()?;
        if request.body.get("stream").and_then(Value::as_bool) == Some(true) {
            return Err(TransportError::Protocol(
                "transport does not support streaming".into(),
            ));
        }
        let response = self.post_json(request)?;
        control.check()?;
        Ok(response)
    }
}

/// Request-local cancellation also fires when its model future is dropped.
#[derive(Clone, Default)]
pub struct HttpControl {
    cancellation: CancellationToken,
    abandoned: CancellationToken,
}
impl HttpControl {
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled() || self.abandoned.is_cancelled()
    }
    fn check(&self) -> Result<(), TransportError> {
        if self.is_cancelled() {
            Err(TransportError::Io("request cancelled".into()))
        } else {
            Ok(())
        }
    }
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

#[derive(Clone)]
pub struct OpenAiCompatibleAdapter {
    streaming: bool,
    config: OpenAiConfig,
    transport: Arc<dyn HttpTransport>,
    request_budget: Option<(RequestBudget, Arc<dyn RequestTokenCounter>)>,
}

/// Input and reserved output must fit this model's context window. This is a
/// per-request limit, not a cumulative task spending budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestBudget {
    pub context_window_tokens: u64,
    pub reserved_output_tokens: u64,
}

/// Trusted model-specific accounting for the final provider request, including
/// message framing, tool schemas and any token-bearing extension fields.
/// Return an error if the model or a field cannot be counted reliably. There is
/// intentionally no universal characters-per-token default.
pub trait RequestTokenCounter: Send + Sync {
    fn count_input_tokens(&self, body: &Value) -> Result<u64, ModelError>;
}

impl OpenAiCompatibleAdapter {
    #[must_use]
    pub fn new(config: OpenAiConfig, transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            config,
            transport,
            request_budget: None,
            streaming: false,
        }
    }

    /// Stream text previews; tool arguments are exposed only in a complete response.
    #[must_use]
    pub fn with_streaming(mut self, enabled: bool) -> Self {
        self.streaming = enabled;
        self
    }

    /// Install accounting for the exact body sent to the transport. Existing
    /// output caps may be smaller than the reservation, but never exceed it.
    pub fn with_request_budget(
        mut self,
        budget: RequestBudget,
        counter: Arc<dyn RequestTokenCounter>,
    ) -> Result<Self, ModelError> {
        if budget.reserved_output_tokens == 0
            || budget.reserved_output_tokens >= budget.context_window_tokens
        {
            return Err(ModelError::new(
                "request budget needs positive input capacity and output reservation",
                false,
            ));
        }
        self.request_budget = Some((budget, counter));
        Ok(self)
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
        // Provider extensions cannot restore schemas removed by core policy.
        body.remove("tools");
        body.remove("functions");
        body.remove("function_call");
        if tools.is_empty() {
            body.remove("tool_choice");
        }
        body.insert("model".to_owned(), Value::String(self.config.model.clone()));
        body.insert("messages".to_owned(), Value::Array(messages));
        if !tools.is_empty() {
            body.insert("tools".to_owned(), Value::Array(tools));
        }
        Ok(Value::Object(body))
    }

    fn complete_blocking(
        &self,
        request: &ModelRequest,
        control: &HttpControl,
        on_text: &mut dyn FnMut(String),
        timings: &mut RequestTimings,
    ) -> Result<ModelResponse, ModelError> {
        let mut body = self.request_body(request)?;
        body.as_object_mut()
            .expect("request object")
            .remove("stream");
        body.as_object_mut()
            .expect("request object")
            .remove("stream_options");
        if self.streaming {
            body["stream"] = json!(true);
            body["stream_options"] = json!({"include_usage": true});
        }
        if let Some((budget, counter)) = &self.request_budget {
            let modern = body.get("max_completion_tokens");
            let legacy = body.get("max_tokens");
            if modern.is_some() && legacy.is_some() {
                return Err(ModelError::new(
                    "request budget rejects conflicting output token caps",
                    false,
                ));
            }
            if let Some(cap) = modern.or(legacy) {
                if !matches!(cap.as_u64(), Some(value) if value > 0 && value <= budget.reserved_output_tokens)
                {
                    return Err(ModelError::new(
                        "output token cap must be positive and within the reservation",
                        false,
                    ));
                }
            } else {
                body["max_completion_tokens"] = json!(budget.reserved_output_tokens);
            }
            let input = counter
                .count_input_tokens(&body)
                .map_err(|error| ModelError::new(error.message, false))?;
            let maximum = budget.context_window_tokens - budget.reserved_output_tokens;
            if input > maximum {
                return Err(ModelError::new(
                    format!(
                        "final request exceeds input token budget ({input} > {maximum}); compact context or reduce tool schemas"
                    ),
                    false,
                ));
            }
        }
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
            .post_json_controlled(
                HttpRequest {
                    url: self.config.endpoint.clone(),
                    headers,
                    body,
                    timeout_seconds: self.config.timeout_seconds,
                },
                control,
                on_text,
                timings,
            )
            .map_err(|error| ModelError::new(error.to_string(), true))?;
        parse_response(response.body)
    }
}

impl ModelAdapter for OpenAiCompatibleAdapter {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        self.complete_controlled(request, ModelControl::default())
    }

    fn complete_controlled<'a>(
        &'a self,
        request: ModelRequest,
        control: ModelControl,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        let adapter = self.clone();
        Box::pin(async move { worker::start(adapter, request, control).await })
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

    fn budget_request() -> ModelRequest {
        ModelRequest {
            thread_id: ThreadId::new(),
            turn_id: TurnId::new(),
            iteration: 0,
            context: CompiledContext {
                prompt: vec![],
                messages: vec![TranscriptMessage::User {
                    content: "hello".into(),
                }],
            },
            tools: vec![],
        }
    }

    // Byte counts are a deterministic test oracle, not a production tokenizer.
    struct RecordingCounter(Mutex<Vec<Value>>);
    impl RequestTokenCounter for RecordingCounter {
        fn count_input_tokens(&self, body: &Value) -> Result<u64, ModelError> {
            self.0.lock().unwrap().push(body.clone());
            Ok(serde_json::to_vec(body).unwrap().len() as u64)
        }
    }

    fn empty_transport() -> Arc<MockTransport> {
        Arc::new(MockTransport {
            requests: Mutex::new(vec![]),
            response: json!({"choices": [{"message": {"content": "done"}, "finish_reason": "stop"}]}),
        })
    }

    #[test]
    fn final_request_budget_counts_extensions_and_schemas_before_transport() {
        let transport = empty_transport();
        let counter = Arc::new(RecordingCounter(Mutex::new(vec![])));
        let mut config = OpenAiConfig::new("https://example.test", "pinned-model");
        config
            .extra_body
            .insert("response_format".into(), json!({"type": "json_object"}));
        let adapter = OpenAiCompatibleAdapter::new(config, transport.clone())
            .with_request_budget(
                RequestBudget {
                    context_window_tokens: 1024,
                    reserved_output_tokens: 128,
                },
                counter.clone(),
            )
            .unwrap();
        block_on(adapter.complete(budget_request())).unwrap();
        let sent = transport.requests.lock().unwrap()[0].body.clone();
        assert_eq!(sent, counter.0.lock().unwrap()[0]);
        assert_eq!(sent["max_completion_tokens"], 128);
        assert!(sent.get("response_format").is_some());
        let mut large = budget_request();
        large.tools.push(ToolDefinition::new(
            "large",
            "schema",
            json!({"description": "x".repeat(2000)}),
        ));
        let error = block_on(adapter.complete(large)).unwrap_err();
        assert!(!error.retryable);
        assert!(error.message.contains("final request exceeds"));
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn exact_budget_boundary_and_invalid_caps_fail_closed() {
        struct FixedCounter(u64);
        impl RequestTokenCounter for FixedCounter {
            fn count_input_tokens(&self, _body: &Value) -> Result<u64, ModelError> {
                Ok(self.0)
            }
        }
        for (input, succeeds) in [(90, true), (91, false), (u64::MAX, false)] {
            let transport = empty_transport();
            let adapter = OpenAiCompatibleAdapter::new(
                OpenAiConfig::new("https://example.test", "model"),
                transport.clone(),
            )
            .with_request_budget(
                RequestBudget {
                    context_window_tokens: 100,
                    reserved_output_tokens: 10,
                },
                Arc::new(FixedCounter(input)),
            )
            .unwrap();
            assert_eq!(
                block_on(adapter.complete(budget_request())).is_ok(),
                succeeds
            );
            assert_eq!(
                transport.requests.lock().unwrap().len(),
                usize::from(succeeds)
            );
        }
        for fields in [
            json!({"max_tokens": 11}),
            json!({"max_tokens": 0}),
            json!({"max_tokens": "10"}),
            json!({"max_tokens": 5, "max_completion_tokens": 5}),
        ] {
            let transport = empty_transport();
            let mut config = OpenAiConfig::new("https://example.test", "model");
            config.extra_body = fields.as_object().unwrap().clone();
            let adapter = OpenAiCompatibleAdapter::new(config, transport.clone())
                .with_request_budget(
                    RequestBudget {
                        context_window_tokens: 100,
                        reserved_output_tokens: 10,
                    },
                    Arc::new(FixedCounter(1)),
                )
                .unwrap();
            assert!(block_on(adapter.complete(budget_request())).is_err());
            assert!(transport.requests.lock().unwrap().is_empty());
        }
        let invalid = OpenAiCompatibleAdapter::new(
            OpenAiConfig::new("https://example.test", "model"),
            empty_transport(),
        )
        .with_request_budget(
            RequestBudget {
                context_window_tokens: 10,
                reserved_output_tokens: 10,
            },
            Arc::new(FixedCounter(1)),
        );
        assert!(invalid.is_err());
    }

    #[test]
    fn counting_errors_never_send_or_request_retry() {
        struct FailedCounter;
        impl RequestTokenCounter for FailedCounter {
            fn count_input_tokens(&self, _body: &Value) -> Result<u64, ModelError> {
                Err(ModelError::new("unknown model tokenizer", true))
            }
        }
        let transport = empty_transport();
        let adapter = OpenAiCompatibleAdapter::new(
            OpenAiConfig::new("https://example.test", "model"),
            transport.clone(),
        )
        .with_request_budget(
            RequestBudget {
                context_window_tokens: 100,
                reserved_output_tokens: 10,
            },
            Arc::new(FailedCounter),
        )
        .unwrap();
        assert!(
            !block_on(adapter.complete(budget_request()))
                .unwrap_err()
                .retryable
        );
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn extra_body_cannot_restore_policy_hidden_tools() {
        let transport = empty_transport();
        let mut config = OpenAiConfig::new("https://example.test", "model");
        config.extra_body = json!({"tools": [{"forbidden": true}], "functions": [{"name": "hidden"}], "function_call": "auto", "tool_choice": "required"}).as_object().unwrap().clone();
        let adapter = OpenAiCompatibleAdapter::new(config, transport.clone());
        block_on(adapter.complete(budget_request())).unwrap();
        let sent = &transport.requests.lock().unwrap()[0].body;
        for key in ["tools", "functions", "function_call", "tool_choice"] {
            assert!(sent.get(key).is_none());
        }
    }

    #[test]
    fn curl_config_escaping_preserves_json_backslashes() {
        assert_eq!(
            curl_config_escape("{\"x\":\"a\\nb\"}"),
            "{\\\"x\\\":\\\"a\\\\nb\\\"}"
        );
    }
}
