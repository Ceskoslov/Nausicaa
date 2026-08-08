use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::control::CancellationToken;
use crate::id::{ThreadId, TurnId};
use crate::model::BoxFuture;
use crate::protocol::{PreparedToolCall, ToolCall};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDefinition {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolExecutionContext {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub workspace: Option<PathBuf>,
    pub cancellation: CancellationToken,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub value: Value,
}

impl ToolOutput {
    #[must_use]
    pub fn new(value: Value) -> Self {
        Self { value }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ToolError {
    #[error("invalid tool arguments: {0}")]
    InvalidArguments(String),
    #[error("tool execution failed: {0}")]
    Execution(String),
    #[error("tool executor is unavailable: {0}")]
    ExecutorUnavailable(String),
    #[error("tool execution was cancelled")]
    Cancelled,
}

/// A tool has two deliberately separate phases:
///
/// 1. `prepare` normalizes the model's request into the exact action that can be
///    inspected by policy, hooks, audit logs, and a human approver.
/// 2. `execute` receives only that prepared action.
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn prepare(
        &self,
        call: &ToolCall,
        context: &ToolExecutionContext,
    ) -> Result<PreparedToolCall, ToolError>;

    fn execute<'a>(
        &'a self,
        prepared: PreparedToolCall,
        context: ToolExecutionContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>>;
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RegistryError {
    #[error("tool name cannot be empty")]
    EmptyName,
    #[error("tool `{0}` is already registered")]
    Duplicate(String),
}

#[derive(Clone)]
struct RegisteredTool {
    definition: ToolDefinition,
    implementation: Arc<dyn Tool>,
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, RegisteredTool>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T>(&mut self, tool: T) -> Result<(), RegistryError>
    where
        T: Tool + 'static,
    {
        self.register_arc(Arc::new(tool))
    }

    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) -> Result<(), RegistryError> {
        let definition = tool.definition();
        if definition.name.trim().is_empty() {
            return Err(RegistryError::EmptyName);
        }
        if self.tools.contains_key(&definition.name) {
            return Err(RegistryError::Duplicate(definition.name));
        }
        self.tools.insert(
            definition.name.clone(),
            RegisteredTool {
                definition,
                implementation: tool,
            },
        );
        Ok(())
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .get(name)
            .map(|registered| registered.implementation.clone())
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    #[must_use]
    pub fn definitions_for<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Vec<ToolDefinition> {
        names
            .into_iter()
            .filter_map(|name| self.tools.get(name))
            .map(|registered| registered.definition.clone())
            .collect()
    }
}
