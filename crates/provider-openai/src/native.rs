use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::*;

const MAX_BODY: usize = 8 * 1024 * 1024;

/// Optional native async HTTP/SSE transport using reqwest and Rustls.
///
/// Poll on a Tokio runtime with I/O and time enabled. Requests have no automatic
/// redirects or retries; cancellation is checked at least every 10 ms while
/// awaiting network I/O (subject to host scheduling). Dropping the future drops
/// its response/connection. DNS/TCP/TLS phase timings are unavailable.
///
/// ```no_run
/// use std::sync::Arc;
/// use agent_harness_core::{CompiledContext, ModelAdapter, ModelRequest, ThreadId, TurnId};
/// use agent_harness_provider_openai::{OpenAiCompatibleAdapter, OpenAiConfig, ReqwestTransport};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
/// let model = OpenAiCompatibleAdapter::new_async(
///     OpenAiConfig::new("https://example.test/v1/chat/completions", "model"),
///     Arc::new(ReqwestTransport::new()?),
/// ).with_streaming(true);
/// let request = ModelRequest {
///     thread_id: ThreadId::new(), turn_id: TurnId::new(), iteration: 0,
///     context: CompiledContext { prompt: vec![], messages: vec![] }, tools: vec![],
/// };
/// let response = runtime.block_on(model.complete(request))?;
/// println!("{}", response.content);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .pool_max_idle_per_host(0)
            .build()
            .map_err(io_error)?;
        Ok(Self { client })
    }

    async fn send(
        &self,
        request: HttpRequest,
        control: &HttpControl,
        on_text: &mut (dyn FnMut(String) + Send),
        timings: &mut RequestTimings,
    ) -> Result<HttpResponse, TransportError> {
        let start = Instant::now();
        let url =
            reqwest::Url::parse(&request.url).map_err(|_| protocol("invalid HTTP endpoint"))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(protocol(
                "endpoint must use HTTP(S) without URL credentials",
            ));
        }
        let mut headers = HeaderMap::new();
        for (name, value) in request.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| protocol("invalid HTTP header name"))?;
            let mut value =
                HeaderValue::from_str(&value).map_err(|_| protocol("invalid HTTP header value"))?;
            if name == reqwest::header::AUTHORIZATION {
                value.set_sensitive(true);
            }
            headers.insert(name, value);
        }
        let streaming = request.body.get("stream").and_then(Value::as_bool) == Some(true);
        let body = serde_json::to_vec(&request.body).map_err(|e| protocol(e.to_string()))?;
        let mut response = self
            .client
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(io_error)?;
        // The public client API exposes completed headers, not the exact first
        // wire byte. Leave first_byte_ms unknown rather than mislabel that time.
        let status = response.status().as_u16();
        let successful = (200..300).contains(&status);
        if response
            .content_length()
            .is_some_and(|size| size > MAX_BODY as u64)
        {
            return Err(protocol("HTTP response exceeds 8 MiB"));
        }
        let mut decoder = stream::Decoder::default();
        let mut body = Vec::new();
        let mut received = 0;
        while let Some(chunk) = response.chunk().await.map_err(io_error)? {
            control.check()?;
            received += chunk.len();
            if received > MAX_BODY {
                return Err(protocol("HTTP response exceeds 8 MiB"));
            }
            if streaming && successful {
                decoder.push(&chunk, on_text)?;
            } else {
                body.extend_from_slice(&chunk);
            }
        }
        control.check()?;
        timings.total_ms = start.elapsed().as_millis() as u64;
        if !successful {
            return Err(TransportError::Http {
                status,
                body: String::from_utf8_lossy(&body).chars().take(4096).collect(),
            });
        }
        let body = if streaming {
            decoder.finish()?
        } else {
            serde_json::from_slice(&body).map_err(|e| protocol(e.to_string()))?
        };
        Ok(HttpResponse { status, body })
    }
}

impl AsyncHttpTransport for ReqwestTransport {
    fn post_json_controlled<'a>(
        &'a self,
        request: HttpRequest,
        control: &'a HttpControl,
        on_text: &'a mut (dyn FnMut(String) + Send),
        timings: &'a mut RequestTimings,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            control.check()?;
            if tokio::runtime::Handle::try_current().is_err() {
                return Err(protocol(
                    "ReqwestTransport requires a Tokio runtime with I/O and time enabled",
                ));
            }
            let deadline = tokio::time::Instant::now()
                .checked_add(Duration::from_secs(request.timeout_seconds.max(1)))
                .ok_or_else(|| protocol("request timeout is too large"))?;
            let operation = self.send(request, control, on_text, timings);
            tokio::pin!(operation);
            let mut cancellation = tokio::time::interval(Duration::from_millis(10));
            cancellation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = cancellation.tick() => { control.check()?; }
                    _ = tokio::time::sleep_until(deadline) => { return Err(TransportError::Io("request timed out".into())); }
                    result = &mut operation => { control.check()?; return result; }
                }
            }
        })
    }
}

fn protocol(message: impl Into<String>) -> TransportError {
    TransportError::Protocol(message.into())
}

fn io_error(error: reqwest::Error) -> TransportError {
    // Endpoint queries can contain secrets; reqwest's default Display includes
    // the URL, which must not enter model errors or durable logs.
    TransportError::Io(error.without_url().to_string())
}

#[cfg(test)]
mod tests;
