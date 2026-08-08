# Nausicaa

The mandatory invariants live in `agent-harness-core`. Filesystem context, memory, process execution, provider integration, background work, the JSON-RPC control plane, and the terminal UI are optional crates. Applications can embed only the core or compose a complete local coding agent through the feature-gated `agent-harness` facade.

## Table of contents

- [Design goals](#design-goals)
- [Architecture](#architecture)
- [Workspace crates](#workspace-crates)
- [Core safety invariants](#core-safety-invariants)
- [Runtime lifecycle](#runtime-lifecycle)
- [Getting started](#getting-started)
- [Using Nausicaa as a library](#using-nausicaa-as-a-library)
- [Context, skills, and memory](#context-skills-and-memory)
- [Workspace tools and process isolation](#workspace-tools-and-process-isolation)
- [Persistence and crash recovery](#persistence-and-crash-recovery)
- [JSON-RPC app server](#json-rpc-app-server)
- [Background task ledger](#background-task-ledger)
- [OpenAI-compatible provider details](#openai-compatible-provider-details)
- [Development and verification](#development-and-verification)
- [Current limitations](#current-limitations)
- [Repository layout](#repository-layout)

## Design goals

Nausicaa is built around three ideas:

1. **Keep authority outside the prompt.** Project instructions, skills, memories, and model responses provide context, but only Rust policy objects decide which tools are visible and executable.
2. **Make side effects explicit and recoverable.** A tool first normalizes a request into a `CanonicalAction`. Approval and execution use that exact value, and every model-emitted tool call receives a durable receipt.
3. **Keep product choices optional.** The core has no dependency on a particular model provider, terminal library, process sandbox, memory store, or application server.

This makes the core useful both as a small embedding library and as the foundation of the included TUI.

## Architecture

The runtime is divided into replaceable boundaries around a small agent loop:

```text
Client / control plane
        |
        v
  thread + turn API
        |
        v
  compile context  <----- rules / skills / advisory memory
        |
        v
   project tools   <----- policy + optional parent capability ceiling
        |
        v
     call model    <----- provider-neutral ModelAdapter
        |
        v
 normalize tool call into CanonicalAction
        |
        v
 policy re-check -> hooks -> exact-action approval, if required
        |
        v
 persist execution-start -> executor boundary -> persist receipt
        |                                         |
        +----------- next model context <---------+

Every durable transition is appended to the EventStore before observers see it.
```

The main extension seams are:

- `ModelAdapter` for model providers.
- `ContextCompiler` for prompt and transcript construction.
- `Tool`, `ToolRegistry`, and `ToolExecutor` for capabilities and execution backends.
- `ToolPolicy` and `ApprovalProvider` for authorization.
- `EventStore` and `EventObserver` for persistence and event delivery.
- `Hook` for deterministic checks before model calls and around tool execution.

## Workspace crates

| Package | Facade feature/module | Responsibility |
| --- | --- | --- |
| `agent-harness-core` | Always available as `agent_harness::core` | Thread/turn loop, model and tool protocols, capability projection, exact approval, hooks, cancellation, events, durable receipts, and recovery. |
| `agent-harness` | N/A | Feature-gated facade. It enables no optional features by default. |
| `agent-harness-context-fs` | `context-fs` / `context_fs` | Hierarchical `AGENTS.md` rules, skill discovery and selection, transcript compaction, and context-size enforcement. |
| `agent-harness-memory` | `memory` / `memory` | In-memory or JSONL advisory memory, lexical recall, and a frozen recall snapshot for each turn. |
| `agent-harness-executor-process` | `process-executor` / `process_executor` | Workspace-scoped file tools plus explicit local and Linux Bubblewrap process runners. |
| `agent-harness-provider-openai` | `provider-openai` / `provider_openai` | Non-streaming OpenAI-compatible Chat Completions mapping with a replaceable HTTP transport. |
| `agent-harness-task-ledger` | `task-ledger` / `task_ledger` | Idempotent background-task submission, worker leases, heartbeat, cancellation, recovery, and delivery acknowledgement. |
| `agent-harness-app-server` | `app-server` / `app_server` | Line-oriented JSON-RPC 2.0 thread and turn control plane. |
| `agent-harness-tui` | `tui` / `tui` | Crossterm UI, background turns, event display, cancellation, and interactive exact-action approval. |

`agent-harness-core` does not depend on any of the optional crates or on a TUI framework.

## Core safety invariants

### Capability projection is fail-closed

Policy access has three levels:

| Access | Model visibility | Execution behavior |
| --- | --- | --- |
| `Deny` | The tool definition is omitted from the model request. | A fabricated or stale call is denied and receives a receipt. |
| `Ask` | The tool definition is visible. | Execution requires an `ApprovalProvider` decision. |
| `Allow` | The tool definition is visible. | Execution can proceed without interactive approval. |

`CapabilityPolicy::deny_by_default()` denies every tool that is not explicitly granted. The runtime computes an effective projection from registered tools and policy. If a parent capability projection is supplied for a child agent, each child grant is restricted by the corresponding parent grant; a child cannot expand its parent's authority.

Visibility is not the only check. The effective projection is checked again when a tool call is handled, so a model cannot gain access by emitting the name of a hidden tool.

### Approval binds to a normalized action

Tools have separate `prepare` and `execute` phases:

1. `prepare` validates model arguments, resolves paths and defaults, describes effect and retry safety, and returns a `PreparedToolCall` containing a `CanonicalAction`.
2. Hooks, audit events, and an approver inspect that canonical action.
3. An approval is accepted only if the returned action is exactly equal to the prepared action.
4. `execute` receives the already prepared value rather than the original model arguments.

This prevents a tool from presenting one path, timeout, scope, or command for approval and silently executing another.

### Execution is explicit

`AgentRuntime` uses `RejectingExecutor` by default. Registering a tool is therefore not enough to make it executable. An embedding application must deliberately install an executor, such as `DirectExecutor` for trusted in-process tools or a custom process/container/remote boundary.

`DenyAllApprovals` is also the default approval provider. A tool with `Ask` access remains denied until an application supplies an approval implementation.

### Receipts are part of the protocol

Every accepted model-emitted call is closed with a `ToolReceipt`, including unknown tools, denied actions, malformed arguments, failed executions, cancelled calls that have not started, and actions whose outcome cannot be determined after a crash. Structurally invalid model responses, such as responses that reuse a call ID, fail the turn before those calls enter the transcript.

| Receipt status | Meaning |
| --- | --- |
| `succeeded` | Execution returned a value. |
| `failed` | Arguments or execution failed with a known result. |
| `denied` | Policy, a hook, approval, or cancellation before preparation prevented execution. |
| `unknown` | Execution started, but no durable result exists; the side effect may have happened. |

A successful tool result is not sent to the model until its receipt has been accepted by the event store. If execution crossed the executor boundary but receipt persistence fails, the runtime returns `ReceiptPersistenceUnknown` instead of continuing as though the outcome were known.

### Prompts are context, not authority

Directory rules, selected skill bodies, recalled memory, and volatile prompt segments are delivered to the model. They do not enter the policy engine and cannot change the capability projection. Memory is explicitly labelled advisory when added to a prompt.

## Runtime lifecycle

A turn proceeds as follows:

1. Verify that the thread exists and prevent concurrent turns on the same thread within the runtime.
2. Recover interrupted protocol edges for that thread without replaying external actions.
3. Persist `TurnStarted` and the user message.
4. Rebuild the transcript from durable user, assistant, and receipt events.
5. Project the registered tools through policy and the optional parent capability ceiling.
6. Compile context for the current iteration and run `before_model` hooks.
7. Persist context/model-request events, invoke the `ModelAdapter`, validate tool-call IDs, and persist the assistant message.
8. If the response contains no tool calls, persist `TurnCompleted` and return its content.
9. For each tool call, check registration and policy, prepare the canonical action, and run `before_tool` hooks.
10. If access is `Ask`, persist the approval request and resolution and require an exact action match.
11. Persist `ToolExecutionStarted`, cross the configured executor boundary, and durably append the resulting receipt.
12. Run `after_tool` hooks, add the receipt to the transcript, and begin the next model iteration.

The default limit is 32 model iterations per turn. Call IDs must be non-empty and unique for the entire turn. Hook failures and terminal turn states are also recorded as events.

Cancellation is cooperative. The runtime checks the token before model iterations and tool calls, tools receive the same token in `ToolExecutionContext`, and any remaining unhandled calls receive denied receipts before the turn is marked cancelled.

## Getting started

### Prerequisites

- Rust 1.85 or newer with Cargo.
- `curl` on `PATH` when using the included provider transport.
- Linux and `bwrap` when using the TUI's default shell runner.
- An OpenAI-compatible Chat Completions endpoint and model name for live model calls.

### Run the API-free TUI demo

This mode requires neither credentials nor Bubblewrap because tools are disabled:

```bash
cargo run -p agent-harness-tui -- --demo --no-tools --workspace .
```

The demo model echoes user input. It is a terminal and event-flow smoke test, not a model-quality demonstration.

If dependencies are already cached, add Cargo's `--offline` flag:

```bash
cargo run -p agent-harness-tui --offline -- --demo --no-tools --workspace .
```

### Run the minimal core example

The example registers an in-process `echo` tool, uses a scripted model, records events in memory, and verifies that the durable receipt appears in the next model request:

```bash
cargo run -p agent-harness-core --example minimal
```

The complete source is in [`crates/harness-core/examples/minimal.rs`](crates/harness-core/examples/minimal.rs).

### Connect the TUI to a live endpoint

```bash
export HARNESS_API_URL="https://api.openai.com/v1/chat/completions"
export HARNESS_MODEL="<model-name>"
export HARNESS_API_KEY="<token>"

cargo run -p agent-harness-tui -- --workspace .
```

`OPENAI_API_KEY` is used as a fallback when `HARNESS_API_KEY` is absent. The default endpoint is `https://api.openai.com/v1/chat/completions`, so `HARNESS_API_URL` is optional for that endpoint.

### TUI options and controls

```text
--demo                Use the built-in offline demo model
--workspace <path>    Set the workspace and event-store location
--endpoint <url>      Set the OpenAI-compatible Chat Completions URL
--model <name>        Set the provider model name
--no-tools            Do not register the workspace tools
--unsafe-local-exec   Run shell commands directly on the host
-h, --help            Show command help
```

Command-line values take precedence over `HARNESS_API_URL` and `HARNESS_MODEL`. API keys are read from `HARNESS_API_KEY` and then `OPENAI_API_KEY`.

Inside the TUI:

- Enter sends a message when no turn is active.
- `/cancel` requests cancellation of the active turn.
- `/quit`, `/exit`, Escape, or Ctrl-C exits.
- `y` approves the displayed canonical action.
- `n` or Escape denies a pending approval.

The default tool policy is:

| Tool | Access | Notes |
| --- | --- | --- |
| `read_file` | `Allow` | Reads a UTF-8 file under the workspace after path validation. |
| `write_file` | `Ask` | Displays the normalized path and full canonical arguments before approval. |
| `shell` | `Ask` | Uses Bubblewrap by default, with no network and the workspace as its only writable host bind. |

TUI events are stored at `<workspace>/.agent-harness/events.jsonl`.

> **Warning:** `--unsafe-local-exec` selects `LocalProcessRunner`. Shell commands then inherit host-level process access and are not sandboxed. The conspicuous option name is intentional.

## Using Nausicaa as a library

The facade always exposes the core and enables optional planes through features:

```toml
[dependencies]
agent-harness = { path = "crates/harness", features = [
    "context-fs",
    "memory",
    "process-executor",
    "provider-openai",
] }
```

Within this repository the path above is correct. External path users should point it at their local checkout. Enable everything with `features = ["full"]`, or depend directly on individual workspace crates.

The facade maps features to modules such as `agent_harness::context_fs`, `agent_harness::process_executor`, and `agent_harness::provider_openai`.

### Minimum runtime assembly

`AgentRuntime::new` requires five application choices:

| Input | Purpose |
| --- | --- |
| `Arc<dyn ModelAdapter>` | Converts compiled context and visible tools into a model response. |
| `Arc<dyn EventStore>` | Durably appends and reloads runtime events. |
| `Arc<dyn ContextCompiler>` | Builds ordered prompt segments and transcript messages. |
| `ToolRegistry` | Holds tool schemas and implementations. An empty registry is valid. |
| `Arc<dyn ToolPolicy>` | Projects registered tools to `Deny`, `Ask`, or `Allow`. |

Optional builder methods install policy context, a parent capability ceiling, an approval provider, an executor, hooks, observers, and `RuntimeConfig`.

The basic control flow is:

```rust
let runtime = AgentRuntime::new(model, store, compiler, tools, policy)
    .with_executor(executor);

let thread_id = runtime.start_thread()?;
let outcome = runtime.run_turn(&thread_id, "Do the work").await?;
println!("{}", outcome.content);
```

The core does not select an async runtime. Its public model, approval, executor, and tool boundaries return `BoxFuture`, allowing the embedding application to choose its own executor. The included provider and process implementations are currently blocking internally.

### Implementing a tool

A custom `Tool` should:

1. Return a stable name, description, and JSON input schema from `definition`.
2. Reject invalid model arguments in `prepare`.
3. Resolve aliases, defaults, paths, scope, effect kind, and retry safety before returning a `CanonicalAction`.
4. Preserve the original call ID and tool name.
5. Execute only the prepared action and honor cooperative cancellation where practical.

Tool registration rejects empty or duplicate names. A tool implementation cannot make itself visible; visibility still comes from effective policy.

## Context, skills, and memory

### Filesystem context compiler

`FsContextCompiler` constructs context deterministically from:

1. configured stable prompt segments;
2. directory rules from project root to the current directory;
3. a compact index of discovered skills;
4. the complete bodies of explicitly selected skills;
5. configured volatile segments; and
6. an explicit compaction notice when older transcript groups were omitted.

For each directory, `AGENTS.override.md` takes precedence over `AGENTS.md`. Only one is loaded from that directory. Rule paths are canonicalized, the current directory must remain under the project root, and symlinked rule files cannot escape the root.

A skill root may contain Markdown files or child directories containing `SKILL.md`. The scanner uses `name` and `description` frontmatter when present, with basic path and first-prose-line fallbacks. Every discovered skill contributes only its summary to the index; full text is loaded only for names in `selected_skills`.

Transcript compaction keeps complete groups from the end of the conversation. An assistant message containing tool calls and its following receipts form one indivisible group, so compaction does not intentionally separate a request from its result. The compiler reports how many groups were omitted and never invents a semantic summary. An optional character budget fails context compilation when the retained prompt and transcript are still too large.

### Advisory memory

`agent-harness-memory` provides `InMemoryStore` and append-only `JsonlMemoryStore`. The default recall algorithm ranks records by case-insensitive lexical overlap, an exact-phrase bonus, recency, and stable ID ordering.

`MemoryContextCompiler` decorates another compiler. On the first compile of a turn it recalls records using the latest user message, then freezes that result for every subsequent model iteration in the same turn. Memory writes during a turn therefore become visible only to later turns. Recalled text is added as an advisory prompt segment and has no path to tool policy or approval.

## Workspace tools and process isolation

`register_workspace_tools` installs three tools:

### `read_file`

- Accepts a non-empty relative path without `..` or absolute components.
- Canonicalizes the existing target and requires it to be a file inside the workspace.
- Reads UTF-8 content only.
- Uses a 1 MiB default limit, configurable through `ReadFileTool`.
- Produces a read-only, safely retryable canonical action.

### `write_file`

- Accepts a relative path, UTF-8 content, and optional `create_directories` flag.
- Rejects paths whose existing target or nearest existing ancestor resolves outside the workspace.
- Rechecks the target immediately before writing, including after creating parent directories.
- Uses a 1 MiB default content limit, truncates or creates the file, and calls `sync_data`.
- Produces a workspace-write, idempotent canonical action.

### `shell`

- Runs `/bin/sh -lc <command>` in the workspace.
- Uses a 30-second default timeout clamped to a 120-second default maximum.
- Captures stdout and stderr separately and reports exit code, timeout, and truncation flags.
- Produces a workspace-write action marked unsafe to retry.

The TUI configures a 1 MiB output cap for each process stream.

`BubblewrapRunner` is the default TUI backend. It unshares namespaces, does not share the network namespace by default, clears the environment, mounts `/usr`, `/bin`, `/lib`, and `/lib64` read-only when present, provides isolated `/proc`, `/dev`, and `/tmp`, and makes the workspace its only writable host bind. Additional read-only binds and network access must be enabled explicitly in code.

`LocalProcessRunner` is an explicit unsandboxed alternative. File path and symlink checks reduce accidental workspace escape for `read_file` and `write_file`, but path validation alone is not equivalent to operating-system isolation.

## Persistence and crash recovery

The core includes `MemoryEventStore` for tests and ephemeral embeddings and `JsonlEventStore` for local persistence. Each JSONL append is written, flushed, and `sync_data`'d before it is reported as successful. Observers are notified only after that append succeeds.

Important runtime events include thread and turn boundaries, user and assistant messages, compiled-context metadata, approval requests and decisions, prepared actions, execution starts, receipts, hook failures, cancellation, and terminal turn results. Replaying user, assistant, and receipt events reconstructs the model transcript.

Recovery deliberately closes incomplete protocol edges without replaying side effects:

| Last durable state for a call | Recovery result |
| --- | --- |
| Assistant requested the call, but preparation was not recorded | Record a failed receipt: interrupted before preparation. |
| `ToolPrepared` exists, but `ToolExecutionStarted` does not | Record a failed receipt: execution is known not to have begun. |
| `ToolExecutionStarted` exists, but no receipt exists | Record an `unknown` receipt because the action may have happened. |
| A receipt already exists | Leave the call unchanged. |

Every interrupted open turn is then marked failed. `run_turn` performs recovery before starting new work on the thread, and callers can invoke recovery explicitly through the core API or app server.

The JSONL stores synchronize access within one process. They do not implement cross-process locking or distributed consensus.

## JSON-RPC app server

`AppServer::serve` reads one JSON-RPC 2.0 value per line and writes one response per line. Requests without an `id` are treated as notifications and produce no response. This crate is a library; the workspace does not currently ship a standalone app-server binary.

| Method | Parameters | Result |
| --- | --- | --- |
| `server/health` | `{}` | `{ "status": "ok" }` |
| `thread/start` | `{}` | A newly generated `thread_id`. |
| `thread/events` | `thread_id`, optional `after_sequence` | Durable event envelopes, filtered to sequences greater than the cursor. |
| `thread/recover` | `thread_id` | Receipts created and turns failed during recovery. |
| `turn/start` | `thread_id`, `input` | A control-plane-generated `turn_id`; work continues on a background thread. |
| `turn/status` | `turn_id` | `running`, `completed`, `failed`, or `cancelled`, plus associated data. |
| `turn/cancel` | `turn_id` | Confirms that cooperative cancellation was requested. |

Example request:

```json
{"jsonrpc":"2.0","id":1,"method":"thread/start","params":{}}
```

Example response shape:

```json
{"jsonrpc":"2.0","id":1,"result":{"thread_id":"thread-generated-id"}}
```

After `turn/start`, clients can poll `turn/status` and incrementally read durable events with `thread/events`. Turn status is held in the app-server process, while the event history is held by the runtime's configured event store.

The server uses standard JSON-RPC codes for parse errors, invalid requests, unknown methods, invalid parameters, and internal runtime errors.

## Background task ledger

`JsonlTaskLedger` is a separate durable primitive for work that outlives an interactive turn. It supports:

- idempotent submission keyed by an application-provided string;
- oldest-first pending-task claims;
- worker identity, leases, and heartbeats;
- success, failure, and cancellation terminal states;
- `unknown` recovery when a running worker lease expires; and
- explicit acknowledgement that a terminal result was delivered.

The normal state flow is:

```text
pending --claim--> running --succeed/fail--> succeeded/failed
   |                  |                         |
   +----cancel--------+------cancel----------> cancelled
                      |
                      +--expired lease-------> unknown

terminal state -> delivery pending -> delivery acknowledged
```

Submitting the same idempotency key returns the original task even if the new payload differs. Claiming increments the attempt count. An expired running lease becomes `unknown`, not `pending`, because its side effects may already have happened and automatic replay would be unsafe.

The ledger is a storage/state-machine component. It does not include a resident worker service, gateway, scheduler, or channel router.

## OpenAI-compatible provider details

`OpenAiCompatibleAdapter` maps ordered prompt segments to system messages, transcript entries to user/assistant/tool messages, and visible `ToolDefinition` values to Chat Completions function tools. It maps provider tool calls, finish reasons, and token usage back into the core protocol.

`OpenAiConfig` supports endpoint, model, optional bearer key, optional organization, timeout, and additional request-body fields. The included `CurlTransport` accepts only HTTP(S) URLs and sends its generated curl configuration—including headers and request body—through stdin instead of exposing the bearer token in process arguments. `HttpTransport` can be replaced for another HTTP stack or for deterministic tests.

The adapter currently returns one completed response and performs blocking transport work inside its future. Streaming and automatic retry orchestration are not implemented.

## Development and verification

Format, lint, test, and build documentation for every crate and feature:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --offline -- -D warnings
cargo test --workspace --all-features --offline
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --offline
```

Remove `--offline` when dependencies are not already cached.

The core integration tests cover capability restriction, exact-action approval, receipt ordering, cancellation, recovery, durable JSONL replay, hierarchical rules, deterministic compaction, and context limits. Optional crates also contain focused unit tests for their adapters and state machines.

## Current limitations

- The included provider supports non-streaming Chat Completions only. Its curl transport is blocking, and the core does not automatically retry `ModelError` values marked retryable.
- Deterministic compaction drops complete old message groups and records the count; it does not generate a semantic summary.
- JSONL stores provide synchronized append and `sync_data` durability within one process, not cross-process distributed locking.
- Bubblewrap is the only included operating-system sandbox and is Linux-specific. Container, VM, SSH, and cloud-sandbox executors remain extension points.
- Cancellation is cooperative. A blocking provider or process runner may not observe it until the current blocking operation returns.
- The app server's background-turn status table is process-local even when runtime events use durable storage.
- The task ledger has recovery semantics but no resident gateway, routing layer, or distributed worker implementation.
- Parent/child capability intersection is implemented, but a complete subagent/worktree scheduler is not included.
- Workspace path and symlink validation are defense-in-depth checks, not substitutes for a process sandbox.

## Repository layout

```text
crates/
├── harness-core/       # Mandatory safety and agent-loop core
├── harness/            # Feature-gated facade
├── context-fs/         # Optional project rules, skills, and compaction
├── memory/             # Optional advisory long-term memory
├── executor-process/   # Optional file tools and process backends
├── provider-openai/    # Optional OpenAI-compatible adapter
├── task-ledger/        # Optional durable background-task state machine
├── app-server/         # Optional JSON-RPC control plane
└── tui/                # Optional Crossterm library and executable
```

All workspace packages declare `MIT OR Apache-2.0` licensing.
