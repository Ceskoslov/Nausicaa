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
    BoxFuture, CancellationToken, CanonicalAction, EffectKind, PreparedToolCall, RetrySafety, Tool,
    ToolCall, ToolDefinition, ToolError, ToolExecutionContext, ToolOutput, ToolRegistry,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ProcessError {
    #[error("process cancelled; effects before termination may have occurred")]
    Cancelled,
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

    /// Override to interrupt a running process. Legacy runners only observe
    /// cancellation around their blocking call.
    fn run_controlled(
        &self,
        request: &ProcessRequest,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessError> {
        if cancellation.is_cancelled() {
            return Err(ProcessError::Cancelled);
        }
        let output = self.run(request);
        if cancellation.is_cancelled() {
            return Err(ProcessError::Cancelled);
        }
        output
    }
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
        self.run_controlled(request, &CancellationToken::new())
    }
    fn run_controlled(
        &self,
        request: &ProcessRequest,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessError> {
        if cancellation.is_cancelled() {
            return Err(ProcessError::Cancelled);
        }
        let mut command = Command::new("/bin/sh");
        command
            .arg("-lc")
            .arg(&request.command)
            .current_dir(&request.working_directory);
        run_command(
            command,
            request.timeout_ms,
            self.maximum_output_bytes,
            cancellation,
        )
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
        self.run_controlled(request, &CancellationToken::new())
    }
    fn run_controlled(
        &self,
        request: &ProcessRequest,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessError> {
        if cancellation.is_cancelled() {
            return Err(ProcessError::Cancelled);
        }
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
        run_command(
            command,
            request.timeout_ms,
            self.maximum_output_bytes,
            cancellation,
        )
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
                .run_controlled(&request, &context.cancellation)
                .map_err(|error| match error {
                    ProcessError::Cancelled => ToolError::OutcomeUnknown(error.to_string()),
                    _ => ToolError::Execution(error.to_string()),
                })?;
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

// Pipe draining must not outlive the command indefinitely, even if a descendant
// escapes the process group and retains an inherited stdout/stderr descriptor.
const OUTPUT_CLEANUP_TIMEOUT: Duration = Duration::from_millis(250);

#[cfg(unix)]
fn run_command(
    mut command: Command,
    timeout_ms: u64,
    maximum_output_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<ProcessOutput, ProcessError> {
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    use std::os::unix::process::CommandExt;

    command.process_group(0);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(io_error)?;
    let result = capture_process(&mut child, timeout_ms, maximum_output_bytes, cancellation);
    if result.is_err() {
        let _ = kill_process_group(&child);
        let _ = child.kill();
        // A process stuck in kernel I/O may not reap promptly after SIGKILL.
        // Keep cleanup off the caller's critical path in that exceptional case.
        thread::spawn(move || {
            let _ = child.wait();
        });
    }
    if cancellation.is_cancelled() {
        Err(ProcessError::Cancelled)
    } else {
        result
    }
}

#[cfg(unix)]
fn kill_process_group(child: &std::process::Child) -> Result<(), ProcessError> {
    let pid = i32::try_from(child.id()).map_err(|error| ProcessError::Io(error.to_string()))?;
    // SAFETY: process_group(0) creates a new group whose positive ID is the
    // spawned child's PID. A negative PID targets that group, not our own.
    let result = unsafe { libc::kill(-pid, libc::SIGKILL) };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(io_error(error));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn child_has_exited(child: &std::process::Child) -> Result<bool, ProcessError> {
    // Keep the exited child waitable until group cleanup is done. Reaping it
    // before kill(-pid) could allow its PID to be reused for an unrelated group.
    // SAFETY: zero initializes siginfo_t, and waitid writes to this live value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(io_error(error));
    }
    // SAFETY: successful waitid provides SIGCHLD fields, or leaves the
    // initialized PID at zero when WNOHANG finds no exited child.
    Ok(unsafe { info.si_pid() } != 0)
}

#[cfg(unix)]
struct CapturedPipe<R> {
    reader: R,
    retained: Vec<u8>,
    maximum: usize,
    truncated: bool,
    eof: bool,
}

#[cfg(unix)]
impl<R: Read + std::os::fd::AsRawFd> CapturedPipe<R> {
    fn new(reader: R, maximum: usize) -> Result<Self, ProcessError> {
        let fd = reader.as_raw_fd();
        // SAFETY: reader owns a live descriptor for the duration of both calls.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        Ok(Self {
            reader,
            retained: Vec::with_capacity(maximum.min(8192)),
            maximum,
            truncated: false,
            eof: false,
        })
    }

    fn drain(&mut self) -> Result<(), ProcessError> {
        if self.eof {
            return Ok(());
        }
        let mut buffer = [0_u8; 8192];
        // Bound each batch so continuous output cannot starve deadline checks
        // or the other pipe. Keep discarding excess bytes until EOF.
        for _ in 0..32 {
            match self.reader.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(count) => {
                    let remaining = self.maximum.saturating_sub(self.retained.len());
                    self.retained
                        .extend_from_slice(&buffer[..count.min(remaining)]);
                    self.truncated |= count > remaining;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_error(error)),
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn capture_process(
    child: &mut std::process::Child,
    timeout_ms: u64,
    maximum: usize,
    cancellation: &CancellationToken,
) -> Result<ProcessOutput, ProcessError> {
    let mut stdout = CapturedPipe::new(
        child
            .stdout
            .take()
            .ok_or_else(|| ProcessError::Io("stdout pipe was not created".into()))?,
        maximum,
    )?;
    let mut stderr = CapturedPipe::new(
        child
            .stderr
            .take()
            .ok_or_else(|| ProcessError::Io("stderr pipe was not created".into()))?,
        maximum,
    )?;
    let start = Instant::now();
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let mut exited = false;
    let mut cleanup_started = None;
    let mut timed_out = false;
    let mut cancelled = false;
    loop {
        stdout.drain()?;
        stderr.drain()?;
        if !exited {
            exited = child_has_exited(child)?;
        }
        if cleanup_started.is_none()
            && (exited || cancellation.is_cancelled() || start.elapsed() >= timeout)
        {
            cancelled = cancellation.is_cancelled();
            timed_out = !exited && !cancelled;
            // This runner owns foreground command groups. Background work must
            // use a separate managed execution rather than orphaning children.
            kill_process_group(child)?;
            cleanup_started = Some(Instant::now());
        }
        if exited && stdout.eof && stderr.eof {
            break;
        }
        if cleanup_started.is_some_and(|at: Instant| at.elapsed() >= OUTPUT_CLEANUP_TIMEOUT) {
            if !exited {
                return Err(ProcessError::Io(
                    "process did not exit within the cleanup deadline".into(),
                ));
            }
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    // Return cancellation before reaping: run_command's error cleanup retains
    // the waitable leader until group signalling is complete, avoiding PID reuse.
    if cancelled {
        return Err(ProcessError::Cancelled);
    }
    let status = child.wait().map_err(io_error)?;
    Ok(ProcessOutput {
        exit_code: status.code(),
        stdout: String::from_utf8_lossy(&stdout.retained).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.retained).into_owned(),
        timed_out,
        stdout_truncated: stdout.truncated || !stdout.eof,
        stderr_truncated: stderr.truncated || !stderr.eof,
    })
}

#[cfg(not(unix))]
fn run_command(
    mut command: Command,
    timeout_ms: u64,
    maximum_output_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<ProcessOutput, ProcessError> {
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
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
        if cancellation.is_cancelled() {
            let _ = child.kill();
            thread::spawn(move || {
                let _ = child.wait();
            });
            return Err(ProcessError::Cancelled);
        }
        if Instant::now() >= deadline {
            child.kill().map_err(io_error)?;
            break (child.wait().map_err(io_error)?, true);
        }
        thread::sleep(Duration::from_millis(10));
    };
    let cleanup_started = Instant::now();
    while !stdout_reader.is_finished() || !stderr_reader.is_finished() {
        if cleanup_started.elapsed() >= OUTPUT_CLEANUP_TIMEOUT {
            return Err(ProcessError::Io(
                "output pipes did not close within the cleanup deadline".into(),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
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

#[cfg(not(unix))]
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

    #[cfg(unix)]
    fn shell_output(command: &str, timeout_ms: u64, maximum: usize) -> ProcessOutput {
        let mut shell = Command::new("/bin/sh");
        shell.arg("-c").arg(command);
        run_command(shell, timeout_ms, maximum, &CancellationToken::new()).unwrap()
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_stops_quiet_and_noisy_commands_without_waiting_for_timeout() {
        for command in [
            "sleep 5",
            "while :; do printf noise; printf noise >&2; done",
        ] {
            let cancellation = CancellationToken::new();
            let signal = cancellation.clone();
            let canceller = thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                signal.cancel();
            });
            let start = Instant::now();
            let result = LocalProcessRunner::new(32).run_controlled(
                &ProcessRequest {
                    command: command.into(),
                    working_directory: std::env::temp_dir(),
                    timeout_ms: 30_000,
                },
                &cancellation,
            );
            canceller.join().unwrap();
            assert_eq!(result, Err(ProcessError::Cancelled));
            assert!(start.elapsed() < Duration::from_secs(2));
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires bwrap and permitted user namespaces"]
    fn bubblewrap_cancellation_stops_a_running_shell() {
        let directory = std::env::temp_dir().join(ThreadId::new().as_str());
        fs::create_dir_all(&directory).unwrap();
        let runner = BubblewrapRunner::new(&directory, 1024).unwrap();
        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        let ready = directory.join("ready");
        let canceller = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !ready.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            let seen = ready.exists();
            let at = Instant::now();
            signal.cancel();
            (seen, at)
        });
        let result = runner.run_controlled(
            &ProcessRequest {
                command: "touch ready; sleep 5; touch after".into(),
                working_directory: directory.clone(),
                timeout_ms: 30_000,
            },
            &cancellation,
        );
        let (seen, at) = canceller.join().unwrap();
        assert!(seen, "sandbox did not start: {result:?}");
        assert_eq!(result, Err(ProcessError::Cancelled));
        assert!(at.elapsed() < Duration::from_secs(2));
        assert!(!directory.join("after").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn runtime_closes_cancelled_shell_with_unknown_receipt_before_turn_cancelled() {
        use agent_harness_core::{
            Access, AgentRuntime, CapabilityPolicy, DirectExecutor, LayeredContextCompiler,
            MemoryEventStore, ModelAdapter, ModelError, ModelRequest, ModelResponse, ReceiptStatus,
            RuntimeError, RuntimeEvent,
        };
        struct ShellModel;
        impl ModelAdapter for ShellModel {
            fn complete<'a>(
                &'a self,
                _: ModelRequest,
            ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
                Box::pin(async {
                    Ok(ModelResponse::tool_calls(vec![
                        ToolCall::new(
                            "shell",
                            json!({"command":"printf before > before; sleep 5 & echo $! > ready; wait; printf after > after", "timeout_ms":30_000}),
                        ),
                        ToolCall::new("shell", json!({"command":"touch second"})),
                    ]))
                })
            }
        }
        let directory = std::env::temp_dir().join(ThreadId::new().as_str());
        fs::create_dir_all(&directory).unwrap();
        let mut tools = ToolRegistry::new();
        tools
            .register(ShellTool::new(&directory, Arc::new(LocalProcessRunner::new(1024))).unwrap())
            .unwrap();
        let store = Arc::new(MemoryEventStore::new());
        let runtime = AgentRuntime::new(
            Arc::new(ShellModel),
            store.clone(),
            Arc::new(LayeredContextCompiler::default()),
            tools,
            Arc::new(CapabilityPolicy::deny_by_default().grant("shell", Access::Allow)),
        )
        .with_executor(Arc::new(DirectExecutor));
        let thread_id = runtime.start_thread().unwrap();
        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        let ready = directory.join("ready");
        let canceller = thread::spawn(move || {
            let start = Instant::now();
            while !ready.exists() && start.elapsed() < Duration::from_secs(3) {
                thread::sleep(Duration::from_millis(5));
            }
            let existed = ready.exists();
            let at = Instant::now();
            signal.cancel();
            (existed, at)
        });
        let result = block_on(runtime.run_turn_with_cancellation(&thread_id, "run", cancellation));
        let (ready, at) = canceller.join().unwrap();
        assert!(ready);
        assert!(matches!(result, Err(RuntimeError::Cancelled(_))));
        assert!(at.elapsed() < Duration::from_secs(2));
        assert!(directory.join("before").exists());
        assert!(!directory.join("after").exists());
        assert!(!directory.join("second").exists());
        let events = store.all_events().unwrap();
        let receipts = events
            .iter()
            .filter_map(|e| {
                if let RuntimeEvent::ToolReceiptRecorded { receipt } = &e.event {
                    Some(receipt)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0].status, ReceiptStatus::Unknown);
        assert_eq!(receipts[1].status, ReceiptStatus::Denied);
        let start = events
            .iter()
            .position(|e| matches!(e.event, RuntimeEvent::ToolExecutionStarted { .. }))
            .unwrap();
        let receipt = events
            .iter()
            .position(|e| matches!(e.event, RuntimeEvent::ToolReceiptRecorded { .. }))
            .unwrap();
        let cancelled = events
            .iter()
            .position(|e| matches!(e.event, RuntimeEvent::TurnCancelled))
            .unwrap();
        assert!(start < receipt && receipt < cancelled);
        #[cfg(target_os = "linux")]
        {
            let pid = fs::read_to_string(directory.join("ready")).unwrap();
            if let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", pid.trim())) {
                assert!(
                    matches!(
                        stat.rsplit_once(") ").unwrap().1.chars().next(),
                        Some('Z' | 'X')
                    ),
                    "descendant still running"
                );
            }
        }
        // An already-cancelled request must not launch a second process.
        let stopped = CancellationToken::new();
        stopped.cancel();
        assert_eq!(
            LocalProcessRunner::new(1024).run_controlled(
                &ProcessRequest {
                    command: "touch forbidden".into(),
                    working_directory: directory.clone(),
                    timeout_ms: 1000
                },
                &stopped
            ),
            Err(ProcessError::Cancelled)
        );
        assert!(!directory.join("forbidden").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn timeout_kills_descendants_and_does_not_wait_for_inherited_pipes() {
        let start = Instant::now();
        let output = shell_output("sleep 3 & echo $!; wait", 100, 1024);
        assert!(start.elapsed() < Duration::from_secs(2), "{output:?}");
        assert!(output.timed_out);
        assert_eq!(output.exit_code, None);
        #[cfg(target_os = "linux")]
        {
            let pid: u32 = output.stdout.trim().parse().unwrap();
            if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                // A killed orphan may remain a zombie until the host reaps it.
                let state = stat.rsplit_once(") ").unwrap().1.chars().next().unwrap();
                assert!(
                    matches!(state, 'Z' | 'X'),
                    "descendant is still running: {stat}"
                );
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn exited_shell_cleans_up_background_children() {
        let start = Instant::now();
        let output = shell_output("sleep 3 & printf done", 5000, 1024);
        assert!(start.elapsed() < Duration::from_secs(2), "{output:?}");
        assert_eq!(output.exit_code, Some(0));
        assert!(!output.timed_out);
        assert_eq!(output.stdout, "done");
    }

    #[test]
    #[cfg(unix)]
    fn continuous_output_is_capped_without_starving_timeout_or_stderr() {
        let start = Instant::now();
        let output = shell_output(
            "while :; do printf 1234567890; printf abcdefghij >&2; done",
            100,
            32,
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(output.timed_out);
        assert_eq!(output.stdout.len(), 32);
        assert_eq!(output.stderr.len(), 32);
        assert!(output.stdout_truncated && output.stderr_truncated);
    }

    #[test]
    #[cfg(unix)]
    fn ordinary_command_preserves_output_and_exit_status() {
        let output = shell_output("printf out; printf err >&2; exit 7", 1000, 1024);
        assert_eq!(output.stdout, "out");
        assert_eq!(output.stderr, "err");
        assert_eq!(output.exit_code, Some(7));
        assert!(!output.timed_out);
        assert!(!output.stdout_truncated && !output.stderr_truncated);
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_deadline_bounds_output_held_open_outside_the_process_group() {
        use std::os::fd::OwnedFd;
        use std::os::unix::{net::UnixStream, process::CommandExt};

        // Hold the writer in the test process to simulate a descendant outside
        // the managed group retaining stdout after the foreground process exits.
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"retained").unwrap();
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdout = Some(std::process::ChildStdout::from(OwnedFd::from(reader)));
        let start = Instant::now();
        let output = capture_process(&mut child, 1000, 1024, &CancellationToken::new()).unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, "retained");
        assert!(output.stdout_truncated);
        assert!(!output.stderr_truncated);
        assert!(!output.timed_out);
    }
}
