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
2. capture 리더 스레드 시작 → 줄경계 펄스(항상) + 로그 영역 중계(‑ stdout은 `-v`를 줬을 때만)
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

원칙은 **exec에 의한 프로세스 대체**다:

```
1. SIGPIPE를 기본 disposition으로 복원
   (Rust 런타임의 ignore가 exec를 넘어가면 `cp -v … | head`에서 순정과 달라진다 — A7)
2. cp로 exec — cprog는 소멸, 같은 PID가 cp가 된다
   (이후의 시그널·exit·job control은 cprog를 거치지 않고, `$!`도 진짜 cp의 PID다.
    exec 실패(cp 없음 등)만 Fatal::CpSpawn → exit 127로 돌아오며, 이때 SIGPIPE ignore를
    되돌린다 — 계속 사는 cprog의 stderr 쓰기가 시그널 사망이 되지 않게. A7 참조)
```

예외 — **버전 한 줄을 붙일 때만**(`--help`/`--version` + stderr TTY + `CPROG_PASSTHROUGH`
아님) cp가 끝난 뒤 덧붙일 프로세스가 남아 있어야 하므로 기존 방식으로 실행한다:

```
1. cp spawn (inherit)
2. wait
3. 버전 한 줄(stderr)
4. cp status로 exit 확정
```

## 샘플링과 종료

- sampler는 **느린 파일일 때만** 대상 크기를 `stat` 폴링(기본 100ms).
- 샘플러는 렌더 루프가 끝난 뒤 세워지는 **`stop` 플래그**로 정지한다. `cp` 종료는 별도
  liveness 폴링이 아니라 `/proc/<pid>/fd`가 비거나 사라져 **틱이 skip되는 것**으로 관측된다.
  `cp`는 샘플러 join **이후에야** `wait()`로 reap되므로, 샘플러가 도는 동안 pid는 예약 상태로
  남아 **재사용될 수 없다**(오염 샘플·pid 재사용 레이스 없음).
- 모든 샘플 실패는 비치명(건너뛰고 마지막 값 유지).
- 리눅스 `PR_SET_PDEATHSIG`로 래퍼가 죽으면 자식이 남지 않게 함(누수될 helper *프로세스*는
  애초에 없다). **spawn 경로에만 건다** — exec된 passthrough에는 지킬 부모(cprog)가 없고,
  설정이 exec를 넘어 남으면 `cp`의 수명이 *셸*에 묶여 순정과 달라진다.

## 정리 (Cleanup)

- footer는 **모든 종료 경로** — 정상 반환, `Fatal`, panic, 시그널 — 에서 지워진다.
  `FooterGuard`의 `Drop`이 지우고, 요약 전 명시적 best-effort clear도 한다.
- **일시정지(Ctrl-Z / `SIGTSTP`)** 도 정리한다: 렌더 루프가 `FooterGuard::suspend_restore`로
  footer를 지우고 커서를 복원한 뒤 `SIGSTOP`으로 실제 정지하고(‑ `SIGTSTP` 플래그 핸들러는 유지),
  재개(`SIGCONT`) 시 다음 tick에서 커서 재숨김 + footer 재그림. `Drop`은 정지가 아니라 종료에서만
  돌기 때문.
- 렌더 루프가 끝나면(정상 종료·시그널·리더 disconnect) **`SIGTSTP` 처리를 기본 동작으로 되돌린다**:
  루프가 사라진 뒤에도 플래그 핸들러가 남아 있으면 teardown(join/wait) 구간의 Ctrl-Z가 정지도
  진행도 못 하고 삼켜지므로(wedge). 기본 disposition(그룹 정지)으로 복원해 그 구간에서도 job
  control이 정상 동작하게 한다.
- **백그라운드 재개(`bg`) 시 footer 억제는 단방향**이다: 재개 시점에 전경이 아니면 footer를
  끄고, 이후 `fg`로 전경에 복귀해도 다시 켜지 않는다(전경 여부는 suspend-재개 시에만 재확인 —
  "모드는 확정 후 안 바뀐다"와 같은 단순화). 또 한 번 Ctrl-Z 후 `fg` 하면 footer가 복구된다.
- capture 리더·sampler 스레드는 정지 신호 후 join. 리더 join이 끝나려면 `cp`가 죽어 파이프가
  닫혀야 하므로, 시그널을 전달할 때 **`SIGCONT`를 함께 보낸다** — 정지된 `cp`는 시그널을 pending으로
  쌓아둘 뿐 죽지 않아 join이 영원히 안 끝나기 때문이다.
  - ⚠️ `cp`가 **uninterruptible(D) 상태**(예: 끊긴 네트워크 마운트)면 어떤 시그널도 전달되지 않아
    `cp`가 죽지 않는다. 이때는 join도 `wait()`도 끝나지 않는데, 이는 `cp`를 직접 실행했을 때와
    **동일한 결과**다(해당 프로세스는 `kill -9`로도 못 죽인다). 여기서 기한을 두고 빠져나가면
    `cp`의 결과 없이 종료 코드를 지어내야 하므로 "cp의 결과가 최종 권위"를 깨뜨린다 — 그래서
    타임아웃을 두지 않는다.
- 정리 문제는 `cp` 결과를 바꾸지 않는다.

## 시그널 보존 종료

- `cp` exit code `n` → `cprog`가 `n` 반환.
- `cp`가 시그널 `s`로 죽음 → `cprog`가 기본 disposition 복원, `s` unblock, 자신에게 `s` 재전송해
  부모 셸이 진짜 signaled 종료를 보게 함. 불가하면 `128 + s` — 단 이 폴백이 닿는 건 **실시간
  시그널이 블록돼 있을 때뿐**이고, 표준 시그널의 재전송 실패는 SIGABRT로 끝난다
  ([exceptions A1a](./exceptions.md)).
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
