use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::event::RuntimeEvent;
use crate::id::{CallId, ThreadId, TurnId};
use crate::protocol::{PreparedToolCall, ToolCall, ToolReceipt};
use crate::store::{EventStore, StoreError};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub receipts_recorded: Vec<ToolReceipt>,
    pub turns_marked_failed: Vec<TurnId>,
}

/// Closes interrupted protocol edges without replaying side effects.
///
/// Any call with a durable `ToolExecutionStarted` but no durable receipt is
/// recorded as `unknown`. A prepared-but-not-started call is known not to have
/// crossed the executor boundary and is recorded as failed. Every interrupted
/// turn is then terminally marked failed.
pub fn recover_thread(
    store: &dyn EventStore,
    thread_id: &ThreadId,
) -> Result<RecoveryReport, StoreError> {
    let events = store.load_thread(thread_id)?;
    let mut open_turns = BTreeSet::<TurnId>::new();
    let mut calls = BTreeMap::<(TurnId, CallId), ToolCall>::new();
    let mut prepared = BTreeMap::<(TurnId, CallId), PreparedToolCall>::new();
    let mut started = BTreeMap::<(TurnId, CallId), PreparedToolCall>::new();
    let mut completed = BTreeSet::<(TurnId, CallId)>::new();

    for envelope in &events {
        let Some(turn_id) = envelope.turn_id.clone() else {
            continue;
        };
        match &envelope.event {
            RuntimeEvent::TurnStarted => {
                open_turns.insert(turn_id);
            }
            RuntimeEvent::AssistantMessage { tool_calls, .. } => {
                for call in tool_calls {
                    calls.insert((turn_id.clone(), call.id.clone()), call.clone());
                }
            }
            RuntimeEvent::ToolPrepared { prepared: action } => {
                prepared.insert((turn_id, action.call_id.clone()), action.clone());
            }
            RuntimeEvent::ToolExecutionStarted { prepared: action } => {
                started.insert((turn_id, action.call_id.clone()), action.clone());
            }
            RuntimeEvent::ToolReceiptRecorded { receipt } => {
                completed.insert((turn_id, receipt.call_id.clone()));
            }
            RuntimeEvent::TurnCompleted { .. }
            | RuntimeEvent::TurnFailed { .. }
            | RuntimeEvent::TurnCancelled => {
                open_turns.remove(&turn_id);
            }
            _ => {}
        }
    }

    let mut keys = BTreeSet::new();
    keys.extend(calls.keys().cloned());
    keys.extend(prepared.keys().cloned());
    keys.extend(started.keys().cloned());

    let mut report = RecoveryReport::default();
    for (turn_id, call_id) in keys {
        let key = (turn_id.clone(), call_id);
        if completed.contains(&key) {
            continue;
        }
        let receipt = if let Some(action) = started.get(&key) {
            ToolReceipt::unknown(
                action.clone(),
                "execution started but no durable receipt was found during recovery",
            )
        } else if let Some(action) = prepared.get(&key) {
            ToolReceipt::failed(
                action.clone(),
                "turn was interrupted before execution began",
            )
        } else if let Some(call) = calls.get(&key) {
            ToolReceipt::malformed(call, "turn was interrupted before tool preparation")
        } else {
            // The key came from one of the maps above, so this is unreachable.
            continue;
        };
        store.append(
            thread_id.clone(),
            Some(turn_id.clone()),
            RuntimeEvent::ToolReceiptRecorded {
                receipt: receipt.clone(),
            },
        )?;
        report.receipts_recorded.push(receipt);
    }

    for turn_id in open_turns {
        store.append(
            thread_id.clone(),
            Some(turn_id.clone()),
            RuntimeEvent::TurnFailed {
                error: "turn was interrupted and recovered without replaying actions".to_owned(),
            },
        )?;
        report.turns_marked_failed.push(turn_id);
    }
    Ok(report)
}
