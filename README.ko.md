# RustClaw

[한국어](README.ko.md) | [English](README.md)

[NanoClaw](https://github.com/qwibitai/nanoclaw)의 구조를 가져와서 Codex OAuth로 사용할 수 있게 만든 Rust 기반 AI 어시스턴트

## Why I Built This

저는 NanoClaw의 구조가 마음에 들어서 이 AI 어시스턴트를 사용하고 싶었습니다. 하지만 Claude Agent SDK가 NanoClaw에서 핵심이기 때문에 Claude API 사용이 필수적이었습니다. 저는 API 비용이 부담스러웠고 제가 사용하는 GPT Pro 플랜을 활용하고 싶었습니다. 그래서 NanoClaw의 구조를 가져와서 새로운 AI 어시스턴트를 개발했습니다.

## NanoClaw와 다른 점

- 채널: **Discord**
- 에이전트 런타임: 컨테이너 안에서 `codex app-server` 경로 사용
- 코어 구현: 오케스트레이터를 Rust로 재구현

## RustClaw로 할 수 있는 것

- Discord 채널 메시지를 받아 AI 답변을 보냅니다.
- **메인 채널 1개**는 `@이름` 없이도 바로 동작합니다.
- 추가 Discord 채널을 그룹으로 등록해 분리 운영할 수 있습니다.
- 스케줄 작업을 `cron`, `interval`(밀리초), `once`로 실행할 수 있습니다.
- CLI로 작업을 조회/일시정지/재개/취소할 수 있습니다.
- 고급 IPC 기능으로 그룹 등록, 작업 생성, 스킬 on/off를 처리할 수 있습니다.
- IPC 권한 규칙:
  메인 그룹은 전체 그룹을 관리할 수 있고, 일반 그룹은 자기 그룹만 관리할 수 있습니다.

## 설치

필요한 것:

- Rust
- Docker
- Codex CLI
- Plan을 구독 중인 OpenAI 계정
- Discord Bot Token

처음 한 번은 아래를 먼저 설치하세요:

1. Rust와 Cargo 설치

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version
cargo --version
```

2. Docker 설치
   - macOS / Windows: Docker Desktop 설치: <https://docs.docker.com/desktop/>
   - Linux: Docker Engine 설치: <https://docs.docker.com/engine/install/>

```bash
docker --version
```

3. Codex CLI 설치
   - `npm` 명령이 없으면 먼저 Node.js LTS 설치: <https://nodejs.org/en/download>
   - SSH/서버 환경에서는 device auth 로그인을 사용하세요.

```bash
npm install -g @openai/codex
codex --version
codex login --device-auth
```

같은 PC에서 브라우저를 함께 쓰는 로컬 환경이면 아래도 가능합니다:

```bash
codex login
```

의존성 설치:

```bash
cargo build
```

## 실행 (권장: 호스트 프로세스 + Docker 런타임)

1. 환경변수를 준비합니다.

```bash
cp .env.example .env
```

2. `.env`에 값을 입력합니다.

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

3. 호스트에서 Codex 로그인을 합니다 (SSH/서버 권장).

```bash
codex login --device-auth
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

## 데몬으로 실행 (systemd, Linux 서버)

운영 서버에서는 SSH가 끊기거나 재부팅되어도 계속 동작하도록 이 방식을 권장합니다.

1. 먼저 릴리즈 바이너리를 빌드합니다.

```bash
cargo build --release
```

2. systemd 서비스 파일을 만듭니다.

```bash
sudo tee /etc/systemd/system/rust-claw.service >/dev/null <<'EOF'
[Unit]
Description=RustClaw daemon
After=network-online.target docker.service
Wants=network-online.target docker.service

[Service]
Type=simple
User=<your_linux_user>
WorkingDirectory=/absolute/path/to/rust-claw
EnvironmentFile=/absolute/path/to/rust-claw/.env
ExecStart=/absolute/path/to/rust-claw/target/release/rust_claw run
Restart=always
RestartSec=5
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
EOF
```

3. 서비스 자동 시작/즉시 실행:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now rust-claw
```

4. 상태와 로그 확인:

```bash
sudo systemctl status rust-claw --no-pager
journalctl -u rust-claw -f
```

## 에이전트 사용법 (Discord)

1. 먼저 메인 채널이 등록되어 있어야 합니다.
   - 권장: `.env`에 `AUTO_REGISTER_MAIN_JID=<discord_channel_id>@discord` 설정
   - 또는 1회 수동 등록:

```bash
cargo run -- bootstrap-main --jid <discord_channel_id>@discord
```

2. 앱 실행:

```bash
cargo run -- run
```

3. 메시지 보내기:
   - 메인 채널(`folder=main`): 일반 메시지로 바로 동작
   - 일반 채널(기본값): 메시지 맨 앞에 `@<ASSISTANT_NAME>`가 있어야 동작

4. 트리거 예시 (`ASSISTANT_NAME=Andy`일 때):
   - 동작함: `@Andy 이 대화 요약해줘`
   - 무시됨: `이거 @Andy 요약해줘`

5. 일반 채널도 트리거 없이 쓰고 싶다면 `--requires-trigger false`로 등록하세요.

## CLI 관리자 사용법

```bash
# 그룹 목록 확인
cargo run -- list-groups

# 다른 Discord 채널을 그룹으로 등록
cargo run -- register-group \
  --jid <discord_channel_id>@discord \
  --name "Team Ops" \
  --folder team-ops \
  --trigger @Andy \
  --requires-trigger true

# 스케줄 작업 생성 (매일 09:00)
cargo run -- create-task \
  --id daily-report \
  --group-folder main \
  --chat-jid <discord_channel_id>@discord \
  --prompt "오늘 일일 보고서 작성해줘." \
  --schedule-type cron \
  --schedule-value "0 9 * * *"

# 작업 목록/제어
cargo run -- list-tasks
cargo run -- pause-task --id daily-report
cargo run -- resume-task --id daily-report
cargo run -- cancel-task --id daily-report
```

## 참고

- 기본 런타임 모드는 `AGENT_RUNNER_MODE=container` 입니다.
- 메인 Rust 프로세스는 호스트에서 실행되고, 작업별 에이전트 컨테이너를 Docker로 생성합니다.
- 데이터/상태는 `data/`, `groups/`, `store/` 아래에 저장됩니다.
- 기본값으로는 API 키를 에이전트 컨테이너에 전달하지 않습니다.

## License

MIT
