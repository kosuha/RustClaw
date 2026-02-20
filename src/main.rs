use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use fs2::FileExt;
use tokio::sync::watch;

use rust_claw::agent_runner_command::MarkerCommandAgentRunner;
use rust_claw::agent_runner_container::ContainerAgentRunner;
use rust_claw::channel_outbound::ChannelOutboundSender;
use rust_claw::command_executor::MarkerCommandExecutor;
use rust_claw::db::Db;
use rust_claw::discord_channel::{DiscordChannel, DiscordChannelConfig};
use rust_claw::env_file::read_env_var;
use rust_claw::inbound_bridge::{CommandInboundBridge, InboundBridgeEvent};
use rust_claw::ipc_watcher::start_ipc_watcher;
use rust_claw::orchestrator::{
    AgentRunner, EchoAgentRunner, Orchestrator, OrchestratorConfig, OutboundSender,
    StdoutOutboundSender,
};
use rust_claw::outbound_command::CommandOutboundSender;
use rust_claw::state_migration::migrate_json_state;
use rust_claw::task_scheduler::{TaskExecution, TaskExecutionFuture, TaskExecutor, TaskScheduler};
use rust_claw::types::{
    Channel, ContextMode, NewMessage, RegisteredGroup, ScheduleType, ScheduledTask, TaskStatus,
};

struct PromptEchoTaskExecutor;

impl TaskExecutor for PromptEchoTaskExecutor {
    fn execute_task<'a>(&'a self, task: &'a ScheduledTask) -> TaskExecutionFuture<'a> {
        Box::pin(async move {
            TaskExecution {
                result: Some(format!("Task executed: {}", task.prompt)),
                error: None,
            }
        })
    }
}

struct OrchestratorTaskExecutor {
    orchestrator: Arc<Orchestrator>,
}

impl TaskExecutor for OrchestratorTaskExecutor {
    fn execute_task<'a>(&'a self, task: &'a ScheduledTask) -> TaskExecutionFuture<'a> {
        Box::pin(async move { self.orchestrator.run_scheduled_task(task).await })
    }
}

struct ManagedTask {
    shutdown_tx: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl ManagedTask {
    async fn shutdown(self, label: &str) {
        let _ = self.shutdown_tx.send(true);
        wait_for_task_completion(self.handle, label).await;
    }
}

enum RuntimeChannel {
    Discord(Arc<DiscordChannel>),
}

impl RuntimeChannel {
    fn as_channel(&self) -> Arc<dyn Channel> {
        match self {
            RuntimeChannel::Discord(channel) => channel.clone(),
        }
    }

    fn set_handlers(&self, orchestrator: Arc<Orchestrator>) {
        match self {
            RuntimeChannel::Discord(channel) => channel.set_handlers(
                {
                    let orchestrator = orchestrator.clone();
                    move |message| {
                        let orchestrator = orchestrator.clone();
                        async move {
                            orchestrator
                                .ingest_message(message)
                                .await
                                .map_err(|err| err.to_string())
                        }
                    }
                },
                {
                    let orchestrator = orchestrator.clone();
                    move |chat_jid, timestamp, name| {
                        let orchestrator = orchestrator.clone();
                        async move {
                            orchestrator
                                .ingest_chat_metadata(&chat_jid, &timestamp, name.as_deref())
                                .await
                                .map_err(|err| err.to_string())
                        }
                    }
                },
            ),
        }
    }

    fn start(&self) -> tokio::task::JoinHandle<()> {
        match self {
            RuntimeChannel::Discord(channel) => channel.clone().start(),
        }
    }

    fn shutdown(&self) {
        match self {
            RuntimeChannel::Discord(channel) => channel.shutdown(),
        }
    }
}

#[derive(Parser)]
#[command(name = "rust-claw")]
#[command(version, about = "Rust rewrite bootstrap of NanoClaw")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Run,
    MigrateJsonState,
    BootstrapMain(BootstrapMainArgs),
    RegisterGroup(RegisterGroupArgs),
    AddMessage(AddMessageArgs),
    CreateTask(CreateTaskArgs),
    ListGroups,
    ListTasks(ListTasksArgs),
    PauseTask(TaskIdArgs),
    ResumeTask(TaskIdArgs),
    CancelTask(TaskIdArgs),
}

#[derive(Args)]
struct BootstrapMainArgs {
    #[arg(long, default_value = "main@local")]
    jid: String,
    #[arg(long, default_value = "Main")]
    name: String,
    #[arg(long)]
    folder: Option<String>,
    #[arg(long)]
    trigger: Option<String>,
}

#[derive(Args)]
struct RegisterGroupArgs {
    #[arg(long)]
    jid: String,
    #[arg(long)]
    name: String,
    #[arg(long)]
    folder: String,
    #[arg(long)]
    trigger: Option<String>,
    #[arg(long, default_value_t = true)]
    requires_trigger: bool,
}

#[derive(Args)]
struct AddMessageArgs {
    #[arg(long)]
    chat_jid: String,
    #[arg(long)]
    sender: String,
    #[arg(long)]
    sender_name: String,
    #[arg(long)]
    content: String,
    #[arg(long)]
    timestamp: Option<String>,
    #[arg(long)]
    id: Option<String>,
    #[arg(long, default_value_t = false)]
    is_from_me: bool,
    #[arg(long, default_value_t = false)]
    is_bot_message: bool,
}

#[derive(Copy, Clone, ValueEnum)]
enum ScheduleTypeArg {
    Cron,
    Interval,
    Once,
}

#[derive(Copy, Clone, ValueEnum)]
enum TaskStatusArg {
    Active,
    Paused,
    Completed,
}

#[derive(Copy, Clone, ValueEnum)]
enum ContextModeArg {
    Group,
    Isolated,
}

#[derive(Args)]
struct CreateTaskArgs {
    #[arg(long)]
    id: String,
    #[arg(long)]
    group_folder: String,
    #[arg(long)]
    chat_jid: String,
    #[arg(long)]
    prompt: String,
    #[arg(long, value_enum)]
    schedule_type: ScheduleTypeArg,
    #[arg(long)]
    schedule_value: String,
    #[arg(long)]
    next_run: Option<String>,
    #[arg(long, value_enum, default_value_t = TaskStatusArg::Active)]
    status: TaskStatusArg,
    #[arg(long, value_enum, default_value_t = ContextModeArg::Isolated)]
    context_mode: ContextModeArg,
}

#[derive(Args)]
struct ListTasksArgs {
    #[arg(long)]
    group: Option<String>,
}

#[derive(Args)]
struct TaskIdArgs {
    #[arg(long)]
    id: String,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("rust-claw failed: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run_daemon().await,
        Command::MigrateJsonState => handle_migrate_json_state(),
        Command::BootstrapMain(args) => handle_bootstrap_main(args),
        Command::RegisterGroup(args) => handle_register_group(args),
        Command::AddMessage(args) => handle_add_message(args),
        Command::CreateTask(args) => handle_create_task(args),
        Command::ListGroups => handle_list_groups(),
        Command::ListTasks(args) => handle_list_tasks(args),
        Command::PauseTask(args) => handle_set_task_status(args, TaskStatus::Paused),
        Command::ResumeTask(args) => handle_set_task_status(args, TaskStatus::Active),
        Command::CancelTask(args) => handle_cancel_task(args),
    }
}

async fn run_daemon() -> Result<(), String> {
    let assistant_name = read_env("ASSISTANT_NAME").unwrap_or_else(|| "Andy".to_string());
    let main_group_folder = read_env("MAIN_GROUP_FOLDER").unwrap_or_else(|| "main".to_string());
    let poll_interval = Duration::from_millis(read_u64("POLL_INTERVAL_MS", 2_000));
    let scheduler_interval = Duration::from_millis(read_u64("SCHEDULER_POLL_INTERVAL_MS", 60_000));
    let idle_timeout = Duration::from_millis(read_u64("IDLE_TIMEOUT_MS", 1_800_000));
    let ipc_poll_interval = Duration::from_millis(read_u64("IPC_POLL_INTERVAL_MS", 1_000));
    let max_concurrent = read_usize("MAX_CONCURRENT_CONTAINERS", 5);
    let agent_runner_mode =
        read_env("AGENT_RUNNER_MODE").unwrap_or_else(|| "container".to_string());
    let task_executor_mode = read_env("TASK_EXECUTOR_MODE").unwrap_or_else(|| "agent".to_string());
    let outbound_sender_mode =
        read_env("OUTBOUND_SENDER_MODE").unwrap_or_else(|| "discord".to_string());
    let data_dir = data_dir_from_env();
    let _runtime_lock = acquire_runtime_lock(&data_dir)?;
    let ipc_base_dir = data_dir.join("ipc");

    if agent_runner_mode == "container" {
        ensure_container_runtime_ready().await?;
        if let Err(err) = cleanup_orphaned_containers().await {
            eprintln!("failed to clean orphaned containers: {err}");
        }
    }

    let db = open_db_with_json_migration()?;
    maybe_auto_register_main_group(&db, &assistant_name, &main_group_folder)?;
    let agent_runner = build_agent_runner(&agent_runner_mode)?;
    let mut runtime_channel: Option<RuntimeChannel> = None;
    let outbound_sender: Arc<dyn OutboundSender> = if outbound_sender_mode == "discord" {
        let channel = RuntimeChannel::Discord(Arc::new(build_discord_channel(&assistant_name)?));
        let channel_as_trait: Arc<dyn Channel> = channel.as_channel();
        let sender = Arc::new(ChannelOutboundSender::new(vec![channel_as_trait]));
        runtime_channel = Some(channel);
        sender
    } else {
        build_outbound_sender(&outbound_sender_mode)?
    };
    let orchestrator = Orchestrator::create(
        db.clone(),
        OrchestratorConfig {
            assistant_name: assistant_name.clone(),
            main_group_folder: main_group_folder.clone(),
            poll_interval,
            idle_timeout,
            ipc_base_dir: Some(ipc_base_dir.clone()),
            max_concurrent_containers: max_concurrent,
        },
        agent_runner,
        outbound_sender,
    )
    .await
    .map_err(|err| format!("failed to create orchestrator: {err}"))?;
    let task_executor = build_task_executor(&task_executor_mode, orchestrator.clone())?;

    let scheduler = TaskScheduler::start(
        db.clone(),
        orchestrator.queue(),
        task_executor,
        scheduler_interval,
    );
    let ipc_watcher_task = start_ipc_watcher(
        orchestrator.clone(),
        db.clone(),
        ipc_base_dir,
        Some(ipc_poll_interval),
    );
    let runtime_channel_task = if let Some(channel) = runtime_channel.as_ref() {
        channel.set_handlers(orchestrator.clone());
        Some(channel.start())
    } else {
        None
    };

    let inbound_bridge_task = if outbound_sender_mode == "discord" {
        None
    } else {
        maybe_start_inbound_bridge(orchestrator.clone())?
    };
    let recovered_groups = orchestrator.recover_pending_messages().await?;
    if recovered_groups > 0 {
        println!(
            "recovery queued {} group(s) with pending messages",
            recovered_groups
        );
    }

    println!("NanoClaw running (trigger: @{})", assistant_name);
    println!("press Ctrl+C to stop.");
    orchestrator.run_loop().await;
    scheduler.shutdown().await;
    ipc_watcher_task.abort();
    let _ = ipc_watcher_task.await;
    if let Some(task) = inbound_bridge_task {
        task.shutdown("inbound bridge").await;
    }
    if let Some(channel) = runtime_channel.as_ref() {
        channel.shutdown();
    }
    if let Some(task) = runtime_channel_task {
        wait_for_task_completion(task, "runtime channel").await;
    }
    orchestrator.shutdown().await;
    Ok(())
}

fn handle_register_group(args: RegisterGroupArgs) -> Result<(), String> {
    let db = open_db_with_json_migration()?;
    let group = RegisteredGroup {
        name: args.name,
        folder: args.folder,
        trigger: args.trigger.unwrap_or_else(default_trigger),
        added_at: Utc::now().to_rfc3339(),
        container_config: None,
        requires_trigger: Some(args.requires_trigger),
    };
    db.set_registered_group(&args.jid, &group)
        .map_err(|err| format!("failed to register group: {err}"))?;
    println!("registered group: {} -> {}", args.jid, group.folder);
    Ok(())
}

fn handle_bootstrap_main(args: BootstrapMainArgs) -> Result<(), String> {
    let db = open_db_with_json_migration()?;
    let assistant_name = read_env("ASSISTANT_NAME").unwrap_or_else(|| "Andy".to_string());
    let main_group_folder = read_env("MAIN_GROUP_FOLDER").unwrap_or_else(|| "main".to_string());
    let folder = args.folder.unwrap_or(main_group_folder);
    let trigger = args.trigger.unwrap_or_else(|| format!("@{assistant_name}"));

    let group = RegisteredGroup {
        name: args.name,
        folder,
        trigger,
        added_at: Utc::now().to_rfc3339(),
        container_config: None,
        requires_trigger: Some(false),
    };
    db.set_registered_group(&args.jid, &group)
        .map_err(|err| format!("failed to bootstrap main group: {err}"))?;
    println!(
        "bootstrapped main group: jid={} folder={} trigger={} requires_trigger=false",
        args.jid, group.folder, group.trigger
    );
    Ok(())
}

fn handle_add_message(args: AddMessageArgs) -> Result<(), String> {
    let db = open_db_with_json_migration()?;
    let timestamp = args.timestamp.unwrap_or_else(|| Utc::now().to_rfc3339());
    db.store_chat_metadata(&args.chat_jid, &timestamp, None)
        .map_err(|err| format!("failed to upsert chat metadata: {err}"))?;
    let message = NewMessage {
        id: args
            .id
            .unwrap_or_else(|| format!("msg-{}", Utc::now().timestamp_millis())),
        chat_jid: args.chat_jid,
        sender: args.sender,
        sender_name: args.sender_name,
        content: args.content,
        timestamp,
        is_from_me: args.is_from_me,
        is_bot_message: args.is_bot_message,
    };
    db.store_message(&message)
        .map_err(|err| format!("failed to store message: {err}"))?;
    println!("stored message: {} in {}", message.id, message.chat_jid);
    Ok(())
}

fn handle_create_task(args: CreateTaskArgs) -> Result<(), String> {
    let db = open_db_with_json_migration()?;
    let schedule_type = match args.schedule_type {
        ScheduleTypeArg::Cron => ScheduleType::Cron,
        ScheduleTypeArg::Interval => ScheduleType::Interval,
        ScheduleTypeArg::Once => ScheduleType::Once,
    };
    let status = match args.status {
        TaskStatusArg::Active => TaskStatus::Active,
        TaskStatusArg::Paused => TaskStatus::Paused,
        TaskStatusArg::Completed => TaskStatus::Completed,
    };
    let context_mode = match args.context_mode {
        ContextModeArg::Group => ContextMode::Group,
        ContextModeArg::Isolated => ContextMode::Isolated,
    };
    let next_run = match args.next_run {
        Some(raw) => Some(parse_timestamp(&raw)?),
        None if status == TaskStatus::Active => Some(Utc::now()),
        None => None,
    };

    let task = ScheduledTask {
        id: args.id,
        group_folder: args.group_folder,
        chat_jid: args.chat_jid,
        prompt: args.prompt,
        schedule_type,
        schedule_value: args.schedule_value,
        context_mode,
        next_run,
        last_run: None,
        last_result: None,
        status,
        created_at: Utc::now(),
    };

    db.create_task(&task)
        .map_err(|err| format!("failed to create task: {err}"))?;
    println!("created task: {}", task.id);
    Ok(())
}

fn handle_list_groups() -> Result<(), String> {
    let db = open_db_with_json_migration()?;
    let groups = db
        .get_all_registered_groups()
        .map_err(|err| format!("failed to load groups: {err}"))?;
    if groups.is_empty() {
        println!("no registered groups");
        return Ok(());
    }
    for (jid, group) in groups {
        println!(
            "{jid}\tname={}\tfolder={}\ttrigger={}\trequires_trigger={}",
            group.name,
            group.folder,
            group.trigger,
            group.requires_trigger.unwrap_or(true)
        );
    }
    Ok(())
}

fn handle_list_tasks(args: ListTasksArgs) -> Result<(), String> {
    let db = open_db_with_json_migration()?;
    let mut tasks = db
        .get_all_tasks()
        .map_err(|err| format!("failed to load tasks: {err}"))?;
    if let Some(group) = args.group {
        tasks.retain(|task| task.group_folder == group);
    }
    if tasks.is_empty() {
        println!("no tasks");
        return Ok(());
    }
    for task in tasks {
        println!(
            "{}\tgroup={}\tjid={}\tstatus={}\tnext_run={}",
            task.id,
            task.group_folder,
            task.chat_jid,
            task.status.as_str(),
            task.next_run
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| "null".to_string())
        );
    }
    Ok(())
}

fn handle_set_task_status(args: TaskIdArgs, status: TaskStatus) -> Result<(), String> {
    let db = open_db_with_json_migration()?;
    let existing = db
        .get_task_by_id(&args.id)
        .map_err(|err| format!("failed to load task '{}': {err}", args.id))?;
    if existing.is_none() {
        return Err(format!("task not found: {}", args.id));
    }
    db.set_task_status(&args.id, status)
        .map_err(|err| format!("failed to update task status: {err}"))?;
    println!("updated task {} status={}", args.id, status.as_str());
    Ok(())
}

fn handle_cancel_task(args: TaskIdArgs) -> Result<(), String> {
    let db = open_db_with_json_migration()?;
    let existing = db
        .get_task_by_id(&args.id)
        .map_err(|err| format!("failed to load task '{}': {err}", args.id))?;
    if existing.is_none() {
        return Err(format!("task not found: {}", args.id));
    }
    db.delete_task(&args.id)
        .map_err(|err| format!("failed to cancel task: {err}"))?;
    println!("cancelled task {}", args.id);
    Ok(())
}

fn handle_migrate_json_state() -> Result<(), String> {
    let _db = open_db_with_json_migration()?;
    println!("legacy JSON state migration check completed");
    Ok(())
}

fn build_agent_runner(mode: &str) -> Result<Arc<dyn AgentRunner>, String> {
    match mode {
        "echo" => Ok(Arc::new(EchoAgentRunner)),
        "command" => {
            let program = read_env("AGENT_RUNNER_PROGRAM").ok_or_else(|| {
                "AGENT_RUNNER_PROGRAM is required when AGENT_RUNNER_MODE=command".to_string()
            })?;
            let args_template_raw = read_env("AGENT_RUNNER_ARGS_JSON").ok_or_else(|| {
                "AGENT_RUNNER_ARGS_JSON is required when AGENT_RUNNER_MODE=command".to_string()
            })?;
            let args_template = serde_json::from_str::<Vec<String>>(&args_template_raw)
                .map_err(|err| format!("invalid AGENT_RUNNER_ARGS_JSON: {err}"))?;
            let timeout = Duration::from_millis(read_u64("AGENT_RUNNER_TIMEOUT_MS", 300_000));

            Ok(Arc::new(MarkerCommandAgentRunner {
                program,
                args_template,
                timeout,
            }))
        }
        "container" => {
            let container_binary =
                read_env("CONTAINER_BINARY").unwrap_or_else(|| "docker".to_string());
            let container_image = read_env("CONTAINER_IMAGE")
                .unwrap_or_else(|| "rust-claw-codex-agent:latest".to_string());
            let project_root = read_env("PROJECT_ROOT")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
            let groups_dir = read_env("GROUPS_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| project_root.join("groups"));
            let data_dir = read_env("DATA_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| project_root.join("data"));
            let timeout = Duration::from_millis(read_u64("CONTAINER_TIMEOUT_MS", 1_800_000));

            Ok(Arc::new(ContainerAgentRunner::new(
                container_binary,
                container_image,
                project_root,
                groups_dir,
                data_dir,
                timeout,
            )))
        }
        other => Err(format!(
            "unsupported AGENT_RUNNER_MODE: {other} (expected: echo | command | container)"
        )),
    }
}

fn build_task_executor(
    mode: &str,
    orchestrator: Arc<Orchestrator>,
) -> Result<Arc<dyn TaskExecutor>, String> {
    match mode {
        "agent" => Ok(Arc::new(OrchestratorTaskExecutor { orchestrator })),
        "echo" => Ok(Arc::new(PromptEchoTaskExecutor)),
        "command" => {
            let program = read_env("TASK_EXECUTOR_PROGRAM").ok_or_else(|| {
                "TASK_EXECUTOR_PROGRAM is required when TASK_EXECUTOR_MODE=command".to_string()
            })?;
            let args_template_raw = read_env("TASK_EXECUTOR_ARGS_JSON").ok_or_else(|| {
                "TASK_EXECUTOR_ARGS_JSON is required when TASK_EXECUTOR_MODE=command".to_string()
            })?;
            let args_template = serde_json::from_str::<Vec<String>>(&args_template_raw)
                .map_err(|err| format!("invalid TASK_EXECUTOR_ARGS_JSON: {err}"))?;
            let timeout = Duration::from_millis(read_u64("TASK_EXECUTOR_TIMEOUT_MS", 300_000));

            Ok(Arc::new(MarkerCommandExecutor {
                program,
                args_template,
                timeout,
            }))
        }
        other => Err(format!(
            "unsupported TASK_EXECUTOR_MODE: {other} (expected: agent | echo | command)"
        )),
    }
}

fn build_outbound_sender(mode: &str) -> Result<Arc<dyn OutboundSender>, String> {
    match mode {
        "stdout" => Ok(Arc::new(StdoutOutboundSender)),
        "command" => {
            let program = read_env("OUTBOUND_SENDER_PROGRAM").ok_or_else(|| {
                "OUTBOUND_SENDER_PROGRAM is required when OUTBOUND_SENDER_MODE=command".to_string()
            })?;
            let args_template_raw = read_env("OUTBOUND_SENDER_ARGS_JSON").ok_or_else(|| {
                "OUTBOUND_SENDER_ARGS_JSON is required when OUTBOUND_SENDER_MODE=command"
                    .to_string()
            })?;
            let args_template = serde_json::from_str::<Vec<String>>(&args_template_raw)
                .map_err(|err| format!("invalid OUTBOUND_SENDER_ARGS_JSON: {err}"))?;
            let timeout = Duration::from_millis(read_u64("OUTBOUND_SENDER_TIMEOUT_MS", 120_000));

            Ok(Arc::new(CommandOutboundSender {
                program,
                args_template,
                timeout,
            }))
        }
        other => Err(format!(
            "unsupported OUTBOUND_SENDER_MODE: {other} (expected: stdout | command | discord)"
        )),
    }
}

fn build_discord_channel(assistant_name: &str) -> Result<DiscordChannel, String> {
    let bot_token = read_env("DISCORD_BOT_TOKEN")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "DISCORD_BOT_TOKEN is required when OUTBOUND_SENDER_MODE=discord".to_string()
        })?;

    Ok(DiscordChannel::new(DiscordChannelConfig {
        bot_token,
        reconnect_delay: Duration::from_millis(read_u64("DISCORD_RECONNECT_DELAY_MS", 5_000)),
        assistant_name: assistant_name.to_string(),
        prefix_outbound: read_bool("DISCORD_PREFIX_OUTBOUND", true),
    }))
}

fn maybe_start_inbound_bridge(
    orchestrator: Arc<Orchestrator>,
) -> Result<Option<ManagedTask>, String> {
    let Some(program) = read_env("INBOUND_BRIDGE_PROGRAM") else {
        return Ok(None);
    };

    let args_raw = read_env("INBOUND_BRIDGE_ARGS_JSON").unwrap_or_else(|| "[]".to_string());
    let args = serde_json::from_str::<Vec<String>>(&args_raw)
        .map_err(|err| format!("invalid INBOUND_BRIDGE_ARGS_JSON: {err}"))?;

    let bridge = CommandInboundBridge { program, args };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let result = bridge
            .run_with_shutdown(Some(shutdown_rx), |event| {
                let orchestrator = orchestrator.clone();
                async move {
                    match event {
                        InboundBridgeEvent::Message { message } => orchestrator
                            .ingest_message(message)
                            .await
                            .map_err(|err| err.to_string()),
                        InboundBridgeEvent::ChatMetadata {
                            chat_jid,
                            timestamp,
                            name,
                        } => orchestrator
                            .ingest_chat_metadata(&chat_jid, &timestamp, name.as_deref())
                            .await
                            .map_err(|err| err.to_string()),
                    }
                }
            })
            .await;
        if let Err(error) = result {
            eprintln!("inbound bridge exited with error: {error}");
        }
    });

    Ok(Some(ManagedTask {
        shutdown_tx,
        handle: task,
    }))
}

async fn wait_for_task_completion(mut task: tokio::task::JoinHandle<()>, label: &str) {
    if tokio::time::timeout(Duration::from_millis(750), &mut task)
        .await
        .is_ok()
    {
        return;
    }

    eprintln!("{label} shutdown timed out; aborting task");
    task.abort();
    let _ = task.await;
}

fn maybe_auto_register_main_group(
    db: &Db,
    assistant_name: &str,
    main_group_folder: &str,
) -> Result<(), String> {
    let Some(main_jid) = read_env("AUTO_REGISTER_MAIN_JID") else {
        return Ok(());
    };

    let existing = db
        .get_registered_group(&main_jid)
        .map_err(|err| format!("failed to check existing main group: {err}"))?;
    if existing.is_some() {
        return Ok(());
    }

    let group = RegisteredGroup {
        name: "Main".to_string(),
        folder: main_group_folder.to_string(),
        trigger: format!("@{assistant_name}"),
        added_at: Utc::now().to_rfc3339(),
        container_config: None,
        requires_trigger: Some(false),
    };
    db.set_registered_group(&main_jid, &group)
        .map_err(|err| format!("failed to auto-register main group: {err}"))?;
    println!(
        "auto-registered main group: jid={} folder={}",
        main_jid, group.folder
    );
    Ok(())
}

fn parse_timestamp(raw: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(raw)
        .map(|datetime| datetime.with_timezone(&Utc))
        .map_err(|err| format!("invalid RFC3339 timestamp '{raw}': {err}"))
}

fn default_trigger() -> String {
    format!(
        "@{}",
        read_env("ASSISTANT_NAME").unwrap_or_else(|| "Andy".to_string())
    )
}

fn db_path_from_env() -> String {
    read_env("RUST_CLAW_DB").unwrap_or_else(|| "store/messages.db".to_string())
}

fn data_dir_from_env() -> std::path::PathBuf {
    read_env("DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| ".".into())
                .join("data")
        })
}

fn acquire_runtime_lock(data_dir: &Path) -> Result<File, String> {
    fs::create_dir_all(data_dir)
        .map_err(|err| format!("failed to create data dir {}: {err}", data_dir.display()))?;
    let lock_path = data_dir.join(".run.lock");
    let mut lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|err| format!("failed to open runtime lock {}: {err}", lock_path.display()))?;

    lock_file.try_lock_exclusive().map_err(|err| {
        format!(
            "another rust-claw daemon is already running (lock: {}): {err}",
            lock_path.display()
        )
    })?;

    lock_file.set_len(0).map_err(|err| {
        format!(
            "failed to truncate runtime lock {}: {err}",
            lock_path.display()
        )
    })?;
    writeln!(lock_file, "pid={}", std::process::id()).map_err(|err| {
        format!(
            "failed to write runtime lock metadata {}: {err}",
            lock_path.display()
        )
    })?;
    writeln!(lock_file, "started_at={}", Utc::now().to_rfc3339()).map_err(|err| {
        format!(
            "failed to write runtime lock metadata {}: {err}",
            lock_path.display()
        )
    })?;

    Ok(lock_file)
}

fn open_db_with_json_migration() -> Result<Db, String> {
    let db_path = db_path_from_env();
    let db = Db::open(&db_path).map_err(|err| format!("failed to open DB at {db_path}: {err}"))?;
    let report = migrate_json_state(&db, &data_dir_from_env())?;
    if report.has_changes() {
        println!(
            "migrated legacy JSON state: files={} router_keys={} sessions={} groups={}",
            report.migrated_files.len(),
            report.router_state_keys,
            report.sessions,
            report.groups
        );
    }
    Ok(db)
}

fn read_env(key: &str) -> Option<String> {
    read_env_var(key)
}

fn read_u64(key: &str, default_value: u64) -> u64 {
    read_env(key)
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(default_value)
}

fn read_usize(key: &str, default_value: usize) -> usize {
    read_env(key)
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(default_value)
}

fn read_bool(key: &str, default_value: bool) -> bool {
    read_env(key)
        .map(|raw| match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default_value,
        })
        .unwrap_or(default_value)
}

async fn ensure_container_runtime_ready() -> Result<(), String> {
    let container_binary = read_env("CONTAINER_BINARY").unwrap_or_else(|| "docker".to_string());
    match detect_container_runtime(&container_binary) {
        ContainerRuntime::Docker => {
            run_checked_command(&container_binary, &["info"], Duration::from_secs(10))
                .await
                .map_err(|err| {
                    format!(
                        "docker runtime check failed: {err}. start Docker Desktop/daemon and retry"
                    )
                })?;
            println!("docker runtime is running");
            Ok(())
        }
        ContainerRuntime::AppleContainer => {
            if run_checked_command(
                &container_binary,
                &["system", "status"],
                Duration::from_secs(5),
            )
            .await
            .is_ok()
            {
                println!("container runtime already running");
                return Ok(());
            }

            println!("starting container runtime...");
            run_checked_command(
                &container_binary,
                &["system", "start"],
                Duration::from_millis(read_u64("CONTAINER_SYSTEM_START_TIMEOUT_MS", 30_000)),
            )
            .await
            .map_err(|err| format!("container runtime start failed: {err}"))?;

            run_checked_command(
                &container_binary,
                &["system", "status"],
                Duration::from_secs(5),
            )
            .await
            .map_err(|err| format!("container runtime status check failed after start: {err}"))?;

            println!("container runtime started");
            Ok(())
        }
    }
}

async fn cleanup_orphaned_containers() -> Result<(), String> {
    let container_binary = read_env("CONTAINER_BINARY").unwrap_or_else(|| "docker".to_string());
    let orphan_names = match detect_container_runtime(&container_binary) {
        ContainerRuntime::Docker => list_docker_orphans(&container_binary).await?,
        ContainerRuntime::AppleContainer => list_apple_container_orphans(&container_binary).await?,
    };

    if orphan_names.is_empty() {
        return Ok(());
    }

    let mut stopped = Vec::new();
    for name in orphan_names {
        let mut stop_command = tokio::process::Command::new(&container_binary);
        stop_command.arg("stop").arg(&name);
        let stop_output = tokio::time::timeout(Duration::from_secs(15), stop_command.output())
            .await
            .map_err(|_| format!("'container stop {name}' timed out after 15000ms"))?
            .map_err(|err| format!("failed to spawn 'container stop {name}': {err}"))?;
        if stop_output.status.success() {
            stopped.push(name);
        }
    }

    if !stopped.is_empty() {
        println!("cleaned orphaned containers: {}", stopped.join(", "));
    }
    Ok(())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ContainerRuntime {
    Docker,
    AppleContainer,
}

fn detect_container_runtime(container_binary: &str) -> ContainerRuntime {
    let file_name = std::path::Path::new(container_binary)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(container_binary)
        .to_ascii_lowercase();
    if file_name.contains("docker") {
        ContainerRuntime::Docker
    } else {
        ContainerRuntime::AppleContainer
    }
}

async fn list_docker_orphans(container_binary: &str) -> Result<Vec<String>, String> {
    let mut list_command = tokio::process::Command::new(container_binary);
    list_command.args(["ps", "--format", "{{.Names}}"]);
    let output = tokio::time::timeout(Duration::from_secs(10), list_command.output())
        .await
        .map_err(|_| {
            format!("'{container_binary} ps --format {{.Names}}' timed out after 10000ms")
        })?
        .map_err(|err| {
            format!("failed to spawn '{container_binary} ps --format {{.Names}}': {err}")
        })?;
    if !output.status.success() {
        return Err(format!(
            "'{container_binary} ps --format {{.Names}}' exited with code {:?}; stderr: {}",
            output.status.code(),
            tail_chars(&String::from_utf8_lossy(&output.stderr), 200)
        ));
    }

    let names = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter(|name| name.starts_with("nanoclaw-"))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    Ok(names)
}

async fn list_apple_container_orphans(container_binary: &str) -> Result<Vec<String>, String> {
    let mut list_command = tokio::process::Command::new(container_binary);
    list_command.args(["ls", "--format", "json"]);
    let output = tokio::time::timeout(Duration::from_secs(10), list_command.output())
        .await
        .map_err(|_| "'container ls --format json' timed out after 10000ms".to_string())?
        .map_err(|err| format!("failed to spawn 'container ls --format json': {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "'container ls --format json' exited with code {:?}; stderr: {}",
            output.status.code(),
            tail_chars(&String::from_utf8_lossy(&output.stderr), 200)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Ok(Vec::new());
    }

    let parsed = serde_json::from_str::<serde_json::Value>(&stdout)
        .map_err(|err| format!("failed to parse container list JSON: {err}"))?;
    let Some(items) = parsed.as_array() else {
        return Ok(Vec::new());
    };

    Ok(items
        .iter()
        .filter_map(extract_container_name)
        .filter(|name| name.starts_with("nanoclaw-"))
        .collect::<Vec<_>>())
}

async fn run_checked_command(
    program: &str,
    args: &[&str],
    timeout_duration: Duration,
) -> Result<(), String> {
    let mut command = tokio::process::Command::new(program);
    command.args(args);
    let output = tokio::time::timeout(timeout_duration, command.output())
        .await
        .map_err(|_| {
            format!(
                "'{} {}' timed out after {}ms",
                program,
                args.join(" "),
                timeout_duration.as_millis()
            )
        })?
        .map_err(|err| format!("failed to spawn '{} {}': {err}", program, args.join(" ")))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Err(format!(
        "'{} {}' exited with code {:?}; stderr: {}; stdout: {}",
        program,
        args.join(" "),
        output.status.code(),
        tail_chars(&stderr, 200),
        tail_chars(&stdout, 200)
    ))
}

fn tail_chars(value: &str, max_chars: usize) -> String {
    let total_chars = value.chars().count();
    if total_chars <= max_chars {
        return value.to_string();
    }
    value.chars().skip(total_chars - max_chars).collect()
}

fn extract_container_name(value: &serde_json::Value) -> Option<String> {
    value
        .get("configuration")
        .and_then(|cfg| cfg.get("id"))
        .and_then(|id| id.as_str())
        .map(ToString::to_string)
        .or_else(|| {
            value
                .get("name")
                .and_then(|name| name.as_str())
                .map(ToString::to_string)
        })
        .or_else(|| {
            value
                .get("id")
                .and_then(|id| id.as_str())
                .map(ToString::to_string)
        })
}
