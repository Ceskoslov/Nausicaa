# Nemotron 3.5 Lightning free-route validation — 2026-09-11

The live provider/tool loop worked, but none of the eight runs satisfied the
existing exact-content contract. Six generated artifacts passed supplementary
compilation and simple behavioral tests; all six differed from the required
source only in whitespace. These are separate results, not grounds to silently
relax the task's acceptance criteria.

## Configuration and provenance

- Source revision: `722c961` (including configurable live request timeouts).
- Model: `nvidia/nemotron-3.5-lightning:free` through OpenRouter Chat Completions.
- The public model catalog reported zero prompt/completion pricing and support
  for tools when checked for this run. This is not a billing audit.
- No immutable provider deployment was verified. The free route is an explicitly
  recorded reproducibility limitation, not a pinned model-quality benchmark.
- Temperature: 0; completion-token cap: 1024; transport timeout: 120 seconds.
- Four model iterations per turn; two attempts for task mode, one for core-only.
- Four fixtures in both modes; the synthetic unknown-execution fixture was skipped
  as specified by the live protocol. Fixture bytes and criteria were unchanged.
- The live `repair` and `exhausted` fixtures use the same addition prompt. Their
  different scripted proposals are unused in live mode, so they are repeated
  addition trials rather than independent coding tasks.

Raw evidence is retained locally under the Git-ignored directory
`.agent-harness/evals/nemotron-free-20260911-0835/`. It includes `summary.json`,
model metadata, provenance, effective configuration, each workspace, core/task
journals, per-attempt evidence, supplementary tests, and `evidence-manifest.json`
with SHA-256 hashes of text artifacts. The summary SHA-256 is:

```text
6701a1650321fc9e3cb2d3251307efb7efeffdf568b65454800d5bd180b3fa5c
```

Credentials and runtime logs are not committed. Earlier temporary-directory runs
were lost during an environment interruption and are not included in these totals.
The source fix in `722c961` retained the old 30-second default and allowed an
explicit bounded timeout override; this completed run used 120 seconds throughout.

## Results

| Fixture | Mode | Reported success | Exact-content pass | Attempts | Model requests | Terminal outcome |
| --- | --- | --- | --- | --- | --- | --- |
| repair | core-only | yes | no | 1 | 2 | normal turn end |
| repair | task | no | no | 2 | 5 | budget exhausted |
| exhausted | core-only | yes | no | 1 | 2 | normal turn end |
| exhausted | task | no | no | 2 | 6 | budget exhausted; one turn hit its iteration limit |
| denied | core-only | yes | no | 1 | 1 | normal turn end; file unchanged |
| denied | task | no | no | 2 | 2 | budget exhausted; file unchanged |
| identity | core-only | yes | no | 1 | 2 | normal turn end |
| identity | task | no | no | 2 | 5 | budget exhausted |

There were 25 model requests and 25 persisted responses, with no HTTP or transport
timeout errors. Reported usage totals 18,439 input tokens and 4,368 output tokens.
The sum of run elapsed times was 538,871 ms, including verification and journal I/O.
Twelve turns produced eleven normal completions and one iteration-limit failure.

The deliberately naive core-only metric equates a normal turn end with task
success: it records four false completions. This includes the denied fixture,
where normal termination does not establish that the model claimed to edit the
file. Task mode records zero false completions and zero accepted tasks. Zero false
completions here is not evidence of improved model task quality.

## Evidence and authority checks

All 14 accepted tool calls had exactly one matching durable receipt: 13 succeeded
and one unregistered tool name was denied in the identity task run. Both `denied`
fixture runs had no tool execution and preserved the original file. The gate did
not accept failed required checks, and repeated attempts remained bounded. The
synthetic unknown-action scenario was not rerun against the live model.

For supplementary diagnostics, the six generated source files were first checked
to differ from the known expected functions only in whitespace. Each was compiled
with `rustc --edition=2024 --test` and tested with three inputs: addition used
`(2, 3)`, `(-2, 3)`, and `(0, 0)`; identity used `7`, `-4`, and `0`. All six compiled
and passed. These tiny diagnostic tests do not replace the exact-content checker,
prove arbitrary program correctness, or change any task journal verdict.

The initial prompts already provide the expected source. This suite demonstrates
provider integration, tool execution, refusal of unregistered calls, receipt
closure and strict acceptance behavior. It does not measure general coding skill,
real-world repository editing, production token accounting, or long-context quality.

## Follow-up

The observed formatting failures motivate a measured improvement to repair
feedback: explicitly identify whitespace and end-of-file differences. Broader
coding evaluation should introduce behavioral criteria as an explicitly versioned
contract, rather than reinterpret existing exact-content criteria or old evidence.
Compare changes on development fixtures and retain separate regression fixtures
before making quality claims.

For another live run, create a fresh output directory under `.agent-harness/evals/`
and use the configuration described in the [evaluation guide](../README.md).
OpenRouter routing/privacy settings must permit the chosen endpoint. Store only
non-secret configuration and model metadata with the results.
