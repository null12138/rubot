use anyhow::{Context, Result};
use tracing::warn;

use crate::config::{self, Config, ConfigKey};
use crate::llm::client::LlmClient;
use crate::llm::types::*;
use crate::markdown::{CYAN, DIM, GREEN, R, RED};
use crate::memory::{MemoryLayer, MemorySearch};
use crate::personality;
use crate::planner::{StepStatus, ToolCallChain};
use crate::subagent::{SubagentManager, SubagentSnapshot};
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
    subagents: SubagentManager,
    history_summary: Option<String>,
}

impl Agent {
    async fn build_prompt_messages(memory: &MemorySearch, config: &Config) -> Result<Vec<Message>> {
        let memory_index = memory
            .get_index_text()
            .await
            .unwrap_or_else(|_| "(empty)".into());

        Ok(vec![
            Message::system(&personality::base_system_prompt()),
            Message::system(&personality::session_context_prompt(
                &config.workspace_path,
                &config.model,
                &config.fast_model,
            )),
            Message::system(&personality::date_context_prompt()),
            Message::system(&personality::memory_snapshot_prompt(&compact_memory_index(
                &memory_index,
            ))),
        ])
    }

    async fn build_runtime(
        config: &Config,
    ) -> Result<(LlmClient, ToolRegistry, MemorySearch, Vec<Message>)> {
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
        let prompt_messages = Self::build_prompt_messages(&memory, config).await?;

        Ok((llm, tools, memory, prompt_messages))
    }

    pub async fn new(config: Config) -> Result<Self> {
        let (llm, tools, memory, prompt_messages) = Self::build_runtime(&config).await?;

        Ok(Self {
            config,
            llm,
            tools,
            memory,
            messages: prompt_messages,
            iteration_count: 0,
            max_iterations: 30,
            last_plan: None,
            subagents: SubagentManager::new(),
            history_summary: None,
        })
    }

    pub async fn process(&mut self, input: &str) -> Result<String> {
        self.messages.push(Message::user(input));
        self.iteration_count = 0;
        if should_auto_plan_mode(input) {
            return self.run_plan_mode(None).await;
        }
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
                    self.messages.push(Message::tool_result(
                        &tc.id,
                        &result.to_string_for_llm_limited(MAX_TOOL_RESULT_CHARS),
                    ));
                }
                continue;
            }

            let response_text = assistant_msg.content.unwrap_or_default();

            if let Some(plan) = extract_plan(&response_text) {
                return self.run_plan_mode(Some(plan)).await;
            }

            if !is_first_round && response_text.trim().len() < 200 {
                let prompt = "Based on the tool results above, provide a comprehensive answer to the user's original question.";
                self.messages.push(Message::user(prompt));
                self.compact_message_history();
                let resp = self.llm.chat(&self.llm_messages(), None, Some(0.7)).await?;
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

    async fn execute_tools(
        &mut self,
        tool_calls: &[ToolCall],
    ) -> Result<Vec<(ToolCall, ToolResult)>> {
        let mut results = Vec::with_capacity(tool_calls.len());
        for tc in tool_calls {
            let params: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::json!({}));
            let summary = summarize_params(&tc.function.name, &params);
            println!(
                "  {}→{} {}{}{} {}{}{}",
                DIM, R, CYAN, tc.function.name, R, DIM, summary, R
            );
            let result = match self.execute_tool_call(&tc.function.name, params).await {
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

    async fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs = self.tools.definitions().await;
        defs.extend(subagent_tool_definitions());
        defs.sort_by(|a, b| a.function.name.cmp(&b.function.name));
        defs
    }

    async fn execute_tool_call(
        &mut self,
        name: &str,
        params: serde_json::Value,
    ) -> Result<ToolResult> {
        match name {
            "rubot_command" => self.rubot_command(params).await,
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
                let display = if key == ConfigKey::ApiKey {
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
                "usage:\n  /config                     list effective config\n  /config get <key>           show one config value\n  /config set <key> <value>   save to .env and apply\n\nkeys: api_base_url, api_key, model, fast_model, workspace, max_retries, code_exec_timeout".into(),
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

    async fn run_plan_mode(&mut self, initial_plan: Option<ToolCallChain>) -> Result<String> {
        const MAX_PLAN_CYCLES: usize = 8;

        let mut pending_plan = initial_plan;
        let mut cycle = 0usize;

        if pending_plan.is_none() {
            self.messages
                .push(Message::user(&plan_mode_kickoff_prompt()));
        }

        loop {
            cycle += 1;
            if cycle > MAX_PLAN_CYCLES {
                return Ok(format!(
                    "Plan mode stopped after {} cycles without reaching `TASK COMPLETE`.",
                    MAX_PLAN_CYCLES
                ));
            }

            let plan = match pending_plan.take() {
                Some(plan) => plan,
                None => {
                    let response = self.plan_mode_chat(cycle == 1).await?;
                    let assistant_msg = response
                        .choices
                        .into_iter()
                        .next()
                        .context("No response from LLM")?
                        .message;
                    self.messages.push(assistant_msg.clone());
                    let response_text = assistant_msg.content.unwrap_or_default();

                    if let Some(done) = extract_task_complete(&response_text) {
                        return Ok(done);
                    }
                    if let Some(plan) = extract_plan(&response_text) {
                        plan
                    } else {
                        self.messages.push(Message::user(
                            "Plan mode requires one of two outputs: either a JSON plan block for the remaining work, or `TASK COMPLETE` followed by the final answer if the goal is fully complete. Try again.",
                        ));
                        continue;
                    }
                }
            };

            let summary = self.execute_plan_cycle(plan).await?;
            self.messages.push(Message::user(&format!(
                "Plan cycle {} complete.\n{}\nIf the goal is fully complete, reply with `TASK COMPLETE` followed by the final answer. Otherwise emit another JSON plan block for the remaining work only.",
                cycle, summary
            )));
        }
    }

    async fn plan_mode_chat(&mut self, first_cycle: bool) -> Result<ChatResponse> {
        self.compact_message_history();
        let messages = self.llm_messages();
        if first_cycle {
            self.llm.chat(&messages, None, Some(0.2)).await
        } else {
            self.llm.chat_fast(&messages, None, Some(0.2)).await
        }
    }

    async fn execute_plan_cycle(&mut self, mut chain: ToolCallChain) -> Result<String> {
        let plan_md = chain.to_md();
        println!("\n--- Plan ---\n{}\n--- End Plan ---\n", plan_md);
        self.last_plan = Some(plan_md);

        let mut outputs = vec![];
        while let Some(id) = chain.next_ready() {
            chain.steps[id].status = StepStatus::Running;
            let params = chain.resolve(&chain.steps[id].params.clone());
            let tool = chain.steps[id].tool.clone();
            let mut final_result = None;

            for _ in 0..=self.config.max_retries {
                let result = match self.execute_tool_call(&tool, params.clone()).await {
                    Ok(res) => res,
                    Err(err) => ToolResult::err(format!("{:#}", err)),
                };
                if result.success {
                    final_result = Some((true, result.output.clone()));
                    break;
                }
                final_result = Some((
                    false,
                    result
                        .error
                        .clone()
                        .unwrap_or_else(|| "Unknown error".into()),
                ));
            }

            let (ok, payload) = final_result.unwrap_or_else(|| (false, "Unknown error".into()));
            if ok {
                chain.steps[id].status = StepStatus::Done;
                chain.steps[id].result = Some(payload.clone());
                outputs.push((id, payload));
            } else {
                let err = format!("[FAILED] {}", payload);
                chain.steps[id].status = StepStatus::Failed;
                chain.steps[id].result = Some(err.clone());
                outputs.push((id, err));
            }
        }
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
        Ok(summary)
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

        if workspace_changed {
            self.messages.clear();
            self.messages.extend(prompt_messages);
        } else {
            self.replace_prefix_messages(prompt_messages);
        }

        Ok(workspace_changed)
    }

    pub fn get_models(&self) -> (String, String) {
        (self.config.model.clone(), self.config.fast_model.clone())
    }

    async fn refresh_prompt_messages(&mut self) {
        if let Ok(prompt_messages) = Self::build_prompt_messages(&self.memory, &self.config).await {
            self.replace_prefix_messages(prompt_messages);
        }
    }

    fn prefix_message_count(&self) -> usize {
        self.messages
            .iter()
            .take_while(|message| message.role == Role::System)
            .count()
    }

    fn replace_prefix_messages(&mut self, prefix_messages: Vec<Message>) {
        let prefix_count = self.prefix_message_count();
        self.messages.splice(0..prefix_count, prefix_messages);
    }

    fn compact_message_history(&mut self) {
        let prefix_count = self.prefix_message_count();
        if self.messages.len() <= prefix_count {
            return;
        }

        let over_messages = self.messages.len() > MAX_HISTORY_MESSAGES;
        let over_chars = total_message_chars(&self.messages) > MAX_HISTORY_CHARS;
        if !over_messages && !over_chars {
            return;
        }

        let keep_from = self
            .messages
            .len()
            .saturating_sub(KEEP_RECENT_MESSAGES)
            .max(prefix_count);
        if keep_from <= prefix_count {
            return;
        }

        let dropped: Vec<Message> = self.messages[prefix_count..keep_from].to_vec();
        let recent: Vec<Message> = self.messages[keep_from..].to_vec();
        let dropped_summary = summarize_messages(&dropped);
        self.history_summary = merge_history_summary(self.history_summary.take(), dropped_summary);

        self.messages.truncate(prefix_count);
        self.messages.extend(recent);
    }

    fn llm_messages(&self) -> Vec<Message> {
        let mut out = Vec::with_capacity(self.messages.len() + 1);
        let prefix_count = self.prefix_message_count();
        out.extend(self.messages.iter().take(prefix_count).cloned());
        if let Some(summary) = &self.history_summary {
            out.push(Message::user(summary));
        }
        if self.messages.len() > prefix_count {
            out.extend(self.messages[prefix_count..].iter().cloned());
        }
        out
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

const MAX_TOOL_RESULT_CHARS: usize = 2_400;
const MAX_MEMORY_INDEX_CHARS: usize = 3_200;
const MAX_HISTORY_MESSAGES: usize = 28;
const KEEP_RECENT_MESSAGES: usize = 12;
const MAX_HISTORY_CHARS: usize = 18_000;
const MAX_HISTORY_SUMMARY_CHARS: usize = 3_000;

fn compact_memory_index(memory_index: &str) -> String {
    if memory_index.chars().count() <= MAX_MEMORY_INDEX_CHARS {
        return memory_index.to_string();
    }
    format!(
        "{}\n\n...(memory index truncated for token efficiency)...",
        truncate(memory_index, MAX_MEMORY_INDEX_CHARS)
    )
}

fn total_message_chars(messages: &[Message]) -> usize {
    messages.iter().map(message_char_len).sum()
}

fn message_char_len(message: &Message) -> usize {
    let content_len = message.content.as_deref().map_or(0, |c| c.chars().count());
    let tool_call_len = message
        .tool_calls
        .as_ref()
        .map(|calls| {
            calls
                .iter()
                .map(|call| {
                    call.function.name.chars().count() + call.function.arguments.chars().count()
                })
                .sum::<usize>()
        })
        .unwrap_or(0);
    content_len + tool_call_len
}

fn summarize_messages(messages: &[Message]) -> String {
    let mut out = String::from("Earlier conversation summary:\n");
    for message in messages.iter().rev().take(16).rev() {
        if out.chars().count() >= MAX_HISTORY_SUMMARY_CHARS {
            break;
        }
        let line = summarize_message(message);
        if line.is_empty() {
            continue;
        }
        out.push_str("- ");
        out.push_str(&line);
        out.push('\n');
    }
    truncate(&out, MAX_HISTORY_SUMMARY_CHARS)
}

fn summarize_message(message: &Message) -> String {
    match message.role {
        Role::System => String::new(),
        Role::User => format!(
            "user: {}",
            truncate(message.content.as_deref().unwrap_or("").trim(), 180)
        ),
        Role::Assistant => {
            if let Some(calls) = &message.tool_calls {
                let names = calls
                    .iter()
                    .map(|call| call.function.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("assistant called tools: {}", truncate(&names, 180))
            } else {
                format!(
                    "assistant: {}",
                    truncate(message.content.as_deref().unwrap_or("").trim(), 180)
                )
            }
        }
        Role::Tool => format!(
            "tool result: {}",
            truncate(message.content.as_deref().unwrap_or("").trim(), 180)
        ),
    }
}

fn merge_history_summary(existing: Option<String>, new_summary: String) -> Option<String> {
    if new_summary.trim().is_empty() {
        return existing;
    }
    let merged = match existing {
        Some(prev) if !prev.trim().is_empty() => {
            format!("{}\n{}\n", prev.trim_end(), new_summary.trim())
        }
        _ => new_summary,
    };
    Some(truncate(&merged, MAX_HISTORY_SUMMARY_CHARS))
}

fn should_auto_plan_mode(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_lowercase();
    if trimmed.lines().count() >= 3 {
        return true;
    }

    let keyword_hits = [
        "step by step",
        "multi-step",
        "multiple steps",
        "first",
        "then",
        "after that",
        "同时",
        "并且",
        "然后",
        "接着",
        "最后",
        "optimize",
        "refactor",
        "debug",
        "investigate",
        "analyze",
        "analyse",
        "implement",
        "build",
        "design",
        "migrate",
        "integrate",
        "audit",
        "review",
        "improve",
        "research",
        "项目",
        "优化",
        "重构",
        "排查",
        "分析",
        "实现",
        "构建",
        "迁移",
        "集成",
        "审计",
        "修复",
        "改造",
    ]
    .iter()
    .filter(|kw| lower.contains(**kw))
    .count();

    let connector_hits = [
        " and ", " then ", " also ", " plus ", "并且", "然后", "接着", "同时",
    ]
    .iter()
    .filter(|kw| lower.contains(**kw))
    .count();

    keyword_hits >= 2
        || connector_hits >= 2
        || (keyword_hits >= 1 && (connector_hits >= 1 || trimmed.len() >= 80))
}

fn plan_mode_kickoff_prompt() -> String {
    "The latest user request appears complex and should start in plan mode. Do not answer normally yet. Return exactly one of the following:\n1. A JSON plan block for the task using the available tools.\n2. `TASK COMPLETE` followed by the final answer if the goal is already complete.\nIf you return a plan, make it only for the next concrete tranche of work.".into()
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

fn extract_task_complete(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let upper = trimmed.to_uppercase();
    if !upper.starts_with("TASK COMPLETE") {
        return None;
    }
    let rest = trimmed["TASK COMPLETE".len()..]
        .trim_start_matches(':')
        .trim();
    Some(if rest.is_empty() {
        "TASK COMPLETE".into()
    } else {
        rest.into()
    })
}

fn summarize_params(tool_name: &str, params: &serde_json::Value) -> String {
    match tool_name {
        "rubot_command" => params["command"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect(),
        "web_fetch" => params["url"].as_str().unwrap_or("").to_string(),
        "web_search" => params["query"].as_str().unwrap_or("").to_string(),
        "subagent_spawn" => params["task"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect(),
        "subagent_wait" | "subagent_close" => params["id"].as_str().unwrap_or("").to_string(),
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
            let action = params["act"]
                .as_str()
                .or_else(|| params["action"].as_str())
                .unwrap_or("?");
            let path = params["path"].as_str().unwrap_or("");
            format!("{} {}", action, path)
        }
        _ => params.to_string().chars().take(50).collect(),
    }
}

fn subagent_tool_definitions() -> Vec<ToolDefinition> {
    static DEFINITIONS: std::sync::OnceLock<Vec<ToolDefinition>> = std::sync::OnceLock::new();
    DEFINITIONS
        .get_or_init(|| {
            vec![
                ToolDefinition::new(
                    "rubot_command",
                    "Run supported Rubot CLI commands: `/model`, `/config`, `/config get`, `/config set`, `/config help`.",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "command": {"type": "string", "description": "Rubot CLI command"}
                        },
                        "required": ["command"]
                    }),
                )
                .compact_for_llm(),
                ToolDefinition::new(
                    "subagent_spawn",
                    "Spawn a background child agent for an independent task.",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "task": {"type": "string", "description": "Child task"},
                            "share_history": {"type": "boolean", "description": "Copy current conversation history", "default": false}
                        },
                        "required": ["task"]
                    }),
                )
                .compact_for_llm(),
                ToolDefinition::new(
                    "subagent_wait",
                    "Wait for a child agent and return its result.",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Subagent id"},
                            "timeout_secs": {"type": "integer", "minimum": 1, "description": "Optional timeout in seconds"}
                        },
                        "required": ["id"]
                    }),
                )
                .compact_for_llm(),
                ToolDefinition::new(
                    "subagent_list",
                    "List child agents and their status.",
                    serde_json::json!({
                        "type": "object",
                        "properties": {},
                        "required": []
                    }),
                )
                .compact_for_llm(),
                ToolDefinition::new(
                    "subagent_close",
                    "Abort a child agent.",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Subagent id"}
                        },
                        "required": ["id"]
                    }),
                )
                .compact_for_llm(),
            ]
        })
        .clone()
}

fn format_subagent_summary(snapshot: &SubagentSnapshot) -> String {
    format!(
        "- {} [{}] share_history={} task={}",
        snapshot.id,
        snapshot.status.as_str(),
        snapshot.share_history,
        snapshot.task
    )
}

fn format_subagent_snapshot(snapshot: &SubagentSnapshot) -> String {
    let mut out = format!(
        "Subagent `{}` [{}]\n- task: {}\n- share_history: {}",
        snapshot.id,
        snapshot.status.as_str(),
        snapshot.task,
        snapshot.share_history
    );
    if let Some(result) = &snapshot.result {
        out.push_str("\n- result:\n");
        out.push_str(result);
    }
    if let Some(error) = &snapshot.error {
        out.push_str("\n- error: ");
        out.push_str(error);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        compact_memory_index, extract_task_complete, should_auto_plan_mode, summarize_messages,
    };
    use crate::llm::types::{Message, Role};

    #[test]
    fn complex_requests_enter_auto_plan_mode() {
        assert!(should_auto_plan_mode(
            "分析这个项目，找出性能瓶颈，然后实现修复并补测试"
        ));
        assert!(should_auto_plan_mode(
            "Investigate the bug, refactor the failing path, and add regression coverage."
        ));
    }

    #[test]
    fn simple_requests_skip_auto_plan_mode() {
        assert!(!should_auto_plan_mode("现在几点"));
        assert!(!should_auto_plan_mode("解释一下这个函数"));
    }

    #[test]
    fn task_complete_prefix_is_stripped() {
        assert_eq!(
            extract_task_complete("TASK COMPLETE\nAll done."),
            Some("All done.".into())
        );
        assert_eq!(
            extract_task_complete("TASK COMPLETE: Finished"),
            Some("Finished".into())
        );
        assert_eq!(extract_task_complete("Not done"), None);
    }

    #[test]
    fn memory_index_is_compacted() {
        let raw = format!("# Memory Index\n\n{}", "x".repeat(5000));
        let compacted = compact_memory_index(&raw);
        assert!(compacted.contains("truncated"));
        assert!(compacted.chars().count() < raw.chars().count());
    }

    #[test]
    fn older_messages_can_be_summarized() {
        let summary = summarize_messages(&[
            Message::new(Role::User, "First request with a lot of detail"),
            Message::new(Role::Assistant, "Initial response"),
            Message::tool("call_1", "Long tool result payload"),
        ]);
        assert!(summary.contains("Earlier conversation summary"));
        assert!(summary.contains("First request"));
        assert!(summary.contains("tool result"));
    }
}
