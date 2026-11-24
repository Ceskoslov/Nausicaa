//! The behavior suite compiles only the built-in scripted fixture proposals.
use std::path::Path;

use agent_harness_executor_process::{LocalProcessRunner, ProcessRequest, ProcessRunner};
use agent_harness_task::{ArtifactRef, Criterion};
use serde_json::json;

use crate::{EvalResult, Fixture, Mode, RunReport, run_with_verifier};

pub(super) struct BehaviorVerifier {
    assertions: &'static str,
}

impl BehaviorVerifier {
    pub(super) fn criterion(&self) -> Criterion {
        Criterion {
            id: "rust-behavior-v1".into(),
            artifact_id: "host-rust-verdict".into(),
            expected: "passed".into(),
        }
    }

    pub(super) fn verify(
        &self,
        run_directory: &Path,
        attempt: u32,
        source: &str,
    ) -> EvalResult<(ArtifactRef, String)> {
        let directory = run_directory.join(format!("behavior-{attempt}"));
        std::fs::create_dir(&directory)?;
        std::fs::write(directory.join("lib.rs"), source)?;
        std::fs::write(
            directory.join("checks.rs"),
            format!(
                "#[path = \"lib.rs\"] mod candidate;\nfn main() {{\n{}\n}}\n",
                self.assertions
            ),
        )?;
        let runner = LocalProcessRunner::new(16 * 1024);
        let compile = ProcessRequest {
            command: "rustc --edition=2024 checks.rs -o checks".into(),
            working_directory: directory.clone(),
            timeout_ms: 10_000,
        };
        let test = ProcessRequest {
            command: "./checks".into(),
            working_directory: directory.clone(),
            timeout_ms: 2_000,
        };
        let built = runner.run(&compile)?;
        let ran = if !built.timed_out && built.exit_code == Some(0) {
            Some(runner.run(&test)?)
        } else {
            None
        };
        let passed = ran
            .as_ref()
            .is_some_and(|result| !result.timed_out && result.exit_code == Some(0));
        std::fs::write(
            directory.join("verification.json"),
            serde_json::to_vec_pretty(&json!({
                "version": 1,
                "compile": {"command": compile.command, "timeout_ms": compile.timeout_ms, "result": built},
                "test": {"command": test.command, "timeout_ms": test.timeout_ms, "result": ran},
                "passed": passed,
            }))?,
        )?;
        let feedback = if passed {
            "Fixed host behavior checks passed".to_owned()
        } else if let Some(result) = ran {
            format!(
                "Repair the implementation to pass the fixed behavior checks: {}",
                result.stderr
            )
        } else {
            format!(
                "Repair the implementation to compile within the verifier limit: {}",
                built.stderr
            )
        };
        Ok((
            ArtifactRef {
                id: self.criterion().artifact_id,
                content: if passed { "passed" } else { "failed" }.into(),
            },
            feedback,
        ))
    }
}

/// Run the fixed repository behavior suite in fresh directories with both modes.
pub fn run_behavior_suite(output: &Path) -> EvalResult<Vec<RunReport>> {
    let cases = [
        (
            include_str!("../fixtures/behavior/repair.json"),
            "assert_eq!(candidate::clamp(-2, 0, 6), 0);\nassert_eq!(candidate::clamp(3, 0, 6), 3);\nassert_eq!(candidate::clamp(9, 0, 6), 6);",
        ),
        (
            include_str!("../fixtures/behavior/equivalent.json"),
            "assert!(candidate::is_even(-2));\nassert!(candidate::is_even(0));\nassert!(!candidate::is_even(-3));\nassert!(!candidate::is_even(7));",
        ),
        (
            include_str!("../fixtures/behavior/held-out.json"),
            "assert_eq!(candidate::sum(&[]), 0);\nassert_eq!(candidate::sum(&[-5, 7]), 2);\nassert_eq!(candidate::sum(&[i16::MAX, i16::MAX]), 65534);",
        ),
    ];
    std::fs::create_dir(output)?;
    let mut reports = Vec::new();
    for (bytes, assertions) in cases {
        let fixture: Fixture = serde_json::from_str(bytes)?;
        let verifier = BehaviorVerifier { assertions };
        for (label, mode) in [("core", Mode::CoreOnly), ("task", Mode::Task)] {
            reports.push(run_with_verifier(
                &fixture,
                mode,
                &output.join(format!("{}-{label}", fixture.id)),
                crate::scripted_model(&fixture),
                "scripted-v1",
                false,
                Some(&verifier),
            )?);
        }
    }
    Ok(reports)
}
