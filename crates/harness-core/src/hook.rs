use std::sync::Arc;

use thiserror::Error;

use crate::context::CompiledContext;
use crate::id::{ThreadId, TurnId};
use crate::protocol::{PreparedToolCall, ToolReceipt};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookContext {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub iteration: usize,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("hook `{hook}` rejected {point}: {reason}")]
pub struct HookError {
    pub hook: String,
    pub point: String,
    pub reason: String,
}

/// Deterministic shell around model and tool actions. Hooks may reject or halt,
/// but may not mutate an already canonicalized action.
pub trait Hook: Send + Sync {
    fn name(&self) -> &str;

    fn before_model(
        &self,
        _context: &HookContext,
        _compiled: &CompiledContext,
    ) -> Result<(), String> {
        Ok(())
    }

    fn before_tool(
        &self,
        _context: &HookContext,
        _prepared: &PreparedToolCall,
    ) -> Result<(), String> {
        Ok(())
    }

    fn after_tool(&self, _context: &HookContext, _receipt: &ToolReceipt) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct HookSet {
    hooks: Vec<Arc<dyn Hook>>,
}

impl HookSet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with<H>(mut self, hook: H) -> Self
    where
        H: Hook + 'static,
    {
        self.hooks.push(Arc::new(hook));
        self
    }

    pub fn push_arc(&mut self, hook: Arc<dyn Hook>) {
        self.hooks.push(hook);
    }

    pub(crate) fn before_model(
        &self,
        context: &HookContext,
        compiled: &CompiledContext,
    ) -> Result<(), HookError> {
        for hook in &self.hooks {
            hook.before_model(context, compiled)
                .map_err(|reason| HookError {
                    hook: hook.name().to_owned(),
                    point: "before_model".to_owned(),
                    reason,
                })?;
        }
        Ok(())
    }

    pub(crate) fn before_tool(
        &self,
        context: &HookContext,
        prepared: &PreparedToolCall,
    ) -> Result<(), HookError> {
        for hook in &self.hooks {
            hook.before_tool(context, prepared)
                .map_err(|reason| HookError {
                    hook: hook.name().to_owned(),
                    point: "before_tool".to_owned(),
                    reason,
                })?;
        }
        Ok(())
    }

    pub(crate) fn after_tool(
        &self,
        context: &HookContext,
        receipt: &ToolReceipt,
    ) -> Result<(), HookError> {
        for hook in &self.hooks {
            hook.after_tool(context, receipt)
                .map_err(|reason| HookError {
                    hook: hook.name().to_owned(),
                    point: "after_tool".to_owned(),
                    reason,
                })?;
        }
        Ok(())
    }
}
