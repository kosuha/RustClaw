# RustClaw

[English](README.md) | [한국어](README.ko.md)

A Rust-based AI assistant built from the [NanoClaw](https://github.com/qwibitai/nanoclaw) architecture so it can run with Codex OAuth.

## Why I Built This

I liked NanoClaw's architecture and wanted to use this style of AI assistant. However, NanoClaw relies on the Claude Agent SDK, so using the Claude API is required. API cost was a concern for me, and I wanted to use my GPT Pro plan. So I reused NanoClaw's structure and developed a new assistant around it.

## How It's Different from NanoClaw

- Channel: **Discord**
- Agent runtime: uses `codex app-server` path inside a container
- Core implementation: reimplemented the orchestrator in Rust

## Installation

Requirements:

- Rust
- Docker
- Codex CLI
- OpenAI account (active plan)
- Discord Bot Token

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

3. Login Codex on host.

```bash
codex login
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

## Notes

- Default runtime mode is `AGENT_RUNNER_MODE=container`.
- The main Rust process runs on host, and it spawns per-task agent containers via Docker.
- Data/state are written under `data/`, `groups/`, and `store/`.
- By default, no API keys are passed into agent containers.

## License

MIT
