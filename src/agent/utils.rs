use crate::llm::types::ToolDefinition;
use crate::subagent::SubagentSnapshot;

// ── constants ──

pub(super) const MAX_TOOL_RESULT_CHARS: usize = 2_400;
pub(super) const MAX_MEMORY_INDEX_CHARS: usize = 3_200;
pub(super) const MAX_HISTORY_MESSAGES: usize = 28;
pub(super) const KEEP_RECENT_MESSAGES: usize = 12;
pub(super) const MAX_HISTORY_CHARS: usize = 18_000;
pub(super) const MAX_HISTORY_SUMMARY_CHARS: usize = 3_000;
pub(super) const MAX_TRACKED_TOOL_ROUNDS: usize = 6;
pub(super) const MAX_NONCONVERGED_ITEMS: usize = 6;

// ── shared helpers ──

pub(super) fn truncate(s: &str, n: usize) -> String {
    let taken: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{}…", taken)
    } else {
        taken
    }
}

pub(crate) fn compact_memory_index(memory_index: &str) -> String {
    if memory_index.chars().count() <= MAX_MEMORY_INDEX_CHARS {
        return memory_index.to_string();
    }
    format!(
        "{}\n\n...(memory index truncated for token efficiency)...",
        truncate(memory_index, MAX_MEMORY_INDEX_CHARS)
    )
}

pub(super) fn push_unique_limited(items: &mut Vec<String>, value: String, max_items: usize) {
    if items.iter().any(|existing| existing == &value) {
        return;
    }
    if items.len() < max_items {
        items.push(value);
    }
}

pub(super) fn is_internal_control_message(content: &str, kickoff_prompt: &str) -> bool {
    let trimmed = content.trim();
    trimmed == kickoff_prompt
        || trimmed.starts_with("Plan cycle ")
        || trimmed.starts_with("Based on the tool results above")
        || trimmed.starts_with("Plan mode requires one of two outputs:")
        || trimmed.starts_with("You are repeating failing tool actions with no progress:")
}

pub(super) fn looks_like_internal_assistant_message(content: &str) -> bool {
    let trimmed = content.trim();
    trimmed.eq_ignore_ascii_case("TASK COMPLETE")
        || trimmed.starts_with("Task did not complete automatically.")
}

// ── tool definitions ──

pub(super) fn channel_send_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "channel_send",
        "Queue a file for delivery through the current chat channel (WeChat). Use this to explicitly send a file or image to the user. Files created by code_exec, latex_pdf, or file_ops are auto-detected too, but calling this gives you explicit control.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute path to the file to send, or path relative to workspace/files/"}
            },
            "required": ["path"]
        }),
    )
    .compact_for_llm()
}

pub(super) fn subagent_tool_definitions() -> Vec<ToolDefinition> {
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

pub(super) fn format_subagent_summary(snapshot: &SubagentSnapshot) -> String {
    format!(
        "- {} [{}] share_history={} task={}",
        snapshot.id,
        snapshot.status.as_str(),
        snapshot.share_history,
        snapshot.task
    )
}

pub(super) fn format_subagent_snapshot(snapshot: &SubagentSnapshot) -> String {
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

// ── param summarizer ──

pub(super) fn summarize_params(tool_name: &str, params: &serde_json::Value) -> String {
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
