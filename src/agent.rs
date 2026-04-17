use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::Config;
use crate::context::cleaner::ContextCleaner;
use crate::llm::client::LlmClient;
use crate::llm::types::*;
use crate::memory::layer::MemoryLayer;
use crate::memory::search::MemorySearch;
use crate::personality;
use crate::planner::chain::{StepStatus, ToolCallChain};
use crate::planner::executor::ChainExecutor;
use crate::reflector::error_book::ErrorBook;
use crate::state::manager::StateManager;
use crate::tools::registry::ToolRegistry;
use crate::tools::{
    code_exec::CodeExec, file_ops::FileOps, path_remember::PathRemember, skill_create::SkillCreate,
    skill_get::SkillGet, tool_forge::ToolForge, web_fetch::WebFetch, web_search::WebSearch,
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
        tools.register(Box::new(CodeExec::new(config.code_exec_timeout_secs)));
        tools.register(Box::new(FileOps::new(&config.workspace_path)));
        tools.register(Box::new(SkillCreate::new(&config.workspace_path)));
        tools.register(Box::new(SkillGet::new(&config.workspace_path)));
        tools.register(Box::new(ToolForge::new(&config.workspace_path)));
        tools.register(Box::new(PathRemember::new(&config.workspace_path)));

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
            .get_index_for_context()
            .await
            .unwrap_or_else(|_| "(empty)".to_string());
        let error_summary = error_book.to_text();
        let system_msg =
            Message::system(&personality::system_prompt(&memory_index, &error_summary));

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
    pub async fn process(&mut self, input: &str) -> Result<String> {
        self.messages.push(Message::user(input));
        self.iteration_count = 0;

        self.run_loop().await
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
        loop {
            self.iteration_count += 1;
            if self.iteration_count > self.max_iterations {
                let msg =
                    "Reached maximum iterations. Stopping to prevent infinite loop.".to_string();
                warn!("{}", msg);
                return Ok(msg);
            }

            // === THINK: Prune context if needed ===
            if self.context_cleaner.needs_pruning(&self.messages) {
                info!("Context pruning triggered");
                self.prune_context().await?;
            }

            // === PLAN: Ask LLM ===
            // Strategy: first round uses heavy model for understanding intent;
            // subsequent tool-calling rounds use fast model for speed;
            // the final synthesis round (no tools returned) goes back to heavy model.
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
            if let Some(ref tool_calls) = assistant_msg.tool_calls {
                if tool_calls.is_empty() {
                    return Ok(assistant_msg.content.unwrap_or_default());
                }

                // Execute tool calls in parallel
                let results = self.execute_tools_parallel(tool_calls).await?;

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
            "tool_forge" => {
                let name = params["name"].as_str().unwrap_or("?");
                format!("create tool: {}", name)
            }
            "path_remember" => {
                let name = params["task_name"].as_str().unwrap_or("?");
                format!("save path: {}", name)
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
        let plan_md = chain.to_markdown();
        println!("\n--- Plan ---\n{}\n--- End Plan ---\n", plan_md);
        println!("(Executing plan...)\n");

        // Save plan to state
        self.state.save_plan(&plan_md).await?;

        // Execute the chain
        let mut executor =
            ChainExecutor::new(&self.tools, &mut self.error_book, self.config.max_retries);
        let outputs = executor.execute(&mut chain).await?;

        // Update plan state
        self.state.save_plan(&chain.to_markdown()).await?;

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
                step.description,
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
    async fn prune_context(&mut self) -> Result<()> {
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

        // Ask LLM to summarize (use fast model for speed)
        let summary_prompt = ContextCleaner::build_summary_prompt(to_summarize);
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

        self.memory
            .index()
            .add_memory(MemoryLayer::Working, &summary, &content, &[])
            .await?;

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
