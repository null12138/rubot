use serde::{Deserialize, Serialize};
use anyhow::Result;
use crate::reflector::error_book::ErrorBook;
use crate::tools::registry::ToolRegistry;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus { #[default] Pending, Running, Done, Failed, Skipped }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainStep {
    pub id: usize,
    pub tool: String,
    pub params: serde_json::Value,
    pub desc: String,
    #[serde(default)] pub depends_on: Vec<usize>,
    #[serde(default)] pub status: StepStatus,
    #[serde(skip)] pub result: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallChain { pub goal: String, pub steps: Vec<ChainStep> }

impl ToolCallChain {
    pub fn new(goal: &str) -> Self { Self { goal: goal.into(), steps: vec![] } }
    pub fn add_step(&mut self, tool: &str, params: serde_json::Value, desc: &str, deps: Vec<usize>) -> usize {
        let id = self.steps.len();
        self.steps.push(ChainStep { id, tool: tool.into(), params, desc: desc.into(), depends_on: deps, status: StepStatus::Pending, result: None });
        id
    }
    pub fn next_ready(&self) -> Option<usize> {
        self.steps.iter().find(|s| s.status == StepStatus::Pending && 
            s.depends_on.iter().all(|&id| self.steps.get(id).map_or(true, |d| d.status == StepStatus::Done)))
            .map(|s| s.id)
    }
    pub fn resolve(&self, params: &serde_json::Value) -> serde_json::Value {
        let mut s = serde_json::to_string(params).unwrap_or_default();
        for step in &self.steps {
            if let Some(r) = &step.result {
                s = s.replace(&format!("$step_{}.result", step.id), &r.replace('\\', "\\\\").replace('"', "\\\""));
            }
        }
        serde_json::from_str(&s).unwrap_or(params.clone())
    }
    pub fn has_failure(&self) -> bool { self.steps.iter().any(|s| s.status == StepStatus::Failed) }
    pub fn to_md(&self) -> String {
        let mut md = format!("# Plan: {}\n\n", self.goal);
        for s in &self.steps {
            let cb = match s.status { StepStatus::Done => "[x]", StepStatus::Failed => "[!]", StepStatus::Running => "[~]", StepStatus::Skipped => "[-]", StepStatus::Pending => "[ ]" };
            md.push_str(&format!("- {} Step {}: {} (`{}`)\n", cb, s.id, s.desc, s.tool));
            if let Some(r) = &s.result {
                let p: String = r.chars().take(100).collect();
                md.push_str(&format!("  Result: {}{}\n", p, if r.len() > 100 { "..." } else { "" }));
            }
        }
        md
    }
}

pub struct ChainExecutor<'a> {
    pub registry: &'a ToolRegistry,
    pub error_book: &'a mut ErrorBook,
    pub retries: u32,
}

impl<'a> ChainExecutor<'a> {
    pub fn new(registry: &'a ToolRegistry, error_book: &'a mut ErrorBook, retries: u32) -> Self {
        Self { registry, error_book, retries }
    }
    pub async fn execute(&mut self, chain: &mut ToolCallChain) -> Result<Vec<(usize, String)>> {
        let mut outs = vec![];
        while let Some(id) = chain.next_ready() {
            chain.steps[id].status = StepStatus::Running;
            let params = chain.resolve(&chain.steps[id].params.clone());
            let tool = chain.steps[id].tool.clone();
            let mut err = None;
            for _ in 0..=self.retries {
                match self.registry.execute(&tool, params.clone()).await {
                    Ok(res) if res.success => {
                        chain.steps[id].status = StepStatus::Done;
                        chain.steps[id].result = Some(res.output.clone());
                        outs.push((id, res.output));
                        err = None; break;
                    }
                    Ok(res) => err = Some(res.to_string_for_llm()),
                    Err(e) => err = Some(format!("{:#}", e)),
                }
            }
            if let Some(e) = err {
                chain.steps[id].status = StepStatus::Failed;
                chain.steps[id].result = Some(format!("[FAILED] {}", e));
                self.error_book.record_error(&tool, &e).await?;
                outs.push((id, format!("[FAILED] {}", e)));
            }
        }
        Ok(outs)
    }
}
