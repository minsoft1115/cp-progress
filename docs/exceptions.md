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

## 모든 예외를 관통하는 두 불변식

이 문서의 모든 항목은 결국 이 둘로 환원된다:

1. **`cp`의 결과가 최종 권위.** cprog 쪽 실패(캡처·렌더·샘플·정리)는 절대 `cp`의 exit
   code/시그널을 바꾸지 않는다. 코드로는 무음 best-effort(`let _ = …`)로 표현된다.
   **panic만은 예외가 될 수 있다** — panic은 exit 101이라 `cp`의 결과를 덮는다. 그래서
   프로덕션 코드의 `unwrap`/`expect`는 전부 도달 불가여야 하고, 그 근거를 [F16](#f16)에
   전수로 적어둔다.
2. **터미널은 어떤 경로로 끝나도 복구된다.** 정상 종료·`Fatal`·panic·시그널·Ctrl-Z —
   `FooterGuard::Drop`(및 `suspend_restore`)이 footer를 지우고 커서를 되살린다.
   **단 하나의 예외는 SIGKILL/SIGSEGV**(→ [F7](#f7)).

---

# A. 시그널 (Signals)

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| A1 | **`cp`가 시그널 `s`로 죽음** (Ctrl-C가 전경 그룹 전체에 전달된 경우 포함) | footer 지움 → **요약 없음** → 기본 핸들러 복원 + `s` unblock + 자기 자신에게 `s` 재전송 → 셸이 진짜 signaled 종료를 봄. 재전송이 실패하면 `128 + s` 반환 — **이 폴백은 실제로는 도달 불가다(A1a·[F15](#f15))** | ✅ `exit::finalize`/`reraise`, `tests/signals.rs::sigint_during_managed_copy_cleans_footer_and_preserves_signal` |
| A1a | <a id="a1a"></a>**재전송 경로의 정확한 범위** (실시간 시그널 / 재전송 실패) | 재현은 `signal_hook::low_level::emulate_default_handler`가 한다(기본 disposition 복원 → unblock → raise). **① 표준 시그널 중 기본 동작이 _종료_ 인 것** — 이 함수는 자기 raise가 돌아오면 `abort()`로 끝내므로, 재전송이 실패해도 A1의 `128 + s`에 **닿지 않고 SIGABRT로 죽는다**. 닿으려면 `WTERMSIG`가 준 유효한 시그널에 `sigaction`이 실패해야 하는데 리눅스에선 일어나지 않는다 — **블록된 시그널로도 부족하다**(내부에서 먼저 unblock하므로. 블록한 SIGTERM이 그대로 SIGTERM으로 죽는 것을 실측). **①a 표준 시그널 중 기본 동작이 _무시_ 나 _정지_ 인 것**(SIGWINCH·SIGCHLD·SIGURG, SIGTSTP·SIGTTIN 등) — **abort하지 않는다.** `emulate_default_handler`가 그 기본 동작을 그대로 재현하고 **`Ok`로 정상 반환**하므로 `reraise`도 돌아오고 `128 + s`에 **실제로 닿는다**(실측: `finalize(Signal(SIGWINCH))` → 156, `finalize(Signal(SIGCHLD))` → 145. ①의 abort 서술을 이 부류까지 확장하면 거짓이다). 그럼에도 도달 불가인 **진짜 근거는 abort가 아니라 `WTERMSIG`가 이런 시그널을 지목할 수 없다는 것**이다 — 프로세스를 종료시키지 않는 시그널은 애초에 wait status의 종료 시그널로 나타나지 않으므로, `disposition`이 `Signal(s)`를 만들 수 없다(#69 D). **② 실시간 시그널**(`SIGRTMIN`~`SIGRTMAX`) — 이 함수의 표에 없어 `EINVAL`이므로 `low_level::raise`로 직접 올린다. disposition이 기본값인 것은 **보장**된다(cprog는 SIGINT/TERM/HUP/QUIT/WINCH/TSTP에만 핸들러를 단다). **이 갈래가 없으면 실시간 시그널은 언제나 `128 + s`로 정상 종료해 A1이 깨진다** — 그래서 갈래 자체는 반드시 있어야 한다. **단, 여기서도 `128 + s`에는 닿지 않는다.** 블록된 시그널이면 raise가 pending만 남기고 돌아오는 건 맞지만, 애초에 이 함수에 오려면 `WTERMSIG`가 그 시그널을 지목했어야 하고, **`cp`는 cprog의 마스크를 그대로 물려받으므로**(spawn을 넘어 상속된다 — 실측) cprog가 블록한 시그널로는 `cp`가 죽을 수 없다. 블록을 뚫고 죽이는 부류는 커널이 강제 전달하는 동기 시그널뿐이고(SIGSEGV·SIGBUS·SIGILL·SIGFPE·SIGSYS·SIGXFSZ), **그건 전부 표준 시그널이라 ①로 간다** — 실시간 시그널은 `kill`/`sigqueue`로만 오므로 블록을 못 뚫는다. 여섯 개 다 `signal-hook`의 표에 있어 `EINVAL`로 새지 않는 것도 확인했다. 즉 **`128 + s`는 어느 갈래로도 도달 불가**이며, 그 판정은 [F15](#f15)에 둔다(#60) | ✅ `tests/signals.rs::cp_killed_by_a_realtime_signal_still_exits_cprog_signaled` (#43) |
| A2 | **`cprog`만 단독으로 시그널 받음** (`kill <cprog-pid>`) | 렌더 루프가 즉시 break → **받은 시그널을 그대로 `cp`에 전달**(SIGTERM으로 정규화하지 않음) → cp가 그 시그널로 죽고 파이프가 닫혀 join이 유계 → A1 규칙으로 cprog도 같은 시그널로 종료 | ✅ `lib.rs::run_managed`(`received_signal`), `tests/signals.rs::signal_to_cprog_alone_is_forwarded_to_cp_and_re_raised` |
| A3 | **시그널 도착 시 `cp`가 이미 종료** | `try_wait`이 `Some(_)`이면 전달 생략. `try_wait` 자체가 에러면 "살아있을 수도"로 보고 방어적으로 `kill` — 이미 죽은 pid엔 `ESRCH`라 무해 | 📄 `lib.rs::run_managed` — **테스트로 관측할 수 없다.** cp는 샘플러 join 뒤 `child.wait()`에서야 reap되므로, 가드를 빼도 `kill`이 닿는 대상은 아직 reap되지 않은 **좀비**다(무음 no-op, 밖에서 구분 불가). pid 재사용도 좀비가 점유해 막는다. 가드를 빼도 스위트가 그대로 통과하는 것을 실측했고, 그래서 ✅가 아니라 여기에 근거를 적는다(#59) |
| A4 | **잡는 시그널의 범위** | `SIGINT`/`SIGTERM`/`SIGHUP`/`SIGQUIT` 4종만 등록. 그 외(`SIGUSR1` 등)는 **기본 동작** — 종료형이면 정리를 한 줄도 못 돌고 죽으므로 **footer 잔상 + 커서 숨김이 남는다**([F7](#f7)과 같은 결말). 4종에 한정한 대가이고, 모든 시그널을 잡으면 그건 그것대로 `cp`와 달라진다 | ✅ `lib.rs::run_managed`, `tests/signals.rs::a_signal_cprog_does_not_register_keeps_its_default_action` (#47) |
| A5 | **Ctrl-C를 두 번 이상** | 첫 번째로 렌더 루프가 break. 이후 teardown(join/wait) 구간의 추가 시그널은 핸들러가 값만 덮어쓰고 **아무도 읽지 않음** → 즉각 반응 없음. 단 cp에 이미 전달돼 곧 종료하므로 대기는 유계 | 📄 `lib.rs::run_managed` |
| A6 | **passthrough에서 시그널** | 핸들러를 아예 등록하지 않으며, passthrough는 exec라 시그널을 받는 것이 곧 `cp` 본체다(‑ 버전 한 줄 경로만 spawn+wait이고 거기서도 핸들러 없이 기본 동작) → `cp`와 완전 동일 | ✅ 설계상, `tests/passthrough.rs` |
| A7 | **`SIGPIPE`** | Rust 런타임이 부모에서 무시하므로 relay 실패가 `EPIPE` 에러로 표면화(패닉 아님). spawn된 자식은 `std::process::Command`가 기본 disposition을 복원하고, **exec 경로는 exec 직전에 직접 `SIG_DFL`로 복원**한다 — ignore가 exec를 넘어가면 `cp -v … \| head`에서 순정과 달라지기 때문. **exec가 실패하면 ignore를 되돌린다** — 안 그러면 cprog 자신의 `Fatal` stderr 쓰기가 broken pipe에서 SIGPIPE 사망이 되어 exit 127 계약을 깬다. **아래 테스트가 고정하는 것은 결과**(순정 `cp`와 같은 시그널 사망 / exit 127 유지)**이지 exec 직전 복원 그 자체가 아니다** — 그 한 줄은 지워도 스위트가 통과한다([F20](#f20) ①) | ✅ `tests/passthrough.rs::verbose_to_a_closed_pipe_dies_of_sigpipe_like_cp`, `tests/exit_contract.rs`(usage·failed-exec 모두 stderr가 broken pipe여도 exit code 유지) |
| A8 | **`cp`가 시그널을 처리할 수 없는 상태에서 cprog가 종료 시그널을 받음** (정지 상태이거나, 끊긴 NFS 등에서 uninterruptible I/O 중) | cp에 시그널을 보낼 때 **`SIGCONT`를 함께** 보내므로, 정지된 cp도 깨어나 시그널을 받고 죽는다 → 파이프가 닫히고 join이 끝난다. 다만 uninterruptible(D) 상태는 여전히 못 푼다 — `cp` 단독 실행과 같은 결과이며 [의도적](./process-model.md#정리-cleanup) | ✅ `lib.rs::run_managed` (#5) |

## Ctrl-Z / job control

<a id="a9"></a>

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| A9 | **Ctrl-Z (`SIGTSTP`) — footer가 떠 있을 때** | 렌더 루프가 플래그를 보고 `FooterGuard::suspend_restore()`로 footer 지움 + 커서 복원 → 그 후 `raise(SIGSTOP)`으로 실제 정지(플래그 핸들러는 유지) → 재개 시 다음 tick에서 커서 재숨김 + footer 재그림 | ✅ `lib.rs::run_managed`, `render::suspend_restore`, `tests/suspend.rs::ctrl_z_restores_terminal_before_stop_then_redraws_on_resume` |
| A10 | **Ctrl-Z 후 `bg`** (백그라운드 재개) | 재개 시점에 `tcgetpgrp != getpgrp`이면 `suppressed = true` → 이후 footer를 그리지 않음. **단방향**: 다시 `fg`로 돌아와도 그 실행에서는 꺼진 채 유지(다시 Ctrl-Z→`fg` 하면 복구) | ✅ `lib.rs::run_managed`, `tests/suspend.rs::ctrl_z_then_bg_does_not_redraw_footer_in_background`, 복구 경로는 `ctrl_z_bg_then_second_ctrl_z_fg_restores_footer` |
| A11 | **teardown(join/wait) 중 Ctrl-Z** | 렌더 루프가 끝나면 `SIGTSTP`를 `SIG_DFL`로 되돌림. 안 그러면 플래그 핸들러만 남아 정지도 진행도 못 하는 wedge가 됨 | ✅ `lib.rs::restore_default_suspend`, 유닛 `teardown_signal_disposition` |
| A12 | **Ctrl-Z 동안 `cp`도 함께 정지** | Ctrl-Z는 전경 프로세스 그룹 전체에 전달되므로 `cp`도 정지 → 복사가 실제로 멈춤. 재개하면 이어서 진행(cp의 정상 동작) | 📄 |
| A13 | **정지→재개 직후 rate/eta** | 재개 시 진행 모델의 rate 히스토리를 **비워** 실제 처리량만으로 다시 계산한다 — window가 벽시계 기준이라, 비우지 않으면 정지 구간이 "진행 0"으로 평균에 섞여 재개 후 한 window 동안 rate가 무너진다. **비우는 대상은 타이밍뿐** — 파일 정체성과 `total`은 남으므로 재개 후 같은 파일의 바가 이어진다 | ✅ 모델은 `progress.rs::reset_samples_clears_the_rate_history_but_keeps_total`, 그 모델을 실제로 비우는 샘플러 배선은 `sampler.rs::reset_rate_history_clears_the_current_files_history` (#9, #53) |
| A14 | **passthrough에서 Ctrl-Z** | 핸들러 없음 → 기본 동작으로 그룹 정지. footer가 없으므로 복구할 것도 없음 | ✅ 설계상 |

---

# B. 모드 선택 / passthrough 강제

아래 조건이 하나라도 어긋나면 passthrough이고, passthrough는 스트림 inherit + env 미변경이라
**`cp`와 바이트 동일**하다.

대부분은 `plan::decide`가 주입된 `Capabilities`로 판정한다. **두 줄은 그 앞에서 갈린다** —
[B12](#b12)(`--help`/`--version`)는 `lib.rs::dispatch`가, [B13](#b13)(스캔 실패)은 `args::inspect`의
결과가 정한다. `decide`의 입력 목록은 [`runtime-model.md`](./runtime-model.md)에 있다.

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| B1 | **`cprog a b \| less`, `\| tee`** (stdout이 파이프) | `stdout_tty=false` → passthrough | ✅ `plan.rs::stdout_not_tty_is_passthrough`, `tests/passthrough.rs::passthrough_output_is_byte_identical_to_cp` |
| B2 | **`cprog a b > log`** (리다이렉트) | 동일하게 passthrough | ✅ |
| B3 | <a id="b3"></a>**`cprog a b 2> err`** (stderr만 리다이렉트) | `stderr_tty=false` → passthrough. footer/요약이 stderr로 나가므로 stderr가 터미널이 아니면 managed를 포기한다 | ✅ `plan.rs::stderr_not_tty_is_passthrough` |
| B4 | **stdout과 stderr가 서로 다른 터미널** | `fstat`의 `(st_dev, st_ino)` 비교로 감지 → passthrough. 두 터미널에 나눠 쓰면 sole-writer 전제가 깨지기 때문 | ✅ `term::same_terminal`, `plan.rs::different_terminals_is_passthrough` |
| B5 | **`TERM` 미설정 / 빈 문자열 / `dumb`** | passthrough. 색도 함께 꺼짐 | ✅ `term::term_ok` |
| B6 | **CI 환경(`CI` 설정)** | passthrough. **`CI=`(빈 문자열)도 CI로 간주**한다 — `var_os().is_some()` 판정이라 값은 안 본다(보수적, 의도적) | ✅ `plan.rs::ci_is_passthrough` |
| B7 | **비-리눅스 / `/proc` 없음** | `cfg!(linux) && /proc/self/fd` 존재 확인 → 실패 시 passthrough | ✅ `term::proc_available`, `plan.rs::non_linux_is_passthrough`. 판정 자체(`proc_available`이 이 호스트에서 무엇을 답하는지)는 리눅스에서 관측 불가라 [F20](#f20) ③에 있다 |
| B8 | **`stdbuf`가 PATH에 없음 — 또는 있는지 알 수 없음** | passthrough. `stdbuf` 없이는 `-v`가 파이프에서 block-buffer돼 라이브 UI를 못 지키므로, 약속을 못 지키느니 깔끔히 포기. **탐침이 실패한 PATH 항목도 "거기엔 없다"로 친다**(`p.metadata()`가 `Err`면 `false`) — 탐색 권한이 없는 디렉터리는 건너뛰고 검색을 계속하며, 다른 디렉터리에 `stdbuf`가 있으면 정상적으로 managed로 간다. 유일한 사본이 읽을 수 없는 곳에 있을 때만 "없음"이 된다. "모르겠다"를 "있다"로 낙관하면 managed로 들어갔다가 라이브성을 못 지키므로 **보수적인 쪽이 맞다**. 대신 조용하다 — 사용자에겐 진행바가 안 뜨는 것으로만 보인다. **탐침은 "정규 파일"과 "실행 비트" 둘 다를 요구한다** — `stdbuf`라는 이름의 디렉터리, 또는 실행 비트가 없는 파일은 **"없음"으로 읽고 다음 PATH 항목으로 넘어간다.** 둘 중 하나만 봐도 "있다"로 치면 그 항목에서 탐색을 멈추고 managed로 들어간다. **그게 PATH의 유일한 `stdbuf`일 때** spawn이 `EACCES`로 실패하고 `Fatal::CpSpawn` → **exit 127** — 진행바가 없는 정도가 아니라 **복사가 아예 안 일어난다**(순정 `cp`라면 성공했을 실행이다). 뒤쪽 PATH에 멀쩡한 `stdbuf`가 있으면 `execvp`가 가짜 항목을 건너뛰어 실행 자체는 멀쩡하다(실측) — 그래서 탐침의 일은 "빨리 있다고 답하기"가 아니라 **탐색을 계속하기**다. [C7](#c7)은 `stdbuf`가 진짜로 있고 `cp`를 못 찾은 경우라 cp 쪽 실패가 그대로 보이지만, 이쪽은 **cprog가 만든 실패**다 | ✅ `term::stdbuf_available`, `tests/fallback.rs::missing_stdbuf_falls_back_to_passthrough`, 탐침 실패는 `unreadable_path_entry_reads_as_stdbuf_missing` (#51), 정규 파일·실행 비트 요구는 `term.rs::the_stdbuf_probe_requires_a_regular_executable_file` (#53) |
| B9 | **`cprog a b &`** (백그라운드 실행) | `tcgetpgrp(stdout) != getpgrp()` → `foreground=false` → passthrough. 백그라운드 작업이 터미널을 점거하면 안 되므로 | ✅ `term::is_foreground`, `tests/background.rs` (bug1 / #1) |
| B10 | <a id="b10"></a>**`tcgetpgrp`이 답을 못 줌** | 두 갈래로 갈리고, 섞으면 안 된다. **① `ENOTTY`**(제어터미널이 아님 — 일반 파일 등): 백그라운드임을 *증명할 수 없으므로* 관대하게 허용(`foreground=true`). 실제 백그라운드 잡은 제어터미널을 갖고 있어 B9로 정상 감지되므로 관대함이 비용을 안 치른다. **② 전경 프로세스 그룹이 아예 없음**(pgid 0 — 대표적으로 **stdout이 pty master**): 이건 "모르겠다"가 아니라 **확정적으로 전경이 아니다** → `foreground=false` → passthrough. master에 footer를 쓰면 화면이 아니라 **slave 쪽 입력으로 주입**되어, 거기서 읽는 프로그램에 키 입력처럼 도착한다. `libc::tcgetpgrp`은 0을 그대로 돌려줘 pgrp 비교가 실패하는 방식으로 *우연히* 맞았고, `rustix`는 같은 상황을 `OPNOTSUPP`으로 보고하므로 **모든 에러를 ①로 뭉뚱그리면 실제 버그가 된다**(#42에서 실측·차단) | ✅ `term::is_foreground`, `term.rs::a_non_terminal_fd_is_treated_as_foreground`(①), `tests/background.rs::a_pty_master_on_stdout_is_not_a_foreground_terminal`(②) (#47, #42) |
| B11 | **`-i` / `--interactive` / `--interactive=…`** | passthrough 강제. 캡처하면 덮어쓰기 프롬프트가 깨지기 때문 | ✅ `args::inspect`, `plan.rs::interactive_forces_passthrough` |
| B12 | <a id="b12"></a>**`--help` / `--version`** | `informational` → passthrough. 복사가 없으니 감시할 것도, 요약할 것도 없음. **판정은 `plan::decide`가 아니라 `lib.rs::dispatch`에서** 한다(B 표 머리말의 조건 목록에 없는 이유) | ✅ `args.rs::help_and_version_are_informational`, PTY 절반은 `tests/managed.rs::help_over_pty_passes_through_but_names_cprog`, **비-TTY 절반은 `tests/passthrough.rs::informational_output_stays_byte_identical_when_not_a_terminal`**(testing.md D8이 요구하는 양쪽) |
| B13 | <a id="b13"></a>**인자 스캔 자체가 실패** (예: `--suffix` 값 누락) | `ArgError::Scan` → **보수적으로 passthrough**(cp가 알아서 에러를 냄). 이 폴백은 **cp도 거부할 인자에만** 걸려야 한다 — B13a 참조 | ✅ `args.rs::missing_required_value_is_scan_error`, 폴백 배선은 `tests/passthrough.rs::scan_error_falls_back_to_passthrough_byte_identical` |
| B13a | **`=값`이 붙은 long 옵션** (`--preserve=all`, `--reflink=auto`, `--sparse=`, `--backup=`, `--no-preserve=`, `--update=`, `--context=`) | **정상 managed.** cp가 받아들이는 인자이므로 B13의 폴백이 걸리면 안 된다. 스캔은 인식하지 못한 long 옵션의 attached value를 삼켜서 통과시킨다 — short에는 적용 불가(`-av`의 `optional_value()`가 번들 나머지 `v`를 값으로 먹어 `-v` 검출이 사라진다) | ✅ `args.rs::a_long_option_with_an_attached_value_does_not_fail_the_scan`, `tests/managed.rs::preserve_all_still_gets_the_managed_tui` (#30) |
| B14 | **`sudo cprog` / setuid `cp`** | `stdbuf`는 `LD_PRELOAD` 기반이라 setuid 바이너리에선 무시됨. cp가 setuid인 극히 드문 환경에서는 라이브성이 degrade | 📄 `capture-and-verbose.md` |
| B15 | **모드는 실행 전에 한 번만 결정** | 실행 중 파이프/TTY 상태가 바뀌어도 모드는 안 바뀐다(단순화). 유일한 런타임 재확인은 A10의 전경 여부 | 📄 `runtime-model.md` |
| B16 | **`CPROG_PASSTHROUGH` 설정** (값 무관 — B6·F10과 같은 규칙, 빈 문자열 포함) | 무조건 passthrough. `--help`/`--version`의 cprog 버전 한 줄(B12/#15)까지 억제되고, passthrough는 exec라 화면에도 프로세스 목록에도 cprog의 흔적이 없다 | ✅ `plan.rs::forced_passthrough_wins_over_everything`, `tests/forced_passthrough.rs` |

---

# C. `cp` 프로세스 생명주기

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| C1 | **`cp`가 PATH에 없음 / 실행 불가** | `Fatal::CpSpawn` → stderr 한 줄 + **exit 127**(셸 관례) | ✅ `messages::Fatal`, `messages.rs::cp_spawn_fatal`, 실바이너리 `tests/exit_contract.rs::missing_cp_is_fatal_cpspawn_exit_127` |
| C2 | **`cp`가 비-0으로 종료** (권한 없음, ENOSPC, `-r` 없이 디렉터리) | cp의 에러를 로그 영역에 relay → footer 지움 → **중립 문구** `✗ cp exited n - T elapsed`(진행바가 떴을 때만) → exit code 그대로 | ✅ 역할이 갈린다. **문구와 게이트**는 유닛이 고정한다 — `messages.rs::failure_summary_states_exit_code_and_elapsed`(`✗ cp exited 1 - 00:03 elapsed` 전문)와 `summary_glyphs_fall_back_to_ascii_without_unicode`(`[!]` 폴백). **exit code와 relay**는 통합이 고정한다 — `tests/managed.rs::managed_relays_cp_error_and_preserves_exit_code`, `tests/passthrough.rs::preserves_nonzero_exit_on_cp_failure`. **통합 쪽은 `✗` 문구를 관측하지 않는다** — cp가 즉시 실패해 바가 뜬 적이 없으므로 앞의 테스트는 오히려 `✗`의 *부재*를 단정한다(그것이 "진행바가 떴을 때만" 절의 근거다, #69 D) |
| C3 | **`wait()` 실패** (예: `ECHILD`) | `Fatal::CpWait { pid, source }` → exit 1 | ✅ `messages.rs::cp_wait_fatal` |
| C4 | **`cprog`가 먼저 죽음** | `PR_SET_PDEATHSIG(SIGTERM)`으로 `cp`가 고아로 남지 않음. `pre_exec` 실패는 삼킴(복사를 막을 이유가 아님). **spawn 경로에만 해당** — exec된 passthrough는 cprog가 곧 `cp`라 고아 문제가 없고, PDEATHSIG를 걸지도 않는다(걸면 `cp`의 수명이 셸에 묶여 순정과 달라짐) | ✅ `process::spawn`, `process::exec_replace`, `tests/signals.rs::killing_cprog_outright_takes_cp_with_it` (#47) |
| C5 | **C4에서 부분 복사된 대상 파일** | `cp`는 SIGTERM에 정리를 하지 않으므로 **잘린 대상 파일이 남는다.** 이는 `cp`를 직접 죽였을 때와 동일한 결과 — 의미론 보존 | 📄 |
| C6 | **PID 재사용 레이스** | `stdbuf`가 `cp`를 `exec`하므로 PID는 그대로 `cp`. 샘플러 join **이후에야** `wait()`로 reap하므로 샘플링 중 pid는 예약 상태 → 오염된 샘플이 불가능 | ✅ `process-model.md`; `tests/managed.rs`가 간접 증명(testing.md D7 — footer가 떴다는 것 자체가 spawn된 pid로 `cp`의 fd를 읽었다는 뜻) |
| C7 | <a id="c7"></a>**`stdbuf`는 있는데 `cp`를 못 찾음** | `stdbuf`가 exec에 실패해 자체 에러 + 127로 종료. cprog 입장에선 **spawn은 성공**했으므로 `Fatal::CpSpawn`이 아니라 "cp가 127로 종료"로 보인다(메시지는 relay되어 화면에 보임). C1과 대칭이 아닌 지점 — 여기서 cprog 이름의 에러를 지어내면 래퍼 탓이 아닌 실패를 래퍼 탓처럼 보이게 한다 | ✅ `tests/fallback.rs::stdbuf_present_but_cp_missing_surfaces_as_cp_exiting_127` (#47) |
| C8 | **자식은 하나뿐** | 외부 progress 도구도 hidden PTY도 없으므로 누수될 helper 프로세스 자체가 없다 | 📄 |

---

# D. 캡처 / relay / 버퍼링

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| D1 | **파이프에서 `cp`가 block-buffer** | `stdbuf -oL`로 라인버퍼 강제 → `-v`가 파일마다 실시간 도착. 진짜 `cp`로 통합 검증(가짜 cp는 flush를 제어할 수 있어 이 버그를 못 잡음) | ✅ `tests/managed.rs::managed_verbose_lines_interleave_with_footer_during_copy` |
| D2 | **개행 없는 꼬리 바이트** | 받는 즉시 relay. 개행을 기다리며 붙잡지 않는다 | ✅ `capture.rs::relays_partial_line_without_waiting_for_newline` |
| D3 | **`-v` 줄이 read 청크 경계에 걸침** | `\n`이 도착하는 청크에서 **정확히 한 번** 펄스. 상태는 필요 없다 — 종결 `\n`은 오직 한 청크에만 들어가므로, 청크별 `\n` 개수를 세는 것만으로 걸친 줄이 한 번만 펄스한다(예전 서술은 "부분 줄을 pending으로 둔다"였는데, 그 플래그는 펄스 수에 관여하지 않았다 — #69 D에서 플래그와 함께 삭제). 뒤집으면 같은 규칙이다 — **줄을 완성하지 못한 read는 펄스를 내지 않는다**(펄스 = 새 항목이므로, 파편에서 항목 시계가 시작되면 안 된다) | ✅ 파서는 `verbose::completed_lines`, `verbose.rs::newline_split_across_chunks_pulses_when_completed`, 캡처 쪽 배선은 `capture.rs::a_chunk_without_a_line_boundary_does_not_pulse` (#53) |
| D4 | **파일명에 개행·NUL·제어문자·ANSI** | `-v` 내용을 파싱하지 않으므로 로직 무영향(`\n`만 세고, 그마저도 "펄스가 하나 더" 수준). 경로는 `/proc` readlink에서 얻음 | ✅ `verbose.rs::arbitrary_bytes_only_newlines_count` |
| D5 | **사용자가 이미 `-v`를 줌** | 이중 주입 안 함(`-v` 하나만) | ✅ `process.rs::managed_does_not_double_inject_verbose` |
| D6 | **stdout/stderr 인터리브 순서** | 파이프 둘을 각각 중계하므로 상대 순서가 순정 `cp`와 미세하게 다를 수 있음 | 📄 `capture-and-verbose.md` |
| D7 | **relay 쓰기 실패**(EPIPE 등) | 무음 best-effort. exit code에 영향 없음 | ✅ `render.rs::io_failure_is_returned_and_drop_never_panics`. `tests/exit_contract.rs`는 **relay가 아니라 cprog 자신의 `Fatal` 쓰기**가 broken pipe여도 exit code가 유지되는 것을 본다(managed에 들어가지도 않는다) — 같은 성질의 다른 경로이므로 함께 적되 구분한다 |
| D8 | **reader가 read 에러** | 루프 종료(EOF와 동일 취급) → 채널 닫힘 → 메인 루프도 정리 단계로. **단 `EINTR`(`ErrorKind::Interrupted`)은 예외로 재시도한다** — 시그널에 끊긴 read는 스트림의 끝이 아니므로, EOF로 접으면 `cp`가 아직 낼 로그·에러가 화면에 닿지 못한다(D10과 같은 종류의 로그 유실). signal-hook이 기본으로 `SA_RESTART`를 걸어 실제로는 거의 안 일어나지만, 로그 무결성이 의존 크레이트의 기본값에 매달리지 않도록 명시한다 | ✅ 재시도는 `capture.rs::an_interrupted_read_does_not_end_the_relay` (#32), **그 외 에러가 루프를 끝내는 쪽은 두 리더 각각** — `a_real_read_error_still_ends_the_relay`(stderr)와 `a_real_read_error_ends_the_stdout_relay_too`(stdout, #53). 한쪽만 있으면 다른 리더의 가드를 "전부 재시도"로 넓혀도 스위트가 그대로 통과한다(실측) |
| D9 | **대량 소파일로 `-v` 폭주** | 채널이 **경계 있는 `sync_channel`** 이라 큐가 차면 리더가 대기하고 → 파이프가 차고 → `cp`가 잠시 기다린다(백프레셔 복원). 렌더 루프는 tick마다 큐를 드레인해 한 번에 쓴다(‑ unbounded 큐라면 터미널이 느릴 때 못 그린 로그가 메모리에 쌓인다) | ✅ `capture.rs::a_full_queue_makes_the_relay_wait_rather_than_buffer` (#8) |
| D10 | <a id="d10"></a>**footer가 떠 있는 동안 도착한 여러 조각짜리 메시지가 화면에서 유실** | 터미널에 **개행으로 끝나지 않은 줄이 남아 있는 동안 footer를 보류**하고, 다음 개행에서 다시 그린다 → 여러 조각으로 오는 `cp` 에러가 온전히 남는다(glibc `error()`는 한 줄을 write 4회로 내므로, 보류하지 않으면 개행을 품은 마지막 조각만 살아남는다) | ✅ `render::line_pending` — 규칙을 실제로 잡는 것은 `render.rs`의 유닛 다섯 — `partial_log_line_withholds_the_footer`, `footer_returns_once_the_line_is_finished`, `split_cp_error_survives_on_screen`, `tick_redraw_is_suppressed_while_a_line_is_pending`, `line_pending_follows_the_last_chunk_of_a_batch` (#4). `tests/log_integrity.rs`는 **end-to-end 스모크**다 — 조각남을 주문할 수 없어서(이 커널에서 glibc의 4회 write가 한 덩어리로 도착한다) 보류 규칙을 꺼도 통과한다. 실제로 조각을 강제하면(stderr relay를 8바이트씩 20ms 간격으로) 같은 뮤테이션에서 빨간불이 나므로 눈이 먼 게 아니라 **시나리오가 안 일어나는 것**이고, 그래서 ✅의 무게는 유닛 쪽에 둔다 (#59) |
| D11 | **느린 파일이 끝난 뒤 footer가 잠시 낡은 값을 보여줌** | tick 결과를 `Sample`/`Skip`/`Idle` 셋으로 구분해, **잴 게 없으면(`Idle`) 바를 내리고** 읽기가 실패했을 때만(`Skip`) 마지막 값을 유지한다(‑ 뭉뚱그리면 끝난 파일의 바가 정지된 채 남는다) | ✅ `sampler.rs::finished_file_reports_idle_not_skip` (#7) |

---

# E. 진행 계산 (`/proc` + `stat`)

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| E1 | **`copy_file_range`로 `fdinfo:pos`가 0** | 애초에 `pos`를 안 읽는다. 대상의 `st_size`를 읽으므로 coreutils 9.x에서도 정확 | ✅ 설계 고정, `progress-model.md` |
| E2 | **`fallocate` 선할당으로 `st_size`가 즉시 full** | 바가 즉시 100%가 된다 — **의도적으로 수용한 한계**다. GNU `cp`엔 선할당 경로가 없어 도달할 수 없고, 이를 막으려 blocks로 재면 sparse·압축·지연할당이라는 **흔한** 경우가 모두 틀어진다(#12) | 📄 `sampler.rs::a_preallocated_destination_reads_complete_immediately` |
| E3 | **reflink / CoW로 즉시 완료** | 첫 샘플 전에 끝나거나 100%로 점프. 정확하지만 점진적이지 않음 | 📄 |
| E4 | <a id="e4"></a>**sparse 파일 / hole이 있는 원본** | `done`·`total` 둘 다 `st_size`라 hole이 많아도 **비율이 정확**하고 100%에 도달한다. 실제로 쓰인 바이트가 아니라 논리적 진행을 세므로 hole 구간에서 rate만 높게 보인다 | ✅ `sampler::FileStat::copied_bytes` (#3, #12) |
| E5 | **빈 원본(total = 0)** | `percent_of`가 `Some(100.0)` — 0 나누기 없음 | ✅ `progress.rs::percent_empty_source_is_complete_not_divide_by_zero` |
| E6 | **`done > total` 오버슈트** | 100으로 clamp | ✅ `progress.rs::percent_overshoot_clamps_to_100` |
| E7 | **두 샘플 간 증가가 0이거나 음수** | rate는 정확히 `0.0`(음수 delta는 saturating으로 0), eta는 `None`(`--:--`) | ✅ `progress.rs::rate_zero_when_no_increase`/`rate_zero_when_negative_increase` |
| E8 | <a id="e8"></a>**현재 대상이 `/proc`에 없음** (파일 사이, 디렉터리 생성 중, hardlink/symlink 생성) | write fd가 없으므로 `select_current`가 `None` → 바 없음 | ✅ `proc.rs::no_write_fd_means_no_current_file` |
| E9 | **원본이 특수파일**(fifo/device) | `RegularRead`가 아니므로 `total = None` → indeterminate(가짜 100% 금지) | ✅ `proc.rs::special_source_gives_indeterminate_total` |
| E10 | **`/proc`/`stat` 읽기 실패** (fd 닫힘, pid 종료, 권한, hidepid) | 그 tick만 skip하고 마지막 값 유지. 크래시 없음 | ✅ `sampler.rs::dest_stat_error_skips_tick_and_keeps_model`, `proc_error_skips_tick` |
| E11 | **stdio가 정규 파일로 리다이렉트**(`cprog a b < in`) | `fd > 2`만 후보로 삼아 stdio를 복사 대상·원본으로 오인하지 않음. **판정은 종류가 아니라 번호로 한다.** 실제로 닿는 것은 **fd 0 하나뿐이다** — managed는 cp의 stdout/stderr를 언제나 파이프로 캡처하므로 fd 1·2는 정규 파일일 수 없고(`2> err`은 stderr가 tty가 아니라 [B3](#b3)으로 passthrough다), 상속되는 stdin만 정규 파일일 수 있다. 번호 규칙이 없으면 그 fd 0이 *읽기 후보*로 들어가, 대상보다 작은 유일한 읽기 fd일 때 원본으로 뽑혀 `total`이 stdin이 가리키는 파일 크기가 된다. fd 1·2까지 배제하는 절반은 그래서 **닿지 않는 방어**이고, 그럼에도 종류가 아니라 번호로 적어두는 이유는 캡처 방식이 바뀌어도 규칙이 남기 때문이다 | ✅ `proc.rs::redirected_low_fds_are_not_selected` — 닿는 절반(fd 0이 원본이 되지 않음)과 번호 경계 양쪽 (#53) |
| E12 | **rate/eta가 아직 미지** | 샘플 2개 미만이면 `None` → `(-- MiB/s)` / `⏳ --:--`. 엉뚱한 숫자를 지어내지 않음 | ✅ `progress.rs::rate_unknown_before_two_samples` |
| E13 | **cp가 다음 파일로 넘어감** | 대상 경로가 바뀌면 새 모델 + 새 `total`로 리셋 | ✅ `sampler.rs::new_file_resets_total` |
| E14 | **`cp`의 기본 `--sparse=auto`가 만든 대상** | sparse 대상에서도 `st_size`로 재므로 비율이 정상적으로 100%에 도달한다 | ✅ `sampler.rs::sparse_destination_progress_reaches_completion` (#3, #12) |
| E15 | **압축/inline 파일시스템**(btrfs `compress`, ZFS) 또는 `st_blocks`를 0으로 보고하는 FS | `st_blocks`를 아예 보지 않으므로 영향받지 않는다 | ✅ `sampler.rs::copied_bytes_are_the_logical_size` — 측정 기준이 하나뿐이라 이 세 상황(E15·E16·E22)에 **분기가 없다**. 하나의 테스트가 셋을 함께 말한다 (#3, #12) |
| E16 | **`total`과 `done`의 측정 기준 비대칭** | `total`(원본)과 `done`(대상) 모두 **`st_size`** 로 재므로 기준이 같아 비대칭이 없다 | ✅ `sampler.rs::copied_bytes_are_the_logical_size`, `sampler.rs::progress_rises_to_complete`(양쪽을 같은 기준으로 읽어 100%에 닿는 것) (#3, #12) |
| E17 | **상속된 fd가 대상으로 오인됨** | 쓰기 후보가 여럿이면 **틱 사이에 크기가 자라는 fd**를 고른다 → 셸이 물려준 fd(`3>/tmp/log`)는 자라지 않으므로 배제된다 | ✅ `sampler::choose_dest` (#6) |
| E23 | **성장 비교용 크기 기록의 수명** | 이전 크기 기록은 **이번 틱의 후보만** 남긴다. 후보 수는 항상 한 자릿수(실제 대상 + 셸이 물려준 것 몇 개)이므로 기록도 그만큼으로 유계다. 복사한 파일 수에 비례해 쌓이면 "cprog의 메모리는 파일 개수와 무관"이라는 성질이 깨진다 — 후보가 계속 2개 이상이면 사이에 후보 없는 틱([E8](#e8) → `Tick::Idle`)이 안 끼어 정리 기회가 없기 때문 | ✅ `sampler.rs::candidate_tracking_does_not_grow_with_the_number_of_files` (#33). 기록을 **비우는** 두 경로는 따로 고정한다 — `a_single_candidate_phase_forgets_the_sizes_it_passed_through`(후보 1개 fast path)와 `an_idle_phase_forgets_the_sizes_it_passed_through`(Idle). 둘 중 하나라도 빠지면 그 경로 앞뒤의 크기가 서로 빼져, 사이에 커진 decoy가 최대 증가분으로 뽑힌다(#69) |
| E24 | **쓰기 후보 둘이 똑같이 자람** | 성장 비교가 **배타적**(`>`)이라 **열거 순서상 먼저 온 후보가 이긴다**. 이건 "추적 중이던 파일을 유지한다"가 **아니다** — `choose_dest`는 아무도 안 자랐을 때만 `current`를 보므로, 바가 두 번째 후보를 따라가던 중에 동점이 나면 **바가 첫 번째 후보로 옮겨간다**(실측). 동점에는 성장으로 둘을 가를 정보가 없고, fd 순서는 오히려 약한 반대 신호다 — 상속된 decoy가 먼저 오는 쪽이기 때문(E17). 즉 이 tie-break는 **자의적이고, 자의적인 채로 받아들인 것**이다. `>=`로 바꿔도 결정성은 똑같이 유지되므로(후보 순서가 안정적이라서) "결정성 때문"이라는 근거는 성립하지 않는다. 적어두는 이유는 최적이라서가 아니라 **어느 쪽인지가 기록돼 있어야 하기 때문**이다. 후보 순서는 `/proc/<pid>/fd` 열거 순서이고, 리눅스 procfs는 fd 번호 **오름차순**으로 dirent를 낸다(실측 — `read_dir`이 정렬하지는 않으므로 보장이 아니라 관찰이다) | ✅ `sampler.rs::equal_growth_keeps_the_candidate_seen_first`, 추적 중인 파일을 뺏는 쪽은 `sampler.rs::a_tie_overrides_the_file_already_being_tracked` (#53) |
| E21 | **상속된 *읽기* fd가 원본으로 오인됨** | 원본을 **고른 대상 fd보다 작은 읽기 fd 중 가장 큰 것**으로 짝짓는다(`cp`는 원본을 열고 곧바로 대상을 연다) → 상속된 읽기 fd와 대상 이후에 열린 fd가 함께 배제된다 | ✅ `proc::source_for` (#11) |
| E22 | **ext4 delayed allocation** (writeback 전 `blocks*512 < size`) | 측정 기준 판정 분기가 **없다** — 언제나 `st_size`로 재므로 지연 할당 상태와 무관하게 정확하다 | ✅ `sampler.rs::copied_bytes_are_the_logical_size`, 실제 sparse 파일로는 `sampler.rs::a_real_sparse_file_reports_its_full_logical_size` (#12) |
| E18 | **삭제된 대상**(`readlink`가 `… (deleted)`) | 그 경로의 `stat`이 실패 → tick skip → 마지막 값 유지 | ✅ E10과 **문자 그대로 같은 분기**라 E10의 테스트가 곧 이것이다(`sampler.rs::dest_stat_error_skips_tick_and_keeps_model`). 이유만 다르고 코드가 같은 것에 테스트를 따로 두면 커버리지가 아니라 중복이다 |
| E19 | **아주 빠른 파일** | 첫 샘플 전에 끝남 → 바 없이 지나감(의도된 동작) | 📄 |
| E20 | **샘플링 비용** | 느린 파일일 때만, tick(기본 100ms)마다 `/proc/<pid>/fd` 열거(항목마다 readlink + 종류·모드 조회) + `stat`. **횟수는 쓰기 후보 수에 달렸다** — 후보가 하나인 흔한 경우는 대상 1회 + (파일이 바뀌었을 때만) 원본 1회 = **1~2회**지만, 셸이 물려준 write fd 때문에 후보가 둘이면 `choose_dest`가 후보마다 한 번씩 더 재므로 **3~4회**가 된다([E17](#e17)·[E23](#e23)의 decoy 상황, #69 D). 파일 **데이터는 읽지 않아** 페이지 캐시를 오염시키지 않음 | 📄 |

---

# F. 터미널 / 렌더

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| F1 | **바 도중 리사이즈(SIGWINCH)** | 플래그 latch → 다음 tick에 `TIOCGWINSZ` 재조회 → 재배치 | ✅ `term::should_requery_size`, `resize.rs::sigwinch_relayouts_the_footer_to_the_new_width`. 후자는 재배치의 **지연**을 잰다 — "좁은 footer가 결국 나왔다"는 F2의 1초 폴백으로도 충족되므로, 폴백 시계를 먼저 0으로 맞춰 놓고 신호 후 400ms 안에 재배치되는 것을 본다(#69) |
| F2 | **SIGWINCH 유실/합쳐짐** | 1초 폴백 재조회가 있어 낡은 크기로 고정되지 않음 | ✅ `term.rs::resize_requery_rule` |
| F3 | **터미널 높이 < 4행** | footer 억제(`rows < MIN_LOG_ROWS + FOOTER_ROWS`) — footer 2행 위에 로그 영역 2행을 항상 남긴다 | ✅ `ui::render_footer`(testing.md C3) |
| F4 | **좁은 폭** | `eta → rate → size → bar → percent` 순으로 필드를 버림. 바는 양자화 값(`100/50/20/10`) 중 들어가는 최대치로 줄고, 10칸도 못 넣으면 버림. **마지막(바 없는) 단계의 경계는 배타적** — 판정이 `고정폭 + 구분자 > cols`라 **폭에 정확히 들어맞는 배치는 버리지 않는다**([ui.md 예제 6](./ui.md)). 바가 있는 단계는 이 비교를 아예 안 쓴다(`checked_sub` + `bar_cells`로 판정) — 고정폭이 폭을 정확히 채우면 바에 남는 칸이 0이라 **그 단계가 통째로 버려지고 다음 단계로 내려간다.** 다음 단계가 버리는 건 바가 아니라 eta·rate·size이므로 **대개 바는 살아남는다**(실측: 폭 46/36/23에서 바 유지, 바가 실제로 사라지는 건 마지막 `바+percent` 단계가 폭 10에 정확히 찰 때뿐) | ✅ `ui.rs` ATTEMPTS, 경계는 `ui.rs::a_layout_that_exactly_fits_is_not_shed` (#53) |
| F5 | **극단적으로 좁은 폭**(percent도 안 들어감) | 최후 수단으로 percent만 출력하며 **오버플로우를 허용**. 터미널이 줄바꿈하면 footer가 2행을 차지해 한 줄만 지우는 erase로는 잔상이 남을 수 있음 | 📄 `ui::render_footer` 주석 |
| F6 | **렌더 중 panic** | `FooterGuard::Drop`이 unwind 중에도 footer 지우고 커서 복원 | ✅ `render.rs::drop_erases_even_on_panic` |
| F7 | <a id="f7"></a>**`SIGKILL` / `SIGSEGV`** | 핸들러도 `Drop`도 못 돈다 → **footer 잔상 + 커서가 숨겨진 채로 터미널이 남는다.** `PDEATHSIG`로 cp는 정리되지만 화면은 사용자가 `tput cnorm` / `reset`으로 복구해야 함 | 📄 회피 불가 — `usage.md`의 `tput cnorm` 안내 (#10) |
| F8 | **렌더/IO 실패** | best-effort. exit code 불변 | ✅ `render.rs::io_failure_is_returned_and_drop_never_panics` + `drop_never_panics_when_the_erase_itself_fails`(실패가 `Err`로 표면화되고 `Drop`이 panic하지 않는 것 — 앞은 footer가 올라가기 전, 뒤는 올라간 뒤의 `erase()`, #69)**과** `tests/exit_contract.rs`(그 실패가 exit code를 안 바꾸는 것). 앞의 테스트만으로는 exit code를 한 번도 안 보므로 이 행의 절반만 근거가 된다 — D7과 같은 짝이다 |
| F9 | **로그 바이트 도착** | footer 지움 → 로그 씀 → footer 재그림(erase-redraw) | ✅ `render.rs::write_log_erases_then_writes_then_redraws` |
| F10 | **`NO_COLOR` 설정** | 값과 무관하게 색 끔 | ✅ `term::color_from` |
| F11 | **비-UTF-8 로케일** | 글리프를 쓰는 **모든 자리**가 함께 폴백한다: 바 `[###---]`, `⏳` 제거, 말줄임표 `...`, 종료 요약 `[ok]`/`[!]`, 버전 한 줄의 구분자 `-`. 하나라도 빠지면 "이 터미널은 UTF-8을 못 그린다"고 판정해놓고 UTF-8을 내보내는 셈이 된다. 버전 한 줄은 footer를 배치하지 않는 passthrough 경로라 특히 빠뜨리기 쉽다 | ✅ `term::unicode_from`, `ui.rs::the_ellipsis_follows_the_glyph_style`, `messages.rs::summary_glyphs_fall_back_to_ascii_without_unicode`, `messages.rs::the_version_notice_separator_falls_back_to_ascii_too`, 경로 전체는 `tests/managed.rs::a_c_locale_copy_emits_nothing_outside_ascii`·`the_version_notice_is_ascii_on_a_c_locale_terminal` (#31) |
| F14 | <a id="f14"></a>**크기 조회 실패 / 폭 0** (`TIOCGWINSZ`가 에러이거나 `ws_col == 0` — 크기가 설정된 적 없는 pty가 대표적) | **80×24로 배치한다.** 초기값이 그대로 남고, 조회가 한 번이라도 성공하면 그 값으로 갱신된 뒤 유지된다. 리사이즈를 한 번 건너뛰는 게 아니라 **실행 내내 80칸 기준**이라는 점이 중요하다. 터미널이 실제로 더 좁으면 [ui.md 불변식 7](./ui.md)이 깨진다(줄 접힘 → 2행 erase 어긋남). 폭을 알 수 없을 때 footer를 아예 포기하는 선택지도 있었지만, 크기를 못 주는 터미널은 드물고 그때마다 진행바를 버리는 대가가 더 크다고 봤다 | ✅ `term::terminal_size`, `tests/resize.rs::an_unsized_terminal_is_laid_out_as_eighty_columns` (#50, #51) |
| F15 | <a id="f15"></a>**도달 불가 방어값 모음** | 아래는 전부 코드에 갈래가 있지만 실행 중 도달할 수 없다. 테스트를 만들어 붙이는 대신 의도적임을 여기 적는다(‑ E18과 같은 판단): ① `exit.rs`의 `status.code().unwrap_or(1)` — `Child::wait`은 signaled도 exited도 아닌 상태를 내지 않는다. ② `term::dev_ino`의 `Err` → `same_terminal_fds=false` — 유효한 `BorrowedFd`에 `fstat`은 실패하지 않는다(B4는 비교 규칙만 다룬다). ③ `proc::access_mode`에서 `flags:`가 8진수 파싱에 실패 → 그 fd는 `Other`로 분류돼 무시(E10은 *읽기* 실패만 다룬다). ④ `/proc/<pid>/fd`에 숫자 아닌 항목 → 건너뜀 — 커널이 만들지 않는다. ⑤ `exit::finalize`의 `128 + s` 폴백 — [A1a](#a1a)의 세 갈래 모두 닿지 않는다: 종료 계열 표준 시그널은 abort로 끝나고(①), 무시·정지 계열은 정상 반환해 폴백에 닿기는 하지만 `WTERMSIG`가 그런 시그널을 지목할 수 없어 애초에 오지 못하며(①a), 실시간 시그널은 블록된 것을 만날 수 없다(**`cp`가 cprog의 마스크를 상속**하므로 cprog가 블록한 시그널은 `cp`도 블록한다 — 실측). 한 줄짜리 방어라 남겨둔다 | 📄 |
| F20 | <a id="f20"></a>**뮤테이션 생존자 판정 대장** | 전수 뮤테이션(`cargo mutants -j 3 -- --features integration`)에서 살아남지만 **테스트 공백이 아닌** 것들. 다시 파생하지 않도록 판정과 근거를 적어둔다. 모두 실측이다(변이를 걸고 통합 스위트를 돌린 결과). **① equivalent — 프로그램이 같아 관측 차이가 없다:** `ui::render_footer`의 `MIN_LOG_ROWS + FOOTER_ROWS` → `*`(둘 다 2라 `2+2 == 2*2`); `ui::name_row`의 `cols == 0 || clean.is_empty()` → `&&`(빈 이름은 폭 검사를 통과해 그대로 반환되고 — 빈 문자열이라 `String::new()`와 같은 값 — 폭 0에 비지 않은 이름은 말줄임표 자리조차 없어 `checked_sub`가 `None`을 준다. **"어긋난 갈래도 결국 `String::new()`에 닿는다"까지는 참이 아니다**: `cols == 0`인데 이름이 폭 0이면서 비어 있지 않으면(결합 문자·ZWJ만으로 된 이름) 뮤턴트는 조기 반환을 건너뛰고 `0 <= 0`을 통과해 **그 바이트를 그대로 돌려준다**. 판정이 equivalent로 남는 근거는 그 입력이 아니라 **`cols == 0`이 도달 불가**라는 것이다 — 크기를 못 읽은 터미널도 초기값 80을 유지한다([F14](#f14), #69 D)); `ui::compose`의 `fixed_width + sep_total` → `-`(이 갈래에 닿는 단계가 percent 하나뿐이라 `sep_total == 0`); `lib.rs`의 `received != 0 && …` → `||`(`Signal::from_named_raw(0)`이 `None`이라 `if let`이 kill을 건너뛴다); `render::draw`의 `shown_rows != 0` → `== 0`(footer가 없을 때 `erase()`가 no-op이라 호출해도 같다 — **같은 줄의 나머지 두 변이는 잡힌다**: `&&`→`||`와 `shown_rows != rows.len()`→`==`는 `render.rs::a_redraw_replaces_both_rows_in_place`가 죽인다); `lib.rs`의 로그 도착 갈래 `progress_shown |= footer.is_some()` → `&=`(**이후** tick의 타임아웃 갈래가 다시 세운다 — 둘은 같은 `match received`의 배타적 arm이라 한 tick에 하나만 돌므로 "같은 tick"이 아니다, #69 D. **두 자리를 함께 바꾸면 잡히므로** 한 자리씩 보는 뮤테이션에서만 생존한다). **성격이 다른 equivalent 하나** — `process::exec_replace`의 exec 직전 `SIGPIPE → SIG_DFL` 복원: 지워도 스위트가 그대로 통과한다. 우리 코드가 같아서가 아니라 **Rust std의 `CommandExt::exec`가 spawn과 같은 prologue를 돌며 SIGPIPE를 이미 리셋하기 때문**이고, 그래서 뮤턴트도 순정 `cp`처럼 SIGPIPE로 죽어 `tests/passthrough.rs::verbose_to_a_closed_pipe_dies_of_sigpipe_like_cp`가 통과한다(실측). **그래도 코드는 남긴다** — 계약을 std의 내부 동작에 맡기는 건 [B10](#b10)이 기록한 종류의 위험이다(의존 대상이 *값*으로 표현하던 것을 다음 버전이 *에러*로 표현하면 조용히 깨진다). 즉 "테스트 공백"이 아니라 "현재 std에서 equivalent"이며, A7의 근거 테스트는 결과(순정과 같은 시그널 사망)를 고정하지 메커니즘을 고정하지 않는다(#59). **② 도달 불가 — 갈래는 다르나 실행이 안 닿는다:** `ui::render_bar` ASCII 갈래의 `width < 2`(`bar_cells`가 10 이상만 준다); `proc::source_for`의 `*fd < dest_fd` → `<=`(`dest_fd`는 쓰기 fd라 `sources`에 없다). **③ 이 플랫폼에서 관측 불가:** `term::proc_available`의 `cfg!(linux) && /proc 존재` → `||`/`true`(리눅스+procfs에선 두 항이 다 참이라 구분되지 않는다. 비-리눅스나 procfs 없는 호스트라야 관측된다) | 📄 판정 기준은 [`testing.md`](./testing.md) "가드레일" (#53) |
| F16 | <a id="f16"></a>**프로덕션 코드의 `unwrap`/`expect`** | panic은 exit 101이라 **불변식 1을 깨는 유일한 방법**이다(‑ `FooterGuard::Drop`이 화면은 살리지만 exit code는 못 살린다, F6). 그래서 전부 도달 불가여야 하고 근거는 이렇다: ① `lib.rs`의 `child.stdout/stderr.take().expect(…)` — managed는 `CommandSpec::managed`가 `capture=true`로 만들어 `Stdio::piped()`를 걸므로 항상 `Some`. ② `progress.rs`의 `samples.front()/back().unwrap()` — 바로 위 `len() < 2` 가드가 통과한 뒤에만 닿는다. ③ `sampler.rs`의 `current.as_mut().expect(…)` — `is_new`가 참이면 직전에 대입했고, 거짓이면 이미 `Some`이었다는 뜻이다(`is_none_or` 판정이 그것). 새 `unwrap`/`expect`를 프로덕션에 넣을 때는 이 표에 근거를 추가한다 | 📄 |
| F17 | **로케일 변수가 전부 미설정** (`LC_ALL`·`LC_CTYPE`·`LANG` 모두 없음) | **UTF-8로 가정한다**(`unicode_from(None) = true`). POSIX 기본값은 `C`(비-UTF-8)이므로 규격대로면 ASCII 폴백이 맞지만, 로케일은 *프로그램*에게 사용자 문자셋을 알려줄 뿐 터미널의 렌더링을 정하지 않는다 — 요즘 터미널은 `LANG`과 무관하게 UTF-8을 그린다. cron·systemd·최소 컨테이너처럼 변수가 비어 있는 환경에서 ASCII로 떨어뜨리면 멀쩡한 터미널이 손해를 본다. 반대 방향(비-UTF-8 로케일이 *명시*된 경우)은 F11대로 폴백한다 | ✅ `term::unicode_from`, `term.rs::unicode_rule` |
| F18 | <a id="f18"></a>**단위 표를 넘어서는 값** (≥ 1 PiB 파일, ≥ 1 TiB/s) | 표의 **마지막 단위에서 포화**한다 — `1024.0 TiB`, `1024 GiB/s`. 단위 인덱스가 표 밖으로 걸어 나가면 인덱스 범위 초과 panic이고, panic은 exit 101이라 불변식 1을 깬다. 둘 다 닿는다. **크기** — `truncate -s 1P`가 sparse 파일을 즉시 만들고 cprog는 논리 길이로 재므로([E4](#e4)) 그대로 1 PiB다(ext4는 파일 최대 16 TiB라 안 되고, XFS·btrfs·ZFS·tmpfs에서 된다). **속도** — 하드웨어가 TiB/s를 내서가 아니라, `done`이 대상의 **논리 크기**라 sparse 복사가 hole을 건너뛰면 샘플 하나가 테라바이트를 넘길 수 있고, rate는 그 delta를 **rate window(기본 1초)** 로 나눈 값이기 때문이다(E4의 "hole 구간에서 rate만 높게 보인다"가 극단으로 간 경우). 두 함수 다 순수 함수라 확인에 파일조차 필요 없다 | ✅ `ui.rs::size_saturates_at_the_largest_unit`, `ui.rs::rate_saturates_at_the_largest_unit` (#53) |
| F12 | **같은 터미널에 다른 프로세스가 씀** | sole-writer 전제가 깨져 footer가 깨질 수 있음. cprog가 제어할 수 없는 영역 | 📄 |
| F13 | **아주 오래된 터미널이 `DECTCEM`(`?25l`)을 모름** | 커서 숨김/복원 시퀀스가 그대로 화면에 보일 수 있음. `TERM` 검사는 `dumb`만 거르고 terminfo는 쓰지 않음 | 📄 |
| F19 | <a id="f19"></a>**파일명 렌더**(제어문자·폭 초과) | `-v` 없이 실행하면 footer 1행이 대상 경로를 표시한다: 제어문자를 제거한 뒤 표시폭 기준으로 앞에서 자른다. 폭 초과는 미관이 아니라 **정확성** 문제다 — 줄이 접히면 2행 지우기의 커서 이동이 어긋난다 | ✅ `ui::name_row`, `ui.md` 불변식 7 (#20) |

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
| G8 | **비-UTF-8 인자** | `OsString`으로 다뤄 그대로 `cp`에 전달한다 — 스캔은 인자 벡터를 **재구성하지 않는다**. 플래그 검출도 정상 동작한다(벡터에 표현 불가한 바이트가 있다고 스캔을 포기하면 조용히 passthrough로 떨어진다). `cp` 자신은 그 바이트를 `$'\377'`로 이스케이프해 출력하므로, 계약은 "raw 바이트가 보인다"가 아니라 **`cp`와 바이트 동일** | ✅ `args.rs::a_non_utf8_operand_does_not_stop_the_scan`(스캔 규칙), `tests/passthrough.rs::a_non_utf8_argument_reaches_cp_unchanged`·`a_non_utf8_name_in_a_verbose_line_matches_cp_exactly`(cp와 바이트 동일) (#47) |

---

# H. 환경 / 설정

| # | 상황 | 현재 동작 | 근거 / 테스트 |
|---|---|---|---|
| H1 | **`CPROG_*_MS`가 숫자가 아님 — 또는 미설정** | 에러 없이 **조용히 기본값**으로 폴백한다(빈 값·단위 접미사·음수·소수·공백·16진수·`u64` 초과 전부, 그리고 변수가 아예 없는 경우도 같은 규칙). 조용한 이유: 경고는 곧 `cp`가 안 냈을 줄을 cprog가 내는 것이고, 결과는 타이밍 손잡이가 문서화된 기본값으로 돌아가는 것뿐이라 사용자가 조치할 게 없다. **다만 "조용히"는 값만큼 눌려 있지 않다** — `ms_or_default`에 경고를 한 줄 넣어도 스위트가 그대로 통과한다(실측). 폴백 *값*은 아래 테스트들이 고정하고, *침묵*은 설계 결정으로 남아 있다. **그 "문서화된 기본값"이 실제로 무엇인지도 규칙이다** — 100 / 100 / 125([usage.md](./usage.md) 환경 변수 표)이고, **변수마다 자기 이름의 knob에만 꽂힌다**(둘이 기본값을 공유하므로 뒤바뀌어도 값만 봐서는 티가 안 난다) | ✅ 파싱 규칙은 `lib.rs::env_knobs`(#47), 기본값과 변수↔knob 배선은 `lib.rs::unset_knobs_are_the_values_the_docs_promise`·`each_knob_reads_the_variable_that_names_it` (#55) |
| H2 | **`CPROG_SLOW_THRESHOLD_MS=0`** | 에러가 아니라 유효한 설정이다. `>`가 배타적이라 **펄스 순간 자체는 아직 느리지 않고** 그 이후가 전부 느림 → 거의 항상 footer가 뜬다. "펄스 전에는 안 뜬다"는 가드가 임계보다 우선이라 `cp`가 파일 이름을 대기도 전에 바가 뜨는 일은 없다 | ✅ `slowfile.rs::a_zero_threshold_makes_everything_slow_from_the_first_instant_after_a_pulse`, `a_zero_threshold_still_reports_nothing_before_the_first_pulse` (#47) |
| H3 | **managed의 env 변경 범위** | `QUOTING_STYLE=shell-escape` **하나뿐**. `LC_ALL=C`는 일부러 안 건다 — `-v`를 파싱하지 않으니 이득이 없고, C 로케일은 한글 등 비-ASCII 파일명을 옥타 이스케이프로 깨뜨린다 | ✅ `process.rs::managed_sets_only_quoting_style_not_locale` |
| H4 | **passthrough의 env** | **전혀 건드리지 않음** → cp의 에러 메시지 로케일까지 바이트 동일 | ✅ `process.rs::passthrough_never_touches_env`는 **`CommandSpec`의 env가 비어 있는 것**을 본다(spec 수준). 실제 exec된 프로세스의 env까지 보는 것은 `tests/passthrough.rs::informational_output_stays_byte_identical_when_not_a_terminal` — `--version` 출력에 비-ASCII 저자명이 있어 로케일 유출이 드러난다(실측). `QUOTING_STYLE` 유출은 coreutils가 `cp`에서 그 변수를 안 보므로 **어떤 테스트로도 관측 불가**다 |
| H5 | **Mutex poisoning**(스레드 패닉) | `lock_shared`가 `into_inner()`로 복구. 공유 값(슬로우 타이머·최근 샘플)엔 깨질 불변식이 없고, 여기서 죽으면 `cp`를 wait 못 해 exit code 계약이 깨지므로 | ✅ `lib::lock_shared` |
| H6 | **요약이 안 나오는 경우** | `progress_shown == false`(footer가 한 번도 안 뜸)면 요약 없음 — 즉시 끝난 복사, **그리고 권한 문제로 샘플이 계속 실패한 경우**. `--help`는 이 게이트에 닿지도 않는다: informational은 `lib.rs::dispatch`에서 passthrough로 갈라지고 `summary()`는 `run_managed` 안에서만 불린다([B12](#b12)) | ✅ 게이트는 `messages.rs::no_summary_without_progress`, 배선은 `tests/managed.rs::managed_verbose_lines_interleave_with_footer_during_copy`(요약이 난다)와 `managed_relays_cp_error_and_preserves_exit_code`(안 난다) |
