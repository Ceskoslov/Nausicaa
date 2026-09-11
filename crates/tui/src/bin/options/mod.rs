use agent_harness_provider_openai::OpenAiConfig;
use std::collections::{BTreeMap, BTreeSet};

pub struct Options {
    pub demo: bool,
    pub no_tools: bool,
    pub unsafe_local: bool,
    pub workspace: Option<String>,
    pub endpoint: String,
    pub model: Option<String>,
    pub request_timeout: u64,
    pub max_output_tokens: u64,
    pub temperature: Option<f64>,
    pub max_model_iterations: usize,
    pub history_groups: usize,
}

impl Options {
    pub fn parse(args: &[String], env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut values = BTreeMap::new();
        let mut flags = BTreeSet::new();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--demo" | "--no-tools" | "--unsafe-local-exec" => {
                    if !flags.insert(arg.as_str()) {
                        return Err(format!("duplicate option: {arg}"));
                    }
                }
                "--workspace"
                | "--endpoint"
                | "--model"
                | "--request-timeout"
                | "--max-output-tokens"
                | "--temperature"
                | "--max-model-iterations"
                | "--history-groups" => {
                    let value = iter
                        .next()
                        .filter(|s| !s.starts_with("--") && !s.trim().is_empty())
                        .ok_or_else(|| format!("missing value for {arg}"))?;
                    if values.insert(arg.as_str(), value.clone()).is_some() {
                        return Err(format!("duplicate option: {arg}"));
                    }
                }
                _ => return Err(format!("unknown option: {arg}")),
            }
        }
        let value = |flag, variable| values.get(flag).cloned().or_else(|| env(variable));
        let number = |flag, variable, default, maximum| -> Result<u64, String> {
            let result = value(flag, variable).map_or(Ok(default), |s| {
                s.parse::<u64>()
                    .map_err(|_| format!("{flag} must be an integer"))
            })?;
            if result == 0 || result > maximum {
                return Err(format!("{flag} must be between 1 and {maximum}"));
            }
            Ok(result)
        };
        let temperature = value("--temperature", "HARNESS_TEMPERATURE")
            .map(|s| {
                s.parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite() && (0.0..=2.0).contains(n))
                    .ok_or_else(|| "--temperature must be finite and between 0 and 2".to_owned())
            })
            .transpose()?;
        let demo = flags.contains("--demo");
        let model = value("--model", "HARNESS_MODEL").filter(|s| !s.trim().is_empty());
        if !demo && model.is_none() {
            return Err("set --model, HARNESS_MODEL, or use --demo".into());
        }
        Ok(Self {
            demo,
            no_tools: flags.contains("--no-tools"),
            unsafe_local: flags.contains("--unsafe-local-exec"),
            workspace: values.get("--workspace").cloned(),
            endpoint: value("--endpoint", "HARNESS_API_URL")
                .unwrap_or_else(|| "https://api.openai.com/v1/chat/completions".into()),
            model,
            request_timeout: number("--request-timeout", "HARNESS_REQUEST_TIMEOUT", 120, 600)?,
            max_output_tokens: number(
                "--max-output-tokens",
                "HARNESS_MAX_OUTPUT_TOKENS",
                4096,
                131072,
            )?,
            temperature,
            max_model_iterations: number(
                "--max-model-iterations",
                "HARNESS_MAX_MODEL_ITERATIONS",
                32,
                128,
            )? as usize,
            history_groups: number("--history-groups", "HARNESS_HISTORY_GROUPS", 200, 2000)?
                as usize,
        })
    }

    pub fn provider_config(&self, api_key: Option<String>) -> OpenAiConfig {
        let mut config = OpenAiConfig::new(&self.endpoint, self.model.as_deref().unwrap_or("demo"));
        config.api_key = api_key;
        config.timeout_seconds = self.request_timeout;
        config.extra_body.insert(
            "max_completion_tokens".into(),
            self.max_output_tokens.into(),
        );
        if let Some(temperature) = self.temperature {
            config
                .extra_body
                .insert("temperature".into(), temperature.into());
        }
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }
    #[test]
    fn cli_overrides_environment_and_maps_exact_request_settings() {
        let options = Options::parse(&args("--model chosen --request-timeout 240 --max-output-tokens 2048 --temperature 0 --max-model-iterations 8 --history-groups 20"), |name| match name {
            "HARNESS_MODEL" => Some("environment-model".into()),
            "HARNESS_REQUEST_TIMEOUT" => Some("60".into()), _ => None,
        }).unwrap();
        let config = options.provider_config(None);
        assert_eq!(config.model, "chosen");
        assert_eq!(config.timeout_seconds, 240);
        assert_eq!(config.extra_body["max_completion_tokens"], 2048);
        assert_eq!(config.extra_body["temperature"], 0.0);
        assert_eq!(options.max_model_iterations, 8);
        assert_eq!(options.history_groups, 20);
    }
    #[test]
    fn defaults_are_bounded_and_bad_configuration_fails_before_terminal_or_network() {
        let options = Options::parse(&args("--demo"), |_| None).unwrap();
        assert_eq!(options.request_timeout, 120);
        assert_eq!(options.max_output_tokens, 4096);
        assert!(
            options
                .provider_config(None)
                .extra_body
                .get("temperature")
                .is_none()
        );
        for bad in [
            "--request-timeout 0",
            "--request-timeout 601",
            "--temperature NaN",
            "--temperature inf",
            "--temperature -1",
            "--max-output-tokens 0",
            "--history-groups 2001",
            "--max-model-iterations 129",
            "--model",
            "--model x --model y",
            "--surprise",
            "--request-timeout --temperature 1",
        ] {
            assert!(
                Options::parse(&args(&format!("--demo {bad}")), |_| None).is_err(),
                "{bad}"
            );
        }
        assert!(
            Options::parse(&args("--demo"), |name| (name
                == "HARNESS_MAX_OUTPUT_TOKENS")
                .then(|| "bad".into()))
            .is_err()
        );
    }
}
