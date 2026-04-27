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
    format_subagent_snapshot, format_subagent_summary, is_internal_control_message,
    looks_like_internal_assistant_message, push_unique_limited, summarize_params, truncate,
    MAX_TOOL_RESULT_CHARS, MAX_TRACKED_TOOL_ROUNDS,
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
use std::time::Instant;
use tokio::sync::Mutex;

pub struct Agent {
    pub(crate) config: Config,
    pub(crate) llm: LlmClient,
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
    pub(crate) last_request_start: Option<Instant>,
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) request_count: u32,
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
        let (llm, tools, memory, prompt_messages) = Self::build_runtime(&config).await?;

        let agent = Self {
            config,
            llm,
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
            last_request_start: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            request_count: 0,
            channel_send_queue: Arc::new(Mutex::new(Vec::new())),
        };
        agent.restore_session()
    }

    pub async fn process(&mut self, input: &str) -> Result<String> {
        self.current_request = Some(input.trim().to_string());
        self.last_request_start = Some(Instant::now());
        self.messages.push(Message::user(input));
        self.iteration_count = 0;
        let result = if should_auto_plan_mode(input) {
            self.run_plan_mode(None).await
        } else {
            self.run_loop().await
        };
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
            _ => self.tools.execute(name, params).await,
        }
    }

    async fn subagent_spawn(&self, params: serde_json::Value) -> Result<ToolResult> {
        let task = params["task"].as_str().unwrap_or("").trim().to_string();
        if task.is_empty() {
            return Ok(ToolResult::err("missing task".into()));
        }
        let share_history = params["share_history"].as_bool().unwrap_or(false);
        let config = self.config.clone();
        let seed_messages = share_history.then(|| self.shareable_messages());
        let task_for_runner = task.clone();
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
                    let result = agent.process(&task_for_runner).await;
                    agent.shutdown().await;
                    result
                })
            })
            .await;
        Ok(ToolResult::ok(format!(
            "Spawned subagent `{}`.\n- task: {}\n- share_history: {}",
            id, task, share_history
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
        let (llm, tools, memory, prompt_messages) = Self::build_runtime(&config).await?;
        self.subagents.abort_all().await;

        self.config = config;
        self.llm = llm;
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
}
