# Dependencies (크레이트 선정)

`cprog`가 쓸 크레이트를 정의한다. 선정 기준:

- **유지보수됨 · 널리 쓰임** — 다운로드 많고 활발히 관리되는 것만.
- **최소주의** — 무거운 TUI/async 프레임워크를 끌어오지 않는다. `std`로 되는 건 `std`로.
- **리눅스를 대상**으로 구현(‑ 진행바는 `/proc` 기반, 그 외 플랫폼은 passthrough로 강등)하므로
  크로스플랫폼 터미널 추상화는 불필요.

버전은 목표 major만 적는다. 확정 시 `cargo add`로 최신 patch·유지보수 상태를 확인한다.

## 런타임 필수 (core)

| crate | ≈버전 | 쓰는 곳 | 이유 |
|---|---|---|---|
| **`libc`** | 0.2 | `plan.rs`, `term.rs`, `exit.rs`, `process.rs` | 같은 터미널 판정 `fstat`(st_dev/ino), `ioctl(TIOCGWINSZ)`, signal(`sigaction`/self-raise), `prctl(PR_SET_PDEATHSIG)`. rust-lang 공식, 광범위 사용. |
| **`lexopt`** | 0.3 | `args.rs` | interactive/`-v` 감지 + 값 소비 옵션의 **최소** 검사. 단일 파일·무의존·무매크로. `cp` 인자를 재파싱하지 않는 이 프로젝트에 딱 맞음. |
| **`unicode-width`** | 0.2 | `ui.rs` | footer 줄 폭을 표시 폭 기준으로 계산(‑ wide/CJK 안전)해 좁은 터미널에서 필드를 버릴지 판단하고, footer 1행의 파일명(대상 경로)을 표시폭 기준으로 앞에서 자른다(#20). unicode-rs 관리, ripgrep 등 광범위 사용. |

이 셋이 코어다. 나머지는 선택.

## 런타임 선택 (ergonomic — 안 써도 됨)

| crate | ≈버전 | 대체 대상 | 권장 |
|---|---|---|---|
| **`signal-hook`** | 0.3 | `libc::sigaction`으로 손수 SIGWINCH 플래그 세팅 | **권장.** `signal_hook::flag::register(SIGWINCH, flag)`가 async-signal-safe 플래그를 안전·정확하게 해줌(‑ 손수 unsafe 감소). 단 **종료 시 시그널 재현(self-raise)은 여전히 `libc`** 로 한다. |
| **`terminal_size`** | 0.4 | `libc::ioctl(TIOCGWINSZ)` 직접 | 선택. 크로스플랫폼이지만 우린 리눅스라 `libc` 직접으로도 충분. 코드 깔끔함을 원하면 채택. |

> 최소를 원하면 이 둘 없이 `libc`만으로도 전부 구현된다(‑ signal·ioctl 손수). 안전성/가독성을
> 원하면 `signal-hook`만 추가하는 절충을 권장한다.

## 개발/테스트 전용 (dev-dependencies)

| crate | ≈버전 | 이유 |
|---|---|---|
| **`nix`** | 0.30 | PTY 기반 통합 테스트 하네스(`openpty`)와 `waitpid`. nix-rust 관리, Unix 크레이트 표준. **런타임엔 불필요**(‑ cprog는 PTY를 안 씀) → dev 전용. |

## `std`로 충분해서 크레이트가 필요 없는 것

- **TTY 판정** → `std::io::IsTerminal`(1.70+)
- **`/proc` 읽기** → `std::fs::read_dir` / `read_link`(‑ fd 대상 경로), `std::fs::metadata`
- **대상 파일의 `st_size`(‑ 경로 stat)** → `std::os::unix::fs::MetadataExt`
  - 단, stdout/stderr **fd**의 `st_dev`·`st_ino`로 "같은 터미널"을 판정하는 건 fd를 소유하지
    않고 fstat해야 하므로 `libc::fstat`을 쓴다(위 libc 참조).
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
libc = "0.2"
lexopt = "0.3"
unicode-width = "0.2"
signal-hook = "0.3"   # 선택(권장): SIGWINCH 플래그
# terminal_size = "0.4"  # 선택: ioctl 대신 쓰려면

[dev-dependencies]
# PTY 테스트 하네스: openpty/waitpid(term,process) + killpg(signal) + mkfifo(fs).
nix = { version = "0.30", features = ["term", "process", "signal", "fs"] }
# 통합 테스트 전용: PTY 리사이즈(TIOCSWINSZ — nix에 래퍼가 없다) 등 저수준 호출.
libc = "0.2"
```
