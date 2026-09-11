mod options;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use agent_harness_context_fs::{FsContextCompiler, FsContextConfig};
use agent_harness_core::{
    Access, AgentRuntime, BoxFuture, CapabilityPolicy, DirectExecutor, JsonlEventStore,
    ModelAdapter, ModelError, ModelRequest, ModelResponse, PromptLayer, PromptSegment,
    RuntimeConfig, ToolRegistry,
};
use agent_harness_executor_process::{
    BubblewrapRunner, LocalProcessRunner, ProcessRunner, register_workspace_tools,
};
use agent_harness_provider_openai::{CurlTransport, OpenAiCompatibleAdapter};
use agent_harness_tui::{TuiConfig, approval_channel, event_channel};

struct DemoModel;

impl ModelAdapter for DemoModel {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let input = request
                .context
                .messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    agent_harness_core::TranscriptMessage::User { content } => Some(content),
                    _ => None,
                })
                .cloned()
                .unwrap_or_default();
            Ok(ModelResponse::text(format!("demo response: {input}")))
        })
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        print_help();
        return Ok(());
    }
    let options = options::Options::parse(&arguments, |name| env::var(name).ok())?;
    let workspace = options
        .workspace
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?)
        .canonicalize()?;
    let model: Arc<dyn ModelAdapter> = if options.demo {
        Arc::new(DemoModel)
    } else {
        let config = options.provider_config(
            env::var("HARNESS_API_KEY")
                .or_else(|_| env::var("OPENAI_API_KEY"))
                .ok(),
        );
        Arc::new(OpenAiCompatibleAdapter::new(
            config,
            Arc::new(CurlTransport::new()),
        ))
    };

    let mut tools = ToolRegistry::new();
    if !options.no_tools {
        let runner: Arc<dyn ProcessRunner> = if options.unsafe_local {
            Arc::new(LocalProcessRunner::new(1_048_576))
        } else {
            Arc::new(BubblewrapRunner::new(&workspace, 1_048_576)?)
        };
        register_workspace_tools(&mut tools, &workspace, runner)?;
    }

    let mut context_config = FsContextConfig::new(&workspace, &workspace);
    context_config.stable_segments.push(PromptSegment::new(
        PromptLayer::Stable,
        "agent-harness",
        "You are a coding agent. Inspect before changing files and report verifiable outcomes.",
    ));
    for skill_root in [
        workspace.join(".agents/skills"),
        workspace.join(".codex/skills"),
    ] {
        if skill_root.is_dir() {
            context_config.skill_roots.push(skill_root);
        }
    }
    context_config.max_transcript_groups = Some(options.history_groups);

    let state_directory = workspace.join(".agent-harness");
    fs::create_dir_all(&state_directory)?;
    let (approval_provider, approval_receiver) = approval_channel();
    let (observer, event_receiver) = event_channel();
    let policy = CapabilityPolicy::deny_by_default()
        .grant("read_file", Access::Allow)
        .grant("write_file", Access::Ask)
        .grant("shell", Access::Ask);
    let runtime = Arc::new(
        AgentRuntime::new(
            model,
            Arc::new(JsonlEventStore::open(state_directory.join("events.jsonl"))?),
            Arc::new(FsContextCompiler::new(context_config)),
            tools,
            Arc::new(policy),
        )
        .with_executor(Arc::new(DirectExecutor))
        .with_approval_provider(approval_provider)
        .with_observer(observer)
        .with_config(RuntimeConfig {
            workspace: Some(workspace.clone()),
            max_model_iterations: options.max_model_iterations,
            ..RuntimeConfig::default()
        }),
    );
    let thread_id = runtime.start_thread()?;
    agent_harness_tui::run(
        runtime,
        thread_id,
        event_receiver,
        approval_receiver,
        TuiConfig {
            title: format!("Agent Harness — {}", workspace.display()),
            ..TuiConfig::default()
        },
    )?;
    Ok(())
}

fn print_help() {
    println!(
        "agent-harness-tui\n\n\
         Options:\n\
           --demo                Use the built-in offline demo model\n\
           --workspace <path>    Workspace and event-store location\n\
           --endpoint <url>      OpenAI-compatible chat completions URL\n\
           --model <name>        Provider model name\n\
           --request-timeout <s> Total model request timeout (1–600; default 120)\n\
           --max-output-tokens <n> Completion cap (1–131072; default 4096)\n\
           --temperature <n>     Sampling temperature (0–2; provider default if absent)\n\
           --max-model-iterations <n> Per-turn request limit (1–128; default 32)\n\
           --history-groups <n>  Retained complete transcript groups (1–2000; default 200)\n\
           --no-tools            Disable workspace tools\n\
           --unsafe-local-exec   Run shell on the host instead of Bubblewrap\n\
           -h, --help            Show this help\n\n\
         Environment: HARNESS_API_URL, HARNESS_MODEL, HARNESS_API_KEY, OPENAI_API_KEY,\n\
         HARNESS_REQUEST_TIMEOUT, HARNESS_MAX_OUTPUT_TOKENS, HARNESS_TEMPERATURE,\n\
         HARNESS_MAX_MODEL_ITERATIONS, HARNESS_HISTORY_GROUPS"
    );
}
