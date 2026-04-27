use super::utils::{push_unique_limited, truncate, MAX_NONCONVERGED_ITEMS};
use super::{Agent, ToolAttempt, ToolRoundReport};
use crate::llm::types::Message;
use crate::tools::registry::ToolResult;

use reqwest::Url;
use std::collections::BTreeSet;

// ── standalone helpers ──

pub(crate) fn repeated_failure_signatures(
    history: &[ToolRoundReport],
    current: &ToolRoundReport,
) -> Option<Vec<String>> {
    if current.has_success() {
        return None;
    }

    let previous = history.last()?;
    if previous.has_success() {
        return None;
    }

    let repeated = current.repeated_failure_signatures(previous);
    if repeated.is_empty() {
        return None;
    }

    Some(repeated)
}

pub(crate) fn build_stall_recovery_prompt(
    repeated: &[String],
    auto_subagent_id: Option<&str>,
) -> String {
    let repeated = repeated
        .iter()
        .take(4)
        .map(|s| format!("`{}`", truncate(s, 120)))
        .collect::<Vec<_>>()
        .join(", ");

    let mut out = format!(
        "You are repeating failing tool actions with no progress: {}. Do not retry the same action unless the parameters materially change. Prefer a different approach, inspect the blocker first, or use `subagent_spawn` to delegate diagnosis in parallel. If external/network constraints block completion, stop tool use and give the user a concise progress + blocker summary.",
        repeated
    );
    if let Some(id) = auto_subagent_id {
        out.push_str(&format!(
            " A diagnostic subagent `{}` was spawned automatically; continue with a different strategy and use `subagent_wait` when its diagnosis would help.",
            id
        ));
    }
    out
}

pub(crate) fn build_blocked_source_prompt(domains: &[String]) -> String {
    let listed = domains
        .iter()
        .take(6)
        .map(|domain| format!("`{}`", domain))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "These domains appear blocked or unusable from the current environment: {}. Do not call `web_fetch` or `browser` on them again in this task unless the user explicitly asks for a retry. Prefer official or otherwise authorized alternatives. Do not pivot to 百度网盘, 夸克网盘, or generic free-download PDF mirror sites.",
        listed
    )
}

pub(crate) fn detect_new_blocked_domain(
    already_blocked: &BTreeSet<String>,
    tool_name: &str,
    params: &serde_json::Value,
    result: &ToolResult,
) -> Option<String> {
    if result.success || !matches!(tool_name, "web_fetch" | "browser") {
        return None;
    }
    if !looks_like_blocked_source_error(result.error.as_deref().unwrap_or_default()) {
        return None;
    }
    let raw_url = params["url"].as_str().unwrap_or("").trim();
    let host = Url::parse(raw_url)
        .ok()?
        .host_str()?
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    (!host.is_empty() && !already_blocked.contains(&host)).then_some(host)
}

pub(super) fn looks_like_blocked_source_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    [
        "anti-bot",
        "human-verification",
        "captcha",
        "just a moment",
        "请稍候",
        "waf",
        "tls / connection handshake failed",
        "site-side blocking",
        "browser connection failed before the page loaded",
        "remote site appears to be closing or rejecting",
        "connection closed before",
        "connection closed",
        "unexpected eof",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(crate) fn build_nonconverged_response_from_messages(
    messages: &[Message],
    explicit_request: Option<&str>,
    reason: &str,
    rounds: &[ToolRoundReport],
) -> String {
    // Closure to detect internal control messages
    let is_control = |content: &str| -> bool {
        let trimmed = content.trim();
        trimmed.starts_with("Plan cycle ")
            || trimmed.starts_with("Based on the tool results above")
            || trimmed.starts_with("Plan mode requires one of two outputs:")
            || trimmed.starts_with("You are repeating failing tool actions with no progress:")
    };

    let request = explicit_request
        .map(str::trim)
        .filter(|content| !content.is_empty() && !is_control(content))
        .or_else(|| {
            messages.iter().find_map(|message| {
                (message.role == crate::llm::types::Role::User)
                    .then(|| message.content.as_deref().unwrap_or("").trim())
                    .filter(|content| !content.is_empty() && !is_control(content))
            })
        })
        .unwrap_or("Unknown request");

    let mut successes = Vec::<String>::new();
    let mut failures = Vec::<String>::new();
    for round in rounds {
        for entry in &round.entries {
            let line = format_attempt_summary(&entry.attempt);
            if entry.attempt.success {
                push_unique_limited(&mut successes, line, MAX_NONCONVERGED_ITEMS);
            } else {
                push_unique_limited(&mut failures, line, MAX_NONCONVERGED_ITEMS);
            }
        }
    }

    use crate::llm::types::Role;
    let last_assistant = messages.iter().rev().find_map(|message| {
        (message.role == Role::Assistant && message.tool_calls.is_none())
            .then(|| message.content.as_deref().unwrap_or("").trim())
            .filter(|content| !content.is_empty())
    });

    let mut out = String::new();
    out.push_str("Task did not complete automatically.\n\n");
    out.push_str(&format!("Reason: {}\n", reason));
    out.push_str(&format!("Request: {}\n", truncate(request, 200)));

    if !successes.is_empty() {
        out.push_str("\nProgress made:\n");
        for item in successes {
            out.push_str("- ");
            out.push_str(&item);
            out.push('\n');
        }
    }

    if !failures.is_empty() {
        out.push_str("\nCurrent blockers:\n");
        for item in failures {
            out.push_str("- ");
            out.push_str(&item);
            out.push('\n');
        }
    }

    if let Some(text) = last_assistant {
        out.push_str("\nLatest assistant reasoning:\n");
        out.push_str(&truncate(text, 280));
        out.push('\n');
    }

    out.push_str("\nRecommended next move: change strategy instead of repeating the same failing tool call. If diagnosis can be parallelized, use `subagent_spawn` for a focused blocker-analysis task.\n");
    out
}

pub(crate) fn request_needs_artifact_verification(request: Option<&str>) -> bool {
    let Some(request) = request.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let lower = request.to_ascii_lowercase();
    [
        "下载",
        "保存",
        "导出",
        "生成文件",
        "pdf",
        "download",
        "save",
        "write file",
        "export",
        "crawl papers",
        "download papers",
    ]
    .iter()
    .any(|needle| request.contains(needle) || lower.contains(needle))
}

pub(crate) fn has_recent_artifact_verification(rounds: &[ToolRoundReport]) -> bool {
    rounds
        .iter()
        .rev()
        .take(3)
        .any(round_has_artifact_verification)
}

fn round_has_artifact_verification(round: &ToolRoundReport) -> bool {
    round.entries.iter().any(|entry| {
        if !entry.result.success {
            return false;
        }
        if entry.result.output.contains("[Generated files") {
            return true;
        }
        entry.call.function.name == "file_ops" && entry.attempt.summary.starts_with("list ")
    })
}

pub(super) fn format_attempt_summary(attempt: &ToolAttempt) -> String {
    let mut line = attempt.signature();
    if !attempt.preview.trim().is_empty() {
        line.push_str(" -> ");
        line.push_str(attempt.preview.trim());
    }
    truncate(&line, 180)
}

// ── Agent impl ──

impl Agent {
    pub(super) async fn maybe_spawn_stall_diagnostic_subagent(
        &self,
        repeated: &[String],
        already_spawned: &mut bool,
    ) -> Option<String> {
        if *already_spawned || self.is_subagent || repeated.is_empty() {
            return None;
        }

        let repeated_text = repeated
            .iter()
            .take(4)
            .map(|item| format!("- {}", truncate(item, 140)))
            .collect::<Vec<_>>()
            .join("\n");
        let task = format!(
            "Diagnose why the main agent is stuck repeating these failing actions:\n{}\n\nReturn a concise diagnosis with concrete next steps. Do not repeat the same failing action unless the parameters materially change. Avoid spawning more subagents.",
            repeated_text
        );

        let config = self.config.clone();
        let seed_messages = Some(self.shareable_messages());
        let task_for_runner = task.clone();
        let id = self
            .subagents
            .spawn(task.clone(), true, move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(async move {
                    let mut agent = super::Agent::new(config).await?;
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

        *already_spawned = true;
        Some(id)
    }
}
