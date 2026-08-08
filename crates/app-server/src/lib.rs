//! Optional line-oriented JSON-RPC control plane.

use std::collections::BTreeMap;
use std::future::Future;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;

use agent_harness_core::{AgentRuntime, CancellationToken, RuntimeError, ThreadId, TurnId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TurnRunState {
    Running,
    Completed { content: String },
    Failed { error: String },
    Cancelled,
}

#[derive(Clone)]
struct TurnControl {
    thread_id: ThreadId,
    cancellation: CancellationToken,
    state: TurnRunState,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ServerError {
    #[error("JSON-RPC input error: {0}")]
    Input(String),
    #[error("JSON-RPC output error: {0}")]
    Output(String),
}

#[derive(Clone, Debug)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: -32600,
            message: message.into(),
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
        }
    }
}

pub struct AppServer {
    runtime: Arc<AgentRuntime>,
    turns: Arc<Mutex<BTreeMap<TurnId, TurnControl>>>,
}

impl AppServer {
    #[must_use]
    pub fn new(runtime: Arc<AgentRuntime>) -> Self {
        Self {
            runtime,
            turns: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Handles one JSON-RPC value. Notifications (requests without `id`) are
    /// executed but return `None`.
    pub fn handle_value(&self, request: Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let response = self.dispatch(&request);
        id.map(|id| match response {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": error.code, "message": error.message }
            }),
        })
    }

    pub fn serve<R, W>(&self, reader: R, mut writer: W) -> Result<(), ServerError>
    where
        R: BufRead,
        W: Write,
    {
        for line in reader.lines() {
            let line = line.map_err(|error| ServerError::Input(error.to_string()))?;
            if line.trim().is_empty() {
                continue;
            }
            let request = match serde_json::from_str::<Value>(&line) {
                Ok(request) => request,
                Err(error) => {
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": { "code": -32700, "message": error.to_string() }
                    });
                    writeln!(writer, "{response}")
                        .map_err(|error| ServerError::Output(error.to_string()))?;
                    writer
                        .flush()
                        .map_err(|error| ServerError::Output(error.to_string()))?;
                    continue;
                }
            };
            if let Some(response) = self.handle_value(request) {
                writeln!(writer, "{response}")
                    .map_err(|error| ServerError::Output(error.to_string()))?;
                writer
                    .flush()
                    .map_err(|error| ServerError::Output(error.to_string()))?;
            }
        }
        Ok(())
    }

    fn dispatch(&self, request: &Value) -> Result<Value, RpcError> {
        if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(RpcError::invalid_request("jsonrpc must be `2.0`"));
        }
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_request("method must be a string"))?;
        let params = request.get("params").unwrap_or(&Value::Null);
        match method {
            "server/health" => Ok(json!({ "status": "ok" })),
            "thread/start" => {
                let thread_id = self.runtime.start_thread().map_err(runtime_error)?;
                Ok(json!({ "thread_id": thread_id }))
            }
            "thread/events" => {
                let thread_id = thread_param(params)?;
                let after_sequence = params.get("after_sequence").and_then(Value::as_u64);
                let events = self
                    .runtime
                    .events(&thread_id)
                    .map_err(runtime_error)?
                    .into_iter()
                    .filter(|event| after_sequence.is_none_or(|sequence| event.sequence > sequence))
                    .collect::<Vec<_>>();
                serde_json::to_value(events).map_err(|error| RpcError::internal(error.to_string()))
            }
            "thread/recover" => {
                let thread_id = thread_param(params)?;
                let report = self.runtime.recover(&thread_id).map_err(runtime_error)?;
                serde_json::to_value(report).map_err(|error| RpcError::internal(error.to_string()))
            }
            "turn/start" => self.start_turn(params),
            "turn/status" => {
                let turn_id = turn_param(params)?;
                let turns = self
                    .turns
                    .lock()
                    .map_err(|_| RpcError::internal("turn state lock was poisoned"))?;
                let control = turns
                    .get(&turn_id)
                    .ok_or_else(|| RpcError::invalid_params("turn does not exist"))?;
                Ok(json!({
                    "thread_id": control.thread_id,
                    "turn_id": turn_id,
                    "state": control.state
                }))
            }
            "turn/cancel" => {
                let turn_id = turn_param(params)?;
                let turns = self
                    .turns
                    .lock()
                    .map_err(|_| RpcError::internal("turn state lock was poisoned"))?;
                let control = turns
                    .get(&turn_id)
                    .ok_or_else(|| RpcError::invalid_params("turn does not exist"))?;
                if control.state != TurnRunState::Running {
                    return Err(RpcError::invalid_params("turn is not running"));
                }
                control.cancellation.cancel();
                Ok(json!({ "turn_id": turn_id, "cancellation_requested": true }))
            }
            _ => Err(RpcError {
                code: -32601,
                message: format!("method `{method}` was not found"),
            }),
        }
    }

    fn start_turn(&self, params: &Value) -> Result<Value, RpcError> {
        let thread_id = thread_param(params)?;
        self.runtime.events(&thread_id).map_err(runtime_error)?;
        let input = params
            .get("input")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("input must be a string"))?
            .to_owned();
        let turn_id = TurnId::new();
        let cancellation = CancellationToken::new();
        self.turns
            .lock()
            .map_err(|_| RpcError::internal("turn state lock was poisoned"))?
            .insert(
                turn_id.clone(),
                TurnControl {
                    thread_id: thread_id.clone(),
                    cancellation: cancellation.clone(),
                    state: TurnRunState::Running,
                },
            );
        let runtime = self.runtime.clone();
        let turns = self.turns.clone();
        let background_turn_id = turn_id.clone();
        let background_thread_id = thread_id.clone();
        thread::spawn(move || {
            let result = block_on(runtime.run_turn_with_id_and_cancellation(
                &background_thread_id,
                background_turn_id.clone(),
                input,
                cancellation,
            ));
            if let Ok(mut turns) = turns.lock()
                && let Some(control) = turns.get_mut(&background_turn_id)
            {
                control.state = match result {
                    Ok(outcome) => TurnRunState::Completed {
                        content: outcome.content,
                    },
                    Err(RuntimeError::Cancelled(_)) => TurnRunState::Cancelled,
                    Err(error) => TurnRunState::Failed {
                        error: error.to_string(),
                    },
                };
            }
        });
        Ok(json!({ "thread_id": thread_id, "turn_id": turn_id }))
    }
}

fn thread_param(params: &Value) -> Result<ThreadId, RpcError> {
    params
        .get("thread_id")
        .and_then(Value::as_str)
        .map(ThreadId::from_string)
        .ok_or_else(|| RpcError::invalid_params("thread_id must be a string"))
}

fn turn_param(params: &Value) -> Result<TurnId, RpcError> {
    params
        .get("turn_id")
        .and_then(Value::as_str)
        .map(TurnId::from_string)
        .ok_or_else(|| RpcError::invalid_params("turn_id must be a string"))
}

fn runtime_error(error: RuntimeError) -> RpcError {
    RpcError::internal(error.to_string())
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::yield_now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use agent_harness_core::{
        BoxFuture, CapabilityPolicy, LayeredContextCompiler, MemoryEventStore, ModelAdapter,
        ModelError, ModelRequest, ModelResponse, ToolRegistry,
    };

    use super::*;

    struct TextModel;

    impl ModelAdapter for TextModel {
        fn complete<'a>(
            &'a self,
            _request: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
            Box::pin(async { Ok(ModelResponse::text("hello")) })
        }
    }

    fn request(id: u64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[test]
    fn starts_thread_and_background_turn() {
        let runtime = Arc::new(AgentRuntime::new(
            Arc::new(TextModel),
            Arc::new(MemoryEventStore::new()),
            Arc::new(LayeredContextCompiler::new()),
            ToolRegistry::new(),
            Arc::new(CapabilityPolicy::deny_by_default()),
        ));
        let server = AppServer::new(runtime);
        let thread_response = server
            .handle_value(request(1, "thread/start", json!({})))
            .unwrap();
        let thread_id = thread_response["result"]["thread_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let turn_response = server
            .handle_value(request(
                2,
                "turn/start",
                json!({ "thread_id": thread_id, "input": "hi" }),
            ))
            .unwrap();
        let turn_id = turn_response["result"]["turn_id"]
            .as_str()
            .unwrap()
            .to_owned();

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let status = server
                .handle_value(request(3, "turn/status", json!({ "turn_id": turn_id })))
                .unwrap();
            if status["result"]["state"]["status"] == "completed" {
                assert_eq!(status["result"]["state"]["content"], "hello");
                break;
            }
            assert!(Instant::now() < deadline, "turn did not complete");
            thread::yield_now();
        }
    }
}
