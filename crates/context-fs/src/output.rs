use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_harness_core::*;
use serde_json::{Value, json};

/// Immutable per-thread/call JSON output snapshots in a trusted host directory.
/// Keep this directory outside tool-writable workspaces. Exclusive ownership is
/// required across instances/processes; path checks are not an OS sandbox.
#[derive(Debug)]
pub struct OutputArchive {
    root: PathBuf,
    max_output_bytes: usize,
    writer: Mutex<()>,
}

fn read_error(error: impl std::fmt::Display) -> ContextError {
    ContextError::Read(error.to_string())
}

// Hex encoding is injective and cannot introduce path separators. Bound each
// component below filesystem filename limits rather than truncating IDs.
fn component(id: &str) -> Result<String, ContextError> {
    if id.is_empty() || id.len() > 96 {
        return Err(read_error("archive IDs must contain 1..=96 UTF-8 bytes"));
    }
    Ok(id.bytes().map(|byte| format!("{byte:02x}")).collect())
}

impl OutputArchive {
    pub fn new(root: impl AsRef<Path>, max_output_bytes: usize) -> Result<Self, ContextError> {
        if max_output_bytes == 0 {
            return Err(read_error("archive output limit must be positive"));
        }
        fs::create_dir_all(&root).map_err(read_error)?;
        let root = root.as_ref().canonicalize().map_err(read_error)?;
        Ok(Self {
            root,
            max_output_bytes,
            writer: Mutex::new(()),
        })
    }

    fn paths(&self, thread: &ThreadId, call: &str) -> Result<(PathBuf, PathBuf), ContextError> {
        let directory = self.root.join(component(thread.as_str())?);
        let path = directory.join(format!("{}.json", component(call)?));
        Ok((directory, path))
    }

    fn check_directory(directory: &Path) -> Result<(), ContextError> {
        if !fs::symlink_metadata(directory)
            .map_err(read_error)?
            .file_type()
            .is_dir()
        {
            return Err(read_error("archive thread directory must not be a symlink"));
        }
        Ok(())
    }

    fn open_snapshot(&self, thread: &ThreadId, call: &str) -> Result<File, ContextError> {
        let (directory, path) = self.paths(thread, call)?;
        Self::check_directory(&directory)?;
        let metadata = fs::symlink_metadata(&path).map_err(read_error)?;
        if !metadata.file_type().is_file() || metadata.len() > self.max_output_bytes as u64 {
            return Err(read_error("archive snapshot is not a bounded regular file"));
        }
        File::open(path).map_err(read_error)
    }

    fn retain(&self, thread: &ThreadId, call: &str, bytes: &[u8]) -> Result<(), ContextError> {
        if bytes.len() > self.max_output_bytes {
            return Err(read_error("tool output exceeds archive limit"));
        }
        let _guard = self.writer.lock().map_err(read_error)?;
        let (directory, path) = self.paths(thread, call)?;
        match fs::create_dir(&directory) {
            Ok(()) => {
                #[cfg(unix)]
                File::open(&self.root)
                    .and_then(|file| file.sync_all())
                    .map_err(read_error)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(read_error(error)),
        }
        Self::check_directory(&directory)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if fs::symlink_metadata(&path).is_ok() {
            let mut retained = Vec::new();
            self.open_snapshot(thread, call)?
                .take((self.max_output_bytes as u64).saturating_add(1))
                .read_to_end(&mut retained)
                .map_err(read_error)?;
            if retained != bytes {
                return Err(read_error("archive snapshot conflict; refusing overwrite"));
            }
            self.open_snapshot(thread, call)?
                .sync_all()
                .map_err(read_error)?;
        } else {
            // Publish a complete synced inode without replacing an existing
            // snapshot. A crash can leave a .pending file, never a partial .json.
            let pending = directory.join(format!(".pending-{}", EventId::new()));
            let mut file = options.open(&pending).map_err(read_error)?;
            let result = (|| {
                file.write_all(bytes).map_err(read_error)?;
                file.sync_all().map_err(read_error)?;
                drop(file);
                fs::hard_link(&pending, &path).map_err(read_error)
            })();
            let cleanup = fs::remove_file(&pending);
            result?;
            cleanup.map_err(read_error)?;
        }
        #[cfg(unix)]
        File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(read_error)?;
        Ok(())
    }

    /// Read a page of the serialized JSON value. Offsets and limits are bytes;
    /// offsets must be UTF-8 boundaries. The next offset never splits a character.
    pub fn read_page(
        &self,
        thread: &ThreadId,
        call: &str,
        offset: u64,
        limit: usize,
    ) -> Result<Value, ContextError> {
        if !(4..=4096).contains(&limit) {
            return Err(read_error("page limit must be 4..=4096 bytes"));
        }
        let mut file = self.open_snapshot(thread, call)?;
        let total = file.metadata().map_err(read_error)?.len();
        if offset > total {
            return Err(read_error("page offset exceeds snapshot length"));
        }
        file.seek(SeekFrom::Start(offset)).map_err(read_error)?;
        let mut bytes = Vec::new();
        file.take(limit as u64)
            .read_to_end(&mut bytes)
            .map_err(read_error)?;
        let valid = match std::str::from_utf8(&bytes) {
            Ok(text) => text.len(),
            Err(error) if error.error_len().is_none() && offset + (bytes.len() as u64) < total => {
                error.valid_up_to()
            }
            Err(error) => return Err(read_error(error)),
        };
        let text = std::str::from_utf8(&bytes[..valid]).map_err(read_error)?;
        let next = offset + valid as u64;
        Ok(
            json!({ "call_id": call, "text": text, "next_offset": next, "total_bytes": total, "eof": next == total }),
        )
    }
}

/// Externalize large receipt outputs before inner compilation/compaction. Only
/// cloned model context is changed; durable receipts and acceptance evidence stay
/// intact. Storage errors fail compilation rather than discarding evidence.
pub struct ExternalOutputCompiler {
    inner: Arc<dyn ContextCompiler>,
    archive: Arc<OutputArchive>,
    inline_bytes: usize,
}

impl ExternalOutputCompiler {
    pub fn new(
        inner: Arc<dyn ContextCompiler>,
        archive: Arc<OutputArchive>,
        inline_bytes: usize,
    ) -> Self {
        Self {
            inner,
            archive,
            inline_bytes,
        }
    }
}

impl ContextCompiler for ExternalOutputCompiler {
    fn compile(&self, mut input: ContextInput) -> Result<CompiledContext, ContextError> {
        for message in &mut input.transcript {
            if let TranscriptMessage::Tool { receipt } = message {
                // Retrieval pages are already bounded. Do not recursively hide
                // the very evidence the model requested to inspect.
                if receipt.tool_name == "read_output" {
                    continue;
                }
                if let Some(output) = &receipt.output {
                    let bytes = serde_json::to_vec(output).map_err(read_error)?;
                    if bytes.len() > self.inline_bytes {
                        self.archive
                            .retain(&input.thread_id, receipt.call_id.as_str(), &bytes)?;
                        receipt.output = Some(json!({ "externalized_output": {
                            "version": 1, "call_id": receipt.call_id.as_str(), "total_bytes": bytes.len(),
                            "format": "json", "read_tool": "read_output",
                            "note": "Reading requires host registration and policy authorization"
                        }}));
                    }
                }
            }
        }
        self.inner.compile(input)
    }
}

/// Explicitly register and authorize this read-only tool to expose archived
/// evidence. Model arguments cannot select another thread or an arbitrary path.
pub struct ReadOutputTool {
    archive: Arc<OutputArchive>,
}

impl ReadOutputTool {
    pub fn new(archive: Arc<OutputArchive>) -> Self {
        Self { archive }
    }
}

impl Tool for ReadOutputTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "read_output",
            "Read a UTF-8 page of archived JSON tool output in this thread",
            json!({
                "type": "object", "properties": {
                    "call_id": {"type": "string"}, "offset": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 4, "maximum": 4096}
                }, "required": ["call_id"], "additionalProperties": false
            }),
        )
    }

    fn prepare(
        &self,
        call: &ToolCall,
        context: &ToolExecutionContext,
    ) -> Result<PreparedToolCall, ToolError> {
        let invalid = |error: &str| ToolError::InvalidArguments(error.into());
        let args = call
            .arguments
            .as_object()
            .ok_or_else(|| invalid("expected object"))?;
        if args
            .keys()
            .any(|key| !["call_id", "offset", "limit"].contains(&key.as_str()))
        {
            return Err(invalid("unknown page argument"));
        }
        let id = args
            .get("call_id")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("call_id is required"))?;
        component(id).map_err(|error| invalid(&error.to_string()))?;
        let offset = args
            .get("offset")
            .map_or(Some(0), Value::as_u64)
            .ok_or_else(|| invalid("offset must be an unsigned integer"))?;
        let limit = args
            .get("limit")
            .map_or(Some(2048), Value::as_u64)
            .filter(|n| (4..=4096).contains(n))
            .ok_or_else(|| invalid("limit must be 4..=4096"))?;
        Ok(PreparedToolCall {
            call_id: call.id.clone(),
            action: CanonicalAction::new(
                "read_output",
                json!({"call_id": id, "offset": offset, "limit": limit}),
                EffectKind::ReadOnly,
                RetrySafety::Safe,
            )
            .in_scope(context.thread_id.to_string()),
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
            if prepared.action.scope.as_deref() != Some(context.thread_id.as_str())
                || prepared.action.tool_name != "read_output"
            {
                return Err(ToolError::Execution(
                    "prepared output scope mismatch".into(),
                ));
            }
            let args = &prepared.action.arguments;
            let id = args
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::Execution("missing prepared call_id".into()))?;
            let offset = args
                .get("offset")
                .and_then(Value::as_u64)
                .ok_or_else(|| ToolError::Execution("missing prepared offset".into()))?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| ToolError::Execution("missing prepared limit".into()))?;
            self.archive
                .read_page(&context.thread_id, id, offset, limit)
                .map(ToolOutput::new)
                .map_err(|error| ToolError::Execution(error.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("nausicaa-output-{}", EventId::new()));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, Waker};
        let mut future = std::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("fixture must be synchronous"),
        }
    }
    fn context(thread: ThreadId) -> ToolExecutionContext {
        ToolExecutionContext {
            thread_id: thread,
            turn_id: TurnId::new(),
            workspace: None,
            cancellation: CancellationToken::new(),
            metadata: Default::default(),
        }
    }

    #[test]
    fn large_outputs_are_retained_once_and_pages_reconstruct_exact_json() {
        let fixture = Fixture::new();
        let archive = Arc::new(OutputArchive::new(&fixture.0, 100_000).unwrap());
        let compiler = ExternalOutputCompiler::new(
            Arc::new(LayeredContextCompiler::new().with_max_characters(1000)),
            archive.clone(),
            256,
        );
        let thread = ThreadId::new();
        let call = ToolCall::new("large_output", json!({}));
        let value = json!("€猫🙂".repeat(1000));
        let prepared = PreparedToolCall {
            call_id: call.id.clone(),
            action: CanonicalAction::new(
                "large_output",
                json!({}),
                EffectKind::ReadOnly,
                RetrySafety::Safe,
            ),
        };
        let receipt = ToolReceipt::succeeded(prepared, value.clone());
        let input = ContextInput {
            thread_id: thread.clone(),
            turn_id: TurnId::new(),
            transcript: vec![
                TranscriptMessage::Assistant {
                    content: String::new(),
                    tool_calls: vec![call.clone()],
                },
                TranscriptMessage::Tool {
                    receipt: receipt.clone(),
                },
            ],
        };
        let compiled = compiler.compile(input.clone()).unwrap();
        assert_eq!(
            input.transcript[1],
            TranscriptMessage::Tool {
                receipt: receipt.clone()
            }
        );
        assert_eq!(compiled.messages[0], input.transcript[0]);
        let TranscriptMessage::Tool { receipt: view } = &compiled.messages[1] else {
            panic!()
        };
        assert_eq!(view.call_id, receipt.call_id);
        assert_eq!(view.status, receipt.status);
        assert_eq!(view.action, receipt.action);
        assert_eq!(
            view.output.as_ref().unwrap()["externalized_output"]["call_id"],
            call.id.as_str()
        );
        assert_eq!(compiler.compile(input).unwrap(), compiled);
        let reopened = OutputArchive::new(&fixture.0, 100_000).unwrap();
        let mut result = String::new();
        let mut offset = 0;
        loop {
            let page = reopened
                .read_page(&thread, call.id.as_str(), offset, 7)
                .unwrap();
            let text = page["text"].as_str().unwrap();
            assert!(text.len() <= 7);
            result.push_str(text);
            let next = page["next_offset"].as_u64().unwrap();
            if page["eof"] == true {
                break;
            }
            assert!(next > offset);
            offset = next;
        }
        assert_eq!(serde_json::from_str::<Value>(&result).unwrap(), value);
        assert!(reopened.read_page(&thread, call.id.as_str(), 2, 7).is_err());
        assert!(
            reopened
                .read_page(&ThreadId::new(), call.id.as_str(), 0, 7)
                .is_err()
        );
    }

    #[test]
    fn conflicts_and_invalid_paths_fail_without_overwrite() {
        let fixture = Fixture::new();
        let archive = OutputArchive::new(&fixture.0, 20).unwrap();
        let thread = ThreadId::new();
        archive.retain(&thread, "../../outside", b"123").unwrap();
        assert_eq!(
            archive.read_page(&thread, "../../outside", 0, 4).unwrap()["text"],
            "123"
        );
        assert!(archive.retain(&thread, "../../outside", b"456").is_err());
        assert_eq!(
            archive.read_page(&thread, "../../outside", 0, 4).unwrap()["text"],
            "123"
        );
        assert!(archive.retain(&thread, "large", &[0; 21]).is_err());
        assert!(archive.retain(&thread, &"x".repeat(97), b"0").is_err());
        assert!(archive.read_page(&thread, "../../outside", 4, 4).is_err());
        assert!(
            archive
                .read_page(&thread, "../../outside", 0, 4097)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_archive_files_are_rejected() {
        let fixture = Fixture::new();
        let archive = OutputArchive::new(&fixture.0, 20).unwrap();
        let thread = ThreadId::new();
        let (directory, path) = archive.paths(&thread, "call").unwrap();
        fs::create_dir(directory).unwrap();
        let outside = fixture.0.join("outside");
        fs::write(&outside, b"123").unwrap();
        std::os::unix::fs::symlink(&outside, path).unwrap();
        assert!(archive.read_page(&thread, "call", 0, 4).is_err());
        assert!(archive.retain(&thread, "call", b"456").is_err());
        assert_eq!(fs::read(outside).unwrap(), b"123");
    }

    #[test]
    fn retrieval_executes_prepared_page_and_cannot_change_thread() {
        let fixture = Fixture::new();
        let archive = Arc::new(OutputArchive::new(&fixture.0, 100).unwrap());
        let thread = ThreadId::new();
        archive.retain(&thread, "original", b"123456789").unwrap();
        let tool = ReadOutputTool::new(archive);
        let ctx = context(thread);
        let mut call = ToolCall::new(
            "read_output",
            json!({"call_id": "original", "offset": 2, "limit": 4}),
        );
        let prepared = tool.prepare(&call, &ctx).unwrap();
        call.arguments["offset"] = json!(0);
        let output = block_on(tool.execute(prepared.clone(), ctx)).unwrap();
        assert_eq!(output.value["text"], "3456");
        assert!(block_on(tool.execute(prepared, context(ThreadId::new()))).is_err());
        call.arguments["thread_id"] = json!("other");
        assert!(tool.prepare(&call, &context(ThreadId::new())).is_err());
    }

    struct ReadModel(AtomicUsize);
    impl ModelAdapter for ReadModel {
        fn complete<'a>(
            &'a self,
            request: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
            Box::pin(async move {
                assert!(request.tools.is_empty());
                if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(ModelResponse::tool_calls(vec![ToolCall::new(
                        "read_output",
                        json!({"call_id": "original"}),
                    )]))
                } else {
                    Ok(ModelResponse::text("done"))
                }
            })
        }
    }

    #[test]
    fn registering_retrieval_does_not_authorize_it() {
        let fixture = Fixture::new();
        let archive = Arc::new(OutputArchive::new(&fixture.0, 100).unwrap());
        let store = Arc::new(MemoryEventStore::new());
        let mut tools = ToolRegistry::new();
        tools.register(ReadOutputTool::new(archive)).unwrap();
        let runtime = AgentRuntime::new(
            Arc::new(ReadModel(AtomicUsize::new(0))),
            store.clone(),
            Arc::new(LayeredContextCompiler::new()),
            tools,
            Arc::new(CapabilityPolicy::deny_by_default()),
        )
        .with_executor(Arc::new(DirectExecutor));
        let thread = runtime.start_thread().unwrap();
        block_on(runtime.run_turn(&thread, "read evidence")).unwrap();
        let events = store.all_events().unwrap();
        assert!(events.iter().any(|e| matches!(&e.event, RuntimeEvent::ToolReceiptRecorded { receipt } if receipt.status == ReceiptStatus::Denied)));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.event, RuntimeEvent::ToolExecutionStarted { .. }))
        );
    }
}
