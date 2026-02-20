# Automaton 분석 문서

## 1. 프로젝트 요약

`automaton`은 "생존 가능한 자율 에이전트"를 핵심으로 하는 런타임이다.  
핵심 컨셉은 다음 3가지다.

- 에이전트가 자체 크레딧 잔고를 기반으로 생존 상태를 전환한다.
- 자기수정(self-modification)과 자기복제(replication)를 런타임 기능으로 내장한다.
- 불변 헌법(`constitution.md`)을 최상위 안전 제약으로 강제한다.

---

## 2. 핵심 설계 철학

문서 기준 프로젝트의 가장 중요한 철학은 아래 순서다.

1. "Never harm"를 최상위로 둔 안전 규범
2. 수익/가치 창출 기반 생존 압력
3. 인간 운영자 없이도 지속 가능한 에이전트 시스템

단순 챗봇이 아니라, "자원 고갈 시 종료되는 자율 시스템"으로 설계된 점이 차별점이다.

---

## 3. 아키텍처 구성

## 3.1 런타임 엔트리

- `src/index.ts`가 CLI 엔트리이자 런타임 오케스트레이터다.
- `--run`, `--setup`, `--status`, `--provision` 등의 모드를 제공한다.
- 실행 시 구성 로드 -> 지갑/아이덴티티 준비 -> DB 초기화 -> heartbeat 시작 -> agent loop 진입 흐름으로 동작한다.

## 3.2 에이전트 실행 루프

- `src/agent/loop.ts`는 ReAct 패턴(Think -> Act -> Observe -> Persist)의 핵심 구현이다.
- 상태 머신(`waking`, `running`, `low_compute`, `critical`, `dead`)을 크레딧 기반으로 전환한다.
- 툴 호출 수 상한, 연속 오류 상한, sleep 상태 전환을 내장하여 무한 루프 위험을 낮춘다.

## 3.3 생존 제어

- 잔고 기반 survival tier를 매 턴/heartbeat에서 평가한다.
- 저비용 모드에서는 추론 모드와 heartbeat 작업을 축소한다.
- `dead` 상태에서는 런타임이 종료 수순으로 들어간다.

## 3.4 Heartbeat 데몬

- `src/heartbeat/daemon.ts`가 에이전트 루프와 별도로 주기 작업을 수행한다.
- cron 표현식으로 due 여부를 계산하고, 저자원 상태에서는 "필수 작업"만 유지한다.
- sleep 중에도 heartbeat는 유지되는 구조라, 이벤트 기반 재기동 트리거를 만들기 쉽다.

## 3.5 자기수정 엔진

- `src/self-mod/code.ts`에서 코드 수정을 툴로 허용하되 강한 안전장치를 둔다.
- 보호 파일 목록, 차단 디렉터리 패턴, 경로 정규화/심볼릭 링크 검증, 수정 rate limit, 파일 크기 상한을 사용한다.
- 수정 전 git 스냅샷 + 감사 로그 기록으로 추적 가능성을 확보한다.

## 3.6 자기복제 엔진

- `src/replication/spawn.ts`, `src/replication/genesis.ts`에서 자식 에이전트 생성/기동/메시징을 담당한다.
- 자식 수 상한, 별도 샌드박스 생성, 헌법 파일 전파(읽기 전용), 계보(lineage) 기록을 수행한다.

## 3.7 스킬 시스템

- `src/skills/loader.ts`가 `SKILL.md` 기반 스킬을 로딩한다.
- binary/env requirement 검증 후 DB와 동기화하며, 활성 스킬 지시문을 시스템 프롬프트에 주입한다.

---

## 4. 런타임 동작 흐름 (요약)

1. 실행/설정/프로비저닝
2. 지갑/정체성 확보
3. DB + heartbeat 준비
4. wakeup prompt 생성 후 agent loop 시작
5. 추론 -> 툴 실행 -> 턴 영속화 반복
6. 잔고 감소 시 low_compute/critical 전환
7. 필요 시 self-mod 또는 child spawn 수행
8. sleep/dead 상태 전환

---

## 5. 멀티에이전트 관점 핵심 인사이트

- 멀티에이전트를 "협업"보다 "생존 계보(lineage)" 관점으로 모델링한다.
- 부모-자식 관계를 데이터 모델로 유지하고, 자식을 별도 샌드박스로 생성한다.
- 시스템 규범(헌법)을 복제 경로에 강제 전파한다.
- 개별 에이전트의 경제 상태(credits)를 독립적으로 다루는 설계가 명확하다.

---

## 6. 장점

- 생존 상태 머신이 명확하고 운영 지표(credits)와 직접 연결된다.
- self-mod 안전장치가 코드 레벨에서 구체적이다.
- 복제 기능이 런타임 1급 기능으로 제공된다.
- heartbeat/agent loop 분리로 운영 유연성이 높다.

---

## 7. 리스크 및 한계

- Conway 인프라 의존성이 높아 이식성이 낮다.
- 강력한 기능(수정/복제) 특성상 운영 정책이 약하면 위험 반경이 커진다.
- 경제 모델 기반 생존은 개인용 어시스턴트 UX와 충돌할 수 있다.
- self-mod 허용 범위를 잘못 설계하면 복구 복잡도가 급증한다.

---

## 8. rust-claw 적용 포인트

아래는 도입 권장 순서다.

1. 상태 머신부터 도입  
`normal -> constrained -> paused` 같은 단순 상태를 먼저 도입하고, 추후 경제/리소스 지표를 연결한다.

2. self-mod는 "완전 금지 기본값"으로 시작  
초기에는 코드 자동수정 툴을 비활성화하고, 감사 로그/스냅샷 파이프만 먼저 만든다.

3. 복제보다 "작업자(worker) 세션 분리"를 우선  
별도 프로세스/세션 기반 worker 패턴으로 시작하고, 독립 지갑/독립 생존 모델은 2단계로 미룬다.

4. 헌법/정책 문서를 런타임 불변 파일로 관리  
정책 파일 immutable 처리와 배포/전파 규칙을 먼저 고정해야 한다.

---

## 9. 분석 근거 파일

- `reference/automaton/README.md`
- `reference/automaton/constitution.md`
- `reference/automaton/src/index.ts`
- `reference/automaton/src/agent/loop.ts`
- `reference/automaton/src/heartbeat/daemon.ts`
- `reference/automaton/src/self-mod/code.ts`
- `reference/automaton/src/replication/genesis.ts`
- `reference/automaton/src/replication/spawn.ts`
- `reference/automaton/src/skills/loader.ts`
