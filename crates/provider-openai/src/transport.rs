use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::*;

const MAX_BODY: usize = 8 * 1024 * 1024;

// The guard also covers decoder errors and panic unwinding. Reap in the
// background after kill so cancellation never blocks on an OS wait.
struct ChildGuard(Option<Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

impl HttpTransport for CurlTransport {
    fn post_json(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.post_json_controlled(
            request,
            &HttpControl::default(),
            &mut |_| {},
            &mut RequestTimings::default(),
        )
    }

    fn post_json_controlled(
        &self,
        request: HttpRequest,
        control: &HttpControl,
        on_text: &mut dyn FnMut(String),
        timings: &mut RequestTimings,
    ) -> Result<HttpResponse, TransportError> {
        control.check()?;
        if !(request.url.starts_with("https://") || request.url.starts_with("http://")) {
            return Err(TransportError::Protocol(
                "only http:// and https:// endpoints are supported".into(),
            ));
        }
        let streaming = request.body.get("stream").and_then(Value::as_bool) == Some(true);
        let marker = format!("__NAUSICAA_{}__", agent_harness_core::TurnId::new());
        let mut config = String::from(
            "silent\nshow-error\nno-buffer\nrequest = \"POST\"\nproto = \"=http,https\"\n",
        );
        config.push_str(&format!(
            "url = \"{}\"\nmax-time = \"{}\"\n",
            curl_config_escape(&request.url),
            request.timeout_seconds.max(1)
        ));
        for (name, value) in request.headers {
            if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
                return Err(TransportError::Protocol(
                    "HTTP headers cannot contain CR or LF".into(),
                ));
            }
            config.push_str(&format!(
                "header = \"{}\"\n",
                curl_config_escape(&format!("{name}: {value}"))
            ));
        }
        config.push_str(&format!(
            "data-binary = \"{}\"\n",
            curl_config_escape(&request.body.to_string())
        ));
        config.push_str(&format!("write-out = \"\\n{marker} %{{http_code}} %{{time_namelookup}} %{{time_connect}} %{{time_appconnect}} %{{time_starttransfer}}\\n\"\n"));
        let mut command = Command::new(&self.binary);
        command
            .args(["--disable", "--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = ChildGuard(Some(command.spawn().map_err(io_error)?));
        let process = child.0.as_mut().expect("spawned");
        let mut stdin = process.stdin.take().expect("piped stdin");
        let mut stdout = process.stdout.take().expect("piped stdout");
        let stderr = process.stderr.take().expect("piped stderr");
        std::thread::spawn(move || {
            let _ = stdin.write_all(config.as_bytes());
        });
        // Bounded channel applies backpressure without making cancellation wait
        // for a pipe reader. Dropping receiver releases a blocked sender.
        let (sender, receiver) = mpsc::sync_channel(8);
        std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if sender.send(Ok(buffer[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = sender.send(Err(e));
                        break;
                    }
                }
            }
        });
        let errors = std::thread::spawn(move || {
            let mut error = Vec::new();
            let mut reader = stderr;
            let mut buffer = [0; 1024];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                let remaining = 16384_usize.saturating_sub(error.len());
                error.extend_from_slice(&buffer[..n.min(remaining)]);
            }
            error
        });
        let start = Instant::now();
        let mut bytes = Vec::new();
        let mut decoder = stream::Decoder::default();
        loop {
            control.check()?;
            if start.elapsed() >= Duration::from_secs(request.timeout_seconds.max(1)) {
                return Err(TransportError::Io("request deadline exceeded".into()));
            }
            match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(Ok(chunk)) => {
                    if bytes.len() + chunk.len() > MAX_BODY {
                        return Err(TransportError::Protocol("response exceeds 8 MiB".into()));
                    }
                    if streaming {
                        decoder.push(&chunk, on_text)?;
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(Err(e)) => return Err(io_error(e)),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let status = loop {
            control.check()?;
            if start.elapsed() >= Duration::from_secs(request.timeout_seconds.max(1)) {
                return Err(TransportError::Io("request deadline exceeded".into()));
            }
            if let Some(status) = child
                .0
                .as_mut()
                .expect("child")
                .try_wait()
                .map_err(io_error)?
            {
                break status;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        child.0.take(); // Already reaped; never signal a reused process ID.
        let output = String::from_utf8(bytes)
            .map_err(|_| TransportError::Protocol("non-UTF-8 response".into()))?;
        let (body, metrics) = output
            .rsplit_once(&format!("\n{marker} "))
            .ok_or_else(|| TransportError::Protocol("missing curl metrics".into()))?;
        let values = metrics.split_whitespace().collect::<Vec<_>>();
        if values.len() != 5 {
            return Err(TransportError::Protocol("invalid curl metrics".into()));
        }
        let http_status: u16 = values[0]
            .parse()
            .map_err(|_| TransportError::Protocol("invalid HTTP status".into()))?;
        let millis = |i: usize| {
            values[i]
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map(|v| (v * 1000.0) as u64)
        };
        timings.dns_ms = millis(1);
        timings.tcp_connect_ms = millis(2).zip(millis(1)).map(|(a, b)| a.saturating_sub(b));
        timings.tls_ms = millis(3)
            .filter(|v| *v > 0)
            .zip(millis(2))
            .map(|(a, b)| a.saturating_sub(b));
        timings.first_byte_ms = millis(4).filter(|v| *v > 0);
        if !status.success() {
            let error = if errors.is_finished() {
                errors.join().unwrap_or_default()
            } else {
                Vec::new()
            };
            return Err(TransportError::Io(format!(
                "curl {status}: {}",
                String::from_utf8_lossy(&error).trim()
            )));
        }
        if !(200..300).contains(&http_status) {
            return Err(TransportError::Http {
                status: http_status,
                body: body.chars().take(4096).collect(),
            });
        }
        let body = if streaming {
            decoder.finish()?
        } else {
            serde_json::from_str(body).map_err(|e| TransportError::Protocol(e.to_string()))?
        };
        Ok(HttpResponse {
            status: http_status,
            body,
        })
    }
}
fn io_error(error: std::io::Error) -> TransportError {
    TransportError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn server(body: String, hold: bool) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let handle = std::thread::spawn(move || {
            let start = Instant::now();
            let mut socket = loop {
                if let Ok((socket, _)) = listener.accept() {
                    break socket;
                }
                assert!(start.elapsed() < Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(5));
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let n = socket.read(&mut buffer).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buffer[..n]);
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
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}").unwrap();
            socket.flush().unwrap();
            if hold {
                // Local cancellation must close the socket without waiting for
                // the 10-second request deadline or an upstream [DONE].
                assert_eq!(socket.read(&mut buffer).unwrap(), 0);
            }
        });
        (url, handle)
    }
    fn request(url: String) -> HttpRequest {
        HttpRequest {
            url,
            headers: BTreeMap::new(),
            body: json!({"stream":true}),
            timeout_seconds: 10,
        }
    }
    #[test]
    fn streaming_delivers_preview_before_completion_and_cancels_open_socket() {
        let (url,server) = server("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n".into(),true);
        let control = HttpControl::default();
        let start = Instant::now();
        let mut preview = String::new();
        let result = CurlTransport::new().post_json_controlled(
            request(url),
            &control,
            &mut |text| {
                preview.push_str(&text);
                control.cancellation.cancel();
            },
            &mut RequestTimings::default(),
        );
        assert!(result.is_err());
        assert_eq!(preview, "hello");
        assert!(start.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
    }
    struct ProgressSender(mpsc::Sender<agent_harness_core::ModelProgressEvent>);
    impl agent_harness_core::ModelProgressObserver for ProgressSender {
        fn on_progress(&self, event: &agent_harness_core::ModelProgressEvent) {
            let _ = self.0.send(event.clone());
        }
    }

    #[test]
    fn dropping_pending_future_cancels_only_its_request_and_reports_timings() {
        use agent_harness_core::{CompiledContext, ThreadId, TurnId};
        use std::task::{Context, Poll, Waker};
        let (url, server) = server("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"draft\"},\"finish_reason\":null}]}\n\n".into(),true);
        let adapter = OpenAiCompatibleAdapter::new(
            OpenAiConfig::new(url, "fixture"),
            Arc::new(CurlTransport::new()),
        )
        .with_streaming(true);
        let (sender, receiver) = mpsc::channel();
        let cancellation = CancellationToken::new();
        let mut future = adapter.complete_controlled(
            ModelRequest {
                thread_id: ThreadId::new(),
                turn_id: TurnId::new(),
                iteration: 0,
                context: CompiledContext {
                    prompt: vec![],
                    messages: vec![],
                },
                tools: vec![],
            },
            ModelControl {
                cancellation: cancellation.clone(),
                observer: Some(Arc::new(ProgressSender(sender))),
            },
        );
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        loop {
            if matches!(
                receiver
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .progress,
                ModelProgress::TextDelta { .. }
            ) {
                break;
            }
        }
        drop(future);
        loop {
            if let ModelProgress::Finished { timings, outcome } = receiver
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .progress
            {
                assert_eq!(outcome, "cancelled");
                assert!(timings.first_text_ms.is_some());
                assert!(timings.total_ms < 2000);
                break;
            }
        }
        assert!(!cancellation.is_cancelled());
        server.join().unwrap();
    }

    #[test]
    fn cancellation_with_partial_tool_stream_persists_cancelled_without_assistant() {
        use agent_harness_core::{
            AgentRuntime, CapabilityPolicy, LayeredContextCompiler, MemoryEventStore, RuntimeError,
            RuntimeEvent, ToolRegistry,
        };
        use std::task::{Context, Poll, Waker};
        let (url,server) = server("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"draft\",\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\"}}]},\"finish_reason\":null}]}\n\n".into(),true);
        let (sender, receiver) = mpsc::channel();
        let store = Arc::new(MemoryEventStore::new());
        let runtime = AgentRuntime::new(
            Arc::new(
                OpenAiCompatibleAdapter::new(
                    OpenAiConfig::new(url, "fixture"),
                    Arc::new(CurlTransport::new()),
                )
                .with_streaming(true),
            ),
            store.clone(),
            Arc::new(LayeredContextCompiler::default()),
            ToolRegistry::new(),
            Arc::new(CapabilityPolicy::deny_by_default()),
        )
        .with_model_observer(Arc::new(ProgressSender(sender)));
        let thread = runtime.start_thread().unwrap();
        let cancellation = CancellationToken::new();
        let mut future =
            Box::pin(runtime.run_turn_with_cancellation(&thread, "test", cancellation.clone()));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));
        loop {
            if matches!(
                receiver
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .progress,
                ModelProgress::TextDelta { .. }
            ) {
                break;
            }
        }
        cancellation.cancel();
        let start = Instant::now();
        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(result) => {
                    assert!(matches!(result, Err(RuntimeError::Cancelled(_))));
                    break;
                }
                Poll::Pending => {
                    assert!(start.elapsed() < Duration::from_secs(3));
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        let events = store.all_events().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e.event, RuntimeEvent::TurnCancelled))
        );
        assert!(!events.iter().any(|e| matches!(
            e.event,
            RuntimeEvent::AssistantMessage { .. }
                | RuntimeEvent::ToolExecutionStarted { .. }
                | RuntimeEvent::TurnFailed { .. }
        )));
        server.join().unwrap();
    }

    #[test]
    fn completed_stream_records_network_phases_and_missing_done_fails() {
        for done in [true, false] {
            let (url, server) = server(
                format!(
                    "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"hello\"}},\"finish_reason\":\"stop\"}}]}}\n\n{}",
                    if done { "data: [DONE]\n\n" } else { "" }
                ),
                false,
            );
            let mut timings = RequestTimings::default();
            let result = CurlTransport::new().post_json_controlled(
                request(url),
                &HttpControl::default(),
                &mut |_| {},
                &mut timings,
            );
            assert_eq!(result.is_ok(), done, "{result:?}");
            assert!(timings.dns_ms.is_some());
            assert!(timings.tcp_connect_ms.is_some());
            server.join().unwrap();
        }
    }
}
