use anyhow::Result;
use tracing::{info, warn};

use super::chain::{StepStatus, ToolCallChain};
use crate::reflector::error_book::ErrorBook;
use crate::tools::registry::ToolRegistry;

pub struct ChainExecutor<'a> {
    registry: &'a ToolRegistry,
    error_book: &'a mut ErrorBook,
    max_retries: u32,
}

impl<'a> ChainExecutor<'a> {
    pub fn new(
        registry: &'a ToolRegistry,
        error_book: &'a mut ErrorBook,
        max_retries: u32,
    ) -> Self {
        Self {
            registry,
            error_book,
            max_retries,
        }
    }

    /// Execute a tool call chain step by step
    /// Returns (completed_chain, Vec<(step_id, output)>)
    pub async fn execute(
        &mut self,
        chain: &mut ToolCallChain,
    ) -> Result<Vec<(usize, String)>> {
        let mut outputs = Vec::new();

        loop {
            let step_id = match chain.next_ready() {
                Some(id) => id,
                None => break, // No more ready steps
            };

            chain.steps[step_id].status = StepStatus::Running;
            info!(
                "Executing step {}: {} ({})",
                step_id, chain.steps[step_id].description, chain.steps[step_id].tool
            );

            let resolved_params = chain.resolve_references(&chain.steps[step_id].params.clone());
            let tool_name = chain.steps[step_id].tool.clone();

            let mut last_error = None;
            let mut success = false;

            for attempt in 0..=self.max_retries {
                if attempt > 0 {
                    warn!("Retry step {} attempt {}", step_id, attempt);
                }

                match self.registry.execute(&tool_name, resolved_params.clone()).await {
                    Ok(result) => {
                        if result.success {
                            chain.steps[step_id].status = StepStatus::Done;
                            chain.steps[step_id].result = Some(result.output.clone());
                            outputs.push((step_id, result.output));
                            success = true;
                            break;
                        } else {
                            let error_msg = result.to_string_for_llm();

                            // Check error book for known fix
                            if let Some(fix) = self.error_book.find_fix(&error_msg).await {
                                info!("Found known fix in error book: {}", fix);
                                // For now, log the fix — auto-application depends on fix type
                            }

                            last_error = Some(error_msg);
                        }
                    }
                    Err(e) => {
                        let error_msg = format!("{:#}", e);

                        if let Some(fix) = self.error_book.find_fix(&error_msg).await {
                            info!("Found known fix in error book: {}", fix);
                        }

                        last_error = Some(error_msg);
                    }
                }
            }

            if !success {
                let error = last_error.unwrap_or_else(|| "Unknown error".to_string());
                chain.steps[step_id].status = StepStatus::Failed;
                chain.steps[step_id].result = Some(format!("[FAILED] {}", error));

                // Record to error book
                self.error_book
                    .record_error(&tool_name, &error)
                    .await?;

                outputs.push((step_id, format!("[FAILED] {}", error)));
                warn!("Step {} failed after retries: {}", step_id, error);
            }
        }

        Ok(outputs)
    }
}
