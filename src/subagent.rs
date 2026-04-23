use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, RwLock};
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    Closed,
}

impl SubagentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Closed => "closed",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone)]
pub struct SubagentSnapshot {
    pub id: String,
    pub task: String,
    pub share_history: bool,
    pub status: SubagentStatus,
    pub result: Option<String>,
    pub error: Option<String>,
}

struct SubagentRecord {
    id: String,
    task: String,
    share_history: bool,
    status: SubagentStatus,
    result: Option<String>,
    error: Option<String>,
}

struct SubagentEntry {
    state: Arc<RwLock<SubagentRecord>>,
    notify: Arc<Notify>,
    handle: JoinHandle<()>,
}

pub struct SubagentManager {
    next_id: AtomicU64,
    entries: RwLock<HashMap<String, SubagentEntry>>,
}

impl Default for SubagentManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SubagentManager {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            entries: RwLock::new(HashMap::new()),
        }
    }

    pub async fn spawn<F>(&self, task: String, share_history: bool, runner: F) -> String
    where
        F: FnOnce() -> Result<String> + Send + 'static,
    {
        let id = format!("subagent_{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let state = Arc::new(RwLock::new(SubagentRecord {
            id: id.clone(),
            task,
            share_history,
            status: SubagentStatus::Running,
            result: None,
            error: None,
        }));
        let notify = Arc::new(Notify::new());
        let state_for_task = state.clone();
        let notify_for_task = notify.clone();

        let handle = tokio::spawn(async move {
            let outcome = tokio::task::spawn_blocking(runner)
                .await
                .map_err(|err| anyhow!("subagent join error: {}", err))
                .and_then(|res| res);
            let mut record = state_for_task.write().await;
            if record.status == SubagentStatus::Closed {
                notify_for_task.notify_waiters();
                return;
            }
            match outcome {
                Ok(output) => {
                    record.status = SubagentStatus::Completed;
                    record.result = Some(output);
                    record.error = None;
                }
                Err(err) => {
                    record.status = SubagentStatus::Failed;
                    record.result = None;
                    record.error = Some(format!("{:#}", err));
                }
            }
            drop(record);
            notify_for_task.notify_waiters();
        });

        self.entries.write().await.insert(
            id.clone(),
            SubagentEntry {
                state,
                notify,
                handle,
            },
        );
        id
    }

    pub async fn wait(&self, id: &str, timeout: Option<Duration>) -> Result<SubagentSnapshot> {
        let (state, notify) = self.entry_refs(id).await?;
        if !state.read().await.status.is_terminal() {
            let notified = notify.notified();
            if let Some(limit) = timeout {
                tokio::time::timeout(limit, notified)
                    .await
                    .map_err(|_| anyhow!("timeout waiting for {}", id))?;
            } else {
                notified.await;
            }
        }
        Ok(Self::snapshot_from_state(&state).await)
    }

    pub async fn list(&self) -> Vec<SubagentSnapshot> {
        let states: Vec<_> = {
            let entries = self.entries.read().await;
            entries.values().map(|entry| entry.state.clone()).collect()
        };
        let mut snapshots = Vec::with_capacity(states.len());
        for state in states {
            snapshots.push(Self::snapshot_from_state(&state).await);
        }
        snapshots.sort_by(|a, b| a.id.cmp(&b.id));
        snapshots
    }

    pub async fn close(&self, id: &str) -> Result<SubagentSnapshot> {
        let (state, notify, handle) = {
            let entries = self.entries.read().await;
            let entry = entries
                .get(id)
                .ok_or_else(|| anyhow!("unknown subagent: {}", id))?;
            (
                entry.state.clone(),
                entry.notify.clone(),
                entry.handle.abort_handle(),
            )
        };
        handle.abort();
        {
            let mut record = state.write().await;
            record.status = SubagentStatus::Closed;
            record.result = None;
            record.error = Some("aborted by parent agent".into());
        }
        notify.notify_waiters();
        Ok(Self::snapshot_from_state(&state).await)
    }

    pub async fn abort_all(&self) {
        let ids: Vec<_> = {
            let entries = self.entries.read().await;
            entries.keys().cloned().collect()
        };
        for id in ids {
            let _ = self.close(&id).await;
        }
    }

    async fn entry_refs(&self, id: &str) -> Result<(Arc<RwLock<SubagentRecord>>, Arc<Notify>)> {
        let entries = self.entries.read().await;
        let entry = entries
            .get(id)
            .ok_or_else(|| anyhow!("unknown subagent: {}", id))?;
        Ok((entry.state.clone(), entry.notify.clone()))
    }

    async fn snapshot_from_state(state: &Arc<RwLock<SubagentRecord>>) -> SubagentSnapshot {
        let record = state.read().await;
        SubagentSnapshot {
            id: record.id.clone(),
            task: record.task.clone(),
            share_history: record.share_history,
            status: record.status,
            result: record.result.clone(),
            error: record.error.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SubagentManager, SubagentStatus};
    use std::time::Duration;

    #[tokio::test]
    async fn spawn_and_wait_returns_completed_output() {
        let manager = SubagentManager::new();
        let id = manager
            .spawn("demo".into(), false, || Ok("done".into()))
            .await;
        let snapshot = manager
            .wait(&id, Some(Duration::from_secs(1)))
            .await
            .unwrap();
        assert_eq!(snapshot.status, SubagentStatus::Completed);
        assert_eq!(snapshot.result.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn close_marks_subagent_closed() {
        let manager = SubagentManager::new();
        let id = manager
            .spawn("sleep".into(), false, || {
                std::thread::sleep(Duration::from_secs(5));
                Ok("late".into())
            })
            .await;
        let snapshot = manager.close(&id).await.unwrap();
        assert_eq!(snapshot.status, SubagentStatus::Closed);
        assert_eq!(snapshot.error.as_deref(), Some("aborted by parent agent"));
    }
}
