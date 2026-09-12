# Nausicaa development roadmap

Baseline: 2026-09-11, reliability commit `8bb9e1e`. This is a development plan,
not a list of implemented APIs. Development now continues with the optional
task-acceptance baseline; later stages remain gated by their acceptance evidence.

## 1. Reliability baseline — completed within the reviewed scope

| Failure | Implemented behavior | Regression coverage |
| --- | --- | --- |
| Truncated response reported as successful | Validate stop/call contract, retain partial output, deny accepted calls, fail the turn | Abnormal stops with/without calls; normal tool loop remains covered |
| Explicit recovery fails an active turn | Share the runtime's per-thread guard with execution | Reject recovery without changing events; recover after dropping the turn future |
| Partial final event blocks all history | Archive an incomplete tail before repairing; preserve a complete record without newline | Replay, append after repair, unchanged corruption rejection |
| Descendants keep output open after timeout | Unix group cleanup before leader reaping; nonblocking bounded output drain | Timeout, background children, continuous stdout/stderr, externally held writer, normal exit |
| Wrong or expired worker can finish a task | Require matching worker/attempt and unexpired trusted-host time | Wrong owner/attempt, expiry boundary, heartbeat extension, replay, late results after cancel/recovery |

Cancellation received during a model call also prevents subsequent successful
completion. Ledger worker API signatures changed; see the migration example in
[README.md](README.md#background-task-ledger).

Validation at this baseline: 37 workspace tests passed, plus strict Clippy,
rustfmt, and rustdoc checks. A separate Bubblewrap smoke test verified that a
new process group is compatible with its `--new-session` setup. The tests ran on
Linux with Rust 1.98.0; this is not evidence of cross-platform containment or of
the manifest's declared Rust 1.85 minimum. Some process tests require local IPC
facilities unavailable in restricted sandboxes.

Scope boundaries still apply: no cross-instance locking, no automatic task
resumption, no immediate cancellation of blocking calls, no tail repair in memory
or task-ledger stores, and no guaranteed cleanup of local processes that escape
their process group. See [ARCHITECTURE.md](ARCHITECTURE.md).

## 2. Task acceptance and an evaluation baseline — narrow baseline completed

**Objective:** distinguish "the model ended a turn" from "the requested work has
verifiable completion evidence."

Implemented first slice: optional `agent-harness-task` (facade feature `task`)
provides `TaskSpec`, `CompletionGate`, attempt `Budget`, retained `ArtifactRef`
snapshots, and a versioned `TaskJournal`. Required exact-content checks fail
closed; unknown receipts block progress; replay never retries a running attempt.
The normal embeddable core turn API is unchanged. Six deterministic regression
tests cover repair, budget exhaustion, receipt retention, interrupted attempts,
invalid replay, and append failure. `agent-harness-eval` now runs five fixtures
in isolated directories in both modes, retaining source, events, evidence, and
metrics. Its separate held-out fixtures cover identity and unknown execution.
Explicit opt-in live configuration is available but was not run for this baseline.

- Store the objective, explicit acceptance criteria, artifact references,
  verification evidence, and remaining work independently of chat history.
- A gate should accept evidence, return actionable repair feedback, or record a
  blocked/budget-exhausted outcome. It must not create an unlimited retry loop.
- Introduce an offline evaluation runner with scripted models and isolated task
  fixtures. Add opt-in pinned live-model runs for measuring actual task quality.
- Record success, false completion, iterations, time, token usage, interventions,
  and model/harness configuration. Keep a held-out set for regressions.

**Acceptance:** a task with failing required checks cannot be marked successful;
passing checks reference inspectable artifacts; repeated failure terminates under
a budget; denied and unknown actions remain correctly recorded. Demonstrate a
small coding task that fails verification, is repaired, and then passes. Run the
same fixtures against the core-only baseline and the task layer.

**Initial decisions:** creation, attempt-start, verification inputs, and explicit
blocking are durable version-1 task events. The trusted host fixes criteria and
captures UTF-8 artifacts under logical IDs; complete snapshots are retained inline.
The gate recomputes exact-content evidence on replay. Repair feedback is advisory
context supplied by the host. See [ARCHITECTURE.md](ARCHITECTURE.md#task-acceptance).
The evaluation runner must measure this narrow baseline before broadening it.

**Measured acceptance (2026-09-11):** the coding fixture first fails exact-source
verification, receives repair feedback, and passes on its second attempt. Repeated
failure exhausts two attempts. Denied receipts remain denied; interruption yields
one unknown receipt and blocks further attempts. The paired suite reports 3/5
false completions under the naive core-turn interpretation and 0/5 with the task
layer; independently verified success is 1/5 versus 2/5. See the
[evaluation guide](crates/eval/README.md) for definitions and reproduction.

This completes the deterministic baseline, not general coding evaluation:
artifacts are UTF-8 snapshots, the oracle is exact content, budgets count attempts,
and general live model quality has not been established. A completed
[Nemotron free-route validation](crates/eval/reports/2026-09-11-nemotron-free.md)
now covers eight live runs: all responses arrived, no exact-content task passed,
and six generated artifacts passed supplementary compilation/behavior checks.
Whitespace-only differences, bounded repair failures, and mutable provider routing
limit the conclusions; this is provider/harness validation, not broad coding
quality evidence. General semantic checkers, stronger
artifact retention, and real model comparisons need their own measured changes.

## 3. Sustained execution — context baseline implemented

Deliver these as separate reviewable changes, not one large runtime replacement.

| Area | Intended change | Acceptance evidence |
| --- | --- | --- |
| Context | Account for the final request's token budget, preserve task constraints, externalize long outputs, retrieve evidence on demand | Long-output and compacted-history fixtures retain goals and tool/receipt integrity; final request remains within budget |
| Editing tools | Add focused search, paginated reads, and patches with explicit conflict checks | Multi-file fixture produces a reviewable patch and detects stale input instead of overwriting it |
| Provider/runtime | True async transport, streaming events, bounded retry of eligible model errors, time/token budgets | Cancel a pending request promptly; streaming/interrupted responses never execute partial tool calls; retries cannot duplicate tool effects |
| Persistence | Define checkpoints, durable waiting states, and ownership for resumption | Restart at model/approval/execution/receipt boundaries; resume known-safe work and surface unknown side effects without replay |
| Execution | Separate execution identity/status from the model worker and supervise long-lived work | Restart the worker and reconcile a running/completed execution using its ID; cleanup and resource limits remain observable |
| Control plane | Reconstruct status, expose resume, retain delivery state | A client can reconnect after restart and observe the correct task/turn state and pending actions |

**Implemented context slice:** `TaskContextCompiler` pins the immutable objective
and required criteria after transcript compaction without altering call/receipt
groups. `ExternalOutputCompiler` now retains long output in immutable per-thread
archives, substitutes versioned context references, and offers explicitly
authorized `read_output` pages. The optional provider `RequestBudget` checks the exact final request using
a trusted model-specific `RequestTokenCounter`, reserving bounded output capacity
and rejecting over-budget or uncountable requests before transport. Tool schema
size and extension fields are covered. Provider extensions can no longer restore
hidden tools when the core projection is empty.

Focused regression coverage includes compacted-away initial instructions, preserved
denied call/receipt groups, oversized schemas, exact budget boundaries, invalid
output caps, counting errors, and extra-body schema injection. Archive tests cover
Unicode paging, snapshot reuse/conflicts, symlink rejection, exact prepared
retrieval, and denied access. A combined fixture drops old history and externalizes
55 KB of output while retaining task constraints and call/receipt integrity under
a final-request test-counter budget. This satisfies the Context row at the
embedding boundary; production model tokenizers, token-driven automatic
compaction, archive indexing/GC, and cumulative task budgets remain extensions.
The next ordered implementation area is Editing tools.

**TUI requests, progress and cancellation:** validated CLI/environment settings now expose the
model request timeout, completion-token cap, temperature, per-turn iteration
limit, and retained transcript groups. The provider and TUI now support bounded draft text, phase timings with a separate
version-1 metrics file, and cancellation of in-flight curl requests. Local HTTP
tests cover previews before completion, cancellation closing the connection and
missing stream terminators; UI tests cover stale previews and metric privacy.
Native async HTTP and automatic retry remain proposed. Built-in shell cancellation
is now wired through `run_controlled`: bounded polling, Unix group cleanup and
unknown-effect receipts are tested. Custom runner interruption remains the
implementer’s responsibility.

Validation on 2026-09-12 also exercised the actual TUI against a local SSE fixture:
preview display, active Ctrl-C staying in the UI, socket closure, cancellation in
the journal, metrics without draft text, and idle Ctrl-C exit all passed. A single
live TUI request to `nvidia/nemotron-3.5-lightning:free` (256-token cap, no tools)
hit its 60-second deadline at 60,002 ms without visible text. The failure metric
was recorded; network phases were unavailable. This does not establish live SSE
compatibility or identify upstream queue versus network delay. Live credentials
remain unnecessary for deterministic tests.

The subsequent [DeepSeek validation](crates/provider-openai/reports/2026-09-12-deepseek.md)
completed real short and streaming replies, cancelled an active streamed reply,
and passed one exact-approved Bubblewrap tool round trip.
A subsequent manual-start regression also covers CRLF keys loaded via shell
command substitution: the TUI trims surrounding whitespace, while transport
validation continues to reject interior header newlines. This confirms the tested endpoint's SSE compatibility; it does not explain the
previous free-route timeout. The actual TUI also cancelled an explicitly approved
shell on each of the local and Bubblewrap backends in 51 ms, preserving an
unknown-effect receipt before turn cancellation.

Reserve verification time in a task budget rather than spending the entire budget
on generation. Automatic compaction or resumption must preserve user constraints
and the provenance of verification evidence.

## 4. Measured orchestration and adaptation — exploratory

Enter this stage when the single-agent task baseline and its metrics are useful.

- **Independent reviewer:** use tests or a narrowly scoped evaluator with explicit
  criteria. Measure false completion and cost against the single-agent baseline.
- **Subagents:** define bounded inputs and outputs, isolated contexts/worktrees,
  inherited capability ceilings, shared budget accounting, and cancellation
  propagation. Test failed children and merge conflicts before adding concurrency.
- **Dynamic tools:** introduce discovery when tool/schema volume warrants it.
  Loaded tools still pass normal policy, preparation, approval, and receipt paths.
- **Memory:** add provenance, repository/version scope, invalidation, and
  correction before optimizing semantic recall. Evaluate useful recall and stale
  recall separately.
- **Model profiles:** measure prompt/tool/context settings per model. Keep
  profiles optional and versioned; compare cost, latency, and task success.
- **Trace-driven improvement:** let an agent propose harness changes from failed
  traces, then evaluate on held-out tasks. Adoption must be reversible and based
  on measured improvement, not the agent's own approval of its change.

**Acceptance:** each added mechanism improves a named metric without unacceptable
regressions on safety invariants, held-out tasks, cost, or latency. Configuration
and artifact versions must make comparisons reproducible.

## Maintenance work to schedule explicitly

- Verify or correct the declared minimum Rust version, then automate that check.
- Add CI for the documented checks and a supported-platform matrix; distinguish
  protocol tests from environment-dependent sandbox integration tests.
- Decide whether to share journal recovery across event, task, and memory stores
  or introduce transactional storage. Define schema migration and ownership first.
- Extend fault injection to append failures, lifecycle boundaries, and worker
  interruption. The current green tests do not prove every crash point is safe.
- Review long-history memory growth, event snapshots/indexing, observer backpressure,
  and sensitive-data handling before treating traces as a production dataset.

## Design references

These inform proposed experiments; their results are not Nausicaa benchmarks.

- [Anthropic: harness design for long-running application development](https://www.anthropic.com/engineering/harness-design-long-running-apps)
  (2026-03-24): explicit sprint contracts and a separate evaluator motivate stage 2.
- [LangChain: improving Deep Agents with harness engineering](https://www.langchain.com/blog/improving-deep-agents-with-harness-engineering)
  (2026-02-17): trace analysis and controlled evaluation motivate the measurement loop.
- [Anthropic: scaling Managed Agents](https://www.anthropic.com/engineering/managed-agents)
  (2026-04-08): separate session, harness, and sandbox lifecycles inform stage 3.
- [Deep Agents architecture](https://github.com/langchain-ai/deepagents/blob/main/libs/ARCHITECTURE.md):
  optional middleware, backends, and profiles provide comparison points. This is
  a moving upstream document, not a pinned implementation dependency.
