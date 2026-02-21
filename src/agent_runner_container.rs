use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

use crate::container_runner::{ContainerOutput, ContainerOutputStatus, OutputMarkerParser};
use crate::env_file::read_env_var;
use crate::mount_security::validate_additional_mount;
use crate::orchestrator::{
    AgentProcessCallback, AgentProcessEvent, AgentRunFuture, AgentRunInput, AgentRunOutput,
    AgentRunner, AgentStreamCallback, AgentStreamEvent, AgentStreamStatus,
};
use crate::types::AdditionalMount;

const RUNTIME_OPTIONAL_ENV_KEYS: &[&str] = &[
    "CODEX_MODEL",
    "CODEX_REASONING_EFFORT",
    "TIMEZONE",
    "CODEX_APPROVAL_POLICY",
    "CODEX_SANDBOX_MODE",
    "CODEX_ENABLE_SEARCH",
];

const DEFAULT_CREDENTIAL_ENV_KEYS: &[&str] =
    &["UPBIT_ACCESS_KEY", "UPBIT_SECRET_KEY", "UPBIT_BASE_URL"];

#[derive(Clone, Debug)]
pub struct ContainerAgentRunner {
    pub container_binary: String,
    pub container_image: String,
    pub project_root: PathBuf,
    pub groups_dir: PathBuf,
    pub data_dir: PathBuf,
    pub timeout: Duration,
    pub mount_allowlist_path: Option<PathBuf>,
    pub codex_auth_dir: Option<PathBuf>,
}

impl ContainerAgentRunner {
    pub fn new(
        container_binary: impl Into<String>,
        container_image: impl Into<String>,
        project_root: PathBuf,
        groups_dir: PathBuf,
        data_dir: PathBuf,
        timeout: Duration,
    ) -> Self {
        Self {
            container_binary: container_binary.into(),
            container_image: container_image.into(),
            project_root,
            groups_dir,
            data_dir,
            timeout,
            mount_allowlist_path: read_env_var("MOUNT_ALLOWLIST_PATH").map(PathBuf::from),
            codex_auth_dir: resolve_codex_auth_dir(),
        }
    }

    fn build_volume_mounts(&self, input: &AgentRunInput) -> Result<Vec<VolumeMount>, String> {
        let mut mounts = Vec::<VolumeMount>::new();

        let group_dir = self.groups_dir.join(&input.group.folder);
        fs::create_dir_all(&group_dir).map_err(|err| {
            format!(
                "failed to create group directory '{}': {err}",
                group_dir.display()
            )
        })?;

        if input.is_main {
            mounts.push(VolumeMount {
                host_path: self.project_root.to_string_lossy().to_string(),
                container_path: "/workspace/project".to_string(),
                readonly: false,
            });
            mounts.push(VolumeMount {
                host_path: group_dir.to_string_lossy().to_string(),
                container_path: "/workspace/group".to_string(),
                readonly: false,
            });
        } else {
            mounts.push(VolumeMount {
                host_path: group_dir.to_string_lossy().to_string(),
                container_path: "/workspace/group".to_string(),
                readonly: false,
            });

            let global_dir = self.groups_dir.join("global");
            if global_dir.exists() {
                mounts.push(VolumeMount {
                    host_path: global_dir.to_string_lossy().to_string(),
                    container_path: "/workspace/global".to_string(),
                    readonly: true,
                });
            }
        }

        let sessions_dir = self
            .data_dir
            .join("sessions")
            .join(&input.group.folder)
            .join(".codex");
        fs::create_dir_all(&sessions_dir).map_err(|err| {
            format!(
                "failed to create sessions directory '{}': {err}",
                sessions_dir.display()
            )
        })?;
        mounts.push(VolumeMount {
            host_path: sessions_dir.to_string_lossy().to_string(),
            container_path: "/home/node/.codex".to_string(),
            readonly: false,
        });

        if let Some(auth_dir) = &self.codex_auth_dir {
            if auth_dir.exists() {
                mounts.push(VolumeMount {
                    host_path: auth_dir.to_string_lossy().to_string(),
                    container_path: "/workspace/codex-auth".to_string(),
                    readonly: true,
                });
            } else {
                eprintln!(
                    "codex auth dir not found, continuing without auth seed mount: {}",
                    auth_dir.display()
                );
            }
        }

        let ipc_dir = self.data_dir.join("ipc").join(&input.group.folder);
        fs::create_dir_all(ipc_dir.join("messages"))
            .map_err(|err| format!("failed to create IPC messages dir: {err}"))?;
        fs::create_dir_all(ipc_dir.join("tasks"))
            .map_err(|err| format!("failed to create IPC tasks dir: {err}"))?;
        fs::create_dir_all(ipc_dir.join("input"))
            .map_err(|err| format!("failed to create IPC input dir: {err}"))?;
        mounts.push(VolumeMount {
            host_path: ipc_dir.to_string_lossy().to_string(),
            container_path: "/workspace/ipc".to_string(),
            readonly: false,
        });

        if let Some(config) = &input.group.container_config {
            for additional_mount in &config.additional_mounts {
                match self.resolve_additional_mount(additional_mount, input.is_main) {
                    Ok(mount) => mounts.push(mount),
                    Err(err) => {
                        eprintln!(
                            "additional mount rejected for group {} ({}): {}",
                            input.group.folder, additional_mount.host_path, err
                        );
                    }
                }
            }
        }

        Ok(mounts)
    }

    fn build_container_args(
        &self,
        mounts: &[VolumeMount],
        container_name: &str,
        container_envs: &[(String, String)],
    ) -> Vec<String> {
        let mut args = vec![
            "run".to_string(),
            "-i".to_string(),
            "--rm".to_string(),
            "--name".to_string(),
            container_name.to_string(),
        ];

        for (key, value) in container_envs {
            args.push("-e".to_string());
            args.push(format!("{key}={value}"));
        }

        for mount in mounts {
            if mount.readonly {
                args.push("--mount".to_string());
                args.push(format!(
                    "type=bind,source={},target={},readonly",
                    mount.host_path, mount.container_path
                ));
            } else {
                args.push("-v".to_string());
                args.push(format!("{}:{}", mount.host_path, mount.container_path));
            }
        }

        args.push(self.container_image.clone());
        args
    }

    fn resolve_additional_mount(
        &self,
        mount: &AdditionalMount,
        is_main: bool,
    ) -> Result<VolumeMount, String> {
        let validated =
            validate_additional_mount(mount, is_main, self.mount_allowlist_path.as_deref())?;
        Ok(VolumeMount {
            host_path: validated.host_path,
            container_path: validated.container_path,
            readonly: validated.readonly,
        })
    }

    fn write_container_log(
        &self,
        group_folder: &str,
        container_name: &str,
        args: &[String],
        output: &ProcessOutput,
    ) -> Result<(), String> {
        let logs_dir = self.groups_dir.join(group_folder).join("logs");
        fs::create_dir_all(&logs_dir).map_err(|err| {
            format!(
                "failed to create group log dir '{}': {err}",
                logs_dir.display()
            )
        })?;

        let timestamp = Utc::now().format("%Y%m%d-%H%M%S%.3f").to_string();
        let log_file = logs_dir.join(format!("container-{timestamp}.log"));
        let max_bytes = read_log_max_bytes();
        let stdout = truncate_by_bytes(&output.stdout, max_bytes);
        let stderr = truncate_by_bytes(&output.stderr, max_bytes);
        let codex_summary = summarize_codex_logs(&output.stderr);
        let content = format!(
            "container={container_name}\nprogram={}\nargs={}\nexit_code={:?}\nstdout_bytes={}\nstderr_bytes={}\nparse_errors={}\noutputs={}\ncodex_requests={}\ncodex_events={}\ncodex_errors={}\n\n[stdout]\n{}\n\n[stderr]\n{}\n",
            self.container_binary,
            args.join(" "),
            output.exit_code,
            output.stdout.len(),
            output.stderr.len(),
            output.parse_errors.join(" | "),
            output.outputs.len(),
            codex_summary.requests,
            codex_summary.events,
            codex_summary.errors,
            stdout,
            stderr
        );
        fs::write(&log_file, content).map_err(|err| {
            format!(
                "failed to write container log '{}': {err}",
                log_file.display()
            )
        })
    }
}

impl AgentRunner for ContainerAgentRunner {
    fn run<'a>(&'a self, input: AgentRunInput) -> AgentRunFuture<'a> {
        self.run_with_callbacks(input, None, None)
    }

    fn run_with_callback<'a>(
        &'a self,
        input: AgentRunInput,
        callback: Option<AgentStreamCallback>,
    ) -> AgentRunFuture<'a> {
        self.run_with_callbacks(input, callback, None)
    }

    fn run_with_callbacks<'a>(
        &'a self,
        input: AgentRunInput,
        callback: Option<AgentStreamCallback>,
        process_callback: Option<AgentProcessCallback>,
    ) -> AgentRunFuture<'a> {
        Box::pin(async move {
            let mounts = self.build_volume_mounts(&input)?;
            let safe_name = sanitize_container_name(&input.group.folder);
            let container_name = format!("nanoclaw-{safe_name}-{}", Utc::now().timestamp_millis());
            let mut container_envs = collect_allowed_container_envs();
            container_envs.extend(collect_runtime_option_envs());
            let args = self.build_container_args(&mounts, &container_name, &container_envs);
            let payload = serde_json::to_string(&ContainerInputPayload::from(&input))
                .map_err(|err| format!("failed to serialize container input payload: {err}"))?;
            let configured_timeout = input
                .group
                .container_config
                .as_ref()
                .and_then(|config| config.timeout_ms)
                .map(Duration::from_millis);
            let effective_timeout = compute_effective_timeout(
                configured_timeout,
                self.timeout,
                idle_timeout_from_env(),
            );

            let output = run_process_with_stdin(
                &self.container_binary,
                &args,
                &payload,
                effective_timeout,
                callback,
                process_callback,
                &container_name,
                &input.group.folder,
            )
            .await?;
            if let Err(err) =
                self.write_container_log(&input.group.folder, &container_name, &args, &output)
            {
                eprintln!(
                    "failed to write container log for {}: {}",
                    input.group.folder, err
                );
            }

            if output.exit_code != Some(0) {
                return Err(format!(
                    "container exited with code {:?}: {}",
                    output.exit_code,
                    tail_chars(&output.stderr, 200),
                ));
            }

            if let Some(last_output) = output.outputs.last() {
                return match last_output.status {
                    ContainerOutputStatus::Success => Ok(AgentRunOutput {
                        result: last_output.result.clone(),
                        new_session_id: last_output
                            .new_session_id
                            .clone()
                            .or(input.session_id.clone()),
                    }),
                    ContainerOutputStatus::Error => Err(last_output
                        .error
                        .clone()
                        .unwrap_or_else(|| "container returned error marker".to_string())),
                };
            }

            if !output.parse_errors.is_empty() {
                return Err(output.parse_errors.join("; "));
            }

            let stdout = output.stdout.trim();
            Ok(AgentRunOutput {
                result: if stdout.is_empty() {
                    None
                } else {
                    Some(stdout.to_string())
                },
                new_session_id: input.session_id.clone(),
            })
        })
    }
}

#[derive(Clone, Debug)]
struct VolumeMount {
    host_path: String,
    container_path: String,
    readonly: bool,
}

#[derive(Serialize)]
struct ContainerInputPayload {
    prompt: String,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(rename = "groupFolder")]
    group_folder: String,
    #[serde(rename = "chatJid")]
    chat_jid: String,
    #[serde(rename = "isMain")]
    is_main: bool,
    #[serde(rename = "isScheduledTask")]
    is_scheduled_task: bool,
}

impl From<&AgentRunInput> for ContainerInputPayload {
    fn from(value: &AgentRunInput) -> Self {
        Self {
            prompt: value.prompt.clone(),
            session_id: value.session_id.clone(),
            group_folder: value.group_folder.clone(),
            chat_jid: value.chat_jid.clone(),
            is_main: value.is_main,
            is_scheduled_task: value.is_scheduled_task,
        }
    }
}

#[derive(Debug)]
struct ProcessOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    outputs: Vec<ContainerOutput>,
    parse_errors: Vec<String>,
}

#[derive(Debug)]
struct StdoutTaskOutput {
    bytes: Vec<u8>,
    outputs: Vec<ContainerOutput>,
    parse_errors: Vec<String>,
}

async fn run_process_with_stdin(
    program: &str,
    args: &[String],
    stdin_payload: &str,
    timeout_duration: Duration,
    callback: Option<AgentStreamCallback>,
    process_callback: Option<AgentProcessCallback>,
    container_name: &str,
    group_folder: &str,
) -> Result<ProcessOutput, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|err| format!("failed to spawn process '{program}': {err}"))?;

    if let Some(process_callback) = process_callback {
        process_callback(AgentProcessEvent {
            process_id: child.id(),
            container_name: Some(container_name.to_string()),
            group_folder: Some(group_folder.to_string()),
        })
        .await;
    }

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_payload.as_bytes())
            .await
            .map_err(|err| format!("failed to write stdin payload to '{program}': {err}"))?;
        stdin
            .shutdown()
            .await
            .map_err(|err| format!("failed to close stdin for '{program}': {err}"))?;
    }

    let stdout_task = {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("failed to capture stdout for '{program}'"))?;
        let callback = callback.clone();
        tokio::spawn(async move {
            let mut full_stdout = Vec::new();
            let mut chunk = vec![0u8; 4096];
            let mut parser = OutputMarkerParser::default();
            let mut outputs = Vec::new();
            let mut parse_errors = Vec::new();

            loop {
                let read = stdout.read(&mut chunk).await?;
                if read == 0 {
                    break;
                }

                let data = &chunk[..read];
                full_stdout.extend_from_slice(data);

                let text = String::from_utf8_lossy(data);
                for parsed in parser.push_chunk(&text) {
                    match parsed {
                        Ok(output) => {
                            if let Some(callback) = &callback {
                                callback(AgentStreamEvent {
                                    status: stream_status(output.status),
                                    result: output.result.clone(),
                                    new_session_id: output.new_session_id.clone(),
                                    error: output.error.clone(),
                                })
                                .await;
                            }
                            outputs.push(output);
                        }
                        Err(error) => parse_errors.push(error),
                    }
                }
            }

            Ok::<StdoutTaskOutput, std::io::Error>(StdoutTaskOutput {
                bytes: full_stdout,
                outputs,
                parse_errors,
            })
        })
    };
    let stderr_task = {
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| format!("failed to capture stderr for '{program}'"))?;
        tokio::spawn(async move {
            let mut buffer = Vec::new();
            stderr.read_to_end(&mut buffer).await.map(|_| buffer)
        })
    };

    let status = match timeout(timeout_duration, child.wait()).await {
        Ok(wait_result) => wait_result.map_err(|err| format!("failed to wait process: {err}"))?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(format!(
                "process timed out after {}ms",
                timeout_duration.as_millis()
            ));
        }
    };

    let stdout_output = stdout_task
        .await
        .map_err(|err| format!("stdout task join error: {err}"))?
        .map_err(|err| format!("stdout read error: {err}"))?;
    let stderr_bytes = stderr_task
        .await
        .map_err(|err| format!("stderr task join error: {err}"))?
        .map_err(|err| format!("stderr read error: {err}"))?;

    Ok(ProcessOutput {
        exit_code: status.code(),
        stdout: String::from_utf8_lossy(&stdout_output.bytes).to_string(),
        stderr: String::from_utf8_lossy(&stderr_bytes).to_string(),
        outputs: stdout_output.outputs,
        parse_errors: stdout_output.parse_errors,
    })
}

fn stream_status(status: ContainerOutputStatus) -> AgentStreamStatus {
    match status {
        ContainerOutputStatus::Success => AgentStreamStatus::Success,
        ContainerOutputStatus::Error => AgentStreamStatus::Error,
    }
}

fn idle_timeout_from_env() -> Duration {
    Duration::from_millis(
        read_env_var("IDLE_TIMEOUT_MS")
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(1_800_000),
    )
}

fn compute_effective_timeout(
    configured_timeout: Option<Duration>,
    default_timeout: Duration,
    idle_timeout: Duration,
) -> Duration {
    let base_timeout = configured_timeout.unwrap_or(default_timeout);
    let idle_with_grace = idle_timeout.saturating_add(Duration::from_secs(30));
    base_timeout.max(idle_with_grace)
}

fn sanitize_container_name(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        "group".to_string()
    } else {
        out
    }
}

fn collect_allowed_container_envs() -> Vec<(String, String)> {
    allowed_credential_keys()
        .into_iter()
        .filter_map(|key| read_env_var(&key).map(|value| (key, value)))
        .collect::<Vec<_>>()
}

fn collect_runtime_option_envs() -> Vec<(String, String)> {
    let mut envs = RUNTIME_OPTIONAL_ENV_KEYS
        .iter()
        .filter_map(|key| read_env_var(key).map(|value| ((*key).to_string(), value)))
        .collect::<Vec<_>>();

    if let Some((_, timezone)) = envs.iter().find(|(key, _)| key == "TIMEZONE") {
        if !envs.iter().any(|(key, _)| key == "TZ") {
            envs.push(("TZ".to_string(), timezone.clone()));
        }
    }

    envs
}

fn allowed_credential_keys() -> Vec<String> {
    let extra = read_env_var("CONTAINER_ALLOWED_CREDENTIAL_KEYS_JSON")
        .map(|raw| parse_allowed_credential_keys(&raw))
        .unwrap_or_default();
    merge_allowed_credential_keys(extra)
}

fn parse_allowed_credential_keys(raw: &str) -> Vec<String> {
    let Ok(extra) = serde_json::from_str::<Vec<String>>(raw) else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    for key in extra
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        if !keys.iter().any(|existing| existing == &key) {
            keys.push(key);
        }
    }

    keys
}

fn merge_allowed_credential_keys(extra: Vec<String>) -> Vec<String> {
    let mut keys = DEFAULT_CREDENTIAL_ENV_KEYS
        .iter()
        .map(|key| (*key).to_string())
        .collect::<Vec<_>>();
    for key in extra {
        if !keys.iter().any(|existing| existing == &key) {
            keys.push(key);
        }
    }
    keys
}

#[derive(Default)]
struct CodexLogSummary {
    requests: usize,
    events: usize,
    errors: usize,
}

fn summarize_codex_logs(stderr: &str) -> CodexLogSummary {
    let mut summary = CodexLogSummary::default();
    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[codex-rpc:request]") {
            summary.requests += 1;
        } else if trimmed.starts_with("[codex-rpc:event]") {
            summary.events += 1;
        } else if trimmed.starts_with("[codex-rpc:error]") {
            summary.errors += 1;
        }
    }
    summary
}

fn resolve_codex_auth_dir() -> Option<PathBuf> {
    if let Some(raw) = read_env_var("CODEX_AUTH_DIR") {
        return Some(expand_home_path(&raw));
    }
    default_codex_auth_dir()
}

fn default_codex_auth_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".codex"))
}

fn expand_home_path(path: &str) -> PathBuf {
    if path == "~" {
        return std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(path));
    }
    if let Some(stripped) = path.strip_prefix("~/") {
        return std::env::var("HOME")
            .map(|home| PathBuf::from(home).join(stripped))
            .unwrap_or_else(|_| PathBuf::from(path));
    }
    Path::new(path).to_path_buf()
}

fn read_log_max_bytes() -> usize {
    read_env_var("CONTAINER_LOG_MAX_BYTES")
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(200_000)
}

fn truncate_by_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }

    let mut current = 0usize;
    let mut out = String::new();
    for ch in value.chars() {
        let ch_len = ch.len_utf8();
        if current + ch_len > max_bytes {
            break;
        }
        out.push(ch);
        current += ch_len;
    }
    out
}

fn tail_chars(value: &str, max_chars: usize) -> String {
    let total_chars = value.chars().count();
    if total_chars <= max_chars {
        return value.to_string();
    }
    value.chars().skip(total_chars - max_chars).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::orchestrator::{
        AgentProcessEvent, AgentProcessFuture, AgentStreamEvent, AgentStreamFuture,
        AgentStreamStatus,
    };
    use crate::types::{ContainerConfig, RegisteredGroup};

    fn sample_input() -> AgentRunInput {
        AgentRunInput {
            group: RegisteredGroup {
                name: "Dev Team".to_string(),
                folder: "dev_team".to_string(),
                trigger: "@Andy".to_string(),
                added_at: "2026-02-19T00:00:00.000Z".to_string(),
                container_config: None,
                requires_trigger: Some(true),
            },
            prompt: "hello".to_string(),
            session_id: Some("session-1".to_string()),
            group_folder: "dev_team".to_string(),
            chat_jid: "group@g.us".to_string(),
            is_main: false,
            is_scheduled_task: false,
        }
    }

    #[test]
    fn build_container_args_uses_mount_flags() {
        let runner = ContainerAgentRunner::new(
            "container",
            "image:latest",
            PathBuf::from("/project"),
            PathBuf::from("/groups"),
            PathBuf::from("/data"),
            Duration::from_secs(5),
        );
        let mounts = vec![
            VolumeMount {
                host_path: "/h/rw".to_string(),
                container_path: "/c/rw".to_string(),
                readonly: false,
            },
            VolumeMount {
                host_path: "/h/ro".to_string(),
                container_path: "/c/ro".to_string(),
                readonly: true,
            },
        ];

        let args = runner.build_container_args(
            &mounts,
            "ct-1",
            &[("MY_API_KEY".to_string(), "secret".to_string())],
        );
        assert_eq!(
            args,
            vec![
                "run",
                "-i",
                "--rm",
                "--name",
                "ct-1",
                "-e",
                "MY_API_KEY=secret",
                "-v",
                "/h/rw:/c/rw",
                "--mount",
                "type=bind,source=/h/ro,target=/c/ro,readonly",
                "image:latest",
            ]
        );
    }

    #[test]
    fn parse_allowed_credential_keys_returns_empty_for_invalid_json() {
        assert!(parse_allowed_credential_keys("not-json").is_empty());
    }

    #[test]
    fn parse_allowed_credential_keys_deduplicates_and_trims() {
        let keys = parse_allowed_credential_keys(r#"[" MY_KEY ","", "MY_KEY", "SECOND_KEY"]"#);
        assert_eq!(keys, vec!["MY_KEY".to_string(), "SECOND_KEY".to_string()]);
    }

    #[test]
    fn merge_allowed_credential_keys_includes_defaults() {
        let keys = merge_allowed_credential_keys(Vec::new());
        assert_eq!(
            keys,
            vec![
                "UPBIT_ACCESS_KEY".to_string(),
                "UPBIT_SECRET_KEY".to_string(),
                "UPBIT_BASE_URL".to_string()
            ]
        );
    }

    #[test]
    fn merge_allowed_credential_keys_appends_unique_extras() {
        let keys = merge_allowed_credential_keys(vec![
            "UPBIT_SECRET_KEY".to_string(),
            "MY_API_KEY".to_string(),
        ]);
        assert_eq!(
            keys,
            vec![
                "UPBIT_ACCESS_KEY".to_string(),
                "UPBIT_SECRET_KEY".to_string(),
                "UPBIT_BASE_URL".to_string(),
                "MY_API_KEY".to_string()
            ]
        );
    }

    #[test]
    fn container_input_payload_sets_scheduled_task_flag() {
        let mut input = sample_input();
        input.is_scheduled_task = true;
        let payload =
            serde_json::to_value(ContainerInputPayload::from(&input)).expect("serialize payload");
        assert_eq!(
            payload
                .get("isScheduledTask")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn build_volume_mounts_adds_core_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_root = temp.path().join("project");
        let groups_dir = project_root.join("groups");
        let data_dir = project_root.join("data");
        let codex_auth_dir = temp.path().join("codex-auth");
        fs::create_dir_all(groups_dir.join("global")).expect("create global dir");
        fs::create_dir_all(&codex_auth_dir).expect("create auth dir");

        let mut runner = ContainerAgentRunner::new(
            "container",
            "image:latest",
            project_root.clone(),
            groups_dir.clone(),
            data_dir.clone(),
            Duration::from_secs(5),
        );
        runner.codex_auth_dir = Some(codex_auth_dir);

        let mounts = runner
            .build_volume_mounts(&sample_input())
            .expect("build mounts");
        let target_paths = mounts
            .iter()
            .map(|mount| mount.container_path.as_str())
            .collect::<Vec<_>>();

        assert!(target_paths.contains(&"/workspace/group"));
        assert!(target_paths.contains(&"/workspace/global"));
        assert!(target_paths.contains(&"/home/node/.codex"));
        assert!(target_paths.contains(&"/workspace/codex-auth"));
        assert!(target_paths.contains(&"/workspace/ipc"));
    }

    #[test]
    fn build_volume_mounts_skips_missing_codex_auth_seed_mount() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_root = temp.path().join("project");
        let groups_dir = project_root.join("groups");
        let data_dir = project_root.join("data");

        let mut runner = ContainerAgentRunner::new(
            "container",
            "image:latest",
            project_root,
            groups_dir,
            data_dir,
            Duration::from_secs(5),
        );
        runner.codex_auth_dir = Some(temp.path().join("missing-auth"));

        let mounts = runner
            .build_volume_mounts(&sample_input())
            .expect("build mounts");
        assert!(
            !mounts
                .iter()
                .any(|mount| mount.container_path == "/workspace/codex-auth")
        );
    }

    #[test]
    fn non_main_additional_mount_is_forced_readonly() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_root = temp.path().join("project");
        let groups_dir = project_root.join("groups");
        let data_dir = project_root.join("data");
        let extra_dir = temp.path().join("extra");
        fs::create_dir_all(&extra_dir).expect("create extra dir");

        let mut input = sample_input();
        input.group.container_config = Some(ContainerConfig {
            additional_mounts: vec![AdditionalMount {
                host_path: extra_dir.to_string_lossy().to_string(),
                container_path: Some("work".to_string()),
                readonly: Some(false),
            }],
            timeout_ms: None,
        });

        let runner = ContainerAgentRunner::new(
            "container",
            "image:latest",
            project_root,
            groups_dir,
            data_dir,
            Duration::from_secs(5),
        );
        let allowlist_path = temp.path().join("mount-allowlist.json");
        fs::write(
            &allowlist_path,
            format!(
                "{{\"allowedRoots\":[{{\"path\":\"{}\",\"allowReadWrite\":true}}],\"blockedPatterns\":[],\"nonMainReadOnly\":true}}",
                temp.path().to_string_lossy()
            ),
        )
        .expect("write allowlist");
        let mut runner = runner;
        runner.mount_allowlist_path = Some(allowlist_path);

        let mounts = runner.build_volume_mounts(&input).expect("build mounts");
        let additional = mounts
            .iter()
            .find(|mount| mount.container_path == "/workspace/extra/work")
            .expect("additional mount exists");
        assert!(additional.readonly);
    }

    #[tokio::test]
    async fn run_process_with_stdin_writes_payload_and_reads_output() {
        let output = run_process_with_stdin(
            "sh",
            &[
                "-c".to_string(),
                "cat >/dev/null; echo ---NANOCLAW_OUTPUT_START---; echo '{\"status\":\"success\",\"result\":\"ok\",\"newSessionId\":null,\"error\":null}'; echo ---NANOCLAW_OUTPUT_END---".to_string(),
            ],
            "{\"hello\":\"world\"}",
            Duration::from_secs(2),
            None,
            None,
            "ct-1",
            "dev_team",
        )
        .await
        .expect("run process");

        assert_eq!(output.exit_code, Some(0));
        assert!(output.parse_errors.is_empty());
        assert_eq!(output.outputs.len(), 1);
        assert_eq!(output.outputs[0].result.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn run_process_with_stdin_streams_marker_events_to_callback() {
        let streamed = Arc::new(Mutex::new(Vec::<AgentStreamEvent>::new()));
        let callback = {
            let streamed = streamed.clone();
            Arc::new(move |event: AgentStreamEvent| -> AgentStreamFuture {
                let streamed = streamed.clone();
                Box::pin(async move {
                    streamed.lock().expect("streamed lock").push(event);
                })
            })
        };

        let output = run_process_with_stdin(
            "sh",
            &[
                "-c".to_string(),
                "cat >/dev/null; echo ---NANOCLAW_OUTPUT_START---; echo '{\"status\":\"success\",\"result\":\"chunk-1\",\"newSessionId\":\"s-1\",\"error\":null}'; echo ---NANOCLAW_OUTPUT_END---; echo ---NANOCLAW_OUTPUT_START---; echo '{\"status\":\"success\",\"result\":\"chunk-2\",\"newSessionId\":null,\"error\":null}'; echo ---NANOCLAW_OUTPUT_END---".to_string(),
            ],
            "{}",
            Duration::from_secs(2),
            Some(callback),
            None,
            "ct-2",
            "dev_team",
        )
        .await
        .expect("run process");

        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.outputs.len(), 2);
        let streamed = streamed.lock().expect("streamed lock");
        assert_eq!(streamed.len(), 2);
        assert_eq!(streamed[0].status, AgentStreamStatus::Success);
        assert_eq!(streamed[0].result.as_deref(), Some("chunk-1"));
        assert_eq!(streamed[0].new_session_id.as_deref(), Some("s-1"));
        assert_eq!(streamed[1].result.as_deref(), Some("chunk-2"));
    }

    #[tokio::test]
    async fn run_process_with_stdin_reports_process_metadata() {
        let seen = Arc::new(Mutex::new(None::<AgentProcessEvent>));
        let process_callback = {
            let seen = seen.clone();
            Arc::new(move |event: AgentProcessEvent| -> AgentProcessFuture {
                let seen = seen.clone();
                Box::pin(async move {
                    *seen.lock().expect("seen lock") = Some(event);
                })
            })
        };

        let output = run_process_with_stdin(
            "sh",
            &["-c".to_string(), "cat >/dev/null; echo ok".to_string()],
            "{}",
            Duration::from_secs(2),
            None,
            Some(process_callback),
            "nanoclaw-test-ct",
            "dev-team",
        )
        .await
        .expect("run process");
        assert_eq!(output.exit_code, Some(0));

        let seen = seen
            .lock()
            .expect("seen lock")
            .clone()
            .expect("process event");
        assert!(seen.process_id.is_some());
        assert_eq!(seen.container_name.as_deref(), Some("nanoclaw-test-ct"));
        assert_eq!(seen.group_folder.as_deref(), Some("dev-team"));
    }

    #[test]
    fn compute_effective_timeout_uses_idle_timeout_grace_as_lower_bound() {
        let default_timeout = Duration::from_millis(1_200_000);
        let idle_timeout = Duration::from_millis(1_800_000);
        let timeout = compute_effective_timeout(None, default_timeout, idle_timeout);
        assert_eq!(timeout, Duration::from_millis(1_830_000));
    }

    #[test]
    fn compute_effective_timeout_prefers_larger_configured_timeout() {
        let configured_timeout = Some(Duration::from_millis(2_400_000));
        let default_timeout = Duration::from_millis(1_800_000);
        let idle_timeout = Duration::from_millis(600_000);
        let timeout = compute_effective_timeout(configured_timeout, default_timeout, idle_timeout);
        assert_eq!(timeout, Duration::from_millis(2_400_000));
    }

    #[test]
    fn writes_container_log_with_capped_output() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_root = temp.path().join("project");
        let groups_dir = project_root.join("groups");
        let data_dir = project_root.join("data");

        let runner = ContainerAgentRunner::new(
            "container",
            "image:latest",
            project_root,
            groups_dir.clone(),
            data_dir,
            Duration::from_secs(5),
        );

        let output = ProcessOutput {
            exit_code: Some(0),
            stdout: "x".repeat(1000),
            stderr: "y".repeat(1000),
            outputs: Vec::new(),
            parse_errors: Vec::new(),
        };
        runner
            .write_container_log(
                "dev-team",
                "nanoclaw-dev-team-1",
                &["run".to_string(), "image:latest".to_string()],
                &output,
            )
            .expect("write log");

        let logs_dir = groups_dir.join("dev-team").join("logs");
        let entries = fs::read_dir(logs_dir)
            .expect("read logs dir")
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        let log = fs::read_to_string(entries[0].path()).expect("read log file");
        assert!(log.contains("container=nanoclaw-dev-team-1"));
        assert!(log.contains("codex_requests="));
        assert!(log.contains("[stdout]"));
    }
}
