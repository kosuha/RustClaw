use std::fmt::{Display, Formatter};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::timeout;

pub const OUTPUT_START_MARKER: &str = "---NANOCLAW_OUTPUT_START---";
pub const OUTPUT_END_MARKER: &str = "---NANOCLAW_OUTPUT_END---";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerOutputStatus {
    Success,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerOutput {
    pub status: ContainerOutputStatus,
    pub result: Option<String>,
    #[serde(rename = "newSessionId")]
    pub new_session_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Default, Debug)]
pub struct OutputMarkerParser {
    buffer: String,
}

impl OutputMarkerParser {
    pub fn push_chunk(&mut self, chunk: &str) -> Vec<Result<ContainerOutput, String>> {
        self.buffer.push_str(chunk);
        let mut parsed = Vec::new();

        loop {
            let Some(start_idx) = self.buffer.find(OUTPUT_START_MARKER) else {
                // Prevent unbounded growth from pre-marker noise.
                if self.buffer.len() > OUTPUT_START_MARKER.len() {
                    let keep_from = self.buffer.len() - OUTPUT_START_MARKER.len();
                    self.buffer.drain(..keep_from);
                }
                break;
            };

            let search_start = start_idx + OUTPUT_START_MARKER.len();
            let Some(relative_end_idx) = self.buffer[search_start..].find(OUTPUT_END_MARKER) else {
                // Keep only the incomplete marker block.
                if start_idx > 0 {
                    self.buffer.drain(..start_idx);
                }
                break;
            };

            let end_idx = search_start + relative_end_idx;
            let json_payload = self.buffer[search_start..end_idx].trim().to_string();

            let parsed_output = serde_json::from_str::<ContainerOutput>(&json_payload)
                .map_err(|err| format!("failed to parse marker payload as JSON: {err}"));
            parsed.push(parsed_output);

            let drop_until = end_idx + OUTPUT_END_MARKER.len();
            self.buffer.drain(..drop_until);
        }

        parsed
    }
}

pub fn parse_all_outputs(stdout: &str) -> (Vec<ContainerOutput>, Vec<String>) {
    let mut parser = OutputMarkerParser::default();
    let mut outputs = Vec::new();
    let mut errors = Vec::new();

    for parsed in parser.push_chunk(stdout) {
        match parsed {
            Ok(output) => outputs.push(output),
            Err(error) => errors.push(error),
        }
    }

    (outputs, errors)
}

pub fn parse_last_output(stdout: &str) -> Result<ContainerOutput, String> {
    let (outputs, errors) = parse_all_outputs(stdout);
    if let Some(last) = outputs.into_iter().last() {
        return Ok(last);
    }

    if !errors.is_empty() {
        return Err(errors.join("; "));
    }

    let Some(last_line) = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
    else {
        return Err("no output found".to_string());
    };

    serde_json::from_str(last_line)
        .map_err(|err| format!("failed to parse fallback output line as JSON: {err}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRunRequest {
    pub program: String,
    pub args: Vec<String>,
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRunResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub outputs: Vec<ContainerOutput>,
    pub parse_errors: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommandRunError {
    Timeout {
        timeout: Duration,
    },
    SpawnFailed(String),
    NonZeroExit {
        exit_code: Option<i32>,
        stderr_tail: String,
        stdout_tail: String,
    },
}

impl Display for CommandRunError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandRunError::Timeout { timeout } => {
                write!(f, "process timed out after {}ms", timeout.as_millis())
            }
            CommandRunError::SpawnFailed(error) => write!(f, "failed to spawn process: {error}"),
            CommandRunError::NonZeroExit {
                exit_code,
                stderr_tail,
                ..
            } => write!(
                f,
                "process exited with code {:?}: {}",
                exit_code, stderr_tail
            ),
        }
    }
}

impl std::error::Error for CommandRunError {}

pub async fn run_command(request: &CommandRunRequest) -> Result<CommandRunResult, CommandRunError> {
    let mut command = Command::new(&request.program);
    command.args(&request.args);

    let output = timeout(request.timeout, command.output())
        .await
        .map_err(|_| CommandRunError::Timeout {
            timeout: request.timeout,
        })?
        .map_err(|err| CommandRunError::SpawnFailed(err.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let (parsed_outputs, parse_errors) = parse_all_outputs(&stdout);

    if !output.status.success() {
        return Err(CommandRunError::NonZeroExit {
            exit_code: output.status.code(),
            stderr_tail: tail_chars(&stderr, 200),
            stdout_tail: tail_chars(&stdout, 200),
        });
    }

    Ok(CommandRunResult {
        exit_code: output.status.code(),
        stdout,
        stderr,
        outputs: parsed_outputs,
        parse_errors,
    })
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
    use super::*;

    #[test]
    fn marker_parser_parses_chunked_outputs() {
        let mut parser = OutputMarkerParser::default();
        let chunk1 = "---NANOCLAW_OUTPUT_";
        let chunk2 = "START---\n{\"status\":\"success\",\"result\":\"hello\",\"newSessionId\":\"s1\",\"error\":null}\n---NANOCLAW_OUTPUT_END---";

        let first = parser.push_chunk(chunk1);
        assert!(first.is_empty());

        let second = parser.push_chunk(chunk2);
        assert_eq!(second.len(), 1);
        assert_eq!(
            second[0],
            Ok(ContainerOutput {
                status: ContainerOutputStatus::Success,
                result: Some("hello".to_string()),
                new_session_id: Some("s1".to_string()),
                error: None,
            })
        );
    }

    #[test]
    fn marker_parser_reports_json_errors_and_continues() {
        let mut parser = OutputMarkerParser::default();
        let payload = format!(
            "{start}\nnot-json\n{end}\n{start}\n{{\"status\":\"error\",\"result\":null,\"newSessionId\":null,\"error\":\"boom\"}}\n{end}",
            start = OUTPUT_START_MARKER,
            end = OUTPUT_END_MARKER
        );

        let parsed = parser.push_chunk(&payload);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].is_err());
        assert_eq!(
            parsed[1],
            Ok(ContainerOutput {
                status: ContainerOutputStatus::Error,
                result: None,
                new_session_id: None,
                error: Some("boom".to_string()),
            })
        );
    }

    #[test]
    fn parse_last_output_uses_fallback_line_when_no_markers() {
        let stdout = "debug line\n{\"status\":\"success\",\"result\":\"ok\",\"newSessionId\":null,\"error\":null}\n";
        let parsed = parse_last_output(stdout).expect("parse output");
        assert_eq!(parsed.status, ContainerOutputStatus::Success);
        assert_eq!(parsed.result.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn run_command_collects_marker_outputs() {
        let script = format!(
            "echo '{start}'; echo '{{\"status\":\"success\",\"result\":\"hello\",\"newSessionId\":\"sess-1\",\"error\":null}}'; echo '{end}'",
            start = OUTPUT_START_MARKER,
            end = OUTPUT_END_MARKER
        );

        let result = run_command(&CommandRunRequest {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), script],
            timeout: Duration::from_secs(2),
        })
        .await
        .expect("run command");

        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].result.as_deref(), Some("hello"));
        assert!(result.parse_errors.is_empty());
    }
}
