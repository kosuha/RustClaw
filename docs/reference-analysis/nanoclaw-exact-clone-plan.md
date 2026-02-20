# NanoClaw Exact-Clone Plan (rust-claw)

작성일: 2026-02-19  
목표: `reference/nanoclaw/src/*` 기준 동작/기본 실행 경로를 rust-claw에서 최대한 동일하게 맞춘다.

## 기준(Definition of Done)
- 기본 `run` 경로가 NanoClaw처럼 WhatsApp + container 실행 경로를 기본으로 사용한다.
- 트리거/스케줄/그룹 sync/컨테이너 네이밍 등 런타임 핵심 규칙이 NanoClaw와 동일하게 동작한다.
- 차이가 남는 항목은 문서화하고 후속 단계로 분리한다.

## 현재 확인된 주요 갭
1. [DONE] 기본 실행 모드 불일치
- rust-claw 기본값: `AGENT_RUNNER_MODE=container`, `OUTBOUND_SENDER_MODE=whatsapp`
- NanoClaw 기본값: WhatsApp + container
- 근거: `src/main.rs`, `reference/nanoclaw/src/index.ts`

2. [DONE] WhatsApp 채널 구현 방식 불일치
- rust-claw: `builtin`은 단일 adapter 프로세스(stdin/stdout JSONL) 기반 내장 런타임, `command`는 split-command 호환 모드
- NanoClaw: Baileys 내장 채널
- 근거: `src/whatsapp_builtin_channel.rs`, `src/whatsapp_channel.rs`, `reference/nanoclaw/src/channels/whatsapp.ts`

3. [DONE] 트리거 매칭 규칙 불일치
- rust-claw: 단순 prefix starts_with
- NanoClaw: `^@ASSISTANT_NAME\\b` (case-insensitive regex)
- 근거: `src/orchestrator.rs`, `reference/nanoclaw/src/config.ts`

4. [DONE] cron 타임존 초기 계산 불일치(IPC)
- rust-claw(기존) IPC `schedule_task` cron 초기 next_run: UTC 기준
- rust-claw(현재): `TIMEZONE`/`TZ`(legacy fallback: `TASK_TIMEZONE`) 또는 system timezone 기준
- NanoClaw: `TIMEZONE`(`TZ` or system timezone) 기준
- 근거: `src/ipc_watcher.rs`, `reference/nanoclaw/src/ipc.ts`

5. [DONE] 그룹 메타데이터 sync 주기 tick 동작 불일치
- rust-claw: periodic loop 첫 tick 즉시 실행 가능
- NanoClaw: startup sync 1회 + 24h interval (첫 interval 실행은 24h 후)
- 근거: `src/main.rs`, `reference/nanoclaw/src/channels/whatsapp.ts`

6. [DONE] 컨테이너 이름/고아 정리 prefix 불일치
- rust-claw: `rust-claw-`
- NanoClaw: `nanoclaw-`
- 근거: `src/agent_runner_container.rs`, `src/main.rs`, `reference/nanoclaw/src/container-runner.ts`, `reference/nanoclaw/src/index.ts`

## 실행 계획

### Phase 1 (즉시 실행, 코드 리스크 낮음)
- [x] 트리거 매칭을 NanoClaw regex semantics로 정렬
- [x] IPC cron 초기 next_run에 NanoClaw timezone semantics 적용
- [x] WhatsApp group sync periodic tick을 NanoClaw와 동일하게 정렬
- [x] 컨테이너 name/prefix 및 orphan cleanup prefix 정렬

### Phase 2 (핵심 구조 정렬)
- [x] 기본 `run` 경로를 WhatsApp + container 중심으로 정렬
- [x] `TIMEZONE`/`TZ` 중심 설정 체계 정렬
- [x] 로그/에러 메시지/운영 동작(시작/종료) 세부 정렬

### Phase 3 (큰 구조 변경)
- [x] Baileys 기반 WhatsApp 채널을 Rust 내장 구현으로 전환(또는 동등 어댑터 완성)
- [x] command 기반 WhatsApp 모드를 부가 모드로 격하
- [x] NanoClaw 수준 end-to-end parity 테스트 세트 추가 (현재 구현 범위 기준)

## Execution Log
- 2026-02-19: 계획 문서 생성.
- 2026-02-19: Phase 1 구현 시작.
- 2026-02-19: Phase 1 구현 완료.
  - `has_trigger`를 `(?i)^@name\\b` regex 규칙으로 변경.
  - IPC `schedule_task` cron 초기 계산에 `TASK_TIMEZONE`/`TZ` 반영.
  - WhatsApp group sync periodic loop 첫 tick 즉시 실행 방지.
  - 컨테이너 이름/고아 정리 prefix를 `nanoclaw-`로 정렬.
- 2026-02-19: Phase 2-1 구현 완료.
  - `run_daemon` 기본값을 NanoClaw와 동일하게 `AGENT_RUNNER_MODE=container`, `OUTBOUND_SENDER_MODE=whatsapp`로 정렬.
  - README 기본 동작/운영 설명(`default mode`, orphan cleanup prefix)을 현재 구현과 일치하도록 갱신.
- 2026-02-19: Phase 2-2 구현 완료.
  - cron timezone 해석 우선순위를 `TIMEZONE -> TZ -> TASK_TIMEZONE(legacy) -> system timezone`으로 정렬.
  - `task_scheduler`/`ipc_watcher`가 동일한 timezone 우선순위를 사용하도록 통일.
- 2026-02-19: Phase 2-3 구현 완료.
  - 시작 로그를 NanoClaw 흐름과 유사하게 정렬(`NanoClaw running (trigger: @...)`, container runtime 준비/시작 로그).
  - 종료 시그널 처리 범위를 `Ctrl+C(SIGINT)` + `SIGTERM`으로 확대해 graceful shutdown 동작을 정렬.
- 2026-02-19: Phase 3-3 1차 진행.
  - CLI parity smoke test 추가: 기본 `run` 모드가 `container + whatsapp` 기본값을 실제로 강제하는지 통합 테스트로 검증.
  - `run` 프로세스의 `SIGTERM` graceful 종료 경로를 통합 테스트로 검증.
- 2026-02-19: Phase 3-2 1차 진행.
  - WhatsApp 채널 백엔드 모드 분리: `WHATSAPP_CHANNEL_MODE`(`command` default, `builtin` reserved) 도입.
  - `builtin` 선택 시 명시적 에러를 반환하도록 하여 command adapter를 호환 모드로 분리.
- 2026-02-19: Phase 3-2 2차 진행.
  - `WHATSAPP_CHANNEL_MODE` 기본값을 `builtin`으로 전환하고 Rust 내장 백엔드를 기본 실행 경로로 정렬.
  - `WHATSAPP_CHANNEL_MODE=command`는 호환용 alias로 유지(경고 로그 출력)하여 부가 모드로 격하.
- 2026-02-19: Phase 3-3 2차 진행.
  - WhatsApp group sync cadence parity 통합 테스트 추가:
    - startup 시 1회 실행 후 첫 interval 전에는 재실행되지 않음
    - `__group_sync__` 최근 동기화 기록이 있으면 startup sync를 건너뜀
- 2026-02-19: Phase 3-3 3차 진행.
  - `WHATSAPP_CHANNEL_MODE=command` 호환 모드 런타임 경로를 통합 테스트로 고정.
  - `WHATSAPP_CHANNEL_MODE` unknown 값 거부 경로를 회귀 테스트로 고정.
- 2026-02-19: Phase 3-1 구현 완료.
  - `WHATSAPP_CHANNEL_MODE=builtin` 런타임을 split-command 계층에서 분리하고 단일 adapter 프로세스(JSONL stdin/stdout) 기반으로 전환.
  - builtin 모드 필수 설정을 `WHATSAPP_ADAPTER_PROGRAM`/`WHATSAPP_ADAPTER_ARGS_JSON`로 재정의.
  - 그룹 sync 경로를 mode별로 분리(builtin: 채널 내부 `sync_groups`, command: 기존 `WHATSAPP_GROUP_SYNC_PROGRAM` 유지)하고 parity 테스트를 갱신.
