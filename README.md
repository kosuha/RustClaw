# RustClaw

[English](README.md) | [한국어](README.ko.md)

A Rust-based AI assistant built from the [NanoClaw](https://github.com/qwibitai/nanoclaw) architecture so it can run with Codex OAuth.

## Why I Built This

I liked NanoClaw's architecture and wanted to use this style of AI assistant. However, NanoClaw relies on the Claude Agent SDK, so using the Claude API is required. API cost was a concern for me, and I wanted to use my GPT Pro plan. So I reused NanoClaw's structure and developed a new assistant around it.

## How It's Different from NanoClaw

- Channel: **Discord**
- Agent runtime: uses `codex app-server` path inside a container
- Core implementation: reimplemented the orchestrator in Rust

## What RustClaw Can Do

- Receive messages from Discord channels and send AI replies.
- Keep one **main channel** that works without `@AssistantName` trigger.
- Register additional Discord channels as groups.
- Run scheduled prompts with `cron`, `interval` (milliseconds), or `once`.
- Manage tasks from CLI: list, pause, resume, cancel.
- Handle advanced IPC actions: schedule tasks, register groups, enable/disable skills.
- IPC permission rule: main group can manage all groups; non-main groups can manage only themselves.

## Installation

Requirements:

- Rust
- Docker
- Codex CLI
- OpenAI account (active plan)
- Discord Bot Token

Install prerequisites (first-time setup):

1. Install Rust and Cargo.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version
cargo --version
```

2. Install Docker.
   - macOS / Windows: install Docker Desktop: <https://docs.docker.com/desktop/>
   - Linux: install Docker Engine: <https://docs.docker.com/engine/install/>

```bash
docker --version
```

3. Install Codex CLI.
   - Install Node.js LTS first if `npm` is not available: <https://nodejs.org/en/download>
   - For SSH/server environments, use device auth login.

```bash
npm install -g @openai/codex
codex --version
codex login --device-auth
```

If you are on a local machine with a browser on the same host, you can also use:

```bash
codex login
```

Install dependencies:

```bash
cargo build
```

## Run (Recommended: host process + Docker runtime)

1. Prepare environment variables.

```bash
cp .env.example .env
```

2. Set required values in `.env`.

```env
DISCORD_BOT_TOKEN=your_discord_bot_token
AUTO_REGISTER_MAIN_JID=<discord_channel_id>@discord
```

`AUTO_REGISTER_MAIN_JID` format:
- `<discord_channel_id>@discord`
- example: `123456789012345678@discord`

How to copy Discord channel ID:
- Open Discord Settings -> Advanced -> turn on Developer Mode.
- Right-click your target channel -> Copy Channel ID.

If `AUTO_REGISTER_MAIN_JID` is not set, register once manually:

```bash
cargo run -- bootstrap-main --jid <channel_id>@discord
```

3. Login Codex on host (recommended for SSH/server).

```bash
codex login --device-auth
```

4. Build the agent runner container image.

```bash
docker build -t rust-claw-codex-agent:latest ./container/codex-agent-runner
```

5. Start rust-claw.

```bash
cargo run -- run
```

Or run release binary:

```bash
cargo build --release
./target/release/rust_claw run
```

## Run As a Daemon (systemd, Linux server)

Use this for production servers so rust-claw keeps running after SSH disconnect/reboot.

1. Build release binary first.

```bash
cargo build --release
```

2. Run the auto installer (recommended).

```bash
sudo ./scripts/install-systemd-service.sh
```

If you need custom values:

```bash
sudo ./scripts/install-systemd-service.sh \
  --user <linux_user> \
  --project-root /absolute/path/to/rust-claw \
  --service-name rust-claw
```

3. Check service status and logs.

```bash
sudo systemctl status rust-claw --no-pager
journalctl -u rust-claw -f
```

4. Optional manual method:
   - Template file: `docs/systemd/rust-claw.service`
   - Install path: `/etc/systemd/system/rust-claw.service`

## Agent Usage (Discord)

1. Make sure a main channel is registered.
   - Recommended: set `AUTO_REGISTER_MAIN_JID=<discord_channel_id>@discord` in `.env`.
   - Or register once manually:

```bash
cargo run -- bootstrap-main --jid <discord_channel_id>@discord
```

2. Start the app:

```bash
cargo run -- run
```

3. Send messages:
   - Main channel (`folder=main`): normal messages are enough.
   - Non-main channel (default): message must start with `@<ASSISTANT_NAME>`.

4. Trigger examples (when `ASSISTANT_NAME=Andy`):
   - Works: `@Andy summarize this thread`
   - Ignored: `please @Andy summarize this thread`

5. If you want a non-main channel to work without a trigger, register it with `--requires-trigger false`.

## CLI Admin Usage

```bash
# Show groups
cargo run -- list-groups

# Register another Discord channel as a group
cargo run -- register-group \
  --jid <discord_channel_id>@discord \
  --name "Team Ops" \
  --folder team-ops \
  --trigger @Andy \
  --requires-trigger true

# Create a scheduled task (every day at 09:00)
cargo run -- create-task \
  --id daily-report \
  --group-folder main \
  --chat-jid <discord_channel_id>@discord \
  --prompt "Write today's daily report." \
  --schedule-type cron \
  --schedule-value "0 9 * * *"

# List/control tasks
cargo run -- list-tasks
cargo run -- pause-task --id daily-report
cargo run -- resume-task --id daily-report
cargo run -- cancel-task --id daily-report
```

## Notes

- Default runtime mode is `AGENT_RUNNER_MODE=container`.
- The main Rust process runs on host, and it spawns per-task agent containers via Docker.
- Data/state are written under `data/`, `groups/`, and `store/`.
- By default, no API keys are passed into agent containers.

## License

MIT
