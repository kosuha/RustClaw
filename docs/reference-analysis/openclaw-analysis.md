# OpenClaw 분석 문서

## 1. 프로젝트 요약

`openclaw`는 다채널 메시징과 다중 클라이언트를 단일 Gateway 제어면(control plane)으로 통합한 대형 개인용 에이전트 플랫폼이다.  
특징은 다음과 같다.

- WebSocket 기반 Gateway 아키텍처
- 채널/앱/노드/웹 UI/CLI를 하나의 프로토콜로 통합
- 멀티에이전트 + 서브에이전트 런타임을 1급 기능으로 제공

---

## 2. 핵심 설계 철학

문서(`VISION.md`) 기준 우선순위는 명확하다.

1. 보안 기본값 강화
2. 안정성/버그 수정
3. 온보딩 신뢰성
4. 확장성(채널/모델/플러그인)

또한 코어는 lean하게 유지하고, 확장은 plugin/skill로 분리하려는 방향이 강하다.

---

## 3. 아키텍처 구성

## 3.1 Gateway 중심 구조

- `src/index.ts`가 CLI 진입점이며 환경 로드/런타임 가드/프로그램 등록을 수행한다.
- 실질적 중심은 Gateway(WS 서버)로, 채널 연결과 이벤트 브로드캐스트를 담당한다.
- macOS app, CLI, Web UI, 모바일 노드가 동일 WS 프로토콜에 연결된다.

## 3.2 프로토콜/클라이언트 모델

`docs/concepts/architecture.md` 기준:

- 첫 프레임 `connect` 강제
- req/res/event 프레임 체계
- side-effect 메서드(idempotency key) 중복 방지
- node role 기반 기능 선언(caps/commands)

이 구조는 운영 표준화와 원격 제어에 강하다.

## 3.3 멀티채널/멀티에이전트 라우팅

- `src/channels/registry.ts`가 채널 메타/정규화/별칭 처리의 중심이다.
- 채널별 정책/문서/프라이머를 중앙화해 확장 시 일관성을 높인다.
- 다중 agent/workspace/session 분리가 config 계층에서 지원된다.

## 3.4 세션 시스템

- `src/commands/agent/session.ts`는 session key 해석, 스토어 선택, 신선도 평가, reset 정책을 처리한다.
- per-agent 스토어 분리, sessionKey/sessionId 재해석 등 복잡한 실운영 케이스를 다룬다.

## 3.5 서브에이전트 런타임

- `src/agents/subagent-spawn.ts`가 `sessions_spawn` 핵심 로직이다.
- depth 제한(`maxSpawnDepth`), 자식 수 제한(`maxChildrenPerAgent`), 타겟 agent 허용 목록을 검사한다.
- 자식 세션 생성 후 비동기 실행(lane=`subagent`)과 완료 announce를 자동 연결한다.
- `src/agents/subagent-registry.ts`는 런 레지스트리, 완료 이벤트 처리, announce 재시도/포기 기준, 자동 archive를 담당한다.
- `/subagents` 명령군(`commands-subagents.ts`)으로 운영 제어(list/kill/log/info/steer/spawn)를 제공한다.

## 3.6 샌드박스/툴 정책 계층

`docs/tools/multi-agent-sandbox-tools.md` 기준 정책 계층이 정교하다.

1. 글로벌/에이전트별 sandbox
2. 글로벌/채널/에이전트별 tool allow/deny
3. sandbox 전용 정책
4. subagent 전용 정책

우선순위를 명확히 문서화해 "왜 막혔는지"를 설명 가능한 구조로 설계했다.

---

## 4. 멀티에이전트 관점 핵심 인사이트

- 멀티에이전트를 "채널 분리" 수준이 아니라, session tree와 policy tree로 모델링한다.
- 메인/서브/중첩 서브에이전트의 depth 개념을 정식 지원한다.
- completion announce, retry/backoff, auto-archive까지 운영 lifecycle을 내장했다.
- agent별 인증 저장소 분리 원칙을 가져가면서도 fallback 모델을 함께 제공한다.

---

## 5. 장점

- 대규모 통합 플랫폼으로서 기능 완성도가 높다.
- 제어면(Gateway) 중심 구조라 도구/클라이언트 확장이 쉽다.
- 서브에이전트 운영 기능이 실제 운영 이슈(재시도, 중단, 정리)까지 반영되어 있다.
- 문서가 풍부하고 정책 우선순위가 비교적 명확하다.

---

## 6. 리스크 및 한계

- 코드/설정 표면이 매우 커서 신규 프로젝트 초기에는 과도한 복잡성이 될 수 있다.
- 운영 안전성 확보를 위해 숙지해야 할 정책 계층이 많다.
- 단일 게이트웨이에 기능이 집중되므로 장애 시 영향 반경이 넓다.
- 개인 프로젝트가 단순 use-case라면 기능 과잉이 발생할 수 있다.

---

## 7. rust-claw 적용 포인트

초기 프로젝트 관점에서 실용적인 채택 순서는 아래와 같다.

1. Gateway 프로토콜 패턴만 먼저 채택  
`connect -> req/res -> event` 표준 프레임과 idempotency 키 체계부터 도입한다.

2. 서브에이전트는 최소형으로 시작  
`spawn -> run -> announce` 3단계만 구현하고, depth 제한/자식 제한을 하드코딩으로 먼저 적용한다.

3. 정책 계층은 2단계로 축소  
초기에는 글로벌 정책 + 에이전트 정책만 지원하고, provider/subagent 별도 계층은 후속으로 확장한다.

4. 레지스트리와 정리 작업 분리  
subagent registry(상태)와 cleanup worker(정리)를 분리해 추후 확장을 대비한다.

---

## 8. 분석 근거 파일

- `reference/openclaw/README.md`
- `reference/openclaw/VISION.md`
- `reference/openclaw/docs/concepts/architecture.md`
- `reference/openclaw/docs/concepts/agent.md`
- `reference/openclaw/docs/tools/subagents.md`
- `reference/openclaw/docs/tools/multi-agent-sandbox-tools.md`
- `reference/openclaw/src/index.ts`
- `reference/openclaw/src/commands/agent/session.ts`
- `reference/openclaw/src/channels/registry.ts`
- `reference/openclaw/src/agents/subagent-spawn.ts`
- `reference/openclaw/src/agents/subagent-registry.ts`
- `reference/openclaw/src/auto-reply/reply/commands-subagents.ts`
- `reference/openclaw/src/process/lanes.ts`
