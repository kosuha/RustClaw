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
- Discord Bot Token

의존성 설치:

```bash
cargo build
```

## 실행 (권장: 호스트 프로세스 + Docker 런타임)

1. 환경변수를 준비합니다.

```bash
cp .env.example .env
```

필수:

- `DISCORD_BOT_TOKEN`
- `AUTO_REGISTER_MAIN_JID` (Discord 모드 첫 실행 권장)

선택(권장):

- `OPENAI_API_KEY`
- `CODEX_AUTH_DIR` (기본값: `~/.codex`)

`AUTO_REGISTER_MAIN_JID`를 설정하지 않으면, 사용 전에 메인 그룹을 수동 등록해야 합니다:

```bash
cargo run -- bootstrap-main --jid <channel_id>@discord
```

2. 호스트에서 Codex 로그인을 합니다.

```bash
codex login
```

3. 에이전트 실행용 컨테이너 이미지를 빌드합니다.

```bash
docker build -t rust-claw-codex-agent:latest ./container/codex-agent-runner
```

4. rust-claw를 실행합니다.

```bash
cargo run -- run
```

또는 릴리즈 바이너리로 실행:

```bash
cargo build --release
./target/release/rust_claw run
```

## 참고

- 기본 런타임 모드는 `AGENT_RUNNER_MODE=container` 입니다.
- 메인 Rust 프로세스는 호스트에서 실행되고, 작업별 에이전트 컨테이너를 Docker로 생성합니다.
- 데이터/상태는 `data/`, `groups/`, `store/` 아래에 저장됩니다.

## License

MIT
