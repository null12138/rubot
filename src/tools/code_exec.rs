use anyhow::Result;
use async_trait::async_trait;
use tokio::process::Command;
use std::path::{Path, PathBuf};
use super::registry::{Tool, ToolResult};

pub struct CodeExec { pub timeout: u64, pub dir: PathBuf }
impl CodeExec { 
    pub fn new(t: u64, ws: &Path) -> Self { 
        let d = ws.join("files");
        let _ = std::fs::create_dir_all(&d);
        Self { timeout: t, dir: d.canonicalize().unwrap_or(d) } 
    } 
}

#[async_trait]
impl Tool for CodeExec {
    fn name(&self) -> &str { "code_exec" }
    fn description(&self) -> &str { "Execute bash or python code in the sandbox." }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"lang": {"type": "string", "enum": ["bash", "python"]}, "code": {"type": "string"}}, "required": ["lang", "code"]})
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let lang = params["lang"].as_str().unwrap_or("bash");
        let code = params["code"].as_str().unwrap_or("");
        let (cmd, args) = match lang { "python" => ("python3", vec!["-c", code]), _ => ("bash", vec!["-c", code]) };

        let mut child = Command::new(cmd);
        child.args(&args).current_dir(&self.dir);

        match tokio::time::timeout(std::time::Duration::from_secs(self.timeout), child.output()).await {
            Ok(Ok(o)) => {
                let mut res = String::from_utf8_lossy(&o.stdout).to_string();
                let err = String::from_utf8_lossy(&o.stderr).to_string();
                if !err.is_empty() { res.push_str(&format!("\n[stderr] {}", err)); }
                if o.status.success() { Ok(ToolResult::ok(res)) }
                else { Ok(ToolResult::err(format!("Exit {}: {}", o.status.code().unwrap_or(-1), res))) }
            }
            Ok(Err(e)) => Ok(ToolResult::err(e.to_string())),
            Err(_) => Ok(ToolResult::err("Timeout".into())),
        }
    }
}
