# cprog

[![CI](https://github.com/minsoft1115/cp-progress/actions/workflows/ci.yml/badge.svg)](https://github.com/minsoft1115/cp-progress/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/cp-progress.svg)](https://crates.io/crates/cp-progress)
[![docs.rs](https://img.shields.io/docsrs/cp-progress)](https://docs.rs/cp-progress)
[![license](https://img.shields.io/crates/l/cp-progress.svg)](./LICENSE)
[![MSRV](https://img.shields.io/crates/msrv/cp-progress)](https://www.rust-lang.org)

[English](README.md) | **한국어**

`cprog`는 시스템 `cp`를 감싸는 얇은 래퍼다. **리눅스 대화형 터미널에서만** per-file 진행바를
얹고(‑ 진행바 기능은 `/proc`가 있는 리눅스 전용), footer가 안전하지 않은 곳 — 파이프·리다이렉트·
CI·비-리눅스·백그라운드 작업 — 에서는 **투명하게 `cp`와 바이트 동일**하게 동작한다. 진짜 `cp`를
그대로 실행하고, **오래 걸리는 파일에 대해서만** 하단에 2줄짜리 footer를 그렸다가 끝나면 없앤다.
`cp -v` 출력은 **직접 `-v`를 줬을 때만** 흘려주고, 안 줬으면 `cp`처럼 조용히 있으면서 파일
이름을 footer에 보여준다. 외부 `progress` 명령도, hidden PTY도, 화면 스크래핑도 없다.

```
  …/backup/2026/win11-vm/win11.qcow2
  ████████████░░░░░░░░   62.33 %  0.9/1.4 GiB  (142 MiB/s)  ⏳ 00:05
```

## 무엇인가

- 진짜 복사는 `cp`가 하고, 그 의미론은 안 건드린다. `cp`의 exit code가 최종 권위.
- managed 모드에서 `-v`를 주입·캡처하되, 화면으로 흘려주는 건 **사용자가 `-v`를 직접 줬을
  때뿐**이다. 안 줬으면 `cp`가 냈을 것 외에는 아무것도 출력하지 않는다. 한 파일이 느려지면
  `/proc/<pid>/fd`로 찾아 `stat`으로 커지는 크기를 읽어 **자체 진행바**를 그리고, 그 윗줄에
  파일 이름을 보여준다.
- footer가 안전하지 않은 곳에서는 passthrough로 내려간다 — 스트림 inherit, env 미변경,
  **`cp`와 바이트 동일**. 해당 조건: stdout/stderr가 TTY가 아님(파이프·리다이렉트), 둘이 서로
  다른 터미널, `TERM` 미설정 또는 `dumb`, `CI` 설정됨, 비-리눅스, `stdbuf` 없음,
  백그라운드 작업(`cprog … &`), interactive 플래그(`-i`), `--help`/`--version`, 그리고
  `CPROG_PASSTHROUGH` 설정됨 — 명시적 탈출구로, 이때는 진짜 `cp`로 exec해 래퍼 프로세스도
  남지 않는다.

외부 진행률 도구·hidden PTY·화면 스크래핑 없이, `cp` 자신의 `-v` 타이밍과 커널의 `/proc`/`stat`
만으로 진행을 자체 계산한다.

## 탈출구

`CPROG_PASSTHROUGH`를 설정하면(값 무관) cprog가 완전히 비켜선다: 다른 어떤 조건보다 우선해
passthrough가 강제되고, `--version` 한 줄까지 아무것도 덧붙이지 않으며, 진짜 `cp`로 **exec해
프로세스 자체가 cp가 된다** — `$!`·시그널·exit code 전부 cp 본인 것이고 래퍼 프로세스도 남지
않는다.

```bash
CPROG_PASSTHROUGH=1 cp -r photos backup   # 이 복사 한 번만 순정 cp 그대로
export CPROG_PASSTHROUGH=1                # 이 셸 전체 (unset으로 복귀)
```

cprog를 아예 우회하려면 alias를 건너뛰는 `\cp`(또는 `command cp`)가 언제나 있다.

## 왜 cprog인가 — 대안들과 비교

"cp 진행바"엔 이미 답이 여럿 있다. 핵심 질문은: **여전히 진짜 `cp`인가, 그리고 진행바의 대가가
무엇인가.**

| | `progress` | `advcpmv` | `cpx` | `rsync` | **cprog** |
|---|---|---|---|---|---|
| 진짜 `cp`인가? | ✓ 진짜 cp를 밖에서 관찰 | △ GNU cp이나 패치 후 재빌드 | ✗ Rust 재구현 | ✗ 다른 도구 | ✓ 진짜 cp를 감쌈 |
| 어떻게 실행하나 | 복사마다 **명령이 하나 더**(`progress -mp $!` / `watch progress`) | `advcp -g …`(또는 alias로 `cp` 대체) | `cpx …` — 새 명령, 인자는 cp와 부분 호환 | `rsync -a --info=progress2 …` — 자체 인자 체계 | `cprog <cp 인자…>` — 인자가 cp와 동일; installer가 `cp`를 alias로 대체 |
| 설치 | 배포판 패키지 | **패치 + coreutils 재컴파일** | `cargo install` / AUR | 대개 기본 설치 | `cargo install`/한 줄 |
| 최신 coreutils 추종 | ✓ 포팅할 것이 없음 — 돌고 있는 cp를 관찰 | **구조적으로 뒤처짐** — 패치가 coreutils 릴리스마다 rebase돼야 함 | 해당없음(자체 코드) | 해당없음 | ✓ 시스템 cp에 얹혀 항상 최신 |
| cp 동작 바뀔 위험 | 없음 | 낮음 — GNU cp + 패치, 단 패치가 겨냥한 버전에 고정됨 | **재구현이라 다를 수 있음**(플래그 부분 커버, 병렬 I/O) | **rsync 의미론 다름**(후행 슬래시·속성 기본값) | 없음 — 진짜 cp를 그대로 실행 |
| 진행바 정확도 | fd seek 위치 기반 — **현대 fast path에서 못 봄**(reflink `FICLONE`, coreutils 9.x `copy_file_range`), 네트워크 마운트 약함 | 높음(프로세스 안에서 셈) | 높음(프로세스 안에서 셈) | 높음(전체 전송 기준) | 근사·per-file |

표 요약:

- **`progress`** — 진짜 cp고 포팅할 것도 없지만, 복사마다 **명령이 하나 더** 필요하고, 측정
  방식(fd seek 위치)이 현대 coreutils가 빨라지는 지점에서 정확히 꺼진다 — reflink와
  `copy_file_range` 복사는 fd 위치가 0에 머문다. 이 실패 모드가 cprog가 대상 파일의 커지는
  `st_size`를 읽는 이유다([`docs/progress-model.md`](./docs/progress-model.md)).
- **`advcpmv`** — GNU cp이긴 하나 **재컴파일 빌드라 구조적으로 뒤처짐**: 패치가 coreutils
  릴리스마다 rebase돼야 하므로, 당신의 `cp`는 배포판이 주는 버전이 아니라 패치가 겨냥한 버전에
  고정된다.
- **`cpx` / `rsync`** — 애초에 **`cp`가 아님**(밑바닥부터 Rust 재구현 / 다른 도구)이라 동작이
  다를 수 있다 — `cpx`는 cp 플래그 표면의 일부를 덮고 I/O를 병렬화하며, `rsync`는 후행 슬래시
  규칙(`src/`는 내용물을, `src`는 디렉터리째 복사)과 속성 기본값 — `-a`조차 `cp -a`가 지키는
  ACL(`-A`)·xattr(`-X`)·하드링크(`-H`)를 빼놓는다 — 이 고전적인 함정이다.
- **cprog** — **그냥 `cp`에 진행바가 얹힌 것**: 설치 가볍고, 항상 최신 시스템 cp, 동작 그대로. 유일한 대가는 **진행바가 근사치**라는 것.

**정직한 트레이드오프:** cprog의 진행은 per-file 근사치(전체 %/ETA 없음)이고 리눅스 대화형
터미널 + `stdbuf`에서만 뜬다. 진행바 정확도만 보면 `advcpmv`·`cpx`·`rsync`가 앞선다. cprog가
파는 건 **"정확히 `cp`, 마찰 없음"** 이지 화려한 바가 아니다.

## 설계 결정 (요약)

- **파일 개수는 안 센다** — "파일만" 세려면 항목마다 `stat`이 필요해 대량 소파일에서 성능이
  떨어지기 때문. `-v`는 느린 파일 타이밍(항상)과 활동표시(‑ 직접 `-v`를 준 경우만)로만 쓰고, **내용은 파싱하지 않는다.**
- 진행은 `fdinfo: pos`가 아니라 **대상 파일의 크기**로 잰다 — coreutils 9.x의
  `copy_file_range`에서는 `pos`가 복사 내내 0으로 남기 때문이다.
- 언제나 `st_size`로 재고 블록 수는 보지 않는다. **sparse 대상**(원본에 hole이 있으면 `cp`가
  기본값 `--sparse=auto`로 만든다)은 블록 수가 길이보다 훨씬 적고, 압축 파일시스템과 ext4의
  writeback 이전도 마찬가지다 — 블록으로 재면 이 셋 모두에서 바가 100%에 못 미친다.
  자세한 내용은 [`docs/progress-model.md`](./docs/progress-model.md).

## 상태

**구현됨 (test-first).** docs-first로 설계를 [`docs/`](./docs)에 확정한 뒤, 그 스펙을 TDD로
구현했다. passthrough(cp와 바이트 동일)와 managed TUI(라이브 footer) 양쪽이 동작하며, 순수
유닛 + PTY 통합 테스트로 핵심 계약(cp 결과·시그널 보존, byte-identical 폴백, 라이브 스트리밍)을
검증한다.

```bash
cargo test                          # 유닛 스위트 — 외부 도구 없이 항상 green
cargo test --features integration   # 진짜 cp/stdbuf를 쓰는 PTY 테스트까지
```

통합 테스트는 기본 `cargo test`가 외부 도구에 의존하지 않도록 **일부러 feature로 게이트**돼
있다. 변경을 신뢰하려면 둘 다 돌려야 한다.

## 설치

[crates.io](https://crates.io/crates/cp-progress)에서 설치(‑ `cprog` 바이너리 설치):

```bash
cargo install cp-progress --locked
echo "alias cp='cprog'" >> ~/.bashrc && source ~/.bashrc   # zsh면 ~/.zshrc
```

한 줄 설치(‑ 빌드 + `cp` alias 자동 설정, bash/zsh 감지):

```bash
curl -fsSL https://raw.githubusercontent.com/minsoft1115/cp-progress/main/install.sh | sh
```

- Rust(`cargo`)가 필요하다(‑ 없으면 스크립트가 [rustup](https://rustup.rs) 설치를 안내).
  edition 2024를 쓰므로 **Rust 1.85 이상**이 필요하다(‑ 구버전이면 `rustup update`).
- `~/.cargo/bin`을 PATH에 추가하고, 셸 rc(`.bashrc`/`.zshrc`)에 `alias cp='cprog'`를 넣는다.
- alias 없이 설치만: `... | CPROG_NO_ALIAS=1 sh`.

수동 설치:

```bash
cargo install --git https://github.com/minsoft1115/cp-progress --locked --force
echo "alias cp='cprog'" >> ~/.bashrc && source ~/.bashrc   # zsh면 ~/.zshrc
```

설치 후 대화형 터미널에서:

```bash
cp big.iso /mnt/backup/big.iso   # 느려지면 진행바가 뜬다
```

## 문서

- [문서 인덱스](./docs/index.md)
- [개요](./docs/overview.md) · [UI](./docs/ui.md) · [Capture & Verbose](./docs/capture-and-verbose.md)
- [Progress model](./docs/progress-model.md) · [Runtime model](./docs/runtime-model.md)
- [Architecture](./docs/architecture.md) · [Process model](./docs/process-model.md)
- [Testing](./docs/testing.md) · [Usage](./docs/usage.md) · [Dependencies](./docs/dependencies.md)
- [Performance](./docs/performance.md) — 오버헤드 실측 기준선과 재는 방법
- [Exceptions](./docs/exceptions.md) — 런타임 예외 전수(시그널·Ctrl-Z·passthrough 조건·진행
  계산 한계)와 각각에 대한 동작, 그리고 어디서 테스트되는지

## 요구사항

- 시스템 `cp` (필수 — cprog는 어디서든 이걸 감싼다)
- **진행바를 보려면:** 리눅스(‑ `/proc` 필요) + 대화형 터미널 + `stdbuf`(coreutils, `cp -v`를
  실시간으로 흘리기 위해). 이 중 하나라도 없으면 자동으로 passthrough(‑ `cp`와 바이트 동일)로
  동작하며, 그래도 복사는 정상이다.

## 조정 (환경변수)

전부 선택이고 안전한 기본값이 있다. 값이 숫자가 아니면 **조용히 기본값으로 폴백**한다.

| 변수 | 효과 |
|---|---|
| `CPROG_PASSTHROUGH` | 강제 passthrough(값 무관) — 버전 한 줄까지 cprog가 덧붙이는 모든 것 끔, 진짜 `cp`로 exec |
| `CPROG_SLOW_THRESHOLD_MS` | 한 파일이 이만큼 넘게 걸리면 바가 뜬다 (기본 100) |
| `CPROG_SAMPLE_INTERVAL_MS` | 느린 파일일 때 `stat` 폴링 주기 (기본 100) |
| `CPROG_RENDER_TICK_MS` | footer 리드로우 tick (기본 125) |
| `NO_COLOR` | footer 색 끔 (설정만 돼 있으면 값 무관) |

자세한 설명은 [`docs/usage.md`](./docs/usage.md).

## 라이선스

[MIT](./LICENSE)
