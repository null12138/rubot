use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    #[default]
    Pending,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainStep {
    pub id: usize,
    pub tool: String,
    pub params: serde_json::Value,
    pub desc: String,
    #[serde(default)]
    pub depends_on: Vec<usize>,
    #[serde(default)]
    pub status: StepStatus,
    #[serde(skip)]
    pub result: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallChain {
    pub goal: String,
    pub steps: Vec<ChainStep>,
}

impl ToolCallChain {
    pub fn new(goal: &str) -> Self {
        Self {
            goal: goal.into(),
            steps: vec![],
        }
    }

    pub fn add_step(
        &mut self,
        tool: &str,
        params: serde_json::Value,
        desc: &str,
        deps: Vec<usize>,
    ) -> usize {
        let id = self.steps.len();
        self.steps.push(ChainStep {
            id,
            tool: tool.into(),
            params,
            desc: desc.into(),
            depends_on: deps,
            status: StepStatus::Pending,
            result: None,
        });
        id
    }

    pub fn next_ready(&self) -> Option<usize> {
        self.steps
            .iter()
            .find(|s| {
                s.status == StepStatus::Pending
                    && s.depends_on.iter().all(|&id| {
                        self.steps
                            .get(id)
                            .is_some_and(|d| d.status == StepStatus::Done)
                    })
            })
            .map(|s| s.id)
    }

    pub fn resolve(&self, params: &serde_json::Value) -> serde_json::Value {
        let mut s = serde_json::to_string(params).unwrap_or_default();
        for step in &self.steps {
            if let Some(r) = &step.result {
                s = s.replace(
                    &format!("$step_{}.result", step.id),
                    &r.replace('\\', "\\\\").replace('"', "\\\""),
                );
            }
        }
        serde_json::from_str(&s).unwrap_or(params.clone())
    }

    pub fn has_failure(&self) -> bool {
        self.steps.iter().any(|s| s.status == StepStatus::Failed)
    }

    pub fn to_md(&self) -> String {
        let mut md = format!("# Plan: {}\n\n", self.goal);
        for s in &self.steps {
            let cb = match s.status {
                StepStatus::Done => "[x]",
                StepStatus::Failed => "[!]",
                StepStatus::Running => "[~]",
                StepStatus::Pending => "[ ]",
            };
            md.push_str(&format!(
                "- {} Step {}: {} (`{}`)\n",
                cb, s.id, s.desc, s.tool
            ));
            if let Some(r) = &s.result {
                let p: String = r.chars().take(100).collect();
                md.push_str(&format!(
                    "  Result: {}{}\n",
                    p,
                    if r.len() > 100 { "..." } else { "" }
                ));
            }
        }
        md
    }
}
