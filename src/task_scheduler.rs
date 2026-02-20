use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, Local, Offset, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::cron_schedule::parse_cron_schedule;
use crate::db::Db;
use crate::env_file::read_env_var;
use crate::group_queue::GroupQueue;
use crate::types::{ScheduleType, ScheduledTask, TaskRunLog, TaskRunStatus, TaskStatus};

pub type TaskExecutionFuture<'a> = Pin<Box<dyn Future<Output = TaskExecution> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TaskExecution {
    pub result: Option<String>,
    pub error: Option<String>,
}

pub trait TaskExecutor: Send + Sync {
    fn execute_task<'a>(&'a self, task: &'a ScheduledTask) -> TaskExecutionFuture<'a>;
}

pub struct TaskScheduler {
    shutdown_tx: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl TaskScheduler {
    pub fn start(
        db: Db,
        queue: GroupQueue,
        executor: Arc<dyn TaskExecutor>,
        poll_interval: Duration,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(scheduler_loop(
            db,
            queue,
            executor,
            poll_interval,
            shutdown_rx,
        ));

        Self {
            shutdown_tx,
            handle,
        }
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.handle.await;
    }
}

async fn scheduler_loop(
    db: Db,
    queue: GroupQueue,
    executor: Arc<dyn TaskExecutor>,
    poll_interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        if let Err(err) = scheduler_tick(&db, &queue, executor.clone()).await {
            eprintln!("scheduler tick error: {err}");
        }

        tokio::select! {
            _ = sleep(poll_interval) => {}
            result = shutdown_rx.changed() => {
                if result.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }
}

async fn scheduler_tick(
    db: &Db,
    queue: &GroupQueue,
    executor: Arc<dyn TaskExecutor>,
) -> Result<(), String> {
    let due_tasks = db
        .get_due_tasks(Utc::now())
        .map_err(|err| format!("failed to query due tasks: {err}"))?;

    for task in due_tasks {
        let current_task = db
            .get_task_by_id(&task.id)
            .map_err(|err| format!("failed to load task {}: {err}", task.id))?;

        let Some(current_task) = current_task else {
            continue;
        };
        if current_task.status != TaskStatus::Active {
            continue;
        }

        let db_clone = db.clone();
        let executor_clone = executor.clone();
        queue.enqueue_task(
            current_task.chat_jid.clone(),
            current_task.id.clone(),
            move || async move {
                run_task(current_task, db_clone, executor_clone).await;
            },
        );
    }

    Ok(())
}

async fn run_task(task: ScheduledTask, db: Db, executor: Arc<dyn TaskExecutor>) {
    let start = Instant::now();
    let execution = executor.execute_task(&task).await;
    let run_at = Utc::now();

    let mut error = execution.error;
    let next_run = match compute_next_run(&task, run_at) {
        Ok(next_run) => next_run,
        Err(schedule_error) => {
            if error.is_none() {
                error = Some(schedule_error);
            }
            None
        }
    };

    let summary = match &error {
        Some(err) => format!("Error: {err}"),
        None => execution
            .result
            .as_deref()
            .map(truncate_summary)
            .unwrap_or_else(|| "Completed".to_string()),
    };

    let log = TaskRunLog {
        task_id: task.id.clone(),
        run_at,
        duration_ms: start.elapsed().as_millis() as u64,
        status: if error.is_some() {
            TaskRunStatus::Error
        } else {
            TaskRunStatus::Success
        },
        result: execution.result.clone(),
        error: error.clone(),
    };

    if let Err(err) = db.log_task_run(&log) {
        eprintln!("failed to write task run log: {err}");
    }

    if let Err(err) = db.update_task_after_run(&task.id, next_run, &summary, run_at) {
        eprintln!("failed to update task after run: {err}");
    }
}

fn compute_next_run(
    task: &ScheduledTask,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, String> {
    match task.schedule_type {
        ScheduleType::Once => Ok(None),
        ScheduleType::Interval => {
            let interval_ms = task
                .schedule_value
                .parse::<i64>()
                .map_err(|_| format!("invalid interval value: {}", task.schedule_value))?;
            if interval_ms <= 0 {
                return Err(format!(
                    "interval must be positive milliseconds: {}",
                    task.schedule_value
                ));
            }
            Ok(Some(now + ChronoDuration::milliseconds(interval_ms)))
        }
        ScheduleType::Cron => {
            let schedule = parse_cron_schedule(&task.schedule_value)?;
            let timezone = resolve_task_timezone();
            Ok(next_cron_run(&schedule, now, timezone))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskTimezone {
    Named(Tz),
    Fixed(FixedOffset),
}

fn next_cron_run(
    schedule: &Schedule,
    now: DateTime<Utc>,
    timezone: TaskTimezone,
) -> Option<DateTime<Utc>> {
    match timezone {
        TaskTimezone::Named(tz) => schedule
            .after(&now.with_timezone(&tz))
            .next()
            .map(|next| next.with_timezone(&Utc)),
        TaskTimezone::Fixed(offset) => schedule
            .after(&now.with_timezone(&offset))
            .next()
            .map(|next| next.with_timezone(&Utc)),
    }
}

fn resolve_task_timezone() -> TaskTimezone {
    // Use a single timezone key for predictable deployment behavior.
    let raw_timezone = read_env_var("TIMEZONE");
    resolve_task_timezone_from_sources(
        raw_timezone,
        iana_time_zone::get_timezone().ok(),
        Local::now().offset().fix(),
    )
}

fn resolve_task_timezone_from_sources(
    raw_timezone: Option<String>,
    system_timezone_name: Option<String>,
    local_offset: FixedOffset,
) -> TaskTimezone {
    if let Some(raw) = raw_timezone {
        if let Ok(tz) = raw.parse::<Tz>() {
            return TaskTimezone::Named(tz);
        }
        if let Some(offset) = parse_fixed_offset(&raw) {
            return TaskTimezone::Fixed(offset);
        }
    }

    if let Some(system_timezone_name) = system_timezone_name {
        if let Ok(tz) = system_timezone_name.parse::<Tz>() {
            return TaskTimezone::Named(tz);
        }
    }

    TaskTimezone::Fixed(local_offset)
}

fn parse_fixed_offset(raw: &str) -> Option<FixedOffset> {
    let text = raw.trim();
    if text.len() != 6 {
        return None;
    }
    let sign = match &text[0..1] {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    if &text[3..4] != ":" {
        return None;
    }
    let hours = text[1..3].parse::<i32>().ok()?;
    let minutes = text[4..6].parse::<i32>().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }

    let total = sign * (hours * 3600 + minutes * 60);
    FixedOffset::east_opt(total)
}

fn truncate_summary(value: &str) -> String {
    const MAX_SUMMARY_LEN: usize = 200;
    value.chars().take(MAX_SUMMARY_LEN).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use chrono::TimeZone;

    use tokio::time::sleep;

    use super::*;
    use crate::types::{ContextMode, ScheduleType, TaskStatus};

    #[derive(Default)]
    struct MockExecutor {
        calls: Arc<Mutex<Vec<String>>>,
        responses: Arc<Mutex<HashMap<String, TaskExecution>>>,
    }

    impl MockExecutor {
        fn with_response(self, task_id: &str, response: TaskExecution) -> Self {
            self.responses
                .lock()
                .expect("responses lock")
                .insert(task_id.to_string(), response);
            self
        }

        fn call_count(&self) -> usize {
            self.calls.lock().expect("calls lock").len()
        }
    }

    impl TaskExecutor for MockExecutor {
        fn execute_task<'a>(&'a self, task: &'a ScheduledTask) -> TaskExecutionFuture<'a> {
            let task_id = task.id.clone();
            self.calls.lock().expect("calls lock").push(task_id.clone());
            let response = self
                .responses
                .lock()
                .expect("responses lock")
                .remove(&task_id)
                .unwrap_or_default();

            Box::pin(async move { response })
        }
    }

    fn build_task(
        id: &str,
        schedule_type: ScheduleType,
        schedule_value: &str,
        next_run: Option<DateTime<Utc>>,
        status: TaskStatus,
    ) -> ScheduledTask {
        ScheduledTask {
            id: id.to_string(),
            group_folder: "main".to_string(),
            chat_jid: "group@g.us".to_string(),
            prompt: "prompt".to_string(),
            schedule_type,
            schedule_value: schedule_value.to_string(),
            context_mode: ContextMode::Isolated,
            next_run,
            last_run: None,
            last_result: None,
            status,
            created_at: Utc::now(),
        }
    }

    async fn wait_until<F>(timeout: Duration, condition: F) -> bool
    where
        F: Fn() -> bool,
    {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if condition() {
                return true;
            }
            sleep(Duration::from_millis(2)).await;
        }
        condition()
    }

    #[tokio::test]
    async fn scheduler_runs_due_interval_task_and_keeps_it_active() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(temp_dir.path().join("messages.db")).expect("db open");
        let due_at = Utc::now() - ChronoDuration::seconds(1);
        db.create_task(&build_task(
            "task-1",
            ScheduleType::Interval,
            "50",
            Some(due_at),
            TaskStatus::Active,
        ))
        .expect("create task");

        let executor = Arc::new(MockExecutor::default().with_response(
            "task-1",
            TaskExecution {
                result: Some("done".to_string()),
                error: None,
            },
        ));
        let scheduler = TaskScheduler::start(
            db.clone(),
            GroupQueue::new(1),
            executor.clone(),
            Duration::from_millis(10),
        );

        assert!(wait_until(Duration::from_millis(200), || executor.call_count() >= 1).await);
        scheduler.shutdown().await;

        let updated = db
            .get_task_by_id("task-1")
            .expect("get task")
            .expect("task exists");
        assert_eq!(updated.status, TaskStatus::Active);
        assert!(updated.next_run.is_some());
        assert_eq!(updated.last_result.as_deref(), Some("done"));

        let logs = db.get_task_run_logs("task-1").expect("load logs");
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].status, TaskRunStatus::Success);
    }

    #[tokio::test]
    async fn scheduler_skips_paused_tasks() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(temp_dir.path().join("messages.db")).expect("db open");
        let due_at = Utc::now() - ChronoDuration::seconds(1);
        db.create_task(&build_task(
            "paused-task",
            ScheduleType::Interval,
            "50",
            Some(due_at),
            TaskStatus::Paused,
        ))
        .expect("create paused task");

        let executor = Arc::new(MockExecutor::default());
        let scheduler = TaskScheduler::start(
            db,
            GroupQueue::new(1),
            executor.clone(),
            Duration::from_millis(10),
        );

        sleep(Duration::from_millis(60)).await;
        scheduler.shutdown().await;

        assert_eq!(executor.call_count(), 0);
    }

    #[tokio::test]
    async fn once_tasks_complete_after_execution() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(temp_dir.path().join("messages.db")).expect("db open");
        let due_at = Utc::now() - ChronoDuration::seconds(1);
        db.create_task(&build_task(
            "once-task",
            ScheduleType::Once,
            "once",
            Some(due_at),
            TaskStatus::Active,
        ))
        .expect("create once task");

        let executor = Arc::new(MockExecutor::default());
        let scheduler = TaskScheduler::start(
            db.clone(),
            GroupQueue::new(1),
            executor,
            Duration::from_millis(10),
        );

        assert!(
            wait_until(Duration::from_millis(200), || {
                db.get_task_by_id("once-task")
                    .expect("get once task")
                    .expect("once task exists")
                    .status
                    == TaskStatus::Completed
            })
            .await
        );
        scheduler.shutdown().await;

        let updated = db
            .get_task_by_id("once-task")
            .expect("get task")
            .expect("task exists");
        assert_eq!(updated.status, TaskStatus::Completed);
        assert!(updated.next_run.is_none());
    }

    #[test]
    fn parse_fixed_offset_accepts_valid_and_rejects_invalid_values() {
        let offset = parse_fixed_offset("+09:00").expect("parse +09:00");
        assert_eq!(offset.local_minus_utc(), 9 * 3600);
        assert!(parse_fixed_offset("UTC").is_none());
        assert!(parse_fixed_offset("+99:00").is_none());
    }

    #[test]
    fn cron_next_run_respects_timezone_offset() {
        let schedule = parse_cron_schedule("0 9 * * *").expect("cron schedule");
        let now = Utc.with_ymd_and_hms(2026, 2, 19, 0, 0, 0).unwrap();

        let utc_next = next_cron_run(
            &schedule,
            now,
            TaskTimezone::Fixed(FixedOffset::east_opt(0).expect("utc offset")),
        )
        .expect("utc next");
        let plus_nine_next = next_cron_run(
            &schedule,
            now,
            TaskTimezone::Fixed(FixedOffset::east_opt(9 * 3600).expect("+09 offset")),
        )
        .expect("plus nine next");

        assert_eq!(
            utc_next,
            Utc.with_ymd_and_hms(2026, 2, 19, 9, 0, 0).unwrap()
        );
        assert_eq!(
            plus_nine_next,
            Utc.with_ymd_and_hms(2026, 2, 20, 0, 0, 0).unwrap()
        );
    }

    #[test]
    fn resolve_task_timezone_uses_system_named_timezone_when_env_is_missing() {
        let timezone = resolve_task_timezone_from_sources(
            None,
            Some("America/New_York".to_string()),
            FixedOffset::east_opt(0).expect("utc offset"),
        );
        assert_eq!(
            timezone,
            TaskTimezone::Named("America/New_York".parse::<Tz>().expect("tz parse"))
        );
    }

    #[test]
    fn resolve_task_timezone_falls_back_to_local_offset_when_system_timezone_is_unknown() {
        let local_offset = FixedOffset::east_opt(9 * 3600).expect("+09 offset");
        let timezone = resolve_task_timezone_from_sources(
            None,
            Some("Not/A_Real_Zone".to_string()),
            local_offset,
        );
        assert_eq!(timezone, TaskTimezone::Fixed(local_offset));
    }
}
