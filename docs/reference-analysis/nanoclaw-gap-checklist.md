# NanoClaw 대비 rust-claw 미구현 체크리스트

작성일: 2026-02-19  
비교 기준: `reference/nanoclaw/docs/REQUIREMENTS.md`, `reference/nanoclaw/docs/SPEC.md`, `reference/nanoclaw/docs/SECURITY.md`, `reference/nanoclaw/src/*`  
현재 구현 기준: `src/*`, `README.md`

## 이미 구현된 기반(참고)

- [x] SQLite 스키마(메시지/작업/세션/그룹/라우터 상태)와 기본 CRUD
- [x] 그룹 큐(그룹별 직렬 실행, 전역 동시성 제한, 지수 백오프 재시도)
- [x] 오케스트레이터 폴링 루프 + 트리거 기반 메시지 처리 + 커서 저장/복구
- [x] 스케줄러 due-task 실행 + 실행 로그 + `cron/interval/once` next_run 계산
- [x] 컨테이너/커맨드 기반 러너와 마커(`---NANOCLAW_OUTPUT_START/END---`) 파싱

## P0: NanoClaw 핵심 동작 parity

- [x] `WA-01` WhatsApp 채널 어댑터 구현 (`channels/whatsapp.ts` 대응): 연결/재연결/송수신/아웃바운드 큐/typing indicator/메시지 메타데이터 수집까지 포함.
- [x] `WA-02` 그룹 메타데이터 동기화 구현: 시작 시 + 주기적(24h) 그룹명 동기화, 마지막 sync 시각 저장(`__group_sync__` 패턴 대응).
- [x] `IPC-01` 파일 기반 IPC watcher 구현 (`data/ipc/{group}/messages|tasks`): JSON 파일 폴링 + 성공 시 삭제 + 실패 시 errors 이동.
- [x] `IPC-02` IPC 권한 검증 구현: main/non-main 그룹 권한 분기(`send_message`, `schedule_task`, `pause/resume/cancel`, `register_group`, `refresh_groups`).
- [x] `QUEUE-01` 활성 컨테이너 프로세스 추적 기능 추가: 그룹 큐가 process/container_name/group_folder를 등록/해제하도록 확장. (group_folder + IPC input 추적은 구현됨, process/container_name 추적은 미구현)
- [x] `QUEUE-02` 활성 컨테이너로 메시지 파이프 기능 추가: 새 메시지를 재실행 대신 IPC input 파일로 전달(`sendMessage` equivalent).
- [x] `QUEUE-03` idle close 신호 추가: 유휴 시 `_close` sentinel 작성으로 컨테이너 종료 유도(`closeStdin` equivalent).
- [x] `RUNNER-01` 컨테이너 러너 스트리밍 모드 구현: stdout marker를 실시간 파싱하고 콜백으로 결과를 전달.
- [x] `RUNNER-02` 스트리밍 결과 기반 세션 갱신/실시간 송신 구현: `newSessionId` 즉시 반영, 부분 결과 전송 지원.
- [x] `RUNNER-03` 컨테이너 타임아웃 정책 정합화: `max(container_timeout, idle_timeout+grace)` 규칙 및 timeout 시 graceful stop 우선.
- [x] `SCHED-01` 스케줄러 실행 경로를 에이전트 실행 경로와 통합: task도 메시지와 동일한 컨테이너 런타임/세션 모델 사용.
- [x] `SCHED-02` task 실행 중 결과 전송 경로 추가: 스트리밍 결과를 아웃바운드로 전달하고 run-log/summary 업데이트.
- [x] `SNAP-01` `current_tasks.json` 스냅샷 작성 기능 추가: main은 전체, non-main은 자기 그룹만 노출.
- [x] `SNAP-02` `available_groups.json` 스냅샷 작성 기능 추가: main만 전체 그룹 활성화 목록 조회 가능.

## P1: 보안/운영 parity

- [x] `SEC-01` 추가 마운트 외부 allowlist 도입(`~/.config/...`): 프로젝트 외부 파일 기준으로 mount 허용/차단.
- [x] `SEC-02` 기본 차단 패턴 적용(`.ssh`, `.aws`, `.env`, key 파일 등) + symlink realpath 검증.
- [x] `SEC-03` allowlist 미존재/오류 시 추가 마운트 전면 차단(fail-closed) 동작.
- [x] `SEC-04` 그룹별 read-write 제한 정책 구현: non-main 강제 read-only, root별 `allowReadWrite` 반영.
- [x] `SEC-05` 컨테이너 credential 전달 allowlist 구현: 허용 키만 전달(예: `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`).
- [x] `OPS-01` 컨테이너 시스템 사전 점검 구현: 런타임 준비 상태 확인 및 시작 실패 시 명확한 에러 처리.
- [x] `OPS-02` 고아 컨테이너 정리 루틴 구현: 재시작 시 이전 실행 잔여 컨테이너 cleanup.
- [x] `OPS-03` 컨테이너 실행 로그 파일화: 그룹별 `logs/container-*.log` 기록 + stdout/stderr 크기 제한.
- [x] `OPS-04` graceful shutdown 정책 정리: 재연결/종료 시 활성 작업 처리 원칙(분리 유지 또는 안전 중단) 명시.

## P2: 기능 완성도 및 관리성

- [x] `TASK-01` task 관리 CLI/명령 보강: `pause-task`, `resume-task`, `cancel-task`, `list-tasks --group` 추가.
- [x] `TASK-02` cron 타임존 처리 정합화: 시스템 타임존 또는 설정 타임존 기준 next_run 계산.
- [x] `ROUTER-01` 스트리밍 실패 시 커서 롤백 정책 고도화: 이미 사용자 전송이 발생한 경우 중복 방지 롤백 스킵.
- [x] `DATA-01` 상태 마이그레이션 유틸 추가: 기존 JSON 상태 파일이 있다면 DB로 1회 이관.
- [x] `TEST-01` parity 테스트 세트 추가: IPC 권한, 스트리밍 파싱, task lifecycle, mount 보안 경로를 회귀 테스트로 고정.

## 권장 구현 순서

- [x] 1단계: `RUNNER-*` + `QUEUE-*` + `SCHED-01/02` (실행 경로 통합/스트리밍)
- [x] 2단계: `IPC-*` + `SNAP-*` + `WA-*` (입출력/제어면 완성)
- [x] 3단계: `SEC-*` + `OPS-*` (운영 안전성 강화)
- [x] 4단계: `TASK-*` + `ROUTER-*` + `DATA-*` + `TEST-*` (완성도 및 회귀 안정화)
