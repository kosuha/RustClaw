use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, Row, params};
use serde_json::Value as JsonValue;

use crate::types::{
    ChatInfo, ContextMode, NewMessage, RegisteredGroup, ScheduleType, ScheduledTask, TaskRunLog,
    TaskRunStatus, TaskStatus,
};

#[derive(Debug)]
pub enum DbError {
    Sql(rusqlite::Error),
    Io(std::io::Error),
    Serialization(serde_json::Error),
    InvalidEnum { field: &'static str, value: String },
    InvalidTimestamp { field: &'static str, value: String },
}

impl Display for DbError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Sql(err) => write!(f, "database error: {err}"),
            DbError::Io(err) => write!(f, "io error: {err}"),
            DbError::Serialization(err) => write!(f, "serialization error: {err}"),
            DbError::InvalidEnum { field, value } => {
                write!(f, "invalid enum value for {field}: {value}")
            }
            DbError::InvalidTimestamp { field, value } => {
                write!(f, "invalid timestamp value for {field}: {value}")
            }
        }
    }
}

impl std::error::Error for DbError {}

impl From<rusqlite::Error> for DbError {
    fn from(value: rusqlite::Error) -> Self {
        DbError::Sql(value)
    }
}

impl From<std::io::Error> for DbError {
    fn from(value: std::io::Error) -> Self {
        DbError::Io(value)
    }
}

impl From<serde_json::Error> for DbError {
    fn from(value: serde_json::Error) -> Self {
        DbError::Serialization(value)
    }
}

#[derive(Clone, Debug)]
pub struct Db {
    path: PathBuf,
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&path)?;
        create_schema(&connection)?;
        Ok(Self { path })
    }

    pub fn store_chat_metadata(
        &self,
        chat_jid: &str,
        timestamp: &str,
        name: Option<&str>,
    ) -> Result<(), DbError> {
        self.with_connection(|conn| {
            if let Some(name) = name {
                conn.execute(
                    "INSERT INTO chats (jid, name, last_message_time) VALUES (?, ?, ?)
                     ON CONFLICT(jid) DO UPDATE SET
                       name = excluded.name,
                       last_message_time = MAX(last_message_time, excluded.last_message_time)",
                    params![chat_jid, name, timestamp],
                )?;
            } else {
                conn.execute(
                    "INSERT INTO chats (jid, name, last_message_time) VALUES (?, ?, ?)
                     ON CONFLICT(jid) DO UPDATE SET
                       last_message_time = MAX(last_message_time, excluded.last_message_time)",
                    params![chat_jid, chat_jid, timestamp],
                )?;
            }
            Ok(())
        })
    }

    pub fn update_chat_name(
        &self,
        chat_jid: &str,
        name: &str,
        timestamp: &str,
    ) -> Result<(), DbError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO chats (jid, name, last_message_time) VALUES (?, ?, ?)
                 ON CONFLICT(jid) DO UPDATE SET name = excluded.name",
                params![chat_jid, name, timestamp],
            )?;
            Ok(())
        })
    }

    pub fn get_last_group_sync(&self) -> Result<Option<String>, DbError> {
        self.with_connection(|conn| {
            let mut statement =
                conn.prepare("SELECT last_message_time FROM chats WHERE jid = '__group_sync__'")?;
            let mut rows = statement.query([])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            Ok(Some(row.get("last_message_time")?))
        })
    }

    pub fn set_last_group_sync(&self, timestamp: &str) -> Result<(), DbError> {
        self.store_chat_metadata("__group_sync__", timestamp, Some("__group_sync__"))
    }

    pub fn get_all_chats(&self) -> Result<Vec<ChatInfo>, DbError> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare(
                "SELECT jid, name, last_message_time
                 FROM chats
                 ORDER BY last_message_time DESC",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok(ChatInfo {
                        jid: row.get("jid")?,
                        name: row.get("name")?,
                        last_message_time: row.get("last_message_time")?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn store_message(&self, message: &NewMessage) -> Result<(), DbError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO messages (
                    id,
                    chat_jid,
                    sender,
                    sender_name,
                    content,
                    timestamp,
                    is_from_me,
                    is_bot_message
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    message.id,
                    message.chat_jid,
                    message.sender,
                    message.sender_name,
                    message.content,
                    message.timestamp,
                    if message.is_from_me { 1 } else { 0 },
                    if message.is_bot_message { 1 } else { 0 },
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_new_messages(
        &self,
        jids: &[String],
        last_timestamp: &str,
        bot_prefix: &str,
    ) -> Result<(Vec<NewMessage>, String), DbError> {
        if jids.is_empty() {
            return Ok((Vec::new(), last_timestamp.to_string()));
        }

        self.with_connection(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, chat_jid, sender, sender_name, content, timestamp, is_from_me, is_bot_message
                 FROM messages
                 WHERE timestamp > ?
                   AND is_bot_message = 0
                   AND content NOT LIKE ?
                 ORDER BY timestamp",
            )?;

            let bot_filter = format!("{bot_prefix}:%");
            let all_rows = statement
                .query_map(params![last_timestamp, bot_filter], row_to_message)?
                .collect::<Result<Vec<_>, _>>()?;
            let jid_set = jids.iter().collect::<HashSet<_>>();

            let mut filtered = Vec::new();
            let mut newest = last_timestamp.to_string();
            for message in all_rows {
                if jid_set.contains(&message.chat_jid) {
                    if message.timestamp > newest {
                        newest = message.timestamp.clone();
                    }
                    filtered.push(message);
                }
            }
            Ok((filtered, newest))
        })
    }

    pub fn get_messages_since(
        &self,
        chat_jid: &str,
        since_timestamp: &str,
        bot_prefix: &str,
    ) -> Result<Vec<NewMessage>, DbError> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, chat_jid, sender, sender_name, content, timestamp, is_from_me, is_bot_message
                 FROM messages
                 WHERE chat_jid = ?
                   AND timestamp > ?
                   AND is_bot_message = 0
                   AND content NOT LIKE ?
                 ORDER BY timestamp",
            )?;
            let messages = statement
                .query_map(
                    params![chat_jid, since_timestamp, format!("{bot_prefix}:%")],
                    row_to_message,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(messages)
        })
    }

    pub fn create_task(&self, task: &ScheduledTask) -> Result<(), DbError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO scheduled_tasks (
                    id,
                    group_folder,
                    chat_jid,
                    prompt,
                    schedule_type,
                    schedule_value,
                    context_mode,
                    next_run,
                    last_run,
                    last_result,
                    status,
                    created_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    task.id,
                    task.group_folder,
                    task.chat_jid,
                    task.prompt,
                    task.schedule_type.as_str(),
                    task.schedule_value,
                    task.context_mode.as_str(),
                    task.next_run.map(to_iso8601),
                    task.last_run.map(to_iso8601),
                    task.last_result,
                    task.status.as_str(),
                    to_iso8601(task.created_at),
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_task_by_id(&self, id: &str) -> Result<Option<ScheduledTask>, DbError> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare("SELECT * FROM scheduled_tasks WHERE id = ?")?;
            let mut rows = statement.query(params![id])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            Ok(Some(row_to_task(row)?))
        })
    }

    pub fn get_all_tasks(&self) -> Result<Vec<ScheduledTask>, DbError> {
        self.with_connection(|conn| {
            let mut statement =
                conn.prepare("SELECT * FROM scheduled_tasks ORDER BY created_at DESC")?;
            let tasks = statement
                .query_map([], row_to_task)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(tasks)
        })
    }

    pub fn get_due_tasks(&self, now: DateTime<Utc>) -> Result<Vec<ScheduledTask>, DbError> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare(
                "SELECT * FROM scheduled_tasks
                 WHERE status = 'active' AND next_run IS NOT NULL AND next_run <= ?
                 ORDER BY next_run",
            )?;
            let now_iso = to_iso8601(now);
            let tasks = statement
                .query_map(params![now_iso], row_to_task)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(tasks)
        })
    }

    pub fn update_task_after_run(
        &self,
        id: &str,
        next_run: Option<DateTime<Utc>>,
        last_result: &str,
        run_at: DateTime<Utc>,
    ) -> Result<(), DbError> {
        self.with_connection(|conn| {
            let next_run_value = next_run.map(to_iso8601);
            conn.execute(
                "UPDATE scheduled_tasks
                 SET next_run = ?,
                     last_run = ?,
                     last_result = ?,
                     status = CASE WHEN ? IS NULL THEN 'completed' ELSE status END
                 WHERE id = ?",
                params![
                    next_run_value,
                    to_iso8601(run_at),
                    last_result,
                    next_run_value,
                    id
                ],
            )?;
            Ok(())
        })
    }

    pub fn log_task_run(&self, log: &TaskRunLog) -> Result<(), DbError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO task_run_logs (task_id, run_at, duration_ms, status, result, error)
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    log.task_id,
                    to_iso8601(log.run_at),
                    log.duration_ms as i64,
                    log.status.as_str(),
                    log.result,
                    log.error
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_task_run_logs(&self, task_id: &str) -> Result<Vec<TaskRunLog>, DbError> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare(
                "SELECT task_id, run_at, duration_ms, status, result, error
                 FROM task_run_logs
                 WHERE task_id = ?
                 ORDER BY run_at",
            )?;
            let logs = statement
                .query_map(params![task_id], row_to_task_log)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(logs)
        })
    }

    pub fn set_task_status(&self, id: &str, status: TaskStatus) -> Result<(), DbError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE scheduled_tasks SET status = ? WHERE id = ?",
                params![status.as_str(), id],
            )?;
            Ok(())
        })
    }

    pub fn delete_task(&self, id: &str) -> Result<(), DbError> {
        self.with_connection(|conn| {
            conn.execute("DELETE FROM task_run_logs WHERE task_id = ?", params![id])?;
            conn.execute("DELETE FROM scheduled_tasks WHERE id = ?", params![id])?;
            Ok(())
        })
    }

    pub fn get_router_state(&self, key: &str) -> Result<Option<String>, DbError> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare("SELECT value FROM router_state WHERE key = ?")?;
            let mut rows = statement.query(params![key])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            Ok(Some(row.get("value")?))
        })
    }

    pub fn set_router_state(&self, key: &str, value: &str) -> Result<(), DbError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO router_state (key, value) VALUES (?, ?)",
                params![key, value],
            )?;
            Ok(())
        })
    }

    pub fn get_session(&self, group_folder: &str) -> Result<Option<String>, DbError> {
        self.with_connection(|conn| {
            let mut statement =
                conn.prepare("SELECT session_id FROM sessions WHERE group_folder = ?")?;
            let mut rows = statement.query(params![group_folder])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            Ok(Some(row.get("session_id")?))
        })
    }

    pub fn set_session(&self, group_folder: &str, session_id: &str) -> Result<(), DbError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO sessions (group_folder, session_id) VALUES (?, ?)",
                params![group_folder, session_id],
            )?;
            Ok(())
        })
    }

    pub fn get_all_sessions(&self) -> Result<HashMap<String, String>, DbError> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare("SELECT group_folder, session_id FROM sessions")?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>("group_folder")?,
                        row.get::<_, String>("session_id")?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows.into_iter().collect())
        })
    }

    pub fn get_registered_group(&self, jid: &str) -> Result<Option<RegisteredGroup>, DbError> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare("SELECT * FROM registered_groups WHERE jid = ?")?;
            let mut rows = statement.query(params![jid])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            let (_, group) = row_to_registered_group(row)?;
            Ok(Some(group))
        })
    }

    pub fn set_registered_group(&self, jid: &str, group: &RegisteredGroup) -> Result<(), DbError> {
        self.with_connection(|conn| {
            let container_config = group
                .container_config
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let requires_trigger = match group.requires_trigger {
                None => 1,
                Some(true) => 1,
                Some(false) => 0,
            };
            conn.execute(
                "INSERT OR REPLACE INTO registered_groups (
                    jid,
                    name,
                    folder,
                    trigger_pattern,
                    added_at,
                    container_config,
                    requires_trigger
                 ) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    jid,
                    group.name,
                    group.folder,
                    group.trigger,
                    group.added_at,
                    container_config,
                    requires_trigger
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_all_registered_groups(&self) -> Result<HashMap<String, RegisteredGroup>, DbError> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare("SELECT * FROM registered_groups")?;
            let rows = statement
                .query_map([], row_to_registered_group)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows.into_iter().collect())
        })
    }

    pub fn get_skill_settings(&self, group_folder: &str) -> Result<HashMap<String, bool>, DbError> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare(
                "SELECT skill_name, enabled
                 FROM group_skill_settings
                 WHERE group_folder = ?",
            )?;
            let rows = statement
                .query_map(params![group_folder], |row| {
                    let name: String = row.get("skill_name")?;
                    let enabled: i64 = row.get("enabled")?;
                    Ok((name, enabled == 1))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows.into_iter().collect())
        })
    }

    pub fn set_skill_enabled(
        &self,
        group_folder: &str,
        skill_name: &str,
        enabled: bool,
    ) -> Result<(), DbError> {
        let normalized = normalize_skill_name(skill_name);
        if normalized.is_empty() {
            return Ok(());
        }

        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO group_skill_settings (group_folder, skill_name, enabled, updated_at)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(group_folder, skill_name) DO UPDATE SET
                   enabled = excluded.enabled,
                   updated_at = excluded.updated_at",
                params![
                    group_folder,
                    normalized,
                    if enabled { 1 } else { 0 },
                    to_iso8601(Utc::now()),
                ],
            )?;
            Ok(())
        })
    }

    fn with_connection<T, F>(&self, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&Connection) -> Result<T, DbError>,
    {
        let connection = Connection::open(&self.path)?;
        f(&connection)
    }
}

fn create_schema(connection: &Connection) -> Result<(), DbError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS chats (
          jid TEXT PRIMARY KEY,
          name TEXT,
          last_message_time TEXT
        );

        CREATE TABLE IF NOT EXISTS messages (
          id TEXT NOT NULL,
          chat_jid TEXT NOT NULL,
          sender TEXT NOT NULL,
          sender_name TEXT NOT NULL,
          content TEXT NOT NULL,
          timestamp TEXT NOT NULL,
          is_from_me INTEGER NOT NULL DEFAULT 0,
          is_bot_message INTEGER NOT NULL DEFAULT 0,
          PRIMARY KEY (id, chat_jid),
          FOREIGN KEY (chat_jid) REFERENCES chats(jid)
        );
        CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp);

        CREATE TABLE IF NOT EXISTS scheduled_tasks (
          id TEXT PRIMARY KEY,
          group_folder TEXT NOT NULL,
          chat_jid TEXT NOT NULL,
          prompt TEXT NOT NULL,
          schedule_type TEXT NOT NULL,
          schedule_value TEXT NOT NULL,
          context_mode TEXT NOT NULL DEFAULT 'isolated',
          next_run TEXT,
          last_run TEXT,
          last_result TEXT,
          status TEXT NOT NULL DEFAULT 'active',
          created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_scheduled_tasks_next_run
          ON scheduled_tasks(next_run);
        CREATE INDEX IF NOT EXISTS idx_scheduled_tasks_status
          ON scheduled_tasks(status);

        CREATE TABLE IF NOT EXISTS task_run_logs (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          task_id TEXT NOT NULL,
          run_at TEXT NOT NULL,
          duration_ms INTEGER NOT NULL,
          status TEXT NOT NULL,
          result TEXT,
          error TEXT,
          FOREIGN KEY (task_id) REFERENCES scheduled_tasks(id)
        );
        CREATE INDEX IF NOT EXISTS idx_task_run_logs_task_id_run_at
          ON task_run_logs(task_id, run_at);

        CREATE TABLE IF NOT EXISTS router_state (
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sessions (
          group_folder TEXT PRIMARY KEY,
          session_id TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS registered_groups (
          jid TEXT PRIMARY KEY,
          name TEXT NOT NULL,
          folder TEXT NOT NULL UNIQUE,
          trigger_pattern TEXT NOT NULL,
          added_at TEXT NOT NULL,
          container_config TEXT,
          requires_trigger INTEGER DEFAULT 1
        );

        CREATE TABLE IF NOT EXISTS group_skill_settings (
          group_folder TEXT NOT NULL,
          skill_name TEXT NOT NULL,
          enabled INTEGER NOT NULL DEFAULT 1,
          updated_at TEXT NOT NULL,
          PRIMARY KEY (group_folder, skill_name)
        );
        CREATE INDEX IF NOT EXISTS idx_group_skill_settings_group
          ON group_skill_settings(group_folder);
        ",
    )?;
    Ok(())
}

fn row_to_message(row: &Row<'_>) -> Result<NewMessage, rusqlite::Error> {
    let is_from_me: i64 = row.get("is_from_me")?;
    let is_bot_message: i64 = row.get("is_bot_message")?;
    Ok(NewMessage {
        id: row.get("id")?,
        chat_jid: row.get("chat_jid")?,
        sender: row.get("sender")?,
        sender_name: row.get("sender_name")?,
        content: row.get("content")?,
        timestamp: row.get("timestamp")?,
        is_from_me: is_from_me == 1,
        is_bot_message: is_bot_message == 1,
    })
}

fn row_to_task(row: &Row<'_>) -> Result<ScheduledTask, rusqlite::Error> {
    let schedule_type_raw: String = row.get("schedule_type")?;
    let schedule_type = ScheduleType::from_str(&schedule_type_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(DbError::InvalidEnum {
                field: "schedule_type",
                value: schedule_type_raw.clone(),
            }),
        )
    })?;

    let context_mode_raw: String = row.get("context_mode")?;
    let context_mode = ContextMode::from_str(&context_mode_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(DbError::InvalidEnum {
                field: "context_mode",
                value: context_mode_raw.clone(),
            }),
        )
    })?;

    let status_raw: String = row.get("status")?;
    let status = TaskStatus::from_str(&status_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(DbError::InvalidEnum {
                field: "status",
                value: status_raw.clone(),
            }),
        )
    })?;

    let next_run = row
        .get::<_, Option<String>>("next_run")?
        .map(|value| parse_iso8601("next_run", &value))
        .transpose()
        .map_err(wrap_conversion_error)?;

    let last_run = row
        .get::<_, Option<String>>("last_run")?
        .map(|value| parse_iso8601("last_run", &value))
        .transpose()
        .map_err(wrap_conversion_error)?;

    let created_at_raw: String = row.get("created_at")?;
    let created_at = parse_iso8601("created_at", &created_at_raw).map_err(wrap_conversion_error)?;

    Ok(ScheduledTask {
        id: row.get("id")?,
        group_folder: row.get("group_folder")?,
        chat_jid: row.get("chat_jid")?,
        prompt: row.get("prompt")?,
        schedule_type,
        schedule_value: row.get("schedule_value")?,
        context_mode,
        next_run,
        last_run,
        last_result: row.get("last_result")?,
        status,
        created_at,
    })
}

fn row_to_task_log(row: &Row<'_>) -> Result<TaskRunLog, rusqlite::Error> {
    let run_at_raw: String = row.get("run_at")?;
    let run_at = parse_iso8601("run_at", &run_at_raw).map_err(wrap_conversion_error)?;

    let status_raw: String = row.get("status")?;
    let status = match status_raw.as_str() {
        "success" => TaskRunStatus::Success,
        "error" => TaskRunStatus::Error,
        _ => {
            return Err(wrap_conversion_error(DbError::InvalidEnum {
                field: "status",
                value: status_raw,
            }));
        }
    };

    let duration_ms: i64 = row.get("duration_ms")?;
    Ok(TaskRunLog {
        task_id: row.get("task_id")?,
        run_at,
        duration_ms: duration_ms.max(0) as u64,
        status,
        result: row.get("result")?,
        error: row.get("error")?,
    })
}

fn row_to_registered_group(row: &Row<'_>) -> Result<(String, RegisteredGroup), rusqlite::Error> {
    let container_config_raw: Option<String> = row.get("container_config")?;
    let container_config = container_config_raw
        .as_ref()
        .map(|raw| serde_json::from_str::<JsonValue>(raw))
        .transpose()
        .map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(DbError::Serialization(err)),
            )
        })?
        .map(|json| serde_json::from_value(json))
        .transpose()
        .map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(DbError::Serialization(err)),
            )
        })?;

    let requires_trigger_value: Option<i64> = row.get("requires_trigger")?;
    let requires_trigger = requires_trigger_value.map(|value| value == 1);

    Ok((
        row.get("jid")?,
        RegisteredGroup {
            name: row.get("name")?,
            folder: row.get("folder")?,
            trigger: row.get("trigger_pattern")?,
            added_at: row.get("added_at")?,
            container_config,
            requires_trigger,
        },
    ))
}

fn parse_iso8601(field: &'static str, value: &str) -> Result<DateTime<Utc>, DbError> {
    DateTime::parse_from_rfc3339(value)
        .map(|datetime| datetime.with_timezone(&Utc))
        .map_err(|_| DbError::InvalidTimestamp {
            field,
            value: value.to_string(),
        })
}

fn wrap_conversion_error(error: DbError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn to_iso8601(datetime: DateTime<Utc>) -> String {
    datetime.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn normalize_skill_name(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use chrono::Duration as ChronoDuration;
    use tempfile::tempdir;

    use super::*;
    use crate::types::{AdditionalMount, ContainerConfig};

    fn build_task(
        id: &str,
        schedule_type: ScheduleType,
        next_run: Option<DateTime<Utc>>,
        status: TaskStatus,
    ) -> ScheduledTask {
        let now = Utc::now();
        ScheduledTask {
            id: id.to_string(),
            group_folder: "main".to_string(),
            chat_jid: "chat@g.us".to_string(),
            prompt: "run task".to_string(),
            schedule_type,
            schedule_value: match schedule_type {
                ScheduleType::Cron => "0 9 * * *".to_string(),
                ScheduleType::Interval => "60000".to_string(),
                ScheduleType::Once => "once".to_string(),
            },
            context_mode: ContextMode::Isolated,
            next_run,
            last_run: None,
            last_result: None,
            status,
            created_at: now,
        }
    }

    fn message(
        id: &str,
        chat_jid: &str,
        content: &str,
        timestamp: &str,
        is_bot_message: bool,
    ) -> NewMessage {
        NewMessage {
            id: id.to_string(),
            chat_jid: chat_jid.to_string(),
            sender: "user".to_string(),
            sender_name: "User".to_string(),
            content: content.to_string(),
            timestamp: timestamp.to_string(),
            is_from_me: false,
            is_bot_message,
        }
    }

    #[test]
    fn get_due_tasks_returns_only_active_due_tasks() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("messages.db")).expect("open db");
        let now = Utc::now();

        let due = build_task(
            "due",
            ScheduleType::Interval,
            Some(now - ChronoDuration::seconds(5)),
            TaskStatus::Active,
        );
        db.create_task(&due).expect("create due");

        let paused = build_task(
            "paused",
            ScheduleType::Interval,
            Some(now - ChronoDuration::seconds(5)),
            TaskStatus::Paused,
        );
        db.create_task(&paused).expect("create paused");

        let future = build_task(
            "future",
            ScheduleType::Interval,
            Some(now + ChronoDuration::seconds(10)),
            TaskStatus::Active,
        );
        db.create_task(&future).expect("create future");

        let due_tasks = db.get_due_tasks(now).expect("get due");
        assert_eq!(due_tasks.len(), 1);
        assert_eq!(due_tasks[0].id, "due");
    }

    #[test]
    fn update_task_after_run_marks_once_task_completed_when_next_run_missing() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("messages.db")).expect("open db");
        let now = Utc::now();

        let task = build_task(
            "once-task",
            ScheduleType::Once,
            Some(now - ChronoDuration::seconds(1)),
            TaskStatus::Active,
        );
        db.create_task(&task).expect("create task");

        db.update_task_after_run("once-task", None, "done", now)
            .expect("update task");

        let saved = db
            .get_task_by_id("once-task")
            .expect("get task")
            .expect("task exists");
        assert_eq!(saved.status, TaskStatus::Completed);
        assert!(saved.last_run.is_some());
        assert_eq!(saved.last_result.as_deref(), Some("done"));
    }

    #[test]
    fn log_task_run_persists_execution_result() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("messages.db")).expect("open db");
        let now = Utc::now();
        let task = build_task(
            "task-1",
            ScheduleType::Interval,
            Some(now),
            TaskStatus::Active,
        );
        db.create_task(&task).expect("create task");

        db.log_task_run(&TaskRunLog {
            task_id: "task-1".to_string(),
            run_at: now,
            duration_ms: 321,
            status: TaskRunStatus::Success,
            result: Some("ok".to_string()),
            error: None,
        })
        .expect("log run");

        let logs = db.get_task_run_logs("task-1").expect("get logs");
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].duration_ms, 321);
        assert_eq!(logs[0].status, TaskRunStatus::Success);
        assert_eq!(logs[0].result.as_deref(), Some("ok"));
    }

    #[test]
    fn get_new_messages_filters_bot_messages_and_prefix() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("messages.db")).expect("open db");
        db.store_chat_metadata("group@g.us", "2026-02-19T10:00:00.000Z", Some("Group"))
            .expect("store chat metadata");

        db.store_message(&message(
            "1",
            "group@g.us",
            "hello",
            "2026-02-19T10:00:01.000Z",
            false,
        ))
        .expect("store message 1");
        db.store_message(&message(
            "2",
            "group@g.us",
            "Andy: internal",
            "2026-02-19T10:00:02.000Z",
            false,
        ))
        .expect("store message 2");
        db.store_message(&message(
            "3",
            "group@g.us",
            "hidden",
            "2026-02-19T10:00:03.000Z",
            true,
        ))
        .expect("store message 3");

        let (messages, latest) = db
            .get_new_messages(&["group@g.us".to_string()], "", "Andy")
            .expect("get new messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "1");
        assert_eq!(latest, "2026-02-19T10:00:01.000Z");
    }

    #[test]
    fn sessions_and_router_state_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("messages.db")).expect("open db");

        db.set_router_state("last_timestamp", "2026-02-19T10:00:00.000Z")
            .expect("set router state");
        db.set_session("main", "session-1").expect("set session");

        let state = db.get_router_state("last_timestamp").expect("get state");
        assert_eq!(state.as_deref(), Some("2026-02-19T10:00:00.000Z"));

        let sessions = db.get_all_sessions().expect("get sessions");
        assert_eq!(sessions.get("main").map(String::as_str), Some("session-1"));
    }

    #[test]
    fn group_sync_timestamp_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("messages.db")).expect("open db");

        assert!(db.get_last_group_sync().expect("initial sync").is_none());
        db.set_last_group_sync("2026-02-19T12:00:00.000Z")
            .expect("set group sync");
        assert_eq!(
            db.get_last_group_sync().expect("load sync").as_deref(),
            Some("2026-02-19T12:00:00.000Z")
        );
    }

    #[test]
    fn skill_settings_roundtrip_normalizes_name_and_updates_state() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("messages.db")).expect("open db");

        db.set_skill_enabled("main", "Research", false)
            .expect("disable skill");
        db.set_skill_enabled("main", "  research  ", true)
            .expect("enable skill");

        let settings = db.get_skill_settings("main").expect("get settings");
        assert_eq!(settings.len(), 1);
        assert_eq!(settings.get("research"), Some(&true));
    }

    #[test]
    fn registered_group_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("messages.db")).expect("open db");

        db.set_registered_group(
            "group@g.us",
            &RegisteredGroup {
                name: "Dev Team".to_string(),
                folder: "dev-team".to_string(),
                trigger: "@Andy".to_string(),
                added_at: "2026-02-19T10:00:00.000Z".to_string(),
                container_config: Some(ContainerConfig {
                    additional_mounts: vec![AdditionalMount {
                        host_path: "/tmp/project".to_string(),
                        container_path: Some("project".to_string()),
                        readonly: Some(true),
                    }],
                    timeout_ms: Some(600000),
                }),
                requires_trigger: Some(true),
            },
        )
        .expect("set registered group");

        let groups = db
            .get_all_registered_groups()
            .expect("get all registered groups");
        let group = groups.get("group@g.us").expect("group exists");
        assert_eq!(group.folder, "dev-team");
        assert_eq!(group.trigger, "@Andy");
        assert_eq!(group.requires_trigger, Some(true));
        assert_eq!(
            group
                .container_config
                .as_ref()
                .and_then(|cfg| cfg.timeout_ms),
            Some(600000)
        );
    }
}
