use std::time::Duration;

use crate::container_runner::{
    CommandRunError, CommandRunRequest, ContainerOutputStatus, run_command,
};
use crate::orchestrator::{
    AgentRunFuture, AgentRunInput, AgentRunOutput, AgentRunner, AgentStreamCallback,
    AgentStreamEvent, AgentStreamStatus,
};

#[derive(Clone, Debug)]
pub struct MarkerCommandAgentRunner {
    pub program: String,
    pub args_template: Vec<String>,
    pub timeout: Duration,
}

impl MarkerCommandAgentRunner {
    pub fn build_request(&self, input: &AgentRunInput) -> CommandRunRequest {
        let args = self
            .args_template
            .iter()
            .map(|arg| render_template(arg, input))
            .collect::<Vec<_>>();
        CommandRunRequest {
            program: self.program.clone(),
            args,
            timeout: self.timeout,
        }
    }
}

impl AgentRunner for MarkerCommandAgentRunner {
    fn run<'a>(&'a self, input: AgentRunInput) -> AgentRunFuture<'a> {
        self.run_with_callback(input, None)
    }

    fn run_with_callback<'a>(
        &'a self,
        input: AgentRunInput,
        callback: Option<AgentStreamCallback>,
    ) -> AgentRunFuture<'a> {
        let request = self.build_request(&input);
        Box::pin(async move {
            match run_command(&request).await {
                Ok(output) => {
                    if let Some(callback) = &callback {
                        for marker in &output.outputs {
                            callback(AgentStreamEvent {
                                status: match marker.status {
                                    ContainerOutputStatus::Success => AgentStreamStatus::Success,
                                    ContainerOutputStatus::Error => AgentStreamStatus::Error,
                                },
                                result: marker.result.clone(),
                                new_session_id: marker.new_session_id.clone(),
                                error: marker.error.clone(),
                            })
                            .await;
                        }
                    }

                    if let Some(last_marker) = output.outputs.last() {
                        return match last_marker.status {
                            ContainerOutputStatus::Success => Ok(AgentRunOutput {
                                result: last_marker.result.clone(),
                                new_session_id: last_marker
                                    .new_session_id
                                    .clone()
                                    .or(input.session_id.clone()),
                            }),
                            ContainerOutputStatus::Error => {
                                Err(last_marker.error.clone().unwrap_or_else(|| {
                                    "agent runner returned error marker".to_string()
                                }))
                            }
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
                }
                Err(err) => Err(format_command_error(err)),
            }
        })
    }
}

fn render_template(template: &str, input: &AgentRunInput) -> String {
    template
        .replace("{group_name}", &input.group.name)
        .replace("{group_folder}", &input.group_folder)
        .replace("{chat_jid}", &input.chat_jid)
        .replace("{prompt}", &input.prompt)
        .replace("{session_id}", input.session_id.as_deref().unwrap_or(""))
        .replace("{is_main}", if input.is_main { "true" } else { "false" })
}

fn format_command_error(err: CommandRunError) -> String {
    match err {
        CommandRunError::Timeout { timeout } => {
            format!("agent command timed out after {}ms", timeout.as_millis())
        }
        CommandRunError::SpawnFailed(message) => {
            format!("agent command spawn failed: {message}")
        }
        CommandRunError::NonZeroExit {
            exit_code,
            stderr_tail,
            stdout_tail,
        } => {
            let mut detail = format!("agent command exited with code {:?}", exit_code);
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
    use crate::types::RegisteredGroup;

    use super::*;

    fn input() -> AgentRunInput {
        AgentRunInput {
            group: RegisteredGroup {
                name: "Main".to_string(),
                folder: "main".to_string(),
                trigger: "@Andy".to_string(),
                added_at: "2026-02-19T00:00:00.000Z".to_string(),
                container_config: None,
                requires_trigger: Some(false),
            },
            prompt: "<messages><message>hello</message></messages>".to_string(),
            session_id: Some("s-1".to_string()),
            group_folder: "main".to_string(),
            chat_jid: "123456789012345678@discord".to_string(),
            is_main: true,
            is_scheduled_task: false,
        }
    }

    #[test]
    fn build_request_replaces_templates() {
        let runner = MarkerCommandAgentRunner {
            program: "echo".to_string(),
            args_template: vec![
                "{group_name}".to_string(),
                "{group_folder}".to_string(),
                "{chat_jid}".to_string(),
                "{session_id}".to_string(),
                "{is_main}".to_string(),
            ],
            timeout: Duration::from_secs(1),
        };

        let request = runner.build_request(&input());
        assert_eq!(
            request.args,
            vec!["Main", "main", "123456789012345678@discord", "s-1", "true"]
        );
    }

    #[tokio::test]
    async fn run_returns_output_from_success_marker() {
        let script = "echo '---NANOCLAW_OUTPUT_START---'; \
                      echo '{\"status\":\"success\",\"result\":\"ok\",\"newSessionId\":\"s-2\",\"error\":null}'; \
                      echo '---NANOCLAW_OUTPUT_END---'";
        let runner = MarkerCommandAgentRunner {
            program: "sh".to_string(),
            args_template: vec!["-c".to_string(), script.to_string()],
            timeout: Duration::from_secs(2),
        };

        let output = runner.run(input()).await.expect("run output");
        assert_eq!(output.result.as_deref(), Some("ok"));
        assert_eq!(output.new_session_id.as_deref(), Some("s-2"));
    }

    #[tokio::test]
    async fn run_returns_error_from_error_marker() {
        let script = "echo '---NANOCLAW_OUTPUT_START---'; \
                      echo '{\"status\":\"error\",\"result\":null,\"newSessionId\":null,\"error\":\"boom\"}'; \
                      echo '---NANOCLAW_OUTPUT_END---'";
        let runner = MarkerCommandAgentRunner {
            program: "sh".to_string(),
            args_template: vec!["-c".to_string(), script.to_string()],
            timeout: Duration::from_secs(2),
        };

        let error = runner.run(input()).await.expect_err("expected error");
        assert_eq!(error, "boom");
    }
}
