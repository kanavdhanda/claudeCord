# 테스트 가이드

## 개요

이 프로젝트는 [Vitest](https://vitest.dev/) v2를 테스트 러너로 사용합니다. 모든 테스트 파일은 소스 파일 옆에 위치합니다 (`*.test.ts`).

## 테스트 실행

```bash
npm test              # 전체 테스트 1회 실행
npm run test:watch    # watch 모드 (파일 변경 시 자동 재실행)
npx tsc --noEmit      # 타입 체크만 수행 (빌드 출력 없음)
```

## 테스트 구조

| 테스트 파일 | 개수 | 대상 모듈 | 전략 |
|---|---|---|---|
| `src/claude/output-formatter.test.ts` | 29 | 메시지 분할, 코드 블록 펜스, Discord embed/버튼 생성 | 모킹 없음 — 순수 로직 + discord.js 생성자 네이티브 동작 |
| `src/security/guard.test.ts` | 16 | 유저 화이트리스트, 슬라이딩 윈도우 레이트 리밋, 경로 순회 차단, BASE_PROJECT_DIR 범위 검증 | `getConfig()` 모킹, `vi.spyOn(fs)`, `vi.useFakeTimers()` |
| `src/utils/config.test.ts` | 8 | Zod 환경변수 검증, 싱글톤 캐싱, 에러 시 `process.exit` | `vi.resetModules()` + 동적 `import()` |
| `src/db/database.test.ts` | 12 | Project/Session CRUD 연산 | `better-sqlite3` 생성자 모킹으로 인메모리 SQLite 사용 |
| `src/bot/commands/sessions.test.ts` | 12 | JSONL 세션 파싱, `findSessionDir`, 비정상 JSON 처리 | 실제 임시 파일 (`fs.mkdtempSync`) + `os.homedir()` 모킹 |
| **합계** | **77** | | |

## 각 테스트 커버리지

### output-formatter (29개)

- **formatStreamChunk**: 1900자 절삭, 빈 문자열 처리
- **splitMessage**: 줄바꿈 기준 분할, 긴 줄 강제 분할, 코드 블록 펜스 보존 (언어 지정 유/무), 여러 코드 블록 처리
- **createToolApprovalEmbed**: 도구 타입별 필드 생성 (Edit, Bash, Write, 일반), 버튼 customId 형식, 콘텐츠 절삭
- **createResultEmbed**: 비용 표시 토글, 소요시간 포맷, 설명 절삭
- **createAskUserQuestionEmbed**: 단일 선택 (버튼), 다중 선택 (StringSelectMenu), 질문 인덱스, 행 분리 (행당 버튼 5개)
- **createStopButton / createCompletedButton**: customId 형식, 비활성 상태

### guard (16개)

- **isAllowedUser**: 화이트리스트 매칭, 대소문자 구분, 빈 문자열 거부
- **checkRateLimit**: 제한 내 요청, 초과 차단, 60초 윈도우 리셋, 유저별 독립 추적
- **validateProjectPath**: 경로 순회(`..`) 차단 (fs 호출 전 검사), BASE_PROJECT_DIR 범위 강제, 미존재 경로, 비디렉토리 경로, 유효 디렉토리

### config (8개)

- `process.env`에서 유효한 Config 파싱
- `ALLOWED_USER_IDS` 콤마+공백 분리
- `RATE_LIMIT_PER_MINUTE` 정수 변환, `SHOW_COST` boolean 변환
- 필수 변수 누락 시 `process.exit(1)` 호출
- 싱글톤 캐싱 (반복 호출 시 같은 참조 반환)

### database (12개)

- Project CRUD: 등록, 조회, 전체 조회 (guild 필터), 해제 (세션 연쇄 삭제), auto-approve 토글
- Session CRUD: upsert, 조회 (채널별 최신), 상태 업데이트, 전체 조회 (projects JOIN)

### sessions (12개)

- **findSessionDir**: `~/.claude/projects` 미존재, 단순 경로 인코딩 매칭, 매칭 실패 시 null
- **getLastAssistantMessage**: 배열/문자열 content, 여러 줄 (마지막 줄 반환), assistant 메시지 없음, 잘못된 JSON 무시, 공백만 있는 텍스트 건너뛰기, 여러 텍스트 블록 결합
- **getLastAssistantMessageFull**: 전체 텍스트 반환, 빈 파일 처리

## 새 테스트 추가하기

1. 소스 파일 옆에 `<모듈명>.test.ts` 생성
2. 소스 import 시 `.js` 확장자 사용 (ESM 컨벤션)
3. 외부 의존성은 `vi.mock()`으로 모킹 — 테스트 대상 모듈 자체는 모킹하지 않기
4. `npm test`로 확인
