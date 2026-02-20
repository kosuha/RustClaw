use std::collections::{HashMap, VecDeque};
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::time::sleep;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
type ProcessMessagesFn = Arc<dyn Fn(String) -> BoxFuture<bool> + Send + Sync>;
type TaskFn = Box<dyn FnOnce() -> BoxFuture<()> + Send + 'static>;

const DEFAULT_MAX_CONCURRENT: usize = 5;
const DEFAULT_MAX_RETRIES: u32 = 5;
const DEFAULT_BASE_RETRY_MS: u64 = 5_000;

struct QueuedTask {
    id: String,
    run: TaskFn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActiveRunKind {
    Messages,
    Task,
}

#[derive(Default)]
struct GroupState {
    active: bool,
    active_run: Option<ActiveRunKind>,
    active_task_id: Option<String>,
    pending_messages: bool,
    pending_tasks: VecDeque<QueuedTask>,
    process_id: Option<u32>,
    container_name: Option<String>,
    group_folder: Option<String>,
    retry_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveContainerContext {
    pub active: bool,
    pub process_id: Option<u32>,
    pub container_name: Option<String>,
    pub group_folder: Option<String>,
}

#[derive(Default)]
struct Inner {
    groups: HashMap<String, GroupState>,
    active_count: usize,
    waiting_groups: VecDeque<String>,
    shutting_down: bool,
}

impl Inner {
    fn group_mut(&mut self, group_jid: &str) -> &mut GroupState {
        self.groups.entry(group_jid.to_string()).or_default()
    }

    fn push_waiting_group(&mut self, group_jid: &str) {
        if !self.waiting_groups.iter().any(|jid| jid == group_jid) {
            self.waiting_groups.push_back(group_jid.to_string());
        }
    }
}

enum QueueAction {
    RunMessages { group_jid: String },
    RunTask { group_jid: String, task: QueuedTask },
    ScheduleRetry { group_jid: String, delay: Duration },
}

#[derive(Clone)]
pub struct GroupQueue {
    inner: Arc<Mutex<Inner>>,
    process_messages: Arc<RwLock<Option<ProcessMessagesFn>>>,
    ipc_base_dir: Arc<RwLock<Option<PathBuf>>>,
    max_concurrent: usize,
    base_retry: Duration,
    max_retries: u32,
}

impl Default for GroupQueue {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_CONCURRENT)
    }
}

impl GroupQueue {
    pub fn new(max_concurrent: usize) -> Self {
        Self::with_retry_config(
            max_concurrent,
            Duration::from_millis(DEFAULT_BASE_RETRY_MS),
            DEFAULT_MAX_RETRIES,
        )
    }

    pub fn with_retry_config(
        max_concurrent: usize,
        base_retry: Duration,
        max_retries: u32,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            process_messages: Arc::new(RwLock::new(None)),
            ipc_base_dir: Arc::new(RwLock::new(None)),
            max_concurrent: max_concurrent.max(1),
            base_retry,
            max_retries,
        }
    }

    pub fn set_process_messages_fn<F, Fut>(&self, callback: F)
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = bool> + Send + 'static,
    {
        let wrapped: ProcessMessagesFn =
            Arc::new(move |group_jid: String| -> BoxFuture<bool> { Box::pin(callback(group_jid)) });
        *self
            .process_messages
            .write()
            .expect("set_process_messages_fn lock") = Some(wrapped);
    }

    pub fn set_ipc_base_dir(&self, path: impl Into<PathBuf>) {
        *self.ipc_base_dir.write().expect("set_ipc_base_dir lock") = Some(path.into());
    }

    pub fn register_active_group_folder(&self, group_jid: &str, group_folder: &str) {
        self.register_active_container(group_jid, group_folder, None, None);
    }

    pub fn register_active_container(
        &self,
        group_jid: &str,
        group_folder: &str,
        process_id: Option<u32>,
        container_name: Option<String>,
    ) {
        let mut inner = self.inner.lock().expect("register_active_container lock");
        let state = inner.group_mut(group_jid);
        if state.active {
            state.group_folder = Some(group_folder.to_string());
            state.process_id = process_id;
            state.container_name = container_name;
        }
    }

    pub fn active_container_context(&self, group_jid: &str) -> Option<ActiveContainerContext> {
        let inner = self.inner.lock().expect("active_container_context lock");
        inner
            .groups
            .get(group_jid)
            .map(|state| ActiveContainerContext {
                active: state.active,
                process_id: state.process_id,
                container_name: state.container_name.clone(),
                group_folder: state.group_folder.clone(),
            })
    }

    pub fn send_message(&self, group_jid: &str, text: &str) -> bool {
        let group_folder = {
            let inner = self.inner.lock().expect("send_message lock");
            let Some(state) = inner.groups.get(group_jid) else {
                return false;
            };
            if !state.active {
                return false;
            }
            state.group_folder.clone()
        };

        let Some(group_folder) = group_folder else {
            return false;
        };
        let Some(input_dir) = self.input_dir_for(&group_folder) else {
            return false;
        };

        if fs::create_dir_all(&input_dir).is_err() {
            return false;
        }

        let file_name = format!("{}.json", unique_ipc_name());
        let file_path = input_dir.join(file_name);
        let temp_path = file_path.with_extension("json.tmp");
        let payload = json!({
            "type": "message",
            "text": text
        })
        .to_string();

        if fs::write(&temp_path, payload).is_err() {
            return false;
        }
        fs::rename(&temp_path, &file_path).is_ok()
    }

    pub fn close_stdin(&self, group_jid: &str) -> bool {
        let group_folder = {
            let inner = self.inner.lock().expect("close_stdin lock");
            let Some(state) = inner.groups.get(group_jid) else {
                return false;
            };
            if !state.active {
                return false;
            }
            state.group_folder.clone()
        };

        let Some(group_folder) = group_folder else {
            return false;
        };
        let Some(input_dir) = self.input_dir_for(&group_folder) else {
            return false;
        };

        if fs::create_dir_all(&input_dir).is_err() {
            return false;
        }

        fs::write(input_dir.join("_close"), "").is_ok()
    }

    pub fn enqueue_message_check(&self, group_jid: impl Into<String>) {
        let group_jid = group_jid.into();
        let next_action = {
            let mut inner = self.inner.lock().expect("enqueue_message_check lock");
            if inner.shutting_down {
                return;
            }

            let at_limit = inner.active_count >= self.max_concurrent;
            let mut run_now = false;
            {
                let state = inner.group_mut(&group_jid);
                if state.active {
                    state.pending_messages = true;
                } else if at_limit {
                    state.pending_messages = true;
                } else {
                    state.active = true;
                    state.active_run = Some(ActiveRunKind::Messages);
                    state.active_task_id = None;
                    state.pending_messages = false;
                    run_now = true;
                }
            }

            if at_limit {
                inner.push_waiting_group(&group_jid);
            }

            if run_now {
                inner.active_count += 1;
                Some(QueueAction::RunMessages { group_jid })
            } else {
                None
            }
        };

        if let Some(action) = next_action {
            self.execute_action(action);
        }
    }

    pub fn enqueue_task<F, Fut>(
        &self,
        group_jid: impl Into<String>,
        task_id: impl Into<String>,
        run: F,
    ) where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let group_jid = group_jid.into();
        let task_id = task_id.into();
        let mut task = Some(QueuedTask {
            id: task_id.clone(),
            run: Box::new(move || -> BoxFuture<()> { Box::pin(run()) }),
        });

        let (next_action, close_active_message_stdin) = {
            let mut inner = self.inner.lock().expect("enqueue_task lock");
            if inner.shutting_down {
                return;
            }

            let at_limit = inner.active_count >= self.max_concurrent;
            let mut run_now = false;
            let mut duplicate = false;
            let mut push_waiting = false;
            let mut close_active_message_stdin = false;
            {
                let state = inner.group_mut(&group_jid);
                if state
                    .pending_tasks
                    .iter()
                    .any(|queued| queued.id == task_id)
                    || state.active_task_id.as_deref() == Some(task_id.as_str())
                {
                    duplicate = true;
                } else if state.active {
                    state
                        .pending_tasks
                        .push_back(task.take().expect("task exists for active group"));
                    if state.active_run == Some(ActiveRunKind::Messages) {
                        close_active_message_stdin = true;
                    }
                } else if at_limit {
                    state
                        .pending_tasks
                        .push_back(task.take().expect("task exists at concurrency limit"));
                    push_waiting = true;
                } else {
                    state.active = true;
                    state.active_run = Some(ActiveRunKind::Task);
                    state.active_task_id = Some(task_id.clone());
                    run_now = true;
                }
            }

            if push_waiting {
                inner.push_waiting_group(&group_jid);
            }

            if duplicate {
                (None, close_active_message_stdin)
            } else if run_now {
                inner.active_count += 1;
                (
                    Some(QueueAction::RunTask {
                        group_jid: group_jid.clone(),
                        task: task.take().expect("task exists for immediate run"),
                    }),
                    close_active_message_stdin,
                )
            } else {
                (None, close_active_message_stdin)
            }
        };

        if close_active_message_stdin {
            let _ = self.close_stdin(&group_jid);
        }
        if let Some(action) = next_action {
            self.execute_action(action);
        }
    }

    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().expect("shutdown lock");
        inner.shutting_down = true;
        let active_runs = inner.groups.values().filter(|state| state.active).count();
        let detached_containers = inner
            .groups
            .values()
            .filter(|state| state.active)
            .filter_map(|state| state.container_name.clone())
            .collect::<Vec<_>>();
        eprintln!(
            "group queue shutdown: new enqueues stopped; active runs are detached (active_runs={active_runs}, tracked_containers={})",
            detached_containers.len()
        );
        if !detached_containers.is_empty() {
            eprintln!(
                "group queue shutdown: detaching active containers: {}",
                detached_containers.join(", ")
            );
        }
    }

    fn execute_actions(&self, actions: Vec<QueueAction>) {
        for action in actions {
            self.execute_action(action);
        }
    }

    fn execute_action(&self, action: QueueAction) {
        match action {
            QueueAction::RunMessages { group_jid } => {
                let queue = self.clone();
                let callback = self
                    .process_messages
                    .read()
                    .expect("run messages callback lock")
                    .clone();
                tokio::spawn(async move {
                    let success = match callback {
                        Some(process_messages) => process_messages(group_jid.clone()).await,
                        None => true,
                    };
                    queue.finish_messages_run(group_jid, success);
                });
            }
            QueueAction::RunTask { group_jid, task } => {
                let queue = self.clone();
                tokio::spawn(async move {
                    (task.run)().await;
                    queue.finish_task_run(group_jid);
                });
            }
            QueueAction::ScheduleRetry { group_jid, delay } => {
                let queue = self.clone();
                tokio::spawn(async move {
                    sleep(delay).await;
                    queue.enqueue_message_check(group_jid);
                });
            }
        }
    }

    fn finish_messages_run(&self, group_jid: String, success: bool) {
        let actions = {
            let mut inner = self.inner.lock().expect("finish_messages_run lock");
            inner.active_count = inner.active_count.saturating_sub(1);

            let retry_action = if let Some(state) = inner.groups.get_mut(&group_jid) {
                state.active = false;
                clear_active_context(state);
                if success {
                    state.retry_count = 0;
                    None
                } else {
                    self.compute_retry_action(&group_jid, state)
                }
            } else {
                None
            };

            let mut actions = self.drain_group_locked(&mut inner, &group_jid);
            if let Some(retry_action) = retry_action {
                actions.push(retry_action);
            }
            actions
        };

        self.execute_actions(actions);
    }

    fn finish_task_run(&self, group_jid: String) {
        let actions = {
            let mut inner = self.inner.lock().expect("finish_task_run lock");
            inner.active_count = inner.active_count.saturating_sub(1);
            if let Some(state) = inner.groups.get_mut(&group_jid) {
                state.active = false;
                clear_active_context(state);
            }
            self.drain_group_locked(&mut inner, &group_jid)
        };
        self.execute_actions(actions);
    }

    fn compute_retry_action(&self, group_jid: &str, state: &mut GroupState) -> Option<QueueAction> {
        state.retry_count = state.retry_count.saturating_add(1);
        if state.retry_count > self.max_retries {
            state.retry_count = 0;
            return None;
        }

        let multiplier = 2u32.saturating_pow(state.retry_count.saturating_sub(1));
        let delay = self
            .base_retry
            .checked_mul(multiplier)
            .unwrap_or(self.base_retry);

        Some(QueueAction::ScheduleRetry {
            group_jid: group_jid.to_string(),
            delay,
        })
    }

    fn drain_group_locked(&self, inner: &mut Inner, group_jid: &str) -> Vec<QueueAction> {
        if inner.shutting_down {
            return Vec::new();
        }

        let mut actions = Vec::new();
        let primary_action = if let Some(state) = inner.groups.get_mut(group_jid) {
            if state.active {
                None
            } else if let Some(task) = state.pending_tasks.pop_front() {
                state.active = true;
                state.active_run = Some(ActiveRunKind::Task);
                state.active_task_id = Some(task.id.clone());
                Some(QueueAction::RunTask {
                    group_jid: group_jid.to_string(),
                    task,
                })
            } else if state.pending_messages {
                state.pending_messages = false;
                state.active = true;
                state.active_run = Some(ActiveRunKind::Messages);
                state.active_task_id = None;
                Some(QueueAction::RunMessages {
                    group_jid: group_jid.to_string(),
                })
            } else {
                None
            }
        } else {
            None
        };

        if let Some(primary_action) = primary_action {
            inner.active_count += 1;
            actions.push(primary_action);
        }

        actions.extend(self.drain_waiting_locked(inner));
        actions
    }

    fn drain_waiting_locked(&self, inner: &mut Inner) -> Vec<QueueAction> {
        let mut actions = Vec::new();

        while inner.active_count < self.max_concurrent {
            let Some(next_group_jid) = inner.waiting_groups.pop_front() else {
                break;
            };

            let waiting_action = if let Some(state) = inner.groups.get_mut(&next_group_jid) {
                if state.active {
                    None
                } else if let Some(task) = state.pending_tasks.pop_front() {
                    state.active = true;
                    state.active_run = Some(ActiveRunKind::Task);
                    state.active_task_id = Some(task.id.clone());
                    Some(QueueAction::RunTask {
                        group_jid: next_group_jid.clone(),
                        task,
                    })
                } else if state.pending_messages {
                    state.pending_messages = false;
                    state.active = true;
                    state.active_run = Some(ActiveRunKind::Messages);
                    state.active_task_id = None;
                    Some(QueueAction::RunMessages {
                        group_jid: next_group_jid.clone(),
                    })
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(waiting_action) = waiting_action {
                inner.active_count += 1;
                actions.push(waiting_action);
            }
        }

        actions
    }

    fn input_dir_for(&self, group_folder: &str) -> Option<PathBuf> {
        self.ipc_base_dir
            .read()
            .expect("input_dir_for lock")
            .as_ref()
            .map(|base| base.join(group_folder).join("input"))
    }
}

fn clear_active_context(state: &mut GroupState) {
    state.active_run = None;
    state.active_task_id = None;
    state.process_id = None;
    state.container_name = None;
    state.group_folder = None;
}

fn unique_ipc_name() -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("{now_ms}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::Value as JsonValue;
    use tokio::sync::oneshot;
    use tokio::time::sleep;

    use super::{ActiveContainerContext, GroupQueue};

    fn update_max(max: &AtomicUsize, candidate: usize) {
        let mut current = max.load(Ordering::SeqCst);
        while candidate > current {
            match max.compare_exchange(current, candidate, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
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
    async fn only_runs_one_container_per_group() {
        let queue = GroupQueue::with_retry_config(2, Duration::from_millis(5), 5);
        let current = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));

        queue.set_process_messages_fn({
            let current = current.clone();
            let max = max.clone();
            let calls = calls.clone();
            move |_| {
                let current = current.clone();
                let max = max.clone();
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let active_now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    update_max(&max, active_now);
                    sleep(Duration::from_millis(25)).await;
                    current.fetch_sub(1, Ordering::SeqCst);
                    true
                }
            }
        });

        queue.enqueue_message_check("group1@g.us");
        queue.enqueue_message_check("group1@g.us");

        assert!(
            wait_until(Duration::from_millis(120), || {
                calls.load(Ordering::SeqCst) >= 2
            })
            .await
        );
        assert_eq!(max.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn respects_global_concurrency_limit() {
        let queue = GroupQueue::with_retry_config(2, Duration::from_millis(5), 5);
        let active = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let releases = Arc::new(Mutex::new(Vec::<oneshot::Sender<()>>::new()));

        queue.set_process_messages_fn({
            let active = active.clone();
            let max = max.clone();
            let calls = calls.clone();
            let releases = releases.clone();
            move |_| {
                let active = active.clone();
                let max = max.clone();
                let calls = calls.clone();
                let releases = releases.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let active_now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    update_max(&max, active_now);
                    let (tx, rx) = oneshot::channel();
                    releases.lock().expect("releases lock").push(tx);
                    let _ = rx.await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    true
                }
            }
        });

        queue.enqueue_message_check("group1@g.us");
        queue.enqueue_message_check("group2@g.us");
        queue.enqueue_message_check("group3@g.us");

        assert!(
            wait_until(Duration::from_millis(100), || {
                active.load(Ordering::SeqCst) == 2
            })
            .await
        );
        assert_eq!(max.load(Ordering::SeqCst), 2);

        let first_release = loop {
            if let Some(sender) = releases.lock().expect("releases lock").pop() {
                break sender;
            }
            sleep(Duration::from_millis(2)).await;
        };
        let _ = first_release.send(());

        assert!(
            wait_until(Duration::from_millis(100), || {
                calls.load(Ordering::SeqCst) >= 3
            })
            .await
        );

        for sender in releases.lock().expect("releases lock").drain(..) {
            let _ = sender.send(());
        }
    }

    #[tokio::test]
    async fn drains_tasks_before_messages_for_same_group() {
        let queue = GroupQueue::with_retry_config(1, Duration::from_millis(5), 5);
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let call_index = Arc::new(AtomicUsize::new(0));
        let (gate_tx, gate_rx) = oneshot::channel::<()>();
        let gate = Arc::new(Mutex::new(Some(gate_rx)));

        queue.set_process_messages_fn({
            let events = events.clone();
            let call_index = call_index.clone();
            let gate = gate.clone();
            move |_| {
                let events = events.clone();
                let call_index = call_index.clone();
                let gate = gate.clone();
                async move {
                    let idx = call_index.fetch_add(1, Ordering::SeqCst);
                    if idx == 0 {
                        let maybe_rx = gate.lock().expect("gate lock").take();
                        if let Some(rx) = maybe_rx {
                            let _ = rx.await;
                        }
                    }
                    events
                        .lock()
                        .expect("events lock")
                        .push("messages".to_string());
                    true
                }
            }
        });

        queue.enqueue_message_check("group1@g.us");
        assert!(
            wait_until(Duration::from_millis(50), || {
                call_index.load(Ordering::SeqCst) >= 1
            })
            .await
        );

        queue.enqueue_task("group1@g.us", "task-1", {
            let events = events.clone();
            move || {
                let events = events.clone();
                async move {
                    events.lock().expect("events lock").push("task".to_string());
                }
            }
        });
        queue.enqueue_message_check("group1@g.us");

        let _ = gate_tx.send(());

        assert!(
            wait_until(Duration::from_millis(120), || {
                events.lock().expect("events lock").len() >= 3
            })
            .await
        );

        let snapshot = events.lock().expect("events lock").clone();
        assert_eq!(snapshot[0], "messages");
        assert_eq!(snapshot[1], "task");
        assert_eq!(snapshot[2], "messages");
    }

    #[tokio::test]
    async fn retries_with_exponential_backoff_on_failure() {
        let queue = GroupQueue::with_retry_config(1, Duration::from_millis(5), 5);
        let calls = Arc::new(AtomicUsize::new(0));

        queue.set_process_messages_fn({
            let calls = calls.clone();
            move |_| {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    false
                }
            }
        });

        queue.enqueue_message_check("group1@g.us");

        assert!(
            wait_until(Duration::from_millis(120), || {
                calls.load(Ordering::SeqCst) >= 4
            })
            .await
        );
    }

    #[tokio::test]
    async fn prevents_new_enqueues_after_shutdown() {
        let queue = GroupQueue::with_retry_config(1, Duration::from_millis(5), 5);
        let calls = Arc::new(AtomicUsize::new(0));

        queue.set_process_messages_fn({
            let calls = calls.clone();
            move |_| {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    true
                }
            }
        });

        queue.shutdown();
        queue.enqueue_message_check("group1@g.us");
        sleep(Duration::from_millis(20)).await;

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stops_retrying_after_max_retries() {
        let queue = GroupQueue::with_retry_config(1, Duration::from_millis(5), 2);
        let calls = Arc::new(AtomicUsize::new(0));

        queue.set_process_messages_fn({
            let calls = calls.clone();
            move |_| {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    false
                }
            }
        });

        queue.enqueue_message_check("group1@g.us");
        assert!(
            wait_until(Duration::from_millis(60), || {
                calls.load(Ordering::SeqCst) >= 3
            })
            .await
        );

        sleep(Duration::from_millis(50)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn drains_waiting_groups_when_slots_free_up() {
        let queue = GroupQueue::with_retry_config(2, Duration::from_millis(5), 5);
        let started = Arc::new(Mutex::new(Vec::<String>::new()));
        let releases = Arc::new(Mutex::new(Vec::<oneshot::Sender<()>>::new()));

        queue.set_process_messages_fn({
            let started = started.clone();
            let releases = releases.clone();
            move |jid| {
                let started = started.clone();
                let releases = releases.clone();
                async move {
                    started.lock().expect("started lock").push(jid);
                    let (tx, rx) = oneshot::channel();
                    releases.lock().expect("releases lock").push(tx);
                    let _ = rx.await;
                    true
                }
            }
        });

        queue.enqueue_message_check("group1@g.us");
        queue.enqueue_message_check("group2@g.us");
        queue.enqueue_message_check("group3@g.us");

        assert!(
            wait_until(Duration::from_millis(80), || {
                started.lock().expect("started lock").len() >= 2
            })
            .await
        );

        let first_release = loop {
            if let Some(sender) = releases.lock().expect("releases lock").pop() {
                break sender;
            }
            sleep(Duration::from_millis(2)).await;
        };
        let _ = first_release.send(());

        assert!(
            wait_until(Duration::from_millis(100), || {
                started
                    .lock()
                    .expect("started lock")
                    .iter()
                    .any(|jid| jid == "group3@g.us")
            })
            .await
        );

        for sender in releases.lock().expect("releases lock").drain(..) {
            let _ = sender.send(());
        }
    }

    #[tokio::test]
    async fn send_message_writes_ipc_json_when_group_is_active() {
        let queue = GroupQueue::with_retry_config(1, Duration::from_millis(5), 1);
        let temp_dir = tempfile::tempdir().expect("tempdir");
        queue.set_ipc_base_dir(temp_dir.path().to_path_buf());

        let (tx, rx) = oneshot::channel::<()>();
        let hold = Arc::new(Mutex::new(Some(rx)));
        queue.set_process_messages_fn({
            let hold = hold.clone();
            move |_| {
                let hold = hold.clone();
                async move {
                    let rx = { hold.lock().expect("hold lock").take() };
                    if let Some(rx) = rx {
                        let _ = rx.await;
                    }
                    true
                }
            }
        });

        queue.enqueue_message_check("group@g.us");
        queue.register_active_group_folder("group@g.us", "dev-team");
        assert!(queue.send_message("group@g.us", "<messages>hello</messages>"));

        let input_dir = temp_dir.path().join("dev-team").join("input");
        let mut message_files = fs::read_dir(&input_dir)
            .expect("read input dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
            .collect::<Vec<_>>();
        assert_eq!(message_files.len(), 1);

        let payload = fs::read_to_string(message_files.pop().expect("message file").path())
            .expect("read payload");
        let json: JsonValue = serde_json::from_str(&payload).expect("parse payload");
        assert_eq!(
            json.get("type").and_then(JsonValue::as_str),
            Some("message")
        );
        assert_eq!(
            json.get("text").and_then(JsonValue::as_str),
            Some("<messages>hello</messages>")
        );

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn close_stdin_writes_close_sentinel_for_active_group() {
        let queue = GroupQueue::with_retry_config(1, Duration::from_millis(5), 1);
        let temp_dir = tempfile::tempdir().expect("tempdir");
        queue.set_ipc_base_dir(temp_dir.path().to_path_buf());

        let (tx, rx) = oneshot::channel::<()>();
        let hold = Arc::new(Mutex::new(Some(rx)));
        queue.set_process_messages_fn({
            let hold = hold.clone();
            move |_| {
                let hold = hold.clone();
                async move {
                    let rx = { hold.lock().expect("hold lock").take() };
                    if let Some(rx) = rx {
                        let _ = rx.await;
                    }
                    true
                }
            }
        });

        queue.enqueue_message_check("group@g.us");
        queue.register_active_group_folder("group@g.us", "dev-team");
        assert!(queue.close_stdin("group@g.us"));

        let close_file = temp_dir
            .path()
            .join("dev-team")
            .join("input")
            .join("_close");
        assert!(close_file.exists());
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn enqueue_task_closes_active_message_stdin_to_unblock_task() {
        let queue = GroupQueue::with_retry_config(1, Duration::from_millis(5), 1);
        let temp_dir = tempfile::tempdir().expect("tempdir");
        queue.set_ipc_base_dir(temp_dir.path().to_path_buf());

        let (tx, rx) = oneshot::channel::<()>();
        let hold = Arc::new(Mutex::new(Some(rx)));
        queue.set_process_messages_fn({
            let hold = hold.clone();
            move |_| {
                let hold = hold.clone();
                async move {
                    let rx = { hold.lock().expect("hold lock").take() };
                    if let Some(rx) = rx {
                        let _ = rx.await;
                    }
                    true
                }
            }
        });

        let task_ran = Arc::new(AtomicBool::new(false));
        queue.enqueue_message_check("group@g.us");
        queue.register_active_group_folder("group@g.us", "dev-team");
        queue.enqueue_task("group@g.us", "task-1", {
            let task_ran = task_ran.clone();
            move || async move {
                task_ran.store(true, Ordering::SeqCst);
            }
        });

        let close_file = temp_dir
            .path()
            .join("dev-team")
            .join("input")
            .join("_close");
        assert!(
            wait_until(Duration::from_millis(80), || close_file.exists()).await,
            "expected _close sentinel to be written for active message container"
        );

        let _ = tx.send(());
        assert!(
            wait_until(Duration::from_millis(120), || task_ran
                .load(Ordering::SeqCst))
            .await,
            "expected queued task to run after active message container closes"
        );
    }

    #[tokio::test]
    async fn enqueue_task_ignores_duplicate_when_same_task_is_already_active() {
        let queue = GroupQueue::with_retry_config(1, Duration::from_millis(5), 1);
        let run_count = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let hold = Arc::new(Mutex::new(Some(release_rx)));

        queue.enqueue_task("group@g.us", "task-1", {
            let run_count = run_count.clone();
            let hold = hold.clone();
            move || {
                let run_count = run_count.clone();
                let hold = hold.clone();
                async move {
                    run_count.fetch_add(1, Ordering::SeqCst);
                    let maybe_rx = hold.lock().expect("hold lock").take();
                    if let Some(rx) = maybe_rx {
                        let _ = rx.await;
                    }
                }
            }
        });

        assert!(
            wait_until(Duration::from_millis(80), || run_count
                .load(Ordering::SeqCst)
                == 1)
            .await
        );

        queue.enqueue_task("group@g.us", "task-1", {
            let run_count = run_count.clone();
            move || {
                let run_count = run_count.clone();
                async move {
                    run_count.fetch_add(100, Ordering::SeqCst);
                }
            }
        });

        let _ = release_tx.send(());
        sleep(Duration::from_millis(40)).await;
        assert_eq!(run_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn registers_and_clears_active_container_context() {
        let queue = GroupQueue::with_retry_config(1, Duration::from_millis(5), 1);
        let (tx, rx) = oneshot::channel::<()>();
        let hold = Arc::new(Mutex::new(Some(rx)));
        queue.set_process_messages_fn({
            let hold = hold.clone();
            move |_| {
                let hold = hold.clone();
                async move {
                    let rx = { hold.lock().expect("hold lock").take() };
                    if let Some(rx) = rx {
                        let _ = rx.await;
                    }
                    true
                }
            }
        });

        queue.enqueue_message_check("group@g.us");
        assert!(
            wait_until(Duration::from_millis(60), || {
                queue
                    .active_container_context("group@g.us")
                    .map(|ctx| ctx.active)
                    .unwrap_or(false)
            })
            .await
        );

        queue.register_active_container(
            "group@g.us",
            "dev-team",
            Some(4242),
            Some("rust-claw-dev-team-1".to_string()),
        );
        assert_eq!(
            queue.active_container_context("group@g.us"),
            Some(ActiveContainerContext {
                active: true,
                process_id: Some(4242),
                container_name: Some("rust-claw-dev-team-1".to_string()),
                group_folder: Some("dev-team".to_string()),
            })
        );

        let _ = tx.send(());
        assert!(
            wait_until(Duration::from_millis(80), || {
                queue
                    .active_container_context("group@g.us")
                    .map(|ctx| {
                        !ctx.active
                            && ctx.process_id.is_none()
                            && ctx.container_name.is_none()
                            && ctx.group_folder.is_none()
                    })
                    .unwrap_or(false)
            })
            .await
        );
    }

    #[test]
    fn does_not_register_container_context_for_inactive_group() {
        let queue = GroupQueue::with_retry_config(1, Duration::from_millis(5), 1);
        queue.register_active_container(
            "inactive@g.us",
            "inactive",
            Some(1),
            Some("container".to_string()),
        );
        assert_eq!(
            queue.active_container_context("inactive@g.us"),
            Some(ActiveContainerContext {
                active: false,
                process_id: None,
                container_name: None,
                group_folder: None,
            })
        );
    }
}
