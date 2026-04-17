use serde::{Deserialize, Serialize};

/// A chain of tool calls to execute in sequence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallChain {
    pub goal: String,
    pub steps: Vec<ChainStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainStep {
    pub id: usize,
    pub tool: String,
    pub params: serde_json::Value,
    pub description: String,
    #[serde(default)]
    pub depends_on: Vec<usize>, // step IDs this step depends on
    #[serde(default)]
    pub status: StepStatus,
    #[serde(skip)]
    pub result: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    #[default]
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl ToolCallChain {
    pub fn new(goal: &str) -> Self {
        Self {
            goal: goal.to_string(),
            steps: Vec::new(),
        }
    }

    pub fn add_step(
        &mut self,
        tool: &str,
        params: serde_json::Value,
        description: &str,
        depends_on: Vec<usize>,
    ) -> usize {
        let id = self.steps.len();
        self.steps.push(ChainStep {
            id,
            tool: tool.to_string(),
            params,
            description: description.to_string(),
            depends_on,
            status: StepStatus::Pending,
            result: None,
        });
        id
    }

    /// Get the next step that is ready to execute (all deps satisfied)
    pub fn next_ready(&self) -> Option<usize> {
        for step in &self.steps {
            if step.status != StepStatus::Pending {
                continue;
            }
            let deps_satisfied = step.depends_on.iter().all(|dep_id| {
                self.steps
                    .get(*dep_id)
                    .map_or(true, |dep| dep.status == StepStatus::Done)
            });
            if deps_satisfied {
                return Some(step.id);
            }
        }
        None
    }

    pub fn is_complete(&self) -> bool {
        self.steps
            .iter()
            .all(|s| matches!(s.status, StepStatus::Done | StepStatus::Skipped))
    }

    pub fn has_failure(&self) -> bool {
        self.steps.iter().any(|s| s.status == StepStatus::Failed)
    }

    /// Substitute $step_N.result references in params with actual results
    pub fn resolve_references(&self, params: &serde_json::Value) -> serde_json::Value {
        let json_str = serde_json::to_string(params).unwrap_or_default();
        let mut resolved = json_str;

        for step in &self.steps {
            if let Some(ref result) = step.result {
                let placeholder = format!("$step_{}.result", step.id);
                // Escape the result for JSON string embedding
                let escaped = result.replace('\\', "\\\\").replace('"', "\\\"");
                resolved = resolved.replace(&placeholder, &escaped);
            }
        }

        serde_json::from_str(&resolved).unwrap_or(params.clone())
    }

    /// Format the chain as markdown for display / state file
    pub fn to_markdown(&self) -> String {
        let mut md = format!("# Plan: {}\n\n", self.goal);
        for step in &self.steps {
            let checkbox = match step.status {
                StepStatus::Done => "[x]",
                StepStatus::Failed => "[!]",
                StepStatus::Running => "[~]",
                StepStatus::Skipped => "[-]",
                StepStatus::Pending => "[ ]",
            };
            md.push_str(&format!(
                "- {} **Step {}**: {} (`{}`)\n",
                checkbox, step.id, step.description, step.tool
            ));
            if let Some(ref result) = step.result {
                let preview = if result.chars().count() > 100 {
                    let truncated: String = result.chars().take(100).collect();
                    format!("{}...", truncated)
                } else {
                    result.clone()
                };
                md.push_str(&format!("  Result: {}\n", preview));
            }
        }
        md
    }
}
