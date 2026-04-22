use anyhow::{Context, Result};
use tracing::warn;

use crate::config::Config;
use crate::llm::client::LlmClient;
use crate::llm::types::*;
use crate::markdown::{CYAN, DIM, GREEN, R, RED};
use crate::memory::{MemoryLayer, MemorySearch};
use crate::personality;
use crate::planner::{ChainExecutor, StepStatus, ToolCallChain};
use crate::tools::registry::{ToolRegistry, ToolResult};
use crate::tools::{
    code_exec::CodeExec, file_ops::FileOps, latex_pdf::LatexPdf, web_fetch::WebFetch,
    web_search::WebSearch,
};

pub struct Agent {
    config: Config,
    llm: LlmClient,
    tools: ToolRegistry,
    memory: MemorySearch,
    messages: Vec<Message>,
    iteration_count: u32,
    max_iterations: u32,
    last_plan: Option<String>,
}

impl Agent {
    async fn build_runtime(
        config: &Config,
    ) -> Result<(LlmClient, ToolRegistry, MemorySearch, Message)> {
        let llm = LlmClient::new(
            &config.api_base_url,
            &config.api_key,
            &config.model,
            &config.fast_model,
            config.max_retries,
        );

        let md_dir = config.workspace_path.join("tools");
        let md_workdir = config.workspace_path.join("files");
        let tools = ToolRegistry::new(Some(md_dir), md_workdir, config.code_exec_timeout_secs);
        tools.register(Box::new(WebSearch)).await;
        tools.register(Box::new(WebFetch)).await;
        tools
            .register(Box::new(CodeExec::new(
                config.code_exec_timeout_secs,
                &config.workspace_path,
            )))
            .await;
        tools
            .register(Box::new(FileOps::new(&config.workspace_path)))
            .await;
        tools
            .register(Box::new(LatexPdf::new(&config.workspace_path)))
            .await;
        tools.load_md_tools().await?;

        let memory = MemorySearch::new(&config.workspace_path);
        let memory_index = memory
            .get_index_text()
            .await
            .unwrap_or_else(|_| "(empty)".into());
        let system_msg = Message::system(&personality::system_prompt(
            &memory_index,
            &config.workspace_path,
        ));

        Ok((llm, tools, memory, system_msg))
    }

    pub async fn new(config: Config) -> Result<Self> {
        let (llm, tools, memory, system_msg) = Self::build_runtime(&config).await?;

        Ok(Self {
            config,
            llm,
            tools,
            memory,
            messages: vec![system_msg],
            iteration_count: 0,
            max_iterations: 30,
            last_plan: None,
        })
    }

    pub async fn process(&mut self, input: &str) -> Result<String> {
        self.messages.push(Message::user(input));
        self.iteration_count = 0;
        self.run_loop().await
    }

    async fn run_loop(&mut self) -> Result<String> {
        loop {
            self.iteration_count += 1;
            if self.iteration_count > self.max_iterations {
                warn!("Max iterations hit");
                return Ok(format!(
                    "Reached maximum iterations ({}) without converging.",
                    self.max_iterations
                ));
            }

            let tool_defs = self.tools.definitions().await;
            let is_first_round = self.iteration_count == 1;
            let temp = if is_first_round { 0.7 } else { 0.3 };
            let response = if is_first_round {
                self.llm
                    .chat(&self.messages, Some(&tool_defs), Some(temp))
                    .await
            } else {
                self.llm
                    .chat_fast(&self.messages, Some(&tool_defs), Some(temp))
                    .await
            }
            .context("LLM call failed")?;

            let choice = response
                .choices
                .into_iter()
                .next()
                .context("No response from LLM")?;
            let assistant_msg = choice.message;
            self.messages.push(assistant_msg.clone());

            let tool_calls = assistant_msg.tool_calls.unwrap_or_default();
            if !tool_calls.is_empty() {
                let results = self.execute_tools(&tool_calls).await?;
                for (tc, result) in results {
                    self.messages
                        .push(Message::tool_result(&tc.id, &result.to_string_for_llm()));
                }
                continue;
            }

            let response_text = assistant_msg.content.unwrap_or_default();

            if let Some(plan) = extract_plan(&response_text) {
                return self.handle_plan(plan).await;
            }

            if !is_first_round && response_text.trim().len() < 200 {
                let prompt = "Based on the tool results above, provide a comprehensive answer to the user's original question.";
                self.messages.push(Message::user(prompt));
                let resp = self.llm.chat(&self.messages, None, Some(0.7)).await?;
                let final_text = resp
                    .choices
                    .into_iter()
                    .next()
                    .and_then(|c| c.message.content)
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or(response_text);
                self.messages.pop();
                return Ok(final_text);
            }

            return Ok(response_text);
        }
    }

    async fn execute_tools(&self, tool_calls: &[ToolCall]) -> Result<Vec<(ToolCall, ToolResult)>> {
        let mut results = Vec::with_capacity(tool_calls.len());
        for tc in tool_calls {
            let params: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::json!({}));
            let summary = summarize_params(&tc.function.name, &params);
            println!(
                "  {}→{} {}{}{} {}{}{}",
                DIM, R, CYAN, tc.function.name, R, DIM, summary, R
            );
            let result = match self.tools.execute(&tc.function.name, params).await {
                Ok(r) => r,
                Err(e) => ToolResult::err(format!("{:#}", e)),
            };
            let (mark, color) = if result.success {
                ("✓", GREEN)
            } else {
                ("✗", RED)
            };
            let raw = if result.success {
                result.output.as_str()
            } else {
                result.error.as_deref().unwrap_or("")
            };
            let preview: String = raw
                .lines()
                .find(|l| l.trim().len() >= 4)
                .or_else(|| raw.lines().next())
                .map(|l| l.trim().chars().take(80).collect())
                .unwrap_or_default();
            println!("    {}{}{} {}{}{}", color, mark, R, DIM, preview, R);
            results.push((tc.clone(), result));
        }
        Ok(results)
    }

    async fn handle_plan(&mut self, mut chain: ToolCallChain) -> Result<String> {
        let plan_md = chain.to_md();
        println!("\n--- Plan ---\n{}\n--- End Plan ---\n", plan_md);
        self.last_plan = Some(plan_md);

        let mut executor = ChainExecutor::new(&self.tools, self.config.max_retries);
        let outputs = executor.execute(&mut chain).await?;
        self.last_plan = Some(chain.to_md());

        let mut summary = format!("## Plan Results: {}\n\n", chain.goal);
        for (step_id, output) in &outputs {
            let step = &chain.steps[*step_id];
            let status = match step.status {
                StepStatus::Done => "OK",
                StepStatus::Failed => "FAILED",
                _ => "???",
            };
            let preview: String = output.chars().take(200).collect();
            let suffix = if output.chars().count() > 200 {
                "..."
            } else {
                ""
            };
            summary.push_str(&format!(
                "**Step {} [{}]**: {}\n> {}{}\n\n",
                step_id, status, step.desc, preview, suffix
            ));
        }
        if chain.has_failure() {
            summary.push_str("\nSome steps failed.\n");
        }

        self.messages.push(Message::user(&format!(
            "Plan execution complete. Results:\n{}",
            summary
        )));
        let response = self.llm.chat(&self.messages, None, Some(0.7)).await?;
        let final_text = response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content.unwrap_or_default())
            .unwrap_or(summary);
        self.messages.push(Message::assistant(&final_text));
        Ok(final_text)
    }

    pub fn set_model(&mut self, model: &str) {
        self.config.model = model.to_string();
        self.llm.update_model(model);
    }

    pub async fn apply_config(&mut self, config: Config) -> Result<bool> {
        let workspace_changed = self.config.workspace_path != config.workspace_path;
        let (llm, tools, memory, system_msg) = Self::build_runtime(&config).await?;

        self.config = config;
        self.llm = llm;
        self.tools = tools;
        self.memory = memory;
        self.last_plan = None;

        if workspace_changed {
            self.messages.clear();
            self.messages.push(system_msg);
        } else if let Some(first) = self.messages.first_mut() {
            *first = system_msg;
        } else {
            self.messages.push(system_msg);
        }

        Ok(workspace_changed)
    }

    pub fn get_models(&self) -> (String, String) {
        (self.config.model.clone(), self.config.fast_model.clone())
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn last_plan(&self) -> Option<&str> {
        self.last_plan.as_deref()
    }

    pub fn memory(&self) -> &MemorySearch {
        &self.memory
    }

    pub async fn shutdown(&mut self) {
        if let Ok(r) = self.memory.decay().await {
            if r.promoted + r.evicted > 0 {
                tracing::info!(
                    "memory decay: promoted={} evicted={}",
                    r.promoted,
                    r.evicted
                );
            }
        }
        if self.iteration_count <= 2 {
            return;
        }

        let first_user = self
            .messages
            .iter()
            .find(|m| {
                m.role == Role::User && m.content.as_deref().is_some_and(|c| !c.trim().is_empty())
            })
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        let last_assistant = self
            .messages
            .iter()
            .rev()
            .find(|m| {
                m.role == Role::Assistant
                    && m.content.as_deref().is_some_and(|c| !c.trim().is_empty())
            })
            .and_then(|m| m.content.clone())
            .unwrap_or_default();

        if first_user.trim().is_empty() || last_assistant.trim().is_empty() {
            return;
        }

        let summary = truncate(first_user.lines().next().unwrap_or(&first_user).trim(), 80);
        let body = format!(
            "Q: {}\n\nA: {}\n\nTool rounds: {}",
            truncate(first_user.trim(), 120),
            truncate(last_assistant.trim(), 300),
            self.iteration_count,
        );

        let _ = self
            .memory
            .add_memory(MemoryLayer::Working, &summary, &body, &[])
            .await;
    }
}

fn truncate(s: &str, n: usize) -> String {
    let taken: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{}…", taken)
    } else {
        taken
    }
}

fn extract_plan(text: &str) -> Option<ToolCallChain> {
    let json_start = text.find("```json")?;
    let json_content = &text[json_start + 7..];
    let json_end = json_content.find("```")?;
    let json_str = json_content[..json_end].trim();

    let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;
    if parsed.get("type")?.as_str()? != "plan" {
        return None;
    }

    let goal = parsed.get("goal")?.as_str()?.to_string();
    let steps = parsed.get("steps")?.as_array()?;

    let mut chain = ToolCallChain::new(&goal);
    for (i, step) in steps.iter().enumerate() {
        chain.add_step(
            step.get("tool")?.as_str()?,
            step.get("params").cloned().unwrap_or(serde_json::json!({})),
            step.get("description")
                .and_then(|d| d.as_str())
                .unwrap_or(&format!("Step {}", i)),
            step.get("depends_on")
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as usize))
                        .collect()
                })
                .unwrap_or_default(),
        );
    }
    Some(chain)
}

fn summarize_params(tool_name: &str, params: &serde_json::Value) -> String {
    match tool_name {
        "web_fetch" => params["url"].as_str().unwrap_or("").to_string(),
        "web_search" => params["query"].as_str().unwrap_or("").to_string(),
        "code_exec" => {
            let lang = params["lang"]
                .as_str()
                .or_else(|| params["language"].as_str())
                .unwrap_or("bash");
            let code = params["code"].as_str().unwrap_or("");
            let first = code
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            let truncated: String = first.chars().take(80).collect();
            format!("[{}] {}", lang, truncated)
        }
        "file_ops" => {
            let action = params["action"].as_str().unwrap_or("?");
            let path = params["path"].as_str().unwrap_or("");
            format!("{} {}", action, path)
        }
        _ => params.to_string().chars().take(50).collect(),
    }
}
