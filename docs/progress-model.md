# Progress Model (진행 계산)

느린 파일의 per-file 바를 어떻게 계산하는지 정의한다. 외부 도구 없이, `cp`의 협조 없이,
커널의 `/proc`와 `stat`만 쓴다.

> **근거.** GNU coreutils 9.x의 `cp`는 `copy_file_range`를 쓰는데, 이 경로에서는 fd offset
> (`fdinfo: pos`)이 복사 내내 0으로 남고 `/proc/<pid>/io`의 `wchar`도 0이다. 복사를 따라
> 커지는 신호는 **대상 파일의 크기(`stat().st_size`)** 뿐이다. `cprog`는 이 위에 세운다.

## 핵심 신호: 대상 파일 크기

```
total   = stat(현재 원본).st_size     # 파일 시작 시 1회
done    = stat(현재 대상).st_size      # cp가 그 파일을 쓰는 동안 폴링
percent = done / total
```

`st_size`는 inode의 파일 길이다. `cp`가 `write`/`copy_file_range`/`sendfile` 무엇으로
쓰든 대상이 커지면 증가하므로, **syscall 종류와 무관하게** 진행을 반영한다. 이게 `pos`와
io 카운터가 실패하는 곳에서도 되는 이유다.

원본이 아니라 **대상**을 재는 이유: 대상 크기는 "실제로 안착한 바이트"이고, 원본 read
위치는 read-ahead 캐시로 앞서갈 수 있다.

## 현재 파일을 찾는 법 (read-only 관찰)

바를 켤 시점(느린 파일 감지, [`capture-and-verbose.md`](./capture-and-verbose.md))에 `cp`의
열린 fd를 관찰한다. `cp`를 건드리지도, 막지도 않는다.

1. `/proc/<pid>/fd/`를 나열하고 각 항목을 `readlink`로 대상 경로로 푼다.
2. **정규 파일(regular file)** 만 남긴다(파이프·소켓·tty·디렉터리 제외).
3. **대상 쪽(쓰기 중, 커지는)** 이 지금 쓰는 파일이다 → 그 경로를 `stat` → `done`.
4. 원본 쪽 fd → `stat` → `total`.

경로는 `readlink`가 준 **실제 경로**라 `-v` 텍스트를 unquote할 필요가 없다.

## rate / eta (현재 파일 기준)

`(시각, done)` 샘플을 짧은 rolling window에 담는다:

```
rate = (done_now − done_window_start) / (t_now − t_window_start)
eta  = (total − done) / rate       # rate 모르면 --:--
```

window(기본 ~1s)가 버스트 I/O를 평활화해 숫자가 튀지 않게 한다. **이 %/rate/eta는 현재
파일 하나에 대한 것**이다 — 전체 작업 값이 아니다.

## 샘플링 주기와 비용

- 대상 크기를 고정 주기(기본 100ms)로 `stat` 폴링한다. 렌더 tick과 독립.
- 각 샘플 = `readlink` 1회(현재 파일) + `stat` 1회 — hot inode 메타데이터 조회라 마이크로초.
  **파일 데이터는 안 읽으므로** 페이지 캐시 오염·cp 간섭 없음.
- **느린 파일에만** 이 폴링을 한다. 빠른 소파일은 바를 안 켜므로 stat도 안 한다 → 대량
  소파일에서 stat 폭발이 없다.

## 실패 처리

- 실패한 샘플은 건너뛰고 마지막 값 유지(파일 종료·fd 닫힘·pid 종료·권한).
- 샘플링이 아예 안 되면 바만 안 보일 뿐 복사는 정상. `cp`가 언제나 authoritative.
- 자식의 `/proc`/`stat` 읽기는 같은 user일 때 정상(일반적). setuid/`sudo` cp면 못 읽어
  바가 안 뜰 뿐, 복사엔 영향 없음.

## 한계 (정직하게 명시)

- **preallocation**(`fallocate`): `cp`가 대상을 선할당하면 `st_size`가 한 번에 full로 튈 수
  있다 → **`st_blocks * 512`(실제 디스크 블록)** 로 폴백해 실제 쓰기에 따라 증가시킨다.
- **reflink / copy-on-write**: 거의 즉시 완료 → 바가 100%로 점프(정확하지만 점진적이지 않음).
- **sparse 파일**: `st_size`가 논리 길이라 실제 쓴 바이트를 넘을 수 있음(‑비율은 유의미,
  rate가 높게 보일 수 있음).
- **아주 빠른 파일**: 첫 샘플 전에 끝남 → 바 없이 지나감(의도된 동작).
- **비-리눅스**: `/proc` 없음 → managed 불가, passthrough로.

## 왜 `pos`가 아니라 `st_size`인가 (참고)

커널 `fdinfo`의 `pos`를 읽는 방식(일부 외부 진행률 도구가 쓰는)은 `copy_file_range` 때문에
**coreutils 9.x에서 진행을 못 잡는다**(위 근거). `cprog`는 대상 `st_size`를 직접 읽으므로
외부 프로세스·PTY 없이 단순하면서도 현대 `cp`에서 정확하다.
