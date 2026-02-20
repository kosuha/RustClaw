use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{
    DateTime, Duration as ChronoDuration, FixedOffset, Local, NaiveDateTime, Offset, TimeZone, Utc,
};
use chrono_tz::Tz;
use cron::Schedule;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::cron_schedule::parse_cron_schedule;
use crate::db::Db;
use crate::env_file::read_env_var;
use crate::orchestrator::Orchestrator;
use crate::types::{ContextMode, RegisteredGroup, ScheduleType, ScheduledTask, TaskStatus};

const ERROR_DIR_NAME: &str = "errors";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(1_000);
static TASK_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn start_ipc_watcher(
    orchestrator: Arc<Orchestrator>,
    db: Db,
    ipc_base_dir: PathBuf,
    poll_interval: Option<Duration>,
) -> JoinHandle<()> {
    let poll_interval = poll_interval.unwrap_or(DEFAULT_POLL_INTERVAL);
    tokio::spawn(async move {
        if let Err(err) = fs::create_dir_all(&ipc_base_dir) {
            eprintln!(
                "failed to create IPC base directory {}: {err}",
                ipc_base_dir.display()
            );
        }

        loop {
            if let Err(err) = process_ipc_once(&orchestrator, &db, &ipc_base_dir).await {
                eprintln!("ipc watcher iteration failed: {err}");
            }
            sleep(poll_interval).await;
        }
    })
}

async fn process_ipc_once(
    orchestrator: &Orchestrator,
    db: &Db,
    ipc_base_dir: &Path,
) -> Result<(), String> {
    let mut group_dirs = collect_group_dirs(ipc_base_dir)?;
    group_dirs.sort();

    let main_group_folder = orchestrator.main_group_folder().to_string();
    for source_group in group_dirs {
        let is_main = source_group == main_group_folder;
        process_group_ipc_dir(
            orchestrator,
            db,
            ipc_base_dir,
            &source_group,
            "messages",
            is_main,
        )
        .await;
        process_group_ipc_dir(
            orchestrator,
            db,
            ipc_base_dir,
            &source_group,
            "tasks",
            is_main,
        )
        .await;
    }

    Ok(())
}

async fn process_group_ipc_dir(
    orchestrator: &Orchestrator,
    db: &Db,
    ipc_base_dir: &Path,
    source_group: &str,
    kind: &str,
    is_main: bool,
) {
    let group_dir = ipc_base_dir.join(source_group).join(kind);
    let files = match collect_json_files(&group_dir) {
        Ok(files) => files,
        Err(err) => {
            eprintln!(
                "failed to read IPC {} directory for {}: {err}",
                kind, source_group
            );
            return;
        }
    };

    if files.is_empty() {
        return;
    }

    for file_path in files {
        let payload = match read_json_value(&file_path) {
            Ok(value) => value,
            Err(err) => {
                eprintln!(
                    "failed to parse IPC {} payload {}: {err}",
                    kind,
                    file_path.display()
                );
                if let Err(move_err) = move_to_errors(ipc_base_dir, source_group, &file_path) {
                    eprintln!(
                        "failed to move malformed IPC {} file {} to errors: {}",
                        kind,
                        file_path.display(),
                        move_err
                    );
                }
                continue;
            }
        };

        let process_result = match kind {
            "messages" => {
                process_message_payload(orchestrator, db, source_group, is_main, &payload).await
            }
            "tasks" => {
                process_task_payload(orchestrator, db, source_group, is_main, &payload).await
            }
            _ => Ok(()),
        };
        if let Err(err) = process_result {
            eprintln!(
                "failed to process IPC {} payload {}: {err}",
                kind,
                file_path.display()
            );
            if let Err(move_err) = move_to_errors(ipc_base_dir, source_group, &file_path) {
                eprintln!(
                    "failed to move errored IPC {} file {} to errors: {}",
                    kind,
                    file_path.display(),
                    move_err
                );
            }
            continue;
        }

        if let Err(err) = fs::remove_file(&file_path) {
            if err.kind() == std::io::ErrorKind::NotFound {
                continue;
            }
            eprintln!(
                "failed to delete processed IPC {} file {}: {err}",
                kind,
                file_path.display()
            );
            if let Err(move_err) = move_to_errors(ipc_base_dir, source_group, &file_path) {
                eprintln!(
                    "failed to move undeleted IPC {} file {} to errors: {}",
                    kind,
                    file_path.display(),
                    move_err
                );
            }
        }
    }
}

fn collect_group_dirs(ipc_base_dir: &Path) -> Result<Vec<String>, String> {
    let entries = fs::read_dir(ipc_base_dir)
        .map_err(|err| format!("read_dir {} failed: {err}", ipc_base_dir.display()))?;
    let mut groups = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("read_dir entry failed: {err}"))?;
        let file_type = entry
            .file_type()
            .map_err(|err| format!("file_type check failed: {err}"))?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ERROR_DIR_NAME {
            continue;
        }
        groups.push(name.to_string());
    }
    Ok(groups)
}

fn collect_json_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    let entries =
        fs::read_dir(dir).map_err(|err| format!("read_dir {} failed: {err}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("read_dir entry failed: {err}"))?;
        let file_type = entry
            .file_type()
            .map_err(|err| format!("file_type check failed: {err}"))?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        let is_json = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        if is_json {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn read_json_value(path: &Path) -> Result<Value, String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("read file {} failed: {err}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|err| format!("invalid JSON payload in {}: {err}", path.display()))
}

fn move_to_errors(
    ipc_base_dir: &Path,
    source_group: &str,
    source_file: &Path,
) -> Result<(), String> {
    if !source_file.exists() {
        return Ok(());
    }
    let error_dir = ipc_base_dir.join(ERROR_DIR_NAME);
    fs::create_dir_all(&error_dir)
        .map_err(|err| format!("create error dir {} failed: {err}", error_dir.display()))?;

    let Some(file_name) = source_file.file_name().and_then(|value| value.to_str()) else {
        return Err(format!(
            "cannot derive filename for IPC file {}",
            source_file.display()
        ));
    };

    let mut destination = error_dir.join(format!("{source_group}-{file_name}"));
    if destination.exists() {
        destination = error_dir.join(format!(
            "{source_group}-{}-{file_name}",
            Utc::now().timestamp_millis()
        ));
    }

    match fs::rename(source_file, &destination) {
        Ok(()) => Ok(()),
        Err(err) => {
            if err.kind() == std::io::ErrorKind::NotFound {
                return Ok(());
            }
            fs::copy(source_file, &destination).map_err(|copy_err| {
                format!("copy to {} failed: {copy_err}", destination.display())
            })?;
            if let Err(remove_err) = fs::remove_file(source_file) {
                if remove_err.kind() != std::io::ErrorKind::NotFound {
                    return Err(format!(
                        "delete original IPC file {} after copy failed: {remove_err}",
                        source_file.display()
                    ));
                }
            }
            Ok(())
        }
    }
}

async fn process_message_payload(
    orchestrator: &Orchestrator,
    db: &Db,
    source_group: &str,
    is_main: bool,
    payload: &Value,
) -> Result<(), String> {
    let payload_type = string_field(payload, &["type"]);
    if payload_type.as_deref() != Some("message") {
        return Ok(());
    }

    let Some(chat_jid) = string_field(payload, &["chatJid", "chat_jid"]) else {
        return Ok(());
    };
    let Some(text) = string_field(payload, &["text"]) else {
        return Ok(());
    };

    if !is_main {
        let registered = db
            .get_all_registered_groups()
            .map_err(|err| format!("load registered groups for message auth failed: {err}"))?;
        let authorized = registered
            .get(&chat_jid)
            .map(|group| group.folder == source_group)
            .unwrap_or(false);
        if !authorized {
            return Ok(());
        }
    }

    orchestrator
        .send_outbound_message(&chat_jid, &text)
        .await
        .map_err(|err| format!("outbound send failed: {err}"))
}

async fn process_task_payload(
    orchestrator: &Orchestrator,
    db: &Db,
    source_group: &str,
    is_main: bool,
    payload: &Value,
) -> Result<(), String> {
    let Some(payload_type) = string_field(payload, &["type"]) else {
        return Ok(());
    };

    match payload_type.as_str() {
        "schedule_task" => {
            process_schedule_task(orchestrator, db, source_group, is_main, payload).await
        }
        "pause_task" => {
            set_task_status_if_authorized(db, source_group, is_main, payload, TaskStatus::Paused)
        }
        "resume_task" => {
            set_task_status_if_authorized(db, source_group, is_main, payload, TaskStatus::Active)
        }
        "cancel_task" => cancel_task_if_authorized(db, source_group, is_main, payload),
        "enable_skill" => {
            set_skill_enabled_if_authorized(orchestrator, db, source_group, is_main, payload, true)
                .await
        }
        "disable_skill" => {
            set_skill_enabled_if_authorized(orchestrator, db, source_group, is_main, payload, false)
                .await
        }
        "register_group" => {
            register_group_if_authorized(orchestrator, source_group, is_main, payload).await
        }
        "refresh_groups" => refresh_groups_if_authorized(orchestrator, source_group, is_main).await,
        "refresh_skills" => refresh_skills_if_authorized(orchestrator, source_group).await,
        _ => Ok(()),
    }
}

async fn process_schedule_task(
    _orchestrator: &Orchestrator,
    db: &Db,
    source_group: &str,
    is_main: bool,
    payload: &Value,
) -> Result<(), String> {
    let Some(prompt) = string_field(payload, &["prompt"]) else {
        return Ok(());
    };
    let Some(schedule_type_raw) = string_field(payload, &["schedule_type", "scheduleType"]) else {
        return Ok(());
    };
    let Some(schedule_value) = string_field(payload, &["schedule_value", "scheduleValue"]) else {
        return Ok(());
    };
    let Some(target_jid) =
        string_field(payload, &["targetJid", "target_jid", "chatJid", "chat_jid"])
    else {
        return Ok(());
    };

    let registered_groups = db
        .get_all_registered_groups()
        .map_err(|err| format!("load registered groups for schedule_task failed: {err}"))?;
    let Some(target_group) = registered_groups.get(&target_jid) else {
        return Ok(());
    };
    if !is_main && target_group.folder != source_group {
        return Ok(());
    }

    let schedule_type = match ScheduleType::from_str(&schedule_type_raw) {
        Some(value) => value,
        None => return Ok(()),
    };

    let now = Utc::now();
    let next_run = match compute_initial_run(schedule_type, &schedule_value, now) {
        Ok(value) => value,
        Err(err) => {
            eprintln!(
                "ignored schedule_task due to invalid schedule (type={}, value={}): {}",
                schedule_type.as_str(),
                schedule_value,
                err
            );
            return Ok(());
        }
    };

    let context_mode = match string_field(payload, &["context_mode", "contextMode"]).as_deref() {
        Some("group") => ContextMode::Group,
        Some("isolated") => ContextMode::Isolated,
        _ => ContextMode::Isolated,
    };

    let task = ScheduledTask {
        id: next_task_id(),
        group_folder: target_group.folder.clone(),
        chat_jid: target_jid,
        prompt,
        schedule_type,
        schedule_value,
        context_mode,
        next_run: Some(next_run),
        last_run: None,
        last_result: None,
        status: TaskStatus::Active,
        created_at: now,
    };
    db.create_task(&task)
        .map_err(|err| format!("create IPC task failed: {err}"))?;
    Ok(())
}

fn set_task_status_if_authorized(
    db: &Db,
    source_group: &str,
    is_main: bool,
    payload: &Value,
    status: TaskStatus,
) -> Result<(), String> {
    let Some(task_id) = string_field(payload, &["taskId", "task_id"]) else {
        return Ok(());
    };

    let Some(task) = db
        .get_task_by_id(&task_id)
        .map_err(|err| format!("load task {task_id} failed: {err}"))?
    else {
        return Ok(());
    };
    if !is_main && task.group_folder != source_group {
        return Ok(());
    }

    db.set_task_status(&task_id, status)
        .map_err(|err| format!("set status for task {task_id} failed: {err}"))?;
    Ok(())
}

fn cancel_task_if_authorized(
    db: &Db,
    source_group: &str,
    is_main: bool,
    payload: &Value,
) -> Result<(), String> {
    let Some(task_id) = string_field(payload, &["taskId", "task_id"]) else {
        return Ok(());
    };

    let Some(task) = db
        .get_task_by_id(&task_id)
        .map_err(|err| format!("load task {task_id} failed: {err}"))?
    else {
        return Ok(());
    };
    if !is_main && task.group_folder != source_group {
        return Ok(());
    }

    db.delete_task(&task_id)
        .map_err(|err| format!("delete task {task_id} failed: {err}"))?;
    Ok(())
}

async fn register_group_if_authorized(
    orchestrator: &Orchestrator,
    _source_group: &str,
    is_main: bool,
    payload: &Value,
) -> Result<(), String> {
    if !is_main {
        return Ok(());
    }

    let Some(jid) = string_field(payload, &["jid"]) else {
        return Ok(());
    };
    let Some(name) = string_field(payload, &["name"]) else {
        return Ok(());
    };
    let Some(folder) = string_field(payload, &["folder"]) else {
        return Ok(());
    };
    let Some(trigger) = string_field(payload, &["trigger"]) else {
        return Ok(());
    };

    let container_config = payload
        .get("containerConfig")
        .or_else(|| payload.get("container_config"))
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|err| format!("invalid register_group container config: {err}"))?;

    let group = RegisteredGroup {
        name,
        folder,
        trigger,
        added_at: Utc::now().to_rfc3339(),
        container_config,
        requires_trigger: bool_field(payload, &["requiresTrigger", "requires_trigger"]),
    };
    orchestrator
        .register_group(&jid, group)
        .await
        .map_err(|err| format!("register_group failed: {err}"))?;
    Ok(())
}

async fn refresh_groups_if_authorized(
    orchestrator: &Orchestrator,
    source_group: &str,
    is_main: bool,
) -> Result<(), String> {
    if !is_main {
        return Ok(());
    }
    orchestrator
        .refresh_runtime_snapshots(source_group)
        .await
        .map_err(|err| format!("refresh_groups failed: {err}"))
}

async fn set_skill_enabled_if_authorized(
    orchestrator: &Orchestrator,
    db: &Db,
    source_group: &str,
    is_main: bool,
    payload: &Value,
    enabled: bool,
) -> Result<(), String> {
    let Some(skill_name) = string_field(payload, &["skillName", "skill_name"]) else {
        return Ok(());
    };

    let requested_group =
        string_field(payload, &["targetGroupFolder", "target_group_folder"]).unwrap_or_default();
    let target_group = if is_main {
        if requested_group.is_empty() {
            source_group.to_string()
        } else {
            requested_group
        }
    } else {
        if !requested_group.is_empty() && requested_group != source_group {
            return Ok(());
        }
        source_group.to_string()
    };

    db.set_skill_enabled(&target_group, &skill_name, enabled)
        .map_err(|err| {
            format!(
                "failed to set skill state for {} in group {}: {}",
                skill_name, target_group, err
            )
        })?;
    orchestrator
        .refresh_runtime_snapshots(source_group)
        .await
        .map_err(|err| format!("refresh skill snapshot failed: {err}"))?;
    Ok(())
}

async fn refresh_skills_if_authorized(
    orchestrator: &Orchestrator,
    source_group: &str,
) -> Result<(), String> {
    orchestrator
        .refresh_runtime_snapshots(source_group)
        .await
        .map_err(|err| format!("refresh_skills failed: {err}"))
}

fn string_field(payload: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = payload.get(*key).and_then(Value::as_str) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn bool_field(payload: &Value, keys: &[&str]) -> Option<bool> {
    for key in keys {
        if let Some(value) = payload.get(*key).and_then(Value::as_bool) {
            return Some(value);
        }
    }
    None
}

fn compute_initial_run(
    schedule_type: ScheduleType,
    schedule_value: &str,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, String> {
    match schedule_type {
        ScheduleType::Cron => {
            let schedule = parse_cron_schedule(schedule_value)?;
            next_cron_run(&schedule, now, resolve_task_timezone())
                .ok_or_else(|| "cron expression produced no next run".to_string())
        }
        ScheduleType::Interval => {
            let interval_ms = schedule_value
                .parse::<i64>()
                .map_err(|_| format!("invalid interval value: {schedule_value}"))?;
            if interval_ms <= 0 {
                return Err(format!("interval must be > 0: {schedule_value}"));
            }
            now.checked_add_signed(ChronoDuration::milliseconds(interval_ms))
                .ok_or_else(|| "interval overflow while computing next_run".to_string())
        }
        ScheduleType::Once => parse_once_timestamp(schedule_value, resolve_task_timezone()),
    }
}

fn parse_once_timestamp(
    schedule_value: &str,
    timezone: TaskTimezone,
) -> Result<DateTime<Utc>, String> {
    if let Ok(value) = DateTime::parse_from_rfc3339(schedule_value) {
        return Ok(value.with_timezone(&Utc));
    }

    let naive = NaiveDateTime::parse_from_str(schedule_value, "%Y-%m-%dT%H:%M:%S%.f")
        .map_err(|err| format!("invalid once timestamp: {err}"))?;
    local_datetime_to_utc(naive, timezone).ok_or_else(|| {
        format!(
            "invalid once timestamp for timezone {}: {}",
            timezone_label(timezone),
            schedule_value
        )
    })
}

fn local_datetime_to_utc(naive: NaiveDateTime, timezone: TaskTimezone) -> Option<DateTime<Utc>> {
    match timezone {
        TaskTimezone::Named(tz) => {
            let local = tz.from_local_datetime(&naive);
            local
                .single()
                .or_else(|| local.earliest())
                .map(|value| value.with_timezone(&Utc))
        }
        TaskTimezone::Fixed(offset) => {
            let local = offset.from_local_datetime(&naive);
            local
                .single()
                .or_else(|| local.earliest())
                .map(|value| value.with_timezone(&Utc))
        }
    }
}

fn timezone_label(timezone: TaskTimezone) -> String {
    match timezone {
        TaskTimezone::Named(tz) => tz.name().to_string(),
        TaskTimezone::Fixed(offset) => offset.to_string(),
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

fn next_task_id() -> String {
    let counter = TASK_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("task-{}-{counter}", Utc::now().timestamp_millis())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::orchestrator::{
        EchoAgentRunner, OrchestratorConfig, OutboundSender, SendFuture, StdoutOutboundSender,
    };

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

    fn sample_group(name: &str, folder: &str, requires_trigger: bool) -> RegisteredGroup {
        RegisteredGroup {
            name: name.to_string(),
            folder: folder.to_string(),
            trigger: "@Andy".to_string(),
            added_at: "2026-02-19T00:00:00.000Z".to_string(),
            container_config: None,
            requires_trigger: Some(requires_trigger),
        }
    }

    async fn create_orchestrator(
        db: &Db,
        ipc_base_dir: Option<PathBuf>,
        outbound: Arc<dyn OutboundSender>,
    ) -> Arc<Orchestrator> {
        Orchestrator::create(
            db.clone(),
            OrchestratorConfig {
                ipc_base_dir,
                ..OrchestratorConfig::default()
            },
            Arc::new(EchoAgentRunner),
            outbound,
        )
        .await
        .expect("create orchestrator")
    }

    #[tokio::test]
    async fn process_once_deletes_valid_files_and_moves_invalid_to_errors() {
        let tmp = tempdir().expect("tempdir");
        let db = Db::open(tmp.path().join("messages.db")).expect("open db");
        db.set_registered_group("other@g.us", &sample_group("Other", "other", true))
            .expect("set registered group");

        let outbound = Arc::new(MockOutbound::default());
        let orchestrator = create_orchestrator(&db, None, outbound.clone()).await;

        let ipc_base = tmp.path().join("ipc");
        let messages_dir = ipc_base.join("other").join("messages");
        fs::create_dir_all(&messages_dir).expect("create messages dir");

        let valid_file = messages_dir.join("001-valid.json");
        fs::write(
            &valid_file,
            json!({
                "type": "message",
                "chatJid": "other@g.us",
                "text": "hello from ipc"
            })
            .to_string(),
        )
        .expect("write valid file");

        let invalid_file = messages_dir.join("002-invalid.json");
        fs::write(&invalid_file, "{not-json").expect("write invalid file");

        process_ipc_once(&orchestrator, &db, &ipc_base)
            .await
            .expect("process ipc once");

        assert!(!valid_file.exists());
        assert!(!invalid_file.exists());

        let error_dir = ipc_base.join(ERROR_DIR_NAME);
        assert!(error_dir.exists());
        let moved = fs::read_dir(&error_dir)
            .expect("read error dir")
            .map(|entry| entry.expect("error dir entry").path())
            .collect::<Vec<_>>();
        assert_eq!(moved.len(), 1);
        assert!(
            moved[0]
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("other-002-invalid.json"))
                .unwrap_or(false)
        );

        let sent = outbound.messages.lock().expect("messages lock");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "other@g.us");
    }

    #[tokio::test]
    async fn non_main_authorization_blocks_cross_group_actions() {
        let tmp = tempdir().expect("tempdir");
        let db = Db::open(tmp.path().join("messages.db")).expect("open db");
        db.set_registered_group("main@g.us", &sample_group("Main", "main", false))
            .expect("set main");
        db.set_registered_group("other@g.us", &sample_group("Other", "other", true))
            .expect("set other");

        let outbound = Arc::new(MockOutbound::default());
        let orchestrator = create_orchestrator(&db, None, outbound.clone()).await;

        let main_task = ScheduledTask {
            id: "task-main".to_string(),
            group_folder: "main".to_string(),
            chat_jid: "main@g.us".to_string(),
            prompt: "main task".to_string(),
            schedule_type: ScheduleType::Once,
            schedule_value: "2026-02-20T00:00:00.000Z".to_string(),
            context_mode: ContextMode::Isolated,
            next_run: Some(Utc::now()),
            last_run: None,
            last_result: None,
            status: TaskStatus::Active,
            created_at: Utc::now(),
        };
        db.create_task(&main_task).expect("create main task");

        process_message_payload(
            &orchestrator,
            &db,
            "other",
            false,
            &json!({"type":"message","chatJid":"main@g.us","text":"nope"}),
        )
        .await
        .expect("process unauthorized message");

        process_message_payload(
            &orchestrator,
            &db,
            "other",
            false,
            &json!({"type":"message","chatJid":"other@g.us","text":"ok"}),
        )
        .await
        .expect("process authorized message");

        process_task_payload(
            &orchestrator,
            &db,
            "other",
            false,
            &json!({"type":"pause_task","taskId":"task-main"}),
        )
        .await
        .expect("pause task");

        process_task_payload(
            &orchestrator,
            &db,
            "other",
            false,
            &json!({
                "type":"schedule_task",
                "prompt":"cross",
                "schedule_type":"once",
                "schedule_value":"2026-02-20T00:00:00.000Z",
                "targetJid":"main@g.us"
            }),
        )
        .await
        .expect("schedule task");

        process_task_payload(
            &orchestrator,
            &db,
            "other",
            false,
            &json!({
                "type":"disable_skill",
                "skillName":"research",
                "targetGroupFolder":"main"
            }),
        )
        .await
        .expect("disable cross-group skill");

        process_task_payload(
            &orchestrator,
            &db,
            "other",
            false,
            &json!({
                "type":"disable_skill",
                "skillName":"research"
            }),
        )
        .await
        .expect("disable own-group skill");

        let main_after = db
            .get_task_by_id("task-main")
            .expect("get task")
            .expect("task exists");
        assert_eq!(main_after.status, TaskStatus::Active);

        let all_tasks = db.get_all_tasks().expect("get all tasks");
        assert_eq!(all_tasks.len(), 1);

        let main_skills = db.get_skill_settings("main").expect("main skill settings");
        assert!(!main_skills.contains_key("research"));
        let other_skills = db
            .get_skill_settings("other")
            .expect("other skill settings");
        assert_eq!(other_skills.get("research"), Some(&false));

        let sent = outbound.messages.lock().expect("messages lock");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "other@g.us");
    }

    #[tokio::test]
    async fn main_group_can_register_group_and_refresh_snapshots() {
        let tmp = tempdir().expect("tempdir");
        let db = Db::open(tmp.path().join("messages.db")).expect("open db");
        db.set_registered_group("main@g.us", &sample_group("Main", "main", false))
            .expect("set main");
        db.store_chat_metadata("main@g.us", "2026-02-19T00:00:00.000Z", Some("Main"))
            .expect("store chat metadata");

        let ipc_base = tmp.path().join("ipc");
        let orchestrator =
            create_orchestrator(&db, Some(ipc_base.clone()), Arc::new(StdoutOutboundSender)).await;

        process_task_payload(
            &orchestrator,
            &db,
            "other",
            false,
            &json!({
                "type":"register_group",
                "jid":"blocked@g.us",
                "name":"Blocked",
                "folder":"blocked",
                "trigger":"@Andy"
            }),
        )
        .await
        .expect("non-main register_group");
        assert!(
            db.get_registered_group("blocked@g.us")
                .expect("load blocked")
                .is_none()
        );

        process_task_payload(
            &orchestrator,
            &db,
            "main",
            true,
            &json!({
                "type":"register_group",
                "jid":"new@g.us",
                "name":"New Group",
                "folder":"new-group",
                "trigger":"@Andy",
                "requiresTrigger":true
            }),
        )
        .await
        .expect("main register_group");
        assert!(
            db.get_registered_group("new@g.us")
                .expect("load new group")
                .is_some()
        );

        process_task_payload(
            &orchestrator,
            &db,
            "main",
            true,
            &json!({"type":"refresh_groups"}),
        )
        .await
        .expect("refresh groups");

        let snapshot_path = ipc_base.join("main").join("available_groups.json");
        assert!(snapshot_path.exists());
        let skills_snapshot = ipc_base.join("main").join("current_skills.json");
        assert!(skills_snapshot.exists());
    }

    #[test]
    fn move_to_errors_ignores_missing_source_file() {
        let tmp = tempdir().expect("tempdir");
        let source = tmp
            .path()
            .join("main")
            .join("messages")
            .join("missing.json");
        let result = move_to_errors(tmp.path(), "main", &source);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_once_timestamp_accepts_local_timestamp_without_timezone() {
        let timezone = TaskTimezone::Named("Asia/Seoul".parse::<Tz>().expect("tz parse"));
        let parsed =
            parse_once_timestamp("2026-02-19T22:00:00", timezone).expect("parse once timestamp");
        assert_eq!(parsed.to_rfc3339(), "2026-02-19T13:00:00+00:00");
    }

    #[test]
    fn parse_once_timestamp_accepts_rfc3339_timestamp() {
        let timezone = TaskTimezone::Named("Asia/Seoul".parse::<Tz>().expect("tz parse"));
        let parsed = parse_once_timestamp("2026-02-19T22:00:00+09:00", timezone)
            .expect("parse once timestamp");
        assert_eq!(parsed.to_rfc3339(), "2026-02-19T13:00:00+00:00");
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
