use super::utils::{channel_send_tool_definition, compact_memory_index, subagent_tool_definitions};
use super::Agent;
use crate::config::Config;
use crate::llm::client::LlmClient;
use crate::llm::types::*;
use crate::memory::MemorySearch;
use crate::personality;
use crate::tools::registry::ToolRegistry;
use crate::tools::{
    browser::BrowserTool, code_exec::CodeExec, file_ops::FileOps, latex_pdf::LatexPdf,
    web_fetch::WebFetch, web_search::WebSearch,
};

use anyhow::Result;

impl Agent {
    pub(super) async fn build_prompt_messages(
        memory: &MemorySearch,
        config: &Config,
    ) -> Result<Vec<Message>> {
        let memory_index = memory
            .get_index_text()
            .await
            .unwrap_or_else(|_| "(empty)".into());

        let mut messages = vec![
            Message::system(&personality::base_system_prompt()),
            Message::system(&personality::session_context_prompt(
                &config.workspace_path,
                &config.cwd,
                &config.model,
                &config.fast_model,
            )),
            Message::system(&personality::date_context_prompt()),
            Message::system(&personality::memory_snapshot_prompt(&compact_memory_index(
                &memory_index,
            ))),
        ];

        if !config.wechat_bot_token.is_empty() {
            messages.push(Message::system(&personality::wechat_channel_prompt()));
        }

        Ok(messages)
    }

    pub(super) async fn build_runtime(
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
            .register(Box::new(BrowserTool::new(
                &config.workspace_path,
                config.code_exec_timeout_secs,
            )))
            .await;
        tools
            .register(Box::new(CodeExec::new(
                config.code_exec_timeout_secs,
                &config.cwd,
                &config.workspace_path,
            )))
            .await;
        tools
            .register(Box::new(FileOps::new(&config.workspace_path, &config.cwd)))
            .await;
        tools
            .register(Box::new(LatexPdf::new(&config.workspace_path)))
            .await;
        tools.load_md_tools().await?;

        let memory = MemorySearch::new(&config.workspace_path);
        let prompt_messages = Self::build_prompt_messages(&memory, config).await?;

        Ok((llm, tools, memory, prompt_messages))
    }

    pub(super) async fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs = self.tools.definitions().await;
        defs.extend(subagent_tool_definitions());
        defs.push(channel_send_tool_definition());
        defs.sort_by(|a, b| a.function.name.cmp(&b.function.name));
        defs
    }

    pub(super) async fn refresh_prompt_messages(&mut self) {
        if let Ok(prompt_messages) = Self::build_prompt_messages(&self.memory, &self.config).await {
            self.replace_prefix_messages(prompt_messages);
        }
    }

    pub(super) fn prefix_message_count(&self) -> usize {
        self.messages
            .iter()
            .take_while(|message| message.role == Role::System)
            .count()
    }

    pub(super) fn replace_prefix_messages(&mut self, prefix_messages: Vec<Message>) {
        let prefix_count = self.prefix_message_count();
        self.messages.splice(0..prefix_count, prefix_messages);
    }
}
