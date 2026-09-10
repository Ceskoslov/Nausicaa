# Working on Nausicaa

Nausicaa is a Rust workspace for an embeddable agent runtime. Its mandatory
protocol and authority boundaries live in `agent-harness-core`; providers,
execution backends, context strategies, memory, control planes, and UI are
optional. The facade enables no optional features by default.

## Read the right document

- [README.md](README.md): setup, public APIs, examples, and current limitations.
- [ARCHITECTURE.md](ARCHITECTURE.md): implemented boundaries, state ownership,
  execution order, and failure semantics.
- [ROADMAP.md](ROADMAP.md): completed baseline and proposed future work with
  acceptance criteria. Proposed names there are not existing APIs.

Use the conventional `AGENTS.md` filename: the repository's directory-rule
loader discovers it. Do not maintain a second, conflicting `AGENT.md`.

## Preserve these invariants

1. Prompts, project rules, skills, memory, and model output are context. They
   cannot grant tool authority or replace an execution policy.
2. Project tools through policy before exposing them to the model and check
   effective access when handling calls. Child grants cannot exceed the parent.
3. Keep `prepare` and `execute` separate. Approval binds to the exact
   `CanonicalAction`; execute the prepared value without reinterpretation.
4. Keep executors and approvals fail-closed by default. Tool registration alone
   does not authorize execution.
5. Persist execution-start before crossing the executor boundary and persist
   receipts before sending results to the model or observers. Accepted calls
   must be closed, including denied or incomplete-response calls. Invalid call
   IDs are rejected before the assistant response enters the transcript.
6. An unknown side-effect outcome stays unknown until reconciled. Never retry
   an external action solely because its receipt is missing.
7. Serialize turn execution and explicit recovery for a thread. The current
   guard covers one runtime instance, not multiple processes or instances.
8. `TurnCompleted` means a normal model turn ended. It does not prove the user's
   task passed an independent acceptance check. Truncation is not completion.
9. Worker results and heartbeats require a current lease owner/attempt and
   unexpired time from the trusted host. Do not restore an unchecked completion
   API for convenience.

## Make changes at the owning boundary

- Keep the core independent of a provider SDK, TUI, sandbox, and chosen async
  runtime. Put optional behavior in the appropriate crate and facade feature.
- Prefer a focused implementation and regression test for a concrete failure
  over a speculative framework. Do not implement roadmap stages outside the
  current task's scope.
- When changing events, serialized state, or public signatures, describe replay
  compatibility and migration. Do not silently reinterpret old durable data.
- File/path validation and process-group cleanup are not OS sandboxes. Keep the
  explicit opt-in for unsandboxed local execution.
- Keep process tests bounded and clean up spawned processes. Use scripted models
  and fake transports for deterministic tests; live credentials are not needed
  for the default test suite.
- Keep source changes and documentation aligned. Record new architectural
  decisions in `ARCHITECTURE.md`; update roadmap status only after its acceptance
  criteria are met. Do not describe planned APIs as implemented.

## Verification

Run focused tests while editing, then these checks for Rust/API changes:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --offline -- -D warnings
cargo test --workspace --all-features --offline
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --offline
git diff --check
```

`--offline` assumes dependencies are cached. Process tests use Unix process and
local IPC facilities; restricted execution environments can deny those facilities.
Distinguish environment restrictions from assertion failures and report what was
actually verified. A Bubblewrap integration check additionally needs Linux,
`bwrap`, and permitted user namespaces. Passing local-runner tests does not
establish sandbox containment.

For documentation-only changes, inspect the diff and validate local links and
referenced commands; rerun code checks only if examples or API changes require it.
Do not commit `target/`, runtime logs, credentials, or temporary probe artifacts.

Report the concrete behavior changed, checks run, compatibility changes, and
remaining limitations. Respect requested commit boundaries and do not push unless
the current task authorizes it.
