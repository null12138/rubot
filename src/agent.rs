use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::Config;
use crate::context::cleaner::ContextCleaner;
use crate::llm::client::LlmClient;
use crate::llm::types::*;
use crate::memory::{MemoryLayer, MemorySearch};
use crate::personality;
use crate::planner::{ChainExecutor, StepStatus, ToolCallChain};
use crate::reflector::error_book::ErrorBook;
use crate::state::manager::StateManager;
use crate::tools::registry::ToolRegistry;
use crate::tools::{
    code_exec::CodeExec, file_ops::FileOps, tool_create::ToolCreate, tool_list::ToolList,
    user_tool_types, web_fetch::WebFetch, web_search::WebSearch,
};

use crate::workspace::git::WorkspaceGit;

pub struct Agent {
    config: Config,
    llm: LlmClient,
    tools: ToolRegistry,
    memory: MemorySearch,
    error_book: ErrorBook,
    state: StateManager,
    git: WorkspaceGit,
    context_cleaner: ContextCleaner,
    messages: Vec<Message>,
    iteration_count: u32,
    max_iterations: u32,
}

impl Agent {
    pub async fn new(config: Config) -> Result<Self> {
        // ... init git ...
        let git = WorkspaceGit::new(&config.workspace_path);
        let _ = git.init();

        // Initialize LLM client with both heavy and fast models
        let llm = LlmClient::new(
            &config.api_base_url,
            &config.api_key,
            &config.model,
            &config.fast_model,
            config.max_retries,
        );

        // Initialize tools
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(WebSearch));
        tools.register(Box::new(WebFetch));
        tools.register(Box::new(CodeExec::new(config.code_exec_timeout_secs, &config.workspace_path)));
        tools.register(Box::new(FileOps::new(&config.workspace_path)));
        tools.register(Box::new(ToolCreate::new(&config.workspace_path)));
        tools.register(Box::new(ToolList::new(&config.workspace_path)));

        // Load user-created tools from manifest
        let user_tools = user_tool_types::list_tools_in_manifest(&config.workspace_path);
        for entry in user_tools {
            tools.register(Box::new(user_tool_types::UserTool::new(
                entry,
                &config.workspace_path,
                config.code_exec_timeout_secs,
            )));
        }

        // Initialize memory
        let memory_path = config.workspace_path.join("memory");
        let memory = MemorySearch::new(&memory_path);

        // Initialize error book
        let error_book = ErrorBook::load(&config.workspace_path).await?;

        // Initialize state manager
        let state = StateManager::new(&config.workspace_path);

        // Context cleaner
        let context_cleaner = ContextCleaner::new(config.max_context_tokens);

        // Build initial system prompt
        let memory_index = memory
            .get_index_text()
            .await
            .unwrap_or_else(|_| "(empty)".to_string());
        let error_summary = error_book.to_text();
        let user_tool_list = format_user_tool_list(&tools);
        let system_msg =
            Message::system(&personality::system_prompt(&memory_index, &error_summary, &user_tool_list));

        // Check for unfinished plan
        if state.has_unfinished_plan().await {
            info!("Found unfinished plan from previous session");
        }

        Ok(Self {
            config,
            llm,
            tools,
            memory,
            error_book,
            state,
            git,
            context_cleaner,
            messages: vec![system_msg],
            iteration_count: 0,
            max_iterations: 20,
        })
    }

    /// Process a user message through the Think → Plan → Act → Reflect loop
    /// with streaming output for the final text response.
    pub async fn process(&mut self, input: &str) -> Result<String> {
        self.messages.push(Message::user(input));
        self.iteration_count = 0;

        self.run_loop().await
    }

    /// Same as process(), but streams the final text response via callbacks.
    pub async fn process_stream<F>(&mut self, input: &str, mut on_token: F) -> Result<String>
    where
        F: FnMut(&str),
    {
        self.messages.push(Message::user(input));
        self.iteration_count = 0;

        self.run_loop_stream(&mut on_token).await
    }

    /// Process a user message with additional file/image context attached.
    /// Used by Telegram when the user sends attachments alongside text.
    pub async fn process_with_context(&mut self, input: &str, context: &str) -> Result<String> {
        let full_input = if context.is_empty() {
            input.to_string()
        } else {
            format!("{}\n\n[Attached context]:\n{}", input, context)
        };
        self.process(&full_input).await
    }

    /// Core agent loop — called by both process() and process_with_context().
    async fn run_loop(&mut self) -> Result<String> {
        let mut fingerprint_history = std::collections::HashSet::new();
        loop {
            self.iteration_count += 1;
            if self.iteration_count > self.max_iterations {
                let msg =
                    "Reached maximum iterations. Stopping to prevent infinite loop.".to_string();
                warn!("{}", msg);
                return Ok(msg);
            }

            // === THINK: Prune context if needed ===
            if self.context_cleaner.needs(&self.messages) {
                info!("Context pruning triggered");
                self.prune_context().await?;
            }

            // === PLAN: Ask LLM ===
            let tool_defs = self.tools.definitions();
            let is_first_round = self.iteration_count == 1;

            let (response, model_hint) = if is_first_round {
                info!("Using heavy model for initial reasoning");
                (
                    self.llm
                        .chat(&self.messages, Some(&tool_defs), Some(0.7))
                        .await
                        .context("LLM call failed")?,
                    "heavy",
                )
            } else {
                info!("Using fast model for tool-calling round");
                (
                    self.llm
                        .chat_fast(&self.messages, Some(&tool_defs), Some(0.3))
                        .await
                        .context("LLM call failed")?,
                    "fast",
                )
            };

            crate::ui::llm_round(self.iteration_count, model_hint);

            let choice = response
                .choices
                .into_iter()
                .next()
                .context("No response from LLM")?;

            let assistant_msg = choice.message;
            self.messages.push(assistant_msg.clone());

            // === ACT: Handle tool calls or direct response ===
            if let Some(tool_calls) = assistant_msg.tool_calls {
                if tool_calls.is_empty() {
                    return Ok(assistant_msg.content.unwrap_or_default());
                }

                // 指纹检测逻辑
                let mut filtered_calls = Vec::new();
                let mut loop_errors = Vec::new();

                for call in tool_calls {
                    let finger = format!("{}:{}", call.function.name, call.function.arguments);
                    if fingerprint_history.contains(&finger) {
                        warn!("Duplicate tool call detected: {}", call.function.name);
                        loop_errors.push(Message::tool(&call.id, "ERROR: Repeated call detected. You already tried this exact action and it failed or was insufficient. STOP repeating. Use a DIFFERENT approach (e.g., search a different keyword or use web_search if a custom tool failed)."));
                    } else {
                        fingerprint_history.insert(finger);
                        filtered_calls.push(call);
                    }
                }

                // 如果全部都是重复调用，且没有新行动，则强制报错注入上下文
                if filtered_calls.is_empty() {
                    self.messages.extend(loop_errors);
                    continue;
                }

                // Execute remaining unique tool calls in parallel
                let results = self.execute_tools_parallel(&filtered_calls).await?;
                self.messages.extend(loop_errors); // 先加入错误提示词

                // Add all tool results to conversation
                for (tc, result) in results {
                    // Log to state (truncate to 100 chars safely)
                    let output_for_log = result.to_string_for_llm();
                    let truncated_log = if output_for_log.chars().count() > 100 {
                        output_for_log.chars().take(100).collect::<String>()
                    } else {
                        output_for_log
                    };

                    let _ = self
                        .state
                        .log_action(
                            &tc.function.name,
                            &truncated_log,
                            result.success,
                        )
                        .await;

                    // If error, check error book
                    if !result.success {
                        if let Some(ref err) = result.error {
                            if !self.error_book.is_known_error(err) {
                                self.error_book
                                    .record_error(&tc.function.name, err)
                                    .await?;
                            }
                        }
                    }

                    // Dynamic tool registration after tool_create succeeds
                    if result.success && tc.function.name == "tool_create" {
                        if let Some(tool_name) = extract_created_tool_name(&result.output) {
                            if let Some(entry) =
                                user_tool_types::find_tool_in_manifest(&self.config.workspace_path, &tool_name)
                            {
                                self.tools.register(Box::new(user_tool_types::UserTool::new(
                                    entry,
                                    &self.config.workspace_path,
                                    self.config.code_exec_timeout_secs,
                                )));
                                info!("Dynamically registered user tool: {}", tool_name);
                            }
                        }
                    }

                    self.messages
                        .push(Message::tool_result(&tc.id, &result.to_string_for_llm()));

                    // Auto-commit workspace if tool succeeded
                    if result.success {
                        let _ = self.git.commit(&format!("Tool run: {}", tc.function.name));
                    }
                }

                // Continue the loop — LLM will see the tool results and decide what to do next
                continue;
            }

            // === No tool calls — direct response ===
            let response_text = assistant_msg.content.unwrap_or_default();

            // Check if response contains a plan JSON
            if let Some(plan) = self.extract_plan(&response_text) {
                return self.handle_plan(plan).await;
            }

            // If the fast model produced the final answer, optionally re-synthesize with heavy model
            // for better quality. But only if the answer seems brief/low-quality.
            if !is_first_round && response_text.len() < 200 {
                info!("Re-synthesizing brief answer with heavy model");
                let synthesis_prompt = format!(
                    "Based on the tool results above, provide a comprehensive answer to the user's original question. \
                     The current draft answer is: \"{}\". Improve it with more detail if the tool results contain useful information.",
                    response_text
                );
                self.messages.push(Message::user(&synthesis_prompt));

                let final_response = self
                    .llm
                    .chat(&self.messages, None, Some(0.7))
                    .await?;
                let final_text = final_response
                    .choices
                    .into_iter()
                    .next()
                    .map(|c| c.message.content.unwrap_or_default())
                    .unwrap_or(response_text.clone());

                // Remove the synthesis prompt we added
                self.messages.pop();

                return Ok(final_text);
            }

            // === REFLECT: Save important info to memory if needed ===
            if self.iteration_count > 2 {
                let _ = self.save_session_memory().await;
            }

            return Ok(response_text);
        }
    }

    /// Streaming variant of run_loop: streams the final text answer token by token.
    async fn run_loop_stream<F>(&mut self, on_token: &mut F) -> Result<String>
    where
        F: FnMut(&str),
    {
        let mut fingerprint_history = std::collections::HashSet::new();
        loop {
            self.iteration_count += 1;
            if self.iteration_count > self.max_iterations {
                return Ok("Reached maximum iterations.".into());
            }

            if self.context_cleaner.needs(&self.messages) {
                self.prune_context().await?;
            }

            let tool_defs = self.tools.definitions();
            let is_first_round = self.iteration_count == 1;

            crate::ui::llm_round(self.iteration_count, if is_first_round { "heavy" } else { "fast" });

            let (response, _model_hint) = if is_first_round {
                (
                    self.llm.chat(&self.messages, Some(&tool_defs), Some(0.7)).await.context("LLM call failed")?,
                    "heavy",
                )
            } else {
                (
                    self.llm.chat_fast(&self.messages, Some(&tool_defs), Some(0.3)).await.context("LLM call failed")?,
                    "fast",
                )
            };

            let choice = response.choices.into_iter().next().context("No response from LLM")?;
            let assistant_msg = choice.message;
            self.messages.push(assistant_msg.clone());

            // === ACT ===
            if let Some(tool_calls) = assistant_msg.tool_calls {
                if tool_calls.is_empty() {
                    return Ok(assistant_msg.content.unwrap_or_default());
                }

                let mut filtered_calls = Vec::new();
                let mut loop_errors = Vec::new();
                for call in tool_calls {
                    let finger = format!("{}:{}", call.function.name, call.function.arguments);
                    if fingerprint_history.contains(&finger) {
                        loop_errors.push(Message::tool(&call.id, "ERROR: Repeated call detected. Use a DIFFERENT approach."));
                    } else {
                        fingerprint_history.insert(finger);
                        filtered_calls.push(call);
                    }
                }
                if filtered_calls.is_empty() {
                    self.messages.extend(loop_errors);
                    continue;
                }

                let results = self.execute_tools_parallel(&filtered_calls).await?;
                self.messages.extend(loop_errors);
                for (tc, result) in results {
                    let output_for_log = result.to_string_for_llm();
                    let truncated_log: String = output_for_log.chars().take(100).collect();
                    let _ = self.state.log_action(&tc.function.name, &truncated_log, result.success).await;
                    if !result.success {
                        if let Some(ref err) = result.error {
                            if !self.error_book.is_known_error(err) {
                                self.error_book.record_error(&tc.function.name, err).await?;
                            }
                        }
                    }
                    if result.success && tc.function.name == "tool_create" {
                        if let Some(tool_name) = extract_created_tool_name(&result.output) {
                            if let Some(entry) = user_tool_types::find_tool_in_manifest(&self.config.workspace_path, &tool_name) {
                                self.tools.register(Box::new(user_tool_types::UserTool::new(entry, &self.config.workspace_path, self.config.code_exec_timeout_secs)));
                            }
                        }
                    }
                    self.messages.push(Message::tool_result(&tc.id, &result.to_string_for_llm()));
                    if result.success {
                        let _ = self.git.commit(&format!("Tool run: {}", tc.function.name));
                    }
                }
                continue;
            }

            // === Final text response: stream it ===
            let response_text = assistant_msg.content.unwrap_or_default();

            if let Some(plan) = self.extract_plan(&response_text) {
                return self.handle_plan(plan).await;
            }

            // Re-synthesize with heavy model if brief, streaming the output
            if !is_first_round && response_text.len() < 200 {
                let synthesis_prompt = format!(
                    "Based on the tool results above, provide a comprehensive answer to the user's original question. \
                     The current draft answer is: \"{}\". Improve it with more detail if the tool results contain useful information.",
                    response_text
                );
                self.messages.push(Message::user(&synthesis_prompt));
                println!(); // blank line before stream
                let final_text = self.llm.chat_stream(&self.messages, None, Some(0.7), |token| on_token(token)).await?;
                crate::ui::stream_end();
                self.messages.pop();
                return Ok(final_text);
            }

            // Stream the already-obtained text (print it token-by-token for visual effect)
            println!();
            for ch in response_text.chars() {
                on_token(&ch.to_string());
            }
            crate::ui::stream_end();

            if self.iteration_count > 2 {
                let _ = self.save_session_memory().await;
            }
            return Ok(response_text);
        }
    }

    /// Execute multiple tool calls sequentially, with UI output.
    async fn execute_tools_parallel(
        &self,
        tool_calls: &[ToolCall],
    ) -> Result<Vec<(ToolCall, crate::tools::registry::ToolResult)>> {
        let mut results = Vec::with_capacity(tool_calls.len());
        for tc in tool_calls {
            info!("Tool call: {} ({})", tc.function.name, tc.id);
            let params: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::json!({}));

            // Show tool call start
            let params_summary = Self::summarize_params(&tc.function.name, &params);
            crate::ui::tool_call_start(&tc.function.name, &params_summary);

            let result = self.tools.execute(&tc.function.name, params).await?;

            // Show tool call result
            let result_summary = if result.success {
                let output = &result.output;
                let first_line = output.lines().next().unwrap_or("");
                let truncated: String = first_line.chars().take(80).collect();
                if output.len() > 80 { format!("{}…", truncated) } else { truncated.to_string() }
            } else {
                result.error.as_deref().unwrap_or("unknown error").to_string()
            };
            crate::ui::tool_call_result(result.success, &result_summary);

            results.push((tc.clone(), result));
        }
        Ok(results)
    }

    /// Create a short human-readable summary of tool parameters.
    fn summarize_params(tool_name: &str, params: &serde_json::Value) -> String {
        match tool_name {
            "web_fetch" => params["url"].as_str().unwrap_or("").to_string(),
            "web_search" => params["query"].as_str().unwrap_or("").to_string(),
            "code_exec" => {
                let lang = params["language"].as_str().unwrap_or("bash");
                let code = params["code"].as_str().unwrap_or("");
                let first_line = code.lines().next().unwrap_or("");
                let truncated: String = first_line.chars().take(40).collect();
                format!("[{}] {}", lang, truncated)
            }
            "file_ops" => {
                let action = params["action"].as_str().unwrap_or("?");
                let path = params["path"].as_str().unwrap_or("");
                format!("{} {}", action, path)
            }
            "tool_create" => {
                let name = params["name"].as_str().unwrap_or("?");
                let tt = params["tool_type"].as_str().unwrap_or("?");
                format!("create [{}] {}", tt, name)
            }
            "tool_list" => {
                let filter = params["filter"].as_str().unwrap_or("all");
                format!("list tools ({})", filter)
            }
            _ => {
                let s = params.to_string();
                let truncated: String = s.chars().take(50).collect();
                truncated
            }
        }
    }

    /// Extract a plan JSON from the response text
    fn extract_plan(&self, text: &str) -> Option<ToolCallChain> {
        // Look for JSON blocks
        let json_start = text.find("```json")?;
        let json_content = &text[json_start + 7..];
        let json_end = json_content.find("```")?;
        let json_str = &json_content[..json_end].trim();

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
                step.get("description").and_then(|d| d.as_str()).unwrap_or(&format!("Step {}", i)),
                step.get("depends_on")
                    .and_then(|d| d.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_u64().map(|n| n as usize)).collect())
                    .unwrap_or_default(),
            );
        }

        Some(chain)
    }

    /// Handle a multi-step plan: show to user, execute, reflect
    async fn handle_plan(&mut self, mut chain: ToolCallChain) -> Result<String> {
        // Show plan to user
        let plan_md = chain.to_md();
        println!("\n--- Plan ---\n{}\n--- End Plan ---\n", plan_md);
        println!("(Executing plan...)\n");

        // Save plan to state
        self.state.save_plan(&plan_md).await?;

        // Execute the chain
        let mut executor =
            ChainExecutor::new(&self.tools, &mut self.error_book, self.config.max_retries);
        let outputs = executor.execute(&mut chain).await?;

        // Update plan state
        self.state.save_plan(&chain.to_md()).await?;

        // Build results summary
        let mut summary = format!("## Plan Results: {}\n\n", chain.goal);
        for (step_id, output) in &outputs {
            let step = &chain.steps[*step_id];
            let status = match step.status {
                StepStatus::Done => "OK",
                StepStatus::Failed => "FAILED",
                _ => "???",
            };
            summary.push_str(&format!(
                "**Step {} [{}]**: {}\n> {}\n\n",
                step_id,
                status,
                step.desc,

                if output.chars().count() > 200 {
                    let truncated: String = output.chars().take(200).collect();
                    format!("{}...", truncated)
                } else {
                    output.clone()
                }
            ));
        }

        if chain.has_failure() {
            summary.push_str("\nSome steps failed. Check the error book with /errors.\n");
        }

        // Feed results back to LLM for final synthesis (heavy model)
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

        // Clear plan
        self.state.clear_plan().await?;

        Ok(final_text)
    }

    /// Prune context by summarizing older messages
    pub async fn prune_context(&mut self) -> Result<()> {
        let total = self.messages.len();
        if total <= 6 {
            return Ok(());
        }

        // Messages to summarize: everything except system (0) and last 6
        let start = 1;
        let end = total - 6;
        if end <= start {
            return Ok(());
        }

        let to_summarize = &self.messages[start..end];

        // Ask LLM to summarize
        let summary_prompt = ContextCleaner::prompt(to_summarize);
        let summary_msgs = vec![
            Message::system("You are a summarizer. Be concise."),
            Message::user(&summary_prompt),
        ];

        let summary = match self.llm.chat_fast(&summary_msgs, None, Some(0.3)).await {
            Ok(resp) => resp
                .choices
                .into_iter()
                .next()
                .and_then(|c| c.message.content)
                .unwrap_or_else(|| "(summary unavailable)".to_string()),
            Err(_) => "(summary unavailable)".to_string(),
        };

        self.messages = self.context_cleaner.prune(&self.messages, &summary);
        info!("Context pruned: {} → {} messages", total, self.messages.len());

        Ok(())
    }

    /// Save a session memory with key points from this conversation
    async fn save_session_memory(&mut self) -> Result<()> {
        let recent: Vec<String> = self.messages
            .iter()
            .filter(|m| m.role == Role::User || m.role == Role::Assistant)
            .filter_map(|m| m.content.as_ref())
            .rev()
            .take(4)
            .map(|s| {
                if s.chars().count() > 100 {
                    format!("{}...", s.chars().take(100).collect::<String>())
                } else {
                    s.clone()
                }
            })
            .collect();

        if recent.is_empty() {
            return Ok(());
        }

        let summary = format!("Interaction with {} tool calls", self.iteration_count);
        let content = recent.join("\n---\n");

        let _ = self.memory
            .add_memory(MemoryLayer::Working, &summary, &content, &[])
            .await;


        let _ = self.git.commit("Save session memory");

        Ok(())
    }

    /// Set the heavy model.
    pub fn set_model(&mut self, model: &str) {
        self.config.model = model.to_string();
        self.llm.update_model(model);
    }

    /// Set the fast model.
    pub fn set_fast_model(&mut self, model: &str) {
        self.config.fast_model = model.to_string();
        self.llm.update_fast_model(model);
    }

    /// Get current model configuration.
    pub fn get_models(&self) -> (String, String) {
        (self.config.model.clone(), self.config.fast_model.clone())
    }

    /// Get the current iteration count.
    pub fn iteration_count(&self) -> u32 {
        self.iteration_count
    }

    /// Show the current plan
    pub fn state(&self) -> &StateManager {
        &self.state
    }

    /// Show memory index
    pub fn memory(&self) -> &MemorySearch {
        &self.memory
    }

    /// Show error book
    pub fn error_book(&self) -> &ErrorBook {
        &self.error_book
    }

    /// Graceful shutdown: save session memory, clear working memory
    pub async fn shutdown(&mut self) {
        info!("Shutting down agent...");
        let _ = self.save_session_memory().await;
        info!("Agent shutdown complete.");
    }
}

/// Extract tool name from tool_create's [TOOL_CREATED:<name>] output prefix.
fn extract_created_tool_name(output: &str) -> Option<String> {
    let prefix = "[TOOL_CREATED:";
    let start = output.find(prefix)?;
    let rest = &output[start + prefix.len()..];
    let end = rest.find(']').unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Build a concise listing of user-created tools for the system prompt.
fn format_user_tool_list(tools: &ToolRegistry) -> String {
    let built_in: &[&str] = &[
        "web_search", "web_fetch", "code_exec", "file_ops", "tool_create", "tool_list",
    ];
    let mut lines = Vec::new();
    for name in tools.tool_names() {
        if built_in.contains(&name.as_str()) {
            continue;
        }
        if let Some(tool) = tools.get(&name) {
            lines.push(format!("- `{}` — {}", name, tool.description()));
        }
    }
    if lines.is_empty() {
        "(none yet — use tool_create to make one)".to_string()
    } else {
        lines.join("\n")
    }
}
