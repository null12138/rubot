use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::sync::RwLock;

use crate::llm::types::ToolDefinition;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
}

impl ToolResult {
    pub fn ok(output: String) -> Self {
        Self { success: true, output, error: None }
    }
    pub fn err(error: String) -> Self {
        Self { success: false, output: String::new(), error: Some(error) }
    }
    pub fn to_string_for_llm(&self) -> String {
        if self.success { self.output.clone() }
        else { format!("[ERROR] {}", self.error.as_deref().unwrap_or("Unknown error")) }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult>;
    fn is_md_backed(&self) -> bool { false }
}

pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Box<dyn Tool>>>,
    md_dir: Option<PathBuf>,
    md_workdir: PathBuf,
    md_timeout: u64,
    md_last_mtime: RwLock<Option<SystemTime>>,
}

impl ToolRegistry {
    pub fn new(md_dir: Option<PathBuf>, md_workdir: PathBuf, md_timeout: u64) -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            md_dir,
            md_workdir,
            md_timeout,
            md_last_mtime: RwLock::new(None),
        }
    }

    pub async fn register(&self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.write().await.insert(name, tool);
    }

    pub async fn execute(&self, name: &str, params: serde_json::Value) -> Result<ToolResult> {
        if name == "tool_reload" {
            return match self.reload_md().await {
                Ok(n) => Ok(ToolResult::ok(format!("Reloaded {} md tools", n))),
                Err(e) => Ok(ToolResult::err(format!("{:#}", e))),
            };
        }
        {
            let tools = self.tools.read().await;
            if let Some(tool) = tools.get(name) {
                return tool.execute(params).await;
            }
        }
        // Miss: maybe the LLM just wrote a new md tool. Try an incremental rescan and retry.
        let _ = self.rescan_if_changed().await;
        let tools = self.tools.read().await;
        match tools.get(name) {
            Some(tool) => tool.execute(params).await,
            None => Ok(ToolResult::err(format!("Unknown tool: {}", name))),
        }
    }

    pub async fn definitions(&self) -> Vec<ToolDefinition> {
        let _ = self.rescan_if_changed().await;
        let tools = self.tools.read().await;
        let mut defs: Vec<_> = tools.values()
            .map(|t| ToolDefinition::new(t.name(), t.description(), t.parameters_schema()))
            .collect();
        if self.md_dir.is_some() {
            defs.push(ToolDefinition::new(
                "tool_reload",
                "Force re-scan of workspace/tools/*.md. New tools are normally auto-detected each turn; use this only to force a rescan (e.g. after editing an existing tool).",
                serde_json::json!({"type": "object", "properties": {}, "required": []}),
            ));
        }
        defs
    }

    pub async fn load_md_tools(&self) -> Result<usize> {
        let n = self.reload_md().await?;
        if let Some(dir) = self.md_dir.as_ref() {
            *self.md_last_mtime.write().await = latest_mtime(dir);
        }
        Ok(n)
    }

    /// Cheap check: only re-scan when the md dir's newest file mtime changed.
    /// Enables autonomous registration — LLM drops a new md file, next turn picks it up.
    pub async fn rescan_if_changed(&self) -> Result<usize> {
        let Some(dir) = self.md_dir.as_ref() else { return Ok(0); };
        let current = latest_mtime(dir);
        {
            let last = self.md_last_mtime.read().await;
            if *last == current { return Ok(0); }
        }
        *self.md_last_mtime.write().await = current;
        self.reload_md().await
    }

    pub async fn reload_md(&self) -> Result<usize> {
        let Some(dir) = self.md_dir.as_ref() else { return Ok(0); };
        if !dir.is_dir() { return Ok(0); }
        let mut tools = self.tools.write().await;
        tools.retain(|_, t| !t.is_md_backed());
        let mut n = 0usize;
        for ent in std::fs::read_dir(dir)?.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("md") { continue; }
            let Ok(raw) = std::fs::read_to_string(&p) else { continue; };
            match crate::tools::md_tool::MdTool::parse(&raw, &self.md_workdir, self.md_timeout) {
                Ok(t) => {
                    let name = t.name().to_string();
                    tools.insert(name, Box::new(t));
                    n += 1;
                }
                Err(e) => {
                    tracing::warn!("md tool {}: {:#}", p.display(), e);
                }
            }
        }
        Ok(n)
    }
}

fn latest_mtime(dir: &Path) -> Option<SystemTime> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut max: Option<SystemTime> = std::fs::metadata(dir).and_then(|m| m.modified()).ok();
    for ent in entries.flatten() {
        let p = ent.path();
        if p.extension().and_then(|e| e.to_str()) != Some("md") { continue; }
        if let Ok(m) = ent.metadata() {
            if let Ok(mt) = m.modified() {
                max = Some(max.map_or(mt, |x| x.max(mt)));
            }
        }
    }
    max
}
