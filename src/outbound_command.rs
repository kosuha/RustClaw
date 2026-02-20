use std::time::Duration;

use crate::container_runner::{CommandRunError, CommandRunRequest, run_command};
use crate::orchestrator::{OutboundSender, SendFuture};

#[derive(Clone, Debug)]
pub struct CommandOutboundSender {
    pub program: String,
    pub args_template: Vec<String>,
    pub timeout: Duration,
}

impl CommandOutboundSender {
    pub fn build_request(&self, jid: &str, text: &str) -> CommandRunRequest {
        let args = self
            .args_template
            .iter()
            .map(|arg| arg.replace("{jid}", jid).replace("{text}", text))
            .collect::<Vec<_>>();
        CommandRunRequest {
            program: self.program.clone(),
            args,
            timeout: self.timeout,
        }
    }
}

impl OutboundSender for CommandOutboundSender {
    fn send<'a>(&'a self, jid: &'a str, text: &'a str) -> SendFuture<'a> {
        let request = self.build_request(jid, text);
        Box::pin(async move {
            run_command(&request)
                .await
                .map(|_| ())
                .map_err(format_command_error)
        })
    }
}

fn format_command_error(err: CommandRunError) -> String {
    match err {
        CommandRunError::Timeout { timeout } => {
            format!("outbound command timed out after {}ms", timeout.as_millis())
        }
        CommandRunError::SpawnFailed(message) => {
            format!("outbound command spawn failed: {message}")
        }
        CommandRunError::NonZeroExit {
            exit_code,
            stderr_tail,
            stdout_tail,
        } => {
            let mut detail = format!("outbound command exited with code {:?}", exit_code);
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
    use super::*;

    #[test]
    fn build_request_replaces_placeholders() {
        let sender = CommandOutboundSender {
            program: "echo".to_string(),
            args_template: vec!["to={jid}".to_string(), "msg={text}".to_string()],
            timeout: Duration::from_secs(1),
        };

        let request = sender.build_request("group@g.us", "hello");
        assert_eq!(request.args, vec!["to=group@g.us", "msg=hello"]);
    }

    #[tokio::test]
    async fn send_succeeds_when_command_exits_zero() {
        let sender = CommandOutboundSender {
            program: "sh".to_string(),
            args_template: vec!["-c".to_string(), "exit 0".to_string()],
            timeout: Duration::from_secs(1),
        };

        let result = sender.send("group@g.us", "hello").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn send_fails_when_command_exits_nonzero() {
        let sender = CommandOutboundSender {
            program: "sh".to_string(),
            args_template: vec!["-c".to_string(), "echo bad 1>&2; exit 3".to_string()],
            timeout: Duration::from_secs(1),
        };

        let result = sender.send("group@g.us", "hello").await;
        assert!(result.is_err());
    }
}
