//! Replay of the versioned model boundary for explicitly resumable turns.
use std::collections::{BTreeMap, BTreeSet};

use crate::{CallId, EventEnvelope, ReceiptStatus, RuntimeEvent, StopReason, TurnId};

pub(crate) const RESUME_VERSION: u32 = 1;

pub(crate) struct ModelBoundary {
    pub(crate) next_iteration: usize,
    pub(crate) max_iterations: usize,
    pub(crate) seen_call_ids: BTreeSet<CallId>,
    pub(crate) request_started: bool,
    pub(crate) retries: u32,
}

impl ModelBoundary {
    pub(crate) fn initial(max_iterations: usize) -> Self {
        Self {
            next_iteration: 0,
            max_iterations,
            seen_call_ids: BTreeSet::new(),
            request_started: false,
            retries: 0,
        }
    }
}

pub(crate) fn model_boundary(
    events: &[EventEnvelope],
    turn_id: &TurnId,
) -> Result<ModelBoundary, &'static str> {
    let start = events
        .iter()
        .rposition(|e| matches!(e.event, RuntimeEvent::TurnStarted))
        .ok_or("thread has no turn")?;
    let active = &events[start..];
    if active.iter().any(|e| e.turn_id.as_ref() != Some(turn_id)) {
        return Err("target is not the latest complete turn history");
    }
    let [
        _,
        EventEnvelope {
            event: RuntimeEvent::UserMessage { .. },
            ..
        },
        EventEnvelope {
            event:
                RuntimeEvent::TurnResumable {
                    version: RESUME_VERSION,
                    max_model_iterations,
                },
            ..
        },
        tail @ ..,
    ] = active
    else {
        return Err("turn lacks supported resumable metadata or durable input");
    };
    let mut boundary = ModelBoundary::initial(*max_model_iterations);
    let mut outstanding = BTreeMap::<&CallId, &str>::new();

    for envelope in tail {
        match &envelope.event {
            RuntimeEvent::ContextCompiled { iteration, .. }
            | RuntimeEvent::TurnResumed { iteration }
                if *iteration == boundary.next_iteration && outstanding.is_empty() => {}
            RuntimeEvent::ModelRequestStarted { iteration }
                if *iteration == boundary.next_iteration
                    && outstanding.is_empty()
                    && !boundary.request_started =>
            {
                boundary.request_started = true;
            }
            RuntimeEvent::ModelRequestRetried { iteration, retry }
                if *iteration == boundary.next_iteration
                    && boundary.request_started
                    && boundary.retries.checked_add(1) == Some(*retry) =>
            {
                boundary.retries = *retry;
            }
            RuntimeEvent::AssistantMessage {
                tool_calls,
                stop_reason: StopReason::ToolUse,
                ..
            } if boundary.request_started && !tool_calls.is_empty() => {
                for call in tool_calls {
                    if call.id.as_str().trim().is_empty()
                        || !boundary.seen_call_ids.insert(call.id.clone())
                    {
                        return Err("assistant history has invalid or repeated call IDs");
                    }
                    outstanding.insert(&call.id, &call.name);
                }
                boundary.next_iteration += 1;
                boundary.request_started = false;
                boundary.retries = 0;
            }
            RuntimeEvent::ToolPrepared { prepared }
            | RuntimeEvent::ToolExecutionStarted { prepared }
                if outstanding.contains_key(&prepared.call_id) => {}
            RuntimeEvent::ApprovalRequested { request }
                if outstanding.contains_key(&request.call_id) => {}
            RuntimeEvent::ApprovalResolved { call_id, .. } if outstanding.contains_key(call_id) => {
            }
            RuntimeEvent::ToolReceiptRecorded { receipt } => {
                if receipt.status == ReceiptStatus::Unknown {
                    return Err("tool execution has an unknown outcome");
                }
                if outstanding.remove(&receipt.call_id) != Some(receipt.tool_name.as_str()) {
                    return Err("tool receipt does not match an outstanding call");
                }
            }
            RuntimeEvent::TurnCompleted { .. }
            | RuntimeEvent::TurnCancelled
            | RuntimeEvent::TurnFailed { .. } => {
                return Err("turn is terminal");
            }
            _ => return Err("history does not identify a safe model boundary"),
        }
    }

    if !outstanding.is_empty() {
        return Err("accepted tool calls still lack durable receipts; recover the turn first");
    }
    Ok(boundary)
}
