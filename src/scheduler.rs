use anyhow::Result;
use chrono::Timelike;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ── data types ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub prompt: String,
    pub cron: String,
    pub next_run: String,
    pub last_run: Option<String>,
    pub created: String,
    pub run_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TaskStore {
    tasks: Vec<ScheduledTask>,
}

// ── scheduler ──

pub struct Scheduler {
    path: PathBuf,
    tasks: Vec<ScheduledTask>,
}

impl Scheduler {
    pub fn new(workspace: &Path) -> Self {
        let path = workspace.join("scheduler_tasks.json");
        let tasks = load_tasks(&path).unwrap_or_default();
        Self { path, tasks }
    }

    pub fn all(&self) -> &[ScheduledTask] {
        &self.tasks
    }

    pub fn add(&mut self, prompt: &str, cron: &str) -> Result<String> {
        let id = new_id();
        let now = chrono::Utc::now().to_rfc3339();
        let next = compute_next_run(cron).unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        let task = ScheduledTask {
            id: id.clone(),
            prompt: prompt.to_string(),
            cron: cron.to_string(),
            next_run: next,
            last_run: None,
            created: now,
            run_count: 0,
        };
        self.tasks.push(task);
        self.save()?;
        Ok(id)
    }

    pub fn remove(&mut self, id: &str) -> Result<bool> {
        let before = self.tasks.len();
        self.tasks.retain(|t| t.id != id);
        let removed = self.tasks.len() < before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Collect tasks whose next_run is due, ordered by next_run ascending.
    #[cfg(test)]
    pub fn due_tasks(&self) -> Vec<&ScheduledTask> {
        let now = chrono::Utc::now();
        let mut due: Vec<&ScheduledTask> = self
            .tasks
            .iter()
            .filter(|t| {
                chrono::DateTime::parse_from_rfc3339(&t.next_run)
                    .map(|dt| dt <= now)
                    .unwrap_or(false)
            })
            .collect();
        due.sort_by(|a, b| a.next_run.cmp(&b.next_run));
        due
    }

    /// Mark a task as completed (update next_run and last_run).
    pub fn complete_run(&mut self, id: &str) -> Result<bool> {
        let now = chrono::Utc::now();
        let now_str = now.to_rfc3339();
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) {
            task.last_run = Some(now_str.clone());
            task.run_count += 1;
            task.next_run = compute_next_run(&task.cron).unwrap_or(now_str);
            self.save()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn save(&self) -> Result<()> {
        let store = TaskStore {
            tasks: self.tasks.clone(),
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&store)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

fn load_tasks(path: &Path) -> Result<Vec<ScheduledTask>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)?;
    let store: TaskStore = serde_json::from_str(&raw)?;
    Ok(store.tasks)
}

/// Simple cron evaluator: supports `* * * * *` and `*/N * * * *` (minute-level only).
/// Returns next run time as RFC3339 string.
fn compute_next_run(cron: &str) -> Option<String> {
    let parts: Vec<&str> = cron.split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }

    let now = chrono::Utc::now();

    // Parse minute field
    let minute_interval = if parts[0] == "*" {
        1u32
    } else if let Some(n) = parts[0].strip_prefix("*/") {
        n.parse().ok()?
    } else {
        // Fixed minute
        let target_min: u32 = parts[0].parse().ok()?;
        let mut next = now;
        // Round to next occurrence of target_min
        let current_min = next.minute();
        if current_min < target_min {
            next = next.with_minute(target_min)?;
            next = next.with_second(0)?;
        } else {
            next = next.with_minute(target_min)?;
            next = next.with_second(0)?;
            next = next.checked_add_signed(chrono::Duration::hours(1))?;
        }
        return Some(next.to_rfc3339());
    };

    let next = now
        .with_second(0)
        .unwrap_or(now)
        .checked_add_signed(chrono::Duration::minutes(minute_interval as i64))?;
    Some(next.to_rfc3339())
}

fn new_id() -> String {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("sch_{}_{:08x}", ts, nanos)
}

#[cfg(test)]
mod tests {
    use super::{compute_next_run, Scheduler};

    #[test]
    fn compute_next_run_every_n() {
        let r = compute_next_run("*/5 * * * *");
        assert!(r.is_some());
    }

    #[test]
    fn compute_next_run_every_minute() {
        let r = compute_next_run("* * * * *");
        assert!(r.is_some());
    }

    #[test]
    fn compute_next_run_hourly() {
        let r = compute_next_run("0 * * * *");
        assert!(r.is_some());
    }

    #[test]
    fn compute_next_run_daily() {
        let r = compute_next_run("0 9 * * *");
        assert!(r.is_some());
    }

    #[test]
    fn compute_next_run_invalid() {
        assert!(compute_next_run("bad cron").is_none());
        assert!(compute_next_run("* * * *").is_none());
    }

    #[test]
    fn add_remove_task() {
        let dir = std::env::temp_dir().join(format!("rubot-sched-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let mut sched = Scheduler::new(&dir);
        let id = sched.add("test prompt", "*/5 * * * *").unwrap();
        assert_eq!(sched.all().len(), 1);
        assert!(sched.remove(&id).unwrap());
        assert_eq!(sched.all().len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn due_tasks_returns_past_due() {
        let dir = std::env::temp_dir().join(format!("rubot-sched-due-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let mut sched = Scheduler::new(&dir);
        let id = sched.add("due task", "* * * * *").unwrap();
        // Force next_run to the past
        if let Some(t) = sched.tasks.iter_mut().find(|t| t.id == id) {
            t.next_run = "2020-01-01T00:00:00+00:00".to_string();
        }
        let due = sched.due_tasks();
        assert_eq!(due.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
