<p align="center">
  <img src="icon-rounded.png" alt="Claude Code Discord Controller" width="120">
</p>

# Claude Code Discord Controller

[![CI](https://github.com/chadingTV/claudecode-discord/actions/workflows/ci.yml/badge.svg)](https://github.com/chadingTV/claudecode-discord/actions)

폰에서 Claude Code를 제어하세요 — Discord를 통한 멀티머신 에이전트 허브.
**API 키 불필요 — 기존 Claude Pro 또는 Max 구독으로 동작합니다.**

<p align="center">
  <img src="demo.gif" alt="데모 — 프로젝트 등록 후 Discord에서 코딩" width="300">
</p>

## 왜 이 봇인가? — 공식 Remote Control과의 차이

Anthropic의 [Remote Control](https://code.claude.com/docs/en/remote-control)은 실행 중인 로컬 세션을 폰에서 이어보는 기능입니다. 이 봇은 그 이상 — 데몬으로 상주하며, 새 세션을 즉시 생성하고, 여러 PC를 하나의 Discord 서버에서 통합 관리하는 **멀티머신 에이전트 허브**입니다.

|                              | 이 봇 | 공식 Remote |
|------------------------------|:------:|:-----------:|
| 모바일에서 새 세션 시작        | ✅     | ❌          |
| 데몬 (터미널 닫아도 유지)      | ✅     | ❌          |
| 멀티머신 통합 관리            | ✅     | ❌          |
| 머신당 동시 세션              | ✅     | ❌          |
| 푸시 알림                    | ✅     | ❌          |
| 팀 협업                      | ✅     | ❌          |
| 네이티브 트레이 앱 (3 OS)     | ✅     | ❌          |
| 오픈 포트 없음               | ✅     | ✅          |

### 멀티 PC 허브

PC별로 Discord 봇을 생성하고, 같은 서버에 초대해서 채널을 배정하면 됩니다:

```
내 Discord 서버
├── #회사맥-프론트엔드     ← 회사 Mac의 봇
├── #회사맥-백엔드        ← 회사 Mac의 봇
├── #집PC-사이드프로젝트   ← 집 PC의 봇
├── #클라우드서버-인프라   ← 클라우드 서버의 봇
```

**폰 하나로 모든 머신의 Claude Code를 제어.** 채널 목록 자체가 전체 머신/프로젝트의 실시간 상태 대시보드가 됩니다.

## 왜 Discord인가?

Discord는 단순한 채팅 앱이 아니라, AI 에이전트 제어에 놀라울 정도로 잘 맞는 플랫폼입니다:

- **이미 폰에 깔려 있습니다.** 새 앱 설치도, 웹 UI 북마크도 필요 없습니다. Discord 열면 바로 시작.
- **푸시 알림이 공짜.** Claude가 승인을 기다리거나 작업을 마치면 즉시 알림 — 화면이 꺼져 있어도.
- **채널 = 워크스페이스.** 각 채널이 프로젝트 디렉토리에 매핑됩니다. 사이드바가 곧 실시간 프로젝트 대시보드.
- **풍부한 UI를 그대로 사용.** 버튼, 셀렉트 메뉴, 임베드, 파일 업로드 — Discord가 인터랙티브 컴포넌트를 제공하므로 별도 프론트엔드가 필요 없습니다.
- **팀 협업이 기본.** 서버에 팀원을 초대하면 Claude 작업을 함께 관찰하고, 도구 호출을 승인하고, 작업을 큐에 넣을 수 있습니다.
- **크로스 플랫폼.** Windows, macOS, Linux, iOS, Android, 웹 브라우저 — Discord는 어디서든 실행됩니다.

## 주요 기능

- 💰 **API 키 불필요** — Claude Pro 또는 Max 구독으로 Claude Code CLI 위에서 동작
- 📱 Discord에서 Claude Code 원격 제어 (데스크톱/웹/모바일)
- 🔀 채널별 독립 세션 (프로젝트 디렉토리 매핑)
- ✅ tool use 승인/거절 Discord 버튼 UI
- ❓ 질문 인터랙티브 UI (선택지 버튼 + 직접 입력)
- ⏹️ 진행 중 Stop 버튼으로 즉시 중지
- 📎 이미지, 문서, 코드 등 파일 첨부 지원
- 🔄 세션 재개/삭제/새로 만들기 (봇 재시작 후에도 유지, 마지막 대화 미리보기)
- 📨 작업 중 메시지 큐 (현재 작업 완료 후 자동 처리)
- ⏱️ 실시간 진행 상황 표시 (도구 사용 현황, 경과 시간)
- 🔒 유저 화이트리스트, 레이트리밋, 경로 보안, 중복 실행 방지
- 📊 **Claude Code 사용량 대시보드** — 컨트롤 패널에서 세션(5시간), 주간(7일), 주간 Sonnet 사용량을 프로그레스 바로 표시, 5분마다 자동 갱신, 클릭 시 사용량 페이지 열기

## 기술 스택

| 분류 | 기술 |
|------|------|
| Runtime | Node.js 20+, TypeScript |
| Discord | discord.js v14 |
| AI | @anthropic-ai/claude-agent-sdk |
| DB | better-sqlite3 (SQLite) |
| 검증 | zod v4 |
| 빌드 | tsup (ESM) |
| 테스트 | vitest |

## 설치

```bash
git clone https://github.com/chadingTV/claudecode-discord.git
cd claudecode-discord

# macOS / Linux
./install.sh

# Windows
./install.bat
```

### 셋업 가이드

| 플랫폼 | 가이드 |
|--------|--------|
| **macOS / Linux** | **[SETUP.kr.md](SETUP.kr.md)** — 터미널 기반 설정, 메뉴바 / 트레이 앱 |
| **Windows**    | **[SETUP-WINDOWS.kr.md](SETUP-WINDOWS.kr.md)** — GUI 설치, 시스템 트레이 + 컨트롤 패널, 바탕화면 바로가기 |

Windows 사용자: `install.bat` 하나로 모든 것이 자동 처리됩니다 — 의존성 설치, 빌드, 바탕화면 바로가기 생성, 시스템 트레이 GUI와 함께 봇 실행.

<details>
<summary><strong>프로젝트 구조</strong></summary>

```
claudecode-discord/
├── install.sh / install.bat    # 자동 설치 스크립트
├── mac-start.sh                # macOS 백그라운드 실행 + 메뉴바
├── linux-start.sh              # Linux 백그라운드 실행 + 시스템 트레이
├── win-start.bat               # Windows 백그라운드 실행 + 시스템 트레이
├── menubar/                    # macOS 메뉴바 앱 (Swift)
├── tray/                       # 시스템 트레이 앱 (Linux: Python, Windows: C#)
├── src/
│   ├── index.ts                # 엔트리포인트
│   ├── bot/
│   │   ├── client.ts           # Discord 봇 초기화 & 이벤트
│   │   ├── commands/           # 슬래시 명령어 (10개)
│   │   └── handlers/           # 메시지 & 인터랙션 핸들러
│   ├── claude/
│   │   ├── session-manager.ts  # 세션 생명주기 관리
│   │   └── output-formatter.ts # Discord 출력 포맷
│   ├── db/                     # SQLite (better-sqlite3)
│   ├── security/               # 인증, rate limit, 경로 검증
│   └── utils/                  # 설정 (zod)
├── SETUP.md                    # macOS/Linux 셋업 가이드
├── docs/                       # 번역, 스크린샷
└── package.json
```

</details>

## 사용법

| 명령어 | 설명 | 예시 |
|--------|------|------|
| `/register <폴더명>` | 현재 채널에 프로젝트 연결 | `/register my-project` |
| `/unregister` | 채널 등록 해제 | |
| `/status` | 전체 세션 상태 확인 | |
| `/stop` | 현재 채널 세션 중지 | |
| `/auto-approve on\|off` | 자동 승인 토글 | `/auto-approve on` |
| `/sessions` | 기존 세션 목록 조회, 재개 또는 삭제 | |
| `/last` | 현재 세션의 마지막 Claude 응답 전체 확인 | |
| `/usage` | Claude Code 사용량 확인 (세션 5시간 / 주간 / 소네트) | |
| `/queue list` | 대기 중인 메시지 목록 확인 (개별 또는 전체 취소) | |
| `/queue clear` | 대기 중인 메시지 모두 취소 | |
| `/clear-sessions` | 해당 프로젝트의 모든 세션 일괄 삭제 | |

`/register` 명령어는 `BASE_PROJECT_DIR` 하위 폴더를 **자동완성 드롭다운**으로 표시합니다 — 타이핑하면 필터링되어 선택할 수 있습니다.
첫 번째 옵션 `.`은 베이스 디렉토리 자체를 등록합니다. 직접 경로를 입력해도 되며, 절대 경로도 사용 가능합니다.

> **왜 디렉토리별로 등록하나요?** Claude Code는 프로젝트 디렉토리 단위로 세션을 관리합니다 — 각 디렉토리마다 독립된 대화 기록, `CLAUDE.md` 컨텍스트, 도구 권한이 적용됩니다. Discord 채널 하나를 디렉토리 하나에 매핑하면, 각 채널이 독립적인 Claude 작업 공간이 됩니다.

등록된 채널에 **일반 메시지**를 보내면 Claude가 응답합니다.
이미지, 문서, 코드 파일을 첨부하면 Claude가 읽고 분석할 수 있습니다.

### 진행 중 제어

- 작업 진행 중 **⏹️ Stop** 버튼으로 즉시 중지 가능
- 이전 작업 진행 중 새 메시지를 보내면 **메시지 큐**에 추가 가능 — 현재 작업 완료 후 자동 처리
- `/queue list`로 대기 메시지 확인 — 개별 취소(❌) 또는 전체 취소 가능
- `/stop` 슬래시 명령어로도 중지 가능

<details>
<summary><strong>아키텍처</strong></summary>

```
[모바일 Discord] ←→ [Discord Bot] ←→ [Session Manager] ←→ [Claude Agent SDK]
                          ↕
                     [SQLite DB]
```

- 채널별 독립 세션 (프로젝트 디렉토리 매핑)
- Claude Agent SDK가 Claude Code를 subprocess로 실행 (기존 인증 공유)
- tool use 승인은 Discord 버튼으로 처리 (자동승인 모드 지원)
- 스트리밍 응답을 1.5초 간격으로 Discord 메시지 edit
- 텍스트 출력 전까지 15초마다 heartbeat로 진행 상황 표시
- 마크다운 코드 블록이 메시지 분할 시에도 보존됨

**세션 상태:** 🟢 작업 중 · 🟡 승인 대기 · ⚪ 대기 · 🔴 오프라인

</details>

## 보안

### 외부 공격면 제로

이 봇은 **HTTP 서버, 포트, API 엔드포인트를 일절 열지 않습니다.** 봇이 Discord 서버로 아웃바운드 WebSocket 연결을 거는 구조이기 때문에, 외부에서 이 봇에 직접 접근할 수 있는 경로 자체가 존재하지 않습니다.

```
일반 웹서버:  외부 → [포트 열고 대기] → 요청 받음    (인바운드)
이 봇:        봇 → [Discord 서버로 접속] → 이벤트 수신  (아웃바운드만)
```

### 셀프 호스팅 구조

봇은 본인의 PC/서버에서 직접 실행됩니다. 외부 서버를 거치지 않으며, Discord와 Anthropic API(본인의 Claude Code 로그인 세션 사용)를 통한 통신 외에 데이터가 외부로 나가지 않습니다.

### 접근 제어

- `ALLOWED_USER_IDS` 화이트리스트 기반 인증 — 등록되지 않은 사용자의 모든 메시지와 명령어 무시
- Discord 서버는 기본적으로 비공개 (초대 링크 없이 접근 불가)
- 분당 요청 수 제한 (rate limit)

### 실행 보호

- tool use 기본값: 파일 수정, 명령어 실행 등은 **매번 사용자 승인 필요** (Discord 버튼)
- 프로젝트 경로 `..` 순회 차단
- 파일 첨부: 실행 파일(.exe, .bat 등) 차단, 25MB 크기 제한

### 주의사항

- `.env` 파일에 봇 토큰이 포함되어 있으므로 **절대 외부에 공유하지 마세요**. 유출 시 Discord Developer Portal에서 즉시 Reset Token
- `auto-approve` 모드는 편리하지만, Claude가 의도치 않은 작업을 수행할 수 있으므로 신뢰하는 프로젝트에서만 사용을 권장합니다

## 플랫폼별 간편 실행

각 플랫폼에서 봇을 백그라운드 서비스로 실행하고, 네이티브 GUI로 관리할 수 있습니다 — 터미널을 띄워놓을 필요 없습니다.

### macOS — 메뉴바 앱

<p align="center">
  <img src="mac-tray-kr.png" alt="macOS 컨트롤 패널" width="400">
</p>

```bash
./mac-start.sh          # 시작 (백그라운드 + 메뉴바 아이콘)
./mac-start.sh --stop   # 중지
```

컨트롤 패널 GUI (아이콘 클릭), **Claude Code 사용량 대시보드** (세션 5시간 / 주간 / Sonnet, 클릭 시 사용량 페이지 열기), 설정 다이얼로그, 자동 업데이트, 크래시 자동 재시작, 부팅 시 자동 실행 (launchd). → **[전체 가이드](SETUP.kr.md)**

### Linux — 시스템 트레이 + 컨트롤 패널

<p align="center">
  <img src="linux-tray-kr.png" alt="Linux 시스템 트레이" width="350">
</p>

```bash
./linux-start.sh          # 시작 (systemd + 트레이 아이콘)
./linux-start.sh --stop   # 중지
```

GTK3 **컨트롤 패널** (트레이 아이콘 클릭), **Claude Code 사용량 대시보드**, 설정 다이얼로그, 자동 재시작, 부팅 시 자동 실행 (systemd). 헤드리스 서버에서도 동작. → **[전체 가이드](SETUP.kr.md)**

### Windows — 시스템 트레이 + 컨트롤 패널

<p align="center">
  <img src="windows-tray-kr.png" alt="Windows 컨트롤 패널" width="400">
</p>

```batch
win-start.bat          &:: 시작 (백그라운드 + 트레이 + 컨트롤 패널)
win-start.bat --stop   &:: 중지
```

바탕화면 바로가기, 컨트롤 패널 GUI, **Claude Code 사용량 대시보드**, 설정 다이얼로그, 자동 업데이트, 로그온 시 자동 시작 (Registry). → **[전체 가이드](SETUP-WINDOWS.kr.md)**

## 개발 명령어

```bash
npm run dev          # 개발 실행 (tsx)
npm run build        # 프로덕션 빌드 (tsup)
npm start            # 빌드된 파일 실행
npm test             # 테스트 (vitest)
npm run test:watch   # 테스트 watch 모드
```

## 라이선스

[MIT License](../LICENSE) - 자유롭게 사용, 수정, 상업적 이용 가능. 배포 시 원본 저작권 표시 및 [출처](https://github.com/chadingTV/claudecode-discord) 명시 필요.

---

이 프로젝트가 유용하셨다면 ⭐ 를 눌러주세요 — 더 많은 사람들이 발견할 수 있게 됩니다!
