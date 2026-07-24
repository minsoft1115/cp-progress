# cprog

[English](README.md) | **한국어**

`cprog`는 시스템 `cp`를 감싸는 얇은 래퍼다. **리눅스 대화형 터미널에서만** per-file 진행바를
얹고(‑ 진행바 기능은 `/proc`가 있는 리눅스 전용), 그 외 모든 곳 — 파이프·비-TTY·CI·비-리눅스 —
에서는 **투명하게 `cp`와 바이트 동일**하게 동작한다. 진짜 `cp`를 그대로 실행하고, `cp -v`
출력을 위로 흘려주며, **오래 걸리는 파일에 대해서만** 하단 footer에 진행바를 그렸다가 끝나면
없앤다. 외부 `progress` 명령도, hidden PTY도, 화면 스크래핑도 없다.

```
  'a.iso' -> '/mnt/backup/a.iso'
  ████████░░░░  62.34 %  0.9/1.4 GiB  (142 MiB/s)  ⏳ 00:05
```

## 무엇인가

- 진짜 복사는 `cp`가 하고, 그 의미론은 안 건드린다. `cp`의 exit code가 최종 권위.
- managed 모드에서 `-v`를 주입·캡처해 로그를 위로 흘려주고(‑ 그 스크롤이 "살아있다"는
  신호), 한 파일이 느려지면 `/proc/<pid>/fd`로 찾아 `stat`으로 커지는 크기를 읽어 **자체
  진행바**를 그린다.
- footer가 안전하지 않은 곳(파이프/비-TTY/CI/비-리눅스)에서는 `cp`와 바이트 동일.

외부 진행률 도구·hidden PTY·화면 스크래핑 없이, `cp` 자신의 `-v` 타이밍과 커널의 `/proc`/`stat`
만으로 진행을 자체 계산한다.

## 설계 결정 (요약)

- **파일 개수는 안 센다** — "파일만" 세려면 항목마다 `stat`이 필요해 대량 소파일에서 성능이
  떨어지기 때문. `-v`는 활동표시 + 느린 파일 타이밍으로만 쓰고, **내용은 파싱하지 않는다.**
- 진행은 `fdinfo: pos`가 아니라 **대상 파일 크기(`stat().st_size`)** 로 잰다 —
  coreutils 9.x의 `copy_file_range`에서는 `pos`가 0으로 남기 때문이다.

## 상태

**구현됨 (test-first).** docs-first로 설계를 [`docs/`](./docs)에 확정한 뒤, 그 스펙을 TDD로
구현했다. passthrough(cp와 바이트 동일)와 managed TUI(라이브 footer) 양쪽이 동작하며, 순수
유닛 + PTY 통합 테스트로 핵심 계약(cp 결과·시그널 보존, byte-identical 폴백, 라이브 스트리밍)을
검증한다. `cargo test`로 전체 스위트 실행.

## 설치

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
- [Testing](./docs/testing.md) · [Usage](./docs/usage.md)

## 요구사항

- 시스템 `cp` (필수 — cprog는 어디서든 이걸 감싼다)
- **진행바를 보려면:** 리눅스(‑ `/proc` 필요) + 대화형 터미널 + `stdbuf`(coreutils, `cp -v`를
  실시간으로 흘리기 위해). 이 중 하나라도 없으면 자동으로 passthrough(‑ `cp`와 바이트 동일)로
  동작하며, 그래도 복사는 정상이다.
