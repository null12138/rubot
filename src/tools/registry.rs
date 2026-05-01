use anyhow::Result;
use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::sync::RwLock;

use crate::llm::types::ToolDefinition;

/// CC-style risk level for tool permission gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskLevel {
    /// Read-only informational (web search, web fetch)
    Low,
    /// Creates files, modifies workspace (latex_pdf, browser)
    Medium,
    /// System-modifying (code_exec, file_ops write)
    High,
    /// Destructive (rm -rf, sudo, dangerous commands)
    Critical,
}

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
    pub fn to_string_for_llm_limited(&self, max_chars: usize) -> String {
        if self.success {
            compact_for_llm(&self.output, max_chars)
        } else {
            format!(
                "[ERROR] {}",
                compact_for_llm(self.error.as_deref().unwrap_or("Unknown error"), max_chars)
            )
        }
    }
}

fn compact_for_llm(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let head_budget = max_chars.saturating_mul(2) / 3;
    let tail_budget = max_chars.saturating_sub(head_budget);
    let head: String = text.chars().take(head_budget).collect();
    let tail_chars: Vec<char> = text.chars().rev().take(tail_budget).collect();
    let tail: String = tail_chars.into_iter().rev().collect();

    format!(
        "{}\n...[truncated {} chars]...\n{}",
        head,
        text.chars()
            .count()
            .saturating_sub(head.chars().count() + tail.chars().count()),
        tail
    )
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult>;
    fn is_md_backed(&self) -> bool {
        false
    }

    // ── CC-style safety properties ──

    /// Concurrent-safe tools can run simultaneously with other concurrent-safe tools.
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    /// Risk level for permission gating.
    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Medium
    }
}

pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Box<dyn Tool>>>,
    md_dir: Option<PathBuf>,
    md_workdir: PathBuf,
    md_timeout: u64,
    md_last_mtime: RwLock<Option<SystemTime>>,
    defs_cache: RwLock<Option<Vec<ToolDefinition>>>,
}

impl ToolRegistry {
    pub fn new(md_dir: Option<PathBuf>, md_workdir: PathBuf, md_timeout: u64) -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            md_dir,
            md_workdir,
            md_timeout,
            md_last_mtime: RwLock::new(None),
            defs_cache: RwLock::new(None),
        }
    }

    pub async fn register(&self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.write().await.insert(name, tool);
        *self.defs_cache.write().await = None;
    }

    /// Execute multiple tool calls in a single batch.
    ///
    /// Tools marked `is_concurrency_safe()` run in parallel (via `join_all`).
    /// Non-concurrent-safe tools run sequentially after the parallel batch.
    /// Results are returned in the same order as `calls`.
    pub async fn execute_batch(
        &self,
        calls: &[(String, serde_json::Value)],
    ) -> Vec<Result<ToolResult>> {
        let tools = self.tools.read().await;
        let n = calls.len();
        let mut results: Vec<Option<Result<ToolResult>>> = (0..n).map(|_| None).collect();

        let mut parallel: Vec<usize> = Vec::new();
        let mut sequential: Vec<usize> = Vec::new();

        for (i, (name, _)) in calls.iter().enumerate() {
            let tool = tools.get(name);
            match tool {
                Some(t) if t.is_concurrency_safe() => parallel.push(i),
                Some(_) => sequential.push(i),
                None => {
                    results[i] =
                        Some(Ok(ToolResult::err(format!("Unknown tool: {name}"))));
                }
            }
        }

        // Concurrent-safe tools → parallel batch
        if !parallel.is_empty() {
            let futures: Vec<_> = parallel
                .iter()
                .map(|&i| {
                    let (name, params) = &calls[i];
                    tools.get(name).unwrap().execute(params.clone())
                })
                .collect();
            let outputs = join_all(futures).await;
            for (&i, output) in parallel.iter().zip(outputs) {
                results[i] = Some(output);
            }
        }

        // Non-concurrent-safe tools → sequential
        for &i in &sequential {
            let (name, params) = &calls[i];
            let output = tools.get(name).unwrap().execute(params.clone()).await;
            results[i] = Some(output);
        }

        results.into_iter().map(|r| r.unwrap()).collect()
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

    /// Look up a registered tool's risk level. Returns None for unknown tools.
    pub async fn risk_level(&self, name: &str) -> Option<RiskLevel> {
        let tools = self.tools.read().await;
        tools.get(name).map(|t| t.risk_level())
    }

    pub async fn definitions(&self) -> Vec<ToolDefinition> {
        let _ = self.rescan_if_changed().await;
        if let Some(defs) = self.defs_cache.read().await.clone() {
            return defs;
        }
        let tools = self.tools.read().await;
        let mut defs: Vec<_> = tools
            .values()
            .map(|t| {
                ToolDefinition::new(t.name(), t.description(), t.parameters_schema())
                    .compact_for_llm()
            })
            .collect();
        defs.sort_by(|a, b| a.function.name.cmp(&b.function.name));
        if self.md_dir.is_some() {
            defs.push(ToolDefinition::new(
                "tool_reload",
                "Force re-scan of workspace/tools/*.md. New tools are normally auto-detected each turn; use this only to force a rescan (e.g. after editing an existing tool).",
                serde_json::json!({"type": "object", "properties": {}, "required": []}),
            ).compact_for_llm());
        }
        drop(tools);
        *self.defs_cache.write().await = Some(defs.clone());
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
        let Some(dir) = self.md_dir.as_ref() else {
            return Ok(0);
        };
        let current = latest_mtime(dir);
        {
            let last = self.md_last_mtime.read().await;
            if *last == current {
                return Ok(0);
            }
        }
        *self.md_last_mtime.write().await = current;
        self.reload_md().await
    }

    pub async fn reload_md(&self) -> Result<usize> {
        let Some(dir) = self.md_dir.as_ref() else {
            return Ok(0);
        };
        if !dir.is_dir() {
            return Ok(0);
        }
        let mut tools = self.tools.write().await;
        tools.retain(|_, t| !t.is_md_backed());
        let mut n = 0usize;
        for ent in std::fs::read_dir(dir)?.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&p) else {
                continue;
            };
            match crate::tools::md_tool::MdTool::parse(&raw, &self.md_workdir, self.md_timeout) {
                Ok(t) => {
                    let name = t.name().to_string();
                    tools.insert(name, Box::new(t));
                    n += 1;
                }
                Err(e) => {
                    tracing::debug!("md tool {}: {:#}", p.display(), e);
                }
            }
        }
        drop(tools);
        *self.defs_cache.write().await = None;
        Ok(n)
    }
}

fn latest_mtime(dir: &Path) -> Option<SystemTime> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut max: Option<SystemTime> = std::fs::metadata(dir).and_then(|m| m.modified()).ok();
    for ent in entries.flatten() {
        let p = ent.path();
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Ok(m) = ent.metadata() {
            if let Ok(mt) = m.modified() {
                max = Some(max.map_or(mt, |x| x.max(mt)));
            }
        }
    }
    max
}
