use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use serde_json::json;

use super::registry::{Tool, ToolResult};
use crate::memory::index::MemoryIndex;
use crate::memory::layer::MemoryLayer;

pub struct ToolForge {
    tools_dir: PathBuf,
    memory_path: PathBuf,
}

impl ToolForge {
    pub fn new(workspace: &Path) -> Self {
        Self {
            tools_dir: workspace.join("tools"),
            memory_path: workspace.join("memory"),
        }
    }
}

#[async_trait]
impl Tool for ToolForge {
    fn name(&self) -> &str {
        "tool_forge"
    }

    fn description(&self) -> &str {
        "Create a new Python-based CLI tool with 'uv' dependency management. Saves the script and documentation to the tools library, and adds it to semantic memory."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Tool name (lowercase, snake_case, e.g., 'image_optimizer')"
                },
                "description": {
                    "type": "string",
                    "description": "One-line description of what the tool does"
                },
                "python_code": {
                    "type": "string",
                    "description": "Complete Python script. Use 'uv' inline metadata for dependencies (e.g. # /// script\\n# dependencies = [\"requests\"]\\n# ///)"
                },
                "usage_example": {
                    "type": "string",
                    "description": "Example command line usage (e.g. 'uv run tools/image_optimizer.py input.jpg')"
                }
            },
            "required": ["name", "description", "python_code"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let name = match params["name"].as_str() {
            Some(n) if !n.is_empty() => n.to_lowercase().replace(' ', "_"),
            _ => return Ok(ToolResult::err("Missing or invalid 'name' parameter".to_string())),
        };
        let description = params["description"].as_str().unwrap_or("");
        let python_code = params["python_code"].as_str().unwrap_or("");
        let usage_example = params["usage_example"].as_str().unwrap_or("");

        let py_filename = format!("{}.py", name);
        let md_filename = format!("{}.md", name);
        let py_path = self.tools_dir.join(&py_filename);
        let md_path = self.tools_dir.join(&md_filename);
        let index_path = self.tools_dir.join("index.md");

        // Ensure tools directory exists
        let _ = tokio::fs::create_dir_all(&self.tools_dir).await;

        // Write Python script
        if let Err(e) = tokio::fs::write(&py_path, python_code).await {
            return Ok(ToolResult::err(format!("Failed to write Python script: {}", e)));
        }

        // Write Markdown documentation
        let md_content = format!(
            "# Tool: {}\n\n> {}\n\n## Usage\n\n```bash\n{}\n```\n\n## Source\n\n[{}](./{})\n",
            name, description, usage_example, py_filename, py_filename
        );
        if let Err(e) = tokio::fs::write(&md_path, &md_content).await {
            return Ok(ToolResult::err(format!("Failed to write documentation: {}", e)));
        }

        // Update index.md
        let mut index_content = if index_path.exists() {
            tokio::fs::read_to_string(&index_path).await.unwrap_or_default()
        } else {
            "# Tool Library\n\n".to_string()
        };

        let index_line = format!("- [{}](./{}) — {}\n", name, md_filename, description);
        if !index_content.contains(&index_line) {
            index_content.push_str(&index_line);
            let _ = tokio::fs::write(&index_path, index_content).await;
        }

        // Add to memory index (Semantic layer)
        let memory_index = MemoryIndex::new(&self.memory_path);
        let mem_summary = format!("Tool forged: {}", name);
        let mem_content = format!(
            "Forged a new Python tool '{}'. Description: {}\n\nDocumentation: {}\nUsage: {}",
            name, description, md_path.display(), usage_example
        );
        let _ = memory_index.add_memory(MemoryLayer::Semantic, &mem_summary, &mem_content, &["tool", "python", &name]).await;

        Ok(ToolResult::ok(format!(
            "Successfully forged tool '{}'.\n- Script: {}\n- Documentation: {}\n- Index updated: {}\n- Added to Semantic Memory",
            name,
            py_path.display(),
            md_path.display(),
            index_path.display()
        )))
    }
}
