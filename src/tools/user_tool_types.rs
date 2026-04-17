use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::registry::{Tool, ToolResult};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum UserToolType {
    Script,
    Workflow,
}

impl std::fmt::Display for UserToolType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserToolType::Script => write!(f, "script"),
            UserToolType::Workflow => write!(f, "workflow"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserToolManifest {
    pub name: String,
    pub description: String,
    pub tool_type: UserToolType,
    pub parameters: serde_json::Value,
    pub script_path: Option<String>,
    pub instructions: Option<String>,
    pub tags: Vec<String>,
    pub created: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserToolManifestFile {
    pub version: u32,
    pub tools: Vec<UserToolManifest>,
}

impl Default for UserToolManifestFile {
    fn default() -> Self {
        Self {
            version: 1,
            tools: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Reserved tool names — cannot be overwritten by user tools
// ---------------------------------------------------------------------------

const RESERVED_NAMES: &[&str] = &[
    "web_search",
    "web_fetch",
    "code_exec",
    "file_ops",
    "tool_create",
    "tool_list",
];

pub fn is_reserved_name(name: &str) -> bool {
    RESERVED_NAMES.contains(&name)
}

// ---------------------------------------------------------------------------
// UserTool — dynamically registered Tool implementation
// ---------------------------------------------------------------------------

pub struct UserTool {
    manifest: UserToolManifest,
    workspace_path: PathBuf,
    timeout_secs: u64,
}

impl UserTool {
    pub fn new(manifest: UserToolManifest, workspace_path: &Path, timeout_secs: u64) -> Self {
        Self {
            manifest,
            workspace_path: workspace_path.to_path_buf(),
            timeout_secs,
        }
    }
}

#[async_trait]
impl Tool for UserTool {
    fn name(&self) -> &str {
        &self.manifest.name
    }

    fn description(&self) -> &str {
        &self.manifest.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        if self.manifest.parameters.is_null() || self.manifest.parameters == serde_json::json!({}) {
            serde_json::json!({
                "type": "object",
                "properties": {}
            })
        } else {
            self.manifest.parameters.clone()
        }
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        match self.manifest.tool_type {
            UserToolType::Script => self.execute_script(params).await,
            UserToolType::Workflow => self.execute_workflow(),
        }
    }
}

impl UserTool {
    async fn execute_script(&self, params: serde_json::Value) -> Result<ToolResult> {
        let script_rel = match &self.manifest.script_path {
            Some(p) => p.clone(),
            None => return Ok(ToolResult::err("No script_path in manifest".to_string())),
        };
        let script_path = self.workspace_path.join(&script_rel);

        if !script_path.exists() {
            return Ok(ToolResult::err(format!(
                "Script not found: {}",
                script_path.display()
            )));
        }

        let params_json = params.to_string();
        let shell_cmd = format!(
            "echo '{}' | uv run '{}'",
            params_json.replace('\'', "'\\''"),
            script_path.display()
        );

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            Command::new("bash")
                .arg("-c")
                .arg(&shell_cmd)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                if output.status.success() {
                    let mut result = stdout;
                    if !stderr.is_empty() {
                        result.push_str("\n[stderr] ");
                        result.push_str(&stderr);
                    }
                    Ok(ToolResult::ok(result))
                } else {
                    Ok(ToolResult::err(format!(
                        "Exit code: {}\nstdout: {}\nstderr: {}",
                        output.status.code().unwrap_or(-1),
                        stdout,
                        stderr
                    )))
                }
            }
            Ok(Err(e)) => Ok(ToolResult::err(format!("Execution failed: {}", e))),
            Err(_) => Ok(ToolResult::err(format!(
                "Script timed out after {}s",
                self.timeout_secs
            ))),
        }
    }

    fn execute_workflow(&self) -> Result<ToolResult> {
        let instructions = self
            .manifest
            .instructions
            .clone()
            .unwrap_or_default();
        if instructions.is_empty() {
            return Ok(ToolResult::err(
                "Workflow tool has no instructions".to_string(),
            ));
        }
        Ok(ToolResult::ok(instructions))
    }
}
