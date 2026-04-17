use crate::llm::types::Message;

/// Context cleaner: summarizes and prunes old messages to keep context lean
pub struct ContextCleaner {
    max_tokens: usize,
}

impl ContextCleaner {
    pub fn new(max_tokens: usize) -> Self {
        Self { max_tokens }
    }

    /// Estimate total tokens in the conversation
    pub fn estimate_tokens(messages: &[Message]) -> usize {
        messages.iter().map(|m| m.estimated_tokens()).sum()
    }

    /// Check if context needs pruning
    pub fn needs_pruning(&self, messages: &[Message]) -> bool {
        let tokens = Self::estimate_tokens(messages);
        tokens > (self.max_tokens * 80 / 100) // 80% threshold
    }

    /// Prune messages: keep system message, summarize old messages, keep recent ones
    /// Returns (pruned_messages, summary_of_removed)
    pub fn prune(&self, messages: &[Message], summary: &str) -> Vec<Message> {
        if messages.len() <= 4 {
            return messages.to_vec();
        }

        let mut pruned = Vec::new();

        // Always keep the system message (first)
        if let Some(first) = messages.first() {
            if first.role == crate::llm::types::Role::System {
                pruned.push(first.clone());
            }
        }

        // Add summary of older messages
        if !summary.is_empty() {
            pruned.push(Message::system(&format!(
                "[Context summary of earlier conversation]\n{}",
                summary
            )));
        }

        // Keep the most recent messages (last 6 message pairs)
        let keep_count = 12.min(messages.len());
        let start = messages.len() - keep_count;
        for msg in &messages[start..] {
            if msg.role != crate::llm::types::Role::System || pruned.is_empty() {
                pruned.push(msg.clone());
            }
        }

        pruned
    }

    /// Build a summarization prompt for old messages
    pub fn build_summary_prompt(messages_to_summarize: &[Message]) -> String {
        let mut text = String::from(
            "Summarize the following conversation context into key facts and decisions (max 300 words). \
             Focus on: goals, decisions made, tool results, errors encountered.\n\n",
        );

        for msg in messages_to_summarize {
            let role = match msg.role {
                crate::llm::types::Role::User => "User",
                crate::llm::types::Role::Assistant => "Assistant",
                crate::llm::types::Role::System => continue,
                crate::llm::types::Role::Tool => "Tool",
            };
            if let Some(ref content) = msg.content {
                let preview = if content.chars().count() > 200 {
                    let truncated: String = content.chars().take(200).collect();
                    format!("{}...", truncated)
                } else {
                    content.clone()
                };
                text.push_str(&format!("{}: {}\n", role, preview));
            }
        }

        text
    }
}
