use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::db::Db;
use crate::types::RegisteredGroup;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JsonStateMigrationReport {
    pub migrated_files: Vec<PathBuf>,
    pub router_state_keys: usize,
    pub sessions: usize,
    pub groups: usize,
}

impl JsonStateMigrationReport {
    pub fn has_changes(&self) -> bool {
        !self.migrated_files.is_empty()
    }
}

#[derive(Debug, Deserialize)]
struct RouterStateJson {
    last_timestamp: Option<String>,
    last_agent_timestamp: Option<HashMap<String, String>>,
}

pub fn migrate_json_state(db: &Db, data_dir: &Path) -> Result<JsonStateMigrationReport, String> {
    let mut report = JsonStateMigrationReport::default();

    migrate_router_state(db, data_dir, &mut report)?;
    migrate_sessions(db, data_dir, &mut report)?;
    migrate_registered_groups(db, data_dir, &mut report)?;

    Ok(report)
}

fn migrate_router_state(
    db: &Db,
    data_dir: &Path,
    report: &mut JsonStateMigrationReport,
) -> Result<(), String> {
    let file_path = data_dir.join("router_state.json");
    if !file_path.exists() {
        return Ok(());
    }

    let state: RouterStateJson = read_json_file(&file_path)?;
    if let Some(last_timestamp) = state.last_timestamp {
        db.set_router_state("last_timestamp", &last_timestamp)
            .map_err(|err| format!("failed to migrate router_state last_timestamp: {err}"))?;
        report.router_state_keys += 1;
    }
    if let Some(last_agent_timestamp) = state.last_agent_timestamp {
        let serialized = serde_json::to_string(&last_agent_timestamp).map_err(|err| {
            format!("failed to serialize router_state last_agent_timestamp: {err}")
        })?;
        db.set_router_state("last_agent_timestamp", &serialized)
            .map_err(|err| format!("failed to migrate router_state last_agent_timestamp: {err}"))?;
        report.router_state_keys += 1;
    }

    let archived_path = archive_migrated_file(&file_path)?;
    report.migrated_files.push(archived_path);
    Ok(())
}

fn migrate_sessions(
    db: &Db,
    data_dir: &Path,
    report: &mut JsonStateMigrationReport,
) -> Result<(), String> {
    let file_path = data_dir.join("sessions.json");
    if !file_path.exists() {
        return Ok(());
    }

    let sessions: HashMap<String, String> = read_json_file(&file_path)?;
    for (group_folder, session_id) in sessions {
        if group_folder.trim().is_empty() || session_id.trim().is_empty() {
            continue;
        }
        db.set_session(&group_folder, &session_id).map_err(|err| {
            format!("failed to migrate session for folder '{group_folder}': {err}")
        })?;
        report.sessions += 1;
    }

    let archived_path = archive_migrated_file(&file_path)?;
    report.migrated_files.push(archived_path);
    Ok(())
}

fn migrate_registered_groups(
    db: &Db,
    data_dir: &Path,
    report: &mut JsonStateMigrationReport,
) -> Result<(), String> {
    let file_path = data_dir.join("registered_groups.json");
    if !file_path.exists() {
        return Ok(());
    }

    let groups: HashMap<String, RegisteredGroup> = read_json_file(&file_path)?;
    for (jid, group) in groups {
        if jid.trim().is_empty() {
            continue;
        }
        db.set_registered_group(&jid, &group)
            .map_err(|err| format!("failed to migrate registered group '{jid}': {err}"))?;
        report.groups += 1;
    }

    let archived_path = archive_migrated_file(&file_path)?;
    report.migrated_files.push(archived_path);
    Ok(())
}

fn read_json_file<T: DeserializeOwned>(file_path: &Path) -> Result<T, String> {
    let raw = fs::read_to_string(file_path).map_err(|err| {
        format!(
            "failed to read migration file '{}': {err}",
            file_path.display()
        )
    })?;
    serde_json::from_str(&raw).map_err(|err| {
        format!(
            "failed to parse migration file '{}': {err}",
            file_path.display()
        )
    })
}

fn archive_migrated_file(file_path: &Path) -> Result<PathBuf, String> {
    let file_name = file_path
        .file_name()
        .and_then(|value| value.to_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| file_path.to_string_lossy().to_string());

    let mut candidate = file_path.with_file_name(format!("{file_name}.migrated"));
    if candidate.exists() {
        let mut index = 1;
        loop {
            let next = file_path.with_file_name(format!("{file_name}.migrated.{index}"));
            if !next.exists() {
                candidate = next;
                break;
            }
            index += 1;
        }
    }

    fs::rename(file_path, &candidate).map_err(|err| {
        format!(
            "failed to archive migrated file '{}' to '{}': {err}",
            file_path.display(),
            candidate.display()
        )
    })?;

    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn migrates_legacy_json_state_files_once() {
        let dir = tempdir().expect("tempdir");
        let data_dir = dir.path().join("data");
        fs::create_dir_all(&data_dir).expect("create data dir");

        fs::write(
            data_dir.join("router_state.json"),
            json!({
                "last_timestamp": "2026-02-19T10:00:00.000Z",
                "last_agent_timestamp": {
                    "main@local": "2026-02-19T10:00:01.000Z"
                }
            })
            .to_string(),
        )
        .expect("write router_state");
        fs::write(
            data_dir.join("sessions.json"),
            json!({
                "main": "session-main",
                "dev-team": "session-dev"
            })
            .to_string(),
        )
        .expect("write sessions");
        fs::write(
            data_dir.join("registered_groups.json"),
            json!({
                "main@local": {
                    "name": "Main",
                    "folder": "main",
                    "trigger": "@Andy",
                    "added_at": "2026-02-19T10:00:00.000Z",
                    "requiresTrigger": false
                },
                "12345@g.us": {
                    "name": "Dev Team",
                    "folder": "dev-team",
                    "trigger": "@Andy",
                    "added_at": "2026-02-19T10:00:00.000Z",
                    "requiresTrigger": true,
                    "containerConfig": {
                        "additionalMounts": [
                            {
                                "hostPath": "/tmp/project",
                                "containerPath": "project",
                                "readonly": true
                            }
                        ],
                        "timeout": 600000
                    }
                }
            })
            .to_string(),
        )
        .expect("write groups");

        let db = Db::open(dir.path().join("store/messages.db")).expect("open db");

        let first = migrate_json_state(&db, &data_dir).expect("migrate json state");
        assert!(first.has_changes());
        assert_eq!(first.migrated_files.len(), 3);
        assert_eq!(first.router_state_keys, 2);
        assert_eq!(first.sessions, 2);
        assert_eq!(first.groups, 2);

        assert_eq!(
            db.get_router_state("last_timestamp")
                .expect("last timestamp")
                .as_deref(),
            Some("2026-02-19T10:00:00.000Z")
        );
        let last_agent_timestamp = db
            .get_router_state("last_agent_timestamp")
            .expect("last_agent_timestamp")
            .expect("exists");
        let parsed_last_agent: HashMap<String, String> =
            serde_json::from_str(&last_agent_timestamp).expect("parse last_agent_timestamp");
        assert_eq!(
            parsed_last_agent.get("main@local").map(String::as_str),
            Some("2026-02-19T10:00:01.000Z")
        );

        let sessions = db.get_all_sessions().expect("get sessions");
        assert_eq!(
            sessions.get("main").map(String::as_str),
            Some("session-main")
        );
        assert_eq!(
            sessions.get("dev-team").map(String::as_str),
            Some("session-dev")
        );

        let groups = db
            .get_all_registered_groups()
            .expect("get registered groups");
        assert!(groups.contains_key("main@local"));
        assert!(groups.contains_key("12345@g.us"));

        assert!(!data_dir.join("router_state.json").exists());
        assert!(!data_dir.join("sessions.json").exists());
        assert!(!data_dir.join("registered_groups.json").exists());
        assert!(data_dir.join("router_state.json.migrated").exists());
        assert!(data_dir.join("sessions.json.migrated").exists());
        assert!(data_dir.join("registered_groups.json.migrated").exists());

        let second = migrate_json_state(&db, &data_dir).expect("migrate second pass");
        assert!(!second.has_changes());
        assert_eq!(second.migrated_files.len(), 0);
        assert_eq!(second.router_state_keys, 0);
        assert_eq!(second.sessions, 0);
        assert_eq!(second.groups, 0);
    }

    #[test]
    fn returns_error_and_keeps_file_on_invalid_json() {
        let dir = tempdir().expect("tempdir");
        let data_dir = dir.path().join("data");
        fs::create_dir_all(&data_dir).expect("create data dir");
        fs::write(data_dir.join("sessions.json"), "{ invalid json").expect("write sessions");

        let db = Db::open(dir.path().join("store/messages.db")).expect("open db");
        let result = migrate_json_state(&db, &data_dir);

        assert!(result.is_err());
        assert!(data_dir.join("sessions.json").exists());
        assert!(!data_dir.join("sessions.json.migrated").exists());
    }
}
