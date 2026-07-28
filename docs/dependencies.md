# Dependencies (크레이트 선정)

`cprog`가 쓸 크레이트를 정의한다. 선정 기준:

- **유지보수됨 · 널리 쓰임** — 다운로드 많고 활발히 관리되는 것만.
- **최소주의** — 무거운 TUI/async 프레임워크를 끌어오지 않는다. `std`로 되는 건 `std`로.
- **리눅스를 대상**으로 구현(‑ 진행바는 `/proc` 기반, 그 외 플랫폼은 passthrough로 강등)하므로
  크로스플랫폼 터미널 추상화는 불필요.

버전은 목표 major만 적는다 — 실제 핀은 `Cargo.toml`/`Cargo.lock`이 든다.

## 런타임 필수 (core)

| crate | ≈버전 | 쓰는 곳 | 이유 |
|---|---|---|---|
| **`libc`** | 0.2 | `lib.rs`, `process.rs` | **시그널 disposition을 기본값으로 되돌리는 것 하나** — exec 전후의 `SIGPIPE`, teardown의 `SIGTSTP`. 어떤 크레이트도 이걸 안전하게 감싸주지 않는다: signal-hook의 `restore_default`는 private이고 `unregister`는 ignore로만 만들며, nix의 `signal`/`sigaction`은 그 자체가 `unsafe fn`, rustix는 `runtime` 모듈(소스에 *"only for libc-like users"*, `doc(hidden)`)에만 둔다. 시그널 번호 상수도 여기서 온다. |
| **`rustix`** | 1 | `term.rs`, `process.rs`, `lib.rs` | fd/termios/process syscall의 안전 래퍼: `fstat`·`tcgetwinsize`·`tcgetpgrp`·`getpgrp`·`set_parent_process_death_signal`·`kill_process`. **도입 이유는 편의가 아니라 가변인자 FFI 제거**다 — `ioctl`과 `prctl`은 인자 개수도 타입도 컴파일러가 검사하지 않는, 크레이트에서 유일하게 아무 검증이 없던 두 곳이었다. 부수적으로 `mem::zeroed()`한 POD(`stat`/`winsize`)를 FFI에 넘기는 패턴도 사라진다. 전부 `AsFd`를 받고 `io::stdout()`/`stderr()`가 그걸 구현하므로 `BorrowedFd::borrow_raw` 같은 우회가 필요 없다. 리눅스에서 `linux_raw` 백엔드(musl 포함)라 자기 libc를 끌고 오지 않는다 |
| **`lexopt`** | 0.3 | `args.rs` | interactive/`-v` 감지 + 값 소비 옵션의 **최소** 검사. 단일 파일·무의존·무매크로. `cp` 인자를 재파싱하지 않는 이 프로젝트에 딱 맞음. |
| **`unicode-width`** | 0.2 | `ui.rs` | footer 줄 폭을 표시 폭 기준으로 계산(‑ wide/CJK 안전)해 좁은 터미널에서 필드를 버릴지 판단하고, footer 1행의 파일명(대상 경로)을 표시폭 기준으로 앞에서 자른다(#20). unicode-rs 관리, ripgrep 등 광범위 사용. |

이 셋이 코어다. 나머지는 선택.

## 런타임 선택 (ergonomic — 안 써도 됨)

| crate | ≈버전 | 대체 대상 | 권장 |
|---|---|---|---|
| **`signal-hook`** | 0.3 | `libc::sigaction`으로 손수 SIGWINCH 플래그 세팅 **+ 손수 짠 self-raise** | **권장.** `flag::register(SIGWINCH, flag)`가 async-signal-safe 플래그를 안전·정확하게 해주고, **종료 시 시그널 재현도 `low_level::emulate_default_handler`가 같은 일(기본 disposition 복원 → unblock → raise)을 안전하게 한다** — 손수 짠 판은 zeroed `sigaction`·`sigset_t`를 손으로 채우고 4개 syscall의 반환값을 하나도 안 봤다. 자기 정지도 `low_level::raise`(safe). 이 크레이트가 못 해주는 것은 **disposition을 기본값으로 되돌리기**뿐이라(‑ `restore_default`가 private, `unregister`는 ignore로만 만듦) SIGPIPE·SIGTSTP 복원은 `libc`에 남는다. |
| **`terminal_size`** | 0.4 | `libc::ioctl(TIOCGWINSZ)` 직접 | **불필요해졌다.** `rustix::termios::tcgetwinsize`가 같은 일을 하면서 `tcgetpgrp`·`fstat`까지 덮는다. |

> `libc`만으로도 전부 구현되지만, 그러면 `ioctl`·`prctl`이 가변인자 호출로 남는다 — 컴파일러가
> 인자 개수도 타입도 안 보는 유일한 자리다. 지금 코드는 맞다(실측: gnu·musl 양쪽 빌드 통과,
> 잠재 버그 없음). 그래도 옮긴 이유는 버그 수정이 아니라 **"컴파일러가 아무것도 검증하지 않는
> 범주"를 크레이트에서 없애는 것**이고, 사용자 권한으로 사용자 파일을 다루는 도구에서 그 범주는
> 없는 편이 낫다. 대가는 크레이트 3개(rustix + bitflags + linux-raw-sys), 클린 릴리스 빌드
> +1.3초, 바이너리 +7 KB. 판단 근거 전체는 [#42](https://github.com/minsoft1115/cp-progress/issues/42).

## 개발/테스트 전용 (dev-dependencies)

| crate | ≈버전 | 이유 |
|---|---|---|
| **`nix`** | 0.30 | PTY 기반 통합 테스트 하네스(`openpty`)와 `waitpid`. nix-rust 관리, Unix 크레이트 표준. **런타임엔 불필요**(‑ cprog는 PTY를 안 씀) → dev 전용. |

## `std`로 충분해서 크레이트가 필요 없는 것

- **TTY 판정** → `std::io::IsTerminal`(1.70+)
- **`/proc` 읽기** → `std::fs::read_dir` / `read_link`(‑ fd 대상 경로), `std::fs::metadata`
- **대상 파일의 `st_size`(‑ 경로 stat)** → `std::os::unix::fs::MetadataExt`
  - 단, stdout/stderr **fd**의 `st_dev`·`st_ino`로 "같은 터미널"을 판정하는 건 경로가 아니라
    fd를 stat해야 하므로 `rustix::fs::fstat`을 쓴다 — `io::stdout()`이 `AsFd`라 안전하게 넘어간다.
- **`cp`/`stdbuf` 실행** → `std::process::Command`
- **동시성** → `std::thread`, `std::sync::mpsc`, `Mutex`, `Arc`, `AtomicBool`
- **시간** → `std::time::{Instant, Duration}`

## 의도적으로 **안 쓰는** 크레이트 (이유 명시)

| 안 씀 | 이유 |
|---|---|
| `clap` | CLI 표면을 만들지 않음. 인자는 `lexopt`로 최소 검사만 하고 `cp`로 넘김. |
| `crossterm` / `termion` / `ratatui` | 한 줄 footer + erase-redraw ANSI가 전부. 풀 TUI 프레임워크는 과함. `libc` ioctl + 직접 ANSI로 충분. |
| `procfs` | `/proc/<pid>/fd` readlink + `stat`만 필요 → `std::fs`로 충분. 파서 크레이트는 과함. |
| `tokio` / async | 스레드(`std::thread`) 몇 개로 끝. async 런타임 불필요. |
| 외부 progress/PTY 크레이트 | 진행은 `/proc`+`stat` 자체 계산. hidden PTY 없음. |

## 요약 (Cargo.toml 골자)

```toml
[dependencies]
libc = "0.2"          # 시그널 disposition 복원(SIGPIPE·SIGTSTP)과 시그널 상수 전용
lexopt = "0.3"
unicode-width = "0.2"
signal-hook = "0.3"   # SIGWINCH 플래그 + 종료 시 시그널 재현(low_level)
rustix = { version = "1", default-features = false, features = ["std", "fs", "process", "termios"] }

[dev-dependencies]
# PTY 테스트 하네스: openpty/waitpid(term,process) + killpg(signal) + mkfifo(fs).
nix = { version = "0.30", features = ["term", "process", "signal", "fs"] }
# 통합 테스트 전용: PTY 리사이즈(TIOCSWINSZ — nix에 래퍼가 없다) 등 저수준 호출.
libc = "0.2"
```
