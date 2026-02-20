use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use regex::Regex;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::db::{Db, DbError};
use crate::group_queue::GroupQueue;
use crate::router::{format_messages, format_outbound};
use crate::task_scheduler::TaskExecution;
use crate::types::{ContextMode, NewMessage, RegisteredGroup, ScheduledTask};

pub type AgentRunFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AgentRunOutput, String>> + Send + 'a>>;
pub type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
pub type AgentStreamFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
pub type AgentStreamCallback = Arc<dyn Fn(AgentStreamEvent) -> AgentStreamFuture + Send + Sync>;
pub type AgentProcessFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
pub type AgentProcessCallback = Arc<dyn Fn(AgentProcessEvent) -> AgentProcessFuture + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRunInput {
    pub group: RegisteredGroup,
    pub prompt: String,
    pub session_id: Option<String>,
    pub group_folder: String,
    pub chat_jid: String,
    pub is_main: bool,
    pub is_scheduled_task: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRunOutput {
    pub result: Option<String>,
    pub new_session_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStreamStatus {
    Success,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentStreamEvent {
    pub status: AgentStreamStatus,
    pub result: Option<String>,
    pub new_session_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentProcessEvent {
    pub process_id: Option<u32>,
    pub container_name: Option<String>,
    pub group_folder: Option<String>,
}

pub trait AgentRunner: Send + Sync {
    fn run<'a>(&'a self, input: AgentRunInput) -> AgentRunFuture<'a>;

    fn run_with_callback<'a>(
        &'a self,
        input: AgentRunInput,
        callback: Option<AgentStreamCallback>,
    ) -> AgentRunFuture<'a> {
        let _ = callback;
        self.run(input)
    }

    fn run_with_callbacks<'a>(
        &'a self,
        input: AgentRunInput,
        stream_callback: Option<AgentStreamCallback>,
        process_callback: Option<AgentProcessCallback>,
    ) -> AgentRunFuture<'a> {
        let _ = process_callback;
        self.run_with_callback(input, stream_callback)
    }
}

pub trait OutboundSender: Send + Sync {
    fn send<'a>(&'a self, jid: &'a str, text: &'a str) -> SendFuture<'a>;

    fn set_typing<'a>(&'a self, jid: &'a str, is_typing: bool) -> SendFuture<'a> {
        let _ = (jid, is_typing);
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone, Debug)]
pub struct OrchestratorConfig {
    pub assistant_name: String,
    pub main_group_folder: String,
    pub poll_interval: Duration,
    pub idle_timeout: Duration,
    pub ipc_base_dir: Option<PathBuf>,
    pub max_concurrent_containers: usize,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            assistant_name: "Andy".to_string(),
            main_group_folder: "main".to_string(),
            poll_interval: Duration::from_millis(2_000),
            idle_timeout: Duration::from_millis(1_800_000),
            ipc_base_dir: None,
            max_concurrent_containers: 5,
        }
    }
}

#[derive(Default)]
struct State {
    last_timestamp: String,
    sessions: HashMap<String, String>,
    registered_groups: HashMap<String, RegisteredGroup>,
    last_agent_timestamp: HashMap<String, String>,
}

pub struct Orchestrator {
    db: Db,
    config: OrchestratorConfig,
    queue: GroupQueue,
    state: Arc<Mutex<State>>,
    agent_runner: Arc<dyn AgentRunner>,
    outbound: Arc<dyn OutboundSender>,
}

impl Orchestrator {
    pub async fn create(
        db: Db,
        config: OrchestratorConfig,
        agent_runner: Arc<dyn AgentRunner>,
        outbound: Arc<dyn OutboundSender>,
    ) -> Result<Arc<Self>, DbError> {
        let state = load_state(&db)?;
        let queue = GroupQueue::new(config.max_concurrent_containers);
        if let Some(ipc_base_dir) = config.ipc_base_dir.clone() {
            queue.set_ipc_base_dir(ipc_base_dir);
        }
        let orchestrator = Arc::new(Self {
            db,
            config,
            queue,
            state: Arc::new(Mutex::new(state)),
            agent_runner,
            outbound,
        });

        let callback_orchestrator = orchestrator.clone();
        orchestrator
            .queue
            .set_process_messages_fn(move |chat_jid: String| {
                let callback_orchestrator = callback_orchestrator.clone();
                async move { callback_orchestrator.process_group_messages(chat_jid).await }
            });

        Ok(orchestrator)
    }

    pub fn queue(&self) -> GroupQueue {
        self.queue.clone()
    }

    pub fn main_group_folder(&self) -> &str {
        &self.config.main_group_folder
    }

    pub async fn shutdown(&self) {
        self.queue.shutdown();
    }

    pub async fn send_outbound_message(&self, chat_jid: &str, text: &str) -> Result<(), String> {
        self.outbound.send(chat_jid, text).await
    }

    pub async fn register_group(&self, jid: &str, group: RegisteredGroup) -> Result<(), DbError> {
        self.db.set_registered_group(jid, &group)?;
        let mut state = self.state.lock().await;
        state.registered_groups.insert(jid.to_string(), group);
        Ok(())
    }

    pub async fn ingest_message(&self, message: NewMessage) -> Result<(), DbError> {
        self.db
            .store_chat_metadata(&message.chat_jid, &message.timestamp, None)?;
        self.db.store_message(&message)?;
        Ok(())
    }

    pub async fn ingest_chat_metadata(
        &self,
        chat_jid: &str,
        timestamp: &str,
        name: Option<&str>,
    ) -> Result<(), DbError> {
        self.db.store_chat_metadata(chat_jid, timestamp, name)
    }

    pub async fn get_available_groups(&self) -> Result<Vec<AvailableGroup>, DbError> {
        let chats = self.db.get_all_chats()?;
        let registered = {
            let state = self.state.lock().await;
            state.registered_groups.keys().cloned().collect::<Vec<_>>()
        };
        let registered_set = registered
            .into_iter()
            .collect::<std::collections::HashSet<_>>();

        Ok(chats
            .into_iter()
            .filter(|chat| chat.jid != "__group_sync__" && chat.jid.ends_with("@discord"))
            .map(|chat| AvailableGroup {
                jid: chat.jid.clone(),
                name: chat.name,
                last_activity: chat.last_message_time,
                is_registered: registered_set.contains(&chat.jid),
            })
            .collect())
    }

    pub async fn poll_once(&self) -> Result<usize, String> {
        let (jids, last_timestamp, registered_groups, last_agent_timestamp) = {
            let state = self.state.lock().await;
            (
                state.registered_groups.keys().cloned().collect::<Vec<_>>(),
                state.last_timestamp.clone(),
                state.registered_groups.clone(),
                state.last_agent_timestamp.clone(),
            )
        };

        let (messages, new_timestamp) = self
            .db
            .get_new_messages(&jids, &last_timestamp, &self.config.assistant_name)
            .map_err(|err| err.to_string())?;

        if messages.is_empty() {
            return Ok(0);
        }

        {
            let mut state = self.state.lock().await;
            state.last_timestamp = new_timestamp;
        }
        self.save_state().await.map_err(|err| err.to_string())?;

        let mut grouped = HashMap::<String, Vec<NewMessage>>::new();
        for message in messages {
            grouped
                .entry(message.chat_jid.clone())
                .or_default()
                .push(message);
        }

        let mut enqueued = 0usize;
        for (chat_jid, group_messages) in grouped {
            let Some(group) = registered_groups.get(&chat_jid) else {
                continue;
            };

            let is_main = group.folder == self.config.main_group_folder;
            let needs_trigger = !is_main && group.requires_trigger != Some(false);
            if needs_trigger
                && !group_messages
                    .iter()
                    .any(|message| has_trigger(&message.content, &self.config.assistant_name))
            {
                continue;
            }

            let pending_since = last_agent_timestamp
                .get(&chat_jid)
                .cloned()
                .unwrap_or_default();
            let all_pending = self
                .db
                .get_messages_since(&chat_jid, &pending_since, &self.config.assistant_name)
                .map_err(|err| {
                    format!("failed to fetch pending messages for active group {chat_jid}: {err}")
                })?;
            let messages_to_send = if all_pending.is_empty() {
                group_messages
            } else {
                all_pending
            };
            let prompt = format_messages(&messages_to_send, &self.config.assistant_name);

            if self.queue.send_message(&chat_jid, &prompt) {
                let _ = self.outbound.set_typing(&chat_jid, true).await;
                if let Some(last_message) = messages_to_send.last() {
                    let mut state = self.state.lock().await;
                    state
                        .last_agent_timestamp
                        .insert(chat_jid.clone(), last_message.timestamp.clone());
                    drop(state);
                    self.save_state().await.map_err(|err| err.to_string())?;
                }
            } else {
                self.queue.enqueue_message_check(chat_jid);
                enqueued += 1;
            }
        }

        Ok(enqueued)
    }

    pub async fn recover_pending_messages(&self) -> Result<usize, String> {
        let (registered_groups, last_agent_timestamp) = {
            let state = self.state.lock().await;
            (
                state.registered_groups.clone(),
                state.last_agent_timestamp.clone(),
            )
        };

        let mut recovered = 0usize;
        for (chat_jid, _group) in registered_groups {
            let since = last_agent_timestamp
                .get(&chat_jid)
                .cloned()
                .unwrap_or_default();
            let pending = self
                .db
                .get_messages_since(&chat_jid, &since, &self.config.assistant_name)
                .map_err(|err| format!("failed to query pending messages for {chat_jid}: {err}"))?;
            if pending.is_empty() {
                continue;
            }
            self.queue.enqueue_message_check(chat_jid);
            recovered += 1;
        }

        Ok(recovered)
    }

    async fn process_group_messages(&self, chat_jid: String) -> bool {
        let (group, previous_cursor, stored_session_id) = {
            let state = self.state.lock().await;
            let Some(group) = state.registered_groups.get(&chat_jid).cloned() else {
                return true;
            };
            let previous_cursor = state
                .last_agent_timestamp
                .get(&chat_jid)
                .cloned()
                .unwrap_or_default();
            let stored_session_id = state.sessions.get(&group.folder).cloned();
            (group, previous_cursor, stored_session_id)
        };
        let session_id = session_id_for_runner(stored_session_id);

        let pending_messages = match self.db.get_messages_since(
            &chat_jid,
            &previous_cursor,
            &self.config.assistant_name,
        ) {
            Ok(messages) => messages,
            Err(err) => {
                eprintln!("failed to query messages since cursor for {chat_jid}: {err}");
                return false;
            }
        };
        if pending_messages.is_empty() {
            return true;
        }

        let is_main = group.folder == self.config.main_group_folder;
        let needs_trigger = !is_main && group.requires_trigger != Some(false);
        if needs_trigger
            && !pending_messages
                .iter()
                .any(|message| has_trigger(&message.content, &self.config.assistant_name))
        {
            return true;
        }

        let next_cursor = pending_messages
            .last()
            .map(|message| message.timestamp.clone())
            .unwrap_or(previous_cursor.clone());

        {
            let mut state = self.state.lock().await;
            state
                .last_agent_timestamp
                .insert(chat_jid.clone(), next_cursor.clone());
        }
        if let Err(err) = self.save_state().await {
            eprintln!("failed to save state before running agent for {chat_jid}: {err}");
            return false;
        }

        self.queue
            .register_active_container(&chat_jid, &group.folder, None, None);
        if let Err(err) = self.write_runtime_snapshots(&group.folder, is_main).await {
            eprintln!(
                "failed to write runtime snapshots for {}: {}",
                group.folder, err
            );
        }
        let _ = self.outbound.set_typing(&chat_jid, true).await;
        let prompt = format_messages(&pending_messages, &self.config.assistant_name);
        let received_stream_event = Arc::new(AtomicBool::new(false));
        let output_sent_to_user = Arc::new(AtomicBool::new(false));
        let idle_timer = Arc::new(StdMutex::new(None));
        let stream_callback: AgentStreamCallback = {
            let db = self.db.clone();
            let state = self.state.clone();
            let outbound = self.outbound.clone();
            let queue = self.queue.clone();
            let group_folder = group.folder.clone();
            let chat_jid = chat_jid.clone();
            let idle_timeout = self.config.idle_timeout;
            let idle_timer = idle_timer.clone();
            let received_stream_event = received_stream_event.clone();
            let output_sent_to_user = output_sent_to_user.clone();

            Arc::new(move |event: AgentStreamEvent| -> AgentStreamFuture {
                let db = db.clone();
                let state = state.clone();
                let outbound = outbound.clone();
                let queue = queue.clone();
                let group_folder = group_folder.clone();
                let chat_jid = chat_jid.clone();
                let idle_timer = idle_timer.clone();
                let received_stream_event = received_stream_event.clone();
                let output_sent_to_user = output_sent_to_user.clone();

                Box::pin(async move {
                    received_stream_event.store(true, Ordering::SeqCst);

                    if let Some(new_session_id) = event.new_session_id {
                        let normalized = normalize_session_id_for_storage(&new_session_id);
                        if let Err(err) = db.set_session(&group_folder, &normalized) {
                            eprintln!("failed to persist session id for {}: {err}", group_folder);
                        }
                        let mut state = state.lock().await;
                        state.sessions.insert(group_folder.clone(), normalized);
                    }

                    if event.status == AgentStreamStatus::Success {
                        if let Some(result) = event.result {
                            reset_idle_timer(
                                &idle_timer,
                                queue.clone(),
                                chat_jid.clone(),
                                idle_timeout,
                            );
                            let text = format_outbound(&result);
                            if !text.is_empty() {
                                let send_result = outbound.send(&chat_jid, &text).await;
                                if send_result.is_ok() {
                                    output_sent_to_user.store(true, Ordering::SeqCst);
                                } else if let Err(err) = send_result {
                                    eprintln!(
                                        "failed to send streamed outbound message to {chat_jid}: {err}"
                                    );
                                }
                            }
                        } else {
                            // turn completed marker(result=null) => stop Discord typing indicator
                            let _ = outbound.set_typing(&chat_jid, false).await;
                        }
                    } else {
                        let _ = outbound.set_typing(&chat_jid, false).await;
                    }
                })
            })
        };
        let process_callback =
            build_process_callback(self.queue.clone(), chat_jid.clone(), group.folder.clone());

        let run_output = self
            .agent_runner
            .run_with_callbacks(
                AgentRunInput {
                    group: group.clone(),
                    prompt,
                    session_id,
                    group_folder: group.folder.clone(),
                    chat_jid: chat_jid.clone(),
                    is_main,
                    is_scheduled_task: false,
                },
                Some(stream_callback),
                Some(process_callback),
            )
            .await;
        clear_idle_timer(&idle_timer);

        match run_output {
            Ok(output) => {
                if let Some(new_session_id) = output.new_session_id {
                    let normalized = normalize_session_id_for_storage(&new_session_id);
                    if let Err(err) = self.db.set_session(&group.folder, &normalized) {
                        eprintln!("failed to persist session id for {}: {err}", group.folder);
                    }
                    let mut state = self.state.lock().await;
                    state.sessions.insert(group.folder.clone(), normalized);
                }

                if !received_stream_event.load(Ordering::SeqCst) {
                    if let Some(result) = output.result {
                        let text = format_outbound(&result);
                        if !text.is_empty() {
                            let _ = self.outbound.set_typing(&chat_jid, false).await;
                            let send_result = self.outbound.send(&chat_jid, &text).await;
                            if send_result.is_ok() {
                                output_sent_to_user.store(true, Ordering::SeqCst);
                            } else if let Err(err) = send_result {
                                eprintln!("failed to send outbound message to {chat_jid}: {err}");
                            }
                        }
                    }
                }
                let _ = self.outbound.set_typing(&chat_jid, false).await;
                true
            }
            Err(error) => {
                eprintln!("agent runner failed for {chat_jid}: {error}");
                let _ = self.outbound.set_typing(&chat_jid, false).await;
                if output_sent_to_user.load(Ordering::SeqCst) {
                    eprintln!(
                        "agent error after streamed output was sent for {chat_jid}; skipping cursor rollback"
                    );
                    return true;
                }
                let mut state = self.state.lock().await;
                state.last_agent_timestamp.insert(chat_jid, previous_cursor);
                drop(state);
                let _ = self.save_state().await;
                false
            }
        }
    }

    pub async fn run_scheduled_task(&self, task: &ScheduledTask) -> TaskExecution {
        let group = {
            let state = self.state.lock().await;
            state
                .registered_groups
                .values()
                .find(|group| group.folder == task.group_folder)
                .cloned()
        };
        let Some(group) = group else {
            return TaskExecution {
                result: None,
                error: Some(format!("group not found for folder {}", task.group_folder)),
            };
        };

        let session_id = if task.context_mode == ContextMode::Group {
            let state = self.state.lock().await;
            session_id_for_runner(state.sessions.get(&task.group_folder).cloned())
        } else {
            None
        };
        let is_main = group.folder == self.config.main_group_folder;
        self.queue
            .register_active_container(&task.chat_jid, &group.folder, None, None);
        if let Err(err) = self.write_runtime_snapshots(&group.folder, is_main).await {
            eprintln!(
                "failed to write runtime snapshots for {}: {}",
                group.folder, err
            );
        }

        let received_stream_event = Arc::new(AtomicBool::new(false));
        let output_sent_to_user = Arc::new(AtomicBool::new(false));
        let last_stream_result = Arc::new(StdMutex::new(None::<String>));
        let idle_timer = Arc::new(StdMutex::new(None));

        let stream_callback: AgentStreamCallback = {
            let db = self.db.clone();
            let state = self.state.clone();
            let outbound = self.outbound.clone();
            let queue = self.queue.clone();
            let group_folder = group.folder.clone();
            let chat_jid = task.chat_jid.clone();
            let idle_timeout = self.config.idle_timeout;
            let idle_timer = idle_timer.clone();
            let received_stream_event = received_stream_event.clone();
            let output_sent_to_user = output_sent_to_user.clone();
            let last_stream_result = last_stream_result.clone();

            Arc::new(move |event: AgentStreamEvent| -> AgentStreamFuture {
                let db = db.clone();
                let state = state.clone();
                let outbound = outbound.clone();
                let queue = queue.clone();
                let group_folder = group_folder.clone();
                let chat_jid = chat_jid.clone();
                let idle_timer = idle_timer.clone();
                let received_stream_event = received_stream_event.clone();
                let output_sent_to_user = output_sent_to_user.clone();
                let last_stream_result = last_stream_result.clone();

                Box::pin(async move {
                    received_stream_event.store(true, Ordering::SeqCst);

                    if let Some(new_session_id) = event.new_session_id {
                        let normalized = normalize_session_id_for_storage(&new_session_id);
                        if let Err(err) = db.set_session(&group_folder, &normalized) {
                            eprintln!("failed to persist session id for {}: {err}", group_folder);
                        }
                        let mut state = state.lock().await;
                        state.sessions.insert(group_folder.clone(), normalized);
                    }

                    if event.status == AgentStreamStatus::Success {
                        if let Some(result) = event.result {
                            *last_stream_result.lock().expect("last_stream_result lock") =
                                Some(result.clone());
                            reset_idle_timer(
                                &idle_timer,
                                queue.clone(),
                                chat_jid.clone(),
                                idle_timeout,
                            );
                            let text = format_outbound(&result);
                            if !text.is_empty() {
                                let send_result = outbound.send(&chat_jid, &text).await;
                                if send_result.is_ok() {
                                    output_sent_to_user.store(true, Ordering::SeqCst);
                                } else if let Err(err) = send_result {
                                    eprintln!(
                                        "failed to send scheduled task outbound to {chat_jid}: {err}"
                                    );
                                }
                            }
                        } else {
                            // Scheduled task turns should terminate after completion marker.
                            let _ = queue.close_stdin(&chat_jid);
                        }
                    }
                })
            })
        };
        let process_callback = build_process_callback(
            self.queue.clone(),
            task.chat_jid.clone(),
            group.folder.clone(),
        );

        let run_output = self
            .agent_runner
            .run_with_callbacks(
                AgentRunInput {
                    group: group.clone(),
                    prompt: task.prompt.clone(),
                    session_id,
                    group_folder: task.group_folder.clone(),
                    chat_jid: task.chat_jid.clone(),
                    is_main,
                    is_scheduled_task: true,
                },
                Some(stream_callback),
                Some(process_callback),
            )
            .await;
        clear_idle_timer(&idle_timer);

        match run_output {
            Ok(output) => {
                if let Some(new_session_id) = output.new_session_id {
                    let normalized = normalize_session_id_for_storage(&new_session_id);
                    if let Err(err) = self.db.set_session(&group.folder, &normalized) {
                        eprintln!("failed to persist session id for {}: {err}", group.folder);
                    }
                    let mut state = self.state.lock().await;
                    state.sessions.insert(group.folder.clone(), normalized);
                }

                let mut final_result = last_stream_result
                    .lock()
                    .expect("last_stream_result lock")
                    .clone();
                if !received_stream_event.load(Ordering::SeqCst) {
                    if let Some(result) = output.result {
                        final_result = Some(result.clone());
                        let text = format_outbound(&result);
                        if !text.is_empty() {
                            let send_result = self.outbound.send(&task.chat_jid, &text).await;
                            if send_result.is_ok() {
                                output_sent_to_user.store(true, Ordering::SeqCst);
                            } else if let Err(err) = send_result {
                                eprintln!(
                                    "failed to send scheduled task fallback outbound to {}: {err}",
                                    task.chat_jid
                                );
                            }
                        }
                    }
                }
                // Safety net: ensure one-shot scheduled task containers do not linger waiting for IPC input.
                let _ = self.queue.close_stdin(&task.chat_jid);

                TaskExecution {
                    result: final_result,
                    error: None,
                }
            }
            Err(error) => {
                if output_sent_to_user.load(Ordering::SeqCst) {
                    return TaskExecution {
                        result: last_stream_result
                            .lock()
                            .expect("last_stream_result lock")
                            .clone(),
                        error: None,
                    };
                }
                TaskExecution {
                    result: None,
                    error: Some(error),
                }
            }
        }
    }

    async fn save_state(&self) -> Result<(), DbError> {
        let (last_timestamp, last_agent_timestamp) = {
            let state = self.state.lock().await;
            (
                state.last_timestamp.clone(),
                state.last_agent_timestamp.clone(),
            )
        };

        self.db
            .set_router_state("last_timestamp", &last_timestamp)?;
        self.db.set_router_state(
            "last_agent_timestamp",
            &serde_json::to_string(&last_agent_timestamp)?,
        )?;
        Ok(())
    }

    async fn write_runtime_snapshots(
        &self,
        group_folder: &str,
        is_main: bool,
    ) -> Result<(), String> {
        let Some(ipc_base_dir) = self.config.ipc_base_dir.clone() else {
            return Ok(());
        };

        let group_ipc_dir = ipc_base_dir.join(group_folder);
        fs::create_dir_all(&group_ipc_dir)
            .map_err(|err| format!("failed to create group IPC dir: {err}"))?;

        let all_tasks = self
            .db
            .get_all_tasks()
            .map_err(|err| format!("failed to load tasks for snapshot: {err}"))?;
        let task_snapshots = all_tasks
            .into_iter()
            .filter(|task| is_main || task.group_folder == group_folder)
            .map(|task| TaskSnapshot {
                id: task.id,
                group_folder: task.group_folder,
                prompt: task.prompt,
                schedule_type: task.schedule_type.as_str().to_string(),
                schedule_value: task.schedule_value,
                status: task.status.as_str().to_string(),
                next_run: task.next_run.map(|value| value.to_rfc3339()),
            })
            .collect::<Vec<_>>();

        let tasks_file = group_ipc_dir.join("current_tasks.json");
        let task_json = serde_json::to_string_pretty(&task_snapshots)
            .map_err(|err| format!("failed to serialize task snapshots: {err}"))?;
        fs::write(tasks_file, task_json)
            .map_err(|err| format!("failed to write task snapshots: {err}"))?;

        let available_groups = if is_main {
            self.get_available_groups()
                .await
                .map_err(|err| format!("failed to collect available groups: {err}"))?
                .into_iter()
                .map(|group| AvailableGroupSnapshot {
                    jid: group.jid,
                    name: group.name,
                    last_activity: group.last_activity,
                    is_registered: group.is_registered,
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let groups_snapshot = GroupsSnapshot {
            groups: available_groups,
            last_sync: chrono::Utc::now().to_rfc3339(),
        };
        let groups_file = group_ipc_dir.join("available_groups.json");
        let groups_json = serde_json::to_string_pretty(&groups_snapshot)
            .map_err(|err| format!("failed to serialize available groups snapshot: {err}"))?;
        fs::write(groups_file, groups_json)
            .map_err(|err| format!("failed to write available groups snapshot: {err}"))?;

        Ok(())
    }

    pub async fn refresh_runtime_snapshots(&self, group_folder: &str) -> Result<(), String> {
        let is_main = group_folder == self.config.main_group_folder;
        self.write_runtime_snapshots(group_folder, is_main).await
    }

    pub async fn run_loop(&self) {
        let mut interval = tokio::time::interval(self.config.poll_interval);
        #[cfg(unix)]
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|error| {
                eprintln!("sigterm listener setup error: {error}");
                error
            })
            .ok();

        loop {
            #[cfg(unix)]
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(error) = self.poll_once().await {
                        eprintln!("poll loop error: {error}");
                    }
                }
                signal_result = tokio::signal::ctrl_c() => {
                    if let Err(error) = signal_result {
                        eprintln!("ctrl_c listener error: {error}");
                    } else {
                        eprintln!("shutdown signal received: SIGINT");
                    }
                    break;
                }
                _ = async {
                    if let Some(signal) = sigterm.as_mut() {
                        let _ = signal.recv().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    eprintln!("shutdown signal received: SIGTERM");
                    break;
                }
            }

            #[cfg(not(unix))]
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(error) = self.poll_once().await {
                        eprintln!("poll loop error: {error}");
                    }
                }
                signal_result = tokio::signal::ctrl_c() => {
                    if let Err(error) = signal_result {
                        eprintln!("ctrl_c listener error: {error}");
                    } else {
                        eprintln!("shutdown signal received: Ctrl+C");
                    }
                    break;
                }
            }
        }
    }
}

fn load_state(db: &Db) -> Result<State, DbError> {
    let last_timestamp = db.get_router_state("last_timestamp")?.unwrap_or_default();
    let last_agent_timestamp_raw = db.get_router_state("last_agent_timestamp")?;
    let last_agent_timestamp = match last_agent_timestamp_raw {
        Some(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(value) => {
                serde_json::from_value::<HashMap<String, String>>(value).unwrap_or_default()
            }
            Err(_) => HashMap::new(),
        },
        None => HashMap::new(),
    };

    Ok(State {
        last_timestamp,
        sessions: db.get_all_sessions()?,
        registered_groups: db.get_all_registered_groups()?,
        last_agent_timestamp,
    })
}

fn has_trigger(content: &str, assistant_name: &str) -> bool {
    let pattern = format!(r"(?i)^@{}\b", regex::escape(assistant_name));
    Regex::new(&pattern)
        .map(|regex| regex.is_match(content.trim()))
        .unwrap_or(false)
}

fn session_id_for_runner(stored: Option<String>) -> Option<String> {
    stored.and_then(|value| value.strip_prefix("codex:").map(ToString::to_string))
}

fn normalize_session_id_for_storage(value: &str) -> String {
    if value.starts_with("codex:") {
        value.to_string()
    } else {
        format!("codex:{value}")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableGroup {
    pub jid: String,
    pub name: String,
    pub last_activity: String,
    pub is_registered: bool,
}

#[derive(serde::Serialize)]
struct TaskSnapshot {
    id: String,
    #[serde(rename = "groupFolder")]
    group_folder: String,
    prompt: String,
    schedule_type: String,
    schedule_value: String,
    status: String,
    next_run: Option<String>,
}

#[derive(serde::Serialize)]
struct AvailableGroupSnapshot {
    jid: String,
    name: String,
    #[serde(rename = "lastActivity")]
    last_activity: String,
    #[serde(rename = "isRegistered")]
    is_registered: bool,
}

#[derive(serde::Serialize)]
struct GroupsSnapshot {
    groups: Vec<AvailableGroupSnapshot>,
    #[serde(rename = "lastSync")]
    last_sync: String,
}

fn build_process_callback(
    queue: GroupQueue,
    chat_jid: String,
    default_group_folder: String,
) -> AgentProcessCallback {
    Arc::new(move |event: AgentProcessEvent| -> AgentProcessFuture {
        let queue = queue.clone();
        let chat_jid = chat_jid.clone();
        let default_group_folder = default_group_folder.clone();
        Box::pin(async move {
            let group_folder = event
                .group_folder
                .as_deref()
                .unwrap_or(&default_group_folder);
            queue.register_active_container(
                &chat_jid,
                group_folder,
                event.process_id,
                event.container_name,
            );
        })
    })
}

fn reset_idle_timer(
    idle_timer: &Arc<StdMutex<Option<tokio::task::JoinHandle<()>>>>,
    queue: GroupQueue,
    chat_jid: String,
    idle_timeout: Duration,
) {
    let mut guard = idle_timer.lock().expect("idle timer lock");
    if let Some(handle) = guard.take() {
        handle.abort();
    }
    let handle = tokio::spawn(async move {
        tokio::time::sleep(idle_timeout).await;
        let _ = queue.close_stdin(&chat_jid);
    });
    *guard = Some(handle);
}

fn clear_idle_timer(idle_timer: &Arc<StdMutex<Option<tokio::task::JoinHandle<()>>>>) {
    let mut guard = idle_timer.lock().expect("idle timer lock");
    if let Some(handle) = guard.take() {
        handle.abort();
    }
}

#[derive(Default)]
pub struct EchoAgentRunner;

impl AgentRunner for EchoAgentRunner {
    fn run<'a>(&'a self, input: AgentRunInput) -> AgentRunFuture<'a> {
        Box::pin(async move {
            Ok(AgentRunOutput {
                result: Some(format!("Echo from {}:\n{}", input.group.name, input.prompt)),
                new_session_id: input.session_id.or(Some("session-echo".to_string())),
            })
        })
    }
}

#[derive(Default)]
pub struct StdoutOutboundSender;

impl OutboundSender for StdoutOutboundSender {
    fn send<'a>(&'a self, jid: &'a str, text: &'a str) -> SendFuture<'a> {
        Box::pin(async move {
            println!("[outbound:{jid}] {text}");
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::types::{AdditionalMount, ContainerConfig};

    struct MockAgentRunner;

    impl AgentRunner for MockAgentRunner {
        fn run<'a>(&'a self, _input: AgentRunInput) -> AgentRunFuture<'a> {
            Box::pin(async move {
                Ok(AgentRunOutput {
                    result: Some("agent-response".to_string()),
                    new_session_id: Some("session-1".to_string()),
                })
            })
        }
    }

    struct MockStreamThenErrorRunner;

    impl AgentRunner for MockStreamThenErrorRunner {
        fn run<'a>(&'a self, _input: AgentRunInput) -> AgentRunFuture<'a> {
            Box::pin(async move { Err("simulated stream failure".to_string()) })
        }

        fn run_with_callbacks<'a>(
            &'a self,
            _input: AgentRunInput,
            stream_callback: Option<AgentStreamCallback>,
            _process_callback: Option<AgentProcessCallback>,
        ) -> AgentRunFuture<'a> {
            Box::pin(async move {
                if let Some(callback) = stream_callback {
                    callback(AgentStreamEvent {
                        status: AgentStreamStatus::Success,
                        result: Some("partial-response".to_string()),
                        new_session_id: None,
                        error: None,
                    })
                    .await;
                }
                Err("simulated stream failure".to_string())
            })
        }
    }

    #[derive(Default)]
    struct MockOutbound {
        messages: StdMutex<Vec<(String, String)>>,
    }

    impl OutboundSender for MockOutbound {
        fn send<'a>(&'a self, jid: &'a str, text: &'a str) -> SendFuture<'a> {
            self.messages
                .lock()
                .expect("outbound lock")
                .push((jid.to_string(), text.to_string()));
            Box::pin(async { Ok(()) })
        }
    }

    fn sample_group() -> RegisteredGroup {
        RegisteredGroup {
            name: "Dev Group".to_string(),
            folder: "dev".to_string(),
            trigger: "@Andy".to_string(),
            added_at: "2026-02-19T00:00:00.000Z".to_string(),
            container_config: Some(ContainerConfig {
                additional_mounts: vec![AdditionalMount {
                    host_path: "/tmp/project".to_string(),
                    container_path: Some("project".to_string()),
                    readonly: Some(true),
                }],
                timeout_ms: None,
            }),
            requires_trigger: Some(true),
        }
    }

    fn message(id: &str, content: &str, timestamp: &str) -> NewMessage {
        NewMessage {
            id: id.to_string(),
            chat_jid: "group@g.us".to_string(),
            sender: "u".to_string(),
            sender_name: "User".to_string(),
            content: content.to_string(),
            timestamp: timestamp.to_string(),
            is_from_me: false,
            is_bot_message: false,
        }
    }

    #[tokio::test]
    async fn process_group_messages_sends_output_when_trigger_present() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(temp_dir.path().join("messages.db")).expect("db open");
        db.set_registered_group("group@g.us", &sample_group())
            .expect("set group");
        db.store_chat_metadata("group@g.us", "2026-02-19T00:00:01.000Z", Some("Dev Group"))
            .expect("store chat metadata");
        db.store_message(&message("1", "@Andy hello", "2026-02-19T00:00:01.000Z"))
            .expect("store message");

        let outbound = Arc::new(MockOutbound::default());
        let orchestrator = Orchestrator::create(
            db.clone(),
            OrchestratorConfig::default(),
            Arc::new(MockAgentRunner),
            outbound.clone(),
        )
        .await
        .expect("create orchestrator");

        let success = orchestrator
            .process_group_messages("group@g.us".to_string())
            .await;
        assert!(success);

        let sent = outbound.messages.lock().expect("sent lock");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "group@g.us");
        assert_eq!(sent[0].1, "agent-response");

        let session = db.get_session("dev").expect("get session");
        assert_eq!(session.as_deref(), Some("codex:session-1"));
    }

    #[tokio::test]
    async fn poll_once_enqueues_only_triggered_messages_for_non_main_group() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(temp_dir.path().join("messages.db")).expect("db open");
        db.set_registered_group("group@g.us", &sample_group())
            .expect("set group");
        db.store_chat_metadata("group@g.us", "2026-02-19T00:00:01.000Z", Some("Dev Group"))
            .expect("store chat metadata");
        db.store_message(&message("1", "hello", "2026-02-19T00:00:01.000Z"))
            .expect("store non-trigger message");
        db.store_message(&message("2", "@Andy ping", "2026-02-19T00:00:02.000Z"))
            .expect("store trigger message");

        let orchestrator = Orchestrator::create(
            db,
            OrchestratorConfig::default(),
            Arc::new(MockAgentRunner),
            Arc::new(MockOutbound::default()),
        )
        .await
        .expect("create orchestrator");

        let enqueued = orchestrator.poll_once().await.expect("poll once");
        assert_eq!(enqueued, 1);
    }

    #[tokio::test]
    async fn recover_pending_messages_enqueues_groups_with_unprocessed_messages() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(temp_dir.path().join("messages.db")).expect("db open");
        db.set_registered_group("group@g.us", &sample_group())
            .expect("set group");
        db.store_chat_metadata("group@g.us", "2026-02-19T00:00:01.000Z", Some("Dev Group"))
            .expect("store chat metadata");
        db.store_message(&message("1", "@Andy pending", "2026-02-19T00:00:01.000Z"))
            .expect("store message");

        let orchestrator = Orchestrator::create(
            db,
            OrchestratorConfig::default(),
            Arc::new(MockAgentRunner),
            Arc::new(MockOutbound::default()),
        )
        .await
        .expect("create orchestrator");

        let recovered = orchestrator
            .recover_pending_messages()
            .await
            .expect("recover pending messages");
        assert_eq!(recovered, 1);
    }

    #[tokio::test]
    async fn skips_cursor_rollback_if_streamed_output_was_already_sent() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(temp_dir.path().join("messages.db")).expect("db open");
        db.set_registered_group("group@g.us", &sample_group())
            .expect("set group");
        db.store_chat_metadata("group@g.us", "2026-02-19T00:00:01.000Z", Some("Dev Group"))
            .expect("store chat metadata");
        db.store_message(&message("1", "@Andy hello", "2026-02-19T00:00:01.000Z"))
            .expect("store message");

        let outbound = Arc::new(MockOutbound::default());
        let orchestrator = Orchestrator::create(
            db.clone(),
            OrchestratorConfig::default(),
            Arc::new(MockStreamThenErrorRunner),
            outbound.clone(),
        )
        .await
        .expect("create orchestrator");

        let first = orchestrator
            .process_group_messages("group@g.us".to_string())
            .await;
        let second = orchestrator
            .process_group_messages("group@g.us".to_string())
            .await;
        assert!(first);
        assert!(second);

        let sent = outbound.messages.lock().expect("sent lock");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, "partial-response");
    }

    #[test]
    fn has_trigger_checks_case_insensitive_prefix() {
        assert!(has_trigger("@ANDY do thing", "Andy"));
        assert!(!has_trigger("@Andyyyy do thing", "Andy"));
        assert!(!has_trigger("hi @Andy", "Andy"));
    }

    #[test]
    fn session_id_for_runner_accepts_only_codex_prefixed_values() {
        assert_eq!(
            session_id_for_runner(Some("codex:thread-1".to_string())).as_deref(),
            Some("thread-1")
        );
        assert_eq!(
            session_id_for_runner(Some("legacy-session".to_string())),
            None
        );
    }

    #[test]
    fn normalize_session_id_for_storage_adds_codex_prefix() {
        assert_eq!(
            normalize_session_id_for_storage("thread-1"),
            "codex:thread-1"
        );
        assert_eq!(
            normalize_session_id_for_storage("codex:thread-2"),
            "codex:thread-2"
        );
    }
}
