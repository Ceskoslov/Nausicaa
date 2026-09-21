//! Bounded editing helpers. Hosts must serialize external workspace writers;
//! content checks are optimistic conflict detection, not filesystem transactions.

use std::io::{Seek, SeekFrom};

use super::*;

const MAX_FILE_BYTES: usize = 1_048_576;
const MAX_PAGE_BYTES: usize = 16_384;
const MAX_PATHS: usize = 32;
const MAX_MATCHES: usize = 100;

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArguments(message.into())
}

fn read_text(file: &mut File) -> Result<String, ToolError> {
    if !file.metadata().map_err(execution)?.is_file() {
        return Err(invalid("target must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(execution)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(invalid("file exceeds 1 MiB limit"));
    }
    String::from_utf8(bytes).map_err(|_| invalid("file is not valid UTF-8"))
}

fn execution(error: std::io::Error) -> ToolError {
    ToolError::Execution(error.to_string())
}

fn checked_path(workspace: &Path, path: &Path) -> Result<PathBuf, ToolError> {
    let relative = path
        .strip_prefix(workspace)
        .map_err(|_| invalid("path outside workspace"))?;
    let current = resolve_existing_file(workspace, relative)?;
    if current != path {
        return Err(invalid("prepared path changed; prepare a new call"));
    }
    Ok(current)
}

fn number(args: &Value, key: &str, default: usize, maximum: usize) -> Result<usize, ToolError> {
    match args.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= maximum)
            .ok_or_else(|| invalid(format!("invalid `{key}`"))),
    }
}

fn prepared(
    call: &ToolCall,
    name: &str,
    args: Value,
    workspace: &Path,
    write: bool,
) -> PreparedToolCall {
    PreparedToolCall {
        call_id: call.id.clone(),
        action: CanonicalAction::new(
            name,
            args,
            if write {
                EffectKind::WorkspaceWrite
            } else {
                EffectKind::ReadOnly
            },
            if write {
                RetrySafety::Unsafe
            } else {
                RetrySafety::Safe
            },
        )
        .in_scope(workspace.display().to_string()),
    }
}

/// Reads bounded UTF-8 byte pages with explicit continuation offsets.
#[derive(Clone, Debug)]
pub struct ReadFilePageTool {
    workspace: PathBuf,
}

impl ReadFilePageTool {
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, ProcessError> {
        Ok(Self {
            workspace: canonical_directory(workspace.as_ref())?,
        })
    }
}

impl Tool for ReadFilePageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "read_file_page",
            "Read a UTF-8 byte page (files at most 1 MiB); follow next_offset. Pages are live reads.",
            json!({
                "type":"object", "properties": {
                    "path":{"type":"string"}, "offset":{"type":"integer","minimum":0},
                    "limit":{"type":"integer","minimum":4,"maximum":MAX_PAGE_BYTES}
                }, "required":["path"], "additionalProperties":false
            }),
        )
    }
    fn prepare(
        &self,
        call: &ToolCall,
        _: &ToolExecutionContext,
    ) -> Result<PreparedToolCall, ToolError> {
        let path = resolve_existing_file(
            &self.workspace,
            Path::new(string_argument(&call.arguments, "path")?),
        )?;
        let offset = number(&call.arguments, "offset", 0, MAX_FILE_BYTES)?;
        let limit = number(&call.arguments, "limit", 4096, MAX_PAGE_BYTES)?;
        if limit < 4 {
            return Err(invalid("limit must be at least 4 bytes"));
        }
        Ok(prepared(
            call,
            "read_file_page",
            json!({"path":path,"offset":offset,"limit":limit}),
            &self.workspace,
            false,
        ))
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
            let args = &prepared.action.arguments;
            let path = checked_path(&self.workspace, &path_argument(args, "path")?)?;
            let content = read_text(&mut File::open(&path).map_err(execution)?)?;
            let offset = number(args, "offset", 0, MAX_FILE_BYTES)?;
            let limit = number(args, "limit", 4096, MAX_PAGE_BYTES)?;
            if limit < 4 || offset > content.len() || !content.is_char_boundary(offset) {
                return Err(invalid("invalid page bounds or UTF-8 offset"));
            }
            let mut end = (offset + limit).min(content.len());
            while !content.is_char_boundary(end) {
                end -= 1;
            }
            Ok(ToolOutput::new(
                json!({"path":path,"content":&content[offset..end],"offset":offset,
                "next_offset":(end < content.len()).then_some(end),"total_bytes":content.len()}),
            ))
        })
    }
}

/// Literal, case-sensitive search over an explicitly bounded list of files.
#[derive(Clone, Debug)]
pub struct SearchFilesTool {
    workspace: PathBuf,
}
impl SearchFilesTool {
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, ProcessError> {
        Ok(Self {
            workspace: canonical_directory(workspace.as_ref())?,
        })
    }
}
impl Tool for SearchFilesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "search_files",
            "Search a literal within up to 32 UTF-8 files (1 MiB each). Returns up to 100 matching lines and bounded excerpts.",
            json!({
                "type":"object","properties":{
                    "paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":MAX_PATHS},
                    "query":{"type":"string","minLength":1,"maxLength":1024}
                },"required":["paths","query"],"additionalProperties":false
            }),
        )
    }
    fn prepare(
        &self,
        call: &ToolCall,
        _: &ToolExecutionContext,
    ) -> Result<PreparedToolCall, ToolError> {
        let query = string_argument(&call.arguments, "query")?;
        if query.is_empty() || query.len() > 1024 || query.contains(['\n', '\r']) {
            return Err(invalid(
                "query must be a single nonempty line of at most 1024 bytes",
            ));
        }
        let paths = call
            .arguments
            .get("paths")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("paths must be an array"))?;
        if paths.is_empty() || paths.len() > MAX_PATHS {
            return Err(invalid("provide 1 to 32 paths"));
        }
        let paths = paths
            .iter()
            .map(|value| {
                resolve_existing_file(
                    &self.workspace,
                    Path::new(
                        value
                            .as_str()
                            .ok_or_else(|| invalid("path must be a string"))?,
                    ),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(prepared(
            call,
            "search_files",
            json!({"paths":paths,"query":query}),
            &self.workspace,
            false,
        ))
    }
    fn execute<'a>(
        &'a self,
        prepared: PreparedToolCall,
        context: ToolExecutionContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let args = &prepared.action.arguments;
            let paths: Vec<PathBuf> = serde_json::from_value(args["paths"].clone())
                .map_err(|e| invalid(e.to_string()))?;
            let query = string_argument(args, "query")?;
            let mut matches = Vec::new();
            for path in paths {
                if context.cancellation.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                let path = checked_path(&self.workspace, &path)?;
                let content = read_text(&mut File::open(&path).map_err(execution)?)?;
                let mut offset = 0;
                for (index, line) in content.split_inclusive('\n').enumerate() {
                    if let Some(column) = line.find(query) {
                        if matches.len() == MAX_MATCHES {
                            return Ok(ToolOutput::new(
                                json!({"matches":matches,"truncated":true}),
                            ));
                        }
                        let excerpt: String = line[column..].chars().take(200).collect();
                        matches.push(json!({"path":path.strip_prefix(&self.workspace).map_err(|_| invalid("path outside workspace"))?,
                            "line":index+1,"offset":offset+column,"excerpt":excerpt}));
                    }
                    offset += line.len();
                }
            }
            Ok(ToolOutput::new(
                json!({"matches":matches,"truncated":false}),
            ))
        })
    }
}

/// Replaces exactly one literal occurrence and binds approval to full before/after snapshots.
#[derive(Clone, Debug)]
pub struct ApplyPatchTool {
    workspace: PathBuf,
}
impl ApplyPatchTool {
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, ProcessError> {
        Ok(Self {
            workspace: canonical_directory(workspace.as_ref())?,
        })
    }
}
impl Tool for ApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "apply_patch",
            "Replace one unique exact old_text with new_text in an existing UTF-8 file. Stale or ambiguous text fails; approval binds full before/after content.",
            json!({
                "type":"object","properties":{
                    "path":{"type":"string"},"old_text":{"type":"string","minLength":1},"new_text":{"type":"string"}
                },"required":["path","old_text","new_text"],"additionalProperties":false
            }),
        )
    }
    fn prepare(
        &self,
        call: &ToolCall,
        _: &ToolExecutionContext,
    ) -> Result<PreparedToolCall, ToolError> {
        let path = resolve_existing_file(
            &self.workspace,
            Path::new(string_argument(&call.arguments, "path")?),
        )?;
        let old = string_argument(&call.arguments, "old_text")?;
        let new = string_argument(&call.arguments, "new_text")?;
        if old.is_empty() || old.len() > MAX_FILE_BYTES || new.len() > MAX_FILE_BYTES {
            return Err(invalid("invalid patch size or empty old_text"));
        }
        let before = read_text(&mut File::open(&path).map_err(execution)?)?;
        // Count overlapping matches too: replacing `aa` in `aaa` is ambiguous.
        let start = before
            .find(old)
            .ok_or_else(|| invalid("patch conflict: old_text not found"))?;
        let next = start
            + before[start..]
                .chars()
                .next()
                .expect("nonempty match")
                .len_utf8();
        if before[next..].contains(old) {
            return Err(invalid("patch conflict: old_text is ambiguous"));
        }
        let mut after = before.clone();
        after.replace_range(start..start + old.len(), new);
        if after.len() > MAX_FILE_BYTES {
            return Err(invalid("patched file exceeds 1 MiB"));
        }
        Ok(prepared(
            call,
            "apply_patch",
            json!({"path":path,"before":before,"after":after,"old_text":old,"new_text":new}),
            &self.workspace,
            true,
        ))
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
            let args = &prepared.action.arguments;
            let path = checked_path(&self.workspace, &path_argument(args, "path")?)?;
            let before = string_argument(args, "before")?;
            let after = string_argument(args, "after")?;
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(execution)?;
            if read_text(&mut file)? != before {
                return Err(invalid("patch conflict: file changed after preparation"));
            }
            file.seek(SeekFrom::Start(0)).map_err(execution)?;
            if context.cancellation.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            // Any error after the first write may leave effects. Never report a
            // known failure that encourages automatic replay of a partial patch.
            file.write_all(after.as_bytes())
                .and_then(|()| file.set_len(after.len() as u64))
                .and_then(|()| file.sync_data())
                .map_err(|e| ToolError::OutcomeUnknown(e.to_string()))?;
            Ok(ToolOutput::new(
                json!({"path":path,"bytes_written":after.len()}),
            ))
        })
    }
}

/// Register optional editing tools without changing legacy tool registration or grants.
pub fn register_editing_tools(
    registry: &mut ToolRegistry,
    workspace: impl AsRef<Path>,
) -> Result<(), ProcessError> {
    registry
        .register(ReadFilePageTool::new(&workspace)?)
        .map_err(|e| ProcessError::Invalid(e.to_string()))?;
    registry
        .register(SearchFilesTool::new(&workspace)?)
        .map_err(|e| ProcessError::Invalid(e.to_string()))?;
    registry
        .register(ApplyPatchTool::new(workspace)?)
        .map_err(|e| ProcessError::Invalid(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{block_on, context};
    use agent_harness_core::ThreadId;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(ThreadId::new().as_str());
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn run(&self, tool: &dyn Tool, args: Value) -> Result<Value, ToolError> {
            let call = ToolCall::new(tool.definition().name, args);
            let prepared = tool.prepare(&call, &context(&self.0))?;
            block_on(tool.execute(prepared, context(&self.0))).map(|output| output.value)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn multi_file_search_read_patch_and_stale_conflicts() {
        let f = Fixture::new();
        fs::write(f.0.join("a.rs"), "fn a() -> i32 { 1 }\n").unwrap();
        fs::write(f.0.join("b.rs"), "fn b() -> i32 { 1 }\n").unwrap();
        let search = SearchFilesTool::new(&f.0).unwrap();
        let result = f
            .run(&search, json!({"paths":["a.rs","b.rs"],"query":"{ 1 }"}))
            .unwrap();
        assert_eq!(result["matches"].as_array().unwrap().len(), 2);
        assert_eq!(result["truncated"], false);
        let patch = ApplyPatchTool::new(&f.0).unwrap();
        for path in ["a.rs", "b.rs"] {
            let before = f
                .run(&ReadFilePageTool::new(&f.0).unwrap(), json!({"path":path}))
                .unwrap();
            let mut call = ToolCall::new(
                "apply_patch",
                json!({"path":path,"old_text":"{ 1 }","new_text":"{ 2 }"}),
            );
            let prepared = patch.prepare(&call, &context(&f.0)).unwrap();
            assert_eq!(prepared.action.arguments["before"], before["content"]);
            assert!(
                prepared.action.arguments["after"]
                    .as_str()
                    .unwrap()
                    .contains("{ 2 }")
            );
            assert_eq!(prepared.action.effect, EffectKind::WorkspaceWrite);
            call.arguments["new_text"] = json!("UNAPPROVED");
            block_on(patch.execute(prepared, context(&f.0))).unwrap();
            assert!(
                fs::read_to_string(f.0.join(path))
                    .unwrap()
                    .contains("{ 2 }")
            );
            assert!(
                f.run(
                    &patch,
                    json!({"path":path,"old_text":"{ 1 }","new_text":"{ 3 }"})
                )
                .is_err()
            );
        }
        let prepared = patch
            .prepare(
                &ToolCall::new(
                    "apply_patch",
                    json!({"path":"a.rs","old_text":"{ 2 }","new_text":"{ 3 }"}),
                ),
                &context(&f.0),
            )
            .unwrap();
        fs::write(
            f.0.join("a.rs"),
            "// concurrent edit\nfn a() -> i32 { 2 }\n",
        )
        .unwrap();
        let before = fs::read(f.0.join("a.rs")).unwrap();
        assert!(block_on(patch.execute(prepared, context(&f.0))).is_err());
        assert_eq!(fs::read(f.0.join("a.rs")).unwrap(), before);
    }

    #[test]
    fn unicode_pages_are_bounded_and_reconstruct_exact_content() {
        let f = Fixture::new();
        let content = "a你好🙂\r\nlast";
        fs::write(f.0.join("text"), content).unwrap();
        let tool = ReadFilePageTool::new(&f.0).unwrap();
        let mut offset = 0;
        let mut recovered = String::new();
        loop {
            let page = f
                .run(&tool, json!({"path":"text","offset":offset,"limit":4}))
                .unwrap();
            let part = page["content"].as_str().unwrap();
            assert!(part.len() <= 4);
            recovered.push_str(part);
            match page["next_offset"].as_u64() {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(recovered, content);
        for args in [
            json!({"path":"text","offset":2}),
            json!({"path":"text","offset":999}),
            json!({"path":"text","limit":0}),
            json!({"path":"text","limit":"4"}),
        ] {
            assert!(f.run(&tool, args).is_err());
        }
        assert_eq!(
            f.run(&tool, json!({"path":"text","offset":content.len()}))
                .unwrap()["content"],
            ""
        );
    }

    #[test]
    fn ambiguous_empty_and_oversized_inputs_fail_without_writing() {
        let f = Fixture::new();
        fs::write(f.0.join("text"), "aaa").unwrap();
        let patch = ApplyPatchTool::new(&f.0).unwrap();
        for old in ["", "aa", "missing"] {
            assert!(
                f.run(&patch, json!({"path":"text","old_text":old,"new_text":"x"}))
                    .is_err()
            );
        }
        assert_eq!(fs::read_to_string(f.0.join("text")).unwrap(), "aaa");
        fs::write(f.0.join("text"), vec![b'a'; MAX_FILE_BYTES + 1]).unwrap();
        assert!(
            f.run(
                &ReadFilePageTool::new(&f.0).unwrap(),
                json!({"path":"text"})
            )
            .is_err()
        );
        assert!(
            f.run(&patch, json!({"path":"text","old_text":"a","new_text":"x"}))
                .is_err()
        );
        fs::write(f.0.join("text"), [0xff]).unwrap();
        assert!(
            f.run(
                &ReadFilePageTool::new(&f.0).unwrap(),
                json!({"path":"text"})
            )
            .is_err()
        );
    }

    #[test]
    fn search_is_bounded_and_reports_truncation_and_relative_locations() {
        let f = Fixture::new();
        fs::write(f.0.join("text"), "你needle\n".repeat(101)).unwrap();
        let tool = SearchFilesTool::new(&f.0).unwrap();
        let result = f
            .run(&tool, json!({"paths":["text"],"query":"needle"}))
            .unwrap();
        assert_eq!(result["matches"].as_array().unwrap().len(), 100);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["matches"][0]["path"], "text");
        assert_eq!(result["matches"][0]["offset"], 3);
        assert_eq!(result["matches"][0]["line"], 1);
        for args in [
            json!({"paths":[],"query":"x"}),
            json!({"paths":["text"],"query":""}),
            json!({"paths":["text"],"query":"x\ny"}),
            json!({"paths":vec!["text";33],"query":"x"}),
        ] {
            assert!(f.run(&tool, args).is_err());
        }
    }

    #[test]
    fn cancellation_and_traversal_never_write() {
        let f = Fixture::new();
        fs::write(f.0.join("text"), "before").unwrap();
        let patch = ApplyPatchTool::new(&f.0).unwrap();
        assert!(
            f.run(
                &patch,
                json!({"path":"../text","old_text":"before","new_text":"after"})
            )
            .is_err()
        );
        let prepared = patch
            .prepare(
                &ToolCall::new(
                    "apply_patch",
                    json!({"path":"text","old_text":"before","new_text":"after"}),
                ),
                &context(&f.0),
            )
            .unwrap();
        let ctx = context(&f.0);
        ctx.cancellation.cancel();
        assert_eq!(
            block_on(patch.execute(prepared, ctx)),
            Err(ToolError::Cancelled)
        );
        assert_eq!(fs::read_to_string(f.0.join("text")).unwrap(), "before");
    }

    #[test]
    #[cfg(unix)]
    fn changed_symlink_rejected_at_execution() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let outside = Fixture::new();
        fs::write(f.0.join("text"), "before").unwrap();
        fs::write(outside.0.join("text"), "before").unwrap();
        let patch = ApplyPatchTool::new(&f.0).unwrap();
        let prepared = patch
            .prepare(
                &ToolCall::new(
                    "apply_patch",
                    json!({"path":"text","old_text":"before","new_text":"after"}),
                ),
                &context(&f.0),
            )
            .unwrap();
        fs::remove_file(f.0.join("text")).unwrap();
        symlink(outside.0.join("text"), f.0.join("text")).unwrap();
        assert!(block_on(patch.execute(prepared, context(&f.0))).is_err());
        assert_eq!(
            fs::read_to_string(outside.0.join("text")).unwrap(),
            "before"
        );
        assert!(
            f.run(
                &ReadFilePageTool::new(&f.0).unwrap(),
                json!({"path":"text"})
            )
            .is_err()
        );
    }
    #[test]
    fn runtime_preserves_policy_approval_and_receipt_ordering() {
        use agent_harness_core::{
            Access, AgentRuntime, CapabilityPolicy, DirectExecutor, LayeredContextCompiler,
            MemoryEventStore, ModelAdapter, ModelError, ModelRequest, ModelResponse, ReceiptStatus,
            RuntimeEvent,
        };
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Model(AtomicBool);
        impl ModelAdapter for Model {
            fn complete<'a>(
                &'a self,
                request: ModelRequest,
            ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
                Box::pin(async move {
                    if self.0.swap(true, Ordering::SeqCst) {
                        assert!(request.context.messages.iter().any(|m| matches!(
                            m,
                            agent_harness_core::TranscriptMessage::Tool { .. }
                        )));
                        Ok(ModelResponse::text("done"))
                    } else {
                        Ok(ModelResponse::tool_calls(vec![ToolCall::new(
                            "apply_patch",
                            json!({"path":"text","old_text":"before","new_text":"after"}),
                        )]))
                    }
                })
            }
        }
        for access in [Access::Deny, Access::Ask, Access::Allow] {
            let f = Fixture::new();
            fs::write(f.0.join("text"), "before").unwrap();
            let mut tools = ToolRegistry::new();
            register_editing_tools(&mut tools, &f.0).unwrap();
            let store = Arc::new(MemoryEventStore::new());
            let runtime = AgentRuntime::new(
                Arc::new(Model(AtomicBool::new(false))),
                store.clone(),
                Arc::new(LayeredContextCompiler::default()),
                tools,
                Arc::new(CapabilityPolicy::deny_by_default().grant("apply_patch", access)),
            )
            .with_executor(Arc::new(DirectExecutor));
            let id = runtime.start_thread().unwrap();
            block_on(runtime.run_turn(&id, "edit")).unwrap();
            let events = store.all_events().unwrap();
            let receipt_index = events
                .iter()
                .position(|e| matches!(e.event, RuntimeEvent::ToolReceiptRecorded { .. }))
                .unwrap();
            let RuntimeEvent::ToolReceiptRecorded { receipt } = &events[receipt_index].event else {
                unreachable!()
            };
            if access == Access::Allow {
                assert_eq!(fs::read_to_string(f.0.join("text")).unwrap(), "after");
                assert_eq!(receipt.status, ReceiptStatus::Succeeded);
                let start = events
                    .iter()
                    .position(|e| matches!(e.event, RuntimeEvent::ToolExecutionStarted { .. }))
                    .unwrap();
                assert!(start < receipt_index);
            } else {
                assert_eq!(fs::read_to_string(f.0.join("text")).unwrap(), "before");
                assert_eq!(receipt.status, ReceiptStatus::Denied);
                assert!(
                    !events
                        .iter()
                        .any(|e| matches!(e.event, RuntimeEvent::ToolExecutionStarted { .. }))
                );
            }
        }
    }
}
