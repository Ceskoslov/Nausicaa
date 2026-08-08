//! Optional execution-plane tools.
//!
//! `BubblewrapRunner` provides an OS boundary on Linux. `LocalProcessRunner`
//! and workspace path checks are explicitly not sandboxes.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use agent_harness_core::{
    BoxFuture, CanonicalAction, EffectKind, PreparedToolCall, RetrySafety, Tool, ToolCall,
    ToolDefinition, ToolError, ToolExecutionContext, ToolOutput, ToolRegistry,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ProcessError {
    #[error("process I/O error: {0}")]
    Io(String),
    #[error("invalid process request: {0}")]
    Invalid(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProcessRequest {
    pub command: String,
    pub working_directory: PathBuf,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProcessOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

pub trait ProcessRunner: Send + Sync {
    fn run(&self, request: &ProcessRequest) -> Result<ProcessOutput, ProcessError>;
}

/// Unsandboxed runner. It must be selected explicitly.
#[derive(Clone, Debug)]
pub struct LocalProcessRunner {
    maximum_output_bytes: usize,
}

impl LocalProcessRunner {
    #[must_use]
    pub fn new(maximum_output_bytes: usize) -> Self {
        Self {
            maximum_output_bytes,
        }
    }
}

impl ProcessRunner for LocalProcessRunner {
    fn run(&self, request: &ProcessRequest) -> Result<ProcessOutput, ProcessError> {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-lc")
            .arg(&request.command)
            .current_dir(&request.working_directory);
        run_command(command, request.timeout_ms, self.maximum_output_bytes)
    }
}

/// Linux bubblewrap runner with no network and no home directory mounted by
/// default. The workspace is the only writable host bind.
#[derive(Clone, Debug)]
pub struct BubblewrapRunner {
    binary: PathBuf,
    workspace: PathBuf,
    maximum_output_bytes: usize,
    allow_network: bool,
    additional_read_only_binds: Vec<PathBuf>,
}

impl BubblewrapRunner {
    pub fn new(
        workspace: impl AsRef<Path>,
        maximum_output_bytes: usize,
    ) -> Result<Self, ProcessError> {
        let workspace = canonical_directory(workspace.as_ref())?;
        Ok(Self {
            binary: PathBuf::from("bwrap"),
            workspace,
            maximum_output_bytes,
            allow_network: false,
            additional_read_only_binds: Vec::new(),
        })
    }

    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = binary.into();
        self
    }

    #[must_use]
    pub fn with_network(mut self, allow: bool) -> Self {
        self.allow_network = allow;
        self
    }

    #[must_use]
    pub fn with_read_only_bind(mut self, path: impl Into<PathBuf>) -> Self {
        self.additional_read_only_binds.push(path.into());
        self
    }
}

impl ProcessRunner for BubblewrapRunner {
    fn run(&self, request: &ProcessRequest) -> Result<ProcessOutput, ProcessError> {
        let cwd = request.working_directory.canonicalize().map_err(io_error)?;
        if !cwd.starts_with(&self.workspace) {
            return Err(ProcessError::Invalid(format!(
                "working directory `{}` is outside sandbox workspace `{}`",
                cwd.display(),
                self.workspace.display()
            )));
        }
        let mut command = Command::new(&self.binary);
        command
            .arg("--die-with-parent")
            .arg("--new-session")
            .arg("--unshare-all");
        if self.allow_network {
            command.arg("--share-net");
        }
        for system_path in ["/usr", "/bin", "/lib", "/lib64"] {
            let path = Path::new(system_path);
            if path.exists() {
                command.arg("--ro-bind").arg(path).arg(path);
            }
        }
        for path in &self.additional_read_only_binds {
            let canonical = path.canonicalize().map_err(io_error)?;
            command.arg("--ro-bind").arg(&canonical).arg(&canonical);
        }
        command
            .arg("--proc")
            .arg("/proc")
            .arg("--dev")
            .arg("/dev")
            .arg("--tmpfs")
            .arg("/tmp")
            .arg("--bind")
            .arg(&self.workspace)
            .arg(&self.workspace)
            .arg("--chdir")
            .arg(&cwd)
            .arg("/bin/sh")
            .arg("-lc")
            .arg(&request.command)
            .env_clear();
        run_command(command, request.timeout_ms, self.maximum_output_bytes)
    }
}

#[derive(Clone)]
pub struct ShellTool {
    workspace: PathBuf,
    runner: Arc<dyn ProcessRunner>,
    maximum_timeout_ms: u64,
}

impl ShellTool {
    pub fn new(
        workspace: impl AsRef<Path>,
        runner: Arc<dyn ProcessRunner>,
    ) -> Result<Self, ProcessError> {
        Ok(Self {
            workspace: canonical_directory(workspace.as_ref())?,
            runner,
            maximum_timeout_ms: 120_000,
        })
    }

    #[must_use]
    pub fn with_maximum_timeout_ms(mut self, maximum: u64) -> Self {
        self.maximum_timeout_ms = maximum.max(1);
        self
    }
}

impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "shell",
            "Run a shell command in the configured execution backend",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 1 }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        )
    }

    fn prepare(
        &self,
        call: &ToolCall,
        _context: &ToolExecutionContext,
    ) -> Result<PreparedToolCall, ToolError> {
        let command = string_argument(&call.arguments, "command")?;
        if command.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "`command` cannot be empty".to_owned(),
            ));
        }
        let timeout_ms = call
            .arguments
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000)
            .clamp(1, self.maximum_timeout_ms);
        Ok(PreparedToolCall {
            call_id: call.id.clone(),
            action: CanonicalAction::new(
                "shell",
                json!({
                    "command": command,
                    "working_directory": self.workspace,
                    "timeout_ms": timeout_ms
                }),
                EffectKind::WorkspaceWrite,
                RetrySafety::Unsafe,
            )
            .in_scope(self.workspace.display().to_string()),
        })
    }

    fn execute<'a>(
        &'a self,
        prepared: PreparedToolCall,
        context: ToolExecutionContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            if context.cancellation.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let request: ProcessRequest = serde_json::from_value(prepared.action.arguments)
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            let output = self
                .runner
                .run(&request)
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            Ok(ToolOutput::new(serde_json::to_value(output).map_err(
                |error| ToolError::Execution(error.to_string()),
            )?))
        })
    }
}

#[derive(Clone, Debug)]
pub struct ReadFileTool {
    workspace: PathBuf,
    maximum_bytes: usize,
}

impl ReadFileTool {
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, ProcessError> {
        Ok(Self {
            workspace: canonical_directory(workspace.as_ref())?,
            maximum_bytes: 1_048_576,
        })
    }

    #[must_use]
    pub fn with_maximum_bytes(mut self, maximum: usize) -> Self {
        self.maximum_bytes = maximum.max(1);
        self
    }
}

impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "read_file",
            "Read a UTF-8 file inside the workspace",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    fn prepare(
        &self,
        call: &ToolCall,
        _context: &ToolExecutionContext,
    ) -> Result<PreparedToolCall, ToolError> {
        let requested = string_argument(&call.arguments, "path")?;
        let path = resolve_existing_file(&self.workspace, Path::new(requested))?;
        Ok(PreparedToolCall {
            call_id: call.id.clone(),
            action: CanonicalAction::new(
                "read_file",
                json!({ "path": path, "maximum_bytes": self.maximum_bytes }),
                EffectKind::ReadOnly,
                RetrySafety::Safe,
            )
            .in_scope(self.workspace.display().to_string()),
        })
    }

    fn execute<'a>(
        &'a self,
        prepared: PreparedToolCall,
        context: ToolExecutionContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            if context.cancellation.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let path = path_argument(&prepared.action.arguments, "path")?;
            let maximum = prepared
                .action
                .arguments
                .get("maximum_bytes")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| ToolError::Execution("invalid maximum_bytes".to_owned()))?;
            let mut bytes = Vec::new();
            File::open(&path)
                .map_err(|error| ToolError::Execution(error.to_string()))?
                .take(maximum.saturating_add(1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            if bytes.len() > maximum {
                return Err(ToolError::Execution(format!(
                    "file exceeds {maximum} byte limit"
                )));
            }
            let content = String::from_utf8(bytes)
                .map_err(|_| ToolError::Execution("file is not valid UTF-8".to_owned()))?;
            Ok(ToolOutput::new(json!({ "path": path, "content": content })))
        })
    }
}

#[derive(Clone, Debug)]
pub struct WriteFileTool {
    workspace: PathBuf,
    maximum_bytes: usize,
}

impl WriteFileTool {
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, ProcessError> {
        Ok(Self {
            workspace: canonical_directory(workspace.as_ref())?,
            maximum_bytes: 1_048_576,
        })
    }

    #[must_use]
    pub fn with_maximum_bytes(mut self, maximum: usize) -> Self {
        self.maximum_bytes = maximum.max(1);
        self
    }
}

impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "write_file",
            "Write a UTF-8 file inside the workspace",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "create_directories": { "type": "boolean" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        )
    }

    fn prepare(
        &self,
        call: &ToolCall,
        _context: &ToolExecutionContext,
    ) -> Result<PreparedToolCall, ToolError> {
        let requested = string_argument(&call.arguments, "path")?;
        let content = string_argument(&call.arguments, "content")?;
        if content.len() > self.maximum_bytes {
            return Err(ToolError::InvalidArguments(format!(
                "content exceeds {} byte limit",
                self.maximum_bytes
            )));
        }
        let path = resolve_write_target(&self.workspace, Path::new(requested))?;
        let create_directories = call
            .arguments
            .get("create_directories")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(PreparedToolCall {
            call_id: call.id.clone(),
            action: CanonicalAction::new(
                "write_file",
                json!({
                    "path": path,
                    "content": content,
                    "create_directories": create_directories
                }),
                EffectKind::WorkspaceWrite,
                RetrySafety::Idempotent,
            )
            .in_scope(self.workspace.display().to_string()),
        })
    }

    fn execute<'a>(
        &'a self,
        prepared: PreparedToolCall,
        context: ToolExecutionContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            if context.cancellation.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let path = path_argument(&prepared.action.arguments, "path")?;
            let content = string_argument(&prepared.action.arguments, "content")?;
            let create_directories = prepared
                .action
                .arguments
                .get("create_directories")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            verify_write_target(&self.workspace, &path)?;
            let parent = path
                .parent()
                .ok_or_else(|| ToolError::Execution("write target has no parent".to_owned()))?;
            if create_directories {
                fs::create_dir_all(parent)
                    .map_err(|error| ToolError::Execution(error.to_string()))?;
                verify_write_target(&self.workspace, &path)?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            file.write_all(content.as_bytes())
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            file.sync_data()
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            Ok(ToolOutput::new(
                json!({ "path": path, "bytes_written": content.len() }),
            ))
        })
    }
}

pub fn register_workspace_tools(
    registry: &mut ToolRegistry,
    workspace: impl AsRef<Path>,
    runner: Arc<dyn ProcessRunner>,
) -> Result<(), ProcessError> {
    registry
        .register(ReadFileTool::new(&workspace)?)
        .map_err(|error| ProcessError::Invalid(error.to_string()))?;
    registry
        .register(WriteFileTool::new(&workspace)?)
        .map_err(|error| ProcessError::Invalid(error.to_string()))?;
    registry
        .register(ShellTool::new(workspace, runner)?)
        .map_err(|error| ProcessError::Invalid(error.to_string()))?;
    Ok(())
}

fn run_command(
    mut command: Command,
    timeout_ms: u64,
    maximum_output_bytes: usize,
) -> Result<ProcessOutput, ProcessError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(io_error)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProcessError::Io("stdout pipe was not created".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProcessError::Io("stderr pipe was not created".to_owned()))?;
    let stdout_reader = thread::spawn(move || read_capped(stdout, maximum_output_bytes));
    let stderr_reader = thread::spawn(move || read_capped(stderr, maximum_output_bytes));
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().map_err(io_error)? {
            break (status, false);
        }
        if Instant::now() >= deadline {
            child.kill().map_err(io_error)?;
            break (child.wait().map_err(io_error)?, true);
        }
        thread::sleep(Duration::from_millis(10));
    };
    let (stdout, stdout_truncated) = stdout_reader
        .join()
        .map_err(|_| ProcessError::Io("stdout reader panicked".to_owned()))??;
    let (stderr, stderr_truncated) = stderr_reader
        .join()
        .map_err(|_| ProcessError::Io("stderr reader panicked".to_owned()))??;
    Ok(ProcessOutput {
        exit_code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        timed_out,
        stdout_truncated,
        stderr_truncated,
    })
}

fn read_capped(mut reader: impl Read, maximum: usize) -> Result<(Vec<u8>, bool), ProcessError> {
    let mut retained = Vec::with_capacity(maximum.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).map_err(io_error)?;
        if count == 0 {
            break;
        }
        let remaining = maximum.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..count.min(remaining)]);
        truncated |= count > remaining;
    }
    Ok((retained, truncated))
}

fn resolve_existing_file(workspace: &Path, requested: &Path) -> Result<PathBuf, ToolError> {
    validate_relative(requested)?;
    let canonical = workspace
        .join(requested)
        .canonicalize()
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
    if !canonical.starts_with(workspace) || !canonical.is_file() {
        return Err(ToolError::InvalidArguments(
            "path is not a file inside the workspace".to_owned(),
        ));
    }
    Ok(canonical)
}

fn resolve_write_target(workspace: &Path, requested: &Path) -> Result<PathBuf, ToolError> {
    validate_relative(requested)?;
    let target = workspace.join(requested);
    verify_write_target(workspace, &target)?;
    Ok(target)
}

fn verify_write_target(workspace: &Path, target: &Path) -> Result<(), ToolError> {
    if !target.starts_with(workspace) {
        return Err(ToolError::InvalidArguments(
            "write target is outside the workspace".to_owned(),
        ));
    }
    let mut ancestor = target.parent();
    while let Some(path) = ancestor {
        if path.exists() {
            let canonical = path
                .canonicalize()
                .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
            if !canonical.starts_with(workspace) {
                return Err(ToolError::InvalidArguments(
                    "write target traverses a symlink outside the workspace".to_owned(),
                ));
            }
            break;
        }
        ancestor = path.parent();
    }
    if target.exists() {
        let canonical = target
            .canonicalize()
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        if !canonical.starts_with(workspace) {
            return Err(ToolError::InvalidArguments(
                "write target resolves outside the workspace".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<(), ToolError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ToolError::InvalidArguments(
            "path must be a non-empty relative path without `..`".to_owned(),
        ));
    }
    Ok(())
}

fn string_argument<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidArguments(format!("`{name}` must be a string")))
}

fn path_argument(arguments: &Value, name: &str) -> Result<PathBuf, ToolError> {
    string_argument(arguments, name).map(PathBuf::from)
}

fn canonical_directory(path: &Path) -> Result<PathBuf, ProcessError> {
    let canonical = path.canonicalize().map_err(io_error)?;
    if !canonical.is_dir() {
        return Err(ProcessError::Invalid(format!(
            "`{}` is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn io_error(error: std::io::Error) -> ProcessError {
    ProcessError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use agent_harness_core::{CancellationToken, ThreadId, TurnId};

    use super::*;

    #[derive(Default)]
    struct RecordingRunner {
        requests: Mutex<Vec<ProcessRequest>>,
    }

    impl ProcessRunner for RecordingRunner {
        fn run(&self, request: &ProcessRequest) -> Result<ProcessOutput, ProcessError> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(ProcessOutput {
                exit_code: Some(0),
                stdout: "ok".to_owned(),
                stderr: String::new(),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })
        }
    }

    fn context(workspace: &Path) -> ToolExecutionContext {
        ToolExecutionContext {
            thread_id: ThreadId::new(),
            turn_id: TurnId::new(),
            workspace: Some(workspace.to_path_buf()),
            cancellation: CancellationToken::new(),
            metadata: Default::default(),
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, Waker};
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => thread::yield_now(),
            }
        }
    }

    #[test]
    fn file_tools_reject_parent_traversal() {
        let directory = std::env::temp_dir().join(ThreadId::new().as_str());
        fs::create_dir_all(&directory).unwrap();
        let tool = WriteFileTool::new(&directory).unwrap();
        let call = ToolCall::new("write_file", json!({ "path": "../escape", "content": "x" }));
        assert!(tool.prepare(&call, &context(&directory)).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn shell_executes_only_the_prepared_normalized_request() {
        let directory = std::env::temp_dir().join(ThreadId::new().as_str());
        fs::create_dir_all(&directory).unwrap();
        let runner = Arc::new(RecordingRunner::default());
        let tool = ShellTool::new(&directory, runner.clone()).unwrap();
        let call = ToolCall::new("shell", json!({ "command": "pwd", "timeout_ms": 0 }));
        let prepared = tool.prepare(&call, &context(&directory)).unwrap();
        block_on(tool.execute(prepared, context(&directory))).unwrap();

        let requests = runner.requests.lock().unwrap();
        assert_eq!(requests[0].command, "pwd");
        assert_eq!(requests[0].timeout_ms, 1);
        fs::remove_dir_all(directory).unwrap();
    }
}
