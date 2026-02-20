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

2. `.env`에 필수 값을 입력합니다.

```env
DISCORD_BOT_TOKEN=your_discord_bot_token
AUTO_REGISTER_MAIN_JID=<discord_channel_id>@discord
```

`AUTO_REGISTER_MAIN_JID` 형식:
- `<discord_channel_id>@discord`
- 예시: `123456789012345678@discord`

Discord 채널 ID 확인 방법:
- Discord `설정 -> 고급`에서 `개발자 모드`를 켭니다.
- 대상 채널을 우클릭하고 `채널 ID 복사`를 선택합니다.

`AUTO_REGISTER_MAIN_JID`를 넣지 않으면, 시작 전에 1회 수동 등록이 필요합니다:

```bash
cargo run -- bootstrap-main --jid <channel_id>@discord
```

3. 호스트에서 Codex 로그인을 합니다.

```bash
codex login
```

4. 에이전트 실행용 컨테이너 이미지를 빌드합니다.

```bash
docker build -t rust-claw-codex-agent:latest ./container/codex-agent-runner
```

5. rust-claw를 실행합니다.

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
- 기본값으로는 API 키를 에이전트 컨테이너에 전달하지 않습니다.

## License

MIT
