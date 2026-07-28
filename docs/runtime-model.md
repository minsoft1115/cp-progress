# Runtime Model (실행 정책)

`cprog`는 `cp`를 시작하기 **전에 한 번** managed-TUI인지 passthrough인지 정한다. 이 결정은
검사한 인자와 감지한 런타임 capabilities의 순수 함수다.

## RunMode

`decide(caps, interactive)`는 순수 함수로 모드 하나만 고른다:

```
RunMode = ManagedTui | Passthrough
```

`summary`는 계획에 담긴 값이 아니라 **런타임 산물**이다 — managed 실행 중 footer가 한 번이라도
떴는지(`progress_shown`)로 종료 시점에 정한다(아래 "요약 규칙"). 런타임 상태(캡처 핸들·크기)도
나중에 붙는다. 모드는 확정 후 바뀌지 않는다.

## Managed TUI 선택 조건 (전부 참일 때만)

- interactive 플래그 없음(`-i`, `--interactive`, `--interactive=…`).
- stdout이 TTY.
- stderr가 TTY.
- stdout과 stderr가 **같은** 터미널.
- `TERM`이 설정돼 있고 `dumb`이 아님.
- CI 아님(`CI` 미설정).
- 플랫폼이 리눅스(`/proc` 읽기 가능).
- **`stdbuf`가 실행 가능** — `-v`를 실시간으로 흘리려면 `stdbuf -oL`이 필요하기 때문
  ([`capture-and-verbose.md`](./capture-and-verbose.md)). `cp` 버전이 아니라 `stdbuf` 가용성을
  feature-detect한다(‑ 실질 바닥은 coreutils 7.5). 없으면 managed를 포기한다.
- **전경(foreground) 프로세스 그룹임** — `tcgetpgrp(stdout) == getpgrp()`. 백그라운드 작업
  (`cprog … &`)은 터미널을 점거하면 안 되므로 managed를 포기하고 passthrough로 동작한다.
  (‑ `tcgetpgrp`이 `ENOTTY`면 제어터미널이 아니어서 백그라운드 판정 불가 → 관대하게 허용.)
- **`CPROG_PASSTHROUGH` 미설정** — 설정돼 있으면(값 무관 — `CI`·`NO_COLOR`와 같은 규칙)
  다른 조건과 무관하게 passthrough다. "완전히 비켜라"는 명시적 스위치로, 디버깅과 방어적
  스크립트용이다([`usage.md`](./usage.md) "강제 passthrough").

하나라도 실패하면 **Passthrough**. 인자 검사 자체가 실패해도 보수적으로 passthrough.

## Managed TUI

- `cp`를 **`stdbuf -oL` + `-v` 주입**해서 실행, stdout/stderr **캡처**, sole-writer로 로그
  영역에 중계 ([`capture-and-verbose.md`](./capture-and-verbose.md)).
- 터미널 리사이즈는 **SIGWINCH 이벤트 + 저빈도 폴백 재조회** 하이브리드로 처리한다
  (순수 SIGWINCH만 쓰면 시그널이 유실·합쳐질 때 크기가 낡은 채 고정될 수 있어 폴백을 둔다).
- 느린 파일 감지 → `/proc`+`stat` per-file 바를 footer에 그림
  ([`progress-model.md`](./progress-model.md)).
- 종료 시 요약 1줄(stderr), 시그널이면 요약 없음.
- **파일 개수는 안 셈**(대량 소파일 성능 회피).

## Passthrough

- 원칙적으로 `cp`로 **exec한다(프로세스 대체)** — cprog가 소멸하고 같은 PID가 `cp`가 되므로,
  exit code·시그널·job control이 아무 중계 없이 셸에 그대로 노출된다(`cp … &`의 `$!`도 진짜
  `cp`의 PID다). exec 직전에 SIGPIPE를 기본 disposition으로 되돌린다 — Rust 런타임이 ignore로
  바꿔둔 것이 exec를 넘어 상속되면 `cp -v … | head` 같은 파이프라인에서 순정과 달라진다.
  exec 실패(예: `cp` 없음)만 `Fatal::CpSpawn`(exit 127)으로 돌아온다.
- 유일한 예외는 **버전 한 줄을 붙여야 할 때**(`--help`/`--version` + stderr TTY + 강제 아님,
  아래 "버전 표시"): cp가 끝난 뒤 덧붙일 프로세스가 남아 있어야 하므로 spawn(inherit) → wait로
  실행한다.
- 어느 쪽이든 `-v` 주입·캡처·footer 없음. 출력이 `cp`와 바이트 동일.

## 요약 규칙 (managed일 때만)

**요약은 진행바가 한 번이라도 떴을 때만** 낸다(‑ 실제로 감시할 복사가 있었을 때). `--help`·
`--version`이나 즉시 끝난 실행처럼 **footer가 안 떴으면 요약도 없다**(‑ cp가 이미 자기 출력으로
말했고, cprog가 감시한 게 없으니 덧붙일 게 없다). 그 위에서:

- `cp` exit 0  → `✓ done - T elapsed`를 **stderr**로
- `cp` exit n≠0 → `✗ cp exited n - T elapsed`를 **stderr**로(중립 문구 — "failed" 아님)
- `cp` 시그널 종료 → 요약 **없음**(시그널 의미론 보존)

전부 stderr로 보내 stdout은 `cp` 몫으로 남긴다. 개수/총량을 안 세므로 요약은 언제나 위
형태 그대로다(‑ 경과시간뿐).

## 버전 표시 (`--help` / `--version`)

`--help`·`--version`은 복사를 하지 않으므로 **passthrough**로 내려간다(위 참조). 그런데 그러면
`cp`의 출력만 보여서 **사용자가 cprog의 버전을 알 방법이 없다** — `cprog --version`조차
`cp (GNU coreutils) 9.x`만 낸다.

그래서 informational 인자일 때만, `cp`가 끝난 뒤 **빈 줄 하나와 한 줄**을 덧붙인다:

```
Written by Torbjörn Granlund, David MacKenzie, and Jim Meyering.
                                                            ← 빈 줄
cprog 0.4.0 — https://github.com/minsoft1115/cp-progress    ← dim
```

빈 줄과 dim은 이 줄이 **`cp` 출력의 일부가 아님**을 알리기 위한 것이다. 둘 다 없으면 cp의 마지막
문단에 붙어 읽힌다. **가로줄은 쓰지 않는다** — 터미널 폭을 알아야 하는데 이 경로는 크기를 조회하지
않고, cprog는 다른 어디에서도 장식을 그리지 않는다.

dim은 요약이 쓰는 것과 **같은 `Style.color`** 를 따르므로 `NO_COLOR`·`TERM=dumb`에서 자동으로
꺼진다(‑ 그때는 평문 한 줄).

조건은 요약 규칙과 동일하다:

- **stdout이 아니라 stderr로.** stdout은 `cp` 몫이다. 스크립트가 `cp --version | tail -1`처럼
  파싱하는 경우를 깨지 않는다.
- **stderr가 TTY일 때만.** 리다이렉트·파이프·CI에서는 아무것도 안 붙어 **`cp`와 바이트 동일**이
  유지된다. "추가 UI는 대화형 터미널에서만"이라는 managed/passthrough 분기와 같은 원칙이다.

```bash
cp --version                 # 터미널: cp 출력 + cprog 한 줄
cp --version | head -1       # 파이프: stdout 그대로
cp --version 2>/dev/null     # 스크립트: 조용
```

> **자체 플래그는 두지 않는다.** `cprog --version` 같은 걸 가로채면 `cp`로 인자를 그대로 넘긴다는
> 전제가 깨진다. `--version`은 언제나 `cp`에 도달해야 한다.

**`CPROG_PASSTHROUGH`가 설정돼 있으면 이 한 줄도 억제된다.** 강제 passthrough는 "cprog가
덧붙이는 모든 것을 꺼라"는 명시적 의사이고, 이때 passthrough는 exec라 cp가 끝난 뒤 덧붙일
프로세스 자체가 없다.

## 종료 동작

- 정상: `cp`의 exit code 그대로 반환.
- 시그널(Ctrl-C 등): `cprog`에 같은 시그널을 다시 걸어 부모 셸이 올바른 `$?`/signaled 상태를
  보게 함(SIGINT/TERM/HUP/QUIT), 불가하면 `128 + n`(폴백이 닿는 범위는
  [exceptions A1a](./exceptions.md)).
- `cprog` 쪽 문제(캡처·렌더·정리)는 **경고일 뿐**, `cp`가 낸 코드를 절대 바꾸지 않음.
