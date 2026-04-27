use super::clear_session_snapshot_file;
use super::plan::{extract_task_complete, should_auto_plan_mode};
use super::session::{session_snapshot_path, summarize_messages};
use super::stall::{
    build_blocked_source_prompt, build_nonconverged_response_from_messages,
    build_stall_recovery_prompt, detect_new_blocked_domain, has_recent_artifact_verification,
    repeated_failure_signatures, request_needs_artifact_verification,
};
use super::utils::compact_memory_index;
use super::{ExecutedTool, ToolAttempt, ToolRoundReport};
use crate::agent::Agent;
use crate::config::Config;
use crate::llm::types::{FunctionCall, Message, Role, ToolCall};
use crate::tools::registry::ToolResult;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

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

#[test]
fn repeated_failures_trigger_recovery_prompt() {
    let previous = ToolRoundReport {
        entries: vec![failed_tool(
            "code_exec",
            "[bash] cd files/ssrn_crawler",
            "Exit 1",
        )],
        newly_blocked_domains: vec![],
    };
    let current = ToolRoundReport {
        entries: vec![failed_tool(
            "code_exec",
            "[bash] cd files/ssrn_crawler",
            "Exit 1",
        )],
        newly_blocked_domains: vec![],
    };

    let repeated = repeated_failure_signatures(&[previous], &current).unwrap();
    let prompt = build_stall_recovery_prompt(&repeated, Some("sub_1"));
    assert!(prompt.contains("subagent_spawn"));
    assert!(prompt.contains("repeating failing tool actions"));
    assert!(prompt.contains("sub_1"));
}

#[test]
fn nonconverged_response_summarizes_progress_and_blockers() {
    let messages = vec![
        Message::system("base"),
        Message::user("帮我爬取 SSRN 并全自动完成"),
        Message::user("You are repeating failing tool actions with no progress: `code_exec`"),
        Message::new(Role::Assistant, "Still blocked by SSRN HTTP restrictions."),
    ];
    let rounds = vec![
        ToolRoundReport {
            entries: vec![ok_tool(
                "file_ops",
                "read ssrn_crawler/crawler.py",
                "#!/usr/bin/env python3",
            )],
            newly_blocked_domains: vec![],
        },
        ToolRoundReport {
            entries: vec![failed_tool(
                "web_fetch",
                "https://papers.ssrn.com/robots.txt",
                "403 Forbidden",
            )],
            newly_blocked_domains: vec![],
        },
    ];

    let summary = build_nonconverged_response_from_messages(
        &messages,
        None,
        "Reached maximum iterations (30) without converging.",
        &rounds,
    );
    assert!(summary.contains("Task did not complete automatically."));
    assert!(summary.contains("帮我爬取 SSRN"));
    assert!(summary.contains("Progress made:"));
    assert!(summary.contains("Current blockers:"));
    assert!(summary.contains("subagent_spawn"));
}

#[tokio::test]
async fn agent_restores_persisted_session_snapshot() {
    let workspace = temp_workspace();
    let config = test_config(&workspace);
    std::fs::write(
        session_snapshot_path(&workspace),
        r#"{"version":1,"saved_at":"2026-01-01T00:00:00Z","history_summary":"x","current_request":"keep going","messages":[{"role":"user","content":"old","tool_calls":null,"tool_call_id":null}]}"#,
    )
    .unwrap();

    let restored = Agent::new(config).await.unwrap();
    assert_eq!(restored.restored_session_messages(), 1);
    assert_eq!(restored.history_summary.as_deref(), Some("x"));
    assert_eq!(restored.current_request.as_deref(), Some("keep going"));
    assert!(restored
        .messages
        .iter()
        .any(|m| m.content.as_deref() == Some("old")));
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn nonconverged_response_uses_explicit_request() {
    let summary = build_nonconverged_response_from_messages(
        &[Message::user("Plan cycle 2 complete.")],
        Some("继续 ssrn 爬取任务"),
        "Reached maximum iterations (30) without converging.",
        &[],
    );
    assert!(summary.contains("继续 ssrn 爬取任务"));
    assert!(!summary.contains("Unknown request"));
}

#[test]
fn clear_session_snapshot_is_idempotent() {
    let workspace = temp_workspace();
    clear_session_snapshot_file(&workspace).unwrap();
    std::fs::write(session_snapshot_path(&workspace), "{}").unwrap();
    clear_session_snapshot_file(&workspace).unwrap();
    assert!(!session_snapshot_path(&workspace).exists());
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn blocked_source_prompt_mentions_no_retry_and_mirror_sites() {
    let prompt = build_blocked_source_prompt(&["fenbi.com".into(), "aipta.com".into()]);
    assert!(prompt.contains("fenbi.com"));
    assert!(prompt.contains("Do not call `web_fetch` or `browser`"));
    assert!(prompt.contains("百度网盘"));
}

#[test]
fn blocked_domain_is_detected_from_fetch_failures() {
    let blocked = BTreeSet::new();
    let params = serde_json::json!({"url": "https://www.fenbi.com/fpr/doc-user-v2/dir/21578"});
    let result = ToolResult::err(
        "TLS / connection handshake failed for https://www.fenbi.com/. The remote site closed the connection before completing HTTPS setup. This usually means site-side blocking, WAF / anti-bot protection, or regional network restrictions.".into(),
    );
    let domain = detect_new_blocked_domain(&blocked, "web_fetch", &params, &result);
    assert_eq!(domain.as_deref(), Some("fenbi.com"));
}

#[test]
fn download_requests_require_artifact_verification() {
    assert!(request_needs_artifact_verification(Some(
        "帮我下载几篇 ssrn 论文"
    )));
    assert!(request_needs_artifact_verification(Some(
        "download a few pdf papers"
    )));
    assert!(!request_needs_artifact_verification(Some("解释这个函数")));
}

#[test]
fn generated_files_or_file_list_count_as_artifact_verification() {
    let rounds = vec![ToolRoundReport {
        entries: vec![ExecutedTool {
            call: ToolCall {
                id: "call_1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "code_exec".into(),
                    arguments: "{}".into(),
                },
            },
            result: ToolResult::ok("done\n\n[Generated files]\n- /tmp/a.pdf (12 KB)\n".into()),
            attempt: ToolAttempt {
                name: "code_exec".into(),
                summary: "[python] ...".into(),
                success: true,
                preview: "done".into(),
            },
        }],
        newly_blocked_domains: vec![],
    }];
    assert!(has_recent_artifact_verification(&rounds));
}

fn failed_tool(name: &str, summary: &str, preview: &str) -> ExecutedTool {
    tool(name, summary, preview, false)
}

fn ok_tool(name: &str, summary: &str, preview: &str) -> ExecutedTool {
    tool(name, summary, preview, true)
}

fn tool(name: &str, summary: &str, preview: &str, success: bool) -> ExecutedTool {
    ExecutedTool {
        call: ToolCall {
            id: "call_1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: "{}".into(),
            },
        },
        result: if success {
            ToolResult::ok(preview.into())
        } else {
            ToolResult::err(preview.into())
        },
        attempt: ToolAttempt {
            name: name.into(),
            summary: summary.into(),
            success,
            preview: preview.into(),
        },
    }
}

fn temp_workspace() -> PathBuf {
    let mut dir = std::env::temp_dir();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!(
        "rubot-agent-test-{}-{}",
        std::process::id(),
        unique
    ));
    std::fs::create_dir_all(dir.join("files")).unwrap();
    std::fs::create_dir_all(dir.join("tools")).unwrap();
    std::fs::create_dir_all(dir.join("memory/working")).unwrap();
    std::fs::create_dir_all(dir.join("memory/episodic")).unwrap();
    std::fs::create_dir_all(dir.join("memory/semantic")).unwrap();
    dir
}

fn test_config(workspace: &PathBuf) -> Config {
    Config {
        api_base_url: "https://example.com/v1".into(),
        api_key: "sk-test".into(),
        model: "gpt-test".into(),
        fast_model: "gpt-test-fast".into(),
        tavily_api_key: String::new(),
        workspace_path: workspace.clone(),
        cwd: workspace.clone(),
        max_retries: 0,
        code_exec_timeout_secs: 30,
        wechat_bot_token: String::new(),
        wechat_base_url: String::new(),
        telegram_bot_token: String::new(),
        orkey: String::new(),
        sleep_interval_secs: 300,
    }
}
