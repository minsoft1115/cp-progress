# Exceptions (예외 상황 카탈로그)

`cprog`가 만날 수 있는 예외 상황을 **한 곳에** 모았다. 각 항목은 "무슨 일이 생기나 / 지금
어떻게 동작하나 / 어디에 구현·테스트돼 있나"를 적는다.

[`testing.md`](./testing.md)의 예외 매트릭스가 **"무엇을 테스트할 것인가"**(TDD 명세)라면,
이 문서는 **"실행 중 무슨 일이 벌어지는가"**(런타임 동작 카탈로그)다. 겹치는 항목은 서로
참조한다.

## 상태 표기

| 기호 | 뜻 |
|---|---|
| ✅ | 구현됨 + 자동 테스트 있음 |
| 🟡 | 구현됨, 자동 테스트 없음(수동 확인/자명) |
| 📄 | **의도적 비대응** — 설계상 한계로 받아들이고 문서화한 것 |
| 🔶 | **미커버 갭** — 이 문서를 쓰며 새로 식별 |

> **2026-07 갱신:** 아래 🔶로 식별한 항목은 이슈 **#3–#13**으로 등록해 **전부 처리됐다.**
> 각 행의 "현재 동작" 열은 **고친 뒤의 동작**을 적고, 원래 문제는 *이전에는* 절로 남겼다.
> 무엇을 어떻게 고쳤는지는 문서 끝 [해결 기록](#해결-기록-r1r7)에 정리했다.
>
> 마지막까지 남았던 **E22**는 고칠 결함이라기보다 "이 분기를 계속 둘 것인가"라는 설계 판단이라
> [#12](https://github.com/minsoft1115/cp-progress/issues/12)에서 선택지와 함께 다뤘고,
> **분기 제거**로 결론지었다.

🔶 항목에는 **검증 수준**을 함께 적는다:

- **[실측]** — 이 저장소의 바이너리로 재현해 관찰함(재현 절차는 각 권고에 기록).
- **[코드]** — 코드를 읽어 도출했으나 실행으로 재현하지는 않음.

## 모든 예외를 관통하는 두 불변식

이 문서의 모든 항목은 결국 이 둘로 환원된다:

1. **`cp`의 결과가 최종 권위.** cprog 쪽 실패(캡처·렌더·샘플·정리)는 절대 `cp`의 exit
   code/시그널을 바꾸지 않는다. 코드로는 무음 best-effort(`let _ = …`)로 표현된다.
2. **터미널은 어떤 경로로 끝나도 복구된다.** 정상 종료·`Fatal`·panic·시그널·Ctrl-Z —
   `FooterGuard::Drop`(및 `suspend_restore`)이 footer를 지우고 커서를 되살린다.
   **단 하나의 예외는 SIGKILL/SIGSEGV**(→ [F7](#f7)).

---

# A. 시그널 (Signals)

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| A1 | **`cp`가 시그널 `s`로 죽음** (Ctrl-C가 전경 그룹 전체에 전달된 경우 포함) | footer 지움 → **요약 없음** → 기본 핸들러 복원 + `s` unblock + 자기 자신에게 `s` 재전송 → 셸이 진짜 signaled 종료를 봄. 재전송이 실패하면 `128 + s` 반환 | ✅ `exit::finalize`/`reraise`, `tests/signals.rs::sigint_during_managed_copy_cleans_footer_and_preserves_signal` |
| A2 | **`cprog`만 단독으로 시그널 받음** (`kill <cprog-pid>`) | 렌더 루프가 즉시 break → **받은 시그널을 그대로 `cp`에 전달**(SIGTERM으로 정규화하지 않음) → cp가 그 시그널로 죽고 파이프가 닫혀 join이 유계 → A1 규칙으로 cprog도 같은 시그널로 종료 | ✅ `lib.rs::run_managed`(`received_signal`), `tests/signals.rs::signal_to_cprog_alone_is_forwarded_to_cp_and_re_raised` |
| A3 | **시그널 도착 시 `cp`가 이미 종료** | `try_wait`이 `Some(_)`이면 전달 생략. `try_wait` 자체가 에러면 "살아있을 수도"로 보고 방어적으로 `kill` — 이미 죽은 pid엔 `ESRCH`라 무해 | ✅ `lib.rs::run_managed` |
| A4 | **잡는 시그널의 범위** | `SIGINT`/`SIGTERM`/`SIGHUP`/`SIGQUIT` 4종만 등록. 그 외(`SIGUSR1` 등)는 기본 동작 | 🟡 `lib.rs::run_managed` |
| A5 | **Ctrl-C를 두 번 이상** | 첫 번째로 렌더 루프가 break. 이후 teardown(join/wait) 구간의 추가 시그널은 핸들러가 값만 덮어쓰고 **아무도 읽지 않음** → 즉각 반응 없음. 단 cp에 이미 전달돼 곧 종료하므로 대기는 유계 | 📄 `lib.rs::run_managed` |
| A6 | **passthrough에서 시그널** | 핸들러를 아예 등록하지 않음 → cprog와 cp 모두 기본 동작(전경 그룹 전체가 함께 죽음) → `cp`와 완전 동일 | ✅ 설계상, `tests/passthrough.rs` |
| A7 | **`SIGPIPE`** | Rust 런타임이 부모에서 무시하므로 relay 실패가 `EPIPE` 에러로 표면화(패닉 아님). 자식은 `std::process::Command`가 기본 disposition을 복원해 `cp`가 정상적으로 SIGPIPE에 반응 | 🟡 `tests/exit_contract.rs`(stderr가 broken pipe여도 exit code 유지) |
| A8 | **`cp`가 시그널을 처리할 수 없는 상태에서 cprog가 종료 시그널을 받음** (정지 상태이거나, 끊긴 NFS 등에서 uninterruptible I/O 중) | cp에 시그널을 보낼 때 **`SIGCONT`를 함께** 보내므로, 정지된 cp도 깨어나 시그널을 받고 죽는다 → 파이프가 닫히고 join이 끝난다. *이전에는* 시그널만 보내 정지 상태의 cp가 그대로 남아 join이 무한 대기했다. 다만 uninterruptible(D) 상태는 여전히 못 푼다 — `cp` 단독 실행과 같은 결과이며 [의도적](#a8-note) | ✅ **해결(#5)** — 시그널 전달 시 `SIGCONT` 동반. `lib.rs::run_managed` |

## Ctrl-Z / job control

<a id="a9"></a>

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| A9 | **Ctrl-Z (`SIGTSTP`) — footer가 떠 있을 때** | 렌더 루프가 플래그를 보고 `FooterGuard::suspend_restore()`로 footer 지움 + 커서 복원 → 그 후 `raise(SIGSTOP)`으로 실제 정지(플래그 핸들러는 유지) → 재개 시 다음 tick에서 커서 재숨김 + footer 재그림 | ✅ `lib.rs::run_managed`, `render::suspend_restore`, `tests/suspend.rs::ctrl_z_restores_terminal_before_stop_then_redraws_on_resume` |
| A10 | **Ctrl-Z 후 `bg`** (백그라운드 재개) | 재개 시점에 `tcgetpgrp != getpgrp`이면 `suppressed = true` → 이후 footer를 그리지 않음. **단방향**: 다시 `fg`로 돌아와도 그 실행에서는 꺼진 채 유지(다시 Ctrl-Z→`fg` 하면 복구) | ✅ `lib.rs::run_managed`, `tests/suspend.rs::ctrl_z_then_bg_does_not_redraw_footer_in_background` |
| A11 | **teardown(join/wait) 중 Ctrl-Z** | 렌더 루프가 끝나면 `SIGTSTP`를 `SIG_DFL`로 되돌림. 안 그러면 플래그 핸들러만 남아 정지도 진행도 못 하는 wedge가 됨 | ✅ `lib.rs::restore_default_suspend`, 유닛 `teardown_signal_disposition` |
| A12 | **Ctrl-Z 동안 `cp`도 함께 정지** | Ctrl-Z는 전경 프로세스 그룹 전체에 전달되므로 `cp`도 정지 → 복사가 실제로 멈춤. 재개하면 이어서 진행(cp의 정상 동작) | 📄 |
| A13 | **정지→재개 직후 rate/eta가 잠시 비정상** | 재개 시 진행 모델의 rate 히스토리를 **비운다** → 정지 구간이 평균에 섞이지 않고 재개 후 실제 처리량만으로 다시 계산된다. *이전에는* window가 벽시계 기준이라 "긴 시간 동안 진행 0"을 품어, 재개 후 약 1초간 rate가 0에 가깝고 eta가 `--:--`로 나왔다 | ✅ **해결(#9)** — 재개 시 rate 히스토리를 비운다. `progress::reset_samples` + `sampler::reset_rate_history` |
| A14 | **passthrough에서 Ctrl-Z** | 핸들러 없음 → 기본 동작으로 그룹 정지. footer가 없으므로 복구할 것도 없음 | ✅ 설계상 |

---

# B. 모드 선택 / passthrough 강제

`plan::decide`는 **아래 조건이 전부 참일 때만** managed를 고른다. 하나라도 어긋나면
passthrough이고, passthrough는 스트림 inherit + env 미변경이라 **`cp`와 바이트 동일**하다.

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| B1 | **`cprog a b \| less`, `\| tee`** (stdout이 파이프) | `stdout_tty=false` → passthrough | ✅ `plan.rs::stdout_not_tty_is_passthrough`, `tests/passthrough.rs::passthrough_output_is_byte_identical_to_cp` |
| B2 | **`cprog a b > log`** (리다이렉트) | 동일하게 passthrough | ✅ |
| B3 | **`cprog a b 2> err`** (stderr만 리다이렉트) | `stderr_tty=false` → passthrough. footer/요약이 stderr로 나가므로 stderr가 터미널이 아니면 managed를 포기한다 | ✅ `plan.rs::stderr_not_tty_is_passthrough` |
| B4 | **stdout과 stderr가 서로 다른 터미널** | `fstat`의 `(st_dev, st_ino)` 비교로 감지 → passthrough. 두 터미널에 나눠 쓰면 sole-writer 전제가 깨지기 때문 | ✅ `term::same_terminal`, `plan.rs::different_terminals_is_passthrough` |
| B5 | **`TERM` 미설정 / 빈 문자열 / `dumb`** | passthrough. 색도 함께 꺼짐 | ✅ `term::term_ok` |
| B6 | **CI 환경(`CI` 설정)** | passthrough. **`CI=`(빈 문자열)도 CI로 간주**한다 — `var_os().is_some()` 판정이라 값은 안 본다(보수적, 의도적) | ✅ `plan.rs::ci_is_passthrough` |
| B7 | **비-리눅스 / `/proc` 없음** | `cfg!(linux) && /proc/self/fd` 존재 확인 → 실패 시 passthrough | ✅ `term::proc_available` |
| B8 | **`stdbuf`가 PATH에 없음** | passthrough. `stdbuf` 없이는 `-v`가 파이프에서 block-buffer돼 라이브 UI를 못 지키므로, 약속을 못 지키느니 깔끔히 포기 | ✅ `term::stdbuf_available`, `tests/fallback.rs::missing_stdbuf_falls_back_to_passthrough` |
| B9 | **`cprog a b &`** (백그라운드 실행) | `tcgetpgrp(stdout) != getpgrp()` → `foreground=false` → passthrough. 백그라운드 작업이 터미널을 점거하면 안 되므로 | ✅ `term::is_foreground`, `tests/background.rs` (bug1 / #1) |
| B10 | **`tcgetpgrp`이 `ENOTTY`** (제어터미널 아님) | 백그라운드임을 **증명할 수 없으므로 관대하게 허용**(`foreground=true`). 실제 백그라운드 잡은 제어터미널을 갖고 있어 정상 감지됨 | 🟡 `term::is_foreground` |
| B11 | **`-i` / `--interactive` / `--interactive=…`** | passthrough 강제. 캡처하면 덮어쓰기 프롬프트가 깨지기 때문 | ✅ `args::inspect`, `plan.rs::interactive_forces_passthrough` |
| B12 | **`--help` / `--version`** | `informational` → passthrough. 복사가 없으니 감시할 것도, 요약할 것도 없음 | ✅ `args.rs::help_and_version_are_informational`, `tests/managed.rs::help_over_pty_passes_through_without_summary` |
| B13 | **인자 스캔 자체가 실패** (예: `--suffix` 값 누락) | `ArgError::Scan` → **보수적으로 passthrough**(cp가 알아서 에러를 냄) | ✅ `args.rs::missing_required_value_is_scan_error` |
| B14 | **`sudo cprog` / setuid `cp`** | `stdbuf`는 `LD_PRELOAD` 기반이라 setuid 바이너리에선 무시됨. cp가 setuid인 극히 드문 환경에서는 라이브성이 degrade | 📄 `capture-and-verbose.md` |
| B15 | **모드는 실행 전에 한 번만 결정** | 실행 중 파이프/TTY 상태가 바뀌어도 모드는 안 바뀐다(단순화). 유일한 런타임 재확인은 A10의 전경 여부 | 📄 `runtime-model.md` |

---

# C. `cp` 프로세스 생명주기

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| C1 | **`cp`가 PATH에 없음 / 실행 불가** | `Fatal::CpSpawn` → stderr 한 줄 + **exit 127**(셸 관례) | ✅ `messages::Fatal`, `messages.rs::cp_spawn_fatal` |
| C2 | **`cp`가 비-0으로 종료** (권한 없음, ENOSPC, `-r` 없이 디렉터리) | cp의 에러를 로그 영역에 relay → footer 지움 → **중립 문구** `✗ cp exited n - T elapsed`(진행바가 떴을 때만) → exit code 그대로 | ✅ `tests/managed.rs::managed_relays_cp_error_and_preserves_exit_code`, `tests/passthrough.rs::preserves_nonzero_exit_on_cp_failure` |
| C3 | **`wait()` 실패** (예: `ECHILD`) | `Fatal::CpWait { pid, source }` → exit 1 | ✅ `messages.rs::cp_wait_fatal` |
| C4 | **`cprog`가 먼저 죽음** | `PR_SET_PDEATHSIG(SIGTERM)`으로 `cp`가 고아로 남지 않음. `pre_exec` 실패는 삼킴(복사를 막을 이유가 아님) | 🟡 `process::spawn` |
| C5 | **C4에서 부분 복사된 대상 파일** | `cp`는 SIGTERM에 정리를 하지 않으므로 **잘린 대상 파일이 남는다.** 이는 `cp`를 직접 죽였을 때와 동일한 결과 — 의미론 보존 | 📄 |
| C6 | **PID 재사용 레이스** | `stdbuf`가 `cp`를 `exec`하므로 PID는 그대로 `cp`. 샘플러 join **이후에야** `wait()`로 reap하므로 샘플링 중 pid는 예약 상태 → 오염된 샘플이 불가능 | ✅ `process-model.md`, `tests/managed.rs`(D7) |
| C7 | **`stdbuf`는 있는데 `cp`를 못 찾음** | `stdbuf`가 exec에 실패해 자체 에러 + 127로 종료. cprog 입장에선 **spawn은 성공**했으므로 `Fatal::CpSpawn`이 아니라 "cp가 127로 종료"로 보인다(메시지는 relay되어 화면에 보임) | 🟡 |
| C8 | **자식은 하나뿐** | 외부 progress 도구도 hidden PTY도 없으므로 누수될 helper 프로세스 자체가 없다 | 📄 |

---

# D. 캡처 / relay / 버퍼링

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| D1 | **파이프에서 `cp`가 block-buffer** | `stdbuf -oL`로 라인버퍼 강제 → `-v`가 파일마다 실시간 도착. 진짜 `cp`로 통합 검증(가짜 cp는 flush를 제어할 수 있어 이 버그를 못 잡음) | ✅ `tests/managed.rs::managed_verbose_lines_interleave_with_footer_during_copy` |
| D2 | **개행 없는 꼬리 바이트** | 받는 즉시 relay. 개행을 기다리며 붙잡지 않는다 | ✅ `capture.rs::relays_partial_line_without_waiting_for_newline` |
| D3 | **`-v` 줄이 read 청크 경계에 걸침** | 부분 줄은 pending으로 두고, `\n`이 도착하는 청크에서 **정확히 한 번** 펄스 | ✅ `verbose::LinePulse`, `verbose.rs::newline_split_across_chunks_pulses_when_completed` |
| D4 | **파일명에 개행·NUL·제어문자·ANSI** | `-v` 내용을 파싱하지 않으므로 로직 무영향(`\n`만 세고, 그마저도 "펄스가 하나 더" 수준). 경로는 `/proc` readlink에서 얻음 | ✅ `verbose.rs::arbitrary_bytes_only_newlines_count` |
| D5 | **사용자가 이미 `-v`를 줌** | 이중 주입 안 함(`-v` 하나만) | ✅ `process.rs::managed_does_not_double_inject_verbose` |
| D6 | **stdout/stderr 인터리브 순서** | 파이프 둘을 각각 중계하므로 상대 순서가 순정 `cp`와 미세하게 다를 수 있음 | 📄 `capture-and-verbose.md` |
| D7 | **relay 쓰기 실패**(EPIPE 등) | 무음 best-effort. exit code에 영향 없음 | ✅ `render.rs::io_failure_is_returned_and_drop_never_panics`, `tests/exit_contract.rs` |
| D8 | **reader가 read 에러** | 루프 종료(EOF와 동일 취급) → 채널 닫힘 → 메인 루프도 정리 단계로 | 🟡 `capture::relay_stdout`/`relay_bytes` |
| D9 | **대량 소파일로 `-v` 폭주** | 채널이 **경계 있는 `sync_channel`** 이라 큐가 차면 리더가 대기하고 → 파이프가 차고 → `cp`가 잠시 기다린다(백프레셔 복원). 렌더 루프는 tick마다 큐를 드레인해 한 번에 쓴다. *이전에는* unbounded 큐라 터미널이 느리면 못 그린 로그가 메모리에 계속 쌓였다 | ✅ **해결(#8)** — 경계 있는 `sync_channel` + tick당 배치 드레인. `capture.rs::a_full_queue_makes_the_relay_wait_rather_than_buffer` |
| D10 | <a id="d10"></a>**footer가 떠 있는 동안 도착한 여러 조각짜리 메시지가 화면에서 유실** | 터미널에 **개행으로 끝나지 않은 줄이 남아 있는 동안 footer를 보류**하고, 다음 개행에서 다시 그린다 → 여러 조각으로 오는 `cp` 에러가 온전히 남는다. *이전에는* 조각마다 footer가 `\r`로 그 줄을 덮고 다음 erase가 지워, `cp: error writing '<경로>'`가 사라지고 마지막 조각만 남았다(glibc `error()`는 한 줄을 write 4회로 낸다) | ✅ **해결(#4)** — 부분 줄이 화면에 있으면 footer를 보류. `render::line_pending`, `tests/log_integrity.rs` |
| D11 | **느린 파일이 끝난 뒤 footer가 잠시 낡은 값을 보여줌** | tick 결과를 `Sample`/`Skip`/`Idle` 셋으로 구분해, **잴 게 없으면(`Idle`) 바를 내리고** 읽기가 실패했을 때만(`Skip`) 마지막 값을 유지한다. *이전에는* 둘 다 `None`이라 끝난 파일의 마지막 값이 계속 발행돼 정지된 바가 남았다 | ✅ **해결(#7)** — `Tick::Idle`과 `Tick::Skip`을 구분해 끝난 파일은 바를 내린다. `sampler.rs::finished_file_reports_idle_not_skip` |

---

# E. 진행 계산 (`/proc` + `stat`)

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| E1 | **`copy_file_range`로 `fdinfo:pos`가 0** | 애초에 `pos`를 안 읽는다. 대상의 `st_size`를 읽으므로 coreutils 9.x에서도 정확 | ✅ 설계 고정, `progress-model.md` |
| E2 | **`fallocate` 선할당으로 `st_size`가 즉시 full** | 바가 즉시 100%가 된다 — **의도적으로 수용한 한계**다. GNU `cp`엔 선할당 경로가 없어 도달할 수 없고, 이를 막으려 blocks로 재면 sparse·압축·지연할당이라는 **흔한** 경우가 모두 틀어진다(#12) | 📄 `sampler.rs::a_preallocated_destination_reads_complete_immediately` |
| E3 | **reflink / CoW로 즉시 완료** | 첫 샘플 전에 끝나거나 100%로 점프. 정확하지만 점진적이지 않음 | 📄 |
| E4 | **sparse 파일 / hole이 있는 원본** | `done`·`total` 둘 다 `st_size`라 hole이 많아도 **비율이 정확**하고 100%에 도달한다. 실제로 쓰인 바이트가 아니라 논리적 진행을 세므로 hole 구간에서 rate만 높게 보인다. *이전에는* `done`이 `min(size, blocks*512)`로 눌려 비율이 크게 과소 표시됐다 | ✅ **해결(#3, #12)** — 언제나 `st_size`로 측정. `sampler::FileStat::copied_bytes` |
| E5 | **빈 원본(total = 0)** | `percent_of`가 `Some(100.0)` — 0 나누기 없음 | ✅ `progress.rs::percent_empty_source_is_complete_not_divide_by_zero` |
| E6 | **`done > total` 오버슈트** | 100으로 clamp | ✅ `progress.rs::percent_overshoot_clamps_to_100` |
| E7 | **두 샘플 간 증가가 0이거나 음수** | rate는 정확히 `0.0`(음수 delta는 saturating으로 0), eta는 `None`(`--:--`) | ✅ `progress.rs::rate_zero_when_no_increase`/`rate_zero_when_negative_increase` |
| E8 | **현재 대상이 `/proc`에 없음** (파일 사이, 디렉터리 생성 중, hardlink/symlink 생성) | write fd가 없으므로 `select_current`가 `None` → 바 없음 | ✅ `proc.rs::no_write_fd_means_no_current_file` |
| E9 | **원본이 특수파일**(fifo/device) | `RegularRead`가 아니므로 `total = None` → indeterminate(가짜 100% 금지) | ✅ `proc.rs::special_source_gives_indeterminate_total` |
| E10 | **`/proc`/`stat` 읽기 실패** (fd 닫힘, pid 종료, 권한, hidepid) | 그 tick만 skip하고 마지막 값 유지. 크래시 없음 | ✅ `sampler.rs::dest_stat_error_skips_tick_and_keeps_model`, `proc_error_skips_tick` |
| E11 | **stdin/stdout이 정규 파일로 리다이렉트** | `fd > 2`만 후보로 삼아 stdio를 복사 대상으로 오인하지 않음 | ✅ `proc.rs::redirected_low_fds_are_not_selected` |
| E12 | **rate/eta가 아직 미지** | 샘플 2개 미만이면 `None` → `(-- MiB/s)` / `⏳ --:--`. 엉뚱한 숫자를 지어내지 않음 | ✅ `progress.rs::rate_unknown_before_two_samples` |
| E13 | **cp가 다음 파일로 넘어감** | 대상 경로가 바뀌면 새 모델 + 새 `total`로 리셋 | ✅ `sampler.rs::new_file_resets_total` |
| E14 | **`cp`의 기본 `--sparse=auto`가 만든 대상** | sparse 대상에서도 `st_size`로 재므로 정상 동작한다. 실측: hole 48MiB를 낀 144MiB 복사에서 **96.6%** 까지 상승(구 구현 상한은 66.7%). *이전에는* `cp`의 기본 `--sparse=auto`가 만든 대상의 `blocks*512`가 작아 `done`이 눌렸고, 200MiB(대부분 hole) 복사가 완료 시점에도 0.5%로 보였다 | ✅ **해결(#3, #12)** — 언제나 `st_size`로 측정. `sampler::FileStat::copied_bytes` |
| E15 | **압축/inline 파일시스템**(btrfs `compress`, ZFS) 또는 `st_blocks`를 0으로 보고하는 FS | `st_blocks`를 아예 보지 않으므로 영향받지 않는다. *이전에는* `blocks*512 < size`가 정상인 이 파일시스템들에서 진행이 과소 표시되고, `st_blocks`가 0이면 바가 0%에 고정됐다 | ✅ **해결(#3, #12)** — size만 보므로 영향 없음 |
| E16 | **`total`과 `done`의 측정 기준 비대칭** | `total`(원본)과 `done`(대상) 모두 **`st_size`** 로 재므로 기준이 같다. *이전에는* `done`만 `min(size, blocks*512)`라 기준이 어긋나 오차가 그대로 비율에 남았다 | ✅ **해결(#3, #12)** — 양쪽 모두 `st_size`라 비대칭이 사라짐 |
| E17 | **상속된 fd가 대상으로 오인됨** | 쓰기 후보가 여럿이면 **틱 사이에 크기가 자라는 fd**를 고른다 → 셸이 물려준 fd(`3>/tmp/log`)는 자라지 않으므로 배제된다. *이전에는* `fd > 2`인 첫 `RegularWrite`를 골라, 번호가 낮은 상속 fd가 진짜 대상보다 먼저 선택됐다 | ✅ **해결(#6)** — 후보가 여럿이면 자라는 fd를 고른다. `sampler::choose_dest` |
| E21 | **상속된 *읽기* fd가 원본으로 오인됨** | 원본을 **고른 대상 fd보다 작은 읽기 fd 중 가장 큰 것**으로 짝짓는다(`cp`는 원본을 열고 곧바로 대상을 연다) → 상속된 읽기 fd와 대상 이후에 열린 fd가 함께 배제된다. *이전에는* 가장 낮은 읽기 fd를 원본으로 삼아 `total`이 엉뚱한 파일 크기가 됐다 | ✅ **해결(#11)** — `proc::source_for`. `exec 3<tiny` 상태에서 96 MiB 원본을 정확히 인식 |
| E22 | **`Basis::Blocks` 오검출(ext4 delayed allocation)** | 판정 분기가 **없다** — 언제나 `st_size`로 잰다. *이전에는* 선할당 판정 조건이 ext4 지연 할당과 같은 모양이라, 첫 샘플이 그 순간에 걸리면 blocks 기준으로 고정돼 바가 0% 근처에 머물 수 있었다. `cp`가 하지 않는 동작을 막자고 리눅스 기본 파일시스템에서 틀릴 위험을 안고 있던 셈 | ✅ **해결(#12)** — `Basis` 제거, 항상 `st_size` |
| E18 | **삭제된 대상**(`readlink`가 `… (deleted)`) | 그 경로의 `stat`이 실패 → tick skip → 마지막 값 유지 | 🟡 E10과 동일 경로 |
| E19 | **아주 빠른 파일** | 첫 샘플 전에 끝남 → 바 없이 지나감(의도된 동작) | 📄 |
| E20 | **샘플링 비용** | 느린 파일일 때만 `readlink` 1회 + `stat` 1~2회/100ms. 파일 **데이터는 읽지 않아** 페이지 캐시를 오염시키지 않음 | 📄 |

---

# F. 터미널 / 렌더

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| F1 | **바 도중 리사이즈(SIGWINCH)** | 플래그 latch → 다음 tick에 `TIOCGWINSZ` 재조회 → 재배치 | ✅ `term::should_requery_size`, `tests/resize.rs` |
| F2 | **SIGWINCH 유실/합쳐짐** | 1초 폴백 재조회가 있어 낡은 크기로 고정되지 않음 | ✅ `term.rs::resize_requery_rule` |
| F3 | **터미널 높이 < 3행** | footer 억제(`rows < MIN_LOG_ROWS + 1`) — 로그 영역 2행을 항상 남긴다 | ✅ `ui::render_footer`(C3) |
| F4 | **좁은 폭** | `eta → rate → size → bar → percent` 순으로 필드를 버림. 바는 `50→20→10`으로 양자화 축소, 10칸도 못 넣으면 버림 | ✅ `ui.rs` ATTEMPTS |
| F5 | **극단적으로 좁은 폭**(percent도 안 들어감) | 최후 수단으로 percent만 출력하며 **오버플로우를 허용**. 터미널이 줄바꿈하면 footer가 2행을 차지해 한 줄만 지우는 erase로는 잔상이 남을 수 있음 | 📄 `ui::render_footer` 주석 |
| F6 | **렌더 중 panic** | `FooterGuard::Drop`이 unwind 중에도 footer 지우고 커서 복원 | ✅ `render.rs::drop_erases_even_on_panic` |
| F7 | <a id="f7"></a>**`SIGKILL` / `SIGSEGV`** | 핸들러도 `Drop`도 못 돈다 → **footer 잔상 + 커서가 숨겨진 채로 터미널이 남는다.** `PDEATHSIG`로 cp는 정리되지만 화면은 사용자가 `tput cnorm` / `reset`으로 복구해야 함 | 📄 **문서화(#10)** — 회피 불가. `usage.md`에 `tput cnorm` 안내 추가 |
| F8 | **렌더/IO 실패** | best-effort. exit code 불변 | ✅ `render.rs::io_failure_is_returned_and_drop_never_panics` |
| F9 | **로그 바이트 도착** | footer 지움 → 로그 씀 → footer 재그림(erase-redraw) | ✅ `render.rs::write_log_erases_then_writes_then_redraws` |
| F10 | **`NO_COLOR` 설정** | 값과 무관하게 색 끔 | ✅ `term::color_from` |
| F11 | **비-UTF-8 로케일** | 블록 글리프 대신 ASCII 바(`[###---]`)로 폴백 | ✅ `term::unicode_from` |
| F12 | **같은 터미널에 다른 프로세스가 씀** | sole-writer 전제가 깨져 footer가 깨질 수 있음. cprog가 제어할 수 없는 영역 | 📄 |
| F13 | **아주 오래된 터미널이 `DECTCEM`(`?25l`)을 모름** | 커서 숨김/복원 시퀀스가 그대로 화면에 보일 수 있음. `TERM` 검사는 `dumb`만 거르고 terminfo는 쓰지 않음 | 📄 |
| F14 | **파일명 관련 렌더 사고** | **구조적으로 불가능** — footer에 파일명을 넣지 않으므로 긴 이름·제어문자 문제가 발생할 표면이 없다 | 📄 `ui.md` C4/C5 무효화 |

---

# G. 인자 (Arguments)

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| G1 | **인자 없음** | `Fatal::Usage` → `usage: cprog <cp args...>` + exit 1 | ✅ `args.rs::no_args_is_usage_error`, `tests/passthrough.rs::no_args_prints_usage_and_exits_1` |
| G2 | **G1의 메시지가 `cp`와 다름** | `cp`는 `cp: missing file operand …`를 낸다. cprog는 자기 usage를 낸다(코드는 둘 다 1). `alias cp=cprog` 사용자에게 보이는 **의도적 차이** | 📄 |
| G3 | **`-S`/`--suffix`, `-t`/`--target-directory`의 값이 플래그처럼 생김** | 값을 소비해 `cp -S -i a b`의 `-i`를 interactive로 오인하지 않음 | ✅ `args.rs::suffix_value_looking_like_interactive_is_not_a_flag` 외 |
| G4 | **`--` 이후의 `-i`/`-v`** | operand로 취급 — 플래그로 세지 않음 | ✅ `args.rs::double_dash_makes_following_dash_i_an_operand` |
| G5 | **번들 단축 옵션**(`-ai`, `-av`) | 각각 분해해 감지 | ✅ `args.rs::bundled_short_interactive` |
| G6 | **operand 뒤에 오는 옵션**(`cp src -i dst`) | `cp`처럼 permute 처리 | ✅ `args.rs::permuted_interactive_after_operand` |
| G7 | **그 외 cp 옵션**(`--sparse=`, `--reflink=`, `--preserve=`, `--backup=`) | 인식하지 않고 그대로 통과. 이들은 값을 `=`로 붙이거나 선택적 값이라 `-i`/`-v` 오인 위험이 없다. **값 필수인 단축 옵션은 `-S`/`-t`뿐**이라 G3으로 충분 | 📄 |
| G8 | **비-UTF-8 인자** | `OsString`으로 다뤄 그대로 `cp`에 전달 | 🟡 `args::inspect` |

---

# H. 환경 / 설정

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| H1 | **`CPROG_*_MS`가 숫자가 아님** | 에러 없이 **조용히 기본값**으로 폴백 | 🟡 `lib::env_ms` |
| H2 | **`CPROG_SLOW_THRESHOLD_MS=0`** | 모든 파일이 즉시 "느림" → 거의 항상 footer가 뜸(테스트에서 이 성질을 이용) | 🟡 |
| H3 | **managed의 env 변경 범위** | `QUOTING_STYLE=shell-escape` **하나뿐**. `LC_ALL=C`는 일부러 안 건다 — `-v`를 파싱하지 않으니 이득이 없고, C 로케일은 한글 등 비-ASCII 파일명을 옥타 이스케이프로 깨뜨린다 | ✅ `process.rs::managed_sets_only_quoting_style_not_locale` |
| H4 | **passthrough의 env** | **전혀 건드리지 않음** → cp의 에러 메시지 로케일까지 바이트 동일 | ✅ `process.rs::passthrough_never_touches_env` |
| H5 | **Mutex poisoning**(스레드 패닉) | `lock_shared`가 `into_inner()`로 복구. 공유 값(슬로우 타이머·최근 샘플)엔 깨질 불변식이 없고, 여기서 죽으면 `cp`를 wait 못 해 exit code 계약이 깨지므로 | ✅ `lib::lock_shared` |
| H6 | **요약이 안 나오는 경우** | `progress_shown == false`(footer가 한 번도 안 뜸)면 요약 없음. `--help`, 즉시 끝난 복사, **그리고 권한 문제로 샘플이 계속 실패한 경우**도 포함 | ✅ `messages.rs::no_summary_without_progress` |

---

# 해결 기록 (R1~R7)

처음 이 문서를 쓰며 식별한 미커버 갭과, 이후 이슈 #3–#10으로 처리한 내용이다. 각 항목은 이
저장소의 `target/debug/cprog`로 **재현을 시도한 결과**를 함께 적는다 — 재현된 것과 코드에서
도출만 한 것을 구분한다.

재현에 공통으로 쓴 장치: PTY(managed 모드 진입), 원본으로 쓰는 **FIFO**(공급을 늦춰 복사를
길게 유지 → footer 등장), `CPROG_SLOW_THRESHOLD_MS=50`.

<a id="r1"></a>
### R1. `min(size, blocks*512)`가 sparse 대상에서 진행을 크게 왜곡 — **[실측]**

`sampler::FileStat::effective_bytes`는 `fallocate` 선할당(E2)이 가짜 100%를 만드는 걸 막으려고
`done`을 `min(st_size, st_blocks*512)`로 누른다. 그런데 `total`은 원본의 `st_size` 그대로다.
따라서 **대상이 sparse면 비율이 그대로 무너진다.**

```
$ truncate -s 200M holey.bin && printf 'END' | dd of=holey.bin bs=1 seek=209715196 conv=notrunc
$ cp holey.bin holey.dst          # 기본 옵션 = --sparse=auto
holey.dst: st_size=209715200   st_blocks*512=1052672
   -> done  = min(209715200, 1052672) = 1052672
   -> total = 209715200
   -> 복사가 끝난 뒤에도 0.50 %
```

중요한 건 이게 **특수한 파일시스템 얘기가 아니라는 점**이다. `--sparse=auto`는 `cp`의 기본값이라
hole이 있는 원본이면 언제나 이 경로를 탄다 — VM 디스크 이미지, DB 파일, 코어덤프처럼
"크고 hole이 많은" 파일이 정확히 cprog가 바를 띄우려는 대상이다. btrfs `compress`나
`st_blocks=0`을 보고하는 FS(E15)는 같은 메커니즘의 부차적 사례다.

또한 `progress-model.md`의 sparse 관련 서술("비율은 유의미, rate가 높게 보일 수 있음")은
**현재 구현과 어긋난다** — 실제로는 비율이 과소 표시된다. 그 문서도 함께 고쳐야 한다.

**제안.** 매 tick `min`을 취하는 대신, **파일이 바뀔 때 한 번 기준을 정해 그 파일 내내 고정**한다.
첫 샘플에서 `size ≈ total`인데 `blocks*512`가 훨씬 작으면 선할당으로 보고 blocks 기준,
그렇지 않으면 size 기준. `total`도 같은 기준으로 재면 E16의 비대칭이 사라진다. 두 실패 모드
(선할당 / sparse)는 서로 반대 방향이라 **매 tick `min` 하나로는 동시에 만족시킬 수 없다.**

<a id="r2"></a>
### R2. footer가 떠 있으면 `cp` 에러 메시지가 화면에서 잘려나감 — **[실측]**

`cp`의 에러 한 줄은 glibc `error()` 때문에 **write 4회**로 도착한다:

```
$ strace -e trace=write cp /nonexistent/zzz /tmp/out
write(2, "cp: ", 4)
write(2, "cannot stat '/nonexistent/zzz'", 30)
write(2, ": No such file or directory", 27)
write(2, "\n", 1)
```

`capture`는 읽은 즉시 relay하므로 조각마다 `write_log`가 한 번씩 돈다. `write_log`는
`erase → 바이트 write → footer draw` 순인데, `draw`가 `\r`로 그 줄 맨 앞으로 돌아가 덮고,
다음 조각의 `erase`(`\r\x1b[K`)가 그 줄을 통째로 지운다. 결과적으로 **개행을 품은 마지막
조각만 살아남는다.**

FIFO로 느린 복사를 만들어 footer를 띄운 뒤 `RLIMIT_FSIZE`로 쓰기 에러를 유도해 재현했다.
PTY 원본 바이트에는 에러 전문이 들어 있지만, 같은 바이트를 터미널로 렌더하면:

```
'/tmp/…/slow.src' -> '/tmp/…/dst.bin'
: File too large                        ← "cp: error writing '/tmp/…/dst.bin'" 이 사라짐
✗ cp exited 1 - 00:01 elapsed
```

파일명도 동작명도 없이 `: File too large`만 남아 **에러가 사실상 읽을 수 없게 된다.** 기존
통합 테스트(`managed_relays_cp_error_and_preserves_exit_code`)는 "cp가 즉시 실패해 footer가 뜬 적
없는" 경우만 다루므로 이 구간을 잡지 못한다.

**제안.** `verbose::LinePulse`가 이미 "미완결 줄 pending" 상태를 들고 있다. 이 값을 relay로
넘겨 **화면에 부분 줄이 남아 있는 동안에는 footer를 그리지 않는다**(다음 개행 후 재개).
상태는 이미 존재하므로 배선만 하면 되고, D11(끝난 파일의 낡은 바)도 같이 눌러준다.

<a id="r3"></a>
### R3. 시그널을 처리 못 하는 `cp` 앞에서 join이 무한 대기 — **[실측]**

`run_managed`는 시그널을 받으면 cp에 같은 시그널을 전달한 뒤 reader 스레드를 join한다.
cp가 그 시그널을 **처리할 수 없는 상태**면 파이프가 닫히지 않아 join이 끝나지 않는다.

```
cp를 SIGSTOP → cprog에만 SIGTERM
  → cprog가 10초 넘게 종료하지 않음 (State: S (sleeping), cp는 T (stopped))
  → cp에 SIGCONT를 보내자 cprog가 즉시 signaled 종료 (SIGTERM)
```

`process-model.md`의 "join이 느려도 유계"라는 서술이 이 경우 성립하지 않는다. 실제 운영에서
더 그럴듯한 방아쇠는 수동 `kill -STOP`보다 **끊긴 네트워크 마운트에서의 uninterruptible I/O**다
(복사 도구에겐 현실적인 상황이며, 이때는 SIGCONT로도 못 푼다).

**제안.** (a) 시그널 전달 직후 `SIGCONT`를 함께 보낸다 — 정지 사례를 한 줄로 해결하고, 위
실험이 그 효과를 이미 보여준다. (b) 그것과 별개로 **join에 기한을 둔다**: 만료되면 스레드를
detach하고 cp의 상태 확인으로 넘어간다. D-state는 (a)로 못 풀리므로 (b)가 있어야 실제로 유계가
된다.

<a id="r4"></a>
### R4. relay 채널이 unbounded — **[코드]**

`lib.rs`는 `mpsc::channel()`(무한 큐)을 쓴다. reader 스레드는 파이프에서 읽어 큐에 넣기만 하고,
소비는 메인 루프의 터미널 쓰기 속도에 묶인다. 즉 **파이프가 원래 제공하던 백프레셔가 사라지고**
따라잡지 못한 로그가 메모리에 쌓이는 구조다. 대량 소파일 복사 + 느린 터미널(원격 ssh 등)이
조건이다. 큐가 실제로 유의미하게 자라는 것까지 측정하지는 않았다.

**제안.** `mpsc::sync_channel(N)`으로 경계를 두어 백프레셔를 되살리거나, 메인 루프에서
`try_recv`로 큐를 **drain해 한 번에 write**한다. 후자는 청크마다 반복되던
erase→write→draw 왕복을 줄여 성능에도 이롭다.

<a id="r5"></a>
### R5. 정지→재개 직후 rate/eta가 잠시 흐트러짐 — **[코드] · 경미**

`ProgressModel`의 window는 벽시계 기준이라 정지 구간을 "진행 0"으로 흡수한다. 다만 **처음 쓴
것보다 영향이 작다**: cp도 함께 정지했으므로 delta가 정확히 0이면 `rate = 0`, `eta = None`
(`--:--`)이 되어 사실상 맞는 표시이고, window(1초)가 지나면 자동 회복한다. 정지 직전 몇 바이트가
더 안착해 delta가 작은 양수일 때만 eta가 터무니없이 커진다.

**제안.** `suspend_restore` 경로에서 진행 모델의 샘플을 비운다. 자동 회복되는 표시 문제이므로
우선순위는 낮다.

<a id="r6"></a>
### R6. 상속된 write fd를 대상으로 오인 — **[실측]**

셸 리다이렉션이 만든 fd는 CLOEXEC가 아니라 `cp`까지 상속되고, `select_current`는 `fd > 2`인
**첫 `RegularWrite`** 를 고른다. 리다이렉션 fd는 번호가 낮아 cp가 나중에 여는 진짜 대상보다
먼저 걸린다.

```
$ exec 3>decoy.log; exec cprog slow.src dst.bin      (PTY 안에서)
/proc/<cp>/fd:  3 -> decoy.log (쓰기)   4 -> slow.src (읽기)   5 -> dst.bin (쓰기)
select_current 가 고르는 것 = fd 3 (decoy.log)   ← 진짜 대상 아님
```

이때 바는 자라지 않는 decoy를 따라가므로 진행이 0에 머물거나 indeterminate가 된다. 빈도는
낮지만(대화형 셸에서 여분 fd를 여는 경우), 규칙 자체가 fd 번호 순서에 의존한다는 게 문제다.

**제안.** 후보가 둘 이상이면 **연속 tick 사이에 크기가 증가하는 fd**를 고른다 — 정의상 그것이
복사 중인 대상이다. 후보가 하나면 지금 동작 유지. 첫 tick에서는 판단을 미루면 된다.

<a id="r7"></a>
### R7. `SIGKILL` 후 커서가 숨겨진 채로 남음 — **[코드] · 회피 불가**

핸들러도 `Drop`도 돌 수 없으므로 프로세스 안에서 막을 방법이 없다. 대응은 **알려두는 것**뿐이다:
`usage.md`에 "강제 종료(`kill -9`) 후 커서가 안 보이면 `tput cnorm`" 한 줄을 더하는 것을 권한다.

---

## 처리 결과 요약

전부 이슈로 등록해 TDD로 처리했다. "확인" 열은 수정 후 **같은 재현 절차를 다시 돌린 결과**다.

| 권고 | 이슈 | 처리 | 확인 |
|---|---|---|---|
| [R1](#r1) | #3 | 측정 기준을 파일마다 한 번 고정(`sampler::Basis`) — 이후 **#12에서 분기째 제거**되어 항상 `st_size` | hole 48MiB를 낀 144MiB 복사에서 구 상한 66.7%를 넘어 **92.6%** 관측 |
| [R2](#r2) | #4 | 부분 줄이 화면에 있으면 footer 보류(`render::line_pending`) | 복사 도중 쓰기 에러에서 `cp: error writing '<경로>': …` **전문이 화면에 남음** |
| [R3](#r3) | #5 | 시그널 전달 시 `SIGCONT` 동반 | 정지된 cp + SIGTERM에서 10초+ hang → **0.11초 signaled 종료** |
| [R6](#r6) | #6 | 후보가 여럿이면 자라는 fd 선택(`sampler::choose_dest`) | `exec 3>decoy.log` 상태에서도 **진짜 대상**의 진행을 표시 |
| [R4](#r4) | #8 | 경계 `sync_channel` + tick당 배치 드레인 | 큐가 차면 relay가 대기함을 유닛 테스트로 고정 |
| [R5](#r5) | #9 | 재개 시 rate 히스토리 폐기 | 정지 구간이 평균에 섞이지 않음(유닛) |
| [R7](#r7) | #10 | 회피 불가 → `usage.md`에 `tput cnorm` 안내 | — |
| E22 | #12 | `Basis` 분기 제거, 항상 `st_size` | `sampler.rs` 순감 117줄(112 추가/229 삭제), sparse·압축·지연할당 전부 정상 |
| E21 | #11 | 원본을 대상 fd 기준으로 짝짓기(`proc::source_for`) | `exec 3<tiny.txt` 상태에서도 `total`을 **96.0 MiB**(진짜 원본)로 인식 |

D11(끝난 파일의 낡은 바)은 #7로 함께 처리했다 — `Tick::Idle`과 `Tick::Skip`을 구분해,
잴 게 없을 때는 바를 내리고 읽기가 실패했을 때만 마지막 값을 유지한다.
