use std::path::PathBuf;

use agent_harness_eval::{EvalResult, Mode, fixtures, run_scripted};

#[cfg(feature = "live")]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveConfig {
    endpoint: String,
    model_snapshot: String,
    /// Host-provided immutable provider deployment/revision identifier.
    revision: String,
    /// Keep legacy runs at 30 seconds unless the host explicitly overrides it.
    #[serde(default = "default_timeout_seconds")]
    timeout_seconds: u64,
}

#[cfg(feature = "live")]
fn default_timeout_seconds() -> u64 {
    30
}

#[cfg(feature = "live")]
impl LiveConfig {
    fn validate(&self) -> EvalResult<()> {
        if self.model_snapshot.trim().is_empty()
            || self.revision.trim().is_empty()
            || self.endpoint.contains(['@', '?', '#'])
        {
            return Err("live config needs a pinned model/revision and an endpoint without embedded credentials or query parameters".into());
        }
        if !(1..=300).contains(&self.timeout_seconds) {
            return Err("live timeout_seconds must be between 1 and 300".into());
        }
        Ok(())
    }
}

fn main() -> EvalResult<()> {
    let mut args = std::env::args().skip(1);
    let mut output = None;
    let mut live_config = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().ok_or("missing output directory")?,
                ))
            }
            "--live-config" => {
                live_config = Some(PathBuf::from(args.next().ok_or("missing live config")?))
            }
            _ => {
                return Err(
                    "usage: agent-harness-eval --output NEW_DIRECTORY [--live-config CONFIG.json]"
                        .into(),
                );
            }
        }
    }
    let output = output.ok_or("--output NEW_DIRECTORY is required")?;
    #[cfg(not(feature = "live"))]
    if live_config.is_some() {
        return Err("live runs require --features live".into());
    }
    #[cfg(feature = "live")]
    let config: Option<LiveConfig> = live_config
        .as_ref()
        .map(|path| -> EvalResult<_> {
            let config: LiveConfig = serde_json::from_slice(&std::fs::read(path)?)?;
            config.validate()?;
            Ok(config)
        })
        .transpose()?;
    std::fs::create_dir(&output)?;
    #[cfg(feature = "live")]
    if let Some(config) = &config {
        std::fs::write(
            output.join("live-config.json"),
            serde_json::to_vec_pretty(config)?,
        )?;
    }
    let mut reports = Vec::new();
    let mut skipped = Vec::new();
    for fixture in fixtures()? {
        if live_config.is_some() && fixture.interrupt_execution {
            skipped.push(fixture.id.clone());
            continue;
        }
        for (label, mode) in [("core", Mode::CoreOnly), ("task", Mode::Task)] {
            let directory = output.join(format!("{}-{label}", fixture.id));
            #[cfg(feature = "live")]
            let report = if let Some(config) = &config {
                use agent_harness_provider_openai::{
                    CurlTransport, OpenAiCompatibleAdapter, OpenAiConfig,
                };
                use std::sync::Arc;
                let mut provider = OpenAiConfig::new(&config.endpoint, &config.model_snapshot);
                provider.api_key = std::env::var("NAUSICAA_EVAL_API_KEY").ok();
                provider.timeout_seconds = config.timeout_seconds;
                provider
                    .extra_body
                    .insert("temperature".into(), serde_json::json!(0));
                provider
                    .extra_body
                    .insert("max_completion_tokens".into(), serde_json::json!(1024));
                agent_harness_eval::run(
                    &fixture,
                    mode,
                    &directory,
                    Arc::new(OpenAiCompatibleAdapter::new(
                        provider,
                        Arc::new(CurlTransport::new()),
                    )),
                    &config.model_snapshot,
                    true,
                )?
            } else {
                run_scripted(&fixture, mode, &directory)?
            };
            #[cfg(not(feature = "live"))]
            let report = run_scripted(&fixture, mode, &directory)?;
            reports.push(report);
        }
    }
    #[cfg(feature = "live")]
    let provider_settings = config.as_ref().map(|config| serde_json::json!({
        "temperature": 0, "max_completion_tokens": 1024, "timeout_seconds": config.timeout_seconds
    }));
    #[cfg(not(feature = "live"))]
    let provider_settings: Option<serde_json::Value> = None;
    let summary = serde_json::json!({
        "schema_version": 1,
        "fixture_suite": "task-baseline-v1",
        "live_config": live_config.as_ref().map(|_| "live-config.json"),
        "provider_settings": provider_settings,
        "skipped_fault_injection_fixtures": skipped,
        "runs": reports,
    });
    std::fs::write(
        output.join("summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    println!("{}", output.join("summary.json").display());
    Ok(())
}

#[cfg(all(test, feature = "live"))]
mod tests {
    use super::*;

    #[test]
    fn live_timeout_defaults_and_explicit_limits_are_validated_and_retained() {
        let base = serde_json::json!({
            "endpoint": "https://example.test/v1/chat/completions",
            "model_snapshot": "fixture-model", "revision": "fixture-v1"
        });
        let legacy: LiveConfig = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(legacy.timeout_seconds, 30);
        legacy.validate().unwrap();
        for (timeout, valid) in [
            (0, false),
            (1, true),
            (120, true),
            (300, true),
            (301, false),
        ] {
            let mut value = base.clone();
            value["timeout_seconds"] = serde_json::json!(timeout);
            let config: LiveConfig = serde_json::from_value(value).unwrap();
            assert_eq!(config.validate().is_ok(), valid);
            assert_eq!(
                serde_json::to_value(config).unwrap()["timeout_seconds"],
                timeout
            );
        }
    }
}
