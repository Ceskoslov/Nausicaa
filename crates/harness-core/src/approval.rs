use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::id::{CallId, ThreadId, TurnId};
use crate::model::BoxFuture;
use crate::protocol::CanonicalAction;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub call_id: CallId,
    pub action: CanonicalAction,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved { action: CanonicalAction },
    Denied { reason: String },
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("approval provider failed: {0}")]
pub struct ApprovalError(pub String);

pub trait ApprovalProvider: Send + Sync {
    fn request<'a>(
        &'a self,
        request: ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, ApprovalError>>;
}

/// Fail-closed provider used when a runtime has no interactive control plane.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAllApprovals;

impl ApprovalProvider for DenyAllApprovals {
    fn request<'a>(
        &'a self,
        _request: ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, ApprovalError>> {
        Box::pin(async {
            Ok(ApprovalDecision::Denied {
                reason: "no approval provider is configured".to_owned(),
            })
        })
    }
}
