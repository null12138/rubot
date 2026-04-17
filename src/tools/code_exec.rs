use anyhow::Result;
use async_trait::async_trait;
use tokio::process::Command;

use super::registry::{Tool, ToolResult};

pub struct CodeExec {
    timeout_secs: u64,
}

impl CodeExec {
    pub fn new(timeout_secs: u64) -> Self {
        Self { timeout_secs }
    }
}

#[async_trait]
impl Tool for CodeExec {
    fn name(&self) -> &str {
        "code_exec"
    }

    fn description(&self) -> &str {
        "Execute code via shell (bash) or Python. Returns stdout and stderr."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "language": {
                    "type": "string",
                    "enum": ["bash", "python"],
                    "description": "Language to execute"
                },
                "code": {
                    "type": "string",
                    "description": "Code to execute"
                }
            },
            "required": ["language", "code"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let language = params["language"].as_str().unwrap_or("bash");
        let code = match params["code"].as_str() {
            Some(c) if !c.is_empty() => c,
            _ => return Ok(ToolResult::err("Missing 'code' parameter".to_string())),
        };

        let (cmd, args) = match language {
            "python" | "python3" => ("python3", vec!["-c", code]),
            "bash" | "sh" | _ => ("bash", vec!["-c", code]),
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            Command::new(cmd).args(&args).output(),
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
                "Execution timed out after {}s",
                self.timeout_secs
            ))),
        }
    }
}
