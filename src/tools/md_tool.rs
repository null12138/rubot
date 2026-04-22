use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::SystemTime;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::code_exec::{human_size, scan_new_files};
use super::registry::{Tool, ToolResult};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lang { Bash, Python }

impl Lang {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "bash" | "sh" => Some(Self::Bash),
            "python" | "py" | "python3" => Some(Self::Python),
            _ => None,
        }
    }
}

pub struct MdTool {
    name: String,
    description: String,
    schema: serde_json::Value,
    language: Lang,
    body: String,
    workdir: PathBuf,
    timeout: u64,
}

impl MdTool {
    pub fn parse(content: &str, workdir: &Path, timeout: u64) -> Result<Self> {
        let Some(rest) = content.strip_prefix("---\n") else {
            bail!("missing frontmatter (expected --- at start)");
        };
        let Some(end) = rest.find("\n---") else {
            bail!("unterminated frontmatter (expected closing ---)");
        };
        let header = &rest[..end];
        let body = rest[end..].trim_start_matches("\n---").trim_start_matches('\n').to_string();

        let mut name = String::new();
        let mut description = String::new();
        let mut language: Option<Lang> = None;
        let mut schema: Option<serde_json::Value> = None;

        for line in header.lines() {
            let Some((k, v)) = line.split_once(':') else { continue; };
            let key = k.trim().to_lowercase();
            let val = v.trim();
            match key.as_str() {
                "name" => name = val.to_string(),
                "description" => description = val.to_string(),
                "language" => language = Lang::parse(val),
                "parameters" => {
                    schema = Some(serde_json::from_str(val)
                        .map_err(|e| anyhow!("parameters must be JSON: {}", e))?);
                }
                _ => {}
            }
        }

        if name.is_empty() { bail!("missing name"); }
        if !name.chars().next().map(|c| c.is_ascii_lowercase() || c == '_').unwrap_or(false)
            || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            bail!("name must match [a-z_][a-z0-9_]*");
        }
        let language = language.ok_or_else(|| anyhow!("language must be bash or python"))?;
        let schema = schema.unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));
        if body.trim().is_empty() { bail!("body is empty"); }

        Ok(Self {
            name,
            description,
            schema,
            language,
            body,
            workdir: workdir.to_path_buf(),
            timeout,
        })
    }
}

#[async_trait]
impl Tool for MdTool {
    fn name(&self) -> &str { &self.name }
    fn description(&self) -> &str { &self.description }
    fn parameters_schema(&self) -> serde_json::Value { self.schema.clone() }
    fn is_md_backed(&self) -> bool { true }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let started = SystemTime::now();
        std::fs::create_dir_all(&self.workdir).ok();

        let exec_res = match self.language {
            Lang::Bash => run_bash(&self.body, &params, &self.workdir, self.timeout).await,
            Lang::Python => run_python(&self.body, &params, &self.workdir, self.timeout).await,
        };

        let (ok, mut res) = match exec_res {
            Ok(r) => r,
            Err(e) => return Ok(ToolResult::err(format!("{:#}", e))),
        };

        let generated = scan_new_files(&self.workdir, started);
        if !generated.is_empty() {
            res.push_str("\n\n[Generated files]\n");
            for (path, size) in &generated {
                res.push_str(&format!("- {} ({})\n", path.display(), human_size(*size)));
            }
        }

        if ok { Ok(ToolResult::ok(res)) } else { Ok(ToolResult::err(res)) }
    }
}

async fn run_bash(body: &str, params: &serde_json::Value, workdir: &Path, timeout: u64) -> Result<(bool, String)> {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("powershell");
        c.arg("-Command").arg(body);
        c
    } else {
        let mut c = Command::new("bash");
        c.arg("-c").arg(body);
        c
    };
    cmd.current_dir(workdir);
    if let Some(obj) = params.as_object() {
        for (k, v) in obj {
            let val = match v {
                serde_json::Value::String(s) => s.clone(),
                _ => serde_json::to_string(v).unwrap_or_default(),
            };
            cmd.env(k, val);
        }
    }
    wait_output(cmd, timeout).await
}

async fn run_python(body: &str, params: &serde_json::Value, workdir: &Path, timeout: u64) -> Result<(bool, String)> {
    let py = if cfg!(target_os = "windows") { "python" } else { "python3" };
    let mut cmd = Command::new(py);
    cmd.arg("-c").arg(body).current_dir(workdir)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let payload = serde_json::to_vec(params)?;
        stdin.write_all(&payload).await?;
        stdin.shutdown().await.ok();
    }
    let fut = child.wait_with_output();
    let out = match tokio::time::timeout(std::time::Duration::from_secs(timeout), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Ok((false, "Timeout".into())),
    };
    let mut res = String::from_utf8_lossy(&out.stdout).to_string();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    if !err.is_empty() { res.push_str(&format!("\n[stderr] {}", err)); }
    Ok((out.status.success(), res))
}

async fn wait_output(mut cmd: Command, timeout: u64) -> Result<(bool, String)> {
    let fut = cmd.output();
    let out = match tokio::time::timeout(std::time::Duration::from_secs(timeout), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Ok((false, "Timeout".into())),
    };
    let mut res = String::from_utf8_lossy(&out.stdout).to_string();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    if !err.is_empty() { res.push_str(&format!("\n[stderr] {}", err)); }
    Ok((out.status.success(), res))
}
