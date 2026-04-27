use super::utils::{
    truncate, KEEP_RECENT_MESSAGES, MAX_HISTORY_CHARS, MAX_HISTORY_MESSAGES,
    MAX_HISTORY_SUMMARY_CHARS,
};
use super::{Agent, SessionSnapshot, ToolRoundReport};
use crate::llm::types::{Message, Role};

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;

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

pub(super) fn total_message_chars(messages: &[Message]) -> usize {
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
    pub(super) fn compact_message_history(&mut self) {
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

    pub(super) fn llm_messages(&self) -> Vec<Message> {
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
        fs::write(
            session_snapshot_path(&self.config.workspace_path),
            serde_json::to_vec_pretty(&snapshot)?,
        )?;
        Ok(())
    }
}
