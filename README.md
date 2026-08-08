# Nausicaa(Rust Harness)

安全不变量留在`agent-harness-core`，产品层能力全部拆成可选 crate；应用可以只采用 core，也可以通过
`agent-harness` facade 的 features 逐项组合。

## 模块

| Crate / feature | 平面 | 当前实现 |
| --- | --- | --- |
| `agent-harness-core` | 认知、能力基础 | Thread/Turn loop、能力投影、规范化动作、精确审批、hooks、取消、durable receipt、崩溃恢复 |
| `context-fs` | 认知 | 目录级 `AGENTS.md`、Skill 索引/按需全文、不拆散 tool-call/receipt 的确定性上下文窗口 |
| `memory` | 状态、认知 | JSONL 长期记忆、词法召回、每个 turn 冻结的 advisory memory snapshot |
| `process-executor` | 执行 | `read_file`、`write_file`、`shell`，显式本机 runner 和 Linux Bubblewrap runner |
| `provider-openai` | 认知 | OpenAI-compatible Chat Completions 映射、可替换 HTTP transport、Curl transport |
| `task-ledger` | 状态、控制 | 幂等提交、worker lease、heartbeat、取消、`unknown` 恢复、完成投递确认 |
| `app-server` | 控制 | line-oriented JSON-RPC，支持 thread、后台 turn、状态、取消、事件回放和恢复 |
| `tui` | 客户端 | Crossterm 对话界面、durable 事件流、后台 turn、取消、终端内 y/n 精确动作审批 |

facade 默认只带 core：

```toml
[dependencies]
agent-harness = { path = "crates/harness", features = [
    "context-fs",
    "memory",
    "process-executor",
    "provider-openai",
] }
```

也可以使用 `features = ["full"]`，或直接依赖任意扩展 crate。core 不依赖任何扩展或
TUI 库。

## Core 不变量

```text
compile context
  -> project visible tools
  -> call model
  -> tool.prepare(model arguments)
  -> policy + hooks
  -> exact-action approval (when required)
  -> persist ToolExecutionStarted
  -> executor boundary
  -> persist ToolReceiptRecorded
  -> add receipt to the next model context
```

- deny 工具不会进入模型 schema，执行前仍会再次 fail-closed 检查。
- 子 Agent capability 是父 capability 与自身策略的交集，不能扩大权限。
- 审批绑定完整 `CanonicalAction`，而不是自然语言意图。
- receipt 持久化成功后才能发起下一次模型请求。
- 有 execution-start、无 receipt 的动作恢复为 `unknown`，不会自动重放。
- memory、Skill 和项目 prompt 只提供上下文，不能修改强制策略。
- runtime 默认使用 `RejectingExecutor`；进程内执行必须显式选择。

## TUI

无需 API 的终端烟雾测试：

```bash
cargo run -p agent-harness-tui --offline -- --demo --no-tools --workspace .
```

连接 OpenAI-compatible endpoint：

```bash
export HARNESS_API_URL=https://api.openai.com/v1/chat/completions
export HARNESS_MODEL=<model-name>
export HARNESS_API_KEY=<token>
cargo run -p agent-harness-tui --offline -- --workspace .
```

默认工具策略为：

- `read_file`: allow。
- `write_file`: ask。
- `shell`: ask，并使用无网络、只挂载 workspace 可写的 Bubblewrap runner。

审批时 TUI 显示规范化后的参数，按 `y` 或 `n`。输入 `/cancel` 请求取消当前 turn，输入
`/quit` 退出。会话事件保存在 `<workspace>/.agent-harness/events.jsonl`。

`--unsafe-local-exec` 会把 shell 切换为继承宿主权限的本机 runner；它不是 sandbox，
因此名称刻意保持显眼。`read_file`/`write_file` 有 workspace 路径和 symlink 检查，但路径
检查本身也不等价于 OS 隔离。

## JSON-RPC app-server

`AppServer::serve` 接受每行一个 JSON-RPC 2.0 请求，当前方法包括：

- `server/health`
- `thread/start`
- `thread/events`
- `thread/recover`
- `turn/start`
- `turn/status`
- `turn/cancel`

`turn/start` 立即返回 control-plane 生成的 `turn_id`，模型循环在后台线程运行；客户端可
轮询状态和按 sequence 增量读取 durable events。

## 构建与验证

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --offline -- -D warnings
cargo test --workspace --all-features --offline
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --offline
```

core 的最小组装示例位于
[`crates/harness-core/examples/minimal.rs`](crates/harness-core/examples/minimal.rs)。

## 已知边界

- provider 扩展当前实现非流式 Chat Completions；transport trait 可替换，Curl 调用是阻塞式。
- 确定性 compaction 只丢弃完整旧消息组并明确记录数量，不伪造语义摘要。
- JSONL stores 保证单进程内同步追加和 `sync_data`，不提供跨进程分布式锁。
- Bubblewrap 是当前 Linux OS sandbox 实现；容器、VM、SSH 和云 sandbox 仍需新增 runner。
- task ledger 已实现恢复语义，但尚未加入常驻 Gateway、渠道路由和分布式 worker 服务。
- 子 Agent capability 交集已在 core 中实现，完整 subagent/worktree scheduler 尚未加入。

## Workspace

```text
crates/
├── harness-core/       # 必选的安全与 agent-loop 内核
├── harness/            # feature-gated facade
├── context-fs/         # 可选上下文/Skill
├── memory/             # 可选长期记忆
├── executor-process/   # 可选文件工具和进程 backend
├── provider-openai/    # 可选 provider adapter
├── task-ledger/        # 可选后台任务账本
├── app-server/         # 可选 JSON-RPC 控制面
└── tui/                # 可选 TUI 库和二进制
```
