//! Feature-gated facade. `core` is always available; every product-level plane
//! is an opt-in dependency.

pub use agent_harness_core as core;

#[cfg(feature = "app-server")]
pub use agent_harness_app_server as app_server;
#[cfg(feature = "context-fs")]
pub use agent_harness_context_fs as context_fs;
#[cfg(feature = "process-executor")]
pub use agent_harness_executor_process as process_executor;
#[cfg(feature = "memory")]
pub use agent_harness_memory as memory;
#[cfg(feature = "provider-openai")]
pub use agent_harness_provider_openai as provider_openai;
#[cfg(feature = "task-ledger")]
pub use agent_harness_task_ledger as task_ledger;
#[cfg(feature = "tui")]
pub use agent_harness_tui as tui;
