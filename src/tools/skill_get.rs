use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

use super::registry::{Tool, ToolResult};

pub struct SkillGet {
    skills_dir: PathBuf,
}

impl SkillGet {
    pub fn new(workspace: &Path) -> Self {
        Self {
            skills_dir: workspace.join("skills"),
        }
    }
}

#[async_trait]
impl Tool for SkillGet {
    fn name(&self) -> &str {
        "skill_get"
    }

    fn description(&self) -> &str {
        "List available skills or load a specific skill by name."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "load"],
                    "description": "Action: 'list' to see all skills, 'load' to get a specific skill"
                },
                "name": {
                    "type": "string",
                    "description": "Skill name to load (required for 'load' action)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let action = params["action"].as_str().unwrap_or("list");

        match action {
            "list" => {
                let mut skills = Vec::new();
                if let Ok(mut entries) = tokio::fs::read_dir(&self.skills_dir).await {
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.ends_with(".md") {
                            // Read first line of description from frontmatter
                            if let Ok(content) = tokio::fs::read_to_string(entry.path()).await {
                                let desc = extract_frontmatter_field(&content, "description")
                                    .unwrap_or_default();
                                skills.push(format!(
                                    "- {} — {}",
                                    name.trim_end_matches(".md"),
                                    desc
                                ));
                            } else {
                                skills.push(format!("- {}", name.trim_end_matches(".md")));
                            }
                        }
                    }
                }
                if skills.is_empty() {
                    Ok(ToolResult::ok("No skills available.".to_string()))
                } else {
                    skills.sort();
                    Ok(ToolResult::ok(format!(
                        "Available skills:\n{}",
                        skills.join("\n")
                    )))
                }
            }
            "load" => {
                let name = match params["name"].as_str() {
                    Some(n) if !n.is_empty() => n,
                    _ => {
                        return Ok(ToolResult::err(
                            "Missing 'name' parameter for load action".to_string(),
                        ))
                    }
                };
                let filename = format!("{}.md", name.replace(' ', "_").to_lowercase());
                let path = self.skills_dir.join(&filename);
                match tokio::fs::read_to_string(&path).await {
                    Ok(content) => Ok(ToolResult::ok(content)),
                    Err(_) => Ok(ToolResult::err(format!("Skill '{}' not found", name))),
                }
            }
            _ => Ok(ToolResult::err(format!("Unknown action: {}", action))),
        }
    }
}

fn extract_frontmatter_field(content: &str, field: &str) -> Option<String> {
    let content = content.strip_prefix("---\n")?;
    let end = content.find("---")?;
    let frontmatter = &content[..end];
    for line in frontmatter.lines() {
        if let Some(value) = line.strip_prefix(&format!("{}: ", field)) {
            return Some(value.trim().to_string());
        }
    }
    None
}
