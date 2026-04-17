use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use std::path::Path;

use super::registry::{Tool, ToolResult};
use super::user_tool_types::{add_tool_to_manifest, is_reserved_name, UserToolManifest, UserToolType};
use crate::memory::{MemoryLayer, MemorySearch};

pub struct ToolCreate {
    workspace: std::path::PathBuf,
}

impl ToolCreate {
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
        }
    }
}

#[async_trait]
impl Tool for ToolCreate {
    fn name(&self) -> &str {
        "tool_create"
    }

    fn description(&self) -> &str {
        "Create a new learned tool that becomes immediately callable. Two types: 'script' (Python, receives JSON params via stdin, prints to stdout) or 'workflow' (step-by-step instructions replayed on invocation). Always prefer creating a tool over repeating manual steps."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Tool name (snake_case, e.g. 'fetch_weather')"
                },
                "description": {
                    "type": "string",
                    "description": "One-line description of what the tool does"
                },
                "tool_type": {
                    "type": "string",
                    "enum": ["script", "workflow"],
                    "description": "'script' for executable Python tools, 'workflow' for step-by-step instruction templates"
                },
                "parameters": {
                    "type": "object",
                    "description": "JSON Schema for the parameters this tool accepts (e.g. {\"type\":\"object\",\"properties\":{\"city\":{\"type\":\"string\"}},\"required\":[\"city\"]})"
                },
                "script": {
                    "type": "string",
                    "description": "Python script body (for script tools). Read JSON from stdin via `import sys,json; params=json.load(sys.stdin)`. Use uv inline metadata for deps: # /// script\\n# dependencies = [\"requests\"]\\n# ///"
                },
                "instructions": {
                    "type": "string",
                    "description": "Step-by-step instructions (for workflow tools). Markdown format."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Tags for categorization and discovery"
                }
            },
            "required": ["name", "description", "tool_type"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let raw_name = match params["name"].as_str() {
            Some(n) if !n.is_empty() => n,
            _ => return Ok(ToolResult::err("Missing 'name' parameter".to_string())),
        };
        let name = raw_name.to_lowercase().replace(' ', "_").replace('-', "_");

        if is_reserved_name(&name) {
            return Ok(ToolResult::err(format!(
                "Name '{}' is reserved for a built-in tool",
                name
            )));
        }

        // Check for duplicate
        if super::user_tool_types::find_tool_in_manifest(&self.workspace, &name).is_some() {
            return Ok(ToolResult::err(format!(
                "Tool '{}' already exists. Use tool_list to see existing tools.",
                name
            )));
        }

        let description = params["description"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let tool_type_str = params["tool_type"].as_str().unwrap_or("script");
        let tool_type = match tool_type_str {
            "script" => UserToolType::Script,
            "workflow" => UserToolType::Workflow,
            other => {
                return Ok(ToolResult::err(format!(
                    "Unknown tool_type '{}'. Use 'script' or 'workflow'.",
                    other
                )))
            }
        };

        let parameters = params
            .get("parameters")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let tags: Vec<String> = params["tags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let mut script_path: Option<String> = None;
        let mut instructions: Option<String> = None;

        match tool_type {
            UserToolType::Script => {
                let script = match params["script"].as_str() {
                    Some(s) if !s.is_empty() => s,
                    _ => {
                        return Ok(ToolResult::err(
                            "Script tools require the 'script' parameter".to_string(),
                        ))
                    }
                };

                let rel = format!("tools/{}.py", name);
                let full_path = self.workspace.join(&rel);
                let _ = tokio::fs::create_dir_all(self.workspace.join("tools")).await;
                if let Err(e) = tokio::fs::write(&full_path, script).await {
                    return Ok(ToolResult::err(format!(
                        "Failed to write script: {}",
                        e
                    )));
                }
                script_path = Some(rel);
            }
            UserToolType::Workflow => {
                instructions = Some(
                    params["instructions"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                );
                if instructions.as_ref().map_or(true, |s| s.is_empty()) {
                    return Ok(ToolResult::err(
                        "Workflow tools require the 'instructions' parameter".to_string(),
                    ));
                }
            }
        }

        let manifest_entry = UserToolManifest {
            name: name.clone(),
            description: description.clone(),
            tool_type,
            parameters,
            script_path,
            instructions,
            tags: tags.clone(),
            created: Utc::now().to_rfc3339(),
        };

        if let Err(e) = add_tool_to_manifest(&self.workspace, manifest_entry) {
            return Ok(ToolResult::err(format!("Failed to save manifest: {}", e)));
        }

        // Index to semantic memory
        let memory_path = self.workspace.join("memory");
        let memory = MemorySearch::new(&memory_path);
        let mem_summary = format!("Tool created: {} [{}]", name, tool_type_str);
        let mem_content = format!(
            "# Tool: {}\n\n> {}\n\nType: {}\nTags: {}\nDescription: {}",
            name,
            description,
            tool_type,
            tags.join(", "),
            description,
        );
        let mut mem_tags = vec!["tool", &name];
        if let Some(t) = tags.first() {
            mem_tags.push(t.as_str());
        }
        let _ = memory
            .add_memory(MemoryLayer::Semantic, &mem_summary, &mem_content, &mem_tags)
            .await;

        Ok(ToolResult::ok(format!(
            "[TOOL_CREATED:{}]\nSuccessfully created {} tool '{}'. It is now available as a callable tool.\nDescription: {}",
            name, tool_type, name, description
        )))
    }
}
