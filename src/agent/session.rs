use super::utils::{
    truncate, KEEP_RECENT_MESSAGES, MAX_HISTORY_CHARS, MAX_HISTORY_MESSAGES,
    MAX_HISTORY_SUMMARY_CHARS, MAX_TOOL_RESULT_CHARS,
};
use super::{Agent, SessionSnapshot, ToolRoundReport};
use crate::llm::client::LlmClient;
use crate::llm::types::{Message, Role};

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;

// ── standalone helpers ──

/// Rough token estimate for context management.
/// English: ~4 chars/token, CJK: ~2 chars/token, mixed: ~3.
/// Conservative overestimate keeps us safely within context windows.
fn estimate_tokens(messages: &[Message]) -> usize {
    let chars: usize = messages.iter().map(|m| {
        let content = m.content.as_deref().unwrap_or("").chars().count();
        let tool_call = m.tool_calls.as_ref().map(|c| {
            c.iter()
                .map(|tc| tc.function.name.chars().count() + tc.function.arguments.chars().count())
                .sum::<usize>()
        }).unwrap_or(0);
        content + tool_call
    }).sum();
    chars / 3 + messages.len() * 4 // header overhead per message
}

// ── standalone helpers ──

pub(crate) fn session_snapshot_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".rubot_session.json")
}

pub(crate) fn clear_session_snapshot_file(workspace_root: &Path) -> Result<()> {
    let path = session_snapshot_path(workspace_root);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub(super) fn load_session_snapshot(workspace_root: &Path) -> Result<Option<SessionSnapshot>> {
    let path = session_snapshot_path(workspace_root);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };

    let snapshot = match serde_json::from_str::<SessionSnapshot>(&raw) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            clear_session_snapshot_file(workspace_root)?;
            return Ok(None);
        }
    };
    Ok(Some(snapshot))
}

pub(crate) fn summarize_messages(messages: &[Message]) -> String {
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

/// Use the fast LLM to generate a narrative summary of dropped messages.
/// Falls back to rule-based summarization on LLM error.
pub(super) async fn summarize_messages_with_llm(
    llm: &LlmClient,
    messages: &[Message],
    existing_summary: Option<&str>,
) -> String {
    if messages.is_empty() {
        return existing_summary.unwrap_or_default().to_string();
    }

    // Format messages for the summarization prompt (max 20 to keep it quick)
    let formatted: String = messages
        .iter()
        .rev()
        .take(20)
        .rev()
        .filter_map(|msg| match msg.role {
            Role::System => None,
            Role::User => Some(format!("user: {}", truncate(msg.content.as_deref().unwrap_or(""), 200))),
            Role::Assistant => {
                if let Some(calls) = &msg.tool_calls {
                    let names: Vec<&str> = calls.iter().map(|c| c.function.name.as_str()).collect();
                    Some(format!("assistant [tools: {}]", names.join(", ")))
                } else {
                    Some(format!("assistant: {}", truncate(msg.content.as_deref().unwrap_or(""), 200)))
                }
            }
            Role::Tool => Some(format!("  → {}", truncate(msg.content.as_deref().unwrap_or(""), 120))),
        })
        .collect::<Vec<_>>()
        .join("\n");

    if formatted.trim().is_empty() {
        return existing_summary.unwrap_or_default().to_string();
    }

    let prompt = crate::personality::conversation_summary_prompt(existing_summary, &formatted);
    let msgs = vec![Message::user(&prompt)];

    match llm.chat_fast(&msgs, None, Some(0.3)).await {
        Ok(response) => response
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .map(|t| truncate(&t, 1500))
            .unwrap_or_else(|| summarize_messages(messages)),
        Err(e) => {
            tracing::warn!("LLM summarization failed, using fallback: {:#}", e);
            summarize_messages(messages)
        }
    }
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

// ── Agent impl ──

impl Agent {
    pub(super) async fn compact_message_history(&mut self) {
        let prefix_count = self.prefix_message_count();
        if self.messages.len() <= prefix_count {
            return;
        }

        let active: Vec<Message> = self.messages[prefix_count..].to_vec();
        let over_messages = active.len() > MAX_HISTORY_MESSAGES;
        let over_tokens = estimate_tokens(&active) > MAX_HISTORY_CHARS / 3;
        if !over_messages && !over_tokens {
            return;
        }

        // Tier 1: trim individual large tool results before dropping messages
        if !over_messages {
            let mut trimmed = 0usize;
            for msg in &mut self.messages {
                if msg.role == Role::Tool {
                    if let Some(ref content) = msg.content {
                        let char_count = content.chars().count();
                        if char_count > MAX_TOOL_RESULT_CHARS {
                            let half = MAX_TOOL_RESULT_CHARS / 2;
                            let first: String = content.chars().take(half).collect();
                            let last: String = content
                                .chars()
                                .rev()
                                .take(200)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect();
                            msg.content = Some(format!(
                                "{}…\n[... truncated {} chars …]\n…{}",
                                first,
                                char_count.saturating_sub(MAX_TOOL_RESULT_CHARS),
                                last,
                            ));
                            trimmed += 1;
                        }
                    }
                }
            }
            if trimmed > 0 {
                tracing::debug!("trimmed {} oversized tool results", trimmed);
                let active2: Vec<Message> = self.messages[prefix_count..].to_vec();
                if active2.len() <= MAX_HISTORY_MESSAGES
                    && estimate_tokens(&active2) <= MAX_HISTORY_CHARS / 3
                {
                    return;
                }
            }
        }

        // Tier 2: drop from the middle, keep recent messages and a summary
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
        let recent_len = recent.len();

        let dropped_summary = summarize_messages_with_llm(
            &self.llm,
            &dropped,
            self.history_summary.as_deref(),
        )
        .await;
        self.history_summary = merge_history_summary(self.history_summary.take(), dropped_summary);

        self.messages.truncate(prefix_count);
        self.messages.extend(recent);
        tracing::debug!(
            "compacted: dropped {} messages, kept {}, summary chars={}",
            dropped.len(),
            recent_len,
            self.history_summary.as_ref().map_or(0, |s| s.len()),
        );
    }

    pub(super) fn llm_messages(&self) -> Vec<Message> {
        let mut out = Vec::with_capacity(self.messages.len() + 2);
        let prefix_count = self.prefix_message_count();

        // System messages (stable, avoid timestamps/volatile data here)
        out.extend(self.messages.iter().take(prefix_count).cloned());

        // Transient skill context (included in prompt but never stored in messages)
        if let Some(ref ctx) = self.current_skill_context {
            out.push(Message::system(ctx));
        }

        // History summary — injected as a regular user message (no cache_control:
        // its content changes each turn and would bust the cache prefix)
        if let Some(summary) = &self.history_summary {
            out.push(Message::user(summary));
        }

        // Active conversation
        if self.messages.len() > prefix_count {
            out.extend(self.messages[prefix_count..].iter().cloned());
        }

        // Single cache breakpoint on the LAST user/assistant message (Claude Code pattern).
        for msg in out.iter_mut().rev() {
            if matches!(msg.role, Role::User | Role::Assistant) {
                msg.cache_control = Some(crate::llm::types::CacheControl::ephemeral());
                break;
            }
        }

        out
    }

    pub(super) fn build_nonconverged_response(
        &self,
        reason: &str,
        rounds: &[ToolRoundReport],
    ) -> String {
        crate::agent::stall::build_nonconverged_response_from_messages(
            &self.messages,
            self.current_request.as_deref(),
            reason,
            rounds,
        )
    }

    pub(super) fn restore_session(mut self) -> Result<Self> {
        let Some(snapshot) = load_session_snapshot(&self.config.workspace_path)? else {
            return Ok(self);
        };

        self.history_summary = snapshot.history_summary;
        self.current_request = snapshot.current_request;
        self.restored_session_messages = snapshot.messages.len();
        self.messages.extend(
            snapshot
                .messages
                .into_iter()
                .filter(|message| message.role != Role::System),
        );
        Ok(self)
    }

    pub(super) fn persist_session_snapshot(&self) -> Result<()> {
        if self.is_subagent {
            return Ok(());
        }

        let messages = self
            .messages
            .iter()
            .filter(|message| message.role != Role::System)
            .cloned()
            .collect::<Vec<_>>();
        if messages.is_empty() && self.history_summary.is_none() {
            return clear_session_snapshot_file(&self.config.workspace_path);
        }

        let snapshot = SessionSnapshot {
            version: 1,
            saved_at: chrono::Utc::now().to_rfc3339(),
            history_summary: self.history_summary.clone(),
            current_request: self.current_request.clone(),
            messages,
        };
        // Atomic write: write to temp path, then rename (avoids partial writes on crash).
        let path = session_snapshot_path(&self.config.workspace_path);
        let tmp_path = path.with_extension("session.tmp.json");
        fs::write(&tmp_path, serde_json::to_vec_pretty(&snapshot)?)?;
        fs::rename(&tmp_path, &path)?;
        Ok(())
    }
}
