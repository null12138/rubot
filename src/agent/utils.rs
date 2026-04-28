use crate::llm::types::ToolDefinition;
use crate::subagent::SubagentSnapshot;

// ── constants ──

pub(super) const MAX_TOOL_RESULT_CHARS: usize = 2_400;
pub(super) const MAX_MEMORY_INDEX_CHARS: usize = 600;
pub(super) const MAX_HISTORY_MESSAGES: usize = 20;
pub(super) const KEEP_RECENT_MESSAGES: usize = 10;
pub(super) const MAX_HISTORY_CHARS: usize = 14_000;
pub(super) const MAX_HISTORY_SUMMARY_CHARS: usize = 1_500;
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
                            "share_history": {"type": "boolean", "description": "Copy current conversation history", "default": false},
                            "model": {"type": "string", "enum": ["fast", "heavy"], "description": "Which model to use; fast (default) is cheaper for simple tasks", "default": "fast"},
                            "timeout_secs": {"type": "integer", "minimum": 10, "description": "Auto-close subagent after this many seconds"}
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

// ── scheduler tool definitions ──
pub(super) fn scheduler_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            "scheduler_add",
            "Add a recurring scheduled task. The task runs automatically when its cron triggers. Use for periodic reminders, daily reports, monitoring, etc.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "The task prompt to execute on schedule"},
                    "cron": {"type": "string", "description": "Cron expression: minute hour day-of-month month day-of-week. Examples: '*/5 * * * *' (every 5 min), '0 * * * *' (hourly), '0 9 * * *' (daily 9am), '*/30 * * * *' (every 30 min)"}
                },
                "required": ["prompt", "cron"]
            }),
        )
        .compact_for_llm(),
        ToolDefinition::new(
            "scheduler_list",
            "List all scheduled tasks with their cron, next run time, and run count.",
            serde_json::json!({"type": "object", "properties": {}, "required": []}),
        )
        .compact_for_llm(),
        ToolDefinition::new(
            "scheduler_remove",
            "Remove a scheduled task by its ID.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Task ID to remove"}
                },
                "required": ["id"]
            }),
        )
        .compact_for_llm(),
    ]
}

// ── memory tool definitions ──

pub(super) fn memory_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            "memory_search",
            "Search memory across all layers for relevant past context. Use before starting a task to recall related findings, user preferences, or past solutions.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search keywords"}
                },
                "required": ["query"]
            }),
        )
        .compact_for_llm(),
        ToolDefinition::new(
            "memory_add",
            "Store a fact, finding, or preference in memory. Use after discovering something useful that should be remembered for future tasks.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "layer": {"type": "string", "enum": ["working", "episodic", "semantic"], "description": "Memory tier: working (temporary findings), episodic (project patterns), semantic (permanent knowledge about user/preferences/conventions)", "default": "working"},
                    "summary": {"type": "string", "description": "One-line summary (used for dedup and search)"},
                    "content": {"type": "string", "description": "Full content to remember"},
                    "tags": {"type": "array", "items": {"type": "string"}, "description": "Tags for categorization"}
                },
                "required": ["summary", "content"]
            }),
        )
        .compact_for_llm(),
        ToolDefinition::new(
            "memory_touch",
            "Review and strengthen a memory entry, increasing its retention. Use for due items shown in the memory index or after confirming a memory is still relevant.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string", "description": "Memory entry file ID (e.g. working/20260427_120000_abc12345)"}
                },
                "required": ["file"]
            }),
        )
        .compact_for_llm(),
        ToolDefinition::new(
            "memory_due",
            "List memory entries overdue for review (Ebbinghaus spacing). Review these with memory_touch to strengthen retention.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        )
        .compact_for_llm(),
        ToolDefinition::new(
            "tool_create",
            "Crystallize a working solution into a reusable MD tool. Provide name, description, language, parameter schema, and code. The tool auto-registers for the next turn.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Tool name (lowercase, underscores only, e.g. get_stock_price)"},
                    "description": {"type": "string", "description": "What the tool does, when to use it"},
                    "language": {"type": "string", "enum": ["bash", "python"], "description": "Implementation language"},
                    "code": {"type": "string", "description": "The script body. Bash: params come as env vars. Python: params come as JSON on stdin."},
                    "parameters": {"type": "object", "description": "JSON Schema for tool parameters, e.g. {\"type\":\"object\",\"properties\":{\"symbol\":{\"type\":\"string\"}},\"required\":[\"symbol\"]}"}
                },
                "required": ["name", "description", "language", "code"]
            }),
        )
        .compact_for_llm(),
        ToolDefinition::new(
            "tool_delete",
            "Remove an MD-backed tool by name (without .md extension).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Tool name to delete (e.g. get_stock_price)"}
                },
                "required": ["name"]
            }),
        )
        .compact_for_llm(),
        ToolDefinition::new(
            "tool_list",
            "List all registered tools by name. Use this to discover available tools before creating a new one.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        )
        .compact_for_llm(),
        ToolDefinition::new(
            "tool_show",
            "Show the full definition (description, parameters) of a specific tool by name.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Tool name to inspect (e.g. web_search)"}
                },
                "required": ["name"]
            }),
        )
        .compact_for_llm(),
    ]
}

/// Extract the first JSON object `{...}` from text, skipping markdown fences.
pub(crate) fn extract_json_object(text: &str) -> Option<serde_json::Value> {
    let text = text.trim();
    // Strip ```json / ``` fences if present.
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .map(|s| s.strip_suffix("```").unwrap_or(s))
        .unwrap_or(text);
    if let Some(start) = text.find('{') {
        let slice = &text[start..];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) {
            return Some(v);
        }
        // Try to find the matching closing brace.
        let mut depth = 0i32;
        let mut end = start;
        for (i, ch) in text[start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        if end > start {
            let candidate = &text[start..end];
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate) {
                return Some(v);
            }
        }
    }
    None
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
        "subagent_spawn" => {
            let task: String = params["task"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            let model = params["model"].as_str().unwrap_or("fast");
            format!("[{}] {}", model, task)
        }
        "subagent_wait" | "subagent_close" => params["id"].as_str().unwrap_or("").to_string(),
        "memory_search" => params["query"].as_str().unwrap_or("").to_string(),
        "memory_add" => params["summary"].as_str().unwrap_or("").to_string(),
        "memory_touch" => params["file"].as_str().unwrap_or("").to_string(),
        "tool_create" => params["name"].as_str().unwrap_or("").to_string(),
        "tool_delete" => params["name"].as_str().unwrap_or("").to_string(),
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
