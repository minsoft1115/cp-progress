# cprog 문서

`cprog`는 시스템 `cp`를 감싸는 얇은 래퍼다. **리눅스 대화형 터미널에서만** per-file
진행바를 얹고(‑ 진행바는 `/proc`가 있는 리눅스 전용), 그 외 모든 곳에서는 **투명하게
`cp`와 바이트 동일**하게 동작한다. 진짜 `cp`를 그대로 실행하고, `cp -v` 출력을 평소처럼
위로 흘려주며, **오래 걸리는 파일에 대해서만** 하단 footer에 진행바를 그렸다가 끝나면 없앤다.

자체 진행률 계산 엔진도, 외부 `progress` 명령도, hidden PTY도 없다. 이 문서는
**docs-first**로 작성됐다 — 코드보다 설계를 먼저 확정해, 이후 test-first로 구현한다.

## 문서 목록

- [`overview.md`](./overview.md) — 목표, 확정 컨셉, non-goals
- [`ui.md`](./ui.md) — 2영역 화면과 footer 바 (예제 포함)
- [`capture-and-verbose.md`](./capture-and-verbose.md) — `-v` 주입·캡처·중계 방식,
  `-v`를 왜 파싱하지 않는지, 그리고 왜 **요청받았을 때만** 보여주는지
- [`progress-model.md`](./progress-model.md) — per-file 바를 어떻게 계산하나
  (`/proc/<pid>/fd` + `stat().st_size`)
- [`runtime-model.md`](./runtime-model.md) — managed-TUI vs passthrough 선택
- [`architecture.md`](./architecture.md) — 모듈 구성과 데이터 흐름
- [`process-model.md`](./process-model.md) — `cp` 생명주기, sole-writer 출력, 정리,
  시그널 보존 종료
- [`dependencies.md`](./dependencies.md) — 크레이트 선정(유지보수·최소주의)
- [`testing.md`](./testing.md) — test-first(TDD) 전략
- [`performance.md`](./performance.md) — 오버헤드 실측 기준선과 재는 방법(‑ 회귀 비교용)
- [`exceptions.md`](./exceptions.md) — 예외 상황 카탈로그(시그널·Ctrl-Z·passthrough 강제·
  진행 계산 한계) + 미커버 갭과 권고
- [`usage.md`](./usage.md) — 사용법과 동작

## 한 문단 요약

리눅스 대화형 터미널에서 `cprog`는 `-v`를 주입하고 `cp` 출력을 캡처한다. 그 출력을 화면에
흘려주는 것은 **사용자가 `-v`를 직접 줬을 때뿐**이고, 아니면 타이밍 판정에만 쓰고 버린다 —
요청하지 않은 출력을 쏟지 않는다. 한 파일이 짧은
임계 시간보다 오래 걸리면, `cprog`는 `/proc/<pid>/fd`로 그 파일을 찾아 `stat`으로 커지는
크기를 읽고, footer에 per-file 진행바를 그린다 — 그 파일(과 복사)이 끝나면 바는
사라진다. footer가 안전하지 않은 곳(파이프/비-TTY/CI/비-리눅스)에서는 `cp`를 그대로,
바이트 단위로 동일하게 실행한다.
