use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;

use super::registry::{Tool, ToolResult};
use super::user_tool_manifest;
use super::user_tool_types::UserToolType;

pub struct ToolList {
    workspace: std::path::PathBuf,
}

impl ToolList {
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
        }
    }
}

#[async_trait]
impl Tool for ToolList {
    fn name(&self) -> &str {
        "tool_list"
    }

    fn description(&self) -> &str {
        "List your learned tools (created via tool_create). Shows name, type (script/workflow), and description. Use before creating a new tool to avoid duplicates."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "enum": ["all", "script", "workflow"],
                    "description": "Filter by tool type (default: 'all')"
                }
            }
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let filter = params["filter"].as_str().unwrap_or("all");
        let tools = user_tool_manifest::list_tools(&self.workspace);

        let filtered: Vec<_> = tools
            .into_iter()
            .filter(|t| match filter {
                "script" => t.tool_type == UserToolType::Script,
                "workflow" => t.tool_type == UserToolType::Workflow,
                _ => true,
            })
            .collect();

        if filtered.is_empty() {
            return Ok(ToolResult::ok(format!(
                "No {} learned tools found. Use tool_create to make one.",
                filter
            )));
        }

        let mut lines = vec![format!("## Learned Tools ({})\n", filtered.len())];
        for t in &filtered {
            let type_tag = match t.tool_type {
                UserToolType::Script => "script",
                UserToolType::Workflow => "workflow",
            };
            let params_info = if t.parameters == serde_json::json!({})
                || t.parameters.is_null()
            {
                String::new()
            } else {
                let keys: Vec<&str> = t
                    .parameters
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .map(|obj| obj.keys().map(|k| k.as_str()).collect())
                    .unwrap_or_default();
                if keys.is_empty() {
                    String::new()
                } else {
                    format!(" params: {}", keys.join(", "))
                }
            };
            lines.push(format!(
                "- **{}** [{}] — {}{}",
                t.name, type_tag, t.description, params_info
            ));
        }

        Ok(ToolResult::ok(lines.join("\n")))
    }
}
