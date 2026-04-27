use super::Agent;
use crate::llm::types::{ChatResponse, Message};
use crate::planner::{StepStatus, ToolCallChain};
use crate::tools::registry::ToolResult;

use anyhow::{Context, Result};

// ── standalone helpers ──

pub(crate) fn should_auto_plan_mode(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_lowercase();
    if trimmed.lines().count() >= 3 {
        return true;
    }

    let keyword_hits = [
        "step by step",
        "multi-step",
        "multiple steps",
        "first",
        "then",
        "after that",
        "同时",
        "并且",
        "然后",
        "接着",
        "最后",
        "optimize",
        "refactor",
        "debug",
        "investigate",
        "analyze",
        "analyse",
        "implement",
        "build",
        "design",
        "migrate",
        "integrate",
        "audit",
        "review",
        "improve",
        "research",
        "项目",
        "优化",
        "重构",
        "排查",
        "分析",
        "实现",
        "构建",
        "迁移",
        "集成",
        "审计",
        "修复",
        "改造",
    ]
    .iter()
    .filter(|kw| lower.contains(**kw))
    .count();

    let connector_hits = [
        " and ", " then ", " also ", " plus ", "并且", "然后", "接着", "同时",
    ]
    .iter()
    .filter(|kw| lower.contains(**kw))
    .count();

    keyword_hits >= 2
        || connector_hits >= 2
        || (keyword_hits >= 1 && (connector_hits >= 1 || trimmed.len() >= 80))
}

pub(crate) fn plan_mode_kickoff_prompt() -> String {
    "The latest user request appears complex and should start in plan mode. Do not answer normally yet. Return exactly one of the following:\n1. A JSON plan block for the task using the available tools.\n2. `TASK COMPLETE` followed by the final answer if the goal is already complete.\nIf you return a plan, make it only for the next concrete tranche of work.".into()
}

pub(super) fn extract_plan(text: &str) -> Option<ToolCallChain> {
    let json_start = text.find("```json")?;
    let json_content = &text[json_start + 7..];
    let json_end = json_content.find("```")?;
    let json_str = json_content[..json_end].trim();

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
            step.get("description")
                .and_then(|d| d.as_str())
                .unwrap_or(&format!("Step {}", i)),
            step.get("depends_on")
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as usize))
                        .collect()
                })
                .unwrap_or_default(),
        );
    }
    Some(chain)
}

pub(crate) fn extract_task_complete(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let upper = trimmed.to_uppercase();
    if !upper.starts_with("TASK COMPLETE") {
        return None;
    }
    let rest = trimmed["TASK COMPLETE".len()..]
        .trim_start_matches(':')
        .trim();
    Some(if rest.is_empty() {
        "TASK COMPLETE".into()
    } else {
        rest.into()
    })
}

// ── Agent impl ──

impl Agent {
    pub(super) async fn run_plan_mode(
        &mut self,
        initial_plan: Option<ToolCallChain>,
    ) -> Result<String> {
        const MAX_PLAN_CYCLES: usize = 8;

        let mut pending_plan = initial_plan;
        let mut cycle = 0usize;

        if pending_plan.is_none() {
            self.messages
                .push(Message::user(&plan_mode_kickoff_prompt()));
        }

        loop {
            cycle += 1;
            if cycle > MAX_PLAN_CYCLES {
                return Ok(self.build_nonconverged_response(
                    &format!(
                        "Plan mode stopped after {} cycles without reaching `TASK COMPLETE`.",
                        MAX_PLAN_CYCLES
                    ),
                    &[],
                ));
            }

            let plan = match pending_plan.take() {
                Some(plan) => plan,
                None => {
                    let response = self.plan_mode_chat(cycle == 1).await?;
                    let assistant_msg = response
                        .choices
                        .into_iter()
                        .next()
                        .context("No response from LLM")?
                        .message;
                    self.messages.push(assistant_msg.clone());
                    let response_text = assistant_msg.content.unwrap_or_default();

                    if let Some(done) = extract_task_complete(&response_text) {
                        return Ok(done);
                    }
                    if let Some(plan) = extract_plan(&response_text) {
                        plan
                    } else {
                        self.messages.push(Message::user(
                            "Plan mode requires one of two outputs: either a JSON plan block for the remaining work, or `TASK COMPLETE` followed by the final answer if the goal is fully complete. Try again.",
                        ));
                        continue;
                    }
                }
            };

            let summary = self.execute_plan_cycle(plan).await?;
            self.messages.push(Message::user(&format!(
                "Plan cycle {} complete.\n{}\nIf the goal is fully complete, reply with `TASK COMPLETE` followed by the final answer. Otherwise emit another JSON plan block for the remaining work only.",
                cycle, summary
            )));
        }
    }

    async fn plan_mode_chat(&mut self, first_cycle: bool) -> Result<ChatResponse> {
        self.compact_message_history();
        let messages = self.llm_messages();
        let response = if first_cycle {
            self.llm.chat(&messages, None, Some(0.2)).await
        } else {
            self.llm.chat_fast(&messages, None, Some(0.2)).await
        }?;
        self.track_usage(&response);
        self.request_count += 1;
        Ok(response)
    }

    async fn execute_plan_cycle(&mut self, mut chain: ToolCallChain) -> Result<String> {
        let plan_md = chain.to_md();
        println!("\n--- Plan ---\n{}\n--- End Plan ---\n", plan_md);
        self.last_plan = Some(plan_md);

        let mut outputs = vec![];
        while let Some(id) = chain.next_ready() {
            chain.steps[id].status = StepStatus::Running;
            let params = chain.resolve(&chain.steps[id].params.clone());
            let tool = chain.steps[id].tool.clone();
            let mut final_result = None;

            for _ in 0..=self.config.max_retries {
                let result = match self.execute_tool_call(&tool, params.clone()).await {
                    Ok(res) => res,
                    Err(err) => ToolResult::err(format!("{:#}", err)),
                };
                if result.success {
                    final_result = Some((true, result.output.clone()));
                    break;
                }
                final_result = Some((
                    false,
                    result
                        .error
                        .clone()
                        .unwrap_or_else(|| "Unknown error".into()),
                ));
            }

            let (ok, payload) = final_result.unwrap_or_else(|| (false, "Unknown error".into()));
            if ok {
                chain.steps[id].status = StepStatus::Done;
                chain.steps[id].result = Some(payload.clone());
                outputs.push((id, payload));
            } else {
                let err = format!("[FAILED] {}", payload);
                chain.steps[id].status = StepStatus::Failed;
                chain.steps[id].result = Some(err.clone());
                outputs.push((id, err));
            }
        }
        self.last_plan = Some(chain.to_md());

        let mut summary = format!("## Plan Results: {}\n\n", chain.goal);
        for (step_id, output) in &outputs {
            let step = &chain.steps[*step_id];
            let status = match step.status {
                StepStatus::Done => "OK",
                StepStatus::Failed => "FAILED",
                _ => "???",
            };
            let preview: String = output.chars().take(200).collect();
            let suffix = if output.chars().count() > 200 {
                "..."
            } else {
                ""
            };
            summary.push_str(&format!(
                "**Step {} [{}]**: {}\n> {}{}\n\n",
                step_id, status, step.desc, preview, suffix
            ));
        }
        if chain.has_failure() {
            summary.push_str("\nSome steps failed.\n");
        }
        Ok(summary)
    }
}
