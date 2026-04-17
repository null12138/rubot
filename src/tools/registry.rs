use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::llm::types::ToolDefinition;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
}

impl ToolResult {
    pub fn ok(output: String) -> Self {
        Self {
            success: true,
            output,
            error: None,
        }
    }

    pub fn err(error: String) -> Self {
        Self {
            success: false,
            output: String::new(),
            error: Some(error),
        }
    }

    pub fn to_string_for_llm(&self) -> String {
        if self.success {
            self.output.clone()
        } else {
            format!(
                "[ERROR] {}",
                self.error.as_deref().unwrap_or("Unknown error")
            )
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    pub async fn execute(&self, name: &str, params: serde_json::Value) -> Result<ToolResult> {
        match self.tools.get(name) {
            Some(tool) => tool.execute(params).await,
            None => Ok(ToolResult::err(format!("Unknown tool: {}", name))),
        }
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|tool| {
                ToolDefinition::new(tool.name(), tool.description(), tool.parameters_schema())
            })
            .collect()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }
}
