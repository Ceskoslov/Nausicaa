# Task acceptance evaluation v1

Run paired offline fixtures without network, credentials, shells, or a compiler:

```sh
cargo run -p agent-harness-eval --offline -- --output /tmp/nausicaa-eval-v1
```

The output directory must not exist. Each fixture/mode gets a fresh workspace,
core `events.jsonl`, per-attempt `evidence-N.json`, the exact fixture specification,
and `report.json`. Task mode also retains `task.jsonl`. `summary.json` collects all
reports. Outputs are host files, not an OS sandbox; keep generated evidence outside
the repository. Tests use unique temporary directories and remove them on exit.

## Paired protocol

Both modes start from identical source and use the same scripted model and tool
policy. Only `write_file` is registered. Every core turn allows at most four model
iterations. The script deliberately claims completion after writing the first
proposal. Core-only mode stops after that one turn. Task mode verifies the source
snapshot and may feed repair feedback into a new turn within its attempt budget.
The required exact source is supplied in the initial prompt in both modes.

The checker compares exact UTF-8 source contents, not compilation or arbitrary
semantic equivalence. The coding fixture repairs `a - b` to `a + b`; this tests the
acceptance loop, not general programming skill. The development fixtures exercise
repair, repeated failure and policy denial. Separate held-out regression fixtures
exercise an identity function and an interrupted execution. Keep the held-out
files fixed when tuning development behavior; these public scripts are not an
unseen model-quality benchmark.

Interruption injection drops the runtime future only after reaching the executor
boundary, then calls recovery and collects its unknown receipt. No action is
replayed. Runtime errors are recovered before evidence collection; a recovery or
persistence error aborts the run and leaves a running task for host inspection.
Denied calls exercise the ordinary policy path and remain in both journals.

## Metrics and baseline

- `reported_success`: normal turn completion in core-only mode, or independent
  task success in task mode. The former is an intentionally naive application
  interpretation; the core itself does not claim task acceptance.
- `verified_success`: every required artifact check passed, with no unknown
  receipts in the final attempt.
- `false_completion`: reported success without verified success.
- `attempts`, `model_iterations`: task attempts and actual model-request events.
- `elapsed_ms`: host elapsed time including verification and journal I/O, excluding
  initial setup and final report serialization; not a deterministic assertion.
- `tokens`: summed provider usage from persisted responses. Scripted runs report
  zero; missing live-provider usage also cannot establish actual token cost.
- `interventions`: one if unknown outcomes or task blocking require host attention,
  otherwise zero. Budget exhaustion is recorded separately in `task_status`.
- Configuration includes fixture version and contents, model ID, harness package
  version, per-turn iteration cap, mode, policy flags, and artifact directory.

Observed offline baseline (2026-09-11):

| Fixture | Core reported / verified | Task outcome | Task attempts |
| --- | --- | --- | --- |
| repair | yes / no | succeeded | 2 |
| exhausted | yes / no | budget exhausted | 2 |
| denied | yes / no | budget exhausted | 2 |
| identity (held-out) | yes / yes | succeeded | 1 |
| unknown (held-out) | no / no | blocked, one unknown receipt | 1 |

The paired suite has 3 core-only false completions and 0 task-layer false
completions. Task mode verifies 2 of 5 tasks; core-only verifies 1 of 5. These are
scripted protocol measurements, not evidence that a live model improves.

## Opt-in live runs

Compile the `live` feature and supply an explicit configuration file:

```json
{
  "endpoint": "https://your-provider.example/v1/chat/completions",
  "model_snapshot": "your-immutable-model-snapshot-id",
  "revision": "your-immutable-provider-deployment-revision",
  "timeout_seconds": 120
}
```

The host must resolve these to a pinned deployment; the adapter cannot verify a
provider's immutability promise. Alias names undermine reproducibility. Keep the
configuration free of credentials. Supply authentication through
`NAUSICAA_EVAL_API_KEY` in the environment if required.

```sh
cargo run -p agent-harness-eval --features live --offline -- \
  --output /tmp/nausicaa-eval-live-v1 --live-config /path/to/live-config.json
```

`--offline` only controls Cargo; this explicit live mode makes provider requests.
The CLI records the non-secret live configuration, uses temperature 0, a 1024
completion-token cap, and a configurable transport timeout. `timeout_seconds`
accepts 1–300 seconds and defaults to the original 30 seconds when omitted. Both
the retained configuration and summary record the effective timeout. Increase it
explicitly for endpoints whose response latency exceeds the default; transport
timeouts are infrastructure failures, not evidence of model task quality. It skips the synthetic
interruption fixture and records that omission. Other fixtures run against fresh
adapter instances for each mode. Live calls are sequential, not automatically
retried, and not part of the default tests. No live run was used for the baseline.
Some providers may reject these settings; failures remain visible in turn errors.
This remains an exact-content microbenchmark and does not justify orchestration
or broad model-quality claims.
