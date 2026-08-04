# Testing (test-first / TDD)

`cprog`는 **test-first**로 만든다. 아래 각 동작은 그것을 만족하는 코드보다 **먼저 실패
테스트**로 명세된다. 계층을 이렇게 나눈 이유는 대부분 로직을 *순수*하게 유지해 빠른 유닛
테스트로 검증하고, 진짜 I/O만 통합 테스트로 남기기 위해서다.

## 순수 유닛 (빠름, I/O 없음)

전부 메모리 입력만으로 구동:

- **args** — interactive/`-v` 감지, `--` 이후 무시, 그리고 **값 소비 옵션**(`-S`, `-t`,
  `--suffix`, `--target-directory`) 뒤 인자를 플래그로 오인하지 않기. `-v` 이중 주입 방지
  판정도 여기.
- **plan** — 주입한 `Capabilities`(TTY, 같은 터미널, CI, `TERM`, `/proc` 가능, `stdbuf`
  존재, 강제 passthrough)로 `RunMode` 결정. 실제 환경 안 건드림.
- **verbose(줄 경계)** — 캡처 바이트 청크(개행이 청크 경계에 걸친 경우 포함)에서 "새 항목"
  펄스가 정확히 나오는지. 내용은 파싱 안 하므로 테스트가 단순하다.
- **slowfile** — 펄스 시퀀스 + 주입 `Clock`(가상 시간)으로 "느림/빠름" 판정이 임계를
  정확히 지키는지(임계 직전/정확/직후 경계 포함).
- **resize** — `SIGWINCH` 플래그 래치/소비(한 번만), "이벤트 or 폴백 주기 경과 시 재조회"
  판정. 순수 함수라 시그널 전달 없이 검증.
- **proc/fd 선택** — `(fd, target_path, kind)` 픽스처에서 현재 대상/원본 경로를 고르는 규칙.
  `/proc`/`stat` 읽기는 trait 뒤에 두고 픽스처 주입.
- **progress 모델** — `(t, done)` 샘플 시퀀스로 `rate`/`eta`/`percent`, "충분한 샘플 전엔
  미지수" 규칙, `total=None` indeterminate, 그리고 아래 예외들(0바이트/오버슈트/무증가).
- **ui 레이아웃** — `(TerminalSize, ProgressState)`로 footer 문자열과 폭 축약 순서를 단언.
  in-memory writer(`Vec<u8>`)에 그려 escape 시퀀스를 바이트 단위로 검증.
- **bar 양자화** — 바 폭이 `10/20/50/100` 중 들어가는 최대값으로 스냅되고(그 미만이면 바
  생략), 뒤 필드 텍스트 폭이 바뀌어도 흔들리지 않음. **재는 대상은 필드를 넘치는 rate다**
  — percent·size·rate·eta가 전부 고정폭이 됐으므로(ui.md 불변식 8) 폭이 변할 수 있는 건
  필드를 넘치는 값뿐이다. 측정 대상은 rate → eta → 넘치는 rate로 두 번 옮겼고, 옮긴 이유는
  매번 같다: **그 축을 고정폭으로 만들면 그것으로 재던 테스트가 공허해진다.**
- **파일명 줄** — 제어문자 제거 → 표시폭 기준 앞자름 → `…`/`...` 폴백. 폭 경계값, CJK(2칸),
  개행이 든 경로, 이름이 폭보다 짧을 때(자르지 않음)까지 단언한다.
- **footer 높이 2행** — `rows < 4`면 footer를 아예 그리지 않고, 지울 때 정확히 2행을 지운다.
- **render/footer** — `FooterGuard`가 `Drop`에서 지움(정상 + `catch_unwind` panic);
  footer가 `height − min_log_rows`를 안 넘음.
- **messages/exit** — 요약 포맷, `ExitDisposition` 매핑, 시그널 vs `128+n` 정책. (경고 포맷은
  없다 — 별도 `Warning` 타입을 두지 않기로 했고 방출 sink도 없다, architecture.md 참조.)

## seam 주입 (여전히 빠름)

- **`Clock`** — 가상 시간으로 slow-timer/rate 평활화를 결정론적으로.
- **`ProcSource` / `StatSource`** — 픽스처 fd 목록/크기를 반환해 실제 프로세스 없이 sampler·
  fd 선택 테스트.

## 통합 테스트 (진짜 I/O — 최소한만)

가짜로 못 만드는 것만:

- **진짜 `cp` 스트리밍:** fake cp(shell script)는 `printf` flush를 제어할 수 있어 **진짜
  `cp`의 block-buffering을 못 잡는다.** 그러니 **최소 하나의 통합 테스트는 진짜 `cp`로 큰
  파일을 복사하며 `stdbuf -oL` 경로에서 `-v` 줄이 복사 끝이 아니라 진행 중에 도착하는지**를
  검증한다. (이게 없으면 fake-cp 테스트가 다 통과해도 실전에서 라이브 UI가 죽는다.)
- 진짜 자식이 진짜 임시 파일을 복사, 진짜 `/proc`로 샘플 → percent가 오르고 100%에 도달.
- **`stdbuf` 없을 때 managed→passthrough 폴백** (PATH에서 `stdbuf`를 가린 채 실행).
- **부분 청크 relay:** 개행이 안 끝난 바이트도 즉시 로그로 흘리는지.
- ~~`-v` 이중 주입 방지~~ — 유닛(`process.rs::managed_does_not_double_inject_verbose`)이 전부다.
  중복된 `-v`는 `cp`의 출력으로 관측되지 않으므로 통합 테스트가 볼 수 있는 것이 없다.
- PTY 기반(`openpty`) 테스트: passthrough 출력이 `cp`와 바이트 동일; footer가 종료 전 지워짐;
  `SIGWINCH`가 재배치를 유발(+ 시그널 유실 시 폴백 재조회); `cprog`에 시그널이 오면 signaled-
  exit 보존 + footer 정리.
- **버전 표시(#15):** `--help`/`--version`이 TTY에서는 cprog 한 줄을 stderr에 덧붙이고, 캡처된
  상태(파이프·리다이렉트)에서는 **stdout·stderr 둘 다 `cp`와 바이트 동일**함을 확인한다. 후자가
  핵심이다 — `alias cp='cprog'` 때문에 시스템의 모든 `cp --version | …`이 이 경로를 탄다.
- **passthrough env 순수성:** managed가 쓰는 `QUOTING_STYLE=shell-escape`이 passthrough엔 안
  걸려, `cp`의 stdout·**에러 메시지(로케일 포함)** 까지 바이트 동일. (managed는 `LC_ALL=C`를
  걸지 않는다 — cprog는 `-v`를 파싱하지 않아 로케일 고정이 불필요하고, 비-ASCII 파일명을 보존한다.)
- fault injection("sampler/relay가 중간에 실패" 정리 경로)은 별도 feature 없이 유닛으로
  덮는다: `dest_stat_error_skips_tick_and_keeps_model`, `proc_error_skips_tick`,
  `io_failure_is_returned_and_drop_never_panics` + `drop_never_panics_when_the_erase_itself_fails`,
  `tests/exit_contract.rs`. 앞의 두 개는 짝이다 — 첫 write부터 실패하는 writer는 footer가 화면에
  올라가기 전에 `draw`를 끝내므로 `Drop`의 `erase()`가 `rows == 0` 조기반환을 타고, 두 번째가
  fail-after-N writer로 draw를 성공시켜 그 `erase()`를 실제로 밟게 한다(#69).

> **핵심:** fake cp는 flush를 제어하므로 실전 버퍼링을 못 잡는다. 최소 하나의 통합 테스트는
> **진짜 `cp`** 로 스트리밍을 확인해야 한다.

## 각 기능의 루프

```
1. RED    실패 테스트 먼저 (유닛 우선; 어쩔 수 없을 때만 seam/통합)
2. GREEN  통과하는 최소 코드
3. REFACTOR  정리; 전체 스위트 + clippy green 유지
```

## 가드레일

- **기본 `cargo test`는 외부 도구 없이 완전 green** — 유닛 스위트만 돈다(‑ `cp`/`stdbuf`/PTY
  불요). 커널 `/proc`·임시파일만 쓰는 self-pid 테스트는 유닛에 포함된다.
- **통합 테스트는 `cargo test --features integration`** — 진짜 `cp`/`stdbuf` + PTY를 쓰는
  `tests/*.rs` **전체**가 `integration` feature로 게이트돼, 기본 스위트의 순수성을 지킨다(파일을
  일일이 세지 않는다 — 목록은 늘 뒤처진다). 그중 `harness.rs`만 성격이 다르다: 외부 도구 없이
  공용 하네스(`tests/common/mod.rs`) 자체를 검증한다. 하네스가 터미널을 잘못 재현하면 그 위에서
  한 모든 단언이 허구가 되므로, 렌더 재현은 하네스에서도 테스트 대상이다(#60). 외부
  도구가 필요 없는 `tests/exit_contract.rs`만 기본 스위트에 남는다.
- 동작을 최대한 순수 유닛으로 **끌어내려** 통합 테스트를 적고 안정적으로.
- **`cp` 결과 보존**을 테스트로 못박음: relay/footer IO 실패가 exit code를 바꾸지 않음.
- fault-injection 전용 feature는 두지 않는다. 주입 seam 없이도 실패 경로를 유닛에서 덮을 수 있고,
  쓰이지 않는 feature는 게시된 크레이트에서 "켤 수 있지만 아무 일도 없는" 스위치로 노출되기 때문이다.
- **테스트를 쓰면 뮤테이션으로 검증한다.** 새 테스트가 지킨다는 그 규칙을 프로덕션 코드에서
  일부러 깨뜨려(비교 연산자 뒤집기, 함수 본문 비우기) **그 테스트가 실제로 빨간불이 되는지**
  본다. green인 채로 넘어간 테스트는 커버리지가 아니라 장식이다 — 실제로 그런 테스트를 쓴 적이
  있다. 종료하지 않는 규칙(EINTR 재시도 vs 진짜 에러)은 실패 대신 **정지(hang)** 로 나타나며,
  그것도 검출로 친다(`cargo-mutants`가 timeout을 detection으로 세는 것과 같다).
- **리사이즈 잔상은 `scripts/resize-residue.sh`로 잰다.** 이 버그는 *이미 그려진 행을 터미널이
  다시 접는 것*에서 나오므로 리플로가 없는 화면 모델로는 구조적으로 볼 수 없다. tmux는 리플로하고
  (실측), 명령으로 리사이즈되며(`resize-window`), 화면을 텍스트로 준다(`capture-pane`).
  판정은 "대상 경로가 화면에 몇 번 나오나" — 살아 있는 footer 한 줄뿐이어야 한다.
  **재현 조건 넷이 다 필요하다**(`-v` 없이 / 접힐 만큼 긴 경로 / 복사 위에 출력 / FIFO 원본).
  하나만 빠져도 "잔상 없음"이 나오는데 그건 통과가 아니라 **무결과**다.
- **프레임 경계는 `scripts/flicker.sh --strace`로 잰다.** 판정은 "바가 화면에서 사라졌다가
  돌아오기까지의 시간" 중 1 ms 미만인 것들의 합이다. 1 ms 이상은 footer가 정당하게 없는
  구간이라 섞어 세면 몇 배로 부풀려진다. **지우기 시퀀스를 패턴으로 찾으면 안 된다** — 렌더러마다
  다르고, 안 맞으면 조용히 0을 낸다(두 번 속았다). PTY 크기도 명시해야 한다.
- 전수 확인은 `cargo mutants -j 3 -- --features integration`. 생존자는 셋 중 하나다:
  **테스트 공백**(고친다) / **equivalent**(관측 차이 없음) / **도달 불가**(테스트를 지어내지 않는다).
  뒤의 둘은 판정과 근거를 [`exceptions.md`](./exceptions.md) **F20 대장**에 📄로 남겨 다음 실행이
  같은 판정을 다시 파생하지 않게 한다(방어값 자체의 도달 불가는 F16 옆 F15). `mutants.out*`은
  커밋하지 않는다 — 1,400개가 넘는 untracked 파일이 `cargo publish --locked --dry-run`을 깨뜨린다.
- **판정도 실측한다.** "equivalent라서 못 잡는다"는 주장 자체가 틀릴 수 있다 — 대장에 적기 전에
  변이를 걸어 스위트를 돌려본다. 실제로 한 줄의 세 변이 중 하나만 생존하는데 줄 전체가 생존으로
  기록돼 있던 적이 있고, 한 자리씩 봐야 할 것을 두 자리 함께 바꾸면 판정이 뒤집힌다.

---

# 예외 상황 TDD 매트릭스

이 도구의 전제(`cp` 관찰 + `/proc`/`stat` + `-v` 타이밍 + `stdbuf`)에서 **파생되는 예외**를
카테고리별로 모았다. 각 항목은 RED로 먼저 명세한다. 열: **예외 / 기대 동작 / 테스트 방식**.

## A. 진행 계산 (`/proc` + `st_size`)

| # | 예외 | 기대 동작 | 방식 |
|---|---|---|---|
| A1 | `copy_file_range`로 `fdinfo:pos`가 0 | `pos`가 아니라 `st_size`를 읽음(설계 고정) | 유닛(sampler가 size 소스 사용) |
| A2 | `fallocate`로 `st_size`가 즉시 full | **한계로 수용** — 바가 즉시 100%. `cp`엔 선할당 경로가 없어 도달 불가(#12) | 유닛(한계를 명시하는 테스트) |
| A3 | reflink/CoW로 즉시 완료 | 첫 샘플 전 `done==total` → 바 100% 또는 skip | 유닛 |
| A4 | sparse 파일(‑ `st_size` > 실제) | size로 재므로 %는 정확, rate만 과대 허용 | 유닛 + 문서 한계 |
| A5 | 총량 0(빈 원본) | `0/0` div-by-zero 금지 → 100% 또는 indeterminate | 유닛 |
| A6 | `done > total`(오버슈트) | %를 100으로 clamp | 유닛 |
| A7 | 두 샘플 간 증가 0/음수 | `rate≈0`, `eta=--:--` | 유닛 |
| A8 | 현재 대상 파일이 `/proc`에 없음(파일 사이/디렉터리 중) | 바 없음/indeterminate | 유닛(‑ fd 픽스처에 정규 dst 없음) |
| A9 | `/proc`/`stat` 읽기 실패(fd 닫힘·pid 종료·권한) | 샘플 skip, 마지막 값 유지, 크래시 없음 | 유닛(‑ 소스가 Err) |
| A10 | 원본이 특수파일(fifo/device) | `st_size` 무의미 → indeterminate | 유닛 |
| A11 | 정규 파일 fd 여럿 open | 성장 중인 대상 하나 선택 | 유닛(‑ fd 선택 규칙) |

## B. `-v` / 버퍼링 / 타이밍

| # | 예외 | 기대 동작 | 방식 |
|---|---|---|---|
| B1 | `stdbuf` 없음 | managed 포기 → passthrough | 통합(‑ PATH에서 가림) |
| B2 | `cp`가 파이프에서 block-buffer | `stdbuf -oL`로 실시간 강제됨을 확인 | **통합(진짜 cp)** |
| B3 | `-v` 줄이 read 청크 경계에 걸침 | 부분 줄 보류, 다음 청크에서 펄스 | 유닛 |
| B4 | 파일명에 개행/특수문자 | `-v` 내용 파싱 안 하므로 로직 무영향 | 유닛(‑ 아무 바이트나 넣어 펄스만 셈) |
| B5 | 매우 빠른 파일(<임계) | 바 안 뜸 | 유닛(slowfile+clock) |
| B6 | 임계 경계값 | 직전=바X, 직후=바O | 유닛 |
| B7 | 연속 느린 파일 2개 | 바가 파일 경계에서 전환 | 유닛 |
| B8 | 사용자가 `-v` 이미 줌 | 이중 주입 금지, 표시 유지 | 유닛 + 통합 |
| B9 | 개행 없는 꼬리 바이트(‑ 마지막 출력) | 즉시 relay(‑ 안 물고 있음) | 통합 |

## C. 터미널 / 렌더

| # | 예외 | 기대 동작 | 방식 |
|---|---|---|---|
| C1 | 바 도중 리사이즈 | SIGWINCH → 재배치. 재배치는 스크롤 영역을 다시 걸고 **로그 커서부터 화면 끝까지만** 지운다(`ESC[J`). 커서 위는 사용자 것이라 어떤 방향에서도 안 건드린다 — 넓힐 때 남는 잔상은 [F21](./exceptions.md#f21)로 수용 | 유닛(flag) + 통합, 지우기 범위는 `render.rs::no_resize_direction_blanks_what_is_already_on_screen`·`a_widening_leaves_its_residue_above_the_cursor_and_that_is_accepted` |
| C12 | **어떤 재배치도 화면을 지우면 안 된다** | `ESC[2J`는 잔상과 함께 **셸 프롬프트·사용자가 친 명령줄·화면의 `cp -v` 로그**를 지운다. 넣었다가 실제 터미널에서 되돌렸다(한 번만 넓혀도 명령을 실행한 줄이 사라졌다). cprog는 그 바이트를 본 적이 없어 복원할 수 없으므로, 진행바를 정리하자고 터미널 이력을 파괴하지 않는다 | 유닛 — `render.rs::arming_the_region_preserves_the_log_cursor_and_clears_below_it`(어떤 arm도 `ESC[2J`를 안 낸다), `no_resize_direction_blanks_what_is_already_on_screen`(네 방향 전부) |
| C2 | SIGWINCH 유실/합쳐짐 | 폴백 주기로 재조회 | 유닛 |
| C3 | 터미널이 footer보다 짧음(`height < min_log`) | footer 억제/최소, 로그 영역 보존 | 유닛(layout) |
| C4 | **아주 긴 파일명/경로** | `-v` 없이 실행하면 footer 1행이 대상 경로를 보여준다. 표시폭 기준 **앞에서 자르고** `…` | 유닛(폭 경계·CJK 포함) |
| C5 | **파일명 제어문자/개행** | **자르기 전에 제거** — 남기면 footer가 한 줄을 넘어 2행 지우기가 어긋남 | 유닛(개행·NUL·ESC 픽스처) |
| C6 | 렌더/IO 실패 | exit code 안 바뀜(best-effort) | 유닛 + 통합 |
| C7 | 렌더 중 panic | `FooterGuard::Drop`이 화면 정리 | 유닛(catch_unwind) |
| C8 | ~~로그 바이트 도착~~ | **#76으로 규칙이 사라졌다.** footer가 스크롤 영역 밖에 있어 로그와 겹치지 않으므로 지우고 다시 그릴 일이 없다 | — |
| C11 | **footer 위치가 커서에 의존하지 않는다** | 스크롤 영역 + 절대 좌표. 리사이즈가 이미 그려진 것을 재배치해도 위치가 안 어긋난다. **상단 마진은 1행 고정**(alacritty만이 그 조건에서 스크롤백을 만든다) | 유닛 — `render.rs::the_scroll_region_starts_at_row_one`, `the_footer_is_written_to_the_last_two_rows_by_absolute_position`, `drawing_the_footer_returns_the_cursor_to_the_log`, 모델 쪽 `the_model_feeds_scrollback_only_when_the_top_margin_is_row_one` |
| C10 | **필드 값이 자릿수·단위를 바꿈** | percent·size·rate·eta가 전부 고정폭이라 뒤 필드도 **바도** 안 밀린다(ui.md 불변식 8). 폭을 넘는 값은 자르지 않고 넘친다 | 유닛 — 포매터별(`ui.rs::rate_field_is_constant_width`, `eta_field_is_constant_width`, `the_size_field_is_constant_width_while_the_total_holds`, `a_rate_too_wide_for_the_field_overflows_rather_than_truncating`, `an_eta_too_wide_for_the_field_overflows_rather_than_truncating`) + 합성 결과 둘(`eta_stays_in_one_column_as_the_numbers_change`가 80칸 footer에서 eta의 시작 열을 재고, `the_bar_does_not_move_as_the_eta_crosses_the_hour`가 8~200칸 전 폭에서 바 길이를 잰다). 부작용으로 필드 등장 경계가 올라갔다(rate 46→47은 #74, eta 56→60·바가 20으로 돌아오는 지점 66→70은 #74와 #76을 합친 값, 43칸 이하는 불변). 불변식 6의 근거 테스트는 축을 고정할 때마다 공허해져 rate → eta → **넘치는 rate**(`bar_width_is_stable_as_an_overflowing_rate_changes_width`)로 두 번 옮겼다 |
| C9 | **footer가 터미널 최하단 행에 있음** | 2행 사이의 `\n`이 화면을 스크롤시킨다. 그래도 다음 erase는 정확히 한 줄만 되감아야 하고(안 그러면 이후 모든 쓰기가 한 줄씩 밀려 로그가 잡아먹힌다), tick 재그리기는 **스크롤을 일으키지 않아야** 한다 | 유닛 — 무한 버퍼가 아니라 **높이가 유한하고 스크롤하는 화면 모델**(`render.rs::Screen`)에 대고 검증. 로그 줄이 스크롤백+화면 통틀어 정확히 한 번씩만 나타나는지 확인 (#35) |

## D. `cp` 결과 / 종료

| # | 예외 | 기대 동작 | 방식 |
|---|---|---|---|
| D1 | `cp` 중간 실패(권한/ENOSPC) | 에러 relay, footer 정리, exit code 보존 | 통합 + exit 유닛 |
| D2 | `cp` 시그널 종료 | 시그널 보존, 요약 없음, footer 정리 | 통합 + exit 유닛 |
| D3 | `cprog`가 직접 시그널(Ctrl-C) 받음 | footer 정리 + 같은 시그널 재현 | 통합 |
| D4 | `cp` spawn 실패(PATH 없음) | `Fatal::CpSpawn` | 유닛 + 실바이너리(`tests/exit_contract.rs` — 빈 PATH라 외부 도구 불요, 기본 스위트) |
| D5 | 인자 없음 | `Fatal::Usage`, exit 1 | 유닛 |
| D8 | `--help`/`--version` | passthrough + TTY일 때만 cprog 버전 한 줄(stderr) | 유닛(`version_notice`) + 통합(PTY/비-TTY 양쪽) |
| D6 | `cp` 정상 exit code n | 그대로 n 반환 | 유닛 + 통합 |
| D7 | `stdbuf`가 `cp`를 exec | PID 안정 → `/proc/<pid>/fd` 유효 | 통합(간접 — managed 테스트에서 footer가 뜨는 것 자체가 증거) |
| D9 | teardown이 샘플러 틱 사이에 걸림 | 대기를 깨워 **즉시** join — `cp`보다 한 샘플 주기를 더 살지 않는다 | 유닛(`lib.rs::Stopper`) + 통합(`tests/managed.rs::teardown_does_not_wait_out_the_sampler_interval` — 주기를 3s로 키워 재는 지연 측정) |

## E. passthrough 순수성

| # | 예외 | 기대 동작 | 방식 |
|---|---|---|---|
| E1 | passthrough에서 env(`QUOTING_STYLE`) | **미변경** → 에러 메시지 로케일까지 동일 | 통합 |
| E2 | interactive(`-i`) | passthrough(‑ 프롬프트 정상) | 통합 |
| E3 | 비-TTY/CI/비-리눅스/`stdbuf` 없음 | passthrough, 바이트 동일 | 유닛(plan) + 통합 |
| E4 | `CPROG_PASSTHROUGH` 설정 | 무조건 passthrough + 버전 한 줄 억제 | 유닛(plan) + 통합(PTY에서 footer·요약·버전줄 없음) |
| E5 | passthrough의 exec 프로세스 대체 | spawn한 pid의 `comm`이 `cp`가 됨(래퍼 프로세스 없음) | 통합(FIFO로 붙잡아 두고 `/proc/<pid>/comm` 확인) |
| E6 | exec 후 `SIGPIPE` 의미론 | `cp -v … \| 닫힌 파이프`에서 순정 `cp`와 같은 signal-death | 통합(진짜 `cp`와 종료 상태·stderr 비교) |
