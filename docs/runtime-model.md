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

- `cp`를 스트림 inherit로 실행. `-v` 주입·캡처·footer 없음.
- 출력이 `cp`와 바이트 동일.

## 요약 규칙 (managed일 때만)

**요약은 진행바가 한 번이라도 떴을 때만** 낸다(‑ 실제로 감시할 복사가 있었을 때). `--help`·
`--version`이나 즉시 끝난 실행처럼 **footer가 안 떴으면 요약도 없다**(‑ cp가 이미 자기 출력으로
말했고, cprog가 감시한 게 없으니 덧붙일 게 없다). 그 위에서:

- `cp` exit 0  → `✓ done - T elapsed`를 **stderr**로
- `cp` exit n≠0 → `✗ cp exited n - T elapsed`를 **stderr**로(중립 문구 — "failed" 아님)
- `cp` 시그널 종료 → 요약 **없음**(시그널 의미론 보존)

전부 stderr로 보내 stdout은 `cp` 몫으로 남긴다. 개수/총량을 안 세므로 요약은 최소한
(경과시간 위주, 단일 파일이면 그 파일 크기 정도).

## 종료 동작

- 정상: `cp`의 exit code 그대로 반환.
- 시그널(Ctrl-C 등): `cprog`에 같은 시그널을 다시 걸어 부모 셸이 올바른 `$?`/signaled 상태를
  보게 함(SIGINT/TERM/HUP/QUIT), 불가하면 `128 + n`.
- `cprog` 쪽 문제(캡처·렌더·정리)는 **경고일 뿐**, `cp`가 낸 코드를 절대 바꾸지 않음.
