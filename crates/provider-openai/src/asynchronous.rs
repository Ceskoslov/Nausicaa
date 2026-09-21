//! Async adapter path; no executor or provider SDK reaches the core crate.

use std::time::Instant;

use super::*;

struct Progress<'a> {
    request: &'a ModelRequest,
    control: &'a ModelControl,
    start: Instant,
    finished: bool,
}

impl Progress<'_> {
    fn finish(&mut self, mut timings: RequestTimings, outcome: &str) {
        timings.total_ms = self.start.elapsed().as_millis() as u64;
        self.finished = true;
        self.control.notify(
            self.request,
            ModelProgress::Finished {
                timings,
                outcome: outcome.into(),
            },
        );
    }
}
impl Drop for Progress<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // Dropping a future cannot await cleanup. The transport future owns
            // its I/O and is dropped too; unavailable phase timings stay unknown.
            self.finish(RequestTimings::default(), "cancelled");
        }
    }
}

pub(super) async fn complete(
    adapter: &OpenAiCompatibleAdapter,
    transport: &dyn AsyncHttpTransport,
    request: ModelRequest,
    control: ModelControl,
) -> Result<ModelResponse, ModelError> {
    let mut progress = Progress {
        request: &request,
        control: &control,
        start: Instant::now(),
        finished: false,
    };
    control.notify(&request, ModelProgress::Started);
    let http = HttpControl {
        cancellation: control.cancellation.clone(),
        abandoned: CancellationToken::new(),
    };
    let mut timings = RequestTimings::default();
    let mut first_text = None;
    let start = progress.start;
    let mut on_text = |text| {
        first_text.get_or_insert(start.elapsed().as_millis() as u64);
        control.notify(&request, ModelProgress::TextDelta { text });
    };
    let result = async {
        http.check()
            .map_err(|e| ModelError::new(e.to_string(), false))?;
        let response = transport
            .post_json_controlled(
                adapter.http_request(&request)?,
                &http,
                &mut on_text,
                &mut timings,
            )
            .await
            .map_err(|error| {
                let retryable = !http.is_cancelled()
                    && match &error {
                        TransportError::Io(_) => true,
                        TransportError::Http { status, .. } => *status == 429 || *status >= 500,
                        TransportError::Protocol(_) => false,
                    };
                ModelError::new(error.to_string(), retryable)
            })?;
        http.check()
            .map_err(|e| ModelError::new(e.to_string(), false))?;
        parse_response(response.body)
    }
    .await;
    timings.first_text_ms = first_text;
    let outcome = if http.is_cancelled() {
        "cancelled"
    } else if result.is_ok() {
        "completed"
    } else {
        "failed"
    };
    progress.finish(timings, outcome);
    result
}
