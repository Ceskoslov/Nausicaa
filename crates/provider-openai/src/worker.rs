use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use super::*;

type ResultSlot = (Option<Result<ModelResponse, ModelError>>, Option<Waker>);

pub(super) struct RequestFuture {
    state: Arc<Mutex<ResultSlot>>,
    abandoned: CancellationToken,
}
impl Future for RequestFuture {
    type Output = Result<ModelResponse, ModelError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(result) = state.0.take() {
            Poll::Ready(result)
        } else {
            state.1 = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}
impl Drop for RequestFuture {
    fn drop(&mut self) {
        self.abandoned.cancel();
    }
}

pub(super) fn start(
    adapter: OpenAiCompatibleAdapter,
    request: ModelRequest,
    control: ModelControl,
) -> RequestFuture {
    let state = Arc::new(Mutex::new((None, None::<Waker>)));
    let http = HttpControl {
        cancellation: control.cancellation.clone(),
        abandoned: CancellationToken::new(),
    };
    let future = RequestFuture {
        state: state.clone(),
        abandoned: http.abandoned.clone(),
    };
    let completed = state.clone();
    let spawned = std::thread::Builder::new()
        .name("model-http".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let start = Instant::now();
                control.notify(&request, ModelProgress::Started);
                let mut timings = RequestTimings::default();
                let mut first_text = None;
                let mut on_text = |text: String| {
                    first_text.get_or_insert(start.elapsed().as_millis() as u64);
                    control.notify(&request, ModelProgress::TextDelta { text });
                };
                let result = adapter.complete_blocking(&request, &http, &mut on_text, &mut timings);
                timings.first_text_ms = first_text;
                timings.total_ms = start.elapsed().as_millis() as u64;
                let outcome = if http.is_cancelled() {
                    "cancelled"
                } else if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                };
                control.notify(
                    &request,
                    ModelProgress::Finished {
                        timings,
                        outcome: outcome.into(),
                    },
                );
                result
            }))
            .unwrap_or_else(|_| Err(ModelError::new("model request worker panicked", false)));
            let mut state = completed.lock().unwrap_or_else(|e| e.into_inner());
            state.0 = Some(result);
            if let Some(waker) = state.1.take() {
                waker.wake();
            }
        });
    if let Err(error) = spawned {
        state.lock().unwrap_or_else(|e| e.into_inner()).0 =
            Some(Err(ModelError::new(error.to_string(), false)));
    }
    future
}
