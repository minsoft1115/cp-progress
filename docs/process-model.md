# Process Model (프로세스 모델)

`cprog`는 자식 프로세스 하나 — 시스템 `cp` — 를 조합하고 밖에서 관찰한다. 데이터를 직접
복사하지 않는다.

## 자식 프로세스

- **`cp`** — 진짜 복사 수행.
  - Passthrough: stdin/stdout/stderr **inherit**.
  - Managed: stdin inherit, **stdout/stderr capture(pipe)** + `-v` 주입 + **`stdbuf -oL`로 감쌈**.

`stdbuf`는 `cp`를 `exec`하므로 **PID는 그대로 `cp`가 된다**(‑ `/proc/<pid>/fd` 그대로 유효).
두 번째 자식은 없다(외부 `progress` 없음, hidden PTY 없음). 진행은 `/proc` 읽기로 얻으며
`cp`의 협조가 필요 없다.

## Managed 생명주기

```
1. cp spawn (-v 주입, stdout/stderr capture)   → PID 확보
2. capture 리더 스레드 시작 → 로그 영역 중계 + 줄경계 펄스
3. slow-timer: 마지막 펄스 후 100ms 경과 & cp 생존 → 현재 파일 "느림"
4. 느림이면 sampler 시작: /proc/fd → stat(dst)/stat(src) → ProgressState
5. 렌더 루프(메인 스레드):
     - 느림이면 footer 바 그림 / 아니면 footer 없음
     - 리사이즈: SIGWINCH 플래그가 서거나 저빈도 폴백(예: 1s)이 지나면 크기 재조회+재배치
       (순수 SIGWINCH만 쓰면 시그널이 유실·합쳐질 때 낡은 크기로 고정될 수 있어 폴백을 둔다)
     - 로그 바이트를 통과시키기 전 footer 지움(erase-redraw)
6. cp 종료 대기
7. sampler/리더 정지·join
8. footer 지움(best-effort) ← FooterGuard가 모든 경로에서 보장
9. 요약 출력(시그널이면 생략)
10. cp status로 exit 확정
```

메인 스레드가 터미널의 유일 기록자. sampler·리더는 공유 상태만 발행하고 터미널을 안 건드림.

## Passthrough 생명주기

```
1. cp spawn (inherit)
2. wait
3. cp status로 exit 확정
```

## 샘플링과 종료

- sampler는 **느린 파일일 때만** 대상 크기를 `stat` 폴링(기본 100ms).
- 샘플러는 렌더 루프가 끝난 뒤 세워지는 **`stop` 플래그**로 정지한다. `cp` 종료는 별도
  liveness 폴링이 아니라 `/proc/<pid>/fd`가 비거나 사라져 **틱이 skip되는 것**으로 관측된다.
  `cp`는 샘플러 join **이후에야** `wait()`로 reap되므로, 샘플러가 도는 동안 pid는 예약 상태로
  남아 **재사용될 수 없다**(오염 샘플·pid 재사용 레이스 없음).
- 모든 샘플 실패는 비치명(건너뛰고 마지막 값 유지).
- 리눅스 `PR_SET_PDEATHSIG`로 래퍼가 죽으면 자식이 남지 않게 함(누수될 helper *프로세스*는
  애초에 없다).

## 정리 (Cleanup)

- footer는 **모든 종료 경로** — 정상 반환, `Fatal`, panic, 시그널 — 에서 지워진다.
  `FooterGuard`의 `Drop`이 지우고, 요약 전 명시적 best-effort clear도 한다.
- capture 리더·sampler 스레드는 정지 신호 후 join. join이 느려도 유계이며 exit code를 안 막음.
- 정리 문제는 `Warning`일 뿐 `cp` 결과를 바꾸지 않음.

## 시그널 보존 종료

- `cp` exit code `n` → `cprog`가 `n` 반환.
- `cp`가 시그널 `s`로 죽음 → `cprog`가 기본 핸들러 복원, `s` unblock, 자신에게 `s` 재전송해
  부모 셸이 진짜 signaled 종료를 보게 함. 불가하면 `128 + s`.
- `cprog`가 **단독으로** 시그널 `s`를 받았고(그룹이 아니라 `kill <cprog>`) `cp`가 아직 살아있으면,
  `cprog`는 **그 `s`를 그대로 `cp`에 전달**한다(SIGTERM으로 정규화하지 않음). 그러면 `cp`가 `s`로
  죽고 위 규칙대로 `cprog`도 `s`로 재전송돼, 조작자가 보낸 시그널과 신고 시그널이 일치한다.
- 시그널 시 footer는 지우고 요약은 **안 함**.

## 에러와 경고

치명(중단, non-zero 반환):
- `cp` spawn 실패
- `cp` wait 실패

비치명(경고, 계속, `cp` 결과 보존):
- capture 리더/relay IO 실패
- footer 렌더/erase IO 실패
- `/proc`/`stat` 샘플 실패(→ indeterminate 또는 바 없음)
