# Architecture (구조)

## 상위 흐름

```
main
 └─ run()
     1. 인자 수집
     2. validate (비어있지 않음)                → args
     3. inspect(interactive?) + capabilities 감지 → plan
     4. RunMode 결정 (ManagedTui | Passthrough)   → plan
     5a. Passthrough: cp spawn(inherit) → wait → exit
     5b. ManagedTui:  cp spawn(stdbuf -oL + -v 주입, stdout/stderr capture)
                      ├─ relay: 캡처 → 로그 영역(sole writer) + 줄경계 타이밍
                      ├─ slow-timer: 100ms 넘으면 현재 파일 "느림"
                      ├─ sampler: /proc/fd → stat(dst)/stat(src) → ProgressState
                      └─ render: ProgressState → footer 바 (느릴 때만)
                      → wait → footer 지움 → 요약 → exit
     6. cp status → ExitDisposition → finalize(시그널 보존)
```

두 규칙: **실행 전에 정책 확정**, **실행 후 `cp` 결과 보존**.

## 모듈 구성 (계획)

| 모듈 | 책임 |
|---|---|
| `main.rs` | 얇은 바이너리 진입 → `cprog::run()` |
| `lib.rs` | 상위 오케스트레이션, `run()` |
| `args.rs` | 인자 검증 + interactive/`-v` 최소 검사 |
| `plan.rs` | capabilities 감지 + `RunMode` 결정 |
| `capture.rs` | `cp` stdout/stderr 캡처 + 로그 영역 중계(sole writer) |
| `verbose.rs` | 캡처 스트림의 **줄 경계** 감지 = "새 항목" 펄스 (내용 파싱 안 함) |
| `slowfile.rs` | 펄스 + `Clock` → "현재 파일이 느린가" 판정 |
| `proc.rs` | `/proc/<pid>/fd` readlink로 현재 대상/원본 경로 |
| `sampler.rs` | `stat().st_size`(+`st_blocks` 폴백) 폴링 → `ProgressState`(rate/eta 평활화) |
| `progress.rs` | `ProgressState` 모델(현재 파일 done/total/rate/eta) |
| `ui.rs` | footer 레이아웃 + 바 렌더 + 폭 축약 |
| `render.rs` | 터미널 writer, 커서/erase 시퀀스, `FooterGuard`(RAII 화면복구) |
| `process.rs` | `cp` spawn(managed는 `stdbuf -oL` + `-v` 래핑), wait, PID 확보 |
| `messages.rs` | 요약 문자열, `Fatal` 타입 |
| `term.rs` | TTY 검사, 터미널 크기(`TIOCGWINSZ`), `SIGWINCH` 플래그 + 저빈도 폴백 재조회 |
| `exit.rs` | `ExitDisposition` → 시그널 보존 finalize |

모듈은 책임별로 분리하되, **"hidden PTY + VT 파서" 같은 서브시스템은 두지 않는다.**
`verbose.rs`(줄 펄스) + `proc.rs`(경로) + `sampler.rs`(크기)가 단순한 `ProgressState`를
채우고 `ui.rs`가 그린다.

## 핵심 타입

```rust
enum RunMode { ManagedTui, Passthrough }

struct ProgressState {
    name:  String,          // 현재 파일
    total: Option<u64>,     // 모르면 None → indeterminate 바
    done:  u64,
    rate:  Option<f64>,     // bytes/sec, 평활화; 모르면 None
    eta:   Option<Duration>,
}

enum Fatal   { Usage, CpSpawn(String), CpWait { pid: u32, source: String } }
```

## 동시성

- **메인 스레드**: `cp` spawn·wait, 렌더 루프, footer writer 소유(=유일 기록자).
- **capture 리더 스레드**(stdout/stderr): 캡처 바이트를 메인으로 넘김(중계·줄경계용).
  - 채널은 **경계 있는 `sync_channel`** 이다. 무한 큐를 쓰면 파이프가 주던 백프레셔가 사라져,
    터미널이 느릴 때(원격 ssh 등) 못 그린 로그가 메모리에 계속 쌓인다. 경계를 두면 리더가
    막히고 → 파이프가 차고 → `cp`가 잠시 기다린다(= `cp | 느린소비자`의 원래 동작).
  - 렌더 루프는 tick마다 큐를 **드레인해 한 번에 쓴다.** 청크마다 erase→write→draw를 왕복하지
    않으므로 대량 소파일에서 처리량이 오른다.
  - teardown 진입 시 **`rx`를 먼저 떨군다.** 경계 있는 채널에서는 아무도 안 읽으면 리더가 send에서
    막혀 join이 안 끝나기 때문이다(‑ rx가 사라지면 send가 즉시 실패해 리더가 빠져나온다).
- **sampler 스레드**: 느린 파일일 때 `/proc`+`stat` 폴링 → 공유 `ProgressState` 발행.

`cp`는 스트림이 캡처되지만, 진행은 `/proc`/`stat`에서 **out-of-band**로 얻는다(‑v 내용
파싱 아님).

## 에러 철학

- `Fatal`은 **반환**되어 실행을 중단(진짜 블로커: usage, `cp` spawn/wait 실패).
- relay/render/sample 등 cprog-side 비치명 실패는 **무음 best-effort**로 처리한다(‑ `let _ = ...`).
  exit code에 영향 없음. 별도 `Warning` 타입은 두지 않았다 — 실패하는 대상이 터미널 자체라 그
  터미널로 경고를 내보내는 게 신뢰할 수 없어, 타입도 방출 sink도 배선하지 않았다.
- `Fatal` 메시지와 종료 요약(summary)의 stderr 출력도 best-effort다(‑ `let _ = writeln!(…)`):
  그 쓰기가 실패해도(예: stderr가 끊긴 파이프라 `EPIPE`) panic하지 않아 `cp`의 exit code 계약을
  지킨다(`eprintln!`은 실패 시 panic한다).
- 터미널은 모든 경로(정상/`?`/panic/시그널)에서 `FooterGuard`의 `Drop`으로 복구.
