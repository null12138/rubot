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
pub enum UserToolType { Script, Workflow }

impl std::fmt::Display for UserToolType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { UserToolType::Script => write!(f, "script"), UserToolType::Workflow => write!(f, "workflow") }
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserToolManifestFile {
    pub version: u32,
    pub tools: Vec<UserToolManifest>,
}

// ---------------------------------------------------------------------------
// Manifest I/O
// ---------------------------------------------------------------------------

const MANIFEST_PATH: &str = "tools/manifest.json";

fn manifest_path(workspace: &Path) -> PathBuf { workspace.join(MANIFEST_PATH) }

pub fn load_manifest(workspace: &Path) -> UserToolManifestFile {
    std::fs::read_to_string(manifest_path(workspace))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_manifest(workspace: &Path, manifest: &UserToolManifestFile) -> Result<()> {
    let path = manifest_path(workspace);
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    std::fs::write(&path, serde_json::to_string_pretty(manifest)?)?;
    Ok(())
}

pub fn add_tool_to_manifest(workspace: &Path, tool: UserToolManifest) -> Result<()> {
    let mut mf = load_manifest(workspace);
    mf.tools.push(tool);
    save_manifest(workspace, &mf)
}

pub fn find_tool_in_manifest(workspace: &Path, name: &str) -> Option<UserToolManifest> {
    load_manifest(workspace).tools.into_iter().find(|t| t.name == name)
}

pub fn list_tools_in_manifest(workspace: &Path) -> Vec<UserToolManifest> {
    load_manifest(workspace).tools
}

// ---------------------------------------------------------------------------
// Reserved names
// ---------------------------------------------------------------------------

const RESERVED_NAMES: &[&str] = &["web_search", "web_fetch", "code_exec", "file_ops", "tool_create", "tool_list"];

pub fn is_reserved_name(name: &str) -> bool { RESERVED_NAMES.contains(&name) }

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
        Self { manifest, workspace_path: workspace_path.to_path_buf(), timeout_secs }
    }
}

#[async_trait]
impl Tool for UserTool {
    fn name(&self) -> &str { &self.manifest.name }
    fn description(&self) -> &str { &self.manifest.description }

    fn parameters_schema(&self) -> serde_json::Value {
        match &self.manifest.parameters {
            v if v.is_null() || *v == serde_json::json!({}) =>
                serde_json::json!({"type": "object", "properties": {}}),
            v => v.clone(),
        }
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        match self.manifest.tool_type {
            UserToolType::Script => self.exec_script(params).await,
            UserToolType::Workflow => Ok(ToolResult::ok(
                self.manifest.instructions.clone().unwrap_or_default()
            )),
        }
    }
}

impl UserTool {
    async fn exec_script(&self, params: serde_json::Value) -> Result<ToolResult> {
        let rel = match &self.manifest.script_path {
            Some(p) => p.clone(),
            None => return Ok(ToolResult::err("No script_path in manifest".into())),
        };
        let script = self.workspace_path.join(&rel);
        if !script.exists() { return Ok(ToolResult::err(format!("Script not found: {}", script.display()))); }

        let json = params.to_string().replace('\'', "'\\''");
        let cmd = format!("echo '{}' | uv run '{}'", json, script.display());

        match tokio::time::timeout(std::time::Duration::from_secs(self.timeout_secs), Command::new("bash").arg("-c").arg(&cmd).output()).await {
            Ok(Ok(o)) => {
                let stdout = String::from_utf8_lossy(&o.stdout).to_string();
                let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                if o.status.success() {
                    let mut r = stdout;
                    if !stderr.is_empty() { r.push_str("\n[stderr] "); r.push_str(&stderr); }
                    Ok(ToolResult::ok(r))
                } else {
                    Ok(ToolResult::err(format!("Exit {}\n{}\n{}", o.status.code().unwrap_or(-1), stdout, stderr)))
                }
            }
            Ok(Err(e)) => Ok(ToolResult::err(format!("Exec failed: {}", e))),
            Err(_) => Ok(ToolResult::err(format!("Timeout after {}s", self.timeout_secs))),
        }
    }
}
