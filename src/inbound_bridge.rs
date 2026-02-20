use std::future::Future;
use std::process::Stdio;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;

use crate::types::NewMessage;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundBridgeEvent {
    Message {
        message: NewMessage,
    },
    ChatMetadata {
        chat_jid: String,
        timestamp: String,
        name: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub struct CommandInboundBridge {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandInboundBridge {
    pub async fn run<F, Fut>(&self, mut on_event: F) -> Result<(), String>
    where
        F: FnMut(InboundBridgeEvent) -> Fut + Send,
        Fut: Future<Output = Result<(), String>> + Send,
    {
        self.run_with_shutdown(None, move |event| on_event(event))
            .await
    }

    pub async fn run_with_shutdown<F, Fut>(
        &self,
        shutdown_rx: Option<watch::Receiver<bool>>,
        mut on_event: F,
    ) -> Result<(), String>
    where
        F: FnMut(InboundBridgeEvent) -> Fut + Send,
        Fut: Future<Output = Result<(), String>> + Send,
    {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|err| format!("failed to spawn inbound bridge '{}': {err}", self.program))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to capture inbound bridge stdout".to_string())?;
        let mut stdout_lines = BufReader::new(stdout).lines();
        let mut shutdown_rx = shutdown_rx;
        let mut shutdown_requested = false;

        let stderr_task = {
            let mut stderr = child
                .stderr
                .take()
                .ok_or_else(|| "failed to capture inbound bridge stderr".to_string())?;
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                stderr.read_to_end(&mut buffer).await.map(|_| buffer)
            })
        };

        loop {
            tokio::select! {
                next_line = stdout_lines.next_line() => {
                    let Some(line) = next_line
                        .map_err(|err| format!("failed reading inbound bridge stdout: {err}"))?
                    else {
                        break;
                    };
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let event = serde_json::from_str::<InboundBridgeEvent>(line)
                        .map_err(|err| format!("invalid inbound bridge JSON line: {err}; line={line}"))?;
                    on_event(event).await?;
                }
                _ = async {
                    if let Some(receiver) = shutdown_rx.as_mut() {
                        wait_for_shutdown_signal(receiver).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    shutdown_requested = true;
                    break;
                }
            }
        }

        if shutdown_requested {
            let _ = child.start_kill();
        }

        let status = child
            .wait()
            .await
            .map_err(|err| format!("failed waiting inbound bridge process: {err}"))?;
        let stderr = stderr_task
            .await
            .map_err(|err| format!("stderr task join error: {err}"))?
            .map_err(|err| format!("stderr read error: {err}"))?;
        let stderr = String::from_utf8_lossy(&stderr).to_string();

        if shutdown_requested {
            return Ok(());
        }

        if !status.success() {
            return Err(format!(
                "inbound bridge exited with code {:?}: {}",
                status.code(),
                tail_chars(&stderr, 300),
            ));
        }

        Ok(())
    }
}

async fn wait_for_shutdown_signal(shutdown_rx: &mut watch::Receiver<bool>) {
    if *shutdown_rx.borrow() {
        return;
    }
    let _ = shutdown_rx.changed().await;
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

    #[tokio::test]
    async fn reads_message_and_metadata_events_from_stdout() {
        let bridge = CommandInboundBridge {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "echo '{\"type\":\"chat_metadata\",\"chat_jid\":\"group@g.us\",\"timestamp\":\"2026-02-19T00:00:00.000Z\",\"name\":\"Group\"}'; \
                 echo '{\"type\":\"message\",\"message\":{\"id\":\"m1\",\"chat_jid\":\"group@g.us\",\"sender\":\"u1\",\"sender_name\":\"Alice\",\"content\":\"hello\",\"timestamp\":\"2026-02-19T00:00:01.000Z\",\"is_from_me\":false,\"is_bot_message\":false}}'"
                    .to_string(),
            ],
        };

        let events = Arc::new(Mutex::new(Vec::<InboundBridgeEvent>::new()));
        bridge
            .run({
                let events = events.clone();
                move |event| {
                    let events = events.clone();
                    async move {
                        events.lock().expect("events lock").push(event);
                        Ok(())
                    }
                }
            })
            .await
            .expect("bridge run");

        let snapshot = events.lock().expect("events lock");
        assert_eq!(snapshot.len(), 2);
        assert!(matches!(
            snapshot[0],
            InboundBridgeEvent::ChatMetadata { .. }
        ));
        assert!(matches!(snapshot[1], InboundBridgeEvent::Message { .. }));
    }

    #[tokio::test]
    async fn returns_error_on_invalid_json_event() {
        let bridge = CommandInboundBridge {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "echo 'not-json'".to_string()],
        };

        let result = bridge.run(|_| async move { Ok(()) }).await;
        assert!(result.is_err());
    }
}
