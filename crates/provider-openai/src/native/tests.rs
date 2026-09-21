use super::*;
use agent_harness_core::{
    Access, AgentRuntime, CapabilityPolicy, CompiledContext, DirectExecutor,
    LayeredContextCompiler, MemoryEventStore, ModelProgressEvent, ModelProgressObserver,
    RuntimeEvent, ThreadId, ToolRegistry, TurnId,
};
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn start_server(response: String, hold: bool) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(4), async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buffer[..n]);
                assert!(request.len() < 65536);
                if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let len: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            if hold {
                match socket.read(&mut buffer).await {
                    Ok(0) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
                    other => panic!("cancelled request did not close: {other:?}"),
                }
            }
            request
        })
        .await
        .expect("bounded local server")
    });
    (url, task)
}

fn response(body: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
}
fn request() -> ModelRequest {
    ModelRequest {
        thread_id: ThreadId::new(),
        turn_id: TurnId::new(),
        iteration: 0,
        context: CompiledContext {
            prompt: vec![],
            messages: vec![],
        },
        tools: vec![],
    }
}
fn adapter(url: String, streaming: bool) -> OpenAiCompatibleAdapter {
    OpenAiCompatibleAdapter::new_async(
        OpenAiConfig::new(url, "fixture"),
        Arc::new(ReqwestTransport::new().unwrap()),
    )
    .with_streaming(streaming)
}
const DRAFT: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"},\"finish_reason\":null}]}\n\n";
const END: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

#[derive(Default)]
struct Observer {
    events: Mutex<Vec<ModelProgressEvent>>,
    cancel: Option<CancellationToken>,
}
impl ModelProgressObserver for Observer {
    fn on_progress(&self, event: &ModelProgressEvent) {
        self.events.lock().unwrap().push(event.clone());
        if matches!(event.progress, ModelProgress::TextDelta { .. }) {
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
        }
    }
}

#[test]
fn native_json_and_sse_preserve_request_mapping_and_diagnostics() {
    runtime().block_on(async {
        for streaming in [false, true] {
            let body = if streaming {
                format!("{DRAFT}{END}")
            } else {
                json!({"choices":[{"message":{"content":"你好"},"finish_reason":"stop"}]})
                    .to_string()
            };
            let (url, server) = start_server(response(&body), false).await;
            let model = adapter(url, streaming);
            let observer = Arc::new(Observer::default());
            let result = model
                .complete_controlled(
                    request(),
                    ModelControl {
                        cancellation: CancellationToken::new(),
                        observer: Some(observer.clone()),
                    },
                )
                .await
                .unwrap();
            assert_eq!(result.content, "你好");
            assert_eq!(result.stop_reason, StopReason::EndTurn);
            let sent = String::from_utf8(server.await.unwrap()).unwrap();
            assert!(sent.contains("\"model\":\"fixture\""));
            assert_eq!(sent.contains("\"stream\":true"), streaming);
            let events = observer.events.lock().unwrap();
            assert!(matches!(events[0].progress, ModelProgress::Started));
            let ModelProgress::Finished { timings, outcome } = &events.last().unwrap().progress
            else {
                panic!("missing finish")
            };
            assert_eq!(outcome, "completed");
            assert_eq!(timings.first_text_ms.is_some(), streaming);
            assert!(
                timings.dns_ms.is_none()
                    && timings.tcp_connect_ms.is_none()
                    && timings.tls_ms.is_none()
                    && timings.first_byte_ms.is_none()
            );
        }
    });
}

#[test]
fn cancellation_closes_stream_and_does_not_wait_for_deadline() {
    runtime().block_on(async {
        let (url,server) = start_server(response(DRAFT),true).await;
        let cancel = CancellationToken::new();
        let observer = Arc::new(Observer { cancel:Some(cancel.clone()), ..Observer::default() });
        let model = adapter(url,true);
        let start = Instant::now();
        let result = model.complete_controlled(request(),ModelControl { cancellation:cancel, observer:Some(observer.clone()) }).await;
        assert!(!result.unwrap_err().retryable);
        server.await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        let events = observer.events.lock().unwrap();
        assert!(matches!(&events.last().unwrap().progress,ModelProgress::Finished { outcome,.. } if outcome == "cancelled"));
    });
}

#[test]
fn dropping_one_future_closes_only_its_socket_and_settles_progress() {
    runtime().block_on(async {
        let (url,server) = start_server(response(DRAFT),true).await;
        let shared = CancellationToken::new();
        let observer = Arc::new(Observer::default());
        let model = adapter(url,true);
        let mut pending = model.complete_controlled(request(),ModelControl { cancellation:shared.clone(), observer:Some(observer.clone()) });
        tokio::select! {
            result = &mut pending => panic!("unexpected completion {result:?}"),
            _ = async {
                loop {
                    if observer.events.lock().unwrap().iter().any(|e| matches!(e.progress,ModelProgress::TextDelta { .. })) { break; }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            } => {}
            _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("no preview"),
        }
        drop(pending);
        server.await.unwrap();
        assert!(!shared.is_cancelled());
        let events = observer.events.lock().unwrap();
        assert!(matches!(&events.last().unwrap().progress,ModelProgress::Finished { outcome,.. } if outcome == "cancelled"));
    });
}

#[test]
fn incomplete_tool_stream_never_reaches_runtime_execution() {
    runtime().block_on(async {
        let partial = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"function\":{\"name\":\"write\",\"arguments\":\"{\"}}]},\"finish_reason\":null}]}\n\n";
        for end in ["", "data: [DONE]\n\n"] {
            let (url,server) = start_server(response(&format!("{partial}{end}")),false).await;
            let store = Arc::new(MemoryEventStore::new());
            let runtime = AgentRuntime::new(Arc::new(adapter(url,true)),store.clone(),Arc::new(LayeredContextCompiler::default()),ToolRegistry::new(),Arc::new(CapabilityPolicy::deny_by_default().grant("write",Access::Allow)))
                .with_executor(Arc::new(DirectExecutor));
            let thread = runtime.start_thread().unwrap();
            assert!(runtime.run_turn(&thread,"edit").await.is_err());
            let events = store.all_events().unwrap();
            assert!(!events.iter().any(|e| matches!(e.event,RuntimeEvent::ToolExecutionStarted { .. } | RuntimeEvent::AssistantMessage { .. } | RuntimeEvent::TurnCompleted { .. })));
            server.await.unwrap();
        }
    });
}

#[test]
fn deadline_and_http_errors_fail_without_redirect_or_retry() {
    runtime().block_on(async {
        let (url,server) = start_server(response(""),true).await;
        let mut config = OpenAiConfig::new(url,"fixture");
        config.timeout_seconds = 1;
        let model = OpenAiCompatibleAdapter::new_async(config,Arc::new(ReqwestTransport::new().unwrap()));
        let start = Instant::now();
        assert!(model.complete(request()).await.unwrap_err().message.contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
        server.await.unwrap();
        for (status,retryable) in [(302,false),(400,false),(429,true),(503,true)] {
            let (url,server) = start_server(format!("HTTP/1.1 {status} Fixture\r\nLocation: http://127.0.0.1:1/never\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),false).await;
            let error = adapter(url,false).complete(request()).await.unwrap_err();
            assert!(error.message.contains(&status.to_string()),"{error:?}");
            assert_eq!(error.retryable,retryable);
            server.await.unwrap();
        }
    });
}

#[test]
fn invalid_headers_and_budget_fail_before_network() {
    runtime().block_on(async {
        let mut config = OpenAiConfig::new("http://127.0.0.1:1", "fixture");
        config.api_key = Some("secret\ninvalid".into());
        let model =
            OpenAiCompatibleAdapter::new_async(config, Arc::new(ReqwestTransport::new().unwrap()));
        let error = model.complete(request()).await.unwrap_err();
        assert!(error.message.contains("header"));
        assert!(!error.message.contains("secret"));
        assert!(!error.retryable);
        struct Counter;
        impl RequestTokenCounter for Counter {
            fn count_input_tokens(&self, body: &Value) -> Result<u64, ModelError> {
                assert_eq!(body["max_completion_tokens"], 10);
                Ok(100)
            }
        }
        let model = adapter("http://127.0.0.1:1".into(), false)
            .with_request_budget(
                RequestBudget {
                    context_window_tokens: 100,
                    reserved_output_tokens: 10,
                },
                Arc::new(Counter),
            )
            .unwrap();
        let error = model.complete(request()).await.unwrap_err();
        assert!(error.message.contains("budget"));
        assert!(!error.retryable);
    });
}

#[test]
fn oversized_responses_are_rejected_before_decoding() {
    runtime().block_on(async {
        let (url, server) = start_server(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_BODY + 1
            ),
            false,
        )
        .await;
        let error = adapter(url, false).complete(request()).await.unwrap_err();
        assert!(error.message.contains("exceeds 8 MiB"));
        assert!(!error.retryable);
        server.await.unwrap();
    });
}

#[test]
fn cancellation_interrupts_waiting_for_headers() {
    runtime().block_on(async {
        let (url, server) = start_server(String::new(), true).await;
        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            signal.cancel();
        });
        let start = Instant::now();
        let error = adapter(url, false)
            .complete_controlled(
                request(),
                ModelControl {
                    cancellation,
                    observer: None,
                },
            )
            .await
            .unwrap_err();
        assert!(!error.retryable);
        assert!(error.message.contains("cancelled"));
        server.await.unwrap();
        canceller.await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
    });
}

#[test]
fn missing_runtime_fails_without_panicking_or_starting_io() {
    use std::task::{Context, Poll, Waker};
    let model = adapter("http://127.0.0.1:1".into(), false);
    let mut pending = model.complete(request());
    let result = pending
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()));
    let Poll::Ready(Err(error)) = result else {
        panic!("expected runtime error");
    };
    assert!(error.message.contains("Tokio runtime"));
    assert!(!error.retryable);
}
