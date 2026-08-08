use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::id::{ThreadId, TurnId};
use crate::protocol::TranscriptMessage;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptLayer {
    Stable,
    Rules,
    Skills,
    Memory,
    Compaction,
    Volatile,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptSegment {
    pub layer: PromptLayer,
    pub name: String,
    pub text: String,
}

impl PromptSegment {
    #[must_use]
    pub fn new(layer: PromptLayer, name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            layer,
            name: name.into(),
            text: text.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextInput {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub transcript: Vec<TranscriptMessage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompiledContext {
    /// Kept as distinct ordered segments so providers can preserve a stable
    /// prefix for prompt caching instead of concatenating volatile data into it.
    pub prompt: Vec<PromptSegment>,
    pub messages: Vec<TranscriptMessage>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ContextError {
    #[error("context exceeds the configured character budget ({actual} > {maximum})")]
    TooLarge { actual: usize, maximum: usize },
    #[error("context path error: {0}")]
    Path(String),
    #[error("failed to read context source: {0}")]
    Read(String),
}

pub trait ContextCompiler: Send + Sync {
    fn compile(&self, input: ContextInput) -> Result<CompiledContext, ContextError>;
}

/// Deterministic layered compiler. Memory remains an advisory prompt layer and
/// is never fed into the policy engine.
#[derive(Clone, Debug, Default)]
pub struct LayeredContextCompiler {
    segments: Vec<PromptSegment>,
    max_characters: Option<usize>,
}

impl LayeredContextCompiler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_segment(mut self, segment: PromptSegment) -> Self {
        self.segments.push(segment);
        self
    }

    #[must_use]
    pub fn with_segments(mut self, segments: impl IntoIterator<Item = PromptSegment>) -> Self {
        self.segments.extend(segments);
        self
    }

    #[must_use]
    pub fn with_max_characters(mut self, maximum: usize) -> Self {
        self.max_characters = Some(maximum);
        self
    }
}

impl ContextCompiler for LayeredContextCompiler {
    fn compile(&self, input: ContextInput) -> Result<CompiledContext, ContextError> {
        let prompt_size: usize = self.segments.iter().map(|segment| segment.text.len()).sum();
        let transcript_size: usize = input
            .transcript
            .iter()
            .map(|message| serde_json::to_string(message).map_or(0, |value| value.len()))
            .sum();
        let actual = prompt_size + transcript_size;
        if let Some(maximum) = self.max_characters
            && actual > maximum
        {
            return Err(ContextError::TooLarge { actual, maximum });
        }

        Ok(CompiledContext {
            prompt: self.segments.clone(),
            messages: input.transcript,
        })
    }
}

/// Loads one rule file per directory from the project root to the current
/// directory. Earlier filenames take precedence within a directory, allowing
/// `AGENTS.override.md` to shadow `AGENTS.md`.
#[derive(Clone, Debug)]
pub struct DirectoryRuleLoader {
    file_names: Vec<String>,
}

impl Default for DirectoryRuleLoader {
    fn default() -> Self {
        Self {
            file_names: vec!["AGENTS.override.md".to_owned(), "AGENTS.md".to_owned()],
        }
    }
}

impl DirectoryRuleLoader {
    #[must_use]
    pub fn new(file_names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            file_names: file_names.into_iter().map(Into::into).collect(),
        }
    }

    pub fn load(
        &self,
        project_root: impl AsRef<Path>,
        current_directory: impl AsRef<Path>,
    ) -> Result<Vec<PromptSegment>, ContextError> {
        let root = canonical_directory(project_root.as_ref())?;
        let current = canonical_directory(current_directory.as_ref())?;
        if !current.starts_with(&root) {
            return Err(ContextError::Path(format!(
                "current directory `{}` is outside project root `{}`",
                current.display(),
                root.display()
            )));
        }

        let mut directories = Vec::<PathBuf>::new();
        let mut cursor = current.as_path();
        loop {
            directories.push(cursor.to_path_buf());
            if cursor == root {
                break;
            }
            cursor = cursor.parent().ok_or_else(|| {
                ContextError::Path("project root was not found while walking parents".to_owned())
            })?;
        }
        directories.reverse();

        let mut segments = Vec::new();
        for directory in directories {
            let selected = self
                .file_names
                .iter()
                .map(|name| directory.join(name))
                .find(|candidate| candidate.is_file());
            let Some(path) = selected else {
                continue;
            };
            let canonical_path = path
                .canonicalize()
                .map_err(|error| ContextError::Path(format!("{}: {error}", path.display())))?;
            if !canonical_path.starts_with(&root) {
                return Err(ContextError::Path(format!(
                    "rule file `{}` resolves outside project root",
                    path.display()
                )));
            }
            let text = fs::read_to_string(&canonical_path).map_err(|error| {
                ContextError::Read(format!("{}: {error}", canonical_path.display()))
            })?;
            let name = canonical_path
                .strip_prefix(&root)
                .unwrap_or(&canonical_path)
                .display()
                .to_string();
            segments.push(PromptSegment::new(PromptLayer::Rules, name, text));
        }
        Ok(segments)
    }
}

fn canonical_directory(path: &Path) -> Result<PathBuf, ContextError> {
    let canonical = path
        .canonicalize()
        .map_err(|error| ContextError::Path(format!("{}: {error}", path.display())))?;
    if !canonical.is_dir() {
        return Err(ContextError::Path(format!(
            "`{}` is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}
