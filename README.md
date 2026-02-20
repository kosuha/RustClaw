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
- An OpenAI account with an active plan
- Discord Bot Token and Channel ID

## License

MIT
