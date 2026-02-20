# RustClaw

[한국어](README.ko.md) | [English](README.md)

[NanoClaw](https://github.com/qwibitai/nanoclaw)의 구조를 가져와서 Codex OAuth로 사용할 수 있게 만든 Rust 기반 AI 어시스턴트

## Why I Built This

저는 NanoClaw의 구조가 마음에 들어서 이 AI 어시스턴트를 사용하고 싶었습니다. 하지만 Claude Agent SDK가 NanoClaw에서 핵심이기 때문에 Claude API 사용이 필수적이었습니다. 저는 API 비용이 부담스러웠고 제가 사용하는 GPT Pro 플랜을 활용하고 싶었습니다. 그래서 NanoClaw의 구조를 가져와서 새로운 AI 어시스턴트를 개발했습니다.

## NanoClaw와 다른 점

- 채널: **Discord**
- 에이전트 런타임: 컨테이너 안에서 `codex app-server` 경로 사용
- 코어 구현: 오케스트레이터를 Rust로 재구현

## 설치

필요한 것:

- Rust
- Docker
- Codex CLI
- Plan을 구독 중인 OpenAI 계정
- Discord Bot Token, Channel ID

## License

MIT
