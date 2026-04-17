use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

use super::registry::{Tool, ToolResult};

pub struct SkillCreate {
    skills_dir: PathBuf,
}

impl SkillCreate {
    pub fn new(workspace: &Path) -> Self {
        Self {
            skills_dir: workspace.join("skills"),
        }
    }
}

#[async_trait]
impl Tool for SkillCreate {
    fn name(&self) -> &str {
        "skill_create"
    }

    fn description(&self) -> &str {
        "Create a reusable skill (saved as a .md file). Skills define a sequence of steps that can be replayed."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name (used as filename, no spaces)"
                },
                "description": {
                    "type": "string",
                    "description": "One-line description of what the skill does"
                },
                "trigger": {
                    "type": "string",
                    "description": "When this skill should be activated"
                },
                "steps": {
                    "type": "string",
                    "description": "Markdown-formatted steps or instructions"
                }
            },
            "required": ["name", "description", "steps"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let name = match params["name"].as_str() {
            Some(n) if !n.is_empty() => n,
            _ => return Ok(ToolResult::err("Missing 'name' parameter".to_string())),
        };
        let description = params["description"].as_str().unwrap_or("");
        let trigger = params["trigger"].as_str().unwrap_or("manual");
        let steps = params["steps"].as_str().unwrap_or("");

        let filename = format!("{}.md", name.replace(' ', "_").to_lowercase());
        let path = self.skills_dir.join(&filename);

        let content = format!(
            "---\nname: {}\ndescription: {}\ntrigger: {}\n---\n\n# {}\n\n{}\n",
            name, description, trigger, name, steps
        );

        let _ = tokio::fs::create_dir_all(&self.skills_dir).await;
        match tokio::fs::write(&path, &content).await {
            Ok(()) => Ok(ToolResult::ok(format!(
                "Skill '{}' saved to {}",
                name,
                path.display()
            ))),
            Err(e) => Ok(ToolResult::err(format!("Failed to save skill: {}", e))),
        }
    }
}
