mod plan;
mod runtime;
mod session;
mod stall;
#[cfg(test)]
mod tests;
mod utils;

pub(crate) use plan::should_auto_plan_mode;
pub(crate) use session::clear_session_snapshot_file;

use self::utils::{
    extract_json_object, format_subagent_snapshot, format_subagent_summary,
    is_internal_control_message, looks_like_internal_assistant_message, push_unique_limited,
    summarize_params, truncate, MAX_TOOL_RESULT_CHARS, MAX_TRACKED_TOOL_ROUNDS,
};
use crate::config::{self, Config, ConfigKey};
use crate::llm::client::LlmClient;
use crate::llm::types::*;
use crate::markdown::{CYAN, DIM, GREEN, R, RED};
use crate::memory::{MemoryLayer, MemorySearch};
use crate::subagent::SubagentManager;
use crate::tools::registry::{ToolRegistry, ToolResult};

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct BillingInfo {
    /// Compact display for the one-line HUD, e.g. "$12.34" or "45%"
    pub short: String,
    /// Detail lines for /usage, e.g. ["Token usage (5h): 45%", "MCP usage (1m): 12%"]
    pub lines: Vec<String>,
}

pub struct Agent {
    pub(crate) config: Config,
    pub(crate) llm: LlmClient,
    pub(crate) sleep_llm: LlmClient,
    pub(crate) tools: ToolRegistry,
    pub(crate) memory: MemorySearch,
    pub(crate) messages: Vec<Message>,
    pub(crate) current_request: Option<String>,
    pub(crate) iteration_count: u32,
    pub(crate) max_iterations: u32,
    pub(crate) last_plan: Option<String>,
    pub(crate) subagents: SubagentManager,
    pub(crate) history_summary: Option<String>,
    pub(crate) is_subagent: bool,
    pub(crate) restored_session_messages: usize,
    pub(crate) blocked_domains: BTreeSet<String>,
    pub(crate) session_start: Instant,
    pub(crate) last_activity: Instant,
    pub(crate) last_request_start: Option<Instant>,
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) request_count: u32,
    pub billing: Option<BillingInfo>,
    pub channel_send_queue: Arc<Mutex<Vec<PathBuf>>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolAttempt {
    pub(crate) name: String,
    pub(crate) summary: String,
    pub(crate) success: bool,
    pub(crate) preview: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ExecutedTool {
    pub(crate) call: ToolCall,
    pub(crate) result: ToolResult,
    pub(crate) attempt: ToolAttempt,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ToolRoundReport {
    pub(crate) entries: Vec<ExecutedTool>,
    pub(crate) newly_blocked_domains: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionSnapshot {
    pub(crate) version: u32,
    pub(crate) saved_at: String,
    pub(crate) history_summary: Option<String>,
    pub(crate) current_request: Option<String>,
    pub(crate) messages: Vec<Message>,
}

impl ToolRoundReport {
    fn has_success(&self) -> bool {
        self.entries.iter().any(|entry| entry.attempt.success)
    }

    fn repeated_failure_signatures(&self, previous: &Self) -> Vec<String> {
        let current = self
            .entries
            .iter()
            .filter(|entry| !entry.attempt.success)
            .map(|entry| entry.attempt.signature())
            .collect::<BTreeSet<_>>();
        let prior = previous
            .entries
            .iter()
            .filter(|entry| !entry.attempt.success)
            .map(|entry| entry.attempt.signature())
            .collect::<BTreeSet<_>>();

        current.intersection(&prior).cloned().collect()
    }
}

impl ToolAttempt {
    fn signature(&self) -> String {
        format!("{} {}", self.name, self.summary)
    }
}

impl Agent {
    pub async fn new(config: Config) -> Result<Self> {
        let (llm, sleep_llm, tools, memory, prompt_messages) = Self::build_runtime(&config).await?;

        let agent = Self {
            config,
            llm,
            sleep_llm,
            tools,
            memory,
            messages: prompt_messages,
            current_request: None,
            iteration_count: 0,
            max_iterations: 30,
            last_plan: None,
            subagents: SubagentManager::new(),
            history_summary: None,
            is_subagent: false,
            restored_session_messages: 0,
            blocked_domains: BTreeSet::new(),
            session_start: Instant::now(),
            last_activity: Instant::now(),
            last_request_start: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            request_count: 0,
            billing: None,
            channel_send_queue: Arc::new(Mutex::new(Vec::new())),
        };
        // Run memory decay on session start so stale entries are cleaned.
        if let Ok(r) = agent.memory.decay().await {
            if r.promoted + r.evicted > 0 {
                tracing::info!(
                    "memory decay on start: promoted={} evicted={}",
                    r.promoted,
                    r.evicted
                );
            }
        }
        agent.restore_session()
    }

    pub async fn process(&mut self, input: &str) -> Result<String> {
        // Sleep consolidation: if idle long enough, let the dream model consolidate memories.
        let idle_secs = self.last_activity.elapsed().as_secs();
        if idle_secs >= self.config.sleep_interval_secs && !self.is_subagent {
            self.sleep_consolidate().await;
        }
        self.last_activity = Instant::now();

        self.current_request = Some(input.trim().to_string());
        self.last_request_start = Some(Instant::now());
        self.messages.push(Message::user(input));
        self.iteration_count = 0;
        let result = if should_auto_plan_mode(input) {
            self.run_plan_mode(None).await
        } else {
            self.run_loop().await
        };
        // Refresh billing info in background (don't block response).
        if self.billing.is_none() && !self.is_subagent {
            self.billing = self.fetch_billing().await;
        }
        result
    }

    async fn run_loop(&mut self) -> Result<String> {
        let mut recent_tool_rounds = Vec::<ToolRoundReport>::new();
        let mut stall_subagent_spawned = false;
        loop {
            self.iteration_count += 1;
            if self.iteration_count > self.max_iterations {
                tracing::warn!("Max iterations hit");
                return Ok(self.build_nonconverged_response(
                    &format!(
                        "Reached maximum iterations ({}) without converging.",
                        self.max_iterations
                    ),
                    &recent_tool_rounds,
                ));
            }

            self.compact_message_history();
            let tool_defs = self.tool_definitions().await;
            let is_first_round = self.iteration_count == 1;
            let temp = if is_first_round { 0.7 } else { 0.3 };
            let llm_messages = self.llm_messages();
            let response = if is_first_round {
                self.llm
                    .chat(&llm_messages, Some(&tool_defs), Some(temp))
                    .await
            } else {
                self.llm
                    .chat_fast(&llm_messages, Some(&tool_defs), Some(temp))
                    .await
            }
            .context("LLM call failed")?;

            self.track_usage(&response);
            self.request_count += 1;

            let choice = response
                .choices
                .into_iter()
                .next()
                .context("No response from LLM")?;
            let assistant_msg = choice.message;
            self.messages.push(assistant_msg.clone());

            let tool_calls = assistant_msg.tool_calls.unwrap_or_default();
            if !tool_calls.is_empty() {
                let round = self.execute_tools(&tool_calls).await?;
                for executed in &round.entries {
                    self.messages.push(Message::tool_result(
                        &executed.call.id,
                        &executed
                            .result
                            .to_string_for_llm_limited(MAX_TOOL_RESULT_CHARS),
                    ));
                }
                if !round.newly_blocked_domains.is_empty() {
                    self.messages
                        .push(Message::user(&stall::build_blocked_source_prompt(
                            &round.newly_blocked_domains,
                        )));
                }

                if let Some(repeated) =
                    stall::repeated_failure_signatures(&recent_tool_rounds, &round)
                {
                    let auto_subagent_id = self
                        .maybe_spawn_stall_diagnostic_subagent(
                            &repeated,
                            &mut stall_subagent_spawned,
                        )
                        .await;
                    let prompt =
                        stall::build_stall_recovery_prompt(&repeated, auto_subagent_id.as_deref());
                    self.messages.push(Message::user(&prompt));
                }

                recent_tool_rounds.push(round);
                if recent_tool_rounds.len() > MAX_TRACKED_TOOL_ROUNDS {
                    recent_tool_rounds.remove(0);
                }
                // Periodic memory decay every ~10 tool rounds.
                if self.iteration_count % 10 == 0 {
                    if let Ok(r) = self.memory.decay().await {
                        if r.promoted + r.evicted > 0 {
                            tracing::info!(
                                "memory decay: promoted={} evicted={}",
                                r.promoted,
                                r.evicted
                            );
                        }
                    }
                }
                continue;
            }

            let response_text = assistant_msg.content.unwrap_or_default();

            if let Some(plan) = plan::extract_plan(&response_text) {
                return self.run_plan_mode(Some(plan)).await;
            }

            if stall::request_needs_artifact_verification(self.current_request.as_deref())
                && !stall::has_recent_artifact_verification(&recent_tool_rounds)
            {
                self.messages.push(Message::user(
                    "Before answering, verify the exact files that were actually saved on disk. Use `[Generated files]` tool output or run `file_ops list` on the target directory. Do not count attempted downloads as success, and do not present alternative-site files as if they came from the original source unless you say so explicitly.",
                ));
                continue;
            }

            if !is_first_round && response_text.trim().len() < 200 {
                let prompt = "Based on the tool results above, provide a comprehensive answer to the user's original question.";
                self.messages.push(Message::user(prompt));
                self.compact_message_history();
                let resp = self.llm.chat(&self.llm_messages(), None, Some(0.7)).await?;
                self.track_usage(&resp);
                self.request_count += 1;
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

    async fn execute_tools(&mut self, tool_calls: &[ToolCall]) -> Result<ToolRoundReport> {
        let mut entries = Vec::with_capacity(tool_calls.len());
        let mut newly_blocked_domains = Vec::new();
        for tc in tool_calls {
            let params: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::json!({}));
            let summary = summarize_params(&tc.function.name, &params);
            println!(
                "  {}→{} {}{}{} {}{}{}",
                DIM, R, CYAN, tc.function.name, R, DIM, summary, R
            );
            let result = match self
                .execute_tool_call(&tc.function.name, params.clone())
                .await
            {
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
            let success = result.success;
            if let Some(domain) = stall::detect_new_blocked_domain(
                &self.blocked_domains,
                &tc.function.name,
                &params,
                &result,
            ) {
                self.blocked_domains.insert(domain.clone());
                push_unique_limited(&mut newly_blocked_domains, domain, 8);
            }
            entries.push(ExecutedTool {
                call: tc.clone(),
                result,
                attempt: ToolAttempt {
                    name: tc.function.name.clone(),
                    summary,
                    success,
                    preview,
                },
            });
        }
        Ok(ToolRoundReport {
            entries,
            newly_blocked_domains,
        })
    }

    async fn execute_tool_call(
        &mut self,
        name: &str,
        params: serde_json::Value,
    ) -> Result<ToolResult> {
        match name {
            "rubot_command" => self.rubot_command(params).await,
            "channel_send" => self.channel_send(params).await,
            "subagent_spawn" => self.subagent_spawn(params).await,
            "subagent_wait" => self.subagent_wait(params).await,
            "subagent_list" => self.subagent_list().await,
            "subagent_close" => self.subagent_close(params).await,
            "memory_search" => self.memory_search(params).await,
            "memory_add" => self.memory_add(params).await,
            "memory_touch" => self.memory_touch(params).await,
            "memory_due" => self.memory_due().await,
            "tool_create" => self.tool_create(params).await,
            "tool_delete" => self.tool_delete(params).await,
            _ => self.tools.execute(name, params).await,
        }
    }

    async fn subagent_spawn(&self, params: serde_json::Value) -> Result<ToolResult> {
        let task = params["task"].as_str().unwrap_or("").trim().to_string();
        if task.is_empty() {
            return Ok(ToolResult::err("missing task".into()));
        }
        let share_history = params["share_history"].as_bool().unwrap_or(false);
        let use_heavy = params["model"].as_str().unwrap_or("fast") == "heavy";
        let timeout_secs = params["timeout_secs"].as_u64();
        let mut config = self.config.clone();
        if !use_heavy {
            // Subagent uses fast model by default for first-turn cost savings.
            config.model = config.fast_model.clone();
        }
        let seed_messages = share_history.then(|| self.shareable_messages());
        let task_for_runner = task.clone();
        let task_display = task.clone();
        let id = self
            .subagents
            .spawn(task.clone(), share_history, move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(async move {
                    let mut agent = Agent::new(config).await?;
                    agent.is_subagent = true;
                    if let Some(messages) = seed_messages {
                        agent.messages = messages;
                    }
                    agent.max_iterations = 12; // Subagents get fewer iterations.
                    let process_fut = agent.process(&task_for_runner);
                    let result = if let Some(secs) = timeout_secs {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(secs),
                            process_fut,
                        )
                        .await
                        {
                            Ok(r) => r,
                            Err(_) => {
                                agent.shutdown().await;
                                Ok(format!(
                                    "Subagent timed out after {}s. Last output: {}",
                                    secs,
                                    agent
                                        .messages
                                        .iter()
                                        .rev()
                                        .find(|m| m.role == Role::Assistant)
                                        .and_then(|m| m.content.clone())
                                        .unwrap_or_default()
                                ))
                            }
                        }
                    } else {
                        process_fut.await
                    };
                    agent.shutdown().await;
                    result
                })
            })
            .await;
        let model_label = if use_heavy { "heavy" } else { "fast" };
        Ok(ToolResult::ok(format!(
            "Spawned subagent `{}`.\n- task: {}\n- model: {}\n- share_history: {}",
            id, task_display, model_label, share_history
        )))
    }

    async fn subagent_wait(&self, params: serde_json::Value) -> Result<ToolResult> {
        let id = params["id"].as_str().unwrap_or("").trim();
        if id.is_empty() {
            return Ok(ToolResult::err("missing id".into()));
        }
        let timeout_secs = params["timeout_secs"].as_u64();
        match self
            .subagents
            .wait(id, timeout_secs.map(std::time::Duration::from_secs))
            .await
        {
            Ok(snapshot) => Ok(ToolResult::ok(format_subagent_snapshot(&snapshot))),
            Err(e) => Ok(ToolResult::err(format!("{:#}", e))),
        }
    }

    async fn subagent_list(&self) -> Result<ToolResult> {
        let snapshots = self.subagents.list().await;
        if snapshots.is_empty() {
            return Ok(ToolResult::ok("No subagents.".into()));
        }
        let body = snapshots
            .iter()
            .map(format_subagent_summary)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult::ok(body))
    }

    async fn subagent_close(&self, params: serde_json::Value) -> Result<ToolResult> {
        let id = params["id"].as_str().unwrap_or("").trim();
        if id.is_empty() {
            return Ok(ToolResult::err("missing id".into()));
        }
        match self.subagents.close(id).await {
            Ok(snapshot) => Ok(ToolResult::ok(format_subagent_snapshot(&snapshot))),
            Err(e) => Ok(ToolResult::err(format!("{:#}", e))),
        }
    }

    async fn memory_search(&self, params: serde_json::Value) -> Result<ToolResult> {
        let query = params["query"].as_str().unwrap_or("").trim();
        if query.is_empty() {
            return Ok(ToolResult::err("missing query".into()));
        }
        match self.memory.quick_search(query).await {
            Ok(entries) => {
                if entries.is_empty() {
                    return Ok(ToolResult::ok("No matching memories found.".into()));
                }
                let mut out = String::new();
                for e in &entries {
                    out.push_str(&format!(
                        "- `{}` (s{}) [{}]: {}\n",
                        e.file,
                        e.strength,
                        e.tags.join(", "),
                        e.summary
                    ));
                }
                Ok(ToolResult::ok(out.trim_end().to_string()))
            }
            Err(e) => Ok(ToolResult::err(format!("{:#}", e))),
        }
    }

    async fn memory_add(&mut self, params: serde_json::Value) -> Result<ToolResult> {
        let summary = params["summary"].as_str().unwrap_or("").trim();
        let content = params["content"].as_str().unwrap_or("").trim();
        if summary.is_empty() || content.is_empty() {
            return Ok(ToolResult::err("missing summary or content".into()));
        }
        let layer = match params["layer"].as_str().unwrap_or("working") {
            "semantic" => MemoryLayer::Semantic,
            "episodic" => MemoryLayer::Episodic,
            _ => MemoryLayer::Working,
        };
        let tags: Vec<&str> = params["tags"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        match self.memory.add_memory(layer, summary, content, &tags).await {
            Ok(rel) => Ok(ToolResult::ok(format!("Stored at {}", rel))),
            Err(e) => Ok(ToolResult::err(format!("{:#}", e))),
        }
    }

    async fn memory_touch(&self, params: serde_json::Value) -> Result<ToolResult> {
        let file = params["file"].as_str().unwrap_or("").trim();
        if file.is_empty() {
            return Ok(ToolResult::err("missing file id".into()));
        }
        match self.memory.touch(file).await {
            Ok(true) => Ok(ToolResult::ok(format!("Touched {}", file))),
            Ok(false) => Ok(ToolResult::err(format!("not found: {}", file))),
            Err(e) => Ok(ToolResult::err(format!("{:#}", e))),
        }
    }

    async fn memory_due(&self) -> Result<ToolResult> {
        match self.memory.due().await {
            Ok(entries) => {
                if entries.is_empty() {
                    return Ok(ToolResult::ok("No memories due for review.".into()));
                }
                let mut out = String::new();
                for e in &entries {
                    out.push_str(&format!(
                        "- `{}` (s{}) [{}]: {}\n",
                        e.file,
                        e.strength,
                        e.tags.join(", "),
                        e.summary
                    ));
                }
                Ok(ToolResult::ok(out.trim_end().to_string()))
            }
            Err(e) => Ok(ToolResult::err(format!("{:#}", e))),
        }
    }

    async fn tool_create(&mut self, params: serde_json::Value) -> Result<ToolResult> {
        let name = params["name"].as_str().unwrap_or("").trim();
        let description = params["description"].as_str().unwrap_or("").trim();
        let language = params["language"].as_str().unwrap_or("bash").trim();
        let code = params["code"].as_str().unwrap_or("").trim();
        if name.is_empty() || description.is_empty() || code.is_empty() {
            return Ok(ToolResult::err("missing name, description, or code".into()));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Ok(ToolResult::err(
                "name must be lowercase letters, digits, underscores".into(),
            ));
        }
        let params_schema = params
            .get("parameters")
            .map(|p| p.to_string())
            .unwrap_or_else(|| r#"{"type":"object","properties":{},"required":[]}"#.to_string());
        let lang_label = match language {
            "python" | "py" | "python3" => "python",
            _ => "bash",
        };
        let md_content = format!(
            "---\nname: {name}\ndescription: {description}\nlanguage: {lang_label}\nparameters: {params_schema}\n---\n{code}\n",
        );
        let tools_dir = self.config.workspace_path.join("tools");
        let file_path = tools_dir.join(format!("{}.md", name));
        if let Err(e) = tokio::fs::write(&file_path, &md_content).await {
            return Ok(ToolResult::err(format!("failed to write tool: {:#}", e)));
        }
        match self.tools.reload_md().await {
            Ok(n) => Ok(ToolResult::ok(format!(
                "Created tool `{}` ({}) — {} md tools loaded",
                name, lang_label, n
            ))),
            Err(e) => Ok(ToolResult::err(format!("{:#}", e))),
        }
    }

    async fn tool_delete(&mut self, params: serde_json::Value) -> Result<ToolResult> {
        let name = params["name"].as_str().unwrap_or("").trim();
        if name.is_empty() {
            return Ok(ToolResult::err("missing name".into()));
        }
        let file_path = self
            .config
            .workspace_path
            .join("tools")
            .join(format!("{}.md", name));
        if !file_path.is_file() {
            return Ok(ToolResult::err(format!("tool not found: {}", name)));
        }
        if let Err(e) = tokio::fs::remove_file(&file_path).await {
            return Ok(ToolResult::err(format!("failed to delete: {:#}", e)));
        }
        let _ = self.tools.reload_md().await;
        Ok(ToolResult::ok(format!("Deleted tool `{}`", name)))
    }

    async fn channel_send(&self, params: serde_json::Value) -> Result<ToolResult> {
        let path_str = params["path"].as_str().unwrap_or("").trim();
        if path_str.is_empty() {
            return Ok(ToolResult::err("missing path parameter".into()));
        }
        let path = PathBuf::from(path_str);
        let path = if path.is_relative() {
            let resolved = self.config.workspace_path.join("files").join(&path);
            if resolved.exists() {
                resolved
            } else {
                let alt = self.config.workspace_path.join(&path);
                if alt.exists() {
                    alt
                } else {
                    return Ok(ToolResult::err(format!(
                        "file not found: {} (cwd is workspace/files/)",
                        path_str
                    )));
                }
            }
        } else {
            path
        };
        if !path.is_file() {
            return Ok(ToolResult::err(format!(
                "file not found: {}",
                path.display()
            )));
        }
        self.channel_send_queue.lock().await.push(path.clone());
        Ok(ToolResult::ok(format!(
            "File queued for WeChat delivery: {}",
            path.display()
        )))
    }

    async fn rubot_command(&mut self, params: serde_json::Value) -> Result<ToolResult> {
        let command = params["command"].as_str().unwrap_or("").trim().to_string();
        if command.is_empty() {
            return Ok(ToolResult::err("missing command".into()));
        }

        let parts: Vec<&str> = command.split_whitespace().collect();
        let Some(name) = parts.first().copied() else {
            return Ok(ToolResult::err("missing command".into()));
        };

        match name {
            "/model" => {
                if parts.len() > 1 {
                    let value = parts[1..].join(" ");
                    self.set_model(&value).await?;
                    Ok(ToolResult::ok(format!("model set to {}", value)))
                } else {
                    let (heavy, fast) = self.get_models();
                    Ok(ToolResult::ok(format!("heavy={} fast={}", heavy, fast)))
                }
            }
            "/config" => self.rubot_config_command(&parts).await,
            _ => Ok(ToolResult::err(format!(
                "unsupported rubot command: {}",
                command
            ))),
        }
    }

    async fn rubot_config_command(&mut self, parts: &[&str]) -> Result<ToolResult> {
        let sub = parts.get(1).copied().unwrap_or("list");
        match sub {
            "" | "list" => {
                let env_path = config::env_file_path()?;
                let rows = self.config.rows();
                let mut out = format!(".env: {}\n\n", env_path.display());
                for row in rows {
                    out.push_str(&format!(
                        "{:<18} {:<24} {}\n",
                        row.key.cli_name(),
                        row.env_name,
                        row.display_value
                    ));
                }
                Ok(ToolResult::ok(out.trim_end().to_string()))
            }
            "get" => {
                let Some(raw_key) = parts.get(2) else {
                    return Ok(ToolResult::err("usage: /config get <key>".into()));
                };
                let Some(key) = ConfigKey::parse(raw_key) else {
                    return Ok(ToolResult::err(format!("unknown config key: {}", raw_key)));
                };
                if let Some(row) = self.config.rows().into_iter().find(|row| row.key == key) {
                    Ok(ToolResult::ok(format!(
                        "{} ({}) = {}",
                        row.key.cli_name(),
                        row.env_name,
                        row.display_value
                    )))
                } else {
                    Ok(ToolResult::err(format!("unknown config key: {}", raw_key)))
                }
            }
            "set" => {
                let Some(raw_key) = parts.get(2) else {
                    return Ok(ToolResult::err("usage: /config set <key> <value>".into()));
                };
                let Some(key) = ConfigKey::parse(raw_key) else {
                    return Ok(ToolResult::err(format!("unknown config key: {}", raw_key)));
                };
                let value = parts.get(3..).map(|s| s.join(" ")).unwrap_or_default();
                if value.trim().is_empty() {
                    return Ok(ToolResult::err("usage: /config set <key> <value>".into()));
                }

                let env_path = config::save_config_value(key, &value)?;
                let new_config = Config::load()?;
                let reset = self.apply_config(new_config).await?;
                let display = if key == ConfigKey::ApiKey || key == ConfigKey::TavilyApiKey {
                    "********".to_string()
                } else {
                    value.trim().to_string()
                };
                let mut out = format!(
                    "saved {}={} to {}",
                    key.cli_name(),
                    display,
                    env_path.display()
                );
                if reset {
                    out.push_str("\nworkspace changed; session conversation was reset");
                } else {
                    out.push_str("\napplied to current session");
                }
                Ok(ToolResult::ok(out))
            }
            "help" => Ok(ToolResult::ok(
                "usage:\n  /config                     list effective config\n  /config get <key>           show one config value\n  /config set <key> <value>   save to .env and apply\n\nkeys: api_base_url, api_key, model, fast_model, tavily_api_key, workspace, max_retries, code_exec_timeout".into(),
            )),
            _ => Ok(ToolResult::err(
                "usage: /config [list|get|set|help] ...".into(),
            )),
        }
    }

    fn shareable_messages(&self) -> Vec<Message> {
        let mut messages = self.messages.clone();
        if messages.last().is_some_and(|m| {
            m.role == Role::Assistant
                && m.tool_calls.as_ref().is_some_and(|calls| !calls.is_empty())
        }) {
            messages.pop();
        }
        messages
    }

    pub async fn set_model(&mut self, model: &str) -> Result<()> {
        self.config.model = model.to_string();
        self.llm.update_model(model);
        self.refresh_prompt_messages().await;
        Ok(())
    }

    pub async fn apply_config(&mut self, config: Config) -> Result<bool> {
        let workspace_changed = self.config.workspace_path != config.workspace_path;
        let (llm, sleep_llm, tools, memory, prompt_messages) = Self::build_runtime(&config).await?;
        self.subagents.abort_all().await;

        self.config = config;
        self.llm = llm;
        self.sleep_llm = sleep_llm;
        self.tools = tools;
        self.memory = memory;
        self.last_plan = None;
        self.history_summary = None;
        self.restored_session_messages = 0;

        if workspace_changed {
            self.messages.clear();
            self.messages.extend(prompt_messages);
            self.current_request = None;
            self.prompt_tokens = 0;
            self.completion_tokens = 0;
            self.request_count = 0;
            self.session_start = Instant::now();
        } else {
            self.replace_prefix_messages(prompt_messages);
        }

        Ok(workspace_changed)
    }

    pub fn get_models(&self) -> (String, String) {
        (self.config.model.clone(), self.config.fast_model.clone())
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub async fn take_channel_send_queue(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.channel_send_queue.lock().await)
    }

    pub(crate) fn track_usage(&mut self, response: &ChatResponse) {
        if let Some(usage) = &response.usage {
            self.prompt_tokens = self.prompt_tokens.saturating_add(usage.prompt_tokens);
            self.completion_tokens = self
                .completion_tokens
                .saturating_add(usage.completion_tokens);
        }
        self.last_request_start = Some(Instant::now());
    }

    fn fmt_tokens(n: u64) -> String {
        if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1_000.0)
        } else {
            n.to_string()
        }
    }

    fn fmt_num(n: u32) -> String {
        if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1_000.0)
        } else {
            n.to_string()
        }
    }

    fn format_usd(amount: f64) -> String {
        if amount >= 1_000.0 {
            format!("${:.0}", amount)
        } else if amount >= 1.0 {
            format!("${:.2}", amount)
        } else if amount > 0.0 {
            format!("${:.4}", amount)
        } else {
            "free".into()
        }
    }

    /// Compact one-line HUD with session tokens + account billing from API.
    pub fn usage_summary(&self) -> String {
        let secs = self.session_start.elapsed().as_secs();
        let time = if secs >= 3600 {
            format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
        } else {
            format!("{}m", secs / 60)
        };
        let mut parts: Vec<String> = vec![
            format!("↑{} ↓{}", Self::fmt_tokens(self.prompt_tokens), Self::fmt_tokens(self.completion_tokens)),
            format!("{} req", Self::fmt_num(self.request_count)),
            time,
        ];
        if let Some(b) = &self.billing {
            parts.insert(1, b.short.clone());
        }
        format!(
            "  {dim}── {} ──{reset}",
            parts.join("  ·  "),
            reset = R,
            dim = DIM,
        )
    }

    /// Detailed usage breakdown for the /usage slash command.
    pub fn usage_detail(&self) -> String {
        let total = self.prompt_tokens + self.completion_tokens;
        let secs = self.session_start.elapsed().as_secs();
        let time = if secs >= 3600 {
            format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
        } else {
            format!("{}m", secs / 60)
        };

        let mut out = format!(
            "Usage\n─────\n\n\
             · Session    {time}\n\
             · Requests   {n}\n\
             · Prompt     {pt} tokens\n\
             · Output     {ot} tokens\n\
             · Total      {total} tokens",
            time = time,
            n = self.request_count,
            pt = Self::fmt_tokens(self.prompt_tokens),
            ot = Self::fmt_tokens(self.completion_tokens),
            total = Self::fmt_tokens(total),
        );
        if let Some(b) = &self.billing {
            for line in &b.lines {
                out.push_str(&format!("\n· {}", line));
            }
            out.push_str(&format!(
                "\n\n{dim}Token data from chat completion API. Billing from provider API.{reset}",
                dim = DIM,
                reset = R,
            ));
        } else {
            out.push_str(&format!(
                "\n\n{dim}Billing: not available. Set ANTHROPIC_AUTH_TOKEN for GLM, or configure input_price/output_price for estimated cost.{reset}",
                dim = DIM,
                reset = R,
            ));
        }
        out
    }

    /// Fetch account billing/quota from the provider API.
    /// Defaults to GLM (智谱/ZHIPU) endpoints, falls back to OpenAI / OpenRouter.
    pub(crate) async fn fetch_billing(&self) -> Option<BillingInfo> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .ok()?;

        // ── GLM mode from env vars (ANTHROPIC_BASE_URL / ANTHROPIC_AUTH_TOKEN) ──
        let anthropic_base = std::env::var("ANTHROPIC_BASE_URL").ok();
        let anthropic_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok();
        if let (Some(ref base), Some(ref token)) = (&anthropic_base, &anthropic_token) {
            if !token.is_empty() && !base.is_empty() {
                tracing::info!("billing: detected GLM/ZHIPU from ANTHROPIC_BASE_URL");
                if let Some(info) = self.fetch_glm_billing(&client, base, token).await {
                    return Some(info);
                }
                tracing::warn!("billing: GLM fetch failed via ANTHROPIC_BASE_URL");
            }
        }
        // If only ANTHROPIC_AUTH_TOKEN is set (no base URL), try default GLM endpoints
        if let Some(ref token) = anthropic_token {
            if !token.is_empty() && anthropic_base.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
                for fallback in ["https://api.z.ai/api/anthropic", "https://open.bigmodel.cn/api/anthropic"] {
                    tracing::info!("billing: trying GLM fallback endpoint {}", fallback);
                    if let Some(info) = self.fetch_glm_billing(&client, fallback, token).await {
                        return Some(info);
                    }
                }
            }
        }

        // ── GLM mode from api_base_url + api_key ──
        let api_base = self.config.api_base_url.trim_end_matches('/');
        let api_key = &self.config.api_key;
        if !api_key.is_empty() && *api_key != "sk-placeholder" {
            let is_glm = api_base.contains("bigmodel.cn") || api_base.contains("z.ai") || api_base.contains("glm");
            if is_glm {
                tracing::info!("billing: detected GLM from api_base_url");
                if let Some(info) = self.fetch_glm_billing(&client, api_base, api_key).await {
                    return Some(info);
                }
                tracing::warn!("billing: GLM fetch failed via api_key");
            }
        }

        // ── OpenRouter via orkey or api_key ──
        for key in [&self.config.orkey, &self.config.api_key] {
            if key.is_empty() || *key == "sk-placeholder" {
                continue;
            }
            let resp = client
                .get("https://openrouter.ai/api/v1/auth/key")
                .header("Authorization", format!("Bearer {}", key))
                .send()
                .await;
            if let Ok(r) = resp {
                if r.status().is_success() {
                    if let Ok(body) = r.json::<serde_json::Value>().await {
                        if let Some(data) = body.get("data") {
                            let used = data["usage"].as_f64().unwrap_or(0.0);
                            let limit = data["limit"].as_f64().unwrap_or(0.0);
                            let short = if limit > 0.0 {
                                format!("${:.2}/{}", used, Self::format_usd(limit))
                            } else {
                                format!("${:.2}", used)
                            };
                            tracing::info!("billing: OpenRouter — spent=${:.2} limit=${:.0}", used, limit);
                            return Some(BillingInfo {
                                short,
                                lines: vec![
                                    format!("Spent: ${:.2}", used),
                                    if limit > 0.0 { format!("Limit: ${:.0}", limit) } else { "Limit: pay-as-you-go".into() },
                                ],
                            });
                        }
                    }
                }
            }
        }

        // ── OpenAI billing endpoints ──
        if !api_key.is_empty() && *api_key != "sk-placeholder" {
            let sub_url = format!("{}/dashboard/billing/subscription", api_base);
            if let Ok(r) = client.get(&sub_url).header("Authorization", format!("Bearer {}", api_key)).send().await {
                if r.status().is_success() {
                    if let Ok(body) = r.json::<serde_json::Value>().await {
                        let limit = body["hard_limit_usd"].as_f64().unwrap_or(0.0);
                        let now = chrono::Utc::now();
                        let start = format!("{}-01", now.format("%Y-%m"));
                        let end = now.format("%Y-%m-%d").to_string();
                        let usage_url = format!("{}/dashboard/billing/usage?start_date={}&end_date={}", api_base, start, end);
                        let used = if let Ok(ur) = client.get(&usage_url).header("Authorization", format!("Bearer {}", api_key)).send().await {
                            if ur.status().is_success() {
                                if let Ok(body) = ur.json::<serde_json::Value>().await {
                                    body["total_usage"].as_f64().unwrap_or(0.0) / 100.0
                                } else { 0.0 }
                            } else { 0.0 }
                        } else { 0.0 };
                        tracing::info!("billing: OpenAI — spent=${:.2} limit=${:.0}", used, limit);
                        let short = format!("${:.2}/{}", used, Self::format_usd(limit));
                        return Some(BillingInfo {
                            short,
                            lines: vec![
                                format!("Spent: ${:.2}", used),
                                format!("Limit: ${:.0}", limit),
                            ],
                        });
                    }
                }
            }
        }

        tracing::warn!("billing: no provider matched or all requests failed");
        None
    }

    /// GLM / ZHIPU billing: queries model-usage, tool-usage, and quota endpoints.
    async fn fetch_glm_billing(&self, client: &reqwest::Client, base_url: &str, token: &str) -> Option<BillingInfo> {
        // Extract domain: strip trailing path to get the root API domain
        let domain = if let Some(pos) = base_url.rfind("/api/") {
            &base_url[..pos]
        } else {
            base_url.trim_end_matches('/')
        };

        let now = chrono::Utc::now();
        let start = now - chrono::Duration::hours(24);
        let fmt = "%Y-%m-%d %H:%M:%S";
        let start_time = start.format(fmt).to_string();
        let end_time = now.format(fmt).to_string();

        let model_url = format!("{}/api/monitor/usage/model-usage", domain);
        let tool_url = format!("{}/api/monitor/usage/tool-usage", domain);
        let quota_url = format!("{}/api/monitor/usage/quota/limit", domain);

        // Use ANTHROPIC_AUTH_TOKEN as-is (may be Bearer token or raw key)
        let auth_header = token.to_string();

        // Parallel requests
        let (model_r, _tool_r, quota_r) = tokio::join!(
            client.get(&model_url).query(&[("startTime", &start_time), ("endTime", &end_time)]).header("Authorization", &auth_header).send(),
            client.get(&tool_url).query(&[("startTime", &start_time), ("endTime", &end_time)]).header("Authorization", &auth_header).send(),
            client.get(&quota_url).header("Authorization", &auth_header).send(),
        );

        let mut short = String::new();
        let mut lines: Vec<String> = Vec::new();

        // Parse quota limits (most important — shows token % and MCP usage %)
        if let Ok(r) = quota_r {
            if r.status().is_success() {
                if let Ok(body) = r.json::<serde_json::Value>().await {
                    let data = body.get("data").or(Some(&body)).cloned().unwrap_or(body);
                    if let Some(limits) = data.get("limits").and_then(|l| l.as_array()) {
                        for limit in limits {
                            let typ = limit["type"].as_str().unwrap_or("");
                            let pct = limit["percentage"].as_f64().unwrap_or(0.0);
                            if typ.contains("TOKENS") || typ.contains("Token") {
                                short = format!("{:.0}%", pct);
                                lines.push(format!("Token quota (5h): {:.1}%", pct));
                            } else if typ.contains("TIME") || typ.contains("MCP") {
                                let used = limit.get("currentUsage").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                let tot = limit.get("usage").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                lines.push(format!("MCP quota (1m): {:.1}%  ({:.0} / {:.0})", pct, used, tot));
                            }
                        }
                    }
                }
            }
        }

        // Parse model-usage for token detail
        if let Ok(r) = model_r {
            if r.status().is_success() {
                if let Ok(body) = r.json::<serde_json::Value>().await {
                    let data = body.get("data").or(Some(&body)).cloned().unwrap_or(body);
                    let pt = data.get("promptTokens").or_else(|| data.get("inputTokens"));
                    let ct = data.get("completionTokens").or_else(|| data.get("outputTokens"));
                    if let (Some(p), Some(c)) = (pt.and_then(|v| v.as_u64()), ct.and_then(|v| v.as_u64())) {
                        lines.push(format!("Model tokens: ↑{} ↓{}", Self::fmt_tokens(p), Self::fmt_tokens(c)));
                    }
                }
            }
        }

        if short.is_empty() {
            short = "quota".into();
        }
        if lines.is_empty() {
            lines.push("(no quota data)".into());
        }

        Some(BillingInfo { short, lines })
    }

    pub fn last_plan(&self) -> Option<&str> {
        self.last_plan.as_deref()
    }

    pub fn memory(&self) -> &MemorySearch {
        &self.memory
    }

    pub fn restored_session_messages(&self) -> usize {
        self.restored_session_messages
    }

    pub async fn clear_conversation(&mut self) -> Result<()> {
        let prompt_messages = Self::build_prompt_messages(&self.memory, &self.config).await?;
        self.messages = prompt_messages;
        self.current_request = None;
        self.history_summary = None;
        self.last_plan = None;
        self.iteration_count = 0;
        self.restored_session_messages = 0;
        let _ = clear_session_snapshot_file(&self.config.workspace_path);
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        let _ = self.persist_session_snapshot();
        self.subagents.abort_all().await;
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

        let kickoff = plan::plan_mode_kickoff_prompt();
        let first_user = self
            .current_request
            .clone()
            .or_else(|| {
                self.messages
                    .iter()
                    .find(|m| {
                        m.role == Role::User
                            && m.content.as_deref().is_some_and(|c| {
                                let trimmed = c.trim();
                                !trimmed.is_empty()
                                    && !is_internal_control_message(trimmed, &kickoff)
                            })
                    })
                    .and_then(|m| m.content.clone())
            })
            .unwrap_or_default();
        let last_assistant = self
            .messages
            .iter()
            .rev()
            .find(|m| {
                m.role == Role::Assistant
                    && m.content.as_deref().is_some_and(|c| {
                        let trimmed = c.trim();
                        !trimmed.is_empty() && !looks_like_internal_assistant_message(trimmed)
                    })
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
            .add_memory(MemoryLayer::Working, &summary, &body, &["session"])
            .await;
    }

    /// Dream consolidation: review working memories, merge related entries into episodic,
    /// evict stale trivia, touch due entries. Uses the sleep LLM (free model via OpenRouter
    /// if `orkey` is set, otherwise the configured fast model).
    async fn sleep_consolidate(&mut self) {
        // Collect all working and episodic entries for the dream prompt.
        let entries: Vec<_> = self
            .memory
            .scan_all_for_consolidation()
            .into_iter()
            .filter(|(_, layer, _)| matches!(layer, MemoryLayer::Working | MemoryLayer::Episodic))
            .collect();

        // Need at least 2 entries to bother merging.
        let working_count = entries
            .iter()
            .filter(|(_, l, _)| *l == MemoryLayer::Working)
            .count();
        if working_count < 2 {
            // Just run decay and return.
            let _ = self.memory.decay().await;
            return;
        }

        // Build a compact summary of entries for the dream prompt.
        let mut entries_text = String::new();
        for (path, layer, fm) in &entries {
            let raw = std::fs::read_to_string(path).unwrap_or_default();
            let body: String = raw
                .split("\n---\n")
                .nth(2)
                .unwrap_or("")
                .lines()
                .take(6)
                .collect::<Vec<_>>()
                .join("\n");
            entries_text.push_str(&format!(
                "- `{}` [{}] (s{}) tags=[{}]: {}\n  preview: {}\n",
                path.file_name().and_then(|f| f.to_str()).unwrap_or(""),
                layer.dir(),
                fm.strength.min(5),
                fm.tags.join(", "),
                fm.summary,
                truncate(&body, 120),
            ));
        }

        let prompt = crate::personality::sleep_consolidation_prompt(&entries_text);
        let msgs = vec![Message::user(&prompt)];

        let model = self.sleep_llm.model.clone();
        tracing::info!(
            "dream: consolidating {} memories ({} working) with {}",
            entries.len(),
            working_count,
            model,
        );

        // Call the sleep LLM.
        let response = match self.sleep_llm.chat_with_model(&model, &msgs).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("dream LLM call failed: {:#}", e);
                let _ = self.memory.decay().await;
                return;
            }
        };

        let text = response
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();

        // Parse the JSON plan from the response.
        let plan = match extract_json_object(&text) {
            Some(p) => p,
            None => {
                tracing::warn!("dream response had no JSON plan, skipping consolidation");
                let _ = self.memory.decay().await;
                return;
            }
        };

        let mut merged = 0usize;
        let mut evicted = 0usize;
        let mut touched = 0usize;

        // Execute merge_groups: create episodic entry from merged working entries.
        if let Some(groups) = plan.get("merge_groups").and_then(|g| g.as_array()) {
            for group in groups {
                let summary = group
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("merged memory");
                let content = group.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let tags: Vec<&str> = group
                    .get("tags")
                    .and_then(|t| t.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                    .unwrap_or_default();
                let tags_refs: Vec<&str> = tags.iter().map(|s| *s).collect();

                if !content.is_empty() {
                    if let Ok(rel) = self
                        .memory
                        .add_memory(MemoryLayer::Episodic, summary, content, &tags_refs)
                        .await
                    {
                        tracing::info!("dream merged → {}", rel);
                        merged += 1;
                    }
                }

                // Delete source files.
                if let Some(sources) = group.get("source_files").and_then(|s| s.as_array()) {
                    for src in sources {
                        if let Some(fname) = src.as_str() {
                            let _ = self.memory.delete_entry(fname).await;
                        }
                    }
                }
            }
        }

        // Execute evictions.
        if let Some(evict_list) = plan.get("evict").and_then(|e| e.as_array()) {
            for item in evict_list {
                if let Some(fname) = item.as_str() {
                    if self.memory.delete_entry(fname).await.unwrap_or(false) {
                        evicted += 1;
                    }
                }
            }
        }

        // Execute touches.
        if let Some(touch_list) = plan.get("touch").and_then(|t| t.as_array()) {
            for item in touch_list {
                if let Some(fname) = item.as_str() {
                    if self.memory.touch(fname).await.unwrap_or(false) {
                        touched += 1;
                    }
                }
            }
        }

        // Run decay for standard promotion/eviction.
        if let Ok(r) = self.memory.decay().await {
            if r.promoted + r.evicted + merged + evicted + touched > 0 {
                tracing::info!(
                    "dream: merged={} evicted={} touched={} decay_promoted={} decay_evicted={}",
                    merged,
                    evicted,
                    touched,
                    r.promoted,
                    r.evicted
                );
            }
        }
    }
}
