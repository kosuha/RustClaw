use std::time::Duration;

use crate::container_runner::{
    CommandRunError, CommandRunRequest, ContainerOutputStatus, run_command,
};
use crate::task_scheduler::{TaskExecution, TaskExecutionFuture, TaskExecutor};
use crate::types::ScheduledTask;

#[derive(Clone, Debug)]
pub struct MarkerCommandExecutor {
    pub program: String,
    pub args_template: Vec<String>,
    pub timeout: Duration,
}

impl MarkerCommandExecutor {
    pub fn build_request(&self, task: &ScheduledTask) -> CommandRunRequest {
        let args = self
            .args_template
            .iter()
            .map(|arg| render_template(arg, task))
            .collect::<Vec<_>>();
        CommandRunRequest {
            program: self.program.clone(),
            args,
            timeout: self.timeout,
        }
    }
}

impl TaskExecutor for MarkerCommandExecutor {
    fn execute_task<'a>(&'a self, task: &'a ScheduledTask) -> TaskExecutionFuture<'a> {
        let request = self.build_request(task);
        Box::pin(async move {
            match run_command(&request).await {
                Ok(output) => {
                    if let Some(last_marker) = output.outputs.last() {
                        return match last_marker.status {
                            ContainerOutputStatus::Success => TaskExecution {
                                result: Some(
                                    last_marker
                                        .result
                                        .clone()
                                        .unwrap_or_else(|| "Completed".to_string()),
                                ),
                                error: None,
                            },
                            ContainerOutputStatus::Error => TaskExecution {
                                result: None,
                                error: Some(last_marker.error.clone().unwrap_or_else(|| {
                                    "executor returned error status".to_string()
                                })),
                            },
                        };
                    }

                    if !output.parse_errors.is_empty() {
                        return TaskExecution {
                            result: None,
                            error: Some(output.parse_errors.join("; ")),
                        };
                    }

                    let stdout = output.stdout.trim();
                    TaskExecution {
                        result: if stdout.is_empty() {
                            Some("Completed".to_string())
                        } else {
                            Some(stdout.to_string())
                        },
                        error: None,
                    }
                }
                Err(err) => TaskExecution {
                    result: None,
                    error: Some(format_command_error(err)),
                },
            }
        })
    }
}

fn render_template(template: &str, task: &ScheduledTask) -> String {
    template
        .replace("{task_id}", &task.id)
        .replace("{group_folder}", &task.group_folder)
        .replace("{chat_jid}", &task.chat_jid)
        .replace("{prompt}", &task.prompt)
        .replace("{schedule_value}", &task.schedule_value)
}

fn format_command_error(err: CommandRunError) -> String {
    match err {
        CommandRunError::Timeout { timeout } => {
            format!("command timed out after {}ms", timeout.as_millis())
        }
        CommandRunError::SpawnFailed(message) => format!("command spawn failed: {message}"),
        CommandRunError::NonZeroExit {
            exit_code,
            stderr_tail,
            stdout_tail,
        } => {
            let mut detail = format!("command exited with code {:?}", exit_code);
            if !stderr_tail.trim().is_empty() {
                detail.push_str(&format!("; stderr: {stderr_tail}"));
            }
            if !stdout_tail.trim().is_empty() {
                detail.push_str(&format!("; stdout: {stdout_tail}"));
            }
            detail
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::types::{ContextMode, ScheduleType, ScheduledTask, TaskStatus};

    fn sample_task() -> ScheduledTask {
        ScheduledTask {
            id: "task-1".to_string(),
            group_folder: "main".to_string(),
            chat_jid: "group@g.us".to_string(),
            prompt: "hello world".to_string(),
            schedule_type: ScheduleType::Once,
            schedule_value: "once".to_string(),
            context_mode: ContextMode::Isolated,
            next_run: Some(Utc::now()),
            last_run: None,
            last_result: None,
            status: TaskStatus::Active,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn build_request_replaces_supported_placeholders() {
        let executor = MarkerCommandExecutor {
            program: "echo".to_string(),
            args_template: vec![
                "{task_id}".to_string(),
                "{group_folder}".to_string(),
                "{chat_jid}".to_string(),
                "{prompt}".to_string(),
                "{schedule_value}".to_string(),
            ],
            timeout: Duration::from_secs(2),
        };

        let request = executor.build_request(&sample_task());
        assert_eq!(
            request.args,
            vec!["task-1", "main", "group@g.us", "hello world", "once"]
        );
    }

    #[tokio::test]
    async fn execute_task_returns_marker_success_result() {
        let script = "echo '---NANOCLAW_OUTPUT_START---'; \
                      echo '{\"status\":\"success\",\"result\":\"done\",\"newSessionId\":null,\"error\":null}'; \
                      echo '---NANOCLAW_OUTPUT_END---'";

        let executor = MarkerCommandExecutor {
            program: "sh".to_string(),
            args_template: vec!["-c".to_string(), script.to_string()],
            timeout: Duration::from_secs(2),
        };

        let execution = executor.execute_task(&sample_task()).await;
        assert_eq!(execution.result.as_deref(), Some("done"));
        assert!(execution.error.is_none());
    }

    #[tokio::test]
    async fn execute_task_returns_marker_error() {
        let script = "echo '---NANOCLAW_OUTPUT_START---'; \
                      echo '{\"status\":\"error\",\"result\":null,\"newSessionId\":null,\"error\":\"boom\"}'; \
                      echo '---NANOCLAW_OUTPUT_END---'";

        let executor = MarkerCommandExecutor {
            program: "sh".to_string(),
            args_template: vec!["-c".to_string(), script.to_string()],
            timeout: Duration::from_secs(2),
        };

        let execution = executor.execute_task(&sample_task()).await;
        assert!(execution.result.is_none());
        assert_eq!(execution.error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn execute_task_returns_non_zero_exit_error() {
        let executor = MarkerCommandExecutor {
            program: "sh".to_string(),
            args_template: vec!["-c".to_string(), "echo 'bad' 1>&2; exit 3".to_string()],
            timeout: Duration::from_secs(2),
        };

        let execution = executor.execute_task(&sample_task()).await;
        assert!(execution.result.is_none());
        assert!(
            execution
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("command exited with code")
        );
    }
}
