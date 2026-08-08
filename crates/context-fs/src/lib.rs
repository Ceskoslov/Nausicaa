//! Optional filesystem-backed context extension.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use agent_harness_core::{
    CompiledContext, ContextCompiler, ContextError, ContextInput, DirectoryRuleLoader, PromptLayer,
    PromptSegment, TranscriptMessage,
};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillInfo {
    pub name: String,
    pub summary: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum FsContextError {
    #[error("filesystem context I/O error: {0}")]
    Io(String),
    #[error("selected skill `{0}` was not found")]
    UnknownSkill(String),
}

#[derive(Clone, Debug)]
pub struct SkillCatalog {
    roots: Vec<PathBuf>,
}

impl SkillCatalog {
    #[must_use]
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            roots: roots.into_iter().collect(),
        }
    }

    pub fn scan(&self) -> Result<Vec<SkillInfo>, FsContextError> {
        let mut skills = Vec::new();
        for root in &self.roots {
            if !root.exists() {
                continue;
            }
            let entries = fs::read_dir(root)
                .map_err(|error| FsContextError::Io(format!("{}: {error}", root.display())))?;
            for entry in entries {
                let entry = entry.map_err(|error| FsContextError::Io(error.to_string()))?;
                let entry_path = entry.path();
                let skill_path = if entry_path.is_dir() {
                    entry_path.join("SKILL.md")
                } else if entry_path.extension().and_then(|value| value.to_str()) == Some("md") {
                    entry_path
                } else {
                    continue;
                };
                if !skill_path.is_file() {
                    continue;
                }
                let text = fs::read_to_string(&skill_path).map_err(|error| {
                    FsContextError::Io(format!("{}: {error}", skill_path.display()))
                })?;
                let fallback = skill_path
                    .parent()
                    .and_then(Path::file_name)
                    .or_else(|| skill_path.file_stem())
                    .and_then(|value| value.to_str())
                    .unwrap_or("skill");
                let name = frontmatter_value(&text, "name").unwrap_or_else(|| fallback.to_owned());
                let summary = frontmatter_value(&text, "description")
                    .or_else(|| first_prose_line(&text))
                    .unwrap_or_else(|| "No description provided".to_owned());
                skills.push(SkillInfo {
                    name,
                    summary,
                    path: skill_path,
                });
            }
        }
        skills.sort_by(|left, right| left.name.cmp(&right.name));
        skills.dedup_by(|left, right| left.name == right.name);
        Ok(skills)
    }
}

#[derive(Clone, Debug)]
pub struct FsContextConfig {
    pub project_root: PathBuf,
    pub current_directory: PathBuf,
    pub skill_roots: Vec<PathBuf>,
    pub selected_skills: BTreeSet<String>,
    pub stable_segments: Vec<PromptSegment>,
    pub volatile_segments: Vec<PromptSegment>,
    /// Number of complete transcript groups retained. A tool-call assistant
    /// message and its receipts form one indivisible group.
    pub max_transcript_groups: Option<usize>,
    pub max_characters: Option<usize>,
}

impl FsContextConfig {
    #[must_use]
    pub fn new(project_root: impl Into<PathBuf>, current_directory: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            current_directory: current_directory.into(),
            skill_roots: Vec::new(),
            selected_skills: BTreeSet::new(),
            stable_segments: Vec::new(),
            volatile_segments: Vec::new(),
            max_transcript_groups: None,
            max_characters: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FsContextCompiler {
    config: FsContextConfig,
    rule_loader: DirectoryRuleLoader,
}

impl FsContextCompiler {
    #[must_use]
    pub fn new(config: FsContextConfig) -> Self {
        Self {
            config,
            rule_loader: DirectoryRuleLoader::default(),
        }
    }

    fn compile_prompt(&self) -> Result<Vec<PromptSegment>, FsContextError> {
        let mut prompt = self.config.stable_segments.clone();
        let rules = self
            .rule_loader
            .load(&self.config.project_root, &self.config.current_directory)
            .map_err(|error| FsContextError::Io(error.to_string()))?;
        prompt.extend(rules);

        let skills = SkillCatalog::new(self.config.skill_roots.clone()).scan()?;
        if !skills.is_empty() {
            let summaries = skills
                .iter()
                .map(|skill| format!("- {}: {}", skill.name, skill.summary))
                .collect::<Vec<_>>()
                .join("\n");
            prompt.push(PromptSegment::new(
                PromptLayer::Skills,
                "skill-index",
                format!("Available skills:\n{summaries}"),
            ));
        }
        for selected in &self.config.selected_skills {
            let skill = skills
                .iter()
                .find(|skill| &skill.name == selected)
                .ok_or_else(|| FsContextError::UnknownSkill(selected.clone()))?;
            let content = fs::read_to_string(&skill.path).map_err(|error| {
                FsContextError::Io(format!("{}: {error}", skill.path.display()))
            })?;
            prompt.push(PromptSegment::new(
                PromptLayer::Skills,
                format!("skill:{}", skill.name),
                content,
            ));
        }
        prompt.extend(self.config.volatile_segments.clone());
        Ok(prompt)
    }
}

impl ContextCompiler for FsContextCompiler {
    fn compile(&self, input: ContextInput) -> Result<CompiledContext, ContextError> {
        let mut prompt = self
            .compile_prompt()
            .map_err(|error| ContextError::Read(error.to_string()))?;
        let (messages, omitted) = compact_transcript(
            input.transcript,
            self.config.max_transcript_groups.unwrap_or(usize::MAX),
        );
        if omitted > 0 {
            prompt.push(PromptSegment::new(
                PromptLayer::Compaction,
                "deterministic-transcript-window",
                format!(
                    "{omitted} earlier complete transcript group(s) were omitted by the deterministic context window. No semantic summary was invented."
                ),
            ));
        }

        let actual = prompt
            .iter()
            .map(|segment| segment.text.len())
            .sum::<usize>()
            + messages
                .iter()
                .map(|message| format!("{message:?}").len())
                .sum::<usize>();
        if let Some(maximum) = self.config.max_characters
            && actual > maximum
        {
            return Err(ContextError::TooLarge { actual, maximum });
        }
        Ok(CompiledContext { prompt, messages })
    }
}

fn compact_transcript(
    messages: Vec<TranscriptMessage>,
    maximum_groups: usize,
) -> (Vec<TranscriptMessage>, usize) {
    let mut groups = Vec::<Vec<TranscriptMessage>>::new();
    let mut iterator = messages.into_iter().peekable();
    while let Some(message) = iterator.next() {
        let mut group = vec![message];
        let expected_receipts = match &group[0] {
            TranscriptMessage::Assistant { tool_calls, .. } => tool_calls.len(),
            _ => 0,
        };
        while group.len() <= expected_receipts
            && matches!(iterator.peek(), Some(TranscriptMessage::Tool { .. }))
        {
            if let Some(receipt) = iterator.next() {
                group.push(receipt);
            }
        }
        groups.push(group);
    }
    let omitted = groups.len().saturating_sub(maximum_groups);
    let retained = groups
        .into_iter()
        .skip(omitted)
        .flatten()
        .collect::<Vec<_>>();
    (retained, omitted)
}

fn frontmatter_value(text: &str, key: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix(&format!("{key}:")) {
            return Some(value.trim().trim_matches(['\'', '"']).to_owned());
        }
    }
    None
}

fn first_prose_line(text: &str) -> Option<String> {
    let mut lines = text.lines().peekable();
    if lines.peek().is_some_and(|line| line.trim() == "---") {
        lines.next();
        for line in lines.by_ref() {
            if line.trim() == "---" {
                break;
            }
        }
    }
    for line in lines {
        let line = line.trim();
        if !line.is_empty() && !line.starts_with('#') {
            return Some(line.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;

    use agent_harness_core::{ContextInput, ThreadId, TurnId};

    use super::*;

    #[test]
    fn loads_rule_hierarchy_and_selected_skill() {
        let root = std::env::temp_dir().join(ThreadId::new().as_str());
        let child = root.join("src");
        let skills = root.join("skills/review");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir_all(&skills).unwrap();
        fs::write(root.join("AGENTS.md"), "root rule").unwrap();
        fs::write(child.join("AGENTS.md"), "child rule").unwrap();
        fs::write(
            skills.join("SKILL.md"),
            "---\nname: review\ndescription: Review changes\n---\n# Review\nRun tests.",
        )
        .unwrap();
        let mut config = FsContextConfig::new(&root, &child);
        config.skill_roots.push(root.join("skills"));
        config.selected_skills.insert("review".to_owned());
        let compiler = FsContextCompiler::new(config);

        let compiled = compiler
            .compile(ContextInput {
                thread_id: ThreadId::new(),
                turn_id: TurnId::new(),
                transcript: Vec::new(),
            })
            .unwrap();

        assert_eq!(
            compiled
                .prompt
                .iter()
                .filter(|segment| segment.layer == PromptLayer::Rules)
                .count(),
            2
        );
        assert!(compiled.prompt.iter().any(|segment| {
            segment.name == "skill:review" && segment.text.contains("Run tests")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compaction_keeps_assistant_calls_with_receipts() {
        let call = agent_harness_core::ToolCall::new("x", serde_json::json!({}));
        let prepared = agent_harness_core::PreparedToolCall {
            call_id: call.id.clone(),
            action: agent_harness_core::CanonicalAction::new(
                "x",
                serde_json::json!({}),
                agent_harness_core::EffectKind::ReadOnly,
                agent_harness_core::RetrySafety::Safe,
            ),
        };
        let messages = vec![
            TranscriptMessage::User {
                content: "old".to_owned(),
            },
            TranscriptMessage::Assistant {
                content: String::new(),
                tool_calls: vec![call],
            },
            TranscriptMessage::Tool {
                receipt: agent_harness_core::ToolReceipt::succeeded(
                    prepared,
                    serde_json::json!({}),
                ),
            },
        ];
        let (retained, omitted) = compact_transcript(messages, 1);
        assert_eq!(omitted, 1);
        assert_eq!(retained.len(), 2);
    }
}
