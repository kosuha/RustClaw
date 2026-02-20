use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

fn rust_claw_bin_path() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_rust-claw") {
        return PathBuf::from(path);
    }
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_rust_claw") {
        return PathBuf::from(path);
    }

    let mut fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fallback.push("target");
    fallback.push("debug");
    fallback.push(if cfg!(windows) {
        "rust_claw.exe"
    } else {
        "rust_claw"
    });
    fallback
}

fn run_daemon_with_container_binary(temp_dir: &Path, binary: &str) -> std::process::Output {
    let db_path = temp_dir.join("messages.db");
    let data_dir = temp_dir.join("data");
    let mut command = Command::new(rust_claw_bin_path());
    command
        .arg("run")
        .env("RUST_CLAW_DB", db_path)
        .env("DATA_DIR", data_dir)
        .env("CONTAINER_BINARY", binary)
        .env("OUTBOUND_SENDER_MODE", "discord")
        .env("DISCORD_BOT_TOKEN", "")
        .env_remove("AGENT_RUNNER_MODE")
        .env_remove("DISCORD_RECONNECT_DELAY_MS")
        .env_remove("DISCORD_PREFIX_OUTBOUND");

    command.output().expect("failed to run rust-claw")
}

#[test]
fn run_default_mode_uses_container_runner() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let output = run_daemon_with_container_binary(temp_dir.path(), "false");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "expected failure, stdout={}, stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("docker runtime check failed")
            || stderr.contains("container runtime start failed"),
        "expected container runtime failure in stderr, stderr={stderr}"
    );
}

#[test]
fn run_default_mode_requires_discord_token() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let output = run_daemon_with_container_binary(temp_dir.path(), "true");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "expected failure, stdout={}, stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("DISCORD_BOT_TOKEN is required"),
        "expected missing discord token error, stderr={stderr}"
    );
}

#[cfg(unix)]
fn wait_for_exit(child: &mut std::process::Child, timeout: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            panic!("rust-claw run process did not exit within {:?}", timeout);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
fn exited_successfully_or_sigterm(status: &std::process::ExitStatus) -> bool {
    status.success() || status.signal() == Some(15)
}

#[cfg(unix)]
#[test]
fn run_exits_gracefully_on_sigterm() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("messages.db");
    let data_dir = temp_dir.path().join("data");

    let mut child = Command::new(rust_claw_bin_path())
        .arg("run")
        .env("RUST_CLAW_DB", db_path)
        .env("DATA_DIR", data_dir)
        .env("AGENT_RUNNER_MODE", "echo")
        .env("OUTBOUND_SENDER_MODE", "stdout")
        .env("TASK_EXECUTOR_MODE", "echo")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rust-claw run");

    std::thread::sleep(Duration::from_millis(900));
    if let Some(status) = child.try_wait().expect("try_wait before kill") {
        panic!("run process exited before SIGTERM could be sent: status={status:?}");
    }

    let pid = child.id().to_string();
    let kill_status = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .expect("send SIGTERM");
    assert!(kill_status.success(), "failed to send SIGTERM to pid={pid}");

    let status = wait_for_exit(&mut child, Duration::from_secs(4));
    assert!(
        exited_successfully_or_sigterm(&status),
        "expected graceful success exit after SIGTERM, status={status:?}"
    );
}
