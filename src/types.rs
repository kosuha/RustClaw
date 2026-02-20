use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewMessage {
    pub id: String,
    pub chat_jid: String,
    pub sender: String,
    pub sender_name: String,
    pub content: String,
    pub timestamp: String,
    #[serde(default, alias = "isFromMe")]
    pub is_from_me: bool,
    #[serde(default, alias = "isBotMessage")]
    pub is_bot_message: bool,
}

pub type ChannelResult = Result<(), String>;
pub type ChannelFuture<'a> = Pin<Box<dyn Future<Output = ChannelResult> + Send + 'a>>;

pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    fn is_connected(&self) -> bool;
    fn owns_jid(&self, jid: &str) -> bool;
    fn send_message<'a>(&'a self, jid: &'a str, text: &'a str) -> ChannelFuture<'a>;
    fn set_typing<'a>(&'a self, jid: &'a str, is_typing: bool) -> ChannelFuture<'a> {
        let _ = (jid, is_typing);
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdditionalMount {
    #[serde(rename = "hostPath", alias = "host_path")]
    pub host_path: String,
    #[serde(rename = "containerPath", alias = "container_path")]
    pub container_path: Option<String>,
    pub readonly: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerConfig {
    #[serde(rename = "additionalMounts", alias = "additional_mounts", default)]
    pub additional_mounts: Vec<AdditionalMount>,
    #[serde(rename = "timeout", alias = "timeoutMs", alias = "timeout_ms")]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredGroup {
    pub name: String,
    pub folder: String,
    pub trigger: String,
    pub added_at: String,
    #[serde(rename = "containerConfig", alias = "container_config")]
    pub container_config: Option<ContainerConfig>,
    #[serde(rename = "requiresTrigger", alias = "requires_trigger")]
    pub requires_trigger: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatInfo {
    pub jid: String,
    pub name: String,
    pub last_message_time: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleType {
    Cron,
    Interval,
    Once,
}

impl ScheduleType {
    pub fn as_str(self) -> &'static str {
        match self {
            ScheduleType::Cron => "cron",
            ScheduleType::Interval => "interval",
            ScheduleType::Once => "once",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "cron" => Some(ScheduleType::Cron),
            "interval" => Some(ScheduleType::Interval),
            "once" => Some(ScheduleType::Once),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextMode {
    Group,
    Isolated,
}

impl ContextMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextMode::Group => "group",
            ContextMode::Isolated => "isolated",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "group" => Some(ContextMode::Group),
            "isolated" => Some(ContextMode::Isolated),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Active,
    Paused,
    Completed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Active => "active",
            TaskStatus::Paused => "paused",
            TaskStatus::Completed => "completed",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "active" => Some(TaskStatus::Active),
            "paused" => Some(TaskStatus::Paused),
            "completed" => Some(TaskStatus::Completed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledTask {
    pub id: String,
    pub group_folder: String,
    pub chat_jid: String,
    pub prompt: String,
    pub schedule_type: ScheduleType,
    pub schedule_value: String,
    pub context_mode: ContextMode,
    pub next_run: Option<DateTime<Utc>>,
    pub last_run: Option<DateTime<Utc>>,
    pub last_result: Option<String>,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskRunStatus {
    Success,
    Error,
}

impl TaskRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskRunStatus::Success => "success",
            TaskRunStatus::Error => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRunLog {
    pub task_id: String,
    pub run_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub status: TaskRunStatus,
    pub result: Option<String>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_message_deserializes_with_camel_case_flags() {
        let json = r#"{
          "id":"m1",
          "chat_jid":"group@g.us",
          "sender":"u1",
          "sender_name":"Alice",
          "content":"hello",
          "timestamp":"2026-02-19T00:00:01.000Z",
          "isFromMe":false,
          "isBotMessage":true
        }"#;

        let message: NewMessage = serde_json::from_str(json).expect("deserialize new message");
        assert!(!message.is_from_me);
        assert!(message.is_bot_message);
    }

    #[test]
    fn registered_group_deserializes_from_nanoclaw_style_json() {
        let json = r#"{
          "name":"Dev Team",
          "folder":"dev-team",
          "trigger":"@Andy",
          "added_at":"2026-02-19T00:00:00.000Z",
          "requiresTrigger":true,
          "containerConfig":{
            "additionalMounts":[
              {"hostPath":"/tmp/project","containerPath":"project","readonly":false}
            ],
            "timeout":600000
          }
        }"#;

        let group: RegisteredGroup =
            serde_json::from_str(json).expect("deserialize registered group");
        let config = group.container_config.expect("container config");
        assert_eq!(config.additional_mounts.len(), 1);
        assert_eq!(config.additional_mounts[0].host_path, "/tmp/project");
        assert_eq!(config.timeout_ms, Some(600000));
        assert_eq!(group.requires_trigger, Some(true));
    }

    #[test]
    fn registered_group_serializes_container_config_with_camel_case_keys() {
        let group = RegisteredGroup {
            name: "Main".to_string(),
            folder: "main".to_string(),
            trigger: "@Andy".to_string(),
            added_at: "2026-02-19T00:00:00.000Z".to_string(),
            container_config: Some(ContainerConfig {
                additional_mounts: vec![AdditionalMount {
                    host_path: "/tmp/work".to_string(),
                    container_path: Some("work".to_string()),
                    readonly: Some(true),
                }],
                timeout_ms: Some(120000),
            }),
            requires_trigger: Some(false),
        };

        let json = serde_json::to_string(&group).expect("serialize group");
        assert!(json.contains("\"containerConfig\""));
        assert!(json.contains("\"additionalMounts\""));
        assert!(json.contains("\"hostPath\""));
    }
}
