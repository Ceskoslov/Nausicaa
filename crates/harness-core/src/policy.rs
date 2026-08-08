use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Deny,
    Ask,
    Allow,
}

impl Access {
    /// Returns the stricter of two grants.
    #[must_use]
    pub fn restrict(self, other: Self) -> Self {
        self.min(other)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PolicyContext {
    pub principal: Option<String>,
    pub channel: Option<String>,
    pub agent: Option<String>,
    pub sandbox: Option<String>,
}

pub trait ToolPolicy: Send + Sync {
    fn access(&self, tool_name: &str, context: &PolicyContext) -> Access;
}

/// Simple declarative policy. Safe by default: unmentioned tools are denied.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilityPolicy {
    default: Access,
    tools: BTreeMap<String, Access>,
}

impl Default for CapabilityPolicy {
    fn default() -> Self {
        Self {
            default: Access::Deny,
            tools: BTreeMap::new(),
        }
    }
}

impl CapabilityPolicy {
    #[must_use]
    pub fn deny_by_default() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_default(mut self, access: Access) -> Self {
        self.default = access;
        self
    }

    #[must_use]
    pub fn grant(mut self, tool_name: impl Into<String>, access: Access) -> Self {
        self.tools.insert(tool_name.into(), access);
        self
    }
}

impl ToolPolicy for CapabilityPolicy {
    fn access(&self, tool_name: &str, _context: &PolicyContext) -> Access {
        self.tools.get(tool_name).copied().unwrap_or(self.default)
    }
}

/// The concrete per-run projection. Only non-denied entries are visible to the
/// model, but denied entries remain queryable for fail-closed execution checks.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilityProjection {
    entries: BTreeMap<String, Access>,
}

impl CapabilityProjection {
    #[must_use]
    pub fn access(&self, tool_name: &str) -> Access {
        self.entries.get(tool_name).copied().unwrap_or(Access::Deny)
    }

    pub fn visible_names(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|(_, access)| **access != Access::Deny)
            .map(|(name, _)| name.as_str())
    }

    #[must_use]
    pub fn entries(&self) -> &BTreeMap<String, Access> {
        &self.entries
    }
}

/// Computes model-visible capabilities and intersects them with a parent's
/// projection. This intersection is the hard invariant preventing a child
/// agent from expanding its parent's permissions.
#[must_use]
pub fn project_capabilities<'a>(
    tool_names: impl IntoIterator<Item = &'a str>,
    policy: &dyn ToolPolicy,
    context: &PolicyContext,
    parent: Option<&CapabilityProjection>,
) -> CapabilityProjection {
    let entries = tool_names
        .into_iter()
        .map(|name| {
            let requested = policy.access(name, context);
            let effective = parent
                .map(|parent| requested.restrict(parent.access(name)))
                .unwrap_or(requested);
            (name.to_owned(), effective)
        })
        .collect();
    CapabilityProjection { entries }
}
