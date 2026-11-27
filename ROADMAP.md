# Nausicaa development roadmap

Nausicaa should be the embeddable runtime for trusted hosts that need tool
authority, durable receipts, and explicit recovery. Ohmu is the first
application to use for compatibility checks. Teto consumes evidence and owns
evaluation and release policy.

Start by testing real producer artifacts against Teto and running the Ohmu
consumer against candidate Nausicaa revisions. Then make local restart status
visible to clients. Schedule execution supervision when a concrete operation
needs to outlive its host.

## Assessment as of 2026-11-27

| Area | Implemented | Constraint on the next release |
| --- | --- | --- |
| Core, facade, and event storage | Policy projection and effective-access checks, exact prepared-action approval, execution-start and receipt ordering, thread serialization, Unix JSONL ownership, and explicit v1 model-boundary resumption. The facade has no default optional features. | Pending approval, incomplete tool batches, and unknown receipts require explicit recovery. V1 resumption excludes hooks and process-local task budgets. Upgrade readers before writing opt-in resume events. |
| Task acceptance, ledger, and evaluation | The optional task journal accepts exact retained artifacts. The ledger fences worker results using the live owner, attempt, and expiry. The evaluation crate runs scripted exact-content and Rust behavioral fixtures and retains evidence. | The trusted host captures artifacts and receipts. The task journal and ledger have separate ownership and failure semantics. A normal turn, an accepted task, and a delivered background result are distinct. |
| Provider, context, and memory | The optional provider supports curl and native async Chat Completions with SSE, request accounting, and opt-in cumulative budgets and bounded model retries. Context can pin task criteria and archive long output. Memory provides advisory lexical recall. | Hosts supply the trusted model token counter. Durable continuation of budgeted work remains open. Ohmu owns its own source retrieval and citations. |
| Process execution and editing | Bounded workspace reads, exact prepared patches, and cancellation for built-in process runners. Bubblewrap provides opt-in Linux sandboxing. | The process backend has no durable supervisor. Unknown effects require reconciliation. Sandbox containment needs its own validation. |
| App server, TUI, and CI | JSON-RPC exposes events and recovery. Turn status and cancellation handles live in memory. The TUI starts a fresh thread on startup. Linux CI checks the core and optional crates, including Rust 1.85 support. | Clients lack a restart-facing status contract. Preserve the Linux minimum-toolchain and no-default-feature checks when adding integration jobs. |

See the [architecture](ARCHITECTURE.md), [current limitations](README.md#current-limitations),
and [evaluation guide](crates/eval/README.md) for implementation details and
historical evidence. The preceding roadmap is available with
`git show b23edbb:ROADMAP.md`.

### Cross-repository evidence

The review used local Nausicaa `b23edbb`, Teto `38a1256` (v0.3.0), and Ohmu
`8a52985`. Ohmu's manifest and CI pin Nausicaa `a2ab5b3`. Teto's regression
job already pins `b23edbb` and scores a committed synthetic Ohmu artifact.

- A fresh Nausicaa `task-baseline-v1` run imported into Teto. The paired gate
  returned `release` for core-only to task mode and `hold` in reverse. Scoring the
  same task evidence twice produced identical report bytes. The separate Teto
  scripted suite also ran, scored, and passed its standalone gate.
- Nausicaa also produced `rust-behavior-v1` evidence. Teto's Nausicaa scorer
  rejected it as `UNSUPPORTED_NAUSICAA_SUMMARY`. The current importer contract
  targets the exact-content suite. Teto's standalone Suite v1 runner has its
  own exact UTF-8 oracle and should not replace the Rust behavioral verifier.
- Ohmu's temporary-consumer probe compiled against local Nausicaa with one core
  and one native provider using a workspace-root Cargo patch. This verifies
  source identity and type compatibility. Runtime behavior under the
  candidate revision and an update to Ohmu's Git pin remain to be verified.
- A loopback fixture drove an existing Ohmu build through `ask` and
  `scripts/evaluate.py`. Teto scored the resulting `evaluation.json` against
  the original annotations. Host acceptance and abstention passed. Quote
  verification was unscored because the answer contained no quotes. The probe
  used a scripted response and a fixture counter. It establishes artifact
  compatibility for abstention only.
- Ohmu verifies quote identity against immutable source bytes. Teto uses the
  host verdict for that check and recomputes expected-source recall and
  abstention from annotations. Semantic support and contradiction handling need
  independent labels. The current Ohmu exporter aborts on a failed CLI invocation
  and retains its journals. A missing case must never improve a release verdict.

## Ownership and compatibility

| Repository | Owns | Integration contract |
| --- | --- | --- |
| Nausicaa | Turn and tool protocol, durable execution evidence, host recovery, optional task acceptance, and deterministic runtime fixtures. | Rust core and provider APIs for Ohmu. Existing versioned suite artifacts for Teto. |
| Ohmu | Markdown content, source revisions and locators, lexical search, snapshots, draft publication and reconciliation, and wiki workflow acceptance. | `ohmu-nausicaa` composes one runtime. `evaluation.json` and the original case annotations go to Teto. |
| Teto | Suite orchestration, scoring, calibration, release decisions, and any future evaluation job service. | Versioned producer artifacts in. Score reports and gate results out. |

Keep adapters in the consumer or the optional evaluation host. Use one Cargo
source per harness package in an Ohmu host, with portable Git pins for releases
and workspace-root patches for candidate testing. Keep production evidence in
producer-owned stores. Pass selected artifacts and digests at the boundary.
Record the producer, fixture, scorer, and policy revisions used for a decision.
A scorer verdict grants no tool capability, changes no runtime receipt, and
authorizes no publication. Wiki content and repair feedback remain advisory.

Keep v1 journals and evidence readers compatible. When an extension changes the
meaning of a field or verdict, version that artifact and add a test using an
older producer. Do not rewrite journals to fit an evaluation schema. Use public
or synthetic evidence in CI. Retain prompts, private wiki text, and raw tool
output only in explicitly authorized stores.

## Delivery order

A and B can branch independently. Land both before C. D can start after A and
does not depend on the Ohmu pin bump or the behavioral scorer. E is gated by D
and an operation that needs restart reconciliation. F follows B and C and can
proceed without D or E. All items below are proposed.

| Step | Owner and likely files | Dependency |
| --- | --- | --- |
| A. Test the runtime producer against its evaluator | Nausicaa `.github/workflows/` and `crates/eval/` | None |
| B. Test the wiki consumer against candidate revisions | Ohmu `scripts/check_local_host.py`, `crates/ohmu-nausicaa/`, CI and compatibility docs | None |
| C. Gate knowledge artifacts from the real producer | Ohmu `scripts/evaluate.py` and workflow fixtures, Teto scoring tests and CI | A and B |
| D. Surface durable local status and explicit continuation | Nausicaa `crates/app-server/`, with read-only TUI status as a later client change | A |
| E. Reconcile one independently durable operation | Owning execution backend, with a small optional Nausicaa adapter if needed | D and a named host use case |
| F. Expand evaluation with independent behavioral and semantic evidence | Nausicaa `crates/eval/`, Ohmu held-out annotations, and Teto scorers and policies | B and C |

### A. Test the runtime producer against its evaluator

- [ ] Run the current checkout's exact-content fixture CLI in Nausicaa CI and
  feed the artifact to a pinned Teto. Keep the existing Teto-owned static
  producer check as coverage for its supported baseline.
- [ ] Check both gate directions. Keep reports on holds and errors. Check that
  the planned denial and unknown interruption retain their receipt expectations,
  unexpected turn errors fail the integration check, and rescoring identical
  bytes produces identical reports.
- [ ] Keep credentials and live endpoints out of this job. Run the existing
  Rust 1.85 and empty-default-facade checks independently.

### B. Test the wiki consumer against candidate revisions

- [ ] Keep the pinned, locked Ohmu job. Add a candidate-revision check using the
  temporary consumer with `--with-provider`. Test both the dependency graph
  and an actual read or cited workflow using that candidate core and provider.
- [ ] Verify hidden apply denial, exact approved apply in the maintenance path,
  stale-snapshot handling, receipt persistence before the next model request,
  and reconciliation by Ohmu operation ID after an interrupted publication.
  Reopen a supported journal when evaluating a pin change.
- [ ] Treat a release pin bump as a separate change after the candidate checks
  pass. Record the tested revision pair and toolchains. Ohmu's Rust 1.98 pin
  does not change Nausicaa's minimum toolchain.

### C. Gate knowledge artifacts from the real producer

- [ ] Produce a deterministic Ohmu `evaluation.json` from a fresh fixture wiki
  and a local scripted model. Keep the original annotations, snapshot identity,
  journals, and acceptance reports. Score those artifacts in Teto using a
  pinned revision and retain reports on failure.
- [ ] Check an exact citation, a forged or stale locator, and an accepted
  abstention without quotes. Verify that Teto rejects changed annotations and
  incomplete case sets, leaves unavailable metrics unscored, and holds the
  offline check when the producer aborts or evidence cannot be read. Use CI
  assertions over the score report for this check. Teto's standalone
  `gate` command currently expects Suite Run evidence.
- [ ] Keep expected-source coverage, quote identity, and abstention visible
  separately. Leave semantic support and contradictions out of release decisions
  until independent labels exist.

### D. Surface durable local status and explicit continuation

- [ ] Add read-only journal-derived status using thread and turn IDs. Report an
  active turn only with a live owner. After restart, distinguish terminal
  history, eligible model-boundary continuation, and required recovery.
  Keep optional task, ledger, and application verdicts in their owning hosts.
- [ ] Reconnect after faults at model request, approval, execution-start, and
  receipt boundaries. Verify that querying status has no tool effects, known
  boundaries can continue only by explicit request, and an unknown receipt
  blocks continuation. Test a legacy log and concurrent recover/resume requests.
- [ ] Review status and continuation behavior through the JSON-RPC client before
  exposing it in the TUI. Evaluate reconnect latency on a long journal against
  the existing event-read path before choosing pagination or persisted indexes.

### E. Reconcile one independently durable operation

- [ ] Start when a named host operation can outlive its worker and exposes a
  durable operation ID or authoritative status. Ohmu publication is a reference
  for domain-owned reconciliation. A task lease supplies authority to report a
  result while valid and does not supply evidence of the external effect.
- [ ] Fault after execution-start and after the effect but before receipt
  persistence. On worker restart, recover a known outcome or preserve unknown.
  Verify that neither restart nor lease expiry launches a second operation and
  that any unconfirmed status stays visible to clients.
- [ ] Require a concrete host need before durable approval waiting, resident
  delivery, or a generic supervisor. Bind any future approval to the same
  prepared action and specify how old logs replay before writing new events.

### F. Expand evaluation with independent evidence

- [ ] Add an explicit Teto reader or trusted exporter for `rust-behavior-v1`
  before putting that suite in a release gate. Preserve fixture and verifier
  revisions. Keep compile or run infrastructure errors distinct from a failed
  behavior assertion. Retain the fixed Rust fixture contract for existing runs.
- [ ] For live comparisons, pin model deployment, counter, fixture snapshot, and
  environment. Use held-out cases, record coverage, false completion, usage,
  cost, and latency, and derive policy thresholds from retained baseline runs.
  For Ohmu, add human labels for answer support and contradictions before
  calibrating a semantic scorer. Keep live runs opt-in and budgeted.
- [ ] Reuse Teto's offline score and gate commands before adding a transport.
  Consider Teto's planned evaluation service when an identified client needs
  asynchronous submission and retrieval with idempotency and artifact access
  control.

## Work gated by demonstrated need

Keep subagent scheduling, adaptive profiles, dynamic tool discovery, semantic
compaction, shared persistence, and new execution backends out of the delivery
queue. Revisit each when a named host workflow or held-out measurement exposes a
specific gap. Check journal growth and evidence retention before increasing
artifact collection. Keep sandbox containment checks separate from process
lifecycle tests. Ohmu remains responsible for wiki retrieval, and Nausicaa's
advisory memory remains general-purpose.
