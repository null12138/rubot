use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use serde_json::json;

use super::registry::{Tool, ToolResult};
use crate::memory::index::MemoryIndex;
use crate::memory::layer::MemoryLayer;

pub struct PathRemember {
    memory_path: PathBuf,
}

impl PathRemember {
    pub fn new(workspace: &Path) -> Self {
        Self {
            memory_path: workspace.join("memory"),
        }
    }
}

#[async_trait]
impl Tool for PathRemember {
    fn name(&self) -> &str {
        "path_remember"
    }

    fn description(&self) -> &str {
        "Record a highly effective task execution path (success pattern) to long-term memory. Use this after completing a complex task successfully."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_name": {
                    "type": "string",
                    "description": "Short, descriptive name of the task (e.g., 'scrape_weather_data')"
                },
                "successful_path": {
                    "type": "string",
                    "description": "The exact steps, tools, or logic that worked well. Include tips on how to do it 'faster and better'."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Keywords for later retrieval"
                }
            },
            "required": ["task_name", "successful_path"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let task_name = match params["task_name"].as_str() {
            Some(n) if !n.is_empty() => n,
            _ => return Ok(ToolResult::err("Missing 'task_name'".to_string())),
        };
        let successful_path = params["successful_path"].as_str().unwrap_or("");
        let tags_val = params["tags"].as_array();
        let mut tags: Vec<&str> = vec!["effective_path", "pattern"];
        if let Some(arr) = tags_val {
            for v in arr {
                if let Some(s) = v.as_str() { tags.push(s); }
            }
        }

        let memory_index = MemoryIndex::new(&self.memory_path);
        let summary = format!("Effective Path: {}", task_name);
        let content = format!(
            "# Task Optimization: {}\n\n## Successful Path\n{}\n\n## Implementation Tips\n- Always check this memory before starting similar tasks.\n- Use the same tool sequence if applicable.",
            task_name, successful_path
        );

        match memory_index.add_memory(MemoryLayer::Semantic, &summary, &content, &tags).await {
            Ok(_) => Ok(ToolResult::ok(format!("Successfully indexed effective path for '{}' into long-term memory.", task_name))),
            Err(e) => Ok(ToolResult::err(format!("Failed to save path: {}", e))),
        }
    }
}
