use anyhow::Result;
use chrono::Utc;
use std::path::{Path, PathBuf};

/// Manages current_plan.md and execution_log.md
pub struct StateManager {
    state_dir: PathBuf,
}

impl StateManager {
    pub fn new(workspace: &Path) -> Self {
        Self {
            state_dir: workspace.join("state"),
        }
    }

    /// Save the current plan to current_plan.md
    pub async fn save_plan(&self, plan_md: &str) -> Result<()> {
        let path = self.state_dir.join("current_plan.md");
        tokio::fs::write(&path, plan_md).await?;
        Ok(())
    }

    /// Load the current plan (if exists)
    pub async fn load_plan(&self) -> Result<Option<String>> {
        let path = self.state_dir.join("current_plan.md");
        if path.exists() {
            let content = tokio::fs::read_to_string(&path).await?;
            if content.trim().is_empty() || content.trim() == "# Plan:" {
                Ok(None)
            } else {
                Ok(Some(content))
            }
        } else {
            Ok(None)
        }
    }

    /// Clear the current plan
    pub async fn clear_plan(&self) -> Result<()> {
        let path = self.state_dir.join("current_plan.md");
        tokio::fs::write(&path, "").await?;
        Ok(())
    }

    /// Append to execution log
    pub async fn log_action(&self, tool: &str, summary: &str, success: bool) -> Result<()> {
        let path = self.state_dir.join("execution_log.md");
        let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
        let status = if success { "OK" } else { "FAIL" };

        let entry = format!("| {} | {} | {} | {} |\n", timestamp, tool, status, summary);

        // Initialize the log file with header if it doesn't exist
        if !path.exists() {
            let header =
                "# Execution Log\n\n| Timestamp | Tool | Status | Summary |\n|---|---|---|---|\n";
            tokio::fs::write(&path, header).await?;
        }

        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(entry.as_bytes()).await?;

        Ok(())
    }

    /// Get the execution log
    pub async fn get_log(&self) -> Result<String> {
        let path = self.state_dir.join("execution_log.md");
        if path.exists() {
            Ok(tokio::fs::read_to_string(&path).await?)
        } else {
            Ok("(no execution log)".to_string())
        }
    }

    /// Check if there's an unfinished plan from a previous session
    pub async fn has_unfinished_plan(&self) -> bool {
        if let Ok(Some(plan)) = self.load_plan().await {
            plan.contains("[ ]") || plan.contains("[~]")
        } else {
            false
        }
    }
}
